//! `buy` — purchase tokens off the constant-product bonding curve.
//!
//! ## Fee flow (spec-locked)
//!
//! ```text
//!     total_fee     = sol_in · bonding_fee_bps / 10 000
//!     charity_fee   = total_fee · charity_bps / 10 000   (→ LaunchedCoin.charity_wallet)
//!     remaining_fee = total_fee − charity_fee
//!     platform_fee  = remaining_fee · platform_share_bps / 10 000   (→ Config.treasury)
//!     creator_fee   = remaining_fee · creator_share_bps / 10 000    (→ LaunchedCoin.creator_fee_recipient)
//!     rewards_fee   = remaining_fee − platform_fee − creator_fee     (→ RewardsPool)
//!     sol_to_curve  = sol_in − total_fee                             (→ launched_coin PDA lamports)
//! ```
//!
//! ## Curve (constant-product, `xy = k`) with pump.fun-parity offset
//!
//! `virtual_tokens_before = virtual_token_offset + tokens_remaining_on_curve`.
//! Non-zero `virtual_token_offset` (e.g. 280M × 10^6 for Standard) flattens
//! the curve so more tokens are delivered per SOL. See `math::buy_quote`
//! and the pump.fun-parity tests for details.
//!
//! ## Token-2022 transfer fee interaction
//!
//! Every vault→buyer transfer has the mint's `TransferFeeConfig` rate
//! withheld on the buyer's ATA. The buyer's *spendable* credit is
//! `tokens_out · (1 − transfer_fee_bps/10_000)`. Slippage is checked on
//! the spendable net, not the gross curve output.
//!
//! ## SOL reserves live on `LaunchedCoin` (pump.fun pattern)
//!
//! Curve lamports are held on the `LaunchedCoin` PDA itself — no
//! separate `vault_sol` account. `LaunchedCoin.sol_reserves` tracks
//! lamports above the account's rent-exempt minimum. This saves
//! ~0.000954 SOL of rent per launch on every path (atomic / non-atomic,
//! first buyer / subsequent) and matches pump.fun's `bonding_curve`
//! account design.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{self, Transfer as SystemTransfer};
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_2022::{self, Token2022, TransferChecked};
use anchor_spl::token_interface::{Mint, TokenAccount};

use super::bonding_fee_escrow::{emit_escrowed, total_credit, trade_fee_destination};
use crate::constants::*;
use crate::errors::LaunchpadError;
use crate::events::{Bought, CharityPayout};
use crate::math;
use crate::state::{Config, LaunchedCoin, RewardsPool};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct BuyParams {
    pub sol_in: u64,
    /// Minimum tokens the buyer must actually *receive* (after the
    /// Token-2022 transfer-fee withholding). Protects against slippage
    /// and priced-out front-runs.
    pub min_tokens_out: u64,
}

