//! Per-launch state PDA. Seeded by [`LAUNCHED_COIN_SEED`] + mint pubkey.
//!
//! Captures both the immutable record of what was launched (mint, creator,
//! launch type, transfer-fee rate that was set on the mint) and the mutable
//! trading state (remaining curve supply, SOL reserves, cumulative volume,
//! graduation flag).
//!
//! [`LAUNCHED_COIN_SEED`]: crate::constants::LAUNCHED_COIN_SEED

use anchor_lang::prelude::*;

/// Rust discriminants and historical Borsh ordinals differ: Premium/Trader/
/// Community serialize as 2/3/4. Preserve those bytes for existing accounts.
/// Do not reorder variants or reuse the retired Rust discriminants 2, 4 or 5.
#[derive(AnchorSerialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum LaunchType {
    /// Default memecoin-style launch. 0 bps Token-2022 transfer fee —
    /// zero-fee forever-trading, matches pump.fun convention. All trading
    /// revenue accrues via the bonding-curve fee before graduation.
    #[default]
    Standard = 0,
    /// Shopify-integrated brand launch. 100 bps Token-2022 transfer fee.
    /// Merchants accept the higher rate in exchange for better forever
    /// revenue and platform economics.
    Shopify = 1,
    // Discriminant 2 previously hosted an admin-gated Clean launch variant.
    // Removed in the V2 upgrade. The Rust discriminant remains reserved;
    // historical Borsh still encodes Premium at ordinal 2.
    /// Premium brand launch. 50 bps Token-2022 transfer fee. Creator-heavy
    /// post-grad revenue split (80% creator / 20% platform).
    Premium = 3,
    /// Reserved historical Trader discriminant. Decodable for legacy curves;
    /// rejected by both launch handlers. Never reuse this value.
    RetiredTrader = 4,
    /// Reserved historical Community discriminant. Never reuse this value.
    RetiredCommunity = 5,
}

impl AnchorDeserialize for LaunchType {
    fn deserialize_reader<R: std::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Standard),
            1 => Ok(Self::Shopify),
            2 => Ok(Self::Premium),
            3 => Ok(Self::RetiredTrader),
            // Also recognize the documented Community discriminant so raw
            // type 5 fails with RetiredLaunchType in either launch handler.
            4 | 5 => Ok(Self::RetiredCommunity),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid launch type",
            )),
        }
    }
}

impl LaunchType {
    pub fn validate_new_launch(self) -> Result<()> {
        require!(
            !matches!(self, Self::RetiredTrader | Self::RetiredCommunity),
            crate::errors::LaunchpadError::RetiredLaunchType
        );
        Ok(())
    }
}

/// Badge tier. Drives platform perks (homepage featuring, analytics access,
/// community badges) only — does NOT affect fee splits. Kept as `u8` so the
/// tier set can grow without breaking existing accounts.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum BadgeTier {
    #[default]
    Standard = 0,
    Bronze = 1,
    Silver = 2,
    Gold = 3,
    Platinum = 4,
    Diamond = 5,
    Obsidian = 6,
}

/// Bonding-curve shape discriminant. Kept as a field rather than hard-coded
/// so Milestone-2 can introduce new curve types (e.g. exponential, LBP).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum BondingCurveType {
    /// Constant-product xy=k bonding curve. The Phase-1 default.
    #[default]
    ConstantProduct = 0,
}

/// Post-graduation venue selector. Snapshot onto `LaunchedCoin` at launch
/// from `Config.graduation_venue_default` so later admin toggles don't
/// move in-flight launches to a different AMM.
///
/// **Backward-compat constraint:** `Raydium` MUST remain discriminant `0`.
/// Existing `LaunchedCoin` accounts from the pre-V2 binary have a zero
/// byte at the `graduation_venue` offset (it used to be part of
/// `reserved`). If `Raydium` weren't `0`, those accounts would fail
/// AnchorDeserialize.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum GraduationVenue {
    /// Raydium CPMM. The V1 behaviour — full `graduate` flow lands here.
    #[default]
    Raydium = 0,
    /// LIMITLESS Swap (our Raydium-CPMM fork). Graduates via the
    /// dedicated `graduate_limitless_swap` instruction; the Raydium
    /// `graduate` refuses this venue with `SwapVenueMismatch`.
    LimitlessSwap = 1,
    /// Meteora DAMM v2. Only the separate `graduate_meteora` instruction
    /// may consume reserves for this venue.
    MeteoraDammV2 = 2,
}

