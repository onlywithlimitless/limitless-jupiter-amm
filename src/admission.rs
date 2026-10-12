// Pure host adaptation of reference/instructions; regenerate with scripts/extract_admission.py.
use crate::{math, state::*, *};
use primitive_types::U256;
const BPS_DENOMINATOR: u16 = 10_000;
const MIN_GRADUATION_SOL_THRESHOLD: u64 = 1_000_000_000;
const MIN_GRADUATION_PRICE_BPS: u16 = 9500;
const MAX_GRADUATION_PRICE_BPS: u16 = 10500;
const MAX_UNSOLD_AT_GRADUATION_BPS: u16 = 100;
macro_rules! require {($condition:expr, $($error:tt)*)=>{ensure($condition,stringify!($($error)*))?};}
macro_rules! error {($($error:tt)*)=>{AmmError::from(stringify!($($error)*))};}
fn math_error(e: math::MathError) -> AmmError {
    AmmError::from(format!("{e:?}"))
}
pub fn graduation_trigger_met(coin: &LaunchedCoin) -> Result<bool> {
    let reserves = coin
        .sol_reserves
        .checked_add(coin.meteora_preparation_rent())
        .ok_or_else(|| AmmError::from("AmountOverflow"))?;
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
pub fn validate_graduation_trigger(coin: &LaunchedCoin) -> Result<()> {
    require!(
        graduation_trigger_met(coin)?,
        LaunchpadError::NotReadyForGraduation
    );
    Ok(())
}
pub fn v2_pool_tokens(coin: &LaunchedCoin, vault_tokens: u64, pool_sol: u64) -> Result<u64> {
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
pub fn validate_graduation_economics(
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
                    .map_err(math_error)?,
            )
            .ok_or_else(|| AmmError::from("AmountOverflow"))?;
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
pub fn legacy_contribution(coin: &LaunchedCoin, tokens: u64, pool_sol: u64) -> Result<u64> {
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
        .ok_or_else(|| AmmError::from("NotReadyForGraduation"))?;
    let tokens = tokens.min(inventory);
    require!(tokens > 0, LaunchpadError::NotReadyForGraduation);
    Ok(tokens)
}
pub fn validate_buy_inventory(
    coin: &LaunchedCoin,
    vault_tokens: u64,
    quote: &math::BuyQuote,
    rent: &Rent,
) -> Result<()> {
    let mut after = coin.clone();
    after.tokens_remaining_on_curve = after
        .tokens_remaining_on_curve
        .checked_sub(quote.tokens_out)
        .ok_or_else(|| AmmError::from("AmountOverflow"))?;
    after.sol_reserves = after
        .sol_reserves
        .checked_add(quote.sol_to_curve)
        .ok_or_else(|| AmmError::from("AmountOverflow"))?;
    let vault_after = vault_tokens
        .checked_sub(quote.tokens_out)
        .ok_or_else(|| AmmError::from("AmountOverflow"))?;
    validate_continuation(&after, vault_after, rent)
}
pub fn validate_continuation(coin: &LaunchedCoin, vault_tokens: u64, rent: &Rent) -> Result<()> {
    let virtual_sol = coin
        .sol_listed
        .checked_add(coin.sol_reserves)
        .ok_or_else(|| AmmError::from("AmountOverflow"))?;
    let virtual_tokens = coin
        .tokens_remaining_on_curve
        .checked_add(coin.virtual_token_offset)
        .ok_or_else(|| AmmError::from("AmountOverflow"))?;
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
            .ok_or_else(|| AmmError::from("NotReadyForGraduation"))?;
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
                .ok_or_else(|| AmmError::from("AmountOverflow"))?;
            vault_after = vault_after
                .checked_sub(dust.tokens_out)
                .ok_or_else(|| AmmError::from("NotReadyForGraduation"))?;
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
                .filter(|sol| *sol >= 10_000)
                .ok_or_else(|| AmmError::from("NotReadyForGraduation"))?;
            let mut pool_tokens = graduation_pool_tokens(&projected, vault_after, pool_sol)?;
            for _ in 0..2 {
                let fee = math::transfer_fee(u128::from(pool_tokens), coin.transfer_fee_bps)
                    .map_err(math_error)?;
                pool_tokens = u64::try_from(u128::from(pool_tokens) - fee)
                    .map_err(|_| error!(LaunchpadError::NotReadyForGraduation))?;
            }
            require!(pool_tokens >= 10_000, LaunchpadError::NotReadyForGraduation);
        }
    }
    Ok(())
}
fn graduation_pool_tokens(coin: &LaunchedCoin, vault: u64, sol: u64) -> Result<u64> {
    if coin.version >= 2 {
        v2_pool_tokens(coin, vault, sol)
    } else {
        legacy_contribution(coin, vault, sol)
    }
}
fn meteora_graduation_rent(rent: &Rent) -> Result<u64> {
    [255, 187, 165, 1112, 408, 408, 465, 465, 165, 165, 183, 165]
        .into_iter()
        .try_fold(0u64, |sum, size| {
            sum.checked_add(rent.minimum_balance(size))
                .ok_or_else(|| AmmError::from("Rent overflow"))
        })
}
