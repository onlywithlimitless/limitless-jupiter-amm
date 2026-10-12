//! Select graduation economics using the immutable launch version.
//! All versions share threshold/sold-out readiness. v0/v1 contribute available
//! inventory up to their recorded LP allocation plus unsold curve tokens.
//! Unaccounted donations cannot enlarge that inventory. The newer price band
//! and unsold coverage rules belong to v2 launches.

use super::graduation_safety;
use crate::{errors::LaunchpadError, state::LaunchedCoin};
use anchor_lang::prelude::*;

pub(super) fn validate_graduation_trigger(coin: &LaunchedCoin) -> Result<()> {
    // Recovery changes only readiness. Each version retains its contribution
    // policy below, including legacy inventory funding and prepared snapshots.
    graduation_safety::validate_graduation_trigger(coin)
}

/// Both directions stop forever at the shared graduation trigger, including
/// old accounts whose completion bit predates this upgrade.
pub(super) fn validate_curve_trading(coin: &LaunchedCoin) -> Result<()> {
    require!(!coin.graduated, LaunchpadError::AlreadyGraduated);
    require!(
        !coin.curve_complete() && !graduation_safety::graduation_trigger_met(coin)?,
        LaunchpadError::CurveComplete
    );
    Ok(())
}

pub(super) fn record_curve_completion(coin: &mut LaunchedCoin) -> Result<()> {
    if graduation_safety::graduation_trigger_met(coin)? {
        coin.mark_curve_complete();
    }
    Ok(())
}

pub(super) fn graduation_pool_tokens(
    coin: &LaunchedCoin,
    vault_tokens: u64,
    pool_sol: u64,
) -> Result<u64> {
    if coin.version >= 2 {
        return graduation_safety::graduation_pool_tokens(coin, vault_tokens, pool_sol);
    }
    legacy_contribution(coin, vault_tokens, pool_sol)
}

pub(super) fn meteora_pool_tokens(
    coin: &LaunchedCoin,
    vault_tokens: u64,
    pool_sol: u64,
) -> Result<u64> {
    if coin.version >= 2 {
        return graduation_safety::meteora_pool_tokens(coin, vault_tokens, pool_sol);
    }
    // Keep a preparation's contribution, but authenticate its inventory bound
    // too: older binaries could snapshot a vault that already held donations.
    let snapshot = coin.meteora_preparation_tokens();
    let tokens = legacy_contribution(
        coin,
        if snapshot > 0 { snapshot } else { vault_tokens },
        pool_sol,
    )?;
    require!(
        vault_tokens >= tokens,
        LaunchpadError::NotReadyForGraduation
    );
    Ok(tokens)
}

