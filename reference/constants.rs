//! Program-wide constants.
//!
//! All economic parameters have both a compile-time default (used at
//! `initialize`) and a mutable `Config` field (used at runtime). The defaults
//! lock in the LIMITLESS launch spec; the Config fields preserve flexibility
//! for admin-driven adjustments until the admin key is burned.

use anchor_lang::prelude::*;

// ---------------------------------------------------------------------------
// PDA seed prefixes
// ---------------------------------------------------------------------------
pub const CONFIG_SEED: &[u8] = b"config";
pub const REWARDS_POOL_SEED: &[u8] = b"rewards_pool_v2";
pub const LAUNCHED_COIN_SEED: &[u8] = b"launched_coin";
pub const VAULT_AUTHORITY_SEED: &[u8] = b"vault_authority";
pub const VAULT_TOKEN_SEED: &[u8] = b"vault_token";
/// Seed for the `withdraw_withheld_authority` PDA of the fee-splitter
/// program. Must match the identical constant in
/// `fee_splitter_program::constants`.
pub const WITHDRAW_WITHHELD_SEED: &[u8] = b"withdraw_withheld";

/// Default DAMM v2 policy for this integration. The admin activation instruction
/// authenticates its on-chain owner, layout and pool-creator authority before use.
/// Existing Config accounts retain their zero padding until admin selection.
pub const DEFAULT_METEORA_CONFIG: Pubkey =
    anchor_lang::solana_program::pubkey!("9qtecGWmXoTzJcEuNtVxuFkr1N915gPYu3PCgFtBvvmJ");

/// Owner PDA for a launch's Genie allocation vault, seeded by the mint so each
/// launched token gets its own isolated vault. The allocation share is MINTED
/// straight here during the launch instruction (Token-2022 transfer fees do not
/// apply to `MintTo`), so the vault holds exactly the percentage the creator
/// chose, with nothing withheld.
pub const GENIE_ALLOC_VAULT_SEED: &[u8] = b"genie_alloc_vault";

/// Canonical companion program allowed to own the withdraw-withheld PDA.
/// Keep these values aligned with `fee_splitter_program::declare_id!` and
/// `Anchor.toml`; an arbitrary program would permanently strand a launched
/// mint's withheld transfer fees under an unsignable PDA.
#[cfg(not(feature = "devnet"))]
pub const FEE_SPLITTER_PROGRAM_ID: Pubkey = pubkey!("SPLTRtzj8z3mLMwjFXC8rQ7fWmHXxGc9KU7xeeQz2yc");
#[cfg(feature = "devnet")]
pub const FEE_SPLITTER_PROGRAM_ID: Pubkey = pubkey!("rdtqi6P7DJxYwVAEsEffHGZ46Gabe5GXgPE4RMvvy29");

// ---------------------------------------------------------------------------
// Basis-point denominator. All bps fields are `u16` and must sum to 10_000.
// ---------------------------------------------------------------------------
pub const BPS_DENOMINATOR: u16 = 10_000;

// ---------------------------------------------------------------------------
// Bonding curve (LIMITLESS-hosted trades) — spec-locked defaults
// ---------------------------------------------------------------------------
/// Total fee on bonding-curve buys and sells: 125 bps (1.25%).
///
/// Founder decision 2026-09-03, reaffirmed 2026-09-04: 0.75% to the company
/// and 0.50% to the creator. The creator's absolute take is unchanged at
/// 0.50%; the company's goes 0.50% -> 0.75%, so the total rises 100 -> 125.
///
/// This constant is NOT settable on chain. `set_bonding_shares` can only
/// redistribute an existing fee and is explicitly forbidden from raising what
/// a trader pays, so this value can only change by a program upgrade.
pub const BONDING_CURVE_FEE_BPS: u16 = 125;
/// Company share of the bonding-curve fee: 6000 bps of 125 → 0.75%.
pub const BONDING_PLATFORM_SHARE_BPS: u16 = 6_000;
/// Creator share of the bonding-curve fee: 4000 bps of 125 → 0.50%.
///
/// Founder decision 2026-09-02: "let's go 50/50. lock in 50/50." The creator
/// share moves 30% -> 50%, taking the whole of the retired rewards pool. The
/// TOTAL fee is unchanged at 1.0%, so nothing changes for a trader; only who
/// receives the second half of it.
///
/// 4000 bps of the 125 bps total = 0.50% of volume to whoever launched the
/// token, roughly ten times pump.fun's creator share. The creator's absolute
/// take is IDENTICAL to the previous 5000-of-100 setting; only the denominator
/// moved, because the company's share went 0.50% -> 0.75%.
pub const BONDING_CREATOR_SHARE_BPS: u16 = 4_000;
/// Rewards pool share: RETIRED, 0.
///
/// It accrued 20% of every trade into an escrow with no distribution mechanism
/// and no plan for one, which is a liability rather than a reward. The constant
/// stays (rather than being deleted) so the three-way split and its sum check
/// in initialize.rs remain intact, and so restoring a rewards bucket later is a
/// value change rather than a structural one.
pub const BONDING_REWARDS_SHARE_BPS: u16 = 0;