// Every large Account / InterfaceAccount is `Box`ed so the deserialized
// data lives on the heap. Without this, the generated `try_accounts` fn
// for Buy / Sell overruns the SBF 4 KiB stack cap.
#[derive(Accounts)]
pub struct Buy<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,

    #[account(
        mut,
        seeds = [LAUNCHED_COIN_SEED, mint.key().as_ref()],
        bump = launched_coin.bump,
        has_one = mint,
    )]
    pub launched_coin: Box<Account<'info, LaunchedCoin>>,

    #[account(mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = launched_coin,
        associated_token::token_program = token_program,
    )]
    pub vault_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = buyer,
        associated_token::mint = mint,
        associated_token::authority = buyer,
        associated_token::token_program = token_program,
    )]
    pub buyer_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: verified by `address = config.treasury` constraint.
    #[account(mut, address = config.treasury)]
    pub treasury: UncheckedAccount<'info>,

    /// CHECK: verified by `address = launched_coin.creator_fee_recipient`.
    #[account(mut, address = launched_coin.creator_fee_recipient)]
    pub creator_fee_recipient: UncheckedAccount<'info>,

    #[account(mut, seeds = [REWARDS_POOL_SEED], bump = rewards_pool.bump)]
    pub rewards_pool: Box<Account<'info, RewardsPool>>,

    /// CHECK: charity destination. When `launched_coin.charity_bps == 0`
    /// any account may be passed (sentinel — no transfer runs). When
    /// non-zero, must match `launched_coin.charity_wallet`.
    #[account(mut)]
    pub charity_wallet: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token2022>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub(crate) fn handler<'info>(
    ctx: Context<'_, '_, '_, 'info, Buy<'info>>,
    params: BuyParams,
) -> Result<()> {
    super::graduation_policy::validate_curve_trading(&ctx.accounts.launched_coin)?;

    let launched_coin = &ctx.accounts.launched_coin;
    let config = &ctx.accounts.config;
    super::set_bonding_shares::validate_bonding_shares(
        config.bonding_platform_share_bps,
        config.bonding_creator_share_bps,
        config.bonding_rewards_share_bps,
    )?;

    // Charity wallet: if the launch has a charity, require the passed
    // account to match. Otherwise accept any (no transfer runs).
    if launched_coin.charity_bps > 0 {
        require_keys_eq!(
            ctx.accounts.charity_wallet.key(),
            launched_coin.charity_wallet,
            LaunchpadError::CharityWalletMismatch
        );
    }

    let virtual_sol_before = launched_coin
        .sol_listed
        .checked_add(launched_coin.sol_reserves)
        .ok_or(LaunchpadError::AmountOverflow)?;

    let quote = math::buy_quote(
        params.sol_in,
        virtual_sol_before,
        launched_coin.tokens_remaining_on_curve,
        launched_coin.virtual_token_offset,
        config.bonding_curve_fee_bps,
        config.bonding_platform_share_bps,
        config.bonding_creator_share_bps,
        config.bonding_rewards_share_bps,
        launched_coin.charity_bps,
        launched_coin.transfer_fee_bps,
    )
    .map_err(LaunchpadError::from)?;

    // A zero slippage minimum must not permit paying SOL for only withheld fees.
    require!(
        quote.tokens_received_by_buyer > 0
            && quote.tokens_received_by_buyer >= params.min_tokens_out,
        LaunchpadError::SlippageExceeded
    );

    // Optional first-buy supply-share cap — only enforced on the first
    // ever trade (identified by zero cumulative volume).
    if launched_coin.cumulative_token_volume == 0 && config.first_buy_max_pct_of_supply > 0 {
        let cap_tokens = (launched_coin.total_supply as u128)
            .checked_mul(config.first_buy_max_pct_of_supply as u128)
            .and_then(|n| n.checked_div(BPS_DENOMINATOR as u128))
            .ok_or(LaunchpadError::AmountOverflow)?;
        require!(
            (quote.tokens_out as u128) <= cap_tokens,
            LaunchpadError::FirstBuyCapExceeded
        );
    }

    let platform_fee = quote.platform_fee;
    let creator_fee = quote.creator_fee;
    let rewards_fee = quote.rewards_fee;
    let charity_fee = quote.charity_fee;
    let sol_to_curve = quote.sol_to_curve;
    let tokens_out = quote.tokens_out;

    // Recipient ownership and rent coverage can change after launch.
    // Keep each entitlement recoverable without blocking holder trades.
    let rent = Rent::get()?;
    let creator_credit_key = if launched_coin.reserved[super::creator_revenue::REVENUE_LOCK_BYTE] == 0 {
        ctx.accounts.creator_fee_recipient.key()
    } else {
        Pubkey::find_program_address(&[super::creator_revenue::REVENUE_SEED, ctx.accounts.mint.key().as_ref()], &crate::ID).0
    };
    let recipient_credits = [
        (ctx.accounts.treasury.key(), platform_fee),
        (creator_credit_key, creator_fee),
        (ctx.accounts.charity_wallet.key(), charity_fee),
    ];
    let treasury_destination = trade_fee_destination(
        ctx.accounts.mint.key(),
        ctx.accounts.treasury.to_account_info(),
        total_credit(ctx.accounts.treasury.key(), &recipient_credits),
        &rent,
        ctx.remaining_accounts,
    )?;
    let creator_destination = if launched_coin.reserved[super::creator_revenue::REVENUE_LOCK_BYTE] != 0 {
        super::creator_revenue::destination(launched_coin,
            ctx.accounts.creator_fee_recipient.to_account_info(), ctx.remaining_accounts)?
    } else {
        trade_fee_destination(
        ctx.accounts.mint.key(),
        ctx.accounts.creator_fee_recipient.to_account_info(),
        total_credit(ctx.accounts.creator_fee_recipient.key(), &recipient_credits),
        &rent,
        ctx.remaining_accounts,
        )?
    };
    let charity_destination = if launched_coin.charity_bps > 0 {
        trade_fee_destination(
            ctx.accounts.mint.key(),
            ctx.accounts.charity_wallet.to_account_info(),
            total_credit(ctx.accounts.charity_wallet.key(), &recipient_credits),
            &rent,
            ctx.remaining_accounts,
        )?
    } else {
        ctx.accounts.charity_wallet.to_account_info()
    };

    // Preflight every state mutation before the first money-moving CPI. A
    // transaction is atomic, but deterministic overflow should fail before
    // spending compute on up to five SOL transfers and a Token-2022 transfer.
    let accounting_after = buy_accounting_after(
        launched_coin.tokens_remaining_on_curve,
        launched_coin.sol_reserves,
        launched_coin.cumulative_sol_volume,
        launched_coin.cumulative_token_volume,
        ctx.accounts.rewards_pool.accumulated_sol,
        tokens_out,
        sol_to_curve,
        params.sol_in,
        rewards_fee,
    )?;
    let (vault_balance_after, buyer_balance_after) = expected_token_balances_after(
        ctx.accounts.vault_token_account.amount,
        ctx.accounts.buyer_token_account.amount,
        tokens_out,
        quote.tokens_received_by_buyer,
    )?;
    super::launch_graduation::validate_buy_inventory(
        launched_coin,
        ctx.accounts.vault_token_account.amount,
        &quote,
        &rent,
    )?;

    // Fee destinations may alias each other (for example, a creator may use
    // the platform treasury). Preflight the aggregate credit for every unique
    // destination before the first transfer so an otherwise deterministic
    // lamport overflow cannot fail partway through the money-moving CPIs.
    preflight_lamport_credits(
        ctx.accounts.buyer.key(),
        &[
            (&treasury_destination, platform_fee),
            (&creator_destination, creator_fee),
            (&ctx.accounts.rewards_pool.to_account_info(), rewards_fee),
            (&charity_destination, charity_fee),
            (&ctx.accounts.launched_coin.to_account_info(), sol_to_curve),
        ],
    )?;

    // -- SOL transfers (buyer → five destinations) ----------------------
    if platform_fee > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                SystemTransfer {
                    from: ctx.accounts.buyer.to_account_info(),
                    to: treasury_destination.clone(),
                },
            ),
            platform_fee,
        )?;
    }
    if creator_fee > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                SystemTransfer {
                    from: ctx.accounts.buyer.to_account_info(),
                    to: creator_destination.clone(),
                },
            ),
            creator_fee,
        )?;
    }
    if rewards_fee > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                SystemTransfer {
                    from: ctx.accounts.buyer.to_account_info(),
                    to: ctx.accounts.rewards_pool.to_account_info(),
                },
            ),
            rewards_fee,
        )?;
    }
    if charity_fee > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                SystemTransfer {
                    from: ctx.accounts.buyer.to_account_info(),
                    to: charity_destination.clone(),
                },
            ),
            charity_fee,
        )?;
    }
    if sol_to_curve > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                SystemTransfer {
                    from: ctx.accounts.buyer.to_account_info(),
                    to: ctx.accounts.launched_coin.to_account_info(),
                },
            ),
            sol_to_curve,
        )?;
    }

    // -- Token-2022 vault → buyer transfer ------------------------------
    let mint_key = ctx.accounts.mint.key();
    let launched_coin_bump = launched_coin.bump;
    let signer_seeds: &[&[&[u8]]] =
        &[&[LAUNCHED_COIN_SEED, mint_key.as_ref(), &[launched_coin_bump]]];

    token_2022::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.vault_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.buyer_token_account.to_account_info(),
                authority: ctx.accounts.launched_coin.to_account_info(),
            },
            signer_seeds,
        ),
        tokens_out,
        launched_coin.decimals,
    )?;

    // The quote and curve accounting use both the nominal vault debit and the
    // spendable buyer credit after Token-2022 withholding. Verify both sides
    // settled exactly before advancing reserves or cumulative volume. This
    // keeps buys fail-closed if a future token extension or fee behaviour
    // diverges from the launch's snapshotted transfer-fee math.
    ctx.accounts.vault_token_account.reload()?;
    ctx.accounts.buyer_token_account.reload()?;
    require_eq!(
        ctx.accounts.vault_token_account.amount,
        vault_balance_after,
        LaunchpadError::TokenSettlementMismatch
    );
    require_eq!(
        ctx.accounts.buyer_token_account.amount,
        buyer_balance_after,
        LaunchpadError::TokenSettlementMismatch
    );

    // -- state updates --------------------------------------------------
    let launched_coin = &mut ctx.accounts.launched_coin;
    launched_coin.tokens_remaining_on_curve = accounting_after.tokens_remaining_on_curve;
    launched_coin.sol_reserves = accounting_after.sol_reserves;
    launched_coin.cumulative_sol_volume = accounting_after.cumulative_sol_volume;
    launched_coin.cumulative_token_volume = accounting_after.cumulative_token_volume;
    super::graduation_policy::record_curve_completion(launched_coin)?;

    let rewards_pool = &mut ctx.accounts.rewards_pool;
    rewards_pool.accumulated_sol = accounting_after.rewards_accumulated_sol;

    // -- events ----------------------------------------------------------
    emit!(Bought {
        mint: mint_key,
        buyer: ctx.accounts.buyer.key(),
        sol_in: params.sol_in,
        tokens_out,
        platform_fee,
        creator_fee,
        rewards_fee,
        cumulative_volume: launched_coin.cumulative_sol_volume,
        timestamp: Clock::get()?.unix_timestamp,
    });

    for (recipient, destination, amount) in [
        (
            ctx.accounts.treasury.key(),
            &treasury_destination,
            platform_fee,
        ),
        (
            ctx.accounts.creator_fee_recipient.key(),
            &creator_destination,
            creator_fee,
        ),
        (
            ctx.accounts.charity_wallet.key(),
            &charity_destination,
            charity_fee,
        ),
    ] {
        emit_escrowed(mint_key, recipient, destination, amount);
    }
    if charity_fee > 0 && charity_destination.key == ctx.accounts.charity_wallet.key {
        emit!(CharityPayout {
            mint: mint_key,
            charity_wallet: ctx.accounts.charity_wallet.key(),
            amount: charity_fee,
            path: 0,
            timestamp: Clock::get()?.unix_timestamp,
        });
    }

    // -- invariant: launched_coin lamports >= rent + sol_reserves --------
    // Curve SOL lives on the LaunchedCoin PDA itself. Every successful buy
    // must leave the PDA covering its reserves; a shortfall here would
    // indicate a bug in the lamport/state mutation pairing.
    // Not gated on any branch — runs on every success path.
    let actual_lamports = ctx.accounts.launched_coin.to_account_info().lamports();
    let rent_exempt = Rent::get()?.minimum_balance(LaunchedCoin::LEN);
    require!(
        // Floor, not equality: rent_exempt is read from Rent::get() NOW, but
        // this PDA was funded at the rate in force when it launched. A network
        // rent change must not brick Buy on every existing token (2026-09-03).
        math::lamports_cover_curve_reserves(
            actual_lamports,
            rent_exempt,
            ctx.accounts.launched_coin.sol_reserves,
        ),
        LaunchpadError::LamportInvariantViolation
    );

    Ok(())
}

