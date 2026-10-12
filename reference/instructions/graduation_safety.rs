//! Shared graduation trigger and pool-economics preflight for all venues.
//! Bound token contributions so excess donations remain in curve custody.

use crate::{constants::*, errors::LaunchpadError, state::LaunchedCoin};
use anchor_lang::prelude::*;
use raydium_cp_swap::utils::math::U256;

/// Recover curves whose remaining raw units cannot support another buy, even
/// when rounding/sell-out puts them just below their snapshotted SOL trigger.
/// The minimum SOL, unsold-supply and pool-price floors still apply.
pub(super) fn validate_graduation_trigger(coin: &LaunchedCoin) -> Result<()> {
    require!(
        graduation_trigger_met(coin)?,
        LaunchpadError::NotReadyForGraduation
    );
    Ok(())
}

pub(super) fn graduation_trigger_met(coin: &LaunchedCoin) -> Result<bool> {
    let reserves = coin
        .sol_reserves
        .checked_add(coin.meteora_preparation_rent())
        .ok_or(LaunchpadError::AmountOverflow)?;
    let sold_out = reserves >= MIN_GRADUATION_SOL_THRESHOLD
        && coin
            .sol_listed
            .checked_add(reserves)
            .is_some_and(|virtual_sol| {
                crate::math::curve_sold_out(
                    virtual_sol,
                    coin.tokens_remaining_on_curve,
                    coin.virtual_token_offset,
                    coin.transfer_fee_bps,
                )
            });
    Ok(reserves >= coin.graduation_sol_threshold || sold_out)
}

/// Trading is frozen after Meteora preparation, but holders can still donate
/// tokens. Use the contribution validated before the freeze, leaving excess in
/// curve custody. Older preparations have no snapshot: cap their contribution
/// at the price floor so those frozen curves can recover too.
#[inline(never)]
pub(super) fn meteora_pool_tokens(
    coin: &LaunchedCoin,
    vault_tokens: u64,
    pool_sol: u64,
) -> Result<u64> {
    let snapshot = coin.meteora_preparation_tokens();
    if snapshot > 0 {
        require!(
            vault_tokens >= snapshot,
            LaunchpadError::NotReadyForGraduation
        );
        validate_graduation_economics(coin, snapshot, pool_sol)?;
        return Ok(snapshot);
    }
    graduation_pool_tokens(coin, vault_tokens, pool_sol)
}

/// Bound the gross token contribution by the reserve-price floor, independently
/// of the vault's donated balance. Floor division leaves rounding on the safe
/// side. Excess tokens stay in curve custody; Genie allocations already outside
/// this vault cannot be contributed. This also recovers legacy preparations
/// that did not snapshot a token contribution.
#[inline(never)]
pub(super) fn graduation_pool_tokens(
    coin: &LaunchedCoin,
    vault_tokens: u64,
    pool_sol: u64,
) -> Result<u64> {
    let virtual_tokens =
        u128::from(coin.virtual_token_offset) + u128::from(coin.tokens_remaining_on_curve);
    let virtual_sol = u128::from(coin.sol_listed)
        + u128::from(coin.sol_reserves)
        + u128::from(coin.meteora_preparation_rent());
    require!(virtual_sol > 0, LaunchpadError::NotReadyForGraduation);
    let maximum = U256::from(pool_sol) * U256::from(virtual_tokens) * U256::from(BPS_DENOMINATOR)
        / (U256::from(virtual_sol) * U256::from(MIN_GRADUATION_PRICE_BPS));
    let tokens = maximum.min(U256::from(vault_tokens)).as_u64();
    validate_graduation_economics(coin, tokens, pool_sol)?;
    Ok(tokens)
}

