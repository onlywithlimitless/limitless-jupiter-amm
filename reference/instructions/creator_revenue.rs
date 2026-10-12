//! Owner-authorized, immutable sharing of curve SOL and actual LP assets.
//! The original recipient and every historical account layout remain intact.
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use crate::{constants::LAUNCHED_COIN_SEED, errors::LaunchpadError, state::LaunchedCoin};

pub const REVENUE_SEED: &[u8] = b"creator_revenue";
pub const REVENUE_LOCK_BYTE: usize = 32;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevenueShare {
    pub wallet: Pubkey,
    pub bps: u16,
}

#[account]
pub struct CreatorRevenue {
    pub mint: Pubkey,
    pub authority: Pubkey,
    pub count: u8,
    pub entries: [RevenueShare; 10],
    pub bump: u8,
}
impl CreatorRevenue {
    pub const LEN: usize = 8 + 32 + 32 + 1 + 10 * 34 + 1;
    pub fn active(&self) -> &[RevenueShare] { &self.entries[..usize::from(self.count & 127)] }
}

fn validate(entries: &[RevenueShare]) -> Result<()> {
    require!(!entries.is_empty() && entries.len() <= 10, LaunchpadError::InvalidRevenueShares);
    let mut total = 0u32;
    for (i, entry) in entries.iter().enumerate() {
        require!(entry.bps > 0 && entry.wallet != Pubkey::default(), LaunchpadError::InvalidRevenueShares);
        require!(solana_curve25519::edwards::validate_edwards(
            &solana_curve25519::edwards::PodEdwardsPoint(entry.wallet.to_bytes())), LaunchpadError::InvalidRevenueShares);
        require!(!entries[..i].iter().any(|e| e.wallet == entry.wallet), LaunchpadError::InvalidRevenueShares);
        total += u32::from(entry.bps);
    }
    require!(total == 10_000, LaunchpadError::InvalidRevenueShares);
    Ok(())
}

/// Only complete proportional batches leave custody; rounding never reallocates ownership.
pub(crate) fn payouts(amount: u64, entries: &[RevenueShare]) -> Result<Vec<u64>> {
    validate(entries)?;
    fn gcd(mut a: u64, mut b: u64) -> u64 { while b != 0 { (a, b) = (b, a % b); } a }
    let divisor = entries.iter().fold(10_000u64, |a, e| gcd(a, u64::from(e.bps)));
    let quantum = 10_000 / divisor;
    let batches = amount / quantum;
    Ok(entries.iter().map(|e| batches * (u64::from(e.bps) / divisor)).collect())
}

#[derive(Accounts)]
pub struct LockCreatorRevenue<'info> {
    #[account(mut, address = launched_coin.creator_fee_recipient @ LaunchpadError::Unauthorized)]
    pub authority: Signer<'info>,
    #[account(mut, seeds = [LAUNCHED_COIN_SEED, launched_coin.mint.as_ref()], bump = launched_coin.bump)]
    pub launched_coin: Box<Account<'info, LaunchedCoin>>,
    #[account(init, payer = authority, space = CreatorRevenue::LEN,
        seeds = [REVENUE_SEED, launched_coin.mint.as_ref()], bump)]
    pub revenue: Box<Account<'info, CreatorRevenue>>,
    pub system_program: Program<'info, System>,
}

pub fn lock(ctx: Context<LockCreatorRevenue>, entries: Vec<RevenueShare>) -> Result<()> {
    validate(&entries)?;
    require!(ctx.accounts.launched_coin.reserved[REVENUE_LOCK_BYTE] == 0, LaunchpadError::CreatorRevenueLocked);
    let revenue = &mut ctx.accounts.revenue;
    revenue.mint = ctx.accounts.launched_coin.mint;
    revenue.authority = ctx.accounts.authority.key();
    revenue.count = entries.len() as u8;
    revenue.entries = [RevenueShare { wallet: Pubkey::default(), bps: 0 }; 10];
    revenue.entries[..entries.len()].copy_from_slice(&entries);
    revenue.bump = ctx.bumps.revenue;
    ctx.accounts.launched_coin.reserved[REVENUE_LOCK_BYTE] = 1;
    Ok(())
}

