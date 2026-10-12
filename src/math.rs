//! Pure arithmetic helpers for buy/sell quoting and bonding-fee splits.
//!
//! Everything in this module takes plain integer inputs and returns a
//! typed result — no Solana context, no `AccountInfo`, no `msg!`. That
//! lets us `cargo test --lib` against it directly on the host target
//! and exercise every edge case (overflow, zero-reserves, exact
//! boundaries) in microseconds instead of spinning a validator.
//!
//! The constant-product curve uses `u128` for intermediates. A
//! `u64::MAX` buy against a `u64::MAX` curve multiplies to ~`2^128`,
//! which still fits `u128` by construction.
//!
//! ## Pump.fun-parity virtual token offset
//!
//! Pump.fun uses `virtual_token_reserves = 1.073B` (larger than the real
//! curve allocation of 793.1M). This "headroom trick" flattens the price
//! curve — every buy gives more tokens for the same SOL.
//!
//! We parameterize this by `virtual_token_offset`: the curve uses
//! `virtual_tokens_before = virtual_token_offset + tokens_remaining_on_curve`
//! in its constant-product math. `virtual_token_offset == 0` reproduces the
//! legacy LIMITLESS behavior; `280_000_000 × 10^6` matches pump.fun.

const BPS_DENOMINATOR: u16 = 10_000;

/// Verify that an account's lamports above its rent-exempt reserve cover the
/// curve's stored SOL reserves.
pub fn lamports_match_curve_reserves(
    actual_lamports: u64,
    rent_exempt_lamports: u64,
    sol_reserves: u64,
) -> bool {
    actual_lamports
        .checked_sub(rent_exempt_lamports)
        .is_some_and(|available| available >= sol_reserves)
}

/// Trade/launch-path invariant: the PDA must hold AT LEAST rent + reserves.
///
/// 2026-09-03 — An exact invariant is wrong because `rent_exempt_lamports`
/// comes from `Rent::get()` at call time while the PDA
/// was funded at the rate in force when it LAUNCHED. Solana lowered the devnet
/// rent rate, so `actual - rent_exempt` exceeded `sol_reserves` on every
/// pre-existing token and every Buy failed with LamportInvariantViolation,
/// permanently and unfixably without a program upgrade. Mainnet would have hit
/// the same on its next rent change, and the same exact check lets anyone
/// brick sell + graduate on a token by donating 1 lamport to its PDA.
///
/// Being SHORT is the dangerous direction and is still rejected. Excess is
/// harmless and was never enforceable anyway: anyone may donate lamports to
/// any account.
///
/// Excess is never paid out, so it cannot leak into accounting. Every write
/// to `sol_reserves` comes from a quote delta (`buy`, `sell`, the two launch
/// paths) or sets it to zero (`graduate`); none reads the PDA's lamports.
/// The only lamport debits from a `LaunchedCoin` are `sell`
/// (`sol_out_raw`, clamped DOWN to `sol_reserves` in `sell_quote` — a
/// rounding-scale overshoot of at most `MAX_SELL_ROUNDING_LAMPORTS` is
/// absorbed, anything larger still reverts `InsufficientSol`) and `graduate`
/// (exactly `sol_reserves`). Donated dust stays on the PDA with its rent.
pub fn lamports_cover_curve_reserves(
    actual_lamports: u64,
    rent_exempt_lamports: u64,
    sol_reserves: u64,
) -> bool {
    match rent_exempt_lamports.checked_add(sol_reserves) {
        Some(floor) => actual_lamports >= floor,
        None => false,
    }
}

/// Outcomes of a bonding-curve buy quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuyQuote {
    /// Tokens the curve *sends* from the vault — the gross amount that
    /// leaves `vault_token_account`.
    pub tokens_out: u64,
    /// Tokens the buyer actually receives after Token-2022 withholds
    /// the `TransferFeeConfig` rate on their destination ATA. This is
    /// the number clients must compare against `min_tokens_out`.
    pub tokens_received_by_buyer: u64,
    /// SOL that enters the curve (credited to the `LaunchedCoin` PDA's
    /// lamports — there is no separate vault_sol account).
    pub sol_to_curve: u64,
    pub platform_fee: u64,
    pub creator_fee: u64,
    /// Rewards share, including rounding only when enabled. The three fee
    /// shares sum to exactly `total_fee - charity_fee`.
    pub rewards_fee: u64,
    /// Charity overlay: carved from `total_fee` *before* the three-way
    /// split. `0` when the launched coin has no charity configured.
    pub charity_fee: u64,
}