#[account]
pub struct LaunchedCoin {
    // -- Identity --
    pub mint: Pubkey,
    pub creator: Pubkey,
    /// Where the creator's share of fees is routed. May differ from
    /// `creator` (e.g. a multisig or a squad).
    pub creator_fee_recipient: Pubkey,

    // -- Launch metadata (immutable once set) --
    pub launch_type: LaunchType,
    pub badge_tier: BadgeTier,
    pub bonding_curve_type: BondingCurveType,
    /// Exact Token-2022 `TransferFeeConfig.transfer_fee_basis_points`
    /// value written onto the mint at launch time. Locked in by the
    /// revoke of `transfer_fee_config_authority`.
    pub transfer_fee_bps: u16,

    // -- Token economics (set at launch) --
    pub decimals: u8,
    pub total_supply: u64,
    pub token_on_curve: u64,
    pub sol_listed: u64,

    // -- Trading state (mutates on every buy/sell) --
    pub tokens_remaining_on_curve: u64,
    pub sol_reserves: u64,
    pub cumulative_sol_volume: u128,
    pub cumulative_token_volume: u128,

    // -- Graduation --
    pub graduated: bool,
    /// AMM pool account of the preferred post-graduation swap venue.
    /// Committed atomically at graduation; no admin may change this pointer.
    pub preferred_swap_venue: Pubkey,
    /// Snapshot of the graduation trigger (lamports) taken at launch.
    /// `graduate` fires when `sol_reserves >= graduation_sol_threshold`.
    /// Stored per-launch so Config-level threshold changes don't move the
    /// goalposts for in-flight launches.
    pub graduation_sol_threshold: u64,

    // -- PDA bookkeeping --
    pub bump: u8,

    // -- Pre-mainnet additions (pump.fun parity + charity overlay) --
    /// Pump.fun-style virtual token offset. Added to
    /// `tokens_remaining_on_curve` when the constant-product curve computes
    /// its `virtual_tokens_before`. Flattens the price curve so each SOL
    /// buys more tokens. For a Standard launch this equals 280M × 10^6.
    /// 0 disables the offset (backward-compat with old LIMITLESS curve).
    pub virtual_token_offset: u64,
    /// Optional charity destination (system account). `Pubkey::default()`
    /// means no charity overlay. Applies to both bonding-curve SOL fees
    /// and post-graduation transfer-fee harvests.
    pub charity_wallet: Pubkey,
    /// Charity share of total fees in bps. 0–`CHARITY_BPS_MAX` (1_000 = 10%).
    /// Carved out BEFORE the other shares split the remainder.
    pub charity_bps: u16,

    // -- V2 additions (struct versioning + venue prep) ---------------------
    /// Layout-version tag. `0` = pre-V2 binary (field was zero inside
    /// `reserved`), `1` = live V2, `2` = proportional allocation economics and explicit
    /// consent marker in reserved[17]. Always increment when fields are added
    /// to the end of the struct so migration code can branch on it.
    pub version: u8,
    /// Post-graduation venue snapshot, taken at launch from
    /// `Config.graduation_venue_default`. Immutable per-launch.
    /// For version=0 accounts this deserializes as `Raydium` (zero byte).
    pub graduation_venue: GraduationVenue,

    /// Forward-compatibility padding. Shrunk from 78 → 76 to accommodate
    /// the two V2 fields above.
    pub reserved: [u8; 76],
}