/// All old trade account metas survive. A locked coin requires its writable
/// custody PDA in remaining_accounts, so omission cannot bypass the lock.
pub fn destination<'info>(coin: &LaunchedCoin, original: AccountInfo<'info>, remaining: &[AccountInfo<'info>]) -> Result<AccountInfo<'info>> {
    if coin.reserved[REVENUE_LOCK_BYTE] == 0 { return Ok(original); }
    require!(coin.reserved[REVENUE_LOCK_BYTE] == 1, LaunchpadError::CreatorRevenueLocked);
    let key = Pubkey::find_program_address(&[REVENUE_SEED, coin.mint.as_ref()], &crate::ID).0;
    let account = remaining.iter().find(|a| a.key == &key).ok_or(LaunchpadError::InvalidRevenueShares)?;
    require!(account.is_writable && account.owner == &crate::ID, LaunchpadError::InvalidRevenueShares);
    let revenue = CreatorRevenue::try_deserialize(&mut &account.try_borrow_data()?[..])?;
    require_keys_eq!(revenue.mint, coin.mint, LaunchpadError::InvalidRevenueShares);
    require_keys_eq!(revenue.authority, coin.creator_fee_recipient, LaunchpadError::InvalidRevenueShares);
    Ok(account.clone())
}

#[derive(Accounts)]
pub struct DistributeCreatorSol<'info> {
    #[account(mut, seeds = [REVENUE_SEED, revenue.mint.as_ref()], bump = revenue.bump)]
    pub revenue: Box<Account<'info, CreatorRevenue>>,
}
pub fn distribute_sol(ctx: Context<DistributeCreatorSol>) -> Result<()> {
    let revenue = &ctx.accounts.revenue;
    let info = revenue.to_account_info();
    let available = info.lamports().checked_sub(Rent::get()?.minimum_balance(CreatorRevenue::LEN))
        .ok_or(LaunchpadError::AmountOverflow)?;
    let values = payouts(available, revenue.active())?;
    require!(ctx.remaining_accounts.len() == (revenue.count & 127) as usize, LaunchpadError::InvalidRevenueShares);
    for ((entry, amount), account) in revenue.active().iter().zip(values).zip(ctx.remaining_accounts) {
        require_keys_eq!(entry.wallet, account.key(), LaunchpadError::InvalidRevenueShares);
        // Crediting lamports does not require ownership of the recipient. A
        // shareholder may allocate, assign, or initialize a nonce at this key;
        // none of those actions may veto the other shareholders' settlement.
        require!(account.is_writable, LaunchpadError::InvalidRevenueShares);
        let after = account.lamports().checked_add(amount).ok_or(LaunchpadError::AmountOverflow)?;
        require!(after >= Rent::get()?.minimum_balance(0), LaunchpadError::InvalidRevenueShares);
        **info.try_borrow_mut_lamports()? -= amount;
        **account.try_borrow_mut_lamports()? = after;
    }
    Ok(())
}