fn legacy_contribution(coin: &LaunchedCoin, tokens: u64, pool_sol: u64) -> Result<u64> {
    require!(
        coin.token_on_curve > 0
            && coin.tokens_remaining_on_curve <= coin.token_on_curve
            && pool_sol > 0
            && pool_sol <= coin.sol_reserves,
        LaunchpadError::NotReadyForGraduation
    );
    // Buys debit the vault and unsold inventory by the same gross amount;
    // legacy sells credit both by the same net amount. A direct transfer into
    // the vault changes neither recorded value. Subtract first to support the
    // full u64 range. Historical allocations can leave less available than
    // this bound, so retain the smaller vault/preparation contribution.
    let inventory = coin
        .total_supply
        .checked_sub(coin.token_on_curve)
        .and_then(|lp| lp.checked_add(coin.tokens_remaining_on_curve))
        .ok_or(LaunchpadError::NotReadyForGraduation)?;
    let tokens = tokens.min(inventory);
    require!(tokens > 0, LaunchpadError::NotReadyForGraduation);
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{constants::*, state::GraduationVenue};

    #[test]
    fn completion_is_permanent_before_during_and_after_preparation() {
        for version in [0, 1, 2] {
            for venue in [
                GraduationVenue::Raydium,
                GraduationVenue::LimitlessSwap,
                GraduationVenue::MeteoraDammV2,
            ] {
                let mut coin =
                    LaunchedCoin::try_deserialize_unchecked(&mut &[0; LaunchedCoin::LEN][..])
                        .unwrap();
                coin.version = version;
                coin.graduation_venue = venue;
                coin.graduation_sol_threshold = MIN_GRADUATION_SOL_THRESHOLD;
                coin.sol_reserves = MIN_GRADUATION_SOL_THRESHOLD - 1;
                coin.tokens_remaining_on_curve = 1_000_000;
                validate_curve_trading(&coin).unwrap();
                assert!(validate_graduation_trigger(&coin).is_err());
                coin.sol_reserves += 1;
                validate_graduation_trigger(&coin).unwrap();
                assert!(validate_curve_trading(&coin).is_err()); // legacy zero bit
                record_curve_completion(&mut coin).unwrap();
                assert_eq!(coin.reserved[31], 1);
                if venue == GraduationVenue::MeteoraDammV2 {
                    coin.mark_meteora_prepared(123, 456);
                    coin.sol_reserves -= 123;
                    validate_graduation_trigger(&coin).unwrap();
                    assert!(validate_curve_trading(&coin).is_err());
                    coin.sol_reserves += 123;
                    coin.reserved[..17].fill(0); // recovery never clears completion
                }
                assert!(validate_curve_trading(&coin).is_err());
                coin.sol_reserves = 0;
                assert!(validate_curve_trading(&coin).is_err());
                coin.graduated = true;
                assert!(validate_curve_trading(&coin).is_err());
            }
        }
    }

    #[test]
    fn raydium_cli_vectors_match_on_chain_versioned_policy_and_account_layout() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/raydium_graduation_policy.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            let c = &case["curve"];
            let amount = |field: &str| c[field].as_str().unwrap().parse::<u64>().unwrap();
            let mut coin =
                LaunchedCoin::try_deserialize_unchecked(&mut &[0; LaunchedCoin::LEN][..]).unwrap();
            coin.version = c["version"].as_u64().unwrap() as u8;
            coin.graduated = c["graduated"].as_bool().unwrap();
            coin.transfer_fee_bps = c["transferFeeBps"].as_u64().unwrap() as u16;
            coin.total_supply = amount("totalSupply");
            coin.token_on_curve = amount("tokenOnCurve");
            coin.tokens_remaining_on_curve = amount("tokensRemainingOnCurve");
            coin.sol_listed = amount("solListed");
            coin.sol_reserves = amount("solReserves");
            coin.virtual_token_offset = amount("virtualTokenOffset");
            coin.graduation_sol_threshold = amount("graduationSolThreshold");
            let vault = case["vaultBalance"].as_str().unwrap().parse().unwrap();
            let expected = case["poolTokens"]
                .as_str()
                .map(|v| v.parse::<u64>().unwrap());
            assert_eq!(
                !coin.graduated && validate_graduation_trigger(&coin).is_ok(),
                case["ready"].as_bool().unwrap(),
                "{} readiness",
                case["name"]
            );
            assert_eq!(
                graduation_pool_tokens(&coin, vault, coin.sol_reserves).ok(),
                expected,
                "{} contribution",
                case["name"]
            );
            // These are the command's decoder offsets, checked against actual
            // Anchor serialization rather than a second hand-built account.
            let mut bytes = Vec::new();
            coin.try_serialize(&mut bytes).unwrap();
            assert_eq!(bytes.len(), 344);
            assert_eq!(bytes[266], coin.version);
            assert_eq!(bytes[182], u8::from(coin.graduated));
            assert_eq!(&bytes[107..109], &coin.transfer_fee_bps.to_le_bytes());
            for (offset, value) in [
                (110, coin.total_supply),
                (118, coin.token_on_curve),
                (126, coin.sol_listed),
                (134, coin.tokens_remaining_on_curve),
                (142, coin.sol_reserves),
                (215, coin.graduation_sol_threshold),
                (224, coin.virtual_token_offset),
            ] {
                assert_eq!(&bytes[offset..offset + 8], &value.to_le_bytes());
            }
        }
    }

    fn coin(version: u8, reserves: u64) -> (LaunchedCoin, u64) {
        let quote = crate::math::buy_quote(
            reserves,
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
        let mut coin =
            LaunchedCoin::try_deserialize_unchecked(&mut &[0; LaunchedCoin::LEN][..]).unwrap();
        coin.version = version;
        coin.total_supply = STANDARD_TOTAL_SUPPLY;
        coin.token_on_curve = STANDARD_TOKEN_ON_CURVE;
        coin.sol_listed = STANDARD_SOL_LISTED;
        coin.virtual_token_offset = STANDARD_VIRTUAL_TOKEN_OFFSET;
        coin.sol_reserves = quote.sol_to_curve;
        coin.tokens_remaining_on_curve = STANDARD_TOKEN_ON_CURVE - quote.tokens_out;
        coin.graduation_sol_threshold = reserves;
        (coin, STANDARD_TOTAL_SUPPLY - quote.tokens_out)
    }

    #[test]
    fn legacy_contributions_match_the_original_full_vault_policy() {
        for version in [0, 1] {
            for reserves in [
                MIN_GRADUATION_SOL_THRESHOLD,
                60_000_000_000,
                DEFAULT_GRADUATION_SOL_THRESHOLD,
            ] {
                for venue in [
                    GraduationVenue::Raydium,
                    GraduationVenue::LimitlessSwap,
                    GraduationVenue::MeteoraDammV2,
                ] {
                    let (mut coin, vault) = coin(version, reserves);
                    coin.graduation_venue = venue;
                    validate_graduation_trigger(&coin).unwrap();
                    for fee_bps in [0, 50, 100] {
                        coin.transfer_fee_bps = fee_bps;
                        for tokens in [vault / 2, vault] {
                            assert_eq!(
                                graduation_pool_tokens(&coin, tokens, reserves).unwrap(),
                                tokens
                            );
                        }
                    }
                    assert!(graduation_pool_tokens(&coin, 0, reserves).is_err());
                    assert!(graduation_pool_tokens(&coin, vault, 0).is_err());
                    assert!(graduation_pool_tokens(&coin, vault, reserves + 1).is_err());
                    coin.graduation_sol_threshold += 1;
                    assert!(validate_graduation_trigger(&coin).is_err());
                }
            }
        }
    }

    #[test]
    fn legacy_preparation_excludes_pre_snapshot_donations() {
        // Reproduce the audit finding with circulating tokens, not an
        // impossible vault balance. Neither a new preparation nor a snapshot
        // taken by the vulnerable binary may contribute unaccounted tokens.
        for version in [0, 1] {
            let (mut coin, vault) = coin(version, DEFAULT_GRADUATION_SOL_THRESHOLD);
            coin.graduation_venue = GraduationVenue::MeteoraDammV2;
            validate_graduation_trigger(&coin).unwrap();
            let circulating = coin.total_supply.checked_sub(vault).unwrap();
            assert!(circulating > 0);
            let donated_vault = vault + circulating;
            let pool_sol = coin.sol_reserves;
            let baseline = graduation_pool_tokens(&coin, vault, pool_sol).unwrap();
            let donated = graduation_pool_tokens(&coin, donated_vault, pool_sol).unwrap();
            assert_eq!(donated, baseline);
            let virtual_tokens =
                u128::from(coin.virtual_token_offset) + u128::from(coin.tokens_remaining_on_curve);
            let virtual_sol = u128::from(coin.sol_listed) + u128::from(pool_sol);
            assert!(
                u128::from(pool_sol) * virtual_tokens * u128::from(BPS_DENOMINATOR)
                    < virtual_sol
                        * u128::from(donated_vault)
                        * u128::from(MIN_GRADUATION_PRICE_BPS)
            );
            assert!(graduation_safety::validate_graduation_economics(
                &coin,
                donated_vault,
                pool_sol
            )
            .is_err());
            graduation_safety::validate_graduation_economics(&coin, donated, pool_sol).unwrap();
            assert_eq!(
                meteora_pool_tokens(&coin, donated_vault, pool_sol).unwrap(),
                donated
            );
            // Preserve valid snapshots, and bound previously poisoned ones.
            coin.mark_meteora_prepared(0, baseline);
            assert_eq!(
                meteora_pool_tokens(&coin, donated_vault, pool_sol).unwrap(),
                baseline
            );
            coin.mark_meteora_prepared(0, donated_vault);
            assert_eq!(
                meteora_pool_tokens(&coin, donated_vault, pool_sol).unwrap(),
                donated
            );
        }
    }

    #[test]
    fn legacy_donations_cannot_enlarge_recorded_inventory_on_any_venue() {
        for version in [0, 1] {
            for venue in [
                GraduationVenue::Raydium,
                GraduationVenue::LimitlessSwap,
                GraduationVenue::MeteoraDammV2,
            ] {
                let (mut coin, vault) = coin(version, DEFAULT_GRADUATION_SOL_THRESHOLD);
                coin.graduation_venue = venue;
                for fee_bps in [0, 50, 100] {
                    coin.transfer_fee_bps = fee_bps;
                    for donated_vault in [
                        vault,
                        vault + 11_000_000_000_000,
                        coin.total_supply,
                        u64::MAX,
                    ] {
                        assert_eq!(
                            graduation_pool_tokens(&coin, donated_vault, coin.sol_reserves)
                                .unwrap(),
                            vault
                        );
                        // Exercise absent, valid, and historically poisoned snapshots.
                        for snapshot in [0, vault, donated_vault] {
                            coin.mark_meteora_prepared(0, snapshot);
                            assert_eq!(
                                meteora_pool_tokens(&coin, donated_vault, coin.sol_reserves)
                                    .unwrap(),
                                vault
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn legacy_inventory_rejects_invalid_accounting_and_handles_full_width_values() {
        let (valid, vault) = coin(1, DEFAULT_GRADUATION_SOL_THRESHOLD);
        for (total, on_curve, remaining) in [(0, 0, 0), (1, 2, 0), (10, 5, 6), (10, 10, 0)] {
            let mut coin = valid.clone();
            coin.total_supply = total;
            coin.token_on_curve = on_curve;
            coin.tokens_remaining_on_curve = remaining;
            assert!(graduation_pool_tokens(&coin, vault, coin.sol_reserves).is_err());
        }
        let mut coin = valid;
        coin.total_supply = u64::MAX;
        coin.token_on_curve = u64::MAX - 1;
        coin.tokens_remaining_on_curve = u64::MAX - 2;
        assert_eq!(
            graduation_pool_tokens(&coin, u64::MAX, coin.sol_reserves).unwrap(),
            u64::MAX - 1
        );
        coin.graduation_venue = GraduationVenue::MeteoraDammV2;
        coin.mark_meteora_prepared(0, u64::MAX);
        assert_eq!(
            meteora_pool_tokens(&coin, u64::MAX, coin.sol_reserves).unwrap(),
            u64::MAX - 1
        );
        assert!(meteora_pool_tokens(&coin, u64::MAX - 2, coin.sol_reserves).is_err());
    }

    #[test]
    fn v2_keeps_the_minimum_override_rejection_and_donation_cap() {
        let (low, vault) = coin(2, MIN_GRADUATION_SOL_THRESHOLD);
        validate_graduation_trigger(&low).unwrap();
        assert!(graduation_pool_tokens(&low, vault, low.sol_reserves).is_err());
        let (normal, vault) = coin(2, DEFAULT_GRADUATION_SOL_THRESHOLD);
        assert_eq!(
            graduation_pool_tokens(&normal, vault, normal.sol_reserves).unwrap(),
            vault
        );
        let bounded = graduation_pool_tokens(&normal, u64::MAX, normal.sol_reserves).unwrap();
        assert!(bounded < u64::MAX);
        graduation_safety::validate_graduation_economics(&normal, bounded, normal.sol_reserves)
            .unwrap();
        assert!(graduation_safety::validate_graduation_economics(
            &normal,
            bounded + 1,
            normal.sol_reserves
        )
        .is_err());
    }

    // Recovery must use the actual quote ceiling without changing curve maths.
    #[test]
    fn legacy_280m_offset_cannot_reach_85_sol_but_279_9m_offset_can() {
        let listed = 30_000_000_000u64;
        let supply = 793_100_000_000_000u64;
        let threshold = 85_000_000_000u64;
        for (offset, expected_ceiling, reachable) in [
            (280_000_000_000_000u64, 84_975_000_000u64, false),
            (279_900_000_000_000, 85_005_359_056, true),
        ] {
            let k = u128::from(listed) * u128::from(supply + offset);
            let ceiling = (k / u128::from(offset)) as u64 - listed;
            assert_eq!(ceiling, expected_ceiling);
            let quote =
                |sol| crate::math::buy_quote(sol, listed, supply, offset, 0, 6_000, 4_000, 0, 0, 0);
            assert_eq!(quote(threshold).is_ok(), reachable);
            let drained = quote(ceiling).unwrap();
            // At this scale even one extra lamport would oversell inventory.
            assert_eq!(
                supply - drained.tokens_out,
                if reachable { 1_962 } else { 0 }
            );
            assert!(quote(ceiling + 1).is_err());
            for version in [0, 1, 2] {
                let mut coin =
                    LaunchedCoin::try_deserialize_unchecked(&mut &[0; LaunchedCoin::LEN][..])
                        .unwrap();
                coin.version = version;
                coin.sol_listed = listed;
                coin.virtual_token_offset = offset;
                coin.sol_reserves = drained.sol_to_curve;
                coin.tokens_remaining_on_curve = supply - drained.tokens_out;
                coin.graduation_sol_threshold = threshold;
                validate_graduation_trigger(&coin).unwrap();
            }
        }
    }

    #[test]
    fn every_version_recovers_sold_out_above_the_minimum() {
        for version in [0, 1, 2] {
            let (mut coin, _) = coin(version, MIN_GRADUATION_SOL_THRESHOLD);
            coin.tokens_remaining_on_curve = 0;
            coin.graduation_sol_threshold += 1;
            validate_graduation_trigger(&coin).unwrap();
            coin.sol_reserves = MIN_GRADUATION_SOL_THRESHOLD - 1;
            assert!(validate_graduation_trigger(&coin).is_err());
        }
    }

    #[test]
    fn legacy_meteora_preparation_keeps_its_snapshot_and_accounts_for_rent() {
        for version in [0, 1] {
            let (mut coin, vault) = coin(version, MIN_GRADUATION_SOL_THRESHOLD);
            coin.graduation_venue = GraduationVenue::MeteoraDammV2;
            let rent = 100_000_000;
            coin.sol_reserves -= rent;
            for snapshot in [0, vault] {
                coin.mark_meteora_prepared(rent, snapshot);
                validate_graduation_trigger(&coin).unwrap();
                assert_eq!(
                    meteora_pool_tokens(&coin, vault, coin.sol_reserves).unwrap(),
                    vault
                );
            }
            assert_eq!(
                meteora_pool_tokens(&coin, vault + 1, coin.sol_reserves).unwrap(),
                vault
            );
            assert!(meteora_pool_tokens(&coin, vault - 1, coin.sol_reserves).is_err());
            assert!(meteora_pool_tokens(&coin, vault, coin.sol_reserves + 1).is_err());
            coin.sol_reserves -= 1;
            assert!(validate_graduation_trigger(&coin).is_err());
        }
    }
}