impl LaunchedCoin {
    /// Immutable creator/platform shares in reserved[26..31]. Kept separate
    /// from consent and Meteora preparation bytes, without changing the layout.
    pub fn commit_post_graduation_split(&mut self, creator: u16, platform: u16) -> Result<()> {
        require!(
            self.reserved[26..31] == [0; 5],
            crate::errors::LaunchpadError::InvalidPostGraduationSplit
        );
        crate::instructions::set_post_graduation_split::validate_post_graduation_split(
            creator, platform,
        )?;
        self.reserved[26] = 1;
        self.reserved[27..29].copy_from_slice(&creator.to_le_bytes());
        self.reserved[29..31].copy_from_slice(&platform.to_le_bytes());
        Ok(())
    }

    pub fn post_graduation_split(&self, config: &super::Config) -> Result<(u16, u16)> {
        let shares = match self.reserved[26] {
            0 => return config.legacy_post_graduation_split(),
            1 => (
                u16::from_le_bytes(self.reserved[27..29].try_into().unwrap()),
                u16::from_le_bytes(self.reserved[29..31].try_into().unwrap()),
            ),
            _ => return err!(crate::errors::LaunchpadError::InvalidPostGraduationSplit),
        };
        crate::instructions::set_post_graduation_split::validate_post_graduation_split(
            shares.0, shares.1,
        )?;
        Ok(shares)
    }

    /// Only a signed creator and beneficiary (or the signing buyer itself)
    /// authenticate launch attribution. Legacy unsigned records are unverified.
    pub fn launch_consent_verified(&self) -> bool {
        self.version >= 2 && self.reserved[17] == 1
    }

    /// Monotonic completion bit in previously unused padding. Legacy accounts
    /// also use the immutable threshold/sold-out predicate at trade admission.
    /// This bit is never cleared, including on preparation recovery.
    pub fn curve_complete(&self) -> bool {
        self.graduated || self.meteora_prepared() || self.reserved[31] != 0
    }

    pub fn mark_curve_complete(&mut self) {
        self.reserved[31] = 1;
    }

    // Meteora staging state is independent of permanent curve completion.
    pub fn meteora_prepared(&self) -> bool {
        !self.graduated
            && self.graduation_venue == GraduationVenue::MeteoraDammV2
            && self.reserved[0] == 1
    }

    pub fn meteora_preparation_rent(&self) -> u64 {
        if self.meteora_prepared() {
            u64::from_le_bytes(self.reserved[1..9].try_into().unwrap())
        } else {
            0
        }
    }

    /// Gross pool contribution validated at preparation. Zero means
    /// a legacy preparation without this snapshot.
    pub fn meteora_preparation_tokens(&self) -> u64 {
        if self.meteora_prepared() {
            u64::from_le_bytes(self.reserved[9..17].try_into().unwrap())
        } else {
            0
        }
    }

    /// Slot of the first successful preparation; retries do not extend it.
    /// This cleanup clock never authorizes reopening trading.
    pub fn meteora_preparation_started_at(&self) -> u64 {
        if self.meteora_prepared() {
            u64::from_le_bytes(self.reserved[18..26].try_into().unwrap())
        } else {
            0
        }
    }

    pub fn set_meteora_preparation_started_at(&mut self, slot: u64) {
        self.reserved[18..26].copy_from_slice(&slot.to_le_bytes());
    }

    pub fn mark_meteora_prepared(&mut self, rent: u64, tokens: u64) {
        self.mark_curve_complete();
        self.set_meteora_preparation_started_at(0);
        self.reserved[0] = 1;
        self.reserved[1..9].copy_from_slice(&rent.to_le_bytes());
        self.reserved[9..17].copy_from_slice(&tokens.to_le_bytes());
    }

    pub const LEN: usize = 8  // discriminator
        + 32                  // mint
        + 32                  // creator
        + 32                  // creator_fee_recipient
        + 1                   // launch_type
        + 1                   // badge_tier
        + 1                   // bonding_curve_type
        + 2                   // transfer_fee_bps
        + 1                   // decimals
        + 8                   // total_supply
        + 8                   // token_on_curve
        + 8                   // sol_listed
        + 8                   // tokens_remaining_on_curve
        + 8                   // sol_reserves
        + 16                  // cumulative_sol_volume
        + 16                  // cumulative_token_volume
        + 1                   // graduated
        + 32                  // preferred_swap_venue
        + 8                   // graduation_sol_threshold
        + 1                   // bump
        + 8                   // virtual_token_offset
        + 32                  // charity_wallet
        + 2                   // charity_bps
        + 1                   // version (V2)
        + 1                   // graduation_venue (V2)
        + 76; // reserved
}

