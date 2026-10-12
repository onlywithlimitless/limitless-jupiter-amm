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

use crate::constants::BPS_DENOMINATOR;
use crate::errors::LaunchpadError;

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

impl From<MathError> for LaunchpadError {
    fn from(e: MathError) -> Self {
        match e {
            MathError::Overflow => LaunchpadError::AmountOverflow,
            MathError::InsufficientCurveSupply => LaunchpadError::InsufficientCurveSupply,
            MathError::InsufficientSol => LaunchpadError::InsufficientSol,
            MathError::InvalidShares => LaunchpadError::InvalidFeeShares,
            MathError::ZeroAmount => LaunchpadError::InsufficientSol,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sold_out_uses_the_same_rounding_as_buy_and_does_not_confuse_zero_output() {
        assert!(!curve_sold_out(100, 1, 0, 0)); // One lamport buys zero, larger buys work.
        assert!(!curve_sold_out(1, 1, 2, 0)); // ceil reserve: one raw unit is still buyable.
        assert!(curve_sold_out(1, 1, 3, 0));
        assert!(curve_sold_out(114_975_000_000, 27, 280_000_000_000_000, 0));
        for sol in 1..30 {
            for remaining in 1..30 {
                for offset in [0, 1, 30, 1000] {
                    if curve_sold_out(sol, remaining, offset, 0) {
                        for amount in [1, 2, 10, 1000] {
                            assert_eq!(
                                buy_quote(amount, sol, remaining, offset, 0, 6000, 4000, 0, 0, 0),
                                Err(MathError::InsufficientCurveSupply)
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sold_out_transfer_fee_dust_never_has_a_positive_net_buy() {
        for fee in [1, 50, 100, 9_999, 10_000] {
            assert!(curve_sold_out(114_970_893_897, 1, 28_000, fee));
        }
        assert!(!curve_sold_out(114_970_893_897, 1, 28_000, 0));
        for fee in [0, 1, 50, 100] {
            assert!(!curve_sold_out(114_970_893_897, 2, 28_000, fee));
            assert!(
                buy_quote(
                    10_000_000,
                    114_970_893_897,
                    2,
                    28_000,
                    0,
                    10_000,
                    0,
                    0,
                    0,
                    fee
                )
                .unwrap()
                .tokens_received_by_buyer
                    > 0
            );
        }
        // Exhaust small reserve states and all these SOL inputs against the
        // unchanged quote, including fee-free, rounded, and full-fee cases.
        for fee in [0, 1, 50, 100, 9_999, 10_000] {
            for sol in 1..30 {
                for remaining in 0..30 {
                    for offset in [0, 1, 30, 1_000] {
                        if curve_sold_out(sol, remaining, offset, fee) {
                            for amount in 1..100 {
                                if let Ok(quote) = buy_quote(
                                    amount, sol, remaining, offset, 0, 10_000, 0, 0, 0, fee,
                                ) {
                                    assert_eq!(quote.tokens_received_by_buyer, 0);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn audit_p1_buy_cannot_leave_unrepresentable_virtual_sol() {
        use crate::constants::*;
        assert_eq!(
            buy_quote(
                1_000_000_000,
                u64::MAX,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                0,
                6_000,
                4_000,
                0,
                0,
                0
            ),
            Err(MathError::Overflow),
        );
        let buy = buy_quote(
            1_000_000_000,
            u64::MAX - 1_000_000_000,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            0,
            6_000,
            4_000,
            0,
            0,
            0,
        )
        .unwrap();
        let sell = sell_quote(
            buy.tokens_out,
            u64::MAX,
            buy.sol_to_curve,
            STANDARD_TOKEN_ON_CURVE - buy.tokens_out,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            0,
            6_000,
            4_000,
            0,
            0,
            0,
        )
        .unwrap();
        assert!(sell.sol_to_seller > 0 && sell.sol_to_seller <= buy.sol_to_curve);
    }

    #[test]
    fn zero_share_rewards_never_accrue_rounding_dust() {
        for amount in [1, 2, 3, 7, 123, u64::MAX as u128] {
            for platform in (0..=10_000).step_by(37).chain([10_000]) {
                let (p, c, r) = bonding_fee_split(amount, platform, 10_000 - platform, 0).unwrap();
                assert_eq!(r, 0, "amount={amount} platform={platform}");
                assert_eq!(p + c, amount);
                if platform == 0 {
                    assert_eq!(p, 0);
                }
                if platform == 10_000 {
                    assert_eq!(c, 0);
                }
            }
        }
    }

    #[test]
    fn tiny_supply_cannot_extract_another_buyers_sol() {
        let victim = buy_quote(
            1_000_000_000,
            30_000_000_000,
            100,
            0,
            125,
            6000,
            4000,
            0,
            0,
            0,
        )
        .unwrap();
        let remaining = 100 - victim.tokens_out;
        let virtual_sol = 30_000_000_000 + victim.sol_to_curve;
        if let Ok(attack) = buy_quote(1, virtual_sol, remaining, 0, 125, 6000, 4000, 0, 0, 0) {
            let exit = sell_quote(
                attack.tokens_out,
                virtual_sol + attack.sol_to_curve,
                victim.sol_to_curve + attack.sol_to_curve,
                remaining - attack.tokens_out,
                0,
                125,
                6000,
                4000,
                0,
                0,
                0,
            )
            .unwrap();
            assert!(
                exit.sol_to_seller <= 1,
                "one lamport extracted {}",
                exit.sol_to_seller
            );
        }
    }

    #[test]
    fn small_curve_trades_never_reduce_constant_product() {
        for remaining in 1..=100u64 {
            for offset in [0, 1, 100] {
                for amount in 1..=100u64 {
                    let virtual_sol = 1_001;
                    let k = u128::from(virtual_sol) * u128::from(remaining + offset);
                    if let Ok(q) = buy_quote(
                        amount,
                        virtual_sol,
                        remaining,
                        offset,
                        0,
                        6000,
                        4000,
                        0,
                        0,
                        0,
                    ) {
                        assert!(
                            u128::from(virtual_sol + q.sol_to_curve)
                                * u128::from(remaining + offset - q.tokens_out)
                                >= k
                        );
                    }
                    if let Ok(q) = sell_quote(
                        amount,
                        virtual_sol,
                        virtual_sol,
                        remaining,
                        offset,
                        0,
                        6000,
                        4000,
                        0,
                        0,
                        0,
                    ) {
                        assert!(
                            u128::from(virtual_sol - q.sol_out_raw)
                                * u128::from(remaining + offset + q.tokens_net)
                                >= k
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn lamport_invariant_rejects_under_rent_even_with_zero_reserves() {
        assert!(!lamports_match_curve_reserves(99, 100, 0));
        assert!(lamports_match_curve_reserves(100, 100, 0));
        assert!(lamports_match_curve_reserves(101, 100, 1));
        assert!(lamports_match_curve_reserves(101, 100, 0));
        assert!(lamports_match_curve_reserves(u64::MAX, 1, u64::MAX - 1));
    }
    use crate::constants::{
        BONDING_CREATOR_SHARE_BPS, BONDING_CURVE_FEE_BPS, BONDING_PLATFORM_SHARE_BPS,
        BONDING_REWARDS_SHARE_BPS, CHARITY_BPS_MAX, STANDARD_TOKEN_ON_CURVE,
        STANDARD_VIRTUAL_TOKEN_OFFSET,
    };

    // Spec-locked params used across tests (matches `initialize` defaults).
    const FEE_BPS: u16 = BONDING_CURVE_FEE_BPS; // 100
    const PLATFORM_SHARE: u16 = BONDING_PLATFORM_SHARE_BPS; // 5_000
    const CREATOR_SHARE: u16 = BONDING_CREATOR_SHARE_BPS; // 5_000
    const REWARDS_SHARE: u16 = BONDING_REWARDS_SHARE_BPS; // 0 (retired 2026-09-02)
    const NO_CHARITY: u16 = 0;
    const TRANSFER_FEE_BPS: u16 = 50; // Premium default

    const SOL: u64 = 1_000_000_000;
    const INITIAL_SOL_VIRTUAL: u64 = 30 * SOL; // pump-style virtual SOL seed
    const INITIAL_TOKENS_LEGACY_VIRTUAL: u64 = 800_000_000 * 1_000_000; // legacy 800M, no offset

    #[test]
    fn transfer_fee_matches_token_2022_ceiling_boundaries() {
        assert_eq!(transfer_fee(0, 50).unwrap(), 0);
        assert_eq!(transfer_fee(1, 0).unwrap(), 0);
        assert_eq!(transfer_fee(1, 50).unwrap(), 1);
        assert_eq!(transfer_fee(199, 50).unwrap(), 1);
        assert_eq!(transfer_fee(200, 50).unwrap(), 1);
        assert_eq!(transfer_fee(201, 50).unwrap(), 2);
        assert_eq!(
            transfer_fee(u64::MAX as u128, 10_000).unwrap(),
            u64::MAX as u128
        );
    }

    #[test]
    fn sell_quote_uses_token_2022_ceiling_for_net_tokens() {
        let q = sell_quote(
            201,
            INITIAL_SOL_VIRTUAL,
            10 * SOL,
            1_000, // Keep the net 199-token sale worth at least one lamport.
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        )
        .unwrap();

        assert_eq!(q.tokens_net, 199);
    }

    // ---- buy tests ---------------------------------------------------

    #[test]
    fn buy_small_amount_produces_positive_output() {
        let q = buy_quote(
            SOL,
            INITIAL_SOL_VIRTUAL,
            INITIAL_TOKENS_LEGACY_VIRTUAL,
            0, // offset=0 = legacy behavior
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        )
        .unwrap();
        assert!(q.tokens_out > 0);
        assert_eq!(
            q.platform_fee + q.creator_fee + q.rewards_fee + q.charity_fee,
            SOL - q.sol_to_curve,
            "platform + creator + rewards + charity must equal total_fee"
        );
    }

    #[test]
    fn buy_fee_split_sums_exactly_to_total_fee_for_every_input() {
        for (i, sol_in) in (1..=1000u64).map(|i| i * 123_456_789u64).enumerate() {
            let q = buy_quote(
                sol_in,
                INITIAL_SOL_VIRTUAL,
                INITIAL_TOKENS_LEGACY_VIRTUAL,
                0,
                FEE_BPS,
                PLATFORM_SHARE,
                CREATOR_SHARE,
                REWARDS_SHARE,
                NO_CHARITY,
                TRANSFER_FEE_BPS,
            )
            .unwrap();
            let total_fee = q.platform_fee + q.creator_fee + q.rewards_fee + q.charity_fee;
            assert_eq!(
                total_fee + q.sol_to_curve,
                sol_in,
                "run {i}: fee+curve != sol_in"
            );
        }
    }

    #[test]
    fn buy_with_zero_input_errors() {
        assert_eq!(
            buy_quote(
                0,
                INITIAL_SOL_VIRTUAL,
                INITIAL_TOKENS_LEGACY_VIRTUAL,
                0,
                FEE_BPS,
                PLATFORM_SHARE,
                CREATOR_SHARE,
                REWARDS_SHARE,
                NO_CHARITY,
                TRANSFER_FEE_BPS,
            ),
            Err(MathError::ZeroAmount)
        );
    }

    #[test]
    fn buy_with_empty_curve_errors() {
        // offset=0 + tokens_remaining=0 → virtual_tokens_before = 0 → insufficient
        assert_eq!(
            buy_quote(
                SOL,
                INITIAL_SOL_VIRTUAL,
                0,
                0,
                FEE_BPS,
                PLATFORM_SHARE,
                CREATOR_SHARE,
                REWARDS_SHARE,
                NO_CHARITY,
                TRANSFER_FEE_BPS,
            ),
            Err(MathError::InsufficientCurveSupply)
        );
    }

    #[test]
    fn buy_with_invalid_shares_errors() {
        assert_eq!(
            buy_quote(
                SOL,
                INITIAL_SOL_VIRTUAL,
                INITIAL_TOKENS_LEGACY_VIRTUAL,
                0,
                FEE_BPS,
                PLATFORM_SHARE,
                CREATOR_SHARE,
                1_999, // shares sum to 9_999
                NO_CHARITY,
                TRANSFER_FEE_BPS,
            ),
            Err(MathError::InvalidShares)
        );
    }

    #[test]
    fn buy_transfer_fee_withholding_matches_spec_rate() {
        // 50 bps on the vault→buyer transfer is exactly 0.5% of tokens_out.
        let q = buy_quote(
            10 * SOL,
            INITIAL_SOL_VIRTUAL,
            INITIAL_TOKENS_LEGACY_VIRTUAL,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        )
        .unwrap();
        let withheld = q.tokens_out - q.tokens_received_by_buyer;
        let expected = (q.tokens_out as u128 * 50).div_ceil(10_000);
        assert_eq!(withheld as u128, expected);
    }

    // ---- pump.fun-parity virtual_token_offset tests -----------------

    #[test]
    fn offset_zero_matches_legacy_curve_exactly() {
        // With offset=0 and tokens_remaining=800M, we're on the legacy
        // curve shape. Reproduce a historical answer.
        let q_offset = buy_quote(
            5 * SOL,
            INITIAL_SOL_VIRTUAL,
            INITIAL_TOKENS_LEGACY_VIRTUAL,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        )
        .unwrap();
        // Sanity: positive output, reasonable order of magnitude.
        assert!(q_offset.tokens_out > 0);
        // Legacy curve: 800M tokens virtual, 30 SOL virtual, k = 24,000B.
        // 5 SOL net-of-fee ≈ 4.95 SOL enters curve (100 bps fee).
        // tokens_out ≈ 800M × (1 - 30/(30+4.95)) ≈ 113.3M tokens (before transfer fee).
        // So expect >100M and <125M in base units (× 10^6).
        let tokens_out_tokens = q_offset.tokens_out / 1_000_000;
        assert!(tokens_out_tokens > 100_000_000);
        assert!(tokens_out_tokens < 125_000_000);
    }

    #[test]
    fn pump_fun_parity_at_standard_offset() {
        // Using the Standard curve params + pump.fun offset:
        //   tokens_remaining_on_curve = 793.1M × 10^6
        //   virtual_token_offset      = 280M × 10^6
        //   virtual_tokens_before     = 1.073B × 10^6
        //   virtual_sol_before        = 30 SOL
        //   k = 30 × 1.073B = 32.19B × 10^6
        // A fresh 10 SOL buy (net of 100 bps fee ≈ 9.9 SOL into curve)
        // should yield ≈ 268M tokens — matches pump.fun's 10 SOL buy output.
        let q = buy_quote(
            10 * SOL,
            INITIAL_SOL_VIRTUAL,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0, // pump.fun parity math ignores transfer-fee withholding
        )
        .unwrap();
        let tokens_out_whole = q.tokens_out / 1_000_000;
        // Pump.fun documents ~268.3M tokens for a clean 10 SOL buy.
        // Allow ±5M slack to accommodate the 100 bps bonding-fee drag
        // (pump.fun's quoted number isn't bonding-fee adjusted).
        assert!(
            tokens_out_whole > 260_000_000 && tokens_out_whole < 275_000_000,
            "pump.fun parity tokens_out = {tokens_out_whole}, expected ~268.3M"
        );
    }

    #[test]
    fn offset_increases_tokens_per_sol_vs_legacy() {
        // Same SOL in, but with pump.fun's 280M offset: more tokens out
        // than with offset=0 and the same physical 793.1M on curve.
        let legacy = buy_quote(
            SOL,
            INITIAL_SOL_VIRTUAL,
            STANDARD_TOKEN_ON_CURVE,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        )
        .unwrap();
        let with_offset = buy_quote(
            SOL,
            INITIAL_SOL_VIRTUAL,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        )
        .unwrap();
        // With the offset, the curve is flatter — but tokens_out is bounded
        // to physical supply. For 1 SOL far below saturation, offset delivers
        // FEWER tokens per SOL because virtual_tokens_before is larger so the
        // price impact (in token terms) per SOL is smaller.
        //
        // Wait — that's the opposite of "more tokens per SOL." Let's verify:
        //   legacy: dy/dx at x=30, y=793.1M → dy ≈ y * dx/x ≈ 793.1M/30 per SOL
        //   offset: dy/dx at x=30, y=1073M → dy ≈ 1073M/30 per SOL
        // Yes, offset DOES give more tokens per SOL (higher y at same x).
        assert!(
            with_offset.tokens_out > legacy.tokens_out,
            "offset should give more tokens per SOL (legacy={}, offset={})",
            legacy.tokens_out,
            with_offset.tokens_out
        );
    }

    #[test]
    fn offset_cannot_oversell_physical_supply() {
        // Absurd buy against a tiny physical supply (1 token) with huge offset.
        // Curve math would theoretically "sell" far more than exists on curve.
        // The clamp should catch it and return InsufficientCurveSupply.
        let r = buy_quote(
            100 * SOL,
            INITIAL_SOL_VIRTUAL,
            1, // only 1 token physically remains
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        );
        assert_eq!(r, Err(MathError::InsufficientCurveSupply));
    }

    #[test]
    fn offset_overflow_u64_max_inputs_return_error_not_panic() {
        let r = buy_quote(
            SOL,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        );
        // Must not panic; may error on the u128 addition or later.
        let _ = r;
    }

    // ---- charity overlay tests --------------------------------------

    #[test]
    fn charity_zero_matches_no_charity_path() {
        // charity_bps=0 must produce identical values to a run without the
        // charity field.
        let q0 = buy_quote(
            3 * SOL,
            INITIAL_SOL_VIRTUAL,
            INITIAL_TOKENS_LEGACY_VIRTUAL,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            0,
            TRANSFER_FEE_BPS,
        )
        .unwrap();
        assert_eq!(q0.charity_fee, 0);
        // Full fee stays with platform/creator/rewards.
        assert_eq!(
            q0.platform_fee + q0.creator_fee + q0.rewards_fee,
            3 * SOL - q0.sol_to_curve
        );
    }

    #[test]
    fn charity_500bps_extracts_five_percent_of_fee() {
        // 500 bps charity = 5% of total_fee.
        let q = buy_quote(
            100 * SOL,
            INITIAL_SOL_VIRTUAL,
            INITIAL_TOKENS_LEGACY_VIRTUAL,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            500,
            TRANSFER_FEE_BPS,
        )
        .unwrap();
        // DERIVED from FEE_BPS, not hardcoded. This line used to read
        // `SOL * 500 / 10_000`, which silently assumed a 100 bps total fee, so
        // raising the fee to 125 bps failed a test about CHARITY. An
        // expectation that hardcodes another constant's value is a second
        // source of truth (L25/L34).
        //   total_fee    = 100 SOL x FEE_BPS
        //   charity_fee  = total_fee x 500 bps
        let total_fee = 100 * SOL * FEE_BPS as u64 / 10_000;
        let expected_charity = total_fee * 500 / 10_000;
        assert_eq!(q.charity_fee, expected_charity);
        // Total conservation.
        assert_eq!(
            q.platform_fee + q.creator_fee + q.rewards_fee + q.charity_fee + q.sol_to_curve,
            100 * SOL
        );
    }

    #[test]
    fn charity_1000bps_cap_still_conserves_total() {
        // At the 10% cap, all splits still sum to total fee.
        let q = buy_quote(
            50 * SOL,
            INITIAL_SOL_VIRTUAL,
            INITIAL_TOKENS_LEGACY_VIRTUAL,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            1_000,
            TRANSFER_FEE_BPS,
        )
        .unwrap();
        assert_eq!(
            q.platform_fee + q.creator_fee + q.rewards_fee + q.charity_fee + q.sol_to_curve,
            50 * SOL
        );
    }

    // ---- sell tests --------------------------------------------------

    #[test]
    fn sell_small_amount_produces_positive_output() {
        let buy = buy_quote(
            5 * SOL,
            INITIAL_SOL_VIRTUAL,
            INITIAL_TOKENS_LEGACY_VIRTUAL,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        )
        .unwrap();

        let virtual_sol = INITIAL_SOL_VIRTUAL + buy.sol_to_curve;
        let tokens_remaining = INITIAL_TOKENS_LEGACY_VIRTUAL - buy.tokens_out;
        let sol_reserves = buy.sol_to_curve;

        let tokens_in = buy.tokens_received_by_buyer / 2;
        let s = sell_quote(
            tokens_in,
            virtual_sol,
            sol_reserves,
            tokens_remaining,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        )
        .unwrap();
        assert!(s.sol_to_seller > 0);
        assert_eq!(
            s.platform_fee + s.creator_fee + s.rewards_fee + s.charity_fee + s.sol_to_seller,
            s.sol_out_raw,
            "sell fee-split closure broken"
        );
    }

    #[test]
    fn sell_with_zero_input_errors() {
        assert_eq!(
            sell_quote(
                0,
                INITIAL_SOL_VIRTUAL,
                0,
                INITIAL_TOKENS_LEGACY_VIRTUAL,
                0,
                FEE_BPS,
                PLATFORM_SHARE,
                CREATOR_SHARE,
                REWARDS_SHARE,
                NO_CHARITY,
                TRANSFER_FEE_BPS,
            ),
            Err(MathError::ZeroAmount)
        );
    }

    #[test]
    fn sell_exceeding_reserves_errors() {
        let buy = buy_quote(
            SOL,
            INITIAL_SOL_VIRTUAL,
            INITIAL_TOKENS_LEGACY_VIRTUAL,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        )
        .unwrap();
        let virtual_sol = INITIAL_SOL_VIRTUAL + buy.sol_to_curve;
        let tokens_remaining = INITIAL_TOKENS_LEGACY_VIRTUAL - buy.tokens_out;

        let err = sell_quote(
            buy.tokens_received_by_buyer * 1_000,
            virtual_sol,
            buy.sol_to_curve,
            tokens_remaining,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        );
        assert_eq!(err, Err(MathError::InsufficientSol));
    }

    // ---- symmetry (buy-then-sell round-trip) -------------------------

    #[test]
    fn buy_then_sell_preserves_curve_invariant_direction() {
        let buy = buy_quote(
            5 * SOL,
            INITIAL_SOL_VIRTUAL,
            INITIAL_TOKENS_LEGACY_VIRTUAL,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        )
        .unwrap();
        let virtual_sol = INITIAL_SOL_VIRTUAL + buy.sol_to_curve;
        let tokens_remaining = INITIAL_TOKENS_LEGACY_VIRTUAL - buy.tokens_out;

        let sell = sell_quote(
            buy.tokens_received_by_buyer,
            virtual_sol,
            buy.sol_to_curve,
            tokens_remaining,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        )
        .unwrap();

        assert!(
            sell.sol_to_seller < 5 * SOL,
            "round-trip arbitraged against the house: sold {} > bought 5 SOL",
            sell.sol_to_seller
        );
        assert!(
            sell.sol_to_seller > 0,
            "round-trip yielded zero — suspect curve"
        );
    }

    // ---- overflow guards ---------------------------------------------

    #[test]
    fn buy_with_u64_max_inputs_does_not_panic() {
        let r = buy_quote(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        );
        let _ = r;
    }

    #[test]
    fn sell_with_u64_max_inputs_does_not_panic() {
        let r = sell_quote(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            0,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            TRANSFER_FEE_BPS,
        );
        let _ = r;
    }

    #[test]
    fn buy_quote_fuzzes_full_width_money_path_invariants() {
        let mut state = 0x4c49_4d49_544c_4553u64;
        for _ in 0..20_000 {
            let mut next = || {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                state
            };
            let platform_share = (next() % (u64::from(BPS_DENOMINATOR) + 1)) as u16;
            let creator_share = (next() % (u64::from(BPS_DENOMINATOR - platform_share) + 1)) as u16;
            let rewards_share = BPS_DENOMINATOR - platform_share - creator_share;
            let sol_in = next();
            let tokens_remaining = next();

            if let Ok(quote) = buy_quote(
                sol_in,
                next(),
                tokens_remaining,
                next(),
                (next() % (u64::from(BPS_DENOMINATOR) + 1)) as u16,
                platform_share,
                creator_share,
                rewards_share,
                (next() % (u64::from(CHARITY_BPS_MAX) + 1)) as u16,
                (next() % (u64::from(BPS_DENOMINATOR) + 1)) as u16,
            ) {
                let fees = quote
                    .platform_fee
                    .checked_add(quote.creator_fee)
                    .and_then(|amount| amount.checked_add(quote.rewards_fee))
                    .and_then(|amount| amount.checked_add(quote.charity_fee));
                assert_eq!(
                    fees.and_then(|amount| amount.checked_add(quote.sol_to_curve)),
                    Some(sol_in)
                );
                assert!(quote.tokens_out <= tokens_remaining);
                assert!(quote.tokens_received_by_buyer <= quote.tokens_out);
            }
        }
    }

    #[test]
    fn sell_quote_fuzzes_full_width_money_path_invariants() {
        let mut state = 0x5345_4c4c_4c4d_544cu64;
        for _ in 0..20_000 {
            let mut next = || {
                state = state
                    .wrapping_mul(2_862_933_555_777_941_757)
                    .wrapping_add(3_037_000_493);
                state
            };
            let platform_share = (next() % (u64::from(BPS_DENOMINATOR) + 1)) as u16;
            let creator_share = (next() % (u64::from(BPS_DENOMINATOR - platform_share) + 1)) as u16;
            let rewards_share = BPS_DENOMINATOR - platform_share - creator_share;
            let sol_reserves = next();

            if let Ok(quote) = sell_quote(
                next(),
                next(),
                sol_reserves,
                next(),
                next(),
                (next() % (u64::from(BPS_DENOMINATOR) + 1)) as u16,
                platform_share,
                creator_share,
                rewards_share,
                (next() % (u64::from(CHARITY_BPS_MAX) + 1)) as u16,
                (next() % (u64::from(BPS_DENOMINATOR) + 1)) as u16,
            ) {
                let fees = quote
                    .platform_fee
                    .checked_add(quote.creator_fee)
                    .and_then(|amount| amount.checked_add(quote.rewards_fee))
                    .and_then(|amount| amount.checked_add(quote.charity_fee));
                assert_eq!(
                    fees.and_then(|amount| amount.checked_add(quote.sol_to_seller)),
                    Some(quote.sol_out_raw)
                );
                assert!(quote.sol_out_raw <= sol_reserves);
                assert!(quote.tokens_net > 0);
            }
        }
    }

    // ---- bonding_fee_split ------------------------------------------

    #[test]
    fn fee_split_closure_holds_for_tiny_amounts() {
        let (p, c, r) = bonding_fee_split(1, 5_000, 3_000, 2_000).unwrap();
        assert_eq!(p + c + r, 1);
    }

    #[test]
    fn fee_split_closure_holds_for_power_of_two_amounts() {
        for exp in 0..63u32 {
            let total = 1u128 << exp;
            let (p, c, r) = bonding_fee_split(total, 5_000, 3_000, 2_000).unwrap();
            assert_eq!(p + c + r, total, "closure broken at 2^{exp}");
        }
    }

    // ---- full-curve-range pump.fun parity ---------------------------

    /// 85 SOL single buy on a fresh Standard launch.
    /// Expected ~790,998,198 whole tokens (±0.1%) per k=32.19B math.
    /// This exercises the virtual_token_offset trick at high fill.
    #[test]
    fn pump_fun_parity_85_sol_buy() {
        let q = buy_quote(
            85 * SOL,
            INITIAL_SOL_VIRTUAL,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0, // Standard transfer fee is 0 bps
        )
        .unwrap();
        let tokens_out_whole = q.tokens_out / 1_000_000;
        // ±0.1% of 790,998,198 → [790,207,200, 791,789,196]
        assert!(
            (790_207_200..=791_789_196).contains(&tokens_out_whole),
            "85 SOL parity: tokens_out = {tokens_out_whole} whole tokens, expected ~790,998,198 (±0.1%)"
        );
    }

    /// 86 SOL single buy. With STANDARD_VIRTUAL_TOKEN_OFFSET = 280M × 10^6
    /// the curve's virtual inventory is 1,073.1M; pure k-math at 86 SOL
    /// (net 85.14 to curve after 1% fee) computes ~793.5M tokens out,
    /// slightly above the 793.1M physical cap → `InsufficientCurveSupply`.
    /// Pump.fun's IDL-exact offset of 279,900,191 tokens keeps the same
    /// inputs at ~793.44M and just below the cap. At 86 SOL we sit at
    /// the saturation edge — document either outcome as acceptable:
    ///  - `Ok(q)` where tokens_out is within ±0.1% of 793,444,501
    ///  - `Err(InsufficientCurveSupply)` when the offset puts us slightly
    ///    over the cap (safe, no tokens minted that don't exist)
    #[test]
    fn pump_fun_parity_86_sol_buy_edge_of_curve() {
        let r = buy_quote(
            86 * SOL,
            INITIAL_SOL_VIRTUAL,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        );
        match r {
            Ok(q) => {
                let tokens_out_whole = q.tokens_out / 1_000_000;
                // ±0.1% of 793,444,501 → [792,651,056, 794,237,946]
                assert!(
                    (792_651_056..=794_237_946).contains(&tokens_out_whole),
                    "86 SOL parity Ok path: tokens_out = {tokens_out_whole}, expected ~793,444,501 (±0.1%)"
                );
            }
            Err(MathError::InsufficientCurveSupply) => {
                // Expected under the current 280M offset (clamp at 793.1M
                // physical). Acceptable saturation behavior — the curve
                // simply refuses to over-deliver.
            }
            Err(e) => panic!("86 SOL parity: unexpected error {e:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Option C lamport-invariant tests.
    //
    // The runtime invariant enforced at the end of buy/sell/graduate/
    // launch_coin is:
    //
    //     launched_coin.lamports() - rent_exempt == sol_reserves
    //
    // These tests model each handler's lamport + sol_reserves deltas as
    // pure arithmetic and assert the invariant survives the transition.
    // They don't exercise the on-chain account-info plumbing (that's
    // what the validator run does) — they exercise the *accounting
    // contract* every handler promises to uphold.
    //
    // If any of these fail after an Option C edit, the runtime check
    // would also fail — diagnose the state/lamport pairing bug in the
    // handler instead of weakening the assertion.
    // -----------------------------------------------------------------

    /// Synthetic rent-exempt minimum for `LaunchedCoin::LEN`. Exact
    /// value is irrelevant to the invariant — only the *delta* between
    /// `lamports` and `rent_exempt` matters. Chose a plausible number
    /// (~0.00217 SOL) so tests read naturally.
    const MOCK_LAUNCHED_COIN_RENT: u64 = 2_171_520;

    /// Models the invariant check as `lamports - rent == sol_reserves`.
    /// The handlers enforce the floor (`lamports_cover_curve_reserves`);
    /// the exact form is asserted here because a handler that keeps the
    /// pairing exact on a clean PDA trivially keeps the floor as well.
    fn invariant_holds(lamports: u64, sol_reserves: u64) -> bool {
        lamports_match_curve_reserves(lamports, MOCK_LAUNCHED_COIN_RENT, sol_reserves)
    }

    /// The check the trade and launch handlers actually run.
    fn handler_invariant_holds(lamports: u64, rent: u64, sol_reserves: u64) -> bool {
        lamports_cover_curve_reserves(lamports, rent, sol_reserves)
    }

    #[test]
    fn floor_invariant_survives_donation_and_rent_cut_on_every_trade_path() {
        // 1 lamport donated to the PDA before any trade, and the network
        // rent rate cut after launch. Both are outside the program's control
        // and must not brick a single handler.
        const DONATION: u64 = 1;
        const RENT_NOW: u64 = MOCK_LAUNCHED_COIN_RENT - 100;

        let buy = buy_quote(
            5 * SOL,
            INITIAL_SOL_VIRTUAL,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        )
        .unwrap();
        // launch (atomic first buy): rent + sol_to_curve, plus the donation.
        let mut lamports = MOCK_LAUNCHED_COIN_RENT + DONATION + buy.sol_to_curve;
        let mut sol_reserves = buy.sol_to_curve;
        assert!(handler_invariant_holds(lamports, RENT_NOW, sol_reserves));
        assert!(lamports_match_curve_reserves(
            lamports,
            RENT_NOW,
            sol_reserves
        ));

        // sell a quarter back
        let sell = sell_quote(
            buy.tokens_out / 4,
            INITIAL_SOL_VIRTUAL + buy.sol_to_curve,
            sol_reserves,
            STANDARD_TOKEN_ON_CURVE - buy.tokens_out,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        )
        .unwrap();
        assert!(
            sell.sol_out_raw <= sol_reserves,
            "sell never pays out more than reserves"
        );
        lamports -= sell.sol_out_raw;
        sol_reserves -= sell.sol_out_raw;
        assert!(handler_invariant_holds(lamports, RENT_NOW, sol_reserves));

        // graduate: refunds exactly sol_reserves, never the raw balance.
        lamports -= sol_reserves;
        sol_reserves = 0;
        assert!(handler_invariant_holds(lamports, RENT_NOW, sol_reserves));
        // The donation is still on the PDA: nothing paid it out.
        assert_eq!(lamports, MOCK_LAUNCHED_COIN_RENT + DONATION);
    }

    #[test]
    fn floor_invariant_still_rejects_a_short_pda() {
        // Being short is the dangerous direction and stays rejected, on both
        // forms, with reserves zero or not.
        assert!(!handler_invariant_holds(
            MOCK_LAUNCHED_COIN_RENT - 1,
            MOCK_LAUNCHED_COIN_RENT,
            0
        ));
        assert!(!handler_invariant_holds(
            MOCK_LAUNCHED_COIN_RENT + 99,
            MOCK_LAUNCHED_COIN_RENT,
            100
        ));
        assert!(!lamports_match_curve_reserves(
            MOCK_LAUNCHED_COIN_RENT + 99,
            MOCK_LAUNCHED_COIN_RENT,
            100
        ));
    }

    #[test]
    fn invariant_holds_after_normal_buy() {
        // Start: launched_coin just initialised — rent only, no reserves.
        let mut lamports = MOCK_LAUNCHED_COIN_RENT;
        let mut sol_reserves: u64 = 0;
        assert!(invariant_holds(lamports, sol_reserves));

        // Quote a representative buy: 5 SOL in, off the Standard curve.
        let q = buy_quote(
            5 * SOL,
            INITIAL_SOL_VIRTUAL,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        )
        .unwrap();

        // Handler mutation: lamports += sol_to_curve, sol_reserves += sol_to_curve.
        lamports = lamports.checked_add(q.sol_to_curve).unwrap();
        sol_reserves = sol_reserves.checked_add(q.sol_to_curve).unwrap();
        assert!(
            invariant_holds(lamports, sol_reserves),
            "buy broke the invariant: lamports={lamports}, sol_reserves={sol_reserves}"
        );

        // A second buy on top of the first.
        let q2 = buy_quote(
            3 * SOL,
            INITIAL_SOL_VIRTUAL + q.sol_to_curve,
            STANDARD_TOKEN_ON_CURVE - q.tokens_out,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        )
        .unwrap();
        lamports = lamports.checked_add(q2.sol_to_curve).unwrap();
        sol_reserves = sol_reserves.checked_add(q2.sol_to_curve).unwrap();
        assert!(invariant_holds(lamports, sol_reserves));
    }

    #[test]
    fn invariant_holds_after_normal_sell() {
        // Seed the curve with some reserves from a prior buy.
        let buy = buy_quote(
            10 * SOL,
            INITIAL_SOL_VIRTUAL,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        )
        .unwrap();
        let mut lamports = MOCK_LAUNCHED_COIN_RENT + buy.sol_to_curve;
        let mut sol_reserves = buy.sol_to_curve;
        let tokens_remaining = STANDARD_TOKEN_ON_CURVE - buy.tokens_out;
        assert!(invariant_holds(lamports, sol_reserves));

        // Sell back a fraction of the acquired tokens.
        let sell = sell_quote(
            buy.tokens_out / 4,
            INITIAL_SOL_VIRTUAL + buy.sol_to_curve,
            sol_reserves,
            tokens_remaining,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        )
        .unwrap();

        // Handler mutation: lamports -= sol_out_raw, sol_reserves -= sol_out_raw.
        lamports = lamports.checked_sub(sell.sol_out_raw).unwrap();
        sol_reserves = sol_reserves.checked_sub(sell.sol_out_raw).unwrap();
        assert!(
            invariant_holds(lamports, sol_reserves),
            "sell broke the invariant: lamports={lamports}, sol_reserves={sol_reserves}"
        );
    }

    #[test]
    fn invariant_holds_after_graduation() {
        // Stage: curve has hit the 85 SOL graduation threshold.
        let grad_threshold = 85 * SOL;
        let mut lamports = MOCK_LAUNCHED_COIN_RENT + grad_threshold;
        let mut sol_reserves = grad_threshold;
        assert!(invariant_holds(lamports, sol_reserves));

        // Graduate drains the entire curve: lamports -= sol_reserves,
        // then sol_reserves = 0.
        let drain = sol_reserves;
        lamports = lamports.checked_sub(drain).unwrap();
        sol_reserves = 0;

        assert!(
            invariant_holds(lamports, sol_reserves),
            "graduation broke the invariant: lamports={lamports} (expected exactly rent), sol_reserves={sol_reserves}"
        );
        // Post-graduation: lamports are exactly rent-exempt, reserves are 0.
        assert_eq!(lamports, MOCK_LAUNCHED_COIN_RENT);
    }

    #[test]
    fn invariant_holds_after_atomic_launch_plus_buy() {
        // Anchor's `init` just funded launched_coin to rent.
        let mut lamports = MOCK_LAUNCHED_COIN_RENT;
        let mut sol_reserves: u64 = 0;
        assert!(invariant_holds(lamports, sol_reserves));

        // Creator atomic first-buy of 0.001 SOL — the measurement scenario.
        let q = buy_quote(
            SOL / 1_000, // 0.001 SOL in lamports
            INITIAL_SOL_VIRTUAL,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            FEE_BPS,
            PLATFORM_SHARE,
            CREATOR_SHARE,
            REWARDS_SHARE,
            NO_CHARITY,
            0,
        )
        .unwrap();

        // Atomic-buy path: creator → launched_coin transfer of sol_to_curve,
        // then state write sol_reserves += sol_to_curve.
        lamports = lamports.checked_add(q.sol_to_curve).unwrap();
        sol_reserves = sol_reserves.checked_add(q.sol_to_curve).unwrap();
        assert!(
            invariant_holds(lamports, sol_reserves),
            "atomic launch+buy broke the invariant: lamports={lamports}, sol_reserves={sol_reserves}, sol_to_curve={}",
            q.sol_to_curve
        );
    }
}

// ─── Genie allocation split ─────────────────────────────────────────────────
//
// A creator may direct 1-10% of their new token's supply to staked Genie
// holders. That share is MINTED DIRECTLY to the allocation vault during the
// launch instruction, never transferred, because Token-2022's TransferFee
// extension applies to transfers and not to `MintTo`. Minting means the vault
// receives the exact amount the creator chose, with nothing withheld.
//
// Scale the sellable supply, LP headroom and virtual token offset together.
// Taking the allocation only from LP headroom would leave the curve price
// unchanged while raising the pool's opening price (Fable #1).

pub const MIN_GENIE_ALLOCATION_BPS: u16 = 100; // 1%
pub const MAX_GENIE_ALLOCATION_BPS: u16 = 1_000; // 10%

#[derive(Debug, PartialEq, Eq)]
pub struct GenieSupplySplit {
    pub curve_amount: u64,
    pub allocation_amount: u64,
    pub token_on_curve: u64,
    pub virtual_token_offset: u64,
}

/// Preserve the curve/LP/virtual-token proportions in the supply remaining
/// after allocation. Minted amounts sum exactly to total_supply; scaled curve
/// parameters round down by less than one raw unit each.
pub fn split_supply_for_genie_allocation(
    total_supply: u64,
    token_on_curve: u64,
    virtual_token_offset: u64,
    allocation_bps: u16,
) -> Result<GenieSupplySplit, GenieAllocationSplitError> {
    // OFF is an exact identity, before any Genie validation or arithmetic.
    // The launch handler retains its pre-Genie supply/reserve validation.
    if allocation_bps == 0 {
        return Ok(GenieSupplySplit {
            curve_amount: total_supply,
            allocation_amount: 0,
            token_on_curve,
            virtual_token_offset,
        });
    }
    if total_supply == 0 || total_supply < token_on_curve {
        return Err(GenieAllocationSplitError::CurveExceedsSupply);
    }
    if !(MIN_GENIE_ALLOCATION_BPS..=MAX_GENIE_ALLOCATION_BPS).contains(&allocation_bps) {
        return Err(GenieAllocationSplitError::BpsOutOfRange);
    }
    let allocation_amount = (u128::from(total_supply) * u128::from(allocation_bps) / 10_000) as u64;
    if allocation_amount == 0 {
        return Err(GenieAllocationSplitError::AllocationRoundsToZero);
    }
    let curve_amount = total_supply - allocation_amount;
    let scale = |amount: u64| {
        (u128::from(amount) * u128::from(curve_amount) / u128::from(total_supply)) as u64
    };
    Ok(GenieSupplySplit {
        curve_amount,
        allocation_amount,
        token_on_curve: scale(token_on_curve),
        virtual_token_offset: scale(virtual_token_offset),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum GenieAllocationSplitError {
    BpsOutOfRange,
    CurveExceedsSupply,
    AllocationRoundsToZero,
}

#[cfg(test)]
mod genie_allocation_split_tests {
    use super::*;

    #[test]
    fn zero_allocation_split_is_identity_without_genie_validation() {
        // Tiny supplies (which round nonzero allocations to zero), full-width
        // values and even invalid launch inputs pass through unchanged. Only
        // the pre-existing launch validation decides whether a normal launch
        // is valid; none of the three Genie errors can occur on the OFF path.
        for total_supply in [
            0,
            1,
            99,
            100,
            1_000,
            crate::constants::STANDARD_TOTAL_SUPPLY,
            u64::MAX,
        ] {
            for token_on_curve in [0, 1, total_supply, u64::MAX] {
                for virtual_token_offset in [0, 1, 280, u64::MAX] {
                    let split = split_supply_for_genie_allocation(
                        total_supply,
                        token_on_curve,
                        virtual_token_offset,
                        0,
                    )
                    .unwrap();
                    let bytes = [
                        split.allocation_amount,
                        split.curve_amount,
                        split.token_on_curve,
                        split.virtual_token_offset,
                    ]
                    .map(u64::to_le_bytes);
                    assert_eq!(
                        bytes,
                        [0, total_supply, token_on_curve, virtual_token_offset]
                            .map(u64::to_le_bytes)
                    );
                }
            }
        }
    }

    #[test]
    fn scales_curve_headroom_and_offset_and_conserves_supply() {
        for bps in [100, 300, 500, 1_000] {
            for on_curve in [800_000, 950_000, 1_000_000] {
                let split =
                    split_supply_for_genie_allocation(1_000_000, on_curve, 280_000, bps).unwrap();
                assert_eq!(split.allocation_amount, u64::from(bps) * 100);
                assert_eq!(split.curve_amount + split.allocation_amount, 1_000_000);
                assert_eq!(
                    split.token_on_curve,
                    on_curve * (10_000 - u64::from(bps)) / 10_000
                );
                assert_eq!(
                    split.curve_amount - split.token_on_curve,
                    (1_000_000 - on_curve) * (10_000 - u64::from(bps)) / 10_000
                );
                assert_eq!(
                    split.virtual_token_offset,
                    280_000 * (10_000 - u64::from(bps)) / 10_000
                );
            }
        }
    }

    #[test]
    fn rejects_invalid_supply_bps_and_zero_allocation() {
        for bps in [99, 1_001] {
            assert_eq!(
                split_supply_for_genie_allocation(1_000_000, 800_000, 280_000, bps),
                Err(GenieAllocationSplitError::BpsOutOfRange)
            );
        }
        assert_eq!(
            split_supply_for_genie_allocation(99, 80, 28, 100),
            Err(GenieAllocationSplitError::AllocationRoundsToZero)
        );
        for bps in [100, 500, 1_000] {
            assert_eq!(
                split_supply_for_genie_allocation(0, 0, 0, bps),
                Err(GenieAllocationSplitError::CurveExceedsSupply)
            );
            assert_eq!(
                split_supply_for_genie_allocation(100, 101, 28, bps),
                Err(GenieAllocationSplitError::CurveExceedsSupply)
            );
        }
    }

    #[test]
    fn does_not_overflow_at_u64_max() {
        let split = split_supply_for_genie_allocation(u64::MAX, u64::MAX, u64::MAX, 1_000).unwrap();
        assert_eq!(
            u128::from(split.curve_amount) + u128::from(split.allocation_amount),
            u128::from(u64::MAX)
        );
        assert_eq!(split.token_on_curve, split.curve_amount);
        assert_eq!(split.virtual_token_offset, split.curve_amount);
    }
}

#[cfg(test)]
mod lamport_invariant_tests {
    use super::lamports_cover_curve_reserves;

    #[test]
    fn exact_match_passes() {
        assert!(lamports_cover_curve_reserves(1_000 + 500, 1_000, 500));
    }

    #[test]
    fn short_by_one_lamport_fails() {
        assert!(!lamports_cover_curve_reserves(1_000 + 499, 1_000, 500));
    }

    /// The 2026-09-03 devnet break: rent RATE dropped after the PDA was
    /// funded, so the account holds more than rent_exempt + reserves.
    /// This must PASS - it did not before, and every Buy failed.
    fn rent_rate_dropped_after_launch_passes() {
        // funded when rent was 2_000; Rent::get() now returns 1_000
        assert!(lamports_cover_curve_reserves(2_000 + 500, 1_000, 500));
    }

    #[test]
    fn rent_rate_dropped_after_launch_passes_test() {
        rent_rate_dropped_after_launch_passes();
    }

    /// A donation from anyone must not brick the curve either.
    #[test]
    fn donated_lamports_pass() {
        assert!(lamports_cover_curve_reserves(1_000 + 500 + 7, 1_000, 500));
    }

    #[test]
    fn below_rent_exempt_fails_even_with_zero_reserves() {
        assert!(!lamports_cover_curve_reserves(999, 1_000, 0));
    }

    #[test]
    fn overflow_is_rejected_not_wrapped() {
        assert!(!lamports_cover_curve_reserves(u64::MAX, u64::MAX, 1));
    }
}

#[cfg(test)]
mod full_position_sell_tests {
    use super::*;
    use crate::constants::*;

    // The exact mainnet state that reverted on 2026-09-04.
    const SOL_LISTED: u64 = 30_000_000_000;
    const RESERVES: u64 = 6_912_500;
    const REMAINING: u64 = 792_852_819_871_087;
    const ON_CURVE: u64 = 793_100_000_000_000;
    const OFFSET: u64 = 279_900_000_000_000;
    const HELD: u64 = ON_CURVE - REMAINING;

    fn q(reserves: u64) -> Result<SellQuote, MathError> {
        sell_quote(
            HELD,
            SOL_LISTED + reserves,
            reserves,
            REMAINING,
            OFFSET,
            BONDING_CURVE_FEE_BPS,
            BONDING_PLATFORM_SHARE_BPS,
            BONDING_CREATOR_SHARE_BPS,
            BONDING_REWARDS_SHARE_BPS,
            0,
            0,
        )
    }

    #[test]
    fn selling_the_entire_position_succeeds() {
        assert!(
            q(RESERVES).is_ok(),
            "a holder must be able to sell 100% of their position"
        );
    }

    #[test]
    fn the_payout_never_exceeds_what_the_curve_holds() {
        let r = q(RESERVES).unwrap();
        let paid = r.sol_to_seller + r.platform_fee + r.creator_fee + r.rewards_fee + r.charity_fee;
        assert!(paid <= RESERVES, "paid {paid} exceeds reserves {RESERVES}");
    }

    #[test]
    fn a_large_overshoot_still_errors() {
        assert!(
            q(1_000).is_err(),
            "an overshoot far beyond rounding must still be rejected"
        );
    }
}

#[cfg(test)]
mod sell_rounding_bound_tests {
    use super::*;
    /// The bound must be exactly one lamport: a single floored division cannot
    /// err by more, so anything larger would be absorbing a real fault.
    #[test]
    fn the_bound_is_one_lamport_not_a_tolerance_band() {
        assert_eq!(MAX_SELL_ROUNDING_LAMPORTS, 1);
    }
}
