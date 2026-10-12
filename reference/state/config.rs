//! Platform `Config` PDA — single global record, seeded by [`CONFIG_SEED`].
//!
//! Every economically meaningful parameter on the launchpad is stored here
//! and mutable by the configured `admin`. A launch at `launch_coin` time
//! snapshots the relevant field(s) into the `LaunchedCoin` PDA so later
//! parameter changes do not retroactively alter existing launches.
//!
//! The `reserved` tail is sized so we can add u16/u64 fields in-place during
//! Milestone-2 without forcing a migration.
//!
//! [`CONFIG_SEED`]: crate::constants::CONFIG_SEED

use anchor_lang::prelude::*;

use crate::constants::{MAX_GRADUATION_SOL_THRESHOLD, MIN_GRADUATION_SOL_THRESHOLD};
use crate::errors::LaunchpadError;
use crate::state::launched_coin::{GraduationVenue, LaunchType};

/// Platform-side distribution of the non-creator, non-rewards portion of
/// fees. Currently 100% treasury; the other slots are reserved so we can
/// shift share to NFT rewards, LP incentives, or an ecosystem fund without
/// a program upgrade.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeeDistributionConfig {
    pub treasury_share_bps: u16,
    pub nft_rewards_share_bps: u16,
    pub lp_incentive_share_bps: u16,
    pub ecosystem_share_bps: u16,
}

impl FeeDistributionConfig {
    pub fn sum_bps(&self) -> u32 {
        self.treasury_share_bps as u32
            + self.nft_rewards_share_bps as u32
            + self.lp_incentive_share_bps as u32
            + self.ecosystem_share_bps as u32
    }
}

#[account]
pub struct Config {
    /// Admin key. Rotates via `update_admin`. Governs every admin-only
    /// instruction on this program.
    pub admin: Pubkey,

    /// Destination for the platform's share of SOL fees.
    pub treasury: Pubkey,

    /// Program ID of the companion fee-splitter program. The launchpad
    /// uses this to derive the `withdraw_withheld_authority` PDA at mint
    /// creation time.
    pub fee_splitter_program: Pubkey,

    // -- Bonding-curve fees (in-platform trading) --
    pub bonding_curve_fee_bps: u16,
    pub bonding_platform_share_bps: u16,
    pub bonding_creator_share_bps: u16,
    pub bonding_rewards_share_bps: u16,

    // -- Post-graduation Token-2022 transfer fees (all venues) --
    /// Transfer fee applied to Standard / memecoin launches.
    pub standard_transfer_fee_bps: u16,
    /// Transfer fee applied to Shopify-integrated launches.
    pub shopify_transfer_fee_bps: u16,
    /// Creator share of harvested post-graduation transfer fees.
    pub post_grad_creator_share_bps: u16,
    /// Platform share of harvested post-graduation transfer fees.
    pub post_grad_platform_share_bps: u16,

    /// Platform-side distribution of the platform's share.
    pub distribution: FeeDistributionConfig,

    /// Default graduation trigger (lamports) applied to new launches.
    /// Each launch snapshots this value into `LaunchedCoin` so later edits
    /// don't change the bar for already-live launches.
    pub graduation_sol_threshold: u64,

    /// Canonical PDA bump.
    pub bump: u8,

    // -- Pre-mainnet additions (new launch types + safety caps) --
    /// Transfer fee applied to Premium launches.
    pub premium_transfer_fee_bps: u16,
    /// Transfer fee applied to Trader-rewards launches.
    pub trader_transfer_fee_bps: u16,
    /// Transfer fee applied to Community launches.
    pub community_transfer_fee_bps: u16,
    /// Cap on a creator's first-buy amount in an atomic launch+buy,
    /// expressed as a bps share of `total_supply`. 0 disables the cap.
    /// Default: 500 bps (5%).
    pub max_creator_first_buy_bps: u16,
    /// Optional first-buy-as-share-of-supply cap in bps. Enforced on the
    /// *first* trade only (when `cumulative_token_volume == 0`). 0 disables.
    pub first_buy_max_pct_of_supply: u16,