/// Outcomes of a bonding-curve sell quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SellQuote {
    /// Tokens the vault actually *receives* after Token-2022 withholds
    /// the fee on the incoming transfer. The curve math uses this net
    /// amount, not the seller's gross `tokens_in`.
    pub tokens_net: u64,
    /// SOL the curve returns before bonding fees are deducted.
    pub sol_out_raw: u64,
    /// SOL the seller actually receives after bonding fees.
    pub sol_to_seller: u64,
    pub platform_fee: u64,
    pub creator_fee: u64,
    pub rewards_fee: u64,
    pub charity_fee: u64,
}

/// Errors from pure quote math. The handler layer converts these into
/// the on-chain `LaunchpadError` variants via `From`.
/// How much overshoot a sell may absorb before it is treated as a real
/// accounting fault rather than a rounding artefact.
///
/// ONE. Not a tolerance band, a proof. `sol_out_raw` is a single floored
/// integer division (`k / virtual_tokens_after`), so it can differ from the
/// exact rational payout by strictly less than one lamport. Anything larger is
/// therefore NOT rounding and must still revert.
///
/// Codex reviewed an earlier version of this fix that allowed 16 and rejected
/// it for "silently clamping unexplained discrepancies", which was correct: a
/// tolerance band masks accounting faults. A bound derived from the arithmetic
/// does not.
pub const MAX_SELL_ROUNDING_LAMPORTS: u128 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathError {
    Overflow,
    InsufficientCurveSupply,
    InsufficientSol,
    InvalidShares,
    ZeroAmount,
}

// ---------------------------------------------------------------------------
// Fee split
// ---------------------------------------------------------------------------

/// Split a raw bonding-fee amount (the "remaining" portion after charity
/// carve-out) into platform / creator / rewards shares. Rewards absorb the
/// remainder when enabled; otherwise creator does, or the sole platform share.
pub fn bonding_fee_split(
    remaining_fee: u128,
    platform_share_bps: u16,
    creator_share_bps: u16,
    rewards_share_bps: u16,
) -> Result<(u128, u128, u128), MathError> {
    let bps_denom = BPS_DENOMINATOR as u128;
    if u32::from(platform_share_bps) + u32::from(creator_share_bps) + u32::from(rewards_share_bps)
        != u32::from(BPS_DENOMINATOR)
    {
        return Err(MathError::InvalidShares);
    }

    let platform_fee = remaining_fee
        .checked_mul(platform_share_bps as u128)
        .and_then(|n| n.checked_div(bps_denom))
        .ok_or(MathError::Overflow)?;
    let creator_fee = remaining_fee
        .checked_mul(creator_share_bps as u128)
        .and_then(|n| n.checked_div(bps_denom))
        .ok_or(MathError::Overflow)?;
    let rewards_fee = remaining_fee
        .checked_sub(platform_fee)
        .and_then(|n| n.checked_sub(creator_fee))
        .ok_or(MathError::Overflow)?;

    if rewards_share_bps == 0 {
        return if creator_share_bps > 0 {
            Ok((platform_fee, remaining_fee - platform_fee, 0))
        } else {
            Ok((remaining_fee, 0, 0))
        };
    }
    Ok((platform_fee, creator_fee, rewards_fee))
}

/// Match Token-2022's `TransferFee::calculate_fee`: non-zero fractional fees
/// round up to the next base unit. Launch mints use `u64::MAX` as the maximum
/// fee, so no additional cap is needed here.
pub(crate) fn transfer_fee(amount: u128, transfer_fee_bps: u16) -> Result<u128, MathError> {
    if amount == 0 || transfer_fee_bps == 0 {
        return Ok(0);
    }

    let denominator = BPS_DENOMINATOR as u128;
    amount
        .checked_mul(transfer_fee_bps as u128)
        .and_then(|numerator| numerator.checked_add(denominator - 1))
        .and_then(|rounded| rounded.checked_div(denominator))
        .ok_or(MathError::Overflow)
}

