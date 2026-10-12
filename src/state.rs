use crate::*;
use sha2::{Digest, Sha256};
pub fn disc(name: &str) -> [u8; 8] {
    Sha256::digest(name.as_bytes())[..8].try_into().unwrap()
}
pub fn u16_at(d: &[u8], n: usize) -> u16 {
    u16::from_le_bytes(d[n..n + 2].try_into().unwrap())
}
pub fn u64_at(d: &[u8], n: usize) -> u64 {
    u64::from_le_bytes(d[n..n + 8].try_into().unwrap())
}
pub fn key_at(d: &[u8], n: usize) -> Pubkey {
    Pubkey::new_from_array(d[n..n + 32].try_into().unwrap())
}
#[derive(Clone, Copy, PartialEq)]
pub enum GraduationVenue {
    Raydium,
    LimitlessSwap,
    MeteoraDammV2,
}
#[derive(Clone)]
pub struct LaunchedCoin {
    pub mint: Pubkey,
    pub creator_fee_recipient: Pubkey,
    pub transfer_fee_bps: u16,
    pub decimals: u8,
    pub total_supply: u64,
    pub token_on_curve: u64,
    pub sol_listed: u64,
    pub tokens_remaining_on_curve: u64,
    pub sol_reserves: u64,
    pub cumulative_sol_volume: u128,
    pub cumulative_token_volume: u128,
    pub graduated: bool,
    pub graduation_sol_threshold: u64,
    pub virtual_token_offset: u64,
    pub charity_wallet: Pubkey,
    pub charity_bps: u16,
    pub version: u8,
    pub graduation_venue: GraduationVenue,
    pub reserved: [u8; 76],
    pub bump: u8,
}
impl LaunchedCoin {
    pub fn decode(d: &[u8]) -> Result<Self> {
        ensure(
            d.len() == 344 && d[..8] == disc("account:LaunchedCoin"),
            "Unsupported curve layout (legacy 129-byte curves are not supported)",
        )?;
        ensure(
            d[104] <= 5 && d[105] <= 6 && d[106] == 0 && d[182] <= 1 && d[266] <= 2,
            "Unknown curve variant",
        )?;
        ensure(
            u64_at(d, 126) > 0
                && u64_at(d, 118) > 0
                && u64_at(d, 118) <= u64_at(d, 110)
                && u64_at(d, 134) <= u64_at(d, 118)
                && u64_at(d, 215) > 0
                && u16_at(d, 107) <= 10_000
                && u16_at(d, 264) <= 1_000,
            "Invalid curve economics",
        )?;
        Ok(Self {
            mint: key_at(d, 8),
            creator_fee_recipient: key_at(d, 72),
            transfer_fee_bps: u16_at(d, 107),
            decimals: d[109],
            total_supply: u64_at(d, 110),
            token_on_curve: u64_at(d, 118),
            sol_listed: u64_at(d, 126),
            tokens_remaining_on_curve: u64_at(d, 134),
            sol_reserves: u64_at(d, 142),
            cumulative_sol_volume: u128::from_le_bytes(d[150..166].try_into().unwrap()),
            cumulative_token_volume: u128::from_le_bytes(d[166..182].try_into().unwrap()),
            graduated: d[182] != 0,
            graduation_sol_threshold: u64_at(d, 215),
            bump: d[223],
            virtual_token_offset: u64_at(d, 224),
            charity_wallet: key_at(d, 232),
            charity_bps: u16_at(d, 264),
            version: d[266],
            graduation_venue: match d[267] {
                0 => GraduationVenue::Raydium,
                1 => GraduationVenue::LimitlessSwap,
                2 => GraduationVenue::MeteoraDammV2,
                _ => return Err("Unknown venue".into()),
            },
            reserved: d[268..344].try_into().unwrap(),
        })
    }
    pub fn meteora_prepared(&self) -> bool {
        !self.graduated
            && self.graduation_venue == GraduationVenue::MeteoraDammV2
            && self.reserved[0] == 1
    }
    pub fn meteora_preparation_rent(&self) -> u64 {
        if self.meteora_prepared() {
            u64_at(&self.reserved, 1)
        } else {
            0
        }
    }
    pub fn curve_complete(&self) -> bool {
        self.graduated || self.meteora_prepared() || self.reserved[31] != 0
    }
}
#[derive(Clone)]
pub struct Config {
    pub treasury: Pubkey,
    pub fee: u16,
    pub shares: [u16; 3],
    pub first_buy_cap: u16,
}
impl Config {
    pub fn decode(d: &[u8]) -> Result<Self> {
        ensure(
            d.len() == 385 && d[..8] == disc("account:Config"),
            "Invalid Config",
        )?;
        let s = [u16_at(d, 106), u16_at(d, 108), u16_at(d, 110)];
        ensure(
            s.iter().map(|x| u32::from(*x)).sum::<u32>() == 10_000 && u16_at(d, 104) <= 10_000,
            "Invalid fee config",
        )?;
        Ok(Self {
            treasury: key_at(d, 40),
            fee: u16_at(d, 104),
            shares: s,
            first_buy_cap: u16_at(d, 145),
        })
    }
}
#[derive(Clone)]
pub struct Rent {
    rate: u64,
    threshold: f64,
}
impl Rent {
    pub fn decode(d: &[u8]) -> Result<Self> {
        ensure(d.len() == 17, "Invalid rent")?;
        let threshold = f64::from_le_bytes(d[8..16].try_into().unwrap());
        ensure(
            threshold.is_finite() && threshold >= 0.0,
            "Invalid rent threshold",
        )?;
        Ok(Self {
            rate: u64_at(d, 0),
            threshold,
        })
    }
    pub fn minimum_balance(&self, len: usize) -> u64 {
        ((128u128 + len as u128).saturating_mul(u128::from(self.rate)) as f64 * self.threshold)
            as u64
    }
}
#[derive(Clone)]
pub struct TransferFees {
    older: (u64, u64, u16),
    newer: (u64, u64, u16),
}
impl TransferFees {
    pub fn decode(d: &[u8], decimals: u8) -> Result<Self> {
        ensure(
            d.len() >= 82 && d[44] == decimals && d[45] == 1,
            "Invalid mint",
        )?;
        let mut fees = Self {
            older: (0, u64::MAX, 0),
            newer: (0, u64::MAX, 0),
        };
        if d.len() == 82 {
            return Ok(fees);
        }
        ensure(d.len() >= 166 && d[165] == 1, "Invalid extended mint")?;
        let mut i = 166;
        while i + 4 <= d.len() {
            let kind = u16_at(d, i);
            let len = u16_at(d, i + 2) as usize;
            i += 4;
            if kind == 0 {
                break;
            }
            ensure(i + len <= d.len(), "Truncated mint extension")?;
            if kind == 1 {
                ensure(len == 108, "Invalid transfer fees")?;
                let x = &d[i..i + len];
                fees.older = (u64_at(x, 72), u64_at(x, 80), u16_at(x, 88));
                fees.newer = (u64_at(x, 90), u64_at(x, 98), u16_at(x, 106));
            }
            // No hook program is allowed: native handlers supply no hook accounts.
            else if kind == 14 {
                ensure(
                    len == 64 && d[i + 32..i + 64] == [0; 32],
                    "Active transfer hook unsupported",
                )?;
            } else {
                ensure(matches!(kind, 3 | 18 | 19), "Unsupported mint extension")?;
            }
            i += len;
        }
        ensure(
            fees.older.2 <= 10_000 && fees.newer.2 <= 10_000,
            "Invalid transfer fee rate",
        )?;
        Ok(fees)
    }
    pub fn fee(&self, amount: u64, epoch: u64) -> u64 {
        let (_, max, bps) = if epoch >= self.newer.0 {
            self.newer
        } else {
            self.older
        };
        ((u128::from(amount) * u128::from(bps)).div_ceil(10_000) as u64).min(max)
    }
}