    // -- V2 additions -----------------------------------------------------
    /// RETIRED. The pause system was removed 2026-09-15 on founder order.
    /// No instruction reads or writes this field; initialize writes false.
    /// Kept solely to preserve the Config account layout for the upgrade.
    pub is_paused: bool,
    /// Default post-graduation venue snapshot written onto new
    /// `LaunchedCoin` accounts at `launch_coin` time. Existing launches
    /// keep their per-launch snapshot even if this default flips later.
    pub graduation_venue_default: GraduationVenue,

    /// Meteora-only fee in tenths of a basis point despite the retained wire name.
    /// 1250 = 125 actual bps = 1.25%; zero selects this default.
    /// Taken from reserved space; existing fields and account size stay fixed.
    pub meteora_pool_fee_bps: u16,
    /// New-launch admission only; never gates existing curves' graduation.
    /// Carved from legacy zero padding; never inferred from a build feature.
    pub meteora_graduation_enabled: bool,
    /// DAMM policy hint and new-launch admission; existing coins are not pinned.
    pub meteora_config: Pubkey,
    /// Static policy fees come from DAMM, never update_meteora_pool_fee.
    pub meteora_config_is_static: bool,
    /// Forward-compatibility padding; bytes 0..37 retain legacy policy data.
    /// The former graduation operator bytes are inert padding again.
    pub reserved: [u8; 200],
}

impl Config {
    /// Independently selected 1% static policy for pre-V2 launches. Zero means
    /// no separate selection. Uses reserved[5..37], after the legacy split,
    /// without changing the Config layout or the fee of either launch version.
    pub fn legacy_meteora_config(&self) -> Pubkey {
        Pubkey::new_from_array(self.reserved[5..37].try_into().unwrap())
    }

    pub fn set_legacy_meteora_config(&mut self, key: Pubkey) {
        self.reserved[5..37].copy_from_slice(key.as_ref());
    }

    /// Keep the old admin selection usable when the primary selection is
    /// replaced or cleared. A separately selected legacy policy takes priority.
    pub fn preserve_legacy_meteora_config(&mut self) {
        if self.legacy_meteora_config() == Pubkey::default()
            && self.meteora_config_is_static
            && self.meteora_pool_fee_bps == 1000
        {
            self.set_legacy_meteora_config(self.meteora_config);
        }
    }

    /// Shares for launches that predate per-launch commitments. The first
    /// setter call preserves the preceding policy in reserved[0..5], forever.
    /// Until then, the global policy has not changed under this implementation.
    pub fn legacy_post_graduation_split(&self) -> Result<(u16, u16)> {
        let shares = match self.reserved[0] {
            0 => (
                self.post_grad_creator_share_bps,
                self.post_grad_platform_share_bps,
            ),
            1 => (
                u16::from_le_bytes(self.reserved[1..3].try_into().unwrap()),
                u16::from_le_bytes(self.reserved[3..5].try_into().unwrap()),
            ),
            _ => return err!(LaunchpadError::InvalidPostGraduationSplit),
        };
        crate::instructions::set_post_graduation_split::validate_post_graduation_split(
            shares.0, shares.1,
        )?;
        Ok(shares)
    }

    pub fn preserve_legacy_post_graduation_split(&mut self) -> Result<()> {
        let (creator, platform) = self.legacy_post_graduation_split()?;
        if self.reserved[0] == 0 {
            self.reserved[0] = 1;
            self.reserved[1..3].copy_from_slice(&creator.to_le_bytes());
            self.reserved[3..5].copy_from_slice(&platform.to_le_bytes());
        }
        Ok(())
    }

    /// Resolve and validate the graduation threshold snapshotted by either
    /// launch entry point.
    pub fn graduation_threshold_for_new_launch(&self, override_value: Option<u64>) -> Result<u64> {
        let effective_threshold = override_value.unwrap_or(self.graduation_sol_threshold);
        require!(
            (MIN_GRADUATION_SOL_THRESHOLD..=MAX_GRADUATION_SOL_THRESHOLD)
                .contains(&effective_threshold),
            LaunchpadError::InvalidGraduationThreshold
        );

        Ok(effective_threshold)
    }