#[derive(Accounts)]
pub struct DistributeCreatorAsset<'info> {
    #[account(seeds = [REVENUE_SEED, revenue.mint.as_ref()], bump = revenue.bump)]
    pub revenue: Box<Account<'info, CreatorRevenue>>,
    #[account(mint::token_program = token_program,
        constraint = asset_mint.key() == revenue.mint || asset_mint.key() == crate::constants::WSOL_MINT @ LaunchpadError::InvalidRevenueShares)]
    pub asset_mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, associated_token::mint = asset_mint, associated_token::authority = revenue,
        associated_token::token_program = token_program)]
    pub custody: Box<InterfaceAccount<'info, TokenAccount>>,
    pub token_program: Interface<'info, TokenInterface>,
}
pub fn distribute_asset<'info>(ctx: Context<'_, '_, 'info, 'info, DistributeCreatorAsset<'info>>) -> Result<()> {
    let a = ctx.accounts;
    require!(a.revenue.count & 128 == 0, LaunchpadError::Unauthorized);
    let values = payouts(a.custody.amount, a.revenue.active())?;
    // Taxable token transfers need every shareholder's consent to batch timing.
    let mut taxable = false;
    if a.token_program.key() == anchor_spl::token_2022::ID {
        use anchor_spl::token_2022::spl_token_2022::extension::{BaseStateWithExtensions, StateWithExtensions, transfer_fee::TransferFeeConfig};
        use anchor_spl::token_2022::spl_token_2022::state::Mint as RawMint;
        let mint_info = a.asset_mint.to_account_info();
        let data = mint_info.try_borrow_data()?;
        let mint = StateWithExtensions::<RawMint>::unpack(&data)?;
        if let Ok(fee) = mint.get_extension::<TransferFeeConfig>() {
            taxable = u16::from(fee.get_epoch_fee(Clock::get()?.epoch).transfer_fee_basis_points) > 0;
        }
    }
    let count = usize::from(a.revenue.count);
    require!(ctx.remaining_accounts.len() == count * if taxable { 2 } else { 1 }, LaunchpadError::InvalidRevenueShares);
    let bump = [a.revenue.bump];
    let seeds: &[&[u8]] = &[REVENUE_SEED, a.revenue.mint.as_ref(), &bump];
    for (i, ((entry, amount), target)) in a.revenue.active().iter().zip(values).zip(ctx.remaining_accounts).enumerate() {
        require!(target.is_writable && target.owner == &a.token_program.key(), LaunchpadError::InvalidRevenueShares);
        let token = InterfaceAccount::<TokenAccount>::try_from(target)?;
        require_keys_eq!(token.mint, a.asset_mint.key(), LaunchpadError::InvalidRevenueShares);
        require_keys_eq!(token.owner, entry.wallet, LaunchpadError::InvalidRevenueShares);
        // Classic SPL ATAs can hand away their owner. Any account for this
        // mint still owned by the beneficiary is a valid replacement.
        if taxable {
            let signer = &ctx.remaining_accounts[count + i];
            require!(signer.is_signer && signer.key == &entry.wallet, LaunchpadError::Unauthorized);
        }
        if amount > 0 {
            token_interface::transfer_checked(CpiContext::new_with_signer(a.token_program.to_account_info(), TransferChecked {
                from: a.custody.to_account_info(), mint: a.asset_mint.to_account_info(), to: target.clone(), authority: a.revenue.to_account_info(),
            }, &[seeds]), amount, a.asset_mint.decimals)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entries() -> Vec<RevenueShare> {
        // Compressed Edwards basepoint and its sign variant are distinct valid wallets.
        let mut a = [0x66; 32]; a[0] = 0x58;
        let mut b = a; b[31] |= 0x80;
        vec![RevenueShare { wallet: Pubkey::new_from_array(a), bps: 3333 }, RevenueShare { wallet: Pubkey::new_from_array(b), bps: 6667 }]
    }
    #[test] fn exact_entitlements_and_dust() {
        let shares = entries();
        assert_eq!(payouts(19_999, &shares).unwrap(), vec![3333, 6667]);
        assert_eq!(payouts(9_999, &shares).unwrap(), vec![0, 0]);
        let amounts = payouts(u64::MAX, &shares).unwrap();
        assert!(amounts.iter().sum::<u64>() <= u64::MAX);
        assert_eq!(u128::from(amounts[0]) * 6667, u128::from(amounts[1]) * 3333);
    }
    #[test] fn malformed_policy_rejected() {
        assert!(validate(&[]).is_err());
        let mut shares = entries(); shares[1].wallet = shares[0].wallet;
        assert!(validate(&shares).is_err());
        shares = entries(); shares[0].bps = 0; assert!(validate(&shares).is_err());
        shares = entries(); shares[1].bps += 1; assert!(validate(&shares).is_err());
        shares = entries(); shares[0].wallet = Pubkey::default(); assert!(validate(&shares).is_err());
    }
}