/// Validate the selected contribution, before moving funds.
/// `pool_sol` excludes all curve-funded rent. Use the gross token contribution
/// for the conservative price floor, but the net pool inventory after both
/// transfer fees for the price ceiling and unsold-token coverage.
#[inline(never)] // Keep wide arithmetic out of the graduation handlers' SBF frames.
pub(super) fn validate_graduation_economics(
    coin: &LaunchedCoin,
    gross_tokens: u64,
    pool_sol: u64,
) -> Result<()> {
    // Every venue stages curve vault -> payer -> pool. Launch mints have an
    // immutable snapshotted rate and u64::MAX fee cap, as in buy/sell quotes.
    // Recalculate the rounded-up fee on the reduced amount at the second hop.
    let mut net_pool_tokens = u128::from(gross_tokens);
    for _ in 0..2 {
        net_pool_tokens = net_pool_tokens
            .checked_sub(
                crate::math::transfer_fee(net_pool_tokens, coin.transfer_fee_bps)
                    .map_err(LaunchpadError::from)?,
            )
            .ok_or(LaunchpadError::AmountOverflow)?;
    }
    require!(
        coin.token_on_curve > 0
            && coin.token_on_curve <= coin.total_supply
            && coin.tokens_remaining_on_curve <= coin.token_on_curve
            && coin.sol_listed > 0
            && net_pool_tokens > 0
            && net_pool_tokens >= u128::from(coin.tokens_remaining_on_curve)
            && pool_sol > 0
            && pool_sol <= coin.sol_reserves,
        LaunchpadError::NotReadyForGraduation
    );
    require!(
        coin.version < 2
            || u128::from(coin.tokens_remaining_on_curve) * u128::from(BPS_DENOMINATOR)
                <= u128::from(coin.token_on_curve) * u128::from(MAX_UNSOLD_AT_GRADUATION_BPS),
        LaunchpadError::NotReadyForGraduation
    );

    let virtual_tokens =
        u128::from(coin.virtual_token_offset) + u128::from(coin.tokens_remaining_on_curve);
    // Preparation has already spent some reserves and frozen trading. Restore
    // those lamports only in the reference price, never in pool funding.
    let virtual_sol = u128::from(coin.sol_listed)
        + u128::from(coin.sol_reserves)
        + u128::from(coin.meteora_preparation_rent());
    require!(virtual_tokens > 0, LaunchpadError::NotReadyForGraduation);

    // Keep the gross price >= 95%. Version 2 additionally caps net price at
    // 105%; live v0/v1 allocations removed pool headroom without scaling the
    // curve, so imposing that new ceiling would strand existing coins.
    // Fees only raise the price, so the gross floor remains conservative.
    // Cross-products need up to 144 bits at full-width u64 inputs. U256 avoids
    // overflow and division rounding that could otherwise admit a bad price.
    let pool_value =
        U256::from(pool_sol) * U256::from(virtual_tokens) * U256::from(BPS_DENOMINATOR);
    let curve_floor =
        U256::from(virtual_sol) * U256::from(gross_tokens) * U256::from(MIN_GRADUATION_PRICE_BPS);
    let curve_ceiling = U256::from(virtual_sol)
        * U256::from(net_pool_tokens)
        * U256::from(MAX_GRADUATION_PRICE_BPS);
    require!(
        pool_value >= curve_floor && (coin.version < 2 || pool_value <= curve_ceiling),
        LaunchpadError::NotReadyForGraduation
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::GraduationVenue;

    fn coin() -> LaunchedCoin {
        let mut coin =
            LaunchedCoin::try_deserialize_unchecked(&mut &[0; LaunchedCoin::LEN][..]).unwrap();
        coin.version = 2;
        coin.total_supply = 1_100;
        coin.token_on_curve = 1_000;
        coin.sol_listed = 100;
        coin.sol_reserves = 100;
        coin.virtual_token_offset = 200;
        coin
    }

    #[test]
    fn graduation_price_floor_is_exact_at_95_percent() {
        let coin = coin();
        assert!(validate_graduation_economics(&coin, 100, 95).is_ok());
        assert!(validate_graduation_economics(&coin, 100, 94).is_err());
        assert!(validate_graduation_economics(&coin, 101, 95).is_err());
    }

    #[test]
    fn transfer_fees_on_both_hops_can_breach_the_ceiling_at_the_gross_floor() {
        use anchor_spl::token_2022::spl_token_2022::extension::transfer_fee::TransferFee;

        let fee = TransferFee {
            epoch: 0.into(),
            maximum_fee: TRANSFER_FEE_MAXIMUM.into(),
            transfer_fee_basis_points: 100.into(),
        };
        let staged = 20 - fee.calculate_fee(20).unwrap();
        let net = staged - fee.calculate_fee(staged).unwrap();
        assert_eq!((staged, net), (19, 18));

        for venue in [
            GraduationVenue::Raydium,
            GraduationVenue::LimitlessSwap,
            GraduationVenue::MeteoraDammV2,
        ] {
            let mut coin = coin();
            coin.graduation_venue = venue;
            // At a reference price of one, gross passes the 95% floor and
            // even the first hop passes the ceiling. Only the second hop's
            // net inventory reveals the opening price above 105% (19/18).
            assert!(validate_graduation_economics(&coin, 20, 19).is_ok());
            assert!(validate_graduation_economics(&coin, staged, 19).is_ok());
            assert!(validate_graduation_economics(&coin, net, 19).is_err());
            assert_eq!(graduation_pool_tokens(&coin, 1_000, 19).unwrap(), 20);

            coin.transfer_fee_bps = 100;
            assert!(validate_graduation_economics(&coin, 20, 19).is_err());
            for vault in [20, 1_000, u64::MAX] {
                assert!(graduation_pool_tokens(&coin, vault, 19).is_err());
            }
            if venue == GraduationVenue::MeteoraDammV2 {
                // Both legacy preparations and saved gross contributions
                // must run the same net-inventory ceiling before funding.
                coin.sol_reserves -= 1;
                for snapshot in [0, 20] {
                    coin.mark_meteora_prepared(1, snapshot);
                    assert!(meteora_pool_tokens(&coin, 1_000, 19).is_err());
                }
            }
        }
    }

    #[test]
    fn transfer_fee_ceiling_is_exact_on_net_pool_inventory() {
        let mut coin = coin();
        coin.sol_listed = 2_000;
        coin.sol_reserves = 2_000;
        coin.virtual_token_offset = 4_000;
        coin.transfer_fee_bps = 100;
        // 1,000 gross -> 990 staged -> 980 net at a reference price of one.
        assert!(validate_graduation_economics(&coin, 1_000, 1_029).is_ok());
        assert!(validate_graduation_economics(&coin, 1_000, 1_030).is_err());
    }

    #[test]
    fn net_pool_inventory_must_cover_unsold_tokens() {
        let mut coin = coin();
        coin.total_supply = 110_000;
        coin.token_on_curve = 100_000;
        coin.sol_listed = 1_000;
        coin.sol_reserves = 1_000;
        coin.tokens_remaining_on_curve = 981;
        coin.virtual_token_offset = 1_019;
        assert!(validate_graduation_economics(&coin, 1_000, 1_000).is_ok());
        coin.transfer_fee_bps = 100;
        // Net inventory is 980: the price remains within the band, and
        // unsold supply is below 1%, but the pool cannot cover 981 tokens.
        assert!(validate_graduation_economics(&coin, 1_000, 1_000).is_err());
        assert!(graduation_pool_tokens(&coin, 1_000, 1_000).is_err());
        coin.tokens_remaining_on_curve = 980;
        coin.virtual_token_offset = 1_020;
        assert!(validate_graduation_economics(&coin, 1_000, 1_000).is_ok());
        assert_eq!(graduation_pool_tokens(&coin, 1_000, 1_000).unwrap(), 1_000);
    }

    #[test]
    fn transfer_fees_preserve_the_gross_floor_and_contribution_cap() {
        let mut coin = coin();
        coin.transfer_fee_bps = 100;
        assert!(validate_graduation_economics(&coin, 100, 95).is_ok());
        assert!(validate_graduation_economics(&coin, 100, 94).is_err());
        assert!(validate_graduation_economics(&coin, 101, 95).is_err());
        for vault in [100, 101, 1_000, u64::MAX] {
            assert_eq!(graduation_pool_tokens(&coin, vault, 95).unwrap(), 100);
            assert_eq!(graduation_pool_tokens(&coin, vault, 94).unwrap(), 98);
        }
        // Rounding can consume all inventory even on a sold-out curve.
        assert!(validate_graduation_economics(&coin, 1, 1).is_err());
    }

    #[test]
    fn zero_transfer_fee_preserves_price_boundaries_and_contributions() {
        let mut coin = coin();
        coin.transfer_fee_bps = 0;
        for (tokens, sol, allowed) in [
            (20, 19, true),
            (20, 21, true),
            (20, 18, false),
            (20, 22, false),
        ] {
            assert_eq!(
                validate_graduation_economics(&coin, tokens, sol).is_ok(),
                allowed
            );
        }
        for vault in [20, 21, 1_000, u64::MAX] {
            assert_eq!(graduation_pool_tokens(&coin, vault, 19).unwrap(), 20);
        }
        coin.graduation_venue = GraduationVenue::MeteoraDammV2;
        coin.sol_reserves -= 1;
        for snapshot in [0, 20] {
            coin.mark_meteora_prepared(1, snapshot);
            assert_eq!(meteora_pool_tokens(&coin, 1_000, 19).unwrap(), 20);
        }
    }

    #[test]
    fn pool_contribution_caps_donations_at_the_exact_price_floor() {
        let coin = coin();
        for vault in [100, 101, 1_000, u64::MAX] {
            assert_eq!(graduation_pool_tokens(&coin, vault, 95).unwrap(), 100);
            assert_eq!(graduation_pool_tokens(&coin, vault, 94).unwrap(), 98);
        }
        // Too little liquidity must fail the ceiling even with sufficient SOL.
        assert!(graduation_pool_tokens(&coin, 80, 95).is_err());
        assert!(graduation_pool_tokens(&coin, 0, 95).is_err());
        assert!(graduation_pool_tokens(&coin, 100, 0).is_err());
        assert!(graduation_pool_tokens(&coin, 100, 101).is_err());
    }

    #[test]
    fn bounded_contribution_keeps_unsold_and_full_width_guards() {
        let mut coin = coin();
        coin.tokens_remaining_on_curve = 11;
        assert!(graduation_pool_tokens(&coin, u64::MAX, 100).is_err());
        coin.tokens_remaining_on_curve = 10;
        // Even a huge donation cannot fund fewer tokens than remain unsold.
        assert!(graduation_pool_tokens(&coin, u64::MAX, 1).is_err());
        assert!(graduation_pool_tokens(&coin, 9, 100).is_err());
        coin.total_supply = u64::MAX;
        coin.token_on_curve = u64::MAX;
        coin.virtual_token_offset = u64::MAX;
        coin.sol_listed = u64::MAX;
        coin.sol_reserves = u64::MAX;
        coin.tokens_remaining_on_curve = 0;
        let tokens = graduation_pool_tokens(&coin, u64::MAX, u64::MAX).unwrap();
        assert!(validate_graduation_economics(&coin, tokens, u64::MAX).is_ok());
        assert!(validate_graduation_economics(&coin, tokens + 1, u64::MAX).is_err());
    }

    #[test]
    fn preparation_snapshots_the_bound_and_ignores_later_donations() {
        let mut coin = coin();
        coin.graduation_venue = GraduationVenue::MeteoraDammV2;
        let tokens = graduation_pool_tokens(&coin, 1_000, 95).unwrap();
        coin.sol_reserves -= 5;
        coin.mark_meteora_prepared(5, tokens);
        for vault in [tokens, 1_000, u64::MAX] {
            assert_eq!(meteora_pool_tokens(&coin, vault, 95).unwrap(), tokens);
        }
        assert!(meteora_pool_tokens(&coin, tokens - 1, 95).is_err());
        // A snapshot cannot silently bypass a later rent/price mismatch.
        assert!(meteora_pool_tokens(&coin, 1_000, 94).is_err());
    }

    #[test]
    fn unsold_supply_floor_is_exact_at_one_percent() {
        let mut coin = coin();
        coin.tokens_remaining_on_curve = 10;
        assert!(validate_graduation_economics(&coin, 100, 100).is_ok());
        coin.tokens_remaining_on_curve = 11;
        assert!(validate_graduation_economics(&coin, 100, 100).is_err());
    }

    /// Real standard-curve inventory at a live-supported 60 SOL override.
    /// Version gating must retain the floor, coverage and reserve limits.
    #[test]
    fn legacy_low_override_graduation_preserves_security_bounds() {
        let buy = crate::math::buy_quote(
            60_000_000_000,
            STANDARD_SOL_LISTED,
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
        for version in [0, 1, 2] {
            for venue in [
                GraduationVenue::Raydium,
                GraduationVenue::LimitlessSwap,
                GraduationVenue::MeteoraDammV2,
            ] {
                let mut coin = coin();
                coin.version = version;
                coin.graduation_venue = venue;
                coin.total_supply = STANDARD_TOTAL_SUPPLY;
                coin.token_on_curve = STANDARD_TOKEN_ON_CURVE;
                coin.virtual_token_offset = STANDARD_VIRTUAL_TOKEN_OFFSET;
                coin.sol_listed = STANDARD_SOL_LISTED;
                coin.sol_reserves = buy.sol_to_curve;
                coin.graduation_sol_threshold = 60_000_000_000;
                coin.tokens_remaining_on_curve = STANDARD_TOKEN_ON_CURVE - buy.tokens_out;
                assert!(coin.tokens_remaining_on_curve > STANDARD_TOKEN_ON_CURVE / 100);
                validate_graduation_trigger(&coin).unwrap();
                let vault = STANDARD_TOTAL_SUPPLY - buy.tokens_out;
                let selected = graduation_pool_tokens(&coin, vault, coin.sol_reserves);
                if version == 2 {
                    assert!(selected.is_err());
                    continue;
                }
                let tokens = selected.unwrap();
                validate_graduation_economics(&coin, tokens, coin.sol_reserves).unwrap();
                assert!(
                    validate_graduation_economics(&coin, tokens + 1, coin.sol_reserves).is_err()
                );
                assert!(validate_graduation_economics(
                    &coin,
                    coin.tokens_remaining_on_curve - 1,
                    coin.sol_reserves
                )
                .is_err());
                assert!(
                    validate_graduation_economics(&coin, tokens, coin.sol_reserves + 1).is_err()
                );
            }
        }
    }

    /// Remaining incompatibility: at the live minimum override the price floor
    /// permits fewer tokens than unsold coverage requires. No contribution can
    /// satisfy both properties; do not claim that version gating fixes this.
    #[test]
    fn minimum_live_override_cannot_satisfy_floor_and_unsold_coverage() {
        let buy = crate::math::buy_quote(
            MIN_GRADUATION_SOL_THRESHOLD,
            STANDARD_SOL_LISTED,
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
        for version in [0, 1] {
            let mut coin = coin();
            coin.version = version;
            coin.total_supply = STANDARD_TOTAL_SUPPLY;
            coin.token_on_curve = STANDARD_TOKEN_ON_CURVE;
            coin.virtual_token_offset = STANDARD_VIRTUAL_TOKEN_OFFSET;
            coin.sol_listed = STANDARD_SOL_LISTED;
            coin.sol_reserves = buy.sol_to_curve;
            coin.graduation_sol_threshold = MIN_GRADUATION_SOL_THRESHOLD;
            coin.tokens_remaining_on_curve = STANDARD_TOKEN_ON_CURVE - buy.tokens_out;
            validate_graduation_trigger(&coin).unwrap();
            let maximum_at_floor = u128::from(coin.sol_reserves)
                * (u128::from(coin.virtual_token_offset)
                    + u128::from(coin.tokens_remaining_on_curve))
                * u128::from(BPS_DENOMINATOR)
                / ((u128::from(coin.sol_listed) + u128::from(coin.sol_reserves))
                    * u128::from(MIN_GRADUATION_PRICE_BPS));
            assert!(maximum_at_floor < u128::from(coin.tokens_remaining_on_curve));
            assert!(graduation_pool_tokens(
                &coin,
                STANDARD_TOTAL_SUPPLY - buy.tokens_out,
                coin.sol_reserves
            )
            .is_err());
        }
    }

    #[test]
    fn preparation_rent_cannot_lower_the_reference_curve_price() {
        let mut coin = coin();
        coin.graduation_venue = GraduationVenue::MeteoraDammV2;
        assert!(validate_graduation_economics(&coin, 100, 95).is_ok());
        coin.mark_meteora_prepared(100, 100);
        assert!(validate_graduation_economics(&coin, 100, 95).is_err());
    }

    #[test]
    fn full_width_reserves_and_supply_do_not_wrap() {
        let mut coin = coin();
        coin.total_supply = u64::MAX;
        coin.token_on_curve = u64::MAX;
        coin.virtual_token_offset = u64::MAX;
        coin.sol_reserves = u64::MAX;
        coin.sol_listed = u64::MAX;
        assert!(validate_graduation_economics(&coin, u64::MAX / 2, u64::MAX).is_ok());
        assert!(validate_graduation_economics(&coin, u64::MAX, u64::MAX).is_err());
        coin.tokens_remaining_on_curve = u64::MAX / 100;
        assert!(validate_graduation_economics(&coin, u64::MAX / 2, u64::MAX).is_ok());
        coin.tokens_remaining_on_curve += 1;
        assert!(validate_graduation_economics(&coin, u64::MAX / 2, u64::MAX).is_err());
    }

    #[test]
    fn zero_or_inconsistent_economics_fail_closed() {
        assert!(validate_graduation_economics(&coin(), 0, 100).is_err());
        assert!(validate_graduation_economics(&coin(), 100, 0).is_err());
        assert!(validate_graduation_economics(&coin(), 100, 101).is_err());
        for variant in 0..5 {
            let mut coin = coin();
            match variant {
                0 => coin.token_on_curve = 0,
                1 => coin.total_supply = coin.token_on_curve - 1,
                2 => coin.sol_listed = 0,
                3 => coin.virtual_token_offset = 0,
                _ => coin.tokens_remaining_on_curve = coin.token_on_curve + 1,
            }
            assert!(validate_graduation_economics(&coin, 100, 100).is_err());
        }
        let mut coin = coin();
        coin.tokens_remaining_on_curve = 10;
        assert!(validate_graduation_economics(&coin, 9, 100).is_err());
    }

    #[test]
    fn graduation_price_ceiling_is_exact_at_105_percent() {
        let mut coin = coin();
        coin.sol_reserves = 200;
        coin.sol_listed = 200;
        assert!(validate_graduation_economics(&coin, 50, 105).is_ok());
        assert!(validate_graduation_economics(&coin, 50, 106).is_err());
        assert!(validate_graduation_economics(&coin, 49, 105).is_err());
    }

    #[test]
    fn zero_allocation_graduation_price_unchanged() {
        use crate::math::{buy_quote, split_supply_for_genie_allocation};
        use anchor_spl::token_2022::spl_token_2022::extension::transfer_fee::TransferFee;

        let split = split_supply_for_genie_allocation(
            STANDARD_TOTAL_SUPPLY,
            STANDARD_TOKEN_ON_CURVE,
            STANDARD_VIRTUAL_TOKEN_OFFSET,
            0,
        )
        .unwrap();
        assert_eq!(split.allocation_amount, 0);
        for venue in [
            GraduationVenue::Raydium,
            GraduationVenue::LimitlessSwap,
            GraduationVenue::MeteoraDammV2,
        ] {
            let mut before = coin();
            before.graduation_venue = venue;
            before.total_supply = STANDARD_TOTAL_SUPPLY;
            before.token_on_curve = STANDARD_TOKEN_ON_CURVE;
            before.virtual_token_offset = STANDARD_VIRTUAL_TOKEN_OFFSET;
            before.sol_listed = STANDARD_SOL_LISTED;
            before.graduation_sol_threshold = DEFAULT_GRADUATION_SOL_THRESHOLD;
            let mut normal = before.clone();
            normal.token_on_curve = split.token_on_curve;
            normal.virtual_token_offset = split.virtual_token_offset;

            let advance = |coin: &mut LaunchedCoin, vault: u64| {
                let buy = buy_quote(
                    DEFAULT_GRADUATION_SOL_THRESHOLD,
                    coin.sol_listed,
                    coin.token_on_curve,
                    coin.virtual_token_offset,
                    0,
                    6_000,
                    4_000,
                    0,
                    0,
                    0,
                )
                .unwrap();
                coin.sol_reserves = buy.sol_to_curve;
                coin.tokens_remaining_on_curve = coin.token_on_curve - buy.tokens_out;
                validate_graduation_trigger(coin).unwrap();
                vault - buy.tokens_out
            };
            let before_vault = advance(&mut before, STANDARD_TOTAL_SUPPLY);
            let normal_vault = advance(&mut normal, split.curve_amount);
            assert_eq!(normal.try_to_vec().unwrap(), before.try_to_vec().unwrap());
            assert_eq!(normal_vault, before_vault);

            for rent in [0, 200_000_000] {
                let contribution = |coin: &LaunchedCoin, vault| {
                    let sol = coin.sol_reserves - rent;
                    let tokens = match venue {
                        GraduationVenue::Raydium | GraduationVenue::LimitlessSwap => {
                            graduation_pool_tokens(coin, vault, sol).unwrap()
                        }
                        GraduationVenue::MeteoraDammV2 => {
                            let tokens = graduation_pool_tokens(coin, vault, sol).unwrap();
                            let mut prepared = coin.clone();
                            prepared.sol_reserves -= rent;
                            prepared.mark_meteora_prepared(rent, tokens);
                            meteora_pool_tokens(&prepared, vault, sol).unwrap()
                        }
                    };
                    (sol, tokens)
                };
                let (before_sol, before_tokens) = contribution(&before, before_vault);
                let (normal_sol, normal_tokens) = contribution(&normal, normal_vault);
                for fee_bps in [
                    STANDARD_TRANSFER_FEE_BPS,
                    PREMIUM_TRANSFER_FEE_BPS,
                    SHOPIFY_TRANSFER_FEE_BPS,
                    TRADER_TRANSFER_FEE_BPS,
                    COMMUNITY_TRANSFER_FEE_BPS,
                ] {
                    // All venues stage vault -> payer -> pool, withholding
                    // on each transfer. Compare exact rational opening prices,
                    // without float tolerance or an approximate comparison.
                    let fee = TransferFee {
                        epoch: 0.into(),
                        maximum_fee: u64::MAX.into(),
                        transfer_fee_basis_points: fee_bps.into(),
                    };
                    let pool_net = |gross| {
                        let staged = gross - fee.calculate_fee(gross).unwrap();
                        staged - fee.calculate_fee(staged).unwrap()
                    };
                    let before_price = [before_sol, pool_net(before_tokens)];
                    let normal_price = [normal_sol, pool_net(normal_tokens)];
                    assert_eq!(
                        normal_price.map(u64::to_le_bytes),
                        before_price.map(u64::to_le_bytes),
                        "{venue:?}, rent={rent}, fee={fee_bps}"
                    );
                }
            }
        }
    }

    #[test]
    fn allocated_launches_open_near_spot_for_all_three_venues() {
        use crate::math::{buy_quote, split_supply_for_genie_allocation};
        for venue in [
            GraduationVenue::Raydium,
            GraduationVenue::LimitlessSwap,
            GraduationVenue::MeteoraDammV2,
        ] {
            for allocation_bps in [0, 500, 1_000] {
                let split = split_supply_for_genie_allocation(
                    STANDARD_TOTAL_SUPPLY,
                    STANDARD_TOKEN_ON_CURVE,
                    STANDARD_VIRTUAL_TOKEN_OFFSET,
                    allocation_bps,
                )
                .unwrap();
                let mut coin = coin();
                coin.graduation_venue = venue;
                coin.total_supply = STANDARD_TOTAL_SUPPLY;
                coin.token_on_curve = split.token_on_curve;
                coin.virtual_token_offset = split.virtual_token_offset;
                coin.sol_listed = STANDARD_SOL_LISTED;
                // Use the trading arithmetic to reach the production threshold.
                let buy = buy_quote(
                    DEFAULT_GRADUATION_SOL_THRESHOLD,
                    coin.sol_listed,
                    coin.token_on_curve,
                    coin.virtual_token_offset,
                    0,
                    6000,
                    4000,
                    0,
                    0,
                    0,
                )
                .unwrap();
                coin.sol_reserves = buy.sol_to_curve;
                coin.tokens_remaining_on_curve = coin.token_on_curve - buy.tokens_out;
                let vault = split.curve_amount - buy.tokens_out;
                // Cover curve-funded costs as well as the no-rent reference.
                for rent in [0, 200_000_000] {
                    let sol = coin.sol_reserves - rent;
                    let tokens = graduation_pool_tokens(&coin, vault, sol).unwrap();
                    let ratio = sol as f64
                        * (coin.virtual_token_offset + coin.tokens_remaining_on_curve) as f64
                        / (tokens as f64 * (coin.sol_listed + coin.sol_reserves) as f64);
                    assert!(
                        (0.95..=1.05).contains(&ratio),
                        "{venue:?}, {allocation_bps} bps: {ratio}"
                    );
                    assert_eq!(
                        tokens, vault,
                        "proportional allocation needs no contribution cap"
                    );
                    if venue == GraduationVenue::MeteoraDammV2 {
                        let mut prepared = coin.clone();
                        prepared.sol_reserves -= rent;
                        prepared.mark_meteora_prepared(rent, tokens);
                        assert_eq!(meteora_pool_tokens(&prepared, vault, sol).unwrap(), tokens);
                    }
                }
            }
        }
    }

    #[test]
    fn live_headroom_only_allocations_graduate_on_every_venue() {
        let mut coin = coin();
        coin.total_supply = STANDARD_TOTAL_SUPPLY;
        coin.token_on_curve = STANDARD_TOKEN_ON_CURVE;
        coin.virtual_token_offset = STANDARD_VIRTUAL_TOKEN_OFFSET;
        coin.sol_listed = STANDARD_SOL_LISTED;
        coin.sol_reserves = DEFAULT_GRADUATION_SOL_THRESHOLD;
        coin.tokens_remaining_on_curve = 182_767_624_021;
        let vault = coin.total_supply - coin.token_on_curve + coin.tokens_remaining_on_curve;
        assert!(validate_graduation_economics(&coin, vault, coin.sol_reserves).is_ok());
        for venue in [
            GraduationVenue::Raydium,
            GraduationVenue::LimitlessSwap,
            GraduationVenue::MeteoraDammV2,
        ] {
            coin.graduation_venue = venue;
            for allocation_bps in [500, 1_000] {
                let allocation = coin.total_supply / 10_000 * allocation_bps;
                coin.version = 2;
                assert!(
                    graduation_pool_tokens(&coin, vault - allocation, coin.sol_reserves).is_err()
                );
                for version in [0, 1] {
                    coin.version = version; // live allocation comes only from headroom
                    let tokens =
                        graduation_pool_tokens(&coin, vault - allocation, coin.sol_reserves)
                            .unwrap();
                    assert_eq!(tokens, vault - allocation);
                    if venue == GraduationVenue::MeteoraDammV2 {
                        coin.mark_meteora_prepared(0, tokens);
                        assert_eq!(
                            meteora_pool_tokens(&coin, vault - allocation, coin.sol_reserves)
                                .unwrap(),
                            tokens
                        );
                    }
                }
            }
        }
        assert!(validate_graduation_economics(&coin, vault * 10, coin.sol_reserves).is_err());
    }
}