struct BuyAccountingAfter {
    tokens_remaining_on_curve: u64,
    sol_reserves: u64,
    cumulative_sol_volume: u128,
    cumulative_token_volume: u128,
    rewards_accumulated_sol: u64,
}

#[allow(clippy::too_many_arguments)]
fn buy_accounting_after(
    tokens_remaining_on_curve: u64,
    sol_reserves: u64,
    cumulative_sol_volume: u128,
    cumulative_token_volume: u128,
    rewards_accumulated_sol: u64,
    tokens_out: u64,
    sol_to_curve: u64,
    sol_in: u64,
    rewards_fee: u64,
) -> Result<BuyAccountingAfter> {
    Ok(BuyAccountingAfter {
        tokens_remaining_on_curve: tokens_remaining_on_curve
            .checked_sub(tokens_out)
            .ok_or(LaunchpadError::AmountOverflow)?,
        sol_reserves: sol_reserves
            .checked_add(sol_to_curve)
            .ok_or(LaunchpadError::AmountOverflow)?,
        cumulative_sol_volume: cumulative_sol_volume
            .checked_add(sol_in as u128)
            .ok_or(LaunchpadError::AmountOverflow)?,
        cumulative_token_volume: cumulative_token_volume
            .checked_add(tokens_out as u128)
            .ok_or(LaunchpadError::AmountOverflow)?,
        rewards_accumulated_sol: rewards_accumulated_sol
            .checked_add(rewards_fee)
            .ok_or(LaunchpadError::AmountOverflow)?,
    })
}