    /// Resolve the graduation venue snapshotted by either launch entry point.
    ///
    /// Validate stored policy at launch time as well as in the admin setters.
    /// Legacy Meteora policies can remain enabled for in-flight v0/v1 coins,
    /// but neither launch entry point may snapshot them for a new v2 coin.
    pub fn graduation_venue_for_new_launch(&self) -> Result<GraduationVenue> {
        match self.graduation_venue_default {
            GraduationVenue::Raydium => Ok(GraduationVenue::Raydium),
            // Delivered by `graduate_limitless_swap`. No extra enable flag:
            // snapshotting this venue is already admin-gated upstream by
            // `update_graduation_venue_default`, which is the arm switch.
            GraduationVenue::LimitlessSwap => Ok(GraduationVenue::LimitlessSwap),
            GraduationVenue::MeteoraDammV2 => {
                // Mirror the admin setter's gate: a launch can only snapshot the
                // Meteora venue while it is explicitly enabled.
                require!(
                    self.meteora_graduation_enabled,
                    LaunchpadError::MeteoraGraduationDisabled
                );
                require!(
                    self.meteora_config != Pubkey::default()
                        && if self.meteora_config_is_static {
                            self.meteora_pool_fee_bps == 1250
                        } else {
                            self.meteora_pool_fee_bps() == 1250
                        },
                    LaunchpadError::MeteoraAccountMismatch
                );
                Ok(GraduationVenue::MeteoraDammV2)
            }
        }
    }

    /// Resolve the Meteora pool fee in tenths of a basis point.
    /// 1250 = 1.25% is the founder-locked default when the stored value is zero.
    pub fn meteora_pool_fee_bps(&self) -> u16 {
        let meteora_pool_fee_bps = 1250;
        match self.meteora_pool_fee_bps {
            0 => meteora_pool_fee_bps,
            value => value,
        }
    }

    /// Resolve the post-graduation transfer-fee rate that a new launch must
    /// snapshot. Keeping this mapping in one place prevents the two launch
    /// entry points from silently assigning different economics.
    pub fn transfer_fee_bps_for_launch_type(&self, launch_type: LaunchType) -> u16 {
        match launch_type {
            LaunchType::Standard => self.standard_transfer_fee_bps,
            LaunchType::Premium => self.premium_transfer_fee_bps,
            LaunchType::Shopify => self.shopify_transfer_fee_bps,
            LaunchType::RetiredTrader => self.trader_transfer_fee_bps,
            LaunchType::RetiredCommunity => self.community_transfer_fee_bps,
        }
    }