// ---------------------------------------------------------------------------
// Buy
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn buy_quote(
    sol_in: u64,
    virtual_sol_before: u64,
    tokens_remaining_on_curve: u64,
    virtual_token_offset: u64,
    bonding_fee_bps: u16,
    platform_share_bps: u16,
    creator_share_bps: u16,
    rewards_share_bps: u16,
    charity_bps: u16,
    transfer_fee_bps: u16,
) -> Result<BuyQuote, MathError> {
    if sol_in == 0 {
        return Err(MathError::ZeroAmount);
    }
    // `virtual_tokens_before` is the effective curve inventory including the
    // pump.fun-parity offset. `tokens_remaining_on_curve == 0` with a
    // non-zero offset still supports quoting, but beyond graduation the
    // handler short-circuits before we get here.
    let virtual_tokens_before = (virtual_token_offset as u128)
        .checked_add(tokens_remaining_on_curve as u128)
        .ok_or(MathError::Overflow)?;
    if virtual_tokens_before == 0 {
        return Err(MathError::InsufficientCurveSupply);
    }
    if (platform_share_bps as u32) + (creator_share_bps as u32) + (rewards_share_bps as u32)
        != BPS_DENOMINATOR as u32
    {
        return Err(MathError::InvalidShares);
    }

    let bps_denom = BPS_DENOMINATOR as u128;
    let sol_in_128 = sol_in as u128;

    let total_fee = sol_in_128
        .checked_mul(bonding_fee_bps as u128)
        .and_then(|n| n.checked_div(bps_denom))
        .ok_or(MathError::Overflow)?;

    let charity_fee = total_fee
        .checked_mul(charity_bps as u128)
        .and_then(|n| n.checked_div(bps_denom))
        .ok_or(MathError::Overflow)?;
    let remaining_fee = total_fee
        .checked_sub(charity_fee)
        .ok_or(MathError::Overflow)?;

    let (platform_fee, creator_fee, rewards_fee) = bonding_fee_split(
        remaining_fee,
        platform_share_bps,
        creator_share_bps,
        rewards_share_bps,
    )?;

    let sol_to_curve = sol_in_128
        .checked_sub(total_fee)
        .ok_or(MathError::Overflow)?;

    let virtual_sol_128 = virtual_sol_before as u128;

    let k = virtual_sol_128
        .checked_mul(virtual_tokens_before)
        .ok_or(MathError::Overflow)?;
    let virtual_sol_after = virtual_sol_128
        .checked_add(sol_to_curve)
        .ok_or(MathError::Overflow)?;
    // Every later buy/sell reconstructs this reserve with u64::checked_add.
    // Reject before accepting SOL if the resulting curve could not be quoted.
    u128_to_u64(virtual_sol_after)?;
    // Round the remaining reserve UP, so output is rounded down. Rounding
    // output up lets a one-lamport buy obtain an expensive whole token on a
    // small-supply curve and sell it back against other buyers' reserves.
    let virtual_tokens_after = checked_ceil_div(k, virtual_sol_after)?;
    let tokens_out = virtual_tokens_before
        .checked_sub(virtual_tokens_after)
        .ok_or(MathError::Overflow)?;

    if tokens_out == 0 {
        return Err(MathError::InsufficientCurveSupply);
    }
    // The curve cannot sell more tokens than are physically on it.
    // Clamp to `tokens_remaining_on_curve` — the offset is virtual, not
    // available for delivery.
    if tokens_out > tokens_remaining_on_curve as u128 {
        return Err(MathError::InsufficientCurveSupply);
    }

    let transfer_fee_withheld = transfer_fee(tokens_out, transfer_fee_bps)?;
    let tokens_received_by_buyer = tokens_out
        .checked_sub(transfer_fee_withheld)
        .ok_or(MathError::Overflow)?;

    Ok(BuyQuote {
        tokens_out: u128_to_u64(tokens_out)?,
        tokens_received_by_buyer: u128_to_u64(tokens_received_by_buyer)?,
        sol_to_curve: u128_to_u64(sol_to_curve)?,
        platform_fee: u128_to_u64(platform_fee)?,
        creator_fee: u128_to_u64(creator_fee)?,
        rewards_fee: u128_to_u64(rewards_fee)?,
        charity_fee: u128_to_u64(charity_fee)?,
    })
}

/// Whether no buy can deliver spendable tokens from the remaining supply.
/// Net token delivery is monotone in gross output: if transferring the entire
/// remainder is all fee, no smaller purchase can succeed. Otherwise retain the
/// existing one-lamport oversell check, using buy_quote's ceiling of reserves.
pub fn curve_sold_out(
    virtual_sol: u64,
    remaining: u64,
    offset: u64,
    transfer_fee_bps: u16,
) -> bool {
    if remaining == 0
        || transfer_fee(u128::from(remaining), transfer_fee_bps) == Ok(u128::from(remaining))
    {
        return true;
    }
    let virtual_tokens = u128::from(offset) + u128::from(remaining);
    let Some(k) = u128::from(virtual_sol).checked_mul(virtual_tokens) else {
        return false;
    };
    checked_ceil_div(k, u128::from(virtual_sol) + 1)
        .ok()
        .and_then(|retained| virtual_tokens.checked_sub(retained))
        .is_some_and(|out| out > u128::from(remaining))
}