fn expected_token_balances_after(
    vault_balance_before: u64,
    buyer_balance_before: u64,
    tokens_out: u64,
    tokens_received_by_buyer: u64,
) -> Result<(u64, u64)> {
    let vault_balance_after = vault_balance_before
        .checked_sub(tokens_out)
        .ok_or(LaunchpadError::AmountOverflow)?;
    let buyer_balance_after = buyer_balance_before
        .checked_add(tokens_received_by_buyer)
        .ok_or(LaunchpadError::AmountOverflow)?;

    Ok((vault_balance_after, buyer_balance_after))
}

fn preflight_lamport_credits(payer: Pubkey, credits: &[(&AccountInfo<'_>, u64)]) -> Result<()> {
    for (index, (account, _)) in credits.iter().enumerate() {
        // A destination may be the buyer itself. Each System Program transfer
        // debits the buyer before crediting the destination, so a self-transfer
        // cannot overflow the buyer's starting balance.
        if *account.key == payer {
            continue;
        }
        if credits[..index]
            .iter()
            .any(|(seen, _)| seen.key == account.key)
        {
            continue;
        }

        let mut aggregate_credit = 0u64;
        for (candidate, credit) in &credits[index..] {
            if candidate.key == account.key {
                aggregate_credit = aggregate_credit
                    .checked_add(*credit)
                    .ok_or(LaunchpadError::AmountOverflow)?;
            }
        }
        account
            .lamports()
            .checked_add(aggregate_credit)
            .ok_or(LaunchpadError::AmountOverflow)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_with_lamports(lamports: &mut u64) -> AccountInfo<'_> {
        let key = Box::leak(Box::new(Pubkey::new_unique()));
        let owner = Box::leak(Box::new(Pubkey::new_unique()));
        let data = Box::leak(Vec::<u8>::new().into_boxed_slice());
        AccountInfo::new(key, false, true, lamports, data, owner, false, 0)
    }

    #[test]
    fn lamport_credit_preflight_aggregates_aliased_buy_destinations() {
        let mut shared_balance = u64::MAX - 1;
        let shared = account_with_lamports(&mut shared_balance);
        let mut distinct_balance = 0;
        let distinct = account_with_lamports(&mut distinct_balance);

        let payer = Pubkey::new_unique();
        assert!(preflight_lamport_credits(payer, &[(&shared, 1), (&shared, 1)]).is_err());
        assert!(preflight_lamport_credits(payer, &[(&shared, 0), (&shared, 1)]).is_ok());
        assert!(preflight_lamport_credits(payer, &[(&shared, 1), (&distinct, u64::MAX)]).is_ok());
        assert_eq!(shared.lamports(), u64::MAX - 1);
        assert_eq!(distinct.lamports(), 0);
    }

    #[test]
    fn lamport_credit_preflight_rejects_sum_overflow() {
        let mut balance = 0;
        let destination = account_with_lamports(&mut balance);

        assert!(preflight_lamport_credits(
            Pubkey::new_unique(),
            &[(&destination, u64::MAX), (&destination, 1),]
        )
        .is_err());
    }

    #[test]
    fn lamport_credit_preflight_allows_the_buyer_as_a_destination() {
        let mut balance = u64::MAX;
        let buyer = account_with_lamports(&mut balance);

        assert!(preflight_lamport_credits(*buyer.key, &[(&buyer, u64::MAX)]).is_ok());
    }

    #[test]
    fn buy_accounting_accepts_boundary_values() {
        let after =
            buy_accounting_after(1, u64::MAX, u128::MAX, u128::MAX - 1, u64::MAX, 1, 0, 0, 0)
                .unwrap();
        assert_eq!(after.tokens_remaining_on_curve, 0);
        assert_eq!(after.sol_reserves, u64::MAX);
        assert_eq!(after.cumulative_sol_volume, u128::MAX);
        assert_eq!(after.cumulative_token_volume, u128::MAX);
        assert_eq!(after.rewards_accumulated_sol, u64::MAX);
    }

    #[test]
    fn buy_accounting_rejects_each_overflow_before_cpis() {
        assert!(buy_accounting_after(0, 0, 0, 0, 0, 1, 0, 0, 0).is_err());
        assert!(buy_accounting_after(1, u64::MAX, 0, 0, 0, 0, 1, 0, 0).is_err());
        assert!(buy_accounting_after(1, 0, u128::MAX, 0, 0, 0, 0, 1, 0).is_err());
        assert!(buy_accounting_after(1, 0, 0, u128::MAX, 0, 1, 0, 0, 0).is_err());
        assert!(buy_accounting_after(1, 0, 0, 0, u64::MAX, 0, 0, 0, 1).is_err());
    }

    #[test]
    fn expected_token_balances_include_transfer_fee_withholding() {
        assert_eq!(
            expected_token_balances_after(1_000, 25, 200, 199).unwrap(),
            (800, 224)
        );
    }

    #[test]
    fn expected_token_balances_accept_boundaries() {
        assert_eq!(
            expected_token_balances_after(u64::MAX, 0, u64::MAX, u64::MAX).unwrap(),
            (0, u64::MAX)
        );
    }

    #[test]
    fn expected_token_balances_reject_underflow_and_overflow() {
        assert!(expected_token_balances_after(0, 0, 1, 0).is_err());
        assert!(expected_token_balances_after(1, u64::MAX, 0, 1).is_err());
    }
}