    /// Total serialized size of the account (including Anchor's 8-byte
    /// discriminator). Kept as `const` so the `init` constraint in
    /// `Initialize` can reference it without computing at runtime.
    pub const LEN: usize = 8      // discriminator
        + 32                      // admin
        + 32                      // treasury
        + 32                      // fee_splitter_program
        + 2 * 4                   // 4x bonding-curve u16
        + 2 * 4                   // 4x post-grad u16
        + 2 * 4                   // 4x distribution u16
        + 8                       // graduation_sol_threshold
        + 1                       // bump
        + 2 * 5                   // 5x new pre-mainnet u16
        + 1                       // is_paused (V2)
        + 1                       // graduation_venue_default (V2)
        + 2                       // meteora_pool_fee_bps
        + 1                       // meteora_graduation_enabled
        + 32                      // meteora_config
        + 1                       // meteora_config_is_static
        + 200; // reserved (including the retired operator bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_config_serialization_pins_permissionless_splitter_policy_offsets() {
        let mut config = Config::try_deserialize_unchecked(&mut &[0; Config::LEN][..]).unwrap();
        config.admin = Pubkey::new_unique();
        config.treasury = Pubkey::new_unique();
        config.post_grad_creator_share_bps = 8000;
        config.post_grad_platform_share_bps = 2000;
        let mut bytes = Vec::new();
        config.try_serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), Config::LEN);
        assert_eq!(&bytes[..8], Config::DISCRIMINATOR);
        assert_eq!(&bytes[8..40], config.admin.as_ref());
        assert_eq!(&bytes[40..72], config.treasury.as_ref());
        assert_eq!(
            u16::from_le_bytes(bytes[116..118].try_into().unwrap()),
            8000
        );
        assert_eq!(
            u16::from_le_bytes(bytes[118..120].try_into().unwrap()),
            2000
        );
    }

    #[test]
    fn launch_snapshot_resolvers_are_exhaustive() {
        let config = Config {
            admin: Pubkey::new_unique(),
            treasury: Pubkey::new_unique(),
            fee_splitter_program: Pubkey::new_unique(),
            bonding_curve_fee_bps: 0,
            bonding_platform_share_bps: 0,
            bonding_creator_share_bps: 0,
            bonding_rewards_share_bps: 0,
            standard_transfer_fee_bps: 11,
            shopify_transfer_fee_bps: 33,
            post_grad_creator_share_bps: 0,
            post_grad_platform_share_bps: 0,
            distribution: FeeDistributionConfig::default(),
            graduation_sol_threshold: MIN_GRADUATION_SOL_THRESHOLD,
            bump: 0,
            premium_transfer_fee_bps: 22,
            trader_transfer_fee_bps: 44,
            community_transfer_fee_bps: 55,
            max_creator_first_buy_bps: 0,
            first_buy_max_pct_of_supply: 0,
            is_paused: true,
            graduation_venue_default: GraduationVenue::Raydium,
            meteora_pool_fee_bps: 0,
            meteora_graduation_enabled: false,
            meteora_config: Pubkey::default(),
            meteora_config_is_static: false,
            reserved: [0; 200],
        };

        assert_eq!(
            config.transfer_fee_bps_for_launch_type(LaunchType::Standard),
            11
        );
        assert_eq!(
            config.transfer_fee_bps_for_launch_type(LaunchType::Premium),
            22
        );
        assert_eq!(
            config.transfer_fee_bps_for_launch_type(LaunchType::Shopify),
            33
        );
        assert_eq!(
            config.transfer_fee_bps_for_launch_type(LaunchType::RetiredTrader),
            44
        );
        assert_eq!(
            config.transfer_fee_bps_for_launch_type(LaunchType::RetiredCommunity),
            55
        );
        assert_eq!(
            config.graduation_venue_for_new_launch().unwrap(),
            GraduationVenue::Raydium
        );

        // Both launch entry points use this resolver, so pin the snapshot's
        // default behavior, inclusive bounds, and fail-closed range checks.
        assert_eq!(
            config.graduation_threshold_for_new_launch(None).unwrap(),
            MIN_GRADUATION_SOL_THRESHOLD
        );
        assert_eq!(
            config
                .graduation_threshold_for_new_launch(Some(MIN_GRADUATION_SOL_THRESHOLD))
                .unwrap(),
            MIN_GRADUATION_SOL_THRESHOLD
        );
        assert_eq!(
            config
                .graduation_threshold_for_new_launch(Some(MAX_GRADUATION_SOL_THRESHOLD))
                .unwrap(),
            MAX_GRADUATION_SOL_THRESHOLD
        );
        assert!(config
            .graduation_threshold_for_new_launch(Some(MIN_GRADUATION_SOL_THRESHOLD - 1))
            .is_err());
        assert!(config
            .graduation_threshold_for_new_launch(Some(MAX_GRADUATION_SOL_THRESHOLD + 1))
            .is_err());

        // Stored defaults are state loaded from an existing account and must
        // fail closed under the same bounds as caller-provided overrides. An
        // invalid zero default would otherwise make a new launch immediately
        // eligible for graduation.
        let mut invalid_default_config = config;
        invalid_default_config.graduation_sol_threshold = MIN_GRADUATION_SOL_THRESHOLD - 1;
        assert!(invalid_default_config
            .graduation_threshold_for_new_launch(None)
            .is_err());
        invalid_default_config.graduation_sol_threshold = MAX_GRADUATION_SOL_THRESHOLD + 1;
        assert!(invalid_default_config
            .graduation_threshold_for_new_launch(None)
            .is_err());

        // A valid explicit override remains usable even if the stored default
        // is invalid, because the value actually snapshotted is still safe.
        assert_eq!(
            invalid_default_config
                .graduation_threshold_for_new_launch(Some(MIN_GRADUATION_SOL_THRESHOLD))
                .unwrap(),
            MIN_GRADUATION_SOL_THRESHOLD
        );

        // `LimitlessSwap` snapshots resolve since `graduate_limitless_swap`
        // shipped. A stored default selecting it only reaches launch_coin
        // through the admin setter, which is the venue's arm switch.
        invalid_default_config.graduation_venue_default = GraduationVenue::LimitlessSwap;
        assert_eq!(
            invalid_default_config
                .graduation_venue_for_new_launch()
                .unwrap(),
            GraduationVenue::LimitlessSwap
        );

        invalid_default_config.graduation_venue_default = GraduationVenue::MeteoraDammV2;
        assert!(invalid_default_config
            .graduation_venue_for_new_launch()
            .is_err());
        invalid_default_config.meteora_graduation_enabled = true;
        invalid_default_config.meteora_config = Pubkey::new_unique();
        assert_eq!(
            invalid_default_config
                .graduation_venue_for_new_launch()
                .unwrap(),
            GraduationVenue::MeteoraDammV2
        );
    }

    /// Guard: Config size must remain exactly at the pre-V2 value. The two
    /// V2 fields were pulled out of `reserved`, so total LEN is unchanged
    /// and existing on-chain Config accounts stay readable. The pre-V2
    /// `is_paused` byte is zero, so pre-V2 accounts deserialize as
    /// not-paused — the safe default.
    #[test]
    fn config_len_unchanged_from_pre_v2() {
        let pre_v2_len = 8 + 32 + 32 + 32 + 2 * 4 + 2 * 4 + 2 * 4 + 8 + 1 + 2 * 5 + 238;
        assert_eq!(Config::LEN, pre_v2_len);
    }

    #[test]
    fn new_meteora_launches_require_a_selected_v2_policy() {
        let mut config = Config::try_deserialize_unchecked(&mut &[0; Config::LEN][..]).unwrap();
        config.graduation_venue_default = GraduationVenue::MeteoraDammV2;
        for enabled in [false, true] {
            for selected in [false, true] {
                for is_static in [false, true] {
                    for fee in [0, 1000, 1250, 1300] {
                        config.meteora_graduation_enabled = enabled;
                        config.meteora_config = if selected {
                            Pubkey::new_unique()
                        } else {
                            Pubkey::default()
                        };
                        config.meteora_config_is_static = is_static;
                        config.meteora_pool_fee_bps = fee;
                        assert_eq!(
                            config.graduation_venue_for_new_launch().is_ok(),
                            enabled && selected && (fee == 1250 || (!is_static && fee == 0)),
                            "enabled={enabled}, selected={selected}, static={is_static}, fee={fee}"
                        );
                    }
                }
            }
        }
        // The admission guard is scoped to the selected venue.
        config.meteora_config = Pubkey::default();
        config.meteora_pool_fee_bps = 1000;
        for venue in [GraduationVenue::Raydium, GraduationVenue::LimitlessSwap] {
            config.graduation_venue_default = venue;
            assert_eq!(config.graduation_venue_for_new_launch().unwrap(), venue);
        }
    }

    /// Guard: the Meteora fields are carved from zero padding, so a legacy
    /// all-zero Config decodes with Meteora disabled, and writing a Meteora
    /// fee never moves a byte in front of (or behind) the carved window.
    #[test]
    fn legacy_config_padding_decodes_to_default_meteora_fee_without_moving_fields() {
        use anchor_lang::Discriminator;
        let mut legacy = vec![0u8; Config::LEN];
        legacy[..8].copy_from_slice(Config::DISCRIMINATOR);
        let mut config = Config::try_deserialize(&mut &legacy[..]).unwrap();
        assert!(!config.meteora_graduation_enabled);
        assert_eq!(config.meteora_config, Pubkey::default());
        assert!(!config.meteora_config_is_static);
        assert_eq!(config.meteora_pool_fee_bps, 0);
        assert_eq!(config.meteora_pool_fee_bps(), 1250);
        assert_eq!(config.graduation_venue_default, GraduationVenue::Raydium);
        config.meteora_pool_fee_bps = 300;
        let mut updated = Vec::new();
        config.try_serialize(&mut updated).unwrap();
        assert_eq!(updated.len(), Config::LEN);
        // The new fields start at the old 236-byte reserved region.
        let fee_offset = Config::LEN - 236;
        assert_eq!(&updated[..fee_offset], &legacy[..fee_offset]);
        assert_eq!(&updated[fee_offset..fee_offset + 2], &300u16.to_le_bytes());
        assert_eq!(&updated[fee_offset + 2..], &legacy[fee_offset + 2..]);
    }
}