// ---------------------------------------------------------------------------
// Sell
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn sell_quote(
    tokens_in: u64,
    virtual_sol_before: u64,
    sol_reserves: u64,
    tokens_remaining_on_curve: u64,
    virtual_token_offset: u64,
    bonding_fee_bps: u16,
    platform_share_bps: u16,
    creator_share_bps: u16,
    rewards_share_bps: u16,
    charity_bps: u16,
    transfer_fee_bps: u16,
) -> Result<SellQuote, MathError> {
    if tokens_in == 0 {
        return Err(MathError::ZeroAmount);
    }
    if (platform_share_bps as u32) + (creator_share_bps as u32) + (rewards_share_bps as u32)
        != BPS_DENOMINATOR as u32
    {
        return Err(MathError::InvalidShares);
    }

    let bps_denom = BPS_DENOMINATOR as u128;
    let tokens_in_128 = tokens_in as u128;

    let transfer_fee_withheld = transfer_fee(tokens_in_128, transfer_fee_bps)?;
    let tokens_net = tokens_in_128
        .checked_sub(transfer_fee_withheld)
        .ok_or(MathError::Overflow)?;
    if tokens_net == 0 {
        return Err(MathError::ZeroAmount);
    }

    let virtual_sol_128 = virtual_sol_before as u128;
    let virtual_tokens_before = (virtual_token_offset as u128)
        .checked_add(tokens_remaining_on_curve as u128)
        .ok_or(MathError::Overflow)?;
    let k = virtual_sol_128
        .checked_mul(virtual_tokens_before)
        .ok_or(MathError::Overflow)?;
    let virtual_tokens_after = virtual_tokens_before
        .checked_add(tokens_net)
        .ok_or(MathError::Overflow)?;
    // As on buys, retain the fractional output in the curve. Both directions
    // must preserve or increase k, including when a base unit is expensive.
    let virtual_sol_after = checked_ceil_div(k, virtual_tokens_after)?;
    let sol_out_raw = virtual_sol_128
        .checked_sub(virtual_sol_after)
        .ok_or(MathError::Overflow)?;

    if sol_out_raw == 0 {
        return Err(MathError::InsufficientSol);
    }
    // Selling a FULL position rounds the ideal payout ONE lamport above what the
    // curve holds, and this used to revert, so a holder selling 100% could not
    // sell at all. Proven on mainnet 2026-09-04 with a real token:
    //     full-position payout   6,912,501 lamports
    //     curve sol_reserves     6,912,500 lamports
    //     overshoot                      1 lamport  -> InsufficientSol
    // Every sell surface offers this (25/50/75/100 chips in quick-sell, a Max
    // button in the quick-trade drawer), so it blocked the exit on all of them.
    //
    // The curve can never owe more than it holds, so clamping DOWN to reserves
    // is always safe: the seller receives at most what they are owed and the
    // curve can never be drained beyond its balance. But clamping an ARBITRARY
    // overshoot would silently mask a real accounting bug, so only absorb a
    // rounding-scale excess and keep erroring on anything larger.
    let overshoot = sol_out_raw.saturating_sub(sol_reserves as u128);
    if overshoot > MAX_SELL_ROUNDING_LAMPORTS {
        return Err(MathError::InsufficientSol);
    }
    let sol_out_raw = sol_out_raw.min(sol_reserves as u128);

    let total_fee = sol_out_raw
        .checked_mul(bonding_fee_bps as u128)
        .and_then(|n| n.checked_div(bps_denom))
        .ok_or(MathError::Overflow)?;

    let charity_fee = total_fee
        .checked_mul(charity_bps as u128)
        .and_then(|n| n.checked_div(bps_denom))
        .ok_or(MathError::Overflow)?;
    let remaining_fee = total_fee
        .checked_sub(charity_fee)
        .ok_or(MathError::Overflow)?;

    let (platform_fee, creator_fee, rewards_fee) = bonding_fee_split(
        remaining_fee,
        platform_share_bps,
        creator_share_bps,
        rewards_share_bps,
    )?;
    let sol_to_seller = sol_out_raw
        .checked_sub(total_fee)
        .ok_or(MathError::Overflow)?;

    Ok(SellQuote {
        tokens_net: u128_to_u64(tokens_net)?,
        sol_out_raw: u128_to_u64(sol_out_raw)?,
        sol_to_seller: u128_to_u64(sol_to_seller)?,
        platform_fee: u128_to_u64(platform_fee)?,
        creator_fee: u128_to_u64(creator_fee)?,
        rewards_fee: u128_to_u64(rewards_fee)?,
        charity_fee: u128_to_u64(charity_fee)?,
    })
}

fn checked_ceil_div(numerator: u128, denominator: u128) -> Result<u128, MathError> {
    let quotient = numerator
        .checked_div(denominator)
        .ok_or(MathError::Overflow)?;
    // Avoid numerator + denominator - 1, which can overflow valid u128 k.
    quotient
        .checked_add(u128::from(numerator % denominator != 0))
        .ok_or(MathError::Overflow)
}

fn u128_to_u64(v: u128) -> Result<u64, MathError> {
    u64::try_from(v).map_err(|_| MathError::Overflow)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