// ---------------------------------------------------------------------------
// Post-graduation Token-2022 transfer fee — per-launch rate, same split
// ---------------------------------------------------------------------------
/// Transfer fee applied to every post-graduation transfer of a Standard
/// (memecoin) launch: 0 bps. Zero-fee forever-trading for memecoins —
/// matches pump.fun convention. Trading fees come entirely from the
/// bonding curve pre-graduation.
pub const STANDARD_TRANSFER_FEE_BPS: u16 = 0;
/// Transfer fee applied to every post-graduation transfer of a Premium
/// launch: 50 bps (0.5%). Creator gets forever revenue.
pub const PREMIUM_TRANSFER_FEE_BPS: u16 = 50;
/// Transfer fee applied to every post-graduation transfer of a Shopify
/// launch: 100 bps (1.0%).
pub const SHOPIFY_TRANSFER_FEE_BPS: u16 = 100;
/// Transfer fee applied to every post-graduation transfer of a Trader
/// launch: 50 bps (0.5%). Revenue routed to the trader-rewards bucket.
pub const TRADER_TRANSFER_FEE_BPS: u16 = 50;
/// Transfer fee applied to every post-graduation transfer of a Community
/// launch: 50 bps (0.5%). 100% of revenue accrues to trader-rewards
/// bucket.
pub const COMMUNITY_TRANSFER_FEE_BPS: u16 = 50;

/// Creator share of post-graduation transfer-fee harvests: 80%.
pub const POST_GRAD_CREATOR_SHARE_BPS: u16 = 8_000;
/// Platform share of post-graduation transfer-fee harvests: 20%.
pub const POST_GRAD_PLATFORM_SHARE_BPS: u16 = 2_000;

// ---------------------------------------------------------------------------
// Charity overlay
// ---------------------------------------------------------------------------
/// Hard ceiling on the charity share of fees, in bps. 10% — charity
/// is an overlay, not the primary economic destination.
pub const CHARITY_BPS_MAX: u16 = 1_000;

// ---------------------------------------------------------------------------
// Atomic launch+buy + first-buy safety
// ---------------------------------------------------------------------------
/// Default cap on a creator's first-buy amount in an atomic launch+buy,
/// expressed as a bps share of `total_supply`. 500 bps = 5%.
pub const DEFAULT_MAX_CREATOR_FIRST_BUY_BPS: u16 = 500;

/// Default optional first-buy-as-share-of-supply cap (bps). 0 disables it.
pub const DEFAULT_FIRST_BUY_MAX_PCT_OF_SUPPLY: u16 = 0;

// ---------------------------------------------------------------------------
// Pump.fun-parity virtual token offset (Standard curve)
// ---------------------------------------------------------------------------
/// Standard-curve virtual token offset in base units (decimals=6 assumed
/// by spec). 280_000_000 tokens × 10^6 = 280_000_000_000_000. Matches
/// pump.fun's `virtual_token_reserves − real_token_reserves` gap.
pub const STANDARD_VIRTUAL_TOKEN_OFFSET: u64 = 280_000_000 * 1_000_000;

/// Standard-curve total supply (base units, decimals=6).
pub const STANDARD_TOTAL_SUPPLY: u64 = 1_000_000_000 * 1_000_000;

/// Standard-curve tokens on curve at launch (base units, decimals=6).
pub const STANDARD_TOKEN_ON_CURVE: u64 = 793_100_000 * 1_000_000;

/// Standard-curve SOL listed at launch (lamports).
pub const STANDARD_SOL_LISTED: u64 = 30 * 1_000_000_000;

/// Bound virtual SOL to the supported graduation range (1,000 SOL). Leaves
/// ample real-reserve headroom; buys also check the resulting virtual reserve.
pub const MAX_SOL_LISTED: u64 = MAX_GRADUATION_SOL_THRESHOLD;

/// Token-2022 `TransferFeeConfig` maximum-fee ceiling, in raw token units.
/// `u64::MAX` disables the absolute cap so the bps rate applies unclamped.
pub const TRANSFER_FEE_MAXIMUM: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// Platform fee distribution (default: 100% treasury, rest reserved)
// ---------------------------------------------------------------------------
pub const DEFAULT_TREASURY_SHARE_BPS: u16 = 10_000;
pub const DEFAULT_NFT_REWARDS_SHARE_BPS: u16 = 0;
pub const DEFAULT_LP_INCENTIVE_SHARE_BPS: u16 = 0;
pub const DEFAULT_ECOSYSTEM_SHARE_BPS: u16 = 0;

