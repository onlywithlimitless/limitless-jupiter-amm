//! Admission and trade inventory. A single-buy projection is only a witness of
//! a viable continuation, not a bound on rounding over arbitrary trade paths.
//! Recheck that witness before buys; retain unpaid sell rounding for LPs
//! when crediting all returned tokens would invalidate the witness.
//! Reuse the unchanged trading quotes and graduation economics.

use super::graduation_policy::{graduation_pool_tokens, validate_graduation_trigger};
use crate::{constants::*, errors::LaunchpadError, math, state::*};
use anchor_lang::prelude::*;

/// `vault_supply` excludes the Genie allocation, and curve/offset must already
/// be scaled. Project the net SOL trigger, ignoring bonding fees (the trigger
/// measures SOL actually retained by the curve). No rent deduction budgets the
/// largest token inventory required by the price ceiling. Graduation still
/// rechecks the actual balances, two transfer fees and curve-funded rent.
/// Meteora's minimum also uses the contribution with all graduation rent paid
/// by the curve, since rent can lower the price-floor contribution cap.
///
/// Also check sell-out for offset curves: buying beyond the SOL threshold must
/// not consume headroom needed for liquidity. Thresholds beyond sell-out use the
/// existing sold-out recovery trigger, including its minimum SOL requirement.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(super) fn validate_inventory(
    vault_supply: u64,
    token_on_curve: u64,
    virtual_token_offset: u64,
    sol_listed: u64,
    threshold: u64,
    transfer_fee_bps: u16,
    graduation_venue: GraduationVenue,
    rent: &Rent,
) -> Result<()> {
    require!(
        token_on_curve > 0 && vault_supply >= token_on_curve && sol_listed > 0,
        LaunchpadError::InvalidTokenConfig
    );
    let coin = LaunchedCoin {
        mint: Pubkey::default(),
        creator: Pubkey::default(),
        creator_fee_recipient: Pubkey::default(),
        launch_type: LaunchType::Standard,
        badge_tier: BadgeTier::Standard,
        bonding_curve_type: BondingCurveType::ConstantProduct,
        transfer_fee_bps,
        decimals: 0,
        total_supply: vault_supply,
        token_on_curve,
        sol_listed,
        tokens_remaining_on_curve: token_on_curve,
        sol_reserves: 0,
        cumulative_sol_volume: 0,
        cumulative_token_volume: 0,
        graduated: false,
        preferred_swap_venue: Pubkey::default(),
        graduation_sol_threshold: threshold,
        bump: 0,
        virtual_token_offset,
        charity_wallet: Pubkey::default(),
        charity_bps: 0,
        version: 2,
        graduation_venue,
        reserved: [0; 76],
    };
    validate_continuation(&coin, vault_supply, rent)
        .map_err(|_| error!(LaunchpadError::InvalidTokenConfig))
}

/// Reject before paying for a buy that would exhaust graduation headroom.
/// Checking only when the trigger is reached is too late: earlier rounding can
/// already leave no feasible completion. The vault balance is spendable tokens,
/// excluding allocations/withheld fees, and may include holder donations.
/// Legacy curves retain their original rules (including no v2 price ceiling).
#[inline(never)]
pub(super) fn validate_buy_inventory(
    coin: &LaunchedCoin,
    vault_tokens: u64,
    quote: &math::BuyQuote,
    rent: &Rent,
) -> Result<()> {
    let mut after = coin.clone();
    after.tokens_remaining_on_curve = after
        .tokens_remaining_on_curve
        .checked_sub(quote.tokens_out)
        .ok_or(LaunchpadError::AmountOverflow)?;
    after.sol_reserves = after
        .sol_reserves
        .checked_add(quote.sol_to_curve)
        .ok_or(LaunchpadError::AmountOverflow)?;
    let vault_after = vault_tokens
        .checked_sub(quote.tokens_out)
        .ok_or(LaunchpadError::AmountOverflow)?;
    validate_continuation(&after, vault_after, rent)
}

/// Keep the token equivalent of unpaid sell rounding in the liquidity reserve.
/// The seller still receives the unmodified quote and the vault still receives
/// every net token. Only the split between saleable and LP inventory changes.
/// If the ordinary transition is feasible, keep its inventory unchanged.
/// Otherwise, if x' is the quoted retained SOL, crediting at most floor(k / x')
/// tokens prevents rounding from increasing the product used by the next buy.
/// The excess is bounded by the returned tokens: x' <= x implies floor(k/x') >= y.
/// This does not gate holder exits on graduation readiness, including exits
/// from already infeasible states. Legacy curves keep their original accounting.
#[inline(never)]
pub(super) fn sell_curve_inventory(
    coin: &LaunchedCoin,
    vault_tokens: u64,
    quote: &math::SellQuote,
    rent: &Rent,
) -> Result<u64> {
    let remaining = coin
        .tokens_remaining_on_curve
        .checked_add(quote.tokens_net)
        .ok_or(LaunchpadError::AmountOverflow)?;
    if coin.version < 2 {
        return Ok(remaining);
    }
    let mut after = coin.clone();
    after.tokens_remaining_on_curve = remaining;
    after.sol_reserves = coin
        .sol_reserves
        .checked_sub(quote.sol_out_raw)
        .ok_or(LaunchpadError::AmountOverflow)?;
    let vault_after = vault_tokens
        .checked_add(quote.tokens_net)
        .ok_or(LaunchpadError::AmountOverflow)?;
    if validate_continuation(&after, vault_after, rent).is_ok() {
        return Ok(remaining);
    }
    let virtual_sol = coin
        .sol_listed
        .checked_add(coin.sol_reserves)
        .ok_or(LaunchpadError::AmountOverflow)?;
    let virtual_tokens =
        u128::from(coin.virtual_token_offset) + u128::from(coin.tokens_remaining_on_curve);
    let sol_after = virtual_sol
        .checked_sub(quote.sol_out_raw)
        .filter(|sol| *sol > 0)
        .ok_or(LaunchpadError::AmountOverflow)?;
    let retained = u128::from(virtual_sol)
        .checked_mul(virtual_tokens)
        .ok_or(LaunchpadError::AmountOverflow)?
        / u128::from(sol_after);
    let saleable = retained
        .checked_sub(u128::from(coin.virtual_token_offset))
        .ok_or(LaunchpadError::AmountOverflow)?;
    Ok(remaining.min(u64::try_from(saleable).map_err(|_| LaunchpadError::AmountOverflow)?))
}