/// Offset of the `graduated` byte within a serialized `LaunchedCoin`
/// account, measured from the *start* of the serialized bytes (Anchor's
/// 8-byte discriminator is byte 0). Exposed for cross-program readers
/// (fee_splitter's `admin_sweep_custody`) that don't link against this
/// crate's types.
///
/// Layout: 8 (discr) + 32 (mint) + 32 (creator) + 32 (creator_fee_recipient)
/// + 1 (launch_type) + 1 (badge_tier) + 1 (bonding_curve_type) + 2 (transfer_fee_bps)
/// + 1 (decimals) + 8 (total_supply) + 8 (token_on_curve) + 8 (sol_listed)
/// + 8 (tokens_remaining_on_curve) + 8 (sol_reserves) + 16 (cumulative_sol_volume)
/// + 16 (cumulative_token_volume) = **182**.
///
/// Keep in sync with the struct layout above. Unit-tested in
/// `tests::graduated_byte_offset_is_stable`.
pub const LAUNCHED_COIN_GRADUATED_OFFSET: usize = 182;

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::AnchorSerialize;

    #[test]
    fn retired_launch_types_keep_their_wire_values_and_cannot_launch() {
        for (kind, byte) in [
            (LaunchType::Standard, 0),
            (LaunchType::Shopify, 1),
            (LaunchType::Premium, 2),
            (LaunchType::RetiredTrader, 3),
            (LaunchType::RetiredCommunity, 4),
        ] {
            assert_eq!(kind.try_to_vec().unwrap(), vec![byte]);
            assert_eq!(LaunchType::try_from_slice(&[byte]).unwrap(), kind);
            if byte == 3 || byte == 4 {
                assert_eq!(
                    kind.validate_new_launch().unwrap_err(),
                    crate::errors::LaunchpadError::RetiredLaunchType.into()
                );
            } else {
                kind.validate_new_launch().unwrap();
            }
        }
        assert_eq!(LaunchType::RetiredTrader as u8, 4);
        assert_eq!(LaunchType::RetiredCommunity as u8, 5);
        assert_eq!(
            LaunchType::try_from_slice(&[5])
                .unwrap()
                .validate_new_launch()
                .unwrap_err(),
            crate::errors::LaunchpadError::RetiredLaunchType.into()
        );
        assert!(LaunchType::try_from_slice(&[6]).is_err());
    }

    /// Guard: if anyone reorders `LaunchedCoin` fields and this offset
    /// breaks, fee_splitter's `admin_sweep_custody` will silently read
    /// the wrong byte. Asserting it here lets cargo test catch drift.
    #[test]
    fn graduated_byte_offset_is_stable() {
        let coin = LaunchedCoin {
            mint: Pubkey::default(),
            creator: Pubkey::default(),
            creator_fee_recipient: Pubkey::default(),
            launch_type: LaunchType::Standard,
            badge_tier: BadgeTier::Standard,
            bonding_curve_type: BondingCurveType::ConstantProduct,
            transfer_fee_bps: 0,
            decimals: 6,
            total_supply: 0,
            token_on_curve: 0,
            sol_listed: 0,
            tokens_remaining_on_curve: 0,
            sol_reserves: 0,
            cumulative_sol_volume: 0,
            cumulative_token_volume: 0,
            graduated: true,
            preferred_swap_venue: Pubkey::default(),
            graduation_sol_threshold: 0,
            bump: 0,
            virtual_token_offset: 0,
            charity_wallet: Pubkey::default(),
            charity_bps: 0,
            version: 1,
            graduation_venue: GraduationVenue::Raydium,
            reserved: [0u8; 76],
        };
        let mut bytes = Vec::new();
        coin.try_serialize(&mut bytes).unwrap();
        // Add the 8-byte discriminator offset (Anchor prepends it).
        assert_eq!(bytes[LAUNCHED_COIN_GRADUATED_OFFSET], 1);
        // Clear graduated to false; byte at offset should go to zero.
        let mut coin2 = coin;
        coin2.graduated = false;
        let mut bytes2 = Vec::new();
        coin2.try_serialize(&mut bytes2).unwrap();
        assert_eq!(bytes2[LAUNCHED_COIN_GRADUATED_OFFSET], 0);
    }

    /// Guard: account size must remain exactly at the pre-V2 value. We're
    /// pulling 2 bytes out of `reserved` and naming them, so total LEN is
    /// unchanged and existing on-chain accounts stay readable.
    #[test]
    fn account_len_unchanged_from_pre_v2() {
        // Pre-V2 LEN expressed from first principles (discriminator +
        // named fields + 78 reserved bytes).
        let pre_v2_len = 8
            + 32
            + 32
            + 32
            + 1
            + 1
            + 1
            + 2
            + 1
            + 8
            + 8
            + 8
            + 8
            + 8
            + 16
            + 16
            + 1
            + 32
            + 8
            + 1
            + 8
            + 32
            + 2
            + 78;
        assert_eq!(LaunchedCoin::LEN, pre_v2_len);
    }

    /// Guard: `GraduationVenue::Raydium` must be discriminant 0. Any
    /// change breaks deserialization of every pre-V2 account (the byte
    /// at `graduation_venue` is zero from old reserved space).
    #[test]
    fn graduation_venue_raydium_is_zero() {
        let venue = GraduationVenue::Raydium;
        let mut bytes = Vec::new();
        venue.serialize(&mut bytes).unwrap();
        assert_eq!(bytes, vec![0u8]);
    }

    /// Guard: the appended Meteora venue keeps the single-byte wire values of
    /// every existing venue, and an unknown venue byte still fails closed.
    #[test]
    fn preparation_freezes_only_ungraduated_meteora_coins_and_preserves_layout() {
        use anchor_lang::Discriminator;
        let mut legacy = vec![0u8; LaunchedCoin::LEN];
        legacy[..8].copy_from_slice(LaunchedCoin::DISCRIMINATOR);
        let mut coin = LaunchedCoin::try_deserialize(&mut &legacy[..]).unwrap();
        assert!(!coin.meteora_prepared());
        coin.mark_meteora_prepared(123_456, 789);
        assert!(!coin.meteora_prepared());
        assert_eq!(coin.meteora_preparation_rent(), 0);
        coin.graduation_venue = GraduationVenue::MeteoraDammV2;
        assert!(coin.meteora_prepared());
        assert_eq!(coin.meteora_preparation_rent(), 123_456);
        assert_eq!(coin.meteora_preparation_tokens(), 789);
        assert_eq!(coin.meteora_preparation_started_at(), 0);
        coin.reserved[17] = 1;
        coin.set_meteora_preparation_started_at(123_456_789);
        assert_eq!(coin.meteora_preparation_started_at(), 123_456_789);
        assert_eq!(coin.reserved[17], 1);
        let mut serialized = Vec::new();
        coin.try_serialize(&mut serialized).unwrap();
        assert_eq!(serialized.len(), LaunchedCoin::LEN);
        assert_eq!(serialized[LAUNCHED_COIN_GRADUATED_OFFSET], 0);
        coin.graduated = true;
        assert!(!coin.meteora_prepared());
        assert_eq!(coin.meteora_preparation_rent(), 0);
    }

    #[test]
    fn graduation_venues_preserve_single_byte_wire_values() {
        for (venue, byte) in [
            (GraduationVenue::Raydium, 0),
            (GraduationVenue::LimitlessSwap, 1),
            (GraduationVenue::MeteoraDammV2, 2),
        ] {
            assert_eq!(venue.try_to_vec().unwrap(), vec![byte]);
            assert_eq!(GraduationVenue::try_from_slice(&[byte]).unwrap(), venue);
        }
        assert!(GraduationVenue::try_from_slice(&[3]).is_err());
        assert_eq!(GraduationVenue::default(), GraduationVenue::Raydium);
    }
}