// ---------------------------------------------------------------------------
// Safety bounds
// ---------------------------------------------------------------------------
/// Hard ceiling on any individual fee bps value accepted via instruction
/// parameters. 50% — same invariant as pre-Token-2022 program.
pub const MAX_FEE_BPS: u16 = 5_000;

/// Hard ceiling on any individual SHARE bps value.
///
/// A share is a PROPORTION of the fee and the three shares must sum to exactly
/// BPS_DENOMINATOR, so the only meaningful ceiling is the denominator itself.
/// This used to reuse MAX_FEE_BPS, which is a ceiling on a fee RATE (50%), and
/// applying a rate cap to a proportion silently meant no recipient could ever
/// take more than half the fee. That made the founder's 2026-09-03 decision of
/// 60/40 platform/creator impossible to set: set_bonding_shares(6000, 4000, 0)
/// reverted with InvalidFeeShares. Two different units, one constant.
/// The sum == BPS_DENOMINATOR check is what actually bounds this.
pub const MAX_SHARE_BPS: u16 = BPS_DENOMINATOR;

/// Maximum supported token decimals for a launched mint.
pub const MAX_TOKEN_DECIMALS: u8 = 9;

// ---------------------------------------------------------------------------
// Graduation
// ---------------------------------------------------------------------------
/// Default graduation trigger: the curve graduates to Raydium CPMM once
/// `LaunchedCoin.sol_reserves` crosses this threshold. 84.9 SOL sits just
/// below the Standard curve's 84.975 SOL ceiling and close to the BonkFun
/// convention and pump.fun's effective ~$69 k market
/// cap at mid-range SOL prices — a familiar ladder for traders migrating
/// from other launchpads.
///
/// This is the *default* written onto `Config` at `initialize`; admins can
/// tune it per-deployment, and each launch snapshots its active value into
/// `LaunchedCoin.graduation_sol_threshold` so later Config changes don't
/// retroactively move the goalposts on in-flight launches.
///
pub const DEFAULT_GRADUATION_SOL_THRESHOLD: u64 = 84_900_000_000;

/// Minimum value accepted for a per-launch `graduation_sol_threshold`
/// override passed through `LaunchCoinParams`. 1 SOL floors the trigger
/// so a malformed override can't graduate a launch instantly.
pub const MIN_GRADUATION_SOL_THRESHOLD: u64 = 1_000_000_000;

/// Maximum value accepted for a per-launch `graduation_sol_threshold`
/// override. 1000 SOL ceiling keeps curves economically meaningful —
/// above this the curve is effectively never going to graduate and the
/// override is almost certainly a mistake.
pub const MAX_GRADUATION_SOL_THRESHOLD: u64 = 1_000 * 1_000_000_000;

/// All three graduation venues may retire the curve with at most 1% unsold.
/// A low caller-selected SOL threshold alone must not sweep a mostly unsold
/// curve into the pool. The standard curve has only 0.023% unsold at 84.9 SOL.
pub const MAX_UNSOLD_AT_GRADUATION_BPS: u16 = 100;

/// Minimum pool SOL/token price relative to the curve's final virtual-reserve
/// spot price: 95%. The supply cap alone cannot bound price when supply and
/// virtual offset are caller-selected. Standard pricing is about 99.97% before
/// graduation costs; the 5% allowance accommodates rent and rounding.
pub const MIN_GRADUATION_PRICE_BPS: u16 = 9_500;
/// Symmetric ceiling prevents extracting SOL from an overpriced opening pool.
pub const MAX_GRADUATION_PRICE_BPS: u16 = 10_500;

// ---------------------------------------------------------------------------
// Admin orphan-PDA close
// ---------------------------------------------------------------------------
/// Maximum lamport balance an orphan PDA may hold and still be closeable
/// via `admin_close_orphan_pda`. 0.01 SOL = 10_000_000 lamports. Accounts
/// heavier than this must be handled by a bespoke close instruction to
/// avoid ever sweeping a populated account.
pub const ORPHAN_CLOSE_DUST_THRESHOLD: u64 = 10_000_000;

/// Hardcoded whitelist of single-seed PDA shapes the admin may close via
/// `admin_close_orphan_pda`. Deliberately narrow: only shapes known to be
/// legacy orphans (e.g. the pre-Token-2022 `rewards_pool` seed that left
/// dust at `Ft3MpbNpttoEg2TSR2TfwTCoHC2EqUUsv4VtqXLSxf1f`).
pub const ORPHAN_SEED_WHITELIST: &[&[u8]] = &[b"rewards_pool"];