#[inline(never)]
fn validate_continuation(coin: &LaunchedCoin, vault_tokens: u64, rent: &Rent) -> Result<()> {
    let virtual_sol = coin
        .sol_listed
        .checked_add(coin.sol_reserves)
        .ok_or(LaunchpadError::AmountOverflow)?;
    let virtual_tokens = coin
        .tokens_remaining_on_curve
        .checked_add(coin.virtual_token_offset)
        .ok_or(LaunchpadError::AmountOverflow)?;
    let sellout = if math::curve_sold_out(
        virtual_sol,
        coin.tokens_remaining_on_curve,
        coin.virtual_token_offset,
        coin.transfer_fee_bps,
    ) {
        Some(coin.sol_reserves)
    } else if coin.virtual_token_offset > 0 {
        let k = u128::from(virtual_sol) * u128::from(virtual_tokens);
        let offset = u128::from(coin.virtual_token_offset);
        let mut terminal_sol = k.div_ceil(offset);
        if offset > 1 {
            // A lamport can buy many raw units. Rounding SOL up to exact
            // sell-out may oversell physical inventory. The largest valid
            // input retains ceil(k / SOL) >= offset, i.e. SOL < k/(offset-1).
            terminal_sol = terminal_sol.min((k - 1) / (offset - 1));
        }
        u64::try_from(terminal_sol - u128::from(coin.sol_listed)).ok()
    } else {
        None
    };
    let threshold = coin.graduation_sol_threshold.max(coin.sol_reserves);
    let first_trigger = sellout.map_or(threshold, |sold_out| threshold.min(sold_out));
    for reserves in [Some(first_trigger), sellout.filter(|s| *s > first_trigger)]
        .into_iter()
        .flatten()
    {
        let mut projected = coin.clone();
        let tokens_out = if reserves > coin.sol_reserves {
            let quote = math::buy_quote(
                reserves - coin.sol_reserves,
                virtual_sol,
                coin.tokens_remaining_on_curve,
                coin.virtual_token_offset,
                0,
                BPS_DENOMINATOR,
                0,
                0,
                0,
                coin.transfer_fee_bps,
            )
            .map_err(|_| error!(LaunchpadError::NotReadyForGraduation))?;
            require!(
                quote.tokens_received_by_buyer > 0,
                LaunchpadError::NotReadyForGraduation
            );
            projected.tokens_remaining_on_curve -= quote.tokens_out;
            projected.sol_reserves = reserves;
            quote.tokens_out
        } else {
            0
        };
        let mut vault_after = vault_tokens
            .checked_sub(tokens_out)
            .ok_or(LaunchpadError::NotReadyForGraduation)?;
        if Some(reserves) == sellout
            && !math::curve_sold_out(
                projected.sol_listed + projected.sol_reserves,
                projected.tokens_remaining_on_curve,
                projected.virtual_token_offset,
                projected.transfer_fee_bps,
            )
        {
            // Retained-token rounding can make one final lamport tradable.
            // At the maximal valid input above, that buy either exceeds the
            // remaining supply (already sold out), or consumes all of it.
            let dust = math::buy_quote(
                1,
                projected.sol_listed + projected.sol_reserves,
                projected.tokens_remaining_on_curve,
                projected.virtual_token_offset,
                0,
                BPS_DENOMINATOR,
                0,
                0,
                0,
                coin.transfer_fee_bps,
            )
            .map_err(|_| error!(LaunchpadError::NotReadyForGraduation))?;
            require!(
                dust.tokens_received_by_buyer > 0,
                LaunchpadError::NotReadyForGraduation
            );
            projected.tokens_remaining_on_curve -= dust.tokens_out;
            projected.sol_reserves = projected
                .sol_reserves
                .checked_add(dust.sol_to_curve)
                .ok_or(LaunchpadError::AmountOverflow)?;
            vault_after = vault_after
                .checked_sub(dust.tokens_out)
                .ok_or(LaunchpadError::NotReadyForGraduation)?;
        }
        let reserves = projected.sol_reserves;
        validate_graduation_trigger(&projected)?;
        graduation_pool_tokens(&projected, vault_after, reserves)?;
        if coin.graduation_venue == GraduationVenue::MeteoraDammV2 {
            // Preparation requires this minimum after curve -> funding -> pool.
            // Check the selected contribution, not total supply or vault balance:
            // the price floor may cap how much of that balance can enter the pool.
            let pool_sol = reserves
                .checked_sub(meteora_graduation_rent(rent)?)
                .filter(|sol| *sol >= super::graduate_meteora::MIN_METEORA_RESERVE)
                .ok_or(LaunchpadError::NotReadyForGraduation)?;
            let mut pool_tokens = graduation_pool_tokens(&projected, vault_after, pool_sol)?;
            for _ in 0..2 {
                let fee = math::transfer_fee(u128::from(pool_tokens), coin.transfer_fee_bps)
                    .map_err(LaunchpadError::from)?;
                pool_tokens = u64::try_from(u128::from(pool_tokens) - fee)
                    .map_err(|_| error!(LaunchpadError::NotReadyForGraduation))?;
            }
            require!(
                pool_tokens >= super::graduate_meteora::MIN_METEORA_RESERVE,
                LaunchpadError::NotReadyForGraduation
            );
        }
    }
    Ok(())
}

