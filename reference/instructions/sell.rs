//! `sell` — sell tokens back to the bonding curve.
//!
//! ## Curve and fee treatment
//!
//! Inverse of `buy`. The seller transfers `tokens_in` to the vault; the
//! mint's `TransferFeeConfig` withholds
//! `tokens_in · transfer_fee_bps/10_000` on the receiving side. The curve
//! therefore sees a net input of `tokens_net = tokens_in − withheld`.
//!
//! ```text
//!     virtual_tokens_before = virtual_token_offset + tokens_remaining_on_curve
//!     x = sol_listed + sol_reserves
//!     y = virtual_tokens_before
//!     k = x · y
//!
//!     sol_out_raw = x − k / (y + tokens_net)
//! ```
//!
//! The 100 bps bonding fee is applied to `sol_out_raw`, a charity carve
//! (if any) is taken first, and the remainder is split 50/30/20 across
//! platform/creator/rewards. The seller's spendable credit is
//! `sol_out_raw − total_fee`.
//!
//! ## SOL flow
//!
//! Curve SOL lives on `LaunchedCoin` itself (pump.fun pattern). The
//! account is owned by this program so lamports are debited by direct
//! `try_borrow_mut_lamports` manipulation — no system-program CPI is
//! possible from a program-owned source. Destination accounts can be
//! anywhere; any program is allowed to *credit* lamports. The
//! rent-exempt reserve on `LaunchedCoin` is never touched because
//! `sol_out_raw` is bounded above by `launched_coin.sol_reserves`
//! (which tracks lamports above rent).

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_2022::{self, Token2022, TransferChecked};
use anchor_spl::token_interface::{Mint, TokenAccount};

use super::bonding_fee_escrow::{emit_escrowed, total_credit, trade_fee_destination};
use crate::constants::*;
use crate::errors::LaunchpadError;
use crate::events::{CharityPayout, Sold};
use crate::math;
use crate::state::{Config, LaunchedCoin, RewardsPool};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct SellParams {
    pub tokens_in: u64,
    /// Minimum net SOL the seller must receive (after both the Token-2022
    /// transfer fee on the incoming token leg and the bonding fee on the
    /// outgoing SOL leg).
    pub min_sol_out: u64,
}