/// Native SOL wrapped-mint (`wSOL`) address. Hard-coded so we don't need a
/// `Sysvar` round-trip and so mis-passed mints fail the constraint check
/// on the graduate instruction.
pub const WSOL_MINT: Pubkey =
    anchor_lang::solana_program::pubkey!("So11111111111111111111111111111111111111112");

/// 0.001 SOL in actual wSOL credits per permissionless position claim.
/// Prevents repeated dust claims from discarding fractional DAMM accrual.
/// Recipients may sign to withdraw positive dust; zero always rolls back.
pub const MIN_PERMISSIONLESS_METEORA_CLAIM_LAMPORTS: u64 = 1_000_000;

/// Give a usable preparation 1,500 slots to graduate before public cleanup.
/// Fixed protocol policy: no signer or Config can shorten or extend this window.
pub const METEORA_PREPARATION_ABORT_DELAY_SLOTS: u64 = 1_500;

/// Approved Raydium CPMM index-0 configuration for the selected cluster.
#[cfg(not(feature = "devnet"))]
pub const RAYDIUM_AMM_CONFIG: Pubkey = pubkey!("D4FPEruKEHrG5TenZ2mpDGEfu1iUvTiqBxvpU8HLBvC2");
#[cfg(feature = "devnet")]
pub const RAYDIUM_AMM_CONFIG: Pubkey = pubkey!("5MxLgy9oPdTC3YgkiePHqr3EoCRD9uLVYRQS2ANAs7wy");

/// Fixture fee values only. Runtime trusts the pinned canonical Raydium account
/// and validates fee domains; governance may change these values.
pub const RAYDIUM_CONFIG_INDEX: u16 = 0;
pub const RAYDIUM_TRADE_FEE_RATE: u64 = 2_500;
pub const RAYDIUM_PROTOCOL_FEE_RATE: u64 = 120_000;
pub const RAYDIUM_FUND_FEE_RATE: u64 = 40_000;
pub const RAYDIUM_CREATE_POOL_FEE: u64 = 150_000_000;
#[cfg(not(feature = "devnet"))]
pub const RAYDIUM_CREATOR_FEE_RATE: u64 = 500;
#[cfg(feature = "devnet")]
pub const RAYDIUM_CREATOR_FEE_RATE: u64 = 2_500;

/// Approved LIMITLESS Swap index-0 AmmConfig PDA
/// (`["amm_config", index_be]` under the swap program). One address for
/// every cluster: the swap program pins a single vanity ID
/// (`Swapx93vz7sdzrLGwoyBFJFtNtEZ7z5x22SLr2Se3pE`) for devnet and mainnet
/// alike, so unlike Raydium there is no cluster split here.
pub const LIMITLESS_AMM_CONFIG: Pubkey = pubkey!("H2wKFCe9aLC64RSV4a3BMsnMxPG4SmrPx6k2A2ubo9Ls");

/// Exact approved LIMITLESS Swap fee policy for graduation pools. Same
/// rates the swap team's own `create_amm_config` driver seeds (devnet_driver.ts
/// stage 1) and identical to the Raydium pins above, EXCEPT the pool
/// creation fee: 0 instead of 150_000_000 lamports. That difference is the
/// entire point of the venue — graduating costs pool rent (~0.008 SOL)
/// instead of rent + 0.15 SOL, and the graduated pool's fees stay inside
/// LIMITLESS instead of flowing to Raydium. Governance can mutate the
/// config account; a drift away from these pins fails graduation loudly
/// (InvalidLimitlessConfig) rather than silently repricing graduation.
pub const LIMITLESS_CONFIG_INDEX: u16 = 0;
pub const LIMITLESS_TRADE_FEE_RATE: u64 = 2_500;
pub const LIMITLESS_PROTOCOL_FEE_RATE: u64 = 120_000;
pub const LIMITLESS_FUND_FEE_RATE: u64 = 40_000;
pub const LIMITLESS_CREATE_POOL_FEE: u64 = 0;
pub const LIMITLESS_CREATOR_FEE_RATE: u64 = 500;

/// LIMITLESS Swap pool-creation-fee receiver. Address-pinned inside the
/// swap program's `initialize` (`create_pool_fee_reveiver::ID`); mirrored
/// here so this program can pre-validate the account it is about to fund.
/// With `LIMITLESS_CREATE_POOL_FEE == 0` nothing is ever transferred to it.
pub const LIMITLESS_CREATE_POOL_FEE_RECEIVER: Pubkey =
    pubkey!("BankLVDiJMEL96WyntftuRVX6TK4mxjwkX5GvhmCFSZP");

// ---------------------------------------------------------------------------
// Token-2022 program ID (mirrors `anchor_spl::token_2022::ID` but kept here
// as a compile-time constant for account-constraint matching).
// ---------------------------------------------------------------------------
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    anchor_lang::solana_program::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