// Same empty-account sizes as Meteora preparation. New Meteora mints have
// TransferFeeConfig + TransferHook; metadata adds no token-account extension.
// Prefunding can only reduce this budget, so admission does not depend on it.
fn meteora_graduation_rent(rent: &Rent) -> Result<u64> {
    use anchor_spl::token_2022::spl_token_2022::{extension::ExtensionType, state};
    let mut extensions = vec![
        ExtensionType::TransferFeeAmount,
        ExtensionType::TransferHookAccount,
    ];
    let vault_len = ExtensionType::try_calculate_account_len::<state::Account>(&extensions)?;
    extensions.push(ExtensionType::ImmutableOwner);
    let ata_len = ExtensionType::try_calculate_account_len::<state::Account>(&extensions)?;
    [
        MeteoraGraduation::LEN,
        ata_len,
        165,
        1112,
        408,
        408,
        465,
        465,
        165,
        165,
        vault_len,
        165,
    ]
    .into_iter()
    .try_fold(0u64, |total, size| {
        total
            .checked_add(rent.minimum_balance(size))
            .ok_or_else(|| error!(LaunchpadError::InvalidTokenConfig))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_buys_keep_a_full_vault_continuation_through_dust_sellout() {
        for version in [0, 1] {
            for venue in [
                GraduationVenue::Raydium,
                GraduationVenue::LimitlessSwap,
                GraduationVenue::MeteoraDammV2,
            ] {
                for fee in [0, 50, 100] {
                    let mut coin = small_coin(venue, fee);
                    coin.version = version;
                    coin.total_supply = 1_000_000_000_000_000;
                    coin.token_on_curve = 793_100_000_000_000;
                    coin.tokens_remaining_on_curve = coin.token_on_curve;
                    coin.virtual_token_offset = 280_000_000_000_000;
                    coin.graduation_sol_threshold = 85_000_000_000;
                    let mut vault = coin.total_supply;
                    for net in std::iter::repeat_n(1_000_000_000, 84).chain([975_000_000]) {
                        let buy = quote_net(&coin, net);
                        validate_buy_inventory(&coin, vault, &buy, &Rent::default()).unwrap();
                        apply_buy(&mut coin, &mut vault, buy);
                    }
                    assert_eq!(coin.sol_reserves, 84_975_000_000);
                    assert_eq!(coin.tokens_remaining_on_curve, 27);
                    validate_graduation_trigger(&coin).unwrap();
                    assert_eq!(
                        graduation_pool_tokens(&coin, vault, coin.sol_reserves).unwrap(),
                        vault
                    );
                    assert!(math::buy_quote(
                        1,
                        coin.sol_listed + coin.sol_reserves,
                        coin.tokens_remaining_on_curve,
                        coin.virtual_token_offset,
                        0,
                        6000,
                        4000,
                        0,
                        0,
                        fee
                    )
                    .is_err());
                    if venue == GraduationVenue::MeteoraDammV2 {
                        let rent = meteora_graduation_rent(&Rent::default()).unwrap();
                        coin.sol_reserves -= rent;
                        coin.mark_meteora_prepared(rent, vault);
                        validate_graduation_trigger(&coin).unwrap();
                        assert_eq!(
                            super::super::graduation_policy::meteora_pool_tokens(
                                &coin,
                                vault + 100,
                                coin.sol_reserves
                            )
                            .unwrap(),
                            vault
                        );
                        assert!(super::super::graduation_policy::meteora_pool_tokens(
                            &coin,
                            vault - 1,
                            coin.sol_reserves
                        )
                        .is_err());
                    }
                }
            }
        }
    }

    fn small_coin(venue: GraduationVenue, fee: u16) -> LaunchedCoin {
        let mut coin =
            LaunchedCoin::try_deserialize_unchecked(&mut &[0; LaunchedCoin::LEN][..]).unwrap();
        coin.version = 2;
        coin.total_supply = 50_000;
        coin.token_on_curve = 40_000;
        coin.tokens_remaining_on_curve = 40_000;
        coin.virtual_token_offset = 14_000;
        coin.sol_listed = STANDARD_SOL_LISTED;
        coin.graduation_sol_threshold = DEFAULT_GRADUATION_SOL_THRESHOLD;
        coin.graduation_venue = venue;
        coin.transfer_fee_bps = fee;
        coin
    }

    #[test]
    fn one_raw_taxed_token_can_graduate_without_an_impossible_final_buy() {
        for venue in [
            GraduationVenue::Raydium,
            GraduationVenue::LimitlessSwap,
            GraduationVenue::MeteoraDammV2,
        ] {
            let mut coin = small_coin(venue, 50);
            coin.total_supply = 100_000;
            coin.token_on_curve = 79_310;
            coin.tokens_remaining_on_curve = 79_310;
            coin.virtual_token_offset = 28_000;
            coin.graduation_sol_threshold = 100_000_000_000;
            let mut vault = coin.total_supply;
            validate_continuation(&coin, vault, &Rent::default()).unwrap();
            let buy = quote_net(&coin, 84_970_893_897);
            assert_eq!(
                buy.sol_to_curve + buy.platform_fee + buy.creator_fee,
                86_046_474_832
            );
            assert_eq!(
                (buy.tokens_out, buy.tokens_received_by_buyer),
                (79_309, 78_912)
            );
            validate_buy_inventory(&coin, vault, &buy, &Rent::default()).unwrap();
            apply_buy(&mut coin, &mut vault, buy);
            assert_eq!(
                (coin.sol_reserves, coin.tokens_remaining_on_curve),
                (84_970_893_897, 1)
            );
            assert_eq!(vault, 20_691);
            // The putative completion is all withheld, even with minimum 0.
            let final_buy = finish_buy(&coin);
            assert_eq!(
                (final_buy.tokens_out, final_buy.tokens_received_by_buyer),
                (1, 0)
            );
            for version in [0, 1, 2] {
                coin.version = version;
                validate_graduation_trigger(&coin).unwrap();
                validate_continuation(&coin, vault, &Rent::default()).unwrap();
            }
            coin.version = 2;
            // Recovery still needs the original minimum and pool inventory.
            assert!(validate_continuation(&coin, 1, &Rent::default()).is_err());
            coin.sol_reserves = MIN_GRADUATION_SOL_THRESHOLD - 1;
            assert!(validate_graduation_trigger(&coin).is_err());
            coin.sol_reserves = 84_970_893_897;
            coin.transfer_fee_bps = 0;
            assert!(validate_graduation_trigger(&coin).is_err());
            assert_eq!(finish_buy(&coin).tokens_received_by_buyer, 1);
            coin.transfer_fee_bps = 50;
            coin.tokens_remaining_on_curve = 2;
            assert!(validate_graduation_trigger(&coin).is_err());
            assert_eq!(finish_buy(&coin).tokens_received_by_buyer, 1);
        }
    }

    #[test]
    fn continuation_rejects_a_threshold_projection_with_zero_net_delivery() {
        let mut coin = small_coin(GraduationVenue::MeteoraDammV2, 50);
        coin.total_supply = 100_000;
        coin.token_on_curve = 79_310;
        coin.tokens_remaining_on_curve = 2;
        coin.virtual_token_offset = 28_000;
        coin.sol_reserves = 84_970_000_000;
        coin.graduation_sol_threshold = coin.sol_reserves + 5_000_000;
        let projected = quote_net(&coin, 5_000_000);
        assert_eq!(
            (projected.tokens_out, projected.tokens_received_by_buyer),
            (1, 0)
        );
        assert!(validate_continuation(&coin, 20_692, &Rent::default()).is_err());
        coin.transfer_fee_bps = 0;
        validate_continuation(&coin, 20_692, &Rent::default()).unwrap();
    }

    fn quote_net(coin: &LaunchedCoin, net: u64) -> math::BuyQuote {
        let gross = u64::try_from(u128::from(net) * 10_000 / 9875).unwrap();
        let quote = math::buy_quote(
            gross,
            coin.sol_listed + coin.sol_reserves,
            coin.tokens_remaining_on_curve,
            coin.virtual_token_offset,
            125,
            6000,
            4000,
            0,
            0,
            coin.transfer_fee_bps,
        )
        .unwrap();
        assert_eq!(quote.sol_to_curve, net);
        assert_eq!(quote.platform_fee + quote.creator_fee, gross - net);
        quote
    }

    fn one_unit_buy(coin: &LaunchedCoin) -> math::BuyQuote {
        let sol = u128::from(coin.sol_listed + coin.sol_reserves);
        let tokens = u128::from(coin.tokens_remaining_on_curve + coin.virtual_token_offset);
        // Largest net input still returning one raw unit. Each quote retains
        // almost one unit of rounding; no floating-point approximation.
        let quote = quote_net(coin, ((sol * tokens - 1) / (tokens - 2) - sol) as u64);
        assert_eq!(quote.tokens_out, 1);
        quote
    }

    fn finish_buy(coin: &LaunchedCoin) -> math::BuyQuote {
        let sol = u128::from(coin.sol_listed + coin.sol_reserves);
        let tokens = u128::from(coin.tokens_remaining_on_curve + coin.virtual_token_offset);
        quote_net(
            coin,
            (sol * tokens).div_ceil(u128::from(coin.virtual_token_offset)) as u64 - sol as u64,
        )
    }

    fn apply_buy(coin: &mut LaunchedCoin, vault: &mut u64, quote: math::BuyQuote) {
        coin.sol_reserves += quote.sol_to_curve;
        coin.tokens_remaining_on_curve -= quote.tokens_out;
        *vault -= quote.tokens_out;
    }

    fn sell_quote(coin: &LaunchedCoin, amount: u64) -> math::SellQuote {
        math::sell_quote(
            amount,
            coin.sol_listed + coin.sol_reserves,
            coin.sol_reserves,
            coin.tokens_remaining_on_curve,
            coin.virtual_token_offset,
            125,
            6000,
            4000,
            0,
            0,
            coin.transfer_fee_bps,
        )
        .unwrap()
    }

    #[test]
    fn sell_rounding_counterexample_keeps_exit_and_graduation_inventory() {
        let mut coin = small_coin(GraduationVenue::MeteoraDammV2, 0);
        coin.total_supply = 49_876_543_210;
        coin.token_on_curve = 40_000_000_000;
        coin.tokens_remaining_on_curve = coin.token_on_curve;
        coin.virtual_token_offset = 14_000_000_000;
        let mut vault = coin.total_supply;
        validate_continuation(&coin, vault, &Rent::default()).unwrap();
        for net in [98_750_000, 987_500_000] {
            let quote = quote_net(&coin, net);
            validate_buy_inventory(&coin, vault, &quote, &Rent::default()).unwrap();
            apply_buy(&mut coin, &mut vault, quote);
        }
        let sell = sell_quote(&coin, 3);
        assert_eq!(
            (sell.sol_out_raw, sell.sol_to_seller, sell.tokens_net),
            (1, 1, 3)
        );
        let mut uncorrected = coin.clone();
        uncorrected.tokens_remaining_on_curve += sell.tokens_net;
        uncorrected.sol_reserves -= sell.sol_out_raw;
        assert_eq!(vault - coin.tokens_remaining_on_curve, 9_876_543_210);
        assert!(
            validate_continuation(&uncorrected, vault + sell.tokens_net, &Rent::default()).is_err()
        );
        coin.tokens_remaining_on_curve =
            sell_curve_inventory(&coin, vault, &sell, &Rent::default()).unwrap();
        coin.sol_reserves -= sell.sol_out_raw;
        vault += sell.tokens_net;
        validate_continuation(&coin, vault, &Rent::default()).unwrap();
        let buy = quote_net(&coin, 98_750_000);
        validate_buy_inventory(&coin, vault, &buy, &Rent::default()).unwrap();
        apply_buy(&mut coin, &mut vault, buy);
        let finish = finish_buy(&coin);
        validate_buy_inventory(&coin, vault, &finish, &Rent::default()).unwrap();
        apply_buy(&mut coin, &mut vault, finish);
        validate_graduation_trigger(&coin).unwrap();
        graduation_pool_tokens(&coin, vault, coin.sol_reserves).unwrap();
    }

    #[test]
    fn rounded_sell_sequences_preserve_a_graduation_continuation() {
        for venue in [
            GraduationVenue::Raydium,
            GraduationVenue::LimitlessSwap,
            GraduationVenue::MeteoraDammV2,
        ] {
            for (fee, headroom) in [(0, 10_000), (50, 10_102), (100, 10_205)] {
                for scale in [1, 1_000_000] {
                    let mut coin = small_coin(venue, fee);
                    coin.token_on_curve *= scale;
                    coin.tokens_remaining_on_curve *= scale;
                    coin.virtual_token_offset *= scale;
                    coin.total_supply = (40_000 + headroom) * scale;
                    let mut vault = coin.total_supply;
                    let mut holdings = 0;
                    for step in 0..2000 {
                        let quote = quote_net(&coin, 98_750_000);
                        if validate_buy_inventory(&coin, vault, &quote, &Rent::default()).is_err() {
                            break;
                        }
                        holdings += quote.tokens_received_by_buyer;
                        apply_buy(&mut coin, &mut vault, quote);
                        let sell = sell_quote(&coin, holdings);
                        coin.tokens_remaining_on_curve =
                            sell_curve_inventory(&coin, vault, &sell, &Rent::default()).unwrap();
                        coin.sol_reserves -= sell.sol_out_raw;
                        vault += sell.tokens_net;
                        holdings = 0;
                        assert!(validate_continuation(&coin, vault, &Rent::default()).is_ok(), "{venue:?}/{fee}/{scale}/{step}: remaining={}, reserves={}, vault={vault}",coin.tokens_remaining_on_curve,coin.sol_reserves);
                    }
                }
            }
        }
    }

    #[test]
    fn sell_rounding_preserves_meteora_minimum_inventory() {
        let mut exercised = 0;
        let mut admitted = 0;
        for curve in 36_400..38_000 {
            let mut coin = small_coin(GraduationVenue::MeteoraDammV2, 0);
            coin.total_supply = 1_000_000;
            coin.token_on_curve = curve;
            coin.tokens_remaining_on_curve = curve;
            coin.virtual_token_offset = 12_855;
            let mut vault = coin.total_supply;
            if validate_continuation(&coin, vault, &Rent::default()).is_err() {
                continue;
            }
            admitted += 1;
            for step in 0..100 {
                let quote = quote_net(&coin, 98_750_000);
                if validate_buy_inventory(&coin, vault, &quote, &Rent::default()).is_err() {
                    break;
                }
                apply_buy(&mut coin, &mut vault, quote);
                let sell = sell_quote(&coin, quote.tokens_received_by_buyer);
                coin.tokens_remaining_on_curve =
                    sell_curve_inventory(&coin, vault, &sell, &Rent::default()).unwrap();
                coin.sol_reserves -= sell.sol_out_raw;
                vault += sell.tokens_net;
                exercised += 1;
                assert!(
                    validate_continuation(&coin, vault, &Rent::default()).is_ok(),
                    "curve={curve}, step={step}"
                );
            }
            if admitted == 10 {
                break;
            }
        }
        assert_eq!(admitted, 10);
        assert!(exercised >= 100);
    }

    #[test]
    fn sell_inventory_preserves_legacy_donations_and_exits_from_infeasible_states() {
        let mut coin = small_coin(GraduationVenue::MeteoraDammV2, 0);
        let buy = quote_net(&coin, 98_750_000);
        let mut vault = coin.total_supply;
        apply_buy(&mut coin, &mut vault, buy);
        let sell = sell_quote(&coin, buy.tokens_out);
        let ordinary = coin.tokens_remaining_on_curve + sell.tokens_net;
        for version in [0, 1, 2] {
            coin.version = version;
            assert_eq!(
                sell_curve_inventory(&coin, vault, &sell, &Rent::default()).unwrap(),
                ordinary
            );
        }
        // An already deficient vault must not introduce a new withdrawal gate.
        assert!(
            validate_continuation(&coin, coin.tokens_remaining_on_curve, &Rent::default()).is_err()
        );
        let retained = sell_curve_inventory(
            &coin,
            coin.tokens_remaining_on_curve,
            &sell,
            &Rent::default(),
        )
        .unwrap();
        assert!((coin.tokens_remaining_on_curve..=ordinary).contains(&retained));
        assert!(sell_curve_inventory(&coin, u64::MAX, &sell, &Rent::default()).is_err());
        let mut invalid = sell;
        invalid.sol_out_raw = coin.sol_reserves + 1;
        assert!(sell_curve_inventory(&coin, vault, &invalid, &Rent::default()).is_err());
    }

    #[test]
    fn tight_inventory_sell_paths_keep_transfer_fees_and_a_completion() {
        let mut repaired = 0;
        for fee in [0, 50, 100] {
            for venue in [
                GraduationVenue::Raydium,
                GraduationVenue::LimitlessSwap,
                GraduationVenue::MeteoraDammV2,
            ] {
                let mut coin = small_coin(venue, fee);
                coin.token_on_curve *= 1_000_000;
                coin.tokens_remaining_on_curve *= 1_000_000;
                coin.virtual_token_offset *= 1_000_000;
                coin.total_supply = 60_000_000_000;
                let mut vault = coin.total_supply;
                let mut holdings = 0;
                for net in [98_750_000, 987_500_000] {
                    let buy = quote_net(&coin, net);
                    holdings += buy.tokens_received_by_buyer;
                    apply_buy(&mut coin, &mut vault, buy);
                }
                // Find the tight spendable headroom using the existing economics,
                // including both rounded transfer fees. No total-supply proxy.
                let mut low = coin.tokens_remaining_on_curve;
                let mut high = vault;
                while low < high {
                    let mid = low + (high - low) / 2;
                    if validate_continuation(&coin, mid, &Rent::default()).is_ok() {
                        high = mid;
                    } else {
                        low = mid + 1;
                    }
                }
                vault = low;
                validate_continuation(&coin, vault, &Rent::default()).unwrap();
                for amount in (3..=500).chain([holdings / 2, holdings]) {
                    let sell = sell_quote(&coin, amount);
                    let mut after = coin.clone();
                    after.tokens_remaining_on_curve =
                        sell_curve_inventory(&coin, vault, &sell, &Rent::default()).unwrap();
                    after.sol_reserves -= sell.sol_out_raw;
                    let vault_after = vault + sell.tokens_net;
                    repaired += usize::from(
                        after.tokens_remaining_on_curve
                            < coin.tokens_remaining_on_curve + sell.tokens_net,
                    );
                    assert!((coin.tokens_remaining_on_curve
                        ..=coin.tokens_remaining_on_curve + sell.tokens_net)
                        .contains(&after.tokens_remaining_on_curve));
                    assert!(
                        validate_continuation(&after, vault_after, &Rent::default()).is_ok(),
                        "{venue:?}/{fee}/{amount}"
                    );
                    let finish = finish_buy(&after);
                    validate_buy_inventory(&after, vault_after, &finish, &Rent::default()).unwrap();
                    let mut completed_vault = vault_after;
                    apply_buy(&mut after, &mut completed_vault, finish);
                    validate_graduation_trigger(&after).unwrap();
                    graduation_pool_tokens(&after, completed_vault, after.sol_reserves).unwrap();
                }
                // A spendable donation supplies the missing headroom; it must
                // preserve the ordinary curve credit instead of taking dust.
                let sell = sell_quote(&coin, 3);
                assert_eq!(
                    sell_curve_inventory(&coin, vault + 100, &sell, &Rent::default()).unwrap(),
                    coin.tokens_remaining_on_curve + sell.tokens_net
                );
            }
        }
        assert!(repaired > 100);
    }

    #[test]
    fn rounding_counterexample_replays_8000_buys_and_has_no_sellback_recovery() {
        let mut coin = small_coin(GraduationVenue::MeteoraDammV2, 0);
        let mut vault = coin.total_supply;
        validate_continuation(&coin, vault, &Rent::default()).unwrap();
        for _ in 0..8000 {
            let quote = one_unit_buy(&coin);
            apply_buy(&mut coin, &mut vault, quote);
        }
        assert_eq!(coin.sol_reserves, 11_342_283_479);
        let finish = finish_buy(&coin);
        assert_eq!(finish.tokens_out, 32_000);
        apply_buy(&mut coin, &mut vault, finish);
        assert_eq!(coin.sol_reserves, 105_838_931_431);
        assert_eq!(vault, 10_000);
        validate_graduation_trigger(&coin).unwrap();
        for rent in [0, meteora_graduation_rent(&Rent::default()).unwrap()] {
            let pool_sol = coin.sol_reserves - rent;
            assert!(graduation_pool_tokens(&coin, vault, pool_sol).is_err());
            let minimum = (u128::from(pool_sol) * 14_000 * 10_000)
                .div_ceil(u128::from(coin.sol_listed + coin.sol_reserves) * 10_500);
            assert_eq!(minimum, if rent == 0 { 10_389 } else { 10_385 });
        }
        // The original path is already stuck: every allowed sellback fails
        // even with curve-funded rent lowering the pool's opening price.
        for tokens_in in 1..=401 {
            let sell = math::sell_quote(
                tokens_in,
                coin.sol_listed + coin.sol_reserves,
                coin.sol_reserves,
                0,
                coin.virtual_token_offset,
                125,
                6000,
                4000,
                0,
                0,
                0,
            )
            .unwrap();
            let mut after = coin.clone();
            after.sol_reserves -= sell.sol_out_raw;
            after.tokens_remaining_on_curve += sell.tokens_net;
            validate_graduation_trigger(&after).unwrap();
            assert!(graduation_pool_tokens(
                &after,
                vault + sell.tokens_net,
                after.sol_reserves - meteora_graduation_rent(&Rent::default()).unwrap(),
            )
            .is_err());
        }
    }

    #[test]
    fn every_accepted_rounded_buy_leaves_a_completion_without_donated_tokens() {
        for venue in [
            GraduationVenue::Raydium,
            GraduationVenue::LimitlessSwap,
            GraduationVenue::MeteoraDammV2,
        ] {
            for (fee, headroom) in [(0, 10_000), (50, 10_102), (100, 10_205)] {
                for threshold in [
                    DEFAULT_GRADUATION_SOL_THRESHOLD,
                    MAX_GRADUATION_SOL_THRESHOLD,
                ] {
                    let mut coin = small_coin(venue, fee);
                    coin.total_supply = coin.token_on_curve + headroom;
                    coin.graduation_sol_threshold = threshold;
                    let mut vault = coin.total_supply;
                    super::validate_inventory(
                        vault,
                        coin.token_on_curve,
                        coin.virtual_token_offset,
                        coin.sol_listed,
                        threshold,
                        fee,
                        venue,
                        &Rent::default(),
                    )
                    .unwrap();
                    let mut accepted = 0;
                    for _ in 0..8000 {
                        let quote = one_unit_buy(&coin);
                        if validate_buy_inventory(&coin, vault, &quote, &Rent::default()).is_err() {
                            break;
                        }
                        apply_buy(&mut coin, &mut vault, quote);
                        accepted += 1;
                        // Check a real fee-bearing completion from every state,
                        // not just the projection that admission itself uses.
                        let finish = finish_buy(&coin);
                        validate_buy_inventory(&coin, vault, &finish, &Rent::default()).unwrap();
                        let mut completed = coin.clone();
                        let mut completed_vault = vault;
                        apply_buy(&mut completed, &mut completed_vault, finish);
                        validate_graduation_trigger(&completed).unwrap();
                        graduation_pool_tokens(&completed, completed_vault, completed.sol_reserves)
                            .unwrap();
                    }
                    assert!(
                        accepted > 0 && accepted < 8000,
                        "{venue:?}/{fee}/{threshold}: {accepted}"
                    );
                    if threshold == DEFAULT_GRADUATION_SOL_THRESHOLD {
                        assert_eq!(accepted, 1127);
                    }
                    let finish = finish_buy(&coin);
                    validate_buy_inventory(&coin, vault, &finish, &Rent::default()).unwrap();
                    apply_buy(&mut coin, &mut vault, finish);
                    assert_eq!(coin.tokens_remaining_on_curve, 0);
                    assert_eq!(vault, headroom);
                    validate_continuation(&coin, vault, &Rent::default()).unwrap();
                }
            }
        }
    }

    #[test]
    fn buy_guard_uses_actual_vault_and_preserves_legacy_and_overflow_rules() {
        let mut coin = small_coin(GraduationVenue::MeteoraDammV2, 100);
        let quote = finish_buy(&coin);
        // Withheld or allocated tokens cannot masquerade as spendable LP
        // inventory merely because total_supply includes them.
        coin.total_supply = 1_000_000;
        assert!(validate_buy_inventory(&coin, 50_204, &quote, &Rent::default()).is_err());
        validate_buy_inventory(&coin, 50_205, &quote, &Rent::default()).unwrap();
        validate_buy_inventory(&coin, u64::MAX, &quote, &Rent::default()).unwrap();
        assert!(validate_buy_inventory(&coin, 0, &quote, &Rent::default()).is_err());
        let mut invalid = quote;
        invalid.sol_to_curve = u64::MAX;
        assert!(validate_buy_inventory(&coin, u64::MAX, &invalid, &Rent::default()).is_err());
        for version in [0, 1] {
            coin.version = version;
            assert!(validate_buy_inventory(&coin, 50_000, &quote, &Rent::default()).is_err());
            validate_buy_inventory(&coin, 50_205, &quote, &Rent::default()).unwrap();
        }
    }

    #[test]
    fn zero_offset_curve_can_cross_threshold_and_keep_trading_inside_price_band() {
        let mut coin = small_coin(GraduationVenue::Raydium, 0);
        coin.sol_listed = 1_000_000_000;
        coin.virtual_token_offset = 0;
        coin.graduation_sol_threshold = 100_000_000_000;
        let mut vault = coin.total_supply;
        validate_continuation(&coin, vault, &Rent::default()).unwrap();
        for net in [30_000_000_000, 70_000_000_000, 1_000_000_000] {
            let quote = quote_net(&coin, net);
            validate_buy_inventory(&coin, vault, &quote, &Rent::default()).unwrap();
            apply_buy(&mut coin, &mut vault, quote);
        }
        validate_graduation_trigger(&coin).unwrap();
        graduation_pool_tokens(&coin, vault, coin.sol_reserves).unwrap();
    }

    #[test]
    fn standard_allocated_curves_reach_tradable_dust_after_fragmented_buys() {
        for allocation in [0, 500, 1000] {
            let supply = math::split_supply_for_genie_allocation(
                STANDARD_TOTAL_SUPPLY,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                allocation,
            )
            .unwrap();
            for fee in [0, 50, 100] {
                let mut coin = small_coin(GraduationVenue::MeteoraDammV2, fee);
                coin.total_supply = STANDARD_TOTAL_SUPPLY;
                coin.token_on_curve = supply.token_on_curve;
                coin.tokens_remaining_on_curve = supply.token_on_curve;
                coin.virtual_token_offset = supply.virtual_token_offset;
                coin.graduation_sol_threshold = MAX_GRADUATION_SOL_THRESHOLD;
                let mut vault = supply.curve_amount;
                for net in [98_750_000, 1_000_000_000, 7_000_000_000] {
                    let quote = quote_net(&coin, net);
                    validate_buy_inventory(&coin, vault, &quote, &Rent::default()).unwrap();
                    apply_buy(&mut coin, &mut vault, quote);
                }
                let sol = u128::from(coin.sol_listed + coin.sol_reserves);
                let tokens = u128::from(coin.virtual_token_offset + coin.tokens_remaining_on_curve);
                let maximum = (sol * tokens - 1) / u128::from(coin.virtual_token_offset - 1);
                let quote = quote_net(&coin, (maximum - sol) as u64);
                validate_buy_inventory(&coin, vault, &quote, &Rent::default()).unwrap();
                apply_buy(&mut coin, &mut vault, quote);
                assert!(coin.sol_reserves < coin.graduation_sol_threshold);
                validate_graduation_trigger(&coin).unwrap();
                graduation_pool_tokens(&coin, vault, coin.sol_reserves).unwrap();
            }
        }
    }

    #[test]
    fn terminal_projection_includes_the_last_tradable_lamport() {
        let mut coin = small_coin(GraduationVenue::MeteoraDammV2, 0);
        coin.sol_listed = 50_000_000_000;
        coin.token_on_curve = 250_000_000_001;
        coin.tokens_remaining_on_curve = coin.token_on_curve;
        coin.virtual_token_offset = 200_000_000_000;
        coin.total_supply = coin.token_on_curve + 250_000_000_000;
        coin.graduation_sol_threshold = MAX_GRADUATION_SOL_THRESHOLD;
        let mut vault = coin.total_supply;
        validate_continuation(&coin, vault, &Rent::default()).unwrap();
        // The rounded SOL for a one-shot exact sell-out oversells by one unit.
        assert_eq!(
            math::buy_quote(
                62_500_000_001,
                coin.sol_listed,
                coin.token_on_curve,
                coin.virtual_token_offset,
                0,
                10_000,
                0,
                0,
                0,
                0
            ),
            Err(math::MathError::InsufficientCurveSupply)
        );
        for net in [62_500_000_000, 1] {
            let quote = quote_net(&coin, net);
            validate_buy_inventory(&coin, vault, &quote, &Rent::default()).unwrap();
            apply_buy(&mut coin, &mut vault, quote);
            assert_eq!(coin.tokens_remaining_on_curve, u64::from(net > 1));
        }
        validate_graduation_trigger(&coin).unwrap();
        graduation_pool_tokens(&coin, vault, coin.sol_reserves).unwrap();
    }

    fn validate_inventory(
        vault: u64,
        curve: u64,
        offset: u64,
        sol: u64,
        threshold: u64,
        fee: u16,
        venue: GraduationVenue,
    ) -> Result<()> {
        super::validate_inventory(
            vault,
            curve,
            offset,
            sol,
            threshold,
            fee,
            venue,
            &Rent::default(),
        )
    }

    #[test]
    fn meteora_admission_budgets_rent_before_applying_the_minimum() {
        // At sell-out the no-rent price floor caps even this large vault at
        // exactly 10,000 tokens. Curve-funded rent lowers that cap below the
        // venue minimum; a supply-only or no-rent check would still strand it.
        super::validate_inventory(
            1_000_000,
            36_407,
            12_855,
            STANDARD_SOL_LISTED,
            DEFAULT_GRADUATION_SOL_THRESHOLD,
            0,
            GraduationVenue::MeteoraDammV2,
            &Rent::free(),
        )
        .unwrap();
        assert!(validate_inventory(
            1_000_000,
            36_407,
            12_855,
            STANDARD_SOL_LISTED,
            DEFAULT_GRADUATION_SOL_THRESHOLD,
            0,
            GraduationVenue::MeteoraDammV2
        )
        .is_err());
    }

    #[test]
    fn tiny_supply_reaches_trigger_but_cannot_meet_meteora_minimum() {
        use anchor_spl::token_2022::spl_token_2022::extension::transfer_fee::TransferFee;
        for (bps, expected_net) in [(0, 208), (50, 204), (100, 202)] {
            let mut coin =
                LaunchedCoin::try_deserialize_unchecked(&mut &[0; LaunchedCoin::LEN][..]).unwrap();
            coin.version = 2;
            coin.total_supply = 1_000;
            coin.token_on_curve = 793;
            coin.tokens_remaining_on_curve = 793;
            coin.virtual_token_offset = 280;
            coin.sol_listed = STANDARD_SOL_LISTED;
            coin.graduation_sol_threshold = DEFAULT_GRADUATION_SOL_THRESHOLD;
            coin.transfer_fee_bps = bps;
            for target in [39_500_000_000u64, 45_400_000_000] {
                let gross = (u128::from(target) * 10_000).div_ceil(9875) as u64;
                let buy = math::buy_quote(
                    gross,
                    coin.sol_listed + coin.sol_reserves,
                    coin.tokens_remaining_on_curve,
                    coin.virtual_token_offset,
                    125,
                    6000,
                    4000,
                    0,
                    0,
                    bps,
                )
                .unwrap();
                coin.sol_reserves += buy.sol_to_curve;
                coin.tokens_remaining_on_curve -= buy.tokens_out;
            }
            validate_graduation_trigger(&coin).unwrap();
            let gross = graduation_pool_tokens(
                &coin,
                coin.total_supply - coin.token_on_curve + coin.tokens_remaining_on_curve,
                coin.sol_reserves,
            )
            .unwrap();
            let fee = TransferFee {
                epoch: 0.into(),
                maximum_fee: u64::MAX.into(),
                transfer_fee_basis_points: bps.into(),
            };
            let staged = gross - fee.calculate_fee(gross).unwrap();
            let net = staged - fee.calculate_fee(staged).unwrap();
            assert_eq!(net, expected_net);
            assert!(coin.total_supply < super::super::graduate_meteora::MIN_METEORA_RESERVE);
            // Other venues retain the existing shared economics. Even a huge
            // vault cannot hide the tiny selected contribution from admission.
            for supply in [1_000, u64::MAX] {
                for venue in [GraduationVenue::Raydium, GraduationVenue::MeteoraDammV2] {
                    assert_eq!(
                        validate_inventory(
                            supply,
                            793,
                            280,
                            STANDARD_SOL_LISTED,
                            DEFAULT_GRADUATION_SOL_THRESHOLD,
                            bps,
                            venue
                        )
                        .is_ok(),
                        venue == GraduationVenue::Raydium
                    );
                }
            }
        }
    }

    #[test]
    fn meteora_minimum_is_inclusive_after_both_fees_and_at_sellout() {
        use anchor_spl::token_2022::spl_token_2022::extension::transfer_fee::TransferFee;
        for (bps, minimum_gross) in [(0, 10_000u64), (50, 10_102), (100, 10_205)] {
            let fee = TransferFee {
                epoch: 0.into(),
                maximum_fee: u64::MAX.into(),
                transfer_fee_basis_points: bps.into(),
            };
            for gross in [minimum_gross - 1, minimum_gross] {
                let staged = gross - fee.calculate_fee(gross).unwrap();
                let net = staged - fee.calculate_fee(staged).unwrap();
                assert_eq!(
                    net,
                    if gross == minimum_gross {
                        10_000
                    } else {
                        9_999
                    }
                );
                for threshold in [
                    DEFAULT_GRADUATION_SOL_THRESHOLD,
                    MAX_GRADUATION_SOL_THRESHOLD,
                ] {
                    assert_eq!(
                        validate_inventory(
                            40_000 + gross,
                            40_000,
                            14_000,
                            STANDARD_SOL_LISTED,
                            threshold,
                            bps,
                            GraduationVenue::MeteoraDammV2
                        )
                        .is_ok(),
                        gross == minimum_gross
                    );
                }
            }
        }
    }

    #[test]
    fn reproduces_funded_but_ungraduatable_all_on_curve_launch() {
        let mut coin =
            LaunchedCoin::try_deserialize_unchecked(&mut &[0; LaunchedCoin::LEN][..]).unwrap();
        coin.version = 2;
        coin.total_supply = STANDARD_TOKEN_ON_CURVE;
        coin.token_on_curve = STANDARD_TOKEN_ON_CURVE;
        coin.tokens_remaining_on_curve = STANDARD_TOKEN_ON_CURVE;
        coin.sol_listed = STANDARD_SOL_LISTED;
        coin.virtual_token_offset = STANDARD_VIRTUAL_TOKEN_OFFSET;
        coin.graduation_sol_threshold = DEFAULT_GRADUATION_SOL_THRESHOLD;
        // Two real quotes, with the unchanged 1.25% bonding fee, reach 84.9 SOL.
        for target_net in [39_500_000_000u64, 45_400_000_000] {
            let gross = (u128::from(target_net) * 10_000).div_ceil(9875) as u64;
            let buy = math::buy_quote(
                gross,
                coin.sol_listed + coin.sol_reserves,
                coin.tokens_remaining_on_curve,
                coin.virtual_token_offset,
                125,
                6000,
                4000,
                0,
                0,
                0,
            )
            .unwrap();
            coin.sol_reserves += buy.sol_to_curve;
            coin.tokens_remaining_on_curve -= buy.tokens_out;
        }
        assert!(coin.sol_reserves >= DEFAULT_GRADUATION_SOL_THRESHOLD);
        assert!((182_000..184_000).contains(&(coin.tokens_remaining_on_curve / 1_000_000)));
        validate_graduation_trigger(&coin).unwrap();
        let minimum_net = (u128::from(coin.sol_reserves)
            * u128::from(coin.virtual_token_offset + coin.tokens_remaining_on_curve)
            * 10_000)
            .div_ceil(
                u128::from(coin.sol_listed + coin.sol_reserves)
                    * u128::from(MAX_GRADUATION_PRICE_BPS),
            );
        assert!((197_000_000..198_000_000).contains(&(minimum_net / 1_000_000)));
        assert!(
            graduation_pool_tokens(&coin, coin.tokens_remaining_on_curve, coin.sol_reserves)
                .is_err()
        );
        assert!(validate_inventory(
            coin.total_supply,
            coin.token_on_curve,
            coin.virtual_token_offset,
            coin.sol_listed,
            coin.graduation_sol_threshold,
            0,
            GraduationVenue::Raydium
        )
        .is_err());
    }

    #[test]
    fn standard_and_allocated_inventory_passes_both_tax_hops_and_sellout() {
        for allocation_bps in [0, 100, 500, 1000] {
            let supply = math::split_supply_for_genie_allocation(
                STANDARD_TOTAL_SUPPLY,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                allocation_bps,
            )
            .unwrap();
            for fee in [0, 50, 100] {
                for threshold in [
                    DEFAULT_GRADUATION_SOL_THRESHOLD,
                    MAX_GRADUATION_SOL_THRESHOLD,
                ] {
                    validate_inventory(
                        supply.curve_amount,
                        supply.token_on_curve,
                        supply.virtual_token_offset,
                        STANDARD_SOL_LISTED,
                        threshold,
                        fee,
                        GraduationVenue::Raydium,
                    )
                    .unwrap();
                }
                assert!(validate_inventory(
                    supply.token_on_curve,
                    supply.token_on_curve,
                    supply.virtual_token_offset,
                    STANDARD_SOL_LISTED,
                    DEFAULT_GRADUATION_SOL_THRESHOLD,
                    fee,
                    GraduationVenue::Raydium
                )
                .is_err());
            }
        }
    }

    #[test]
    fn inventory_boundary_includes_transfer_fees_and_preserves_economics() {
        let mut minimums = Vec::new();
        for fee in [0, 50, 100] {
            let mut low = STANDARD_TOKEN_ON_CURVE;
            let mut high = STANDARD_TOTAL_SUPPLY;
            while low < high {
                let mid = low + (high - low) / 2;
                if validate_inventory(
                    mid,
                    STANDARD_TOKEN_ON_CURVE,
                    STANDARD_VIRTUAL_TOKEN_OFFSET,
                    STANDARD_SOL_LISTED,
                    DEFAULT_GRADUATION_SOL_THRESHOLD,
                    fee,
                    GraduationVenue::Raydium,
                )
                .is_ok()
                {
                    high = mid;
                } else {
                    low = mid + 1;
                }
            }
            validate_inventory(
                low,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                STANDARD_SOL_LISTED,
                DEFAULT_GRADUATION_SOL_THRESHOLD,
                fee,
                GraduationVenue::Raydium,
            )
            .unwrap();
            assert!(validate_inventory(
                low - 1,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                STANDARD_SOL_LISTED,
                DEFAULT_GRADUATION_SOL_THRESHOLD,
                fee,
                GraduationVenue::Raydium
            )
            .is_err());
            minimums.push(low);
        }
        assert!(minimums[0] < minimums[1] && minimums[1] < minimums[2]);
    }

    #[test]
    fn invalid_projection_and_infeasible_unsold_supply_fail_closed() {
        for (supply, curve, offset, sol, threshold) in [
            (
                0,
                0,
                0,
                STANDARD_SOL_LISTED,
                DEFAULT_GRADUATION_SOL_THRESHOLD,
            ),
            (
                1,
                2,
                1,
                STANDARD_SOL_LISTED,
                DEFAULT_GRADUATION_SOL_THRESHOLD,
            ),
            (
                u64::MAX,
                u64::MAX,
                1,
                STANDARD_SOL_LISTED,
                DEFAULT_GRADUATION_SOL_THRESHOLD,
            ),
            (
                STANDARD_TOTAL_SUPPLY,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                0,
                DEFAULT_GRADUATION_SOL_THRESHOLD,
            ),
            (
                STANDARD_TOTAL_SUPPLY,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                u64::MAX,
                DEFAULT_GRADUATION_SOL_THRESHOLD,
            ),
            (
                STANDARD_TOTAL_SUPPLY,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                STANDARD_SOL_LISTED,
                0,
            ),
            (
                STANDARD_TOTAL_SUPPLY,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                STANDARD_SOL_LISTED,
                MIN_GRADUATION_SOL_THRESHOLD,
            ),
            (
                STANDARD_TOTAL_SUPPLY,
                STANDARD_TOKEN_ON_CURVE,
                STANDARD_VIRTUAL_TOKEN_OFFSET,
                STANDARD_SOL_LISTED,
                60_000_000_000,
            ),
        ] {
            assert!(validate_inventory(
                supply,
                curve,
                offset,
                sol,
                threshold,
                0,
                GraduationVenue::Raydium
            )
            .is_err());
        }
        // Equality alone is not the bug: a flat curve can graduate within the
        // existing price band and 1% unsold cap without separate LP headroom.
        validate_inventory(
            1_000_000,
            1_000_000,
            0,
            1_000_000_000,
            100_000_000_000,
            0,
            GraduationVenue::Raydium,
        )
        .unwrap();
    }
}