// See the `Box` rationale on `Buy` — same stack-budget issue.
#[derive(Accounts)]
pub struct Sell<'info> {
    #[account(mut)]
    pub seller: Signer<'info>,

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
        mut,
        associated_token::mint = mint,
        associated_token::authority = seller,
        associated_token::token_program = token_program,
    )]
    pub seller_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: verified by `address = config.treasury`.
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
    ctx: Context<'_, '_, '_, 'info, Sell<'info>>,
    params: SellParams,
) -> Result<()> {
    super::graduation_policy::validate_curve_trading(&ctx.accounts.launched_coin)?;

    // Charity wallet: if the launch has a charity, require the passed
    // account to match. Otherwise accept any (no transfer runs).
    if ctx.accounts.launched_coin.charity_bps > 0 {
        require_keys_eq!(
            ctx.accounts.charity_wallet.key(),
            ctx.accounts.launched_coin.charity_wallet,
            LaunchpadError::CharityWalletMismatch
        );
    }

    // Compute the quote and all state mutations before the first
    // money-moving CPI. Atomic rollback prevents partial loss, but a
    // deterministic overflow should not consume Token-2022 transfer compute.
    let launched_coin = &ctx.accounts.launched_coin;
    let config = &ctx.accounts.config;
    super::set_bonding_shares::validate_bonding_shares(
        config.bonding_platform_share_bps,
        config.bonding_creator_share_bps,
        config.bonding_rewards_share_bps,
    )?;

    let virtual_sol_before = launched_coin
        .sol_listed
        .checked_add(launched_coin.sol_reserves)
        .ok_or(LaunchpadError::AmountOverflow)?;

    let quote = math::sell_quote(
        params.tokens_in,
        virtual_sol_before,
        launched_coin.sol_reserves,
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

    require!(
        quote.sol_to_seller >= params.min_sol_out,
        LaunchpadError::SlippageExceeded
    );

    let platform_fee = quote.platform_fee;
    let creator_fee = quote.creator_fee;
    let rewards_fee = quote.rewards_fee;
    let charity_fee = quote.charity_fee;
    let sol_out_raw = quote.sol_out_raw;
    let sol_to_seller = quote.sol_to_seller;
    let tokens_net = quote.tokens_net;

    // Recipient ownership and rent coverage can change without holder
    // consent. Escrow unsafe credits without changing the quote or reserves.
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
        (ctx.accounts.seller.key(), sol_to_seller),
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

    let mut accounting_after = sell_accounting_after(
        launched_coin.tokens_remaining_on_curve,
        launched_coin.sol_reserves,
        launched_coin.cumulative_sol_volume,
        launched_coin.cumulative_token_volume,
        ctx.accounts.rewards_pool.accumulated_sol,
        tokens_net,
        sol_out_raw,
        params.tokens_in,
        rewards_fee,
    )?;
    accounting_after.tokens_remaining_on_curve = super::launch_graduation::sell_curve_inventory(
        launched_coin,
        ctx.accounts.vault_token_account.amount,
        &quote,
        &rent,
    )?;
    let (seller_balance_after, vault_balance_after) = expected_token_balances_after(
        ctx.accounts.seller_token_account.amount,
        ctx.accounts.vault_token_account.amount,
        params.tokens_in,
        tokens_net,
    )?;

    // Destination accounts are allowed to alias (for example, the seller may
    // also be the creator). Preflight the aggregate credit for each unique
    // account before the Token-2022 transfer so a deterministic lamport
    // overflow cannot fail only after the money-moving CPI.
    preflight_lamport_credits(&[
        (&ctx.accounts.seller.to_account_info(), sol_to_seller),
        (&treasury_destination, platform_fee),
        (&creator_destination, creator_fee),
        (&ctx.accounts.rewards_pool.to_account_info(), rewards_fee),
        (&charity_destination, charity_fee),
    ])?;

    // -- step 1: seller → vault (Token-2022 withholds the transfer fee) --
    token_2022::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            TransferChecked {
                from: ctx.accounts.seller_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.vault_token_account.to_account_info(),
                authority: ctx.accounts.seller.to_account_info(),
            },
        ),
        params.tokens_in,
        ctx.accounts.launched_coin.decimals,
    )?;

    // The SOL quote is priced against `tokens_net`, so prove both the nominal
    // seller debit and the net vault credit settled exactly before debiting
    // curve SOL. This keeps the money path fail-closed if a future Token-2022
    // extension or fee-behaviour change diverges from the launch's
    // snapshotted fee math.
    ctx.accounts.seller_token_account.reload()?;
    ctx.accounts.vault_token_account.reload()?;
    require_eq!(
        ctx.accounts.seller_token_account.amount,
        seller_balance_after,
        LaunchpadError::TokenSettlementMismatch
    );
    require_eq!(
        ctx.accounts.vault_token_account.amount,
        vault_balance_after,
        LaunchpadError::TokenSettlementMismatch
    );

    // -- step 4: debit launched_coin lamports, credit destinations ----
    // `launched_coin` is owned by this program, so the decrement via
    // `try_borrow_mut_lamports` is permitted. The destinations are all
    // *credits*; any program may credit any account. This runs BEFORE
    // any `&mut ctx.accounts.launched_coin` state write below, so the
    // lamport RefMut is dropped before the data RefMut is taken.
    let launched_coin_info = ctx.accounts.launched_coin.to_account_info();
    debit_lamports(&launched_coin_info, sol_out_raw)?;

    credit_lamports(&ctx.accounts.seller.to_account_info(), sol_to_seller)?;
    if platform_fee > 0 {
        credit_lamports(&treasury_destination, platform_fee)?;
    }
    if creator_fee > 0 {
        credit_lamports(&creator_destination, creator_fee)?;
    }
    if rewards_fee > 0 {
        credit_lamports(&ctx.accounts.rewards_pool.to_account_info(), rewards_fee)?;
    }
    if charity_fee > 0 {
        credit_lamports(&charity_destination, charity_fee)?;
    }

    // -- step 5: state updates ----------------------------------------
    let mint_key = ctx.accounts.mint.key();
    let launched_coin = &mut ctx.accounts.launched_coin;
    launched_coin.tokens_remaining_on_curve = accounting_after.tokens_remaining_on_curve;
    launched_coin.sol_reserves = accounting_after.sol_reserves;
    launched_coin.cumulative_sol_volume = accounting_after.cumulative_sol_volume;
    launched_coin.cumulative_token_volume = accounting_after.cumulative_token_volume;

    let rewards_pool = &mut ctx.accounts.rewards_pool;
    rewards_pool.accumulated_sol = accounting_after.rewards_accumulated_sol;

    // -- step 6: events -----------------------------------------------
    emit!(Sold {
        mint: mint_key,
        seller: ctx.accounts.seller.key(),
        tokens_in: params.tokens_in,
        sol_out: sol_to_seller,
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
    // See the matching assertion in `buy`. A sell debits both lamports
    // and `sol_reserves` by `sol_out_raw` (bounded by `sol_reserves` in
    // `sell_quote`), so the PDA still covers its reserves afterwards.
    // Floor, not equality: see `math::lamports_cover_curve_reserves`.
    // Fires on every successful path.
    let actual_lamports = ctx.accounts.launched_coin.to_account_info().lamports();
    let rent_exempt = Rent::get()?.minimum_balance(LaunchedCoin::LEN);
    require!(
        math::lamports_cover_curve_reserves(
            actual_lamports,
            rent_exempt,
            ctx.accounts.launched_coin.sol_reserves,
        ),
        LaunchpadError::LamportInvariantViolation
    );

    Ok(())
}

struct SellAccountingAfter {
    tokens_remaining_on_curve: u64,
    sol_reserves: u64,
    cumulative_sol_volume: u128,
    cumulative_token_volume: u128,
    rewards_accumulated_sol: u64,
}

#[allow(clippy::too_many_arguments)]
fn sell_accounting_after(
    tokens_remaining_on_curve: u64,
    sol_reserves: u64,
    cumulative_sol_volume: u128,
    cumulative_token_volume: u128,
    rewards_accumulated_sol: u64,
    tokens_net: u64,
    sol_out_raw: u64,
    tokens_in: u64,
    rewards_fee: u64,
) -> Result<SellAccountingAfter> {
    Ok(SellAccountingAfter {
        tokens_remaining_on_curve: tokens_remaining_on_curve
            .checked_add(tokens_net)
            .ok_or(LaunchpadError::AmountOverflow)?,
        sol_reserves: sol_reserves
            .checked_sub(sol_out_raw)
            .ok_or(LaunchpadError::AmountOverflow)?,
        cumulative_sol_volume: cumulative_sol_volume
            .checked_add(sol_out_raw as u128)
            .ok_or(LaunchpadError::AmountOverflow)?,
        cumulative_token_volume: cumulative_token_volume
            .checked_add(tokens_in as u128)
            .ok_or(LaunchpadError::AmountOverflow)?,
        rewards_accumulated_sol: rewards_accumulated_sol
            .checked_add(rewards_fee)
            .ok_or(LaunchpadError::AmountOverflow)?,
    })
}

fn expected_token_balances_after(
    seller_balance_before: u64,
    vault_balance_before: u64,
    tokens_in: u64,
    tokens_net: u64,
) -> Result<(u64, u64)> {
    let seller_balance_after = seller_balance_before
        .checked_sub(tokens_in)
        .ok_or(LaunchpadError::AmountOverflow)?;
    let vault_balance_after = vault_balance_before
        .checked_add(tokens_net)
        .ok_or(LaunchpadError::AmountOverflow)?;

    Ok((seller_balance_after, vault_balance_after))
}

fn debit_lamports(account: &AccountInfo<'_>, amount: u64) -> Result<()> {
    let balance = account.lamports();
    let balance_after = balance
        .checked_sub(amount)
        .ok_or(LaunchpadError::AmountOverflow)?;
    **account.try_borrow_mut_lamports()? = balance_after;
    Ok(())
}

fn credit_lamports(account: &AccountInfo<'_>, amount: u64) -> Result<()> {
    let balance = account.lamports();
    let balance_after = balance
        .checked_add(amount)
        .ok_or(LaunchpadError::AmountOverflow)?;
    **account.try_borrow_mut_lamports()? = balance_after;
    Ok(())
}

fn preflight_lamport_credits(credits: &[(&AccountInfo<'_>, u64)]) -> Result<()> {
    for (index, (account, _)) in credits.iter().enumerate() {
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
    fn checked_lamport_mutations_accept_boundaries() {
        let mut debit_balance = u64::MAX;
        let debit_account = account_with_lamports(&mut debit_balance);
        debit_lamports(&debit_account, u64::MAX).unwrap();
        assert_eq!(debit_account.lamports(), 0);

        let mut credit_balance = 0;
        let credit_account = account_with_lamports(&mut credit_balance);
        credit_lamports(&credit_account, u64::MAX).unwrap();
        assert_eq!(credit_account.lamports(), u64::MAX);
    }

    #[test]
    fn checked_lamport_mutations_reject_underflow_and_overflow() {
        let mut debit_balance = 0;
        let debit_account = account_with_lamports(&mut debit_balance);
        assert!(debit_lamports(&debit_account, 1).is_err());
        assert_eq!(debit_account.lamports(), 0);

        let mut credit_balance = u64::MAX;
        let credit_account = account_with_lamports(&mut credit_balance);
        assert!(credit_lamports(&credit_account, 1).is_err());
        assert_eq!(credit_account.lamports(), u64::MAX);
    }

    #[test]
    fn lamport_credit_preflight_accepts_distinct_boundary_credits() {
        let mut first_balance = u64::MAX - 1;
        let first = account_with_lamports(&mut first_balance);
        let mut second_balance = 0;
        let second = account_with_lamports(&mut second_balance);

        assert!(preflight_lamport_credits(&[(&first, 1), (&second, u64::MAX)]).is_ok());
        assert_eq!(first.lamports(), u64::MAX - 1);
        assert_eq!(second.lamports(), 0);
    }

    #[test]
    fn lamport_credit_preflight_aggregates_aliased_destinations() {
        let mut balance = u64::MAX - 1;
        let account = account_with_lamports(&mut balance);

        assert!(preflight_lamport_credits(&[(&account, 1), (&account, 1)]).is_err());
        assert!(preflight_lamport_credits(&[(&account, 0), (&account, 1)]).is_ok());
        assert_eq!(account.lamports(), u64::MAX - 1);
    }

    #[test]
    fn lamport_credit_preflight_checks_alias_after_zero_first_credit() {
        let mut balance = u64::MAX;
        let destination = account_with_lamports(&mut balance);

        assert!(preflight_lamport_credits(&[(&destination, 0), (&destination, 1)]).is_err());
        assert_eq!(destination.lamports(), u64::MAX);
    }

    #[test]
    fn sell_accounting_accepts_boundary_values() {
        let after =
            sell_accounting_after(0, 1, u128::MAX - 1, u128::MAX, u64::MAX, 0, 1, 0, 0).unwrap();
        assert_eq!(after.tokens_remaining_on_curve, 0);
        assert_eq!(after.sol_reserves, 0);
        assert_eq!(after.cumulative_sol_volume, u128::MAX);
        assert_eq!(after.cumulative_token_volume, u128::MAX);
        assert_eq!(after.rewards_accumulated_sol, u64::MAX);
    }

    #[test]
    fn sell_accounting_rejects_each_overflow_before_cpis() {
        assert!(sell_accounting_after(u64::MAX, 1, 0, 0, 0, 1, 0, 0, 0).is_err());
        assert!(sell_accounting_after(0, 0, 0, 0, 0, 0, 1, 0, 0).is_err());
        assert!(sell_accounting_after(0, 1, u128::MAX, 0, 0, 0, 1, 0, 0).is_err());
        assert!(sell_accounting_after(0, 1, 0, u128::MAX, 0, 0, 0, 1, 0).is_err());
        assert!(sell_accounting_after(0, 1, 0, 0, u64::MAX, 0, 0, 0, 1).is_err());
    }

    #[test]
    fn sell_token_settlement_accepts_boundary_debit_and_credit() {
        assert_eq!(
            expected_token_balances_after(100, 40, 100, 2).unwrap(),
            (0, 42)
        );
        assert_eq!(
            expected_token_balances_after(u64::MAX, 0, 0, u64::MAX).unwrap(),
            (u64::MAX, u64::MAX)
        );
    }

    #[test]
    fn sell_token_settlement_rejects_underflow_and_overflow_before_cpi() {
        assert!(expected_token_balances_after(0, 0, 1, 0).is_err());
        assert!(expected_token_balances_after(1, u64::MAX, 1, 1).is_err());
    }
}
