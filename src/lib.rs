//! Offline Jupiter quoting and native instruction metadata for LIMITLESS.
//! Placeholder swaps require Jupiter-side native SOL / WSOL orchestration before routing.
use jupiter_amm_interface::*;
use rust_decimal::Decimal;
use solana_account::{Account, ReadableAccount};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{Pubkey, pubkey};
use std::{collections::HashMap, sync::atomic::Ordering};
mod admission;
// Preserve the live arithmetic verbatim, including its pre-1.87 remainder expression.
#[allow(clippy::manual_is_multiple_of)]
pub mod math;
pub mod state;
use state::*;
pub type Result<T> = std::result::Result<T, AmmError>;
pub const PROGRAM: Pubkey = pubkey!("LimitQGBbcHeGJZd3FbQEejiCRuk1rfvwhbv8XCtAoN");
pub const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const TOKEN: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSTEM: Pubkey = pubkey!("11111111111111111111111111111111");
pub const RENT: Pubkey = pubkey!("SysvarRent111111111111111111111111111111111");
fn ensure(ok: bool, msg: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(msg.into()) }
}
pub fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &PROGRAM).0
}
pub fn ata(mint: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[owner.as_ref(), TOKEN.as_ref(), mint.as_ref()], &ATA).0
}
#[derive(Clone)]
struct Cached {
    config: Config,
    vault: u64,
    rewards: u64,
    rent: Rent,
    fees: TransferFees,
    accounts: HashMap<Pubkey, Account>,
    remaining: Vec<Pubkey>,
}
#[derive(Clone)]
pub struct LimitlessAmm {
    key: Pubkey,
    coin: LaunchedCoin,
    clock: ClockRef,
    cached: Option<Cached>,
}
impl LimitlessAmm {
    fn direction(&self, input: Pubkey, output: Pubkey, mode: SwapMode) -> Result<bool> {
        ensure(mode == SwapMode::ExactIn, "Only ExactIn is supported")?;
        if input == WSOL && output == self.coin.mint {
            Ok(true)
        } else if input == self.coin.mint && output == WSOL {
            Ok(false)
        } else {
            Err("Invalid mint pair".into())
        }
    }
    fn ready(&self) -> Result<&Cached> {
        let c = self
            .cached
            .as_ref()
            .ok_or(AmmError::from("update required"))?;
        ensure(
            !self.coin.curve_complete() && !admission::graduation_trigger_met(&self.coin)?,
            "Curve complete or graduated",
        )?;
        Ok(c)
    }
    fn math_error(e: math::MathError) -> AmmError {
        format!("{e:?}").into()
    }
    pub fn native_instruction(&self, params: &SwapParams) -> Result<Instruction> {
        let metas = self.get_swap_and_account_metas(params)?;
        Ok(Instruction {
            program_id: PROGRAM,
            accounts: metas.account_metas,
            data: encode_swap(&metas.swap, params.in_amount, params.out_amount)?,
        })
    }
}
impl Amm for LimitlessAmm {
    fn from_keyed_account(a: &KeyedAccount, ctx: &AmmContext) -> Result<Self> {
        ensure(
            a.account.owner == PROGRAM && !a.account.executable,
            "Invalid curve owner",
        )?;
        let coin = LaunchedCoin::decode(&a.account.data)?;
        let (expected, bump) =
            Pubkey::find_program_address(&[b"launched_coin", coin.mint.as_ref()], &PROGRAM);
        ensure(a.key == expected && coin.bump == bump, "Invalid curve PDA")?;
        Ok(Self {
            key: a.key,
            coin,
            clock: ctx.clock_ref.clone(),
            cached: None,
        })
    }
    fn label(&self) -> AmmLabel {
        "LIMITLESS"
    }
    fn program_id(&self) -> Pubkey {
        PROGRAM
    }
    fn key(&self) -> Pubkey {
        self.key
    }
    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![WSOL, self.coin.mint]
    }
    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let mut keys = vec![
            self.key,
            pda(&[b"config"]),
            pda(&[b"rewards_pool_v2"]),
            self.coin.mint,
            ata(&self.coin.mint, &self.key),
            RENT,
            self.coin.creator_fee_recipient,
            self.coin.charity_wallet,
            pda(&[b"creator_revenue", self.coin.mint.as_ref()]),
        ];
        if let Some(c) = &self.cached {
            keys.push(c.config.treasury);
            for recipient in [
                c.config.treasury,
                self.coin.creator_fee_recipient,
                self.coin.charity_wallet,
            ] {
                keys.push(pda(&[
                    b"bonding_fee_escrow",
                    self.coin.mint.as_ref(),
                    recipient.as_ref(),
                ]));
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }
    fn has_dynamic_accounts(&self) -> bool {
        true
    }
    fn update(&mut self, provider: impl AccountProvider) -> Result<()> {
        // Invalidate before parsing: a failed refresh must never leave stale tradable state.
        self.cached = None;
        let get = |key: Pubkey| -> Result<Account> {
            let a = provider.try_get(&key)?;
            Ok(Account {
                lamports: a.lamports(),
                data: a.data().to_vec(),
                owner: *a.owner(),
                executable: a.executable(),
                rent_epoch: a.rent_epoch(),
            })
        };
        let curve = get(self.key)?;
        ensure(curve.owner == PROGRAM, "Invalid curve owner")?;
        let coin = LaunchedCoin::decode(&curve.data)?;
        ensure(
            coin.mint == self.coin.mint && coin.bump == self.coin.bump,
            "Curve identity changed",
        )?;
        self.coin = coin;
        let config_account = get(pda(&[b"config"]))?;
        ensure(config_account.owner == PROGRAM, "Invalid config owner")?;
        let config = Config::decode(&config_account.data)?;
        let mint = get(self.coin.mint)?;
        ensure(mint.owner == TOKEN, "Not Token-2022")?;
        let fees = TransferFees::decode(&mint.data, self.coin.decimals)?;
        let rent = Rent::decode(&get(RENT)?.data)?;
        let vault_account = get(ata(&self.coin.mint, &self.key))?;
        ensure(
            vault_account.owner == TOKEN
                && vault_account.data.len() >= 165
                && key_at(&vault_account.data, 0) == self.coin.mint
                && key_at(&vault_account.data, 32) == self.key
                && vault_account.data[108] == 1,
            "Invalid vault",
        )?;
        let vault = u64_at(&vault_account.data, 64);
        ensure(
            curve.lamports
                >= rent
                    .minimum_balance(344)
                    .checked_add(self.coin.sol_reserves)
                    .ok_or(AmmError::from("Reserve overflow"))?,
            "Curve reserves not funded",
        )?;
        let rewards_account = get(pda(&[b"rewards_pool_v2"]))?;
        ensure(
            rewards_account.owner == PROGRAM
                && rewards_account.data.len() >= 25
                && rewards_account.data[..8] == disc("account:RewardsPool"),
            "Invalid rewards pool",
        )?;
        let rewards = u64_at(&rewards_account.data, 8);
        let mut accounts = HashMap::new();
        let mut remaining = vec![];
        for recipient in [
            config.treasury,
            self.coin.creator_fee_recipient,
            self.coin.charity_wallet,
        ] {
            if let Ok(a) = get(recipient) {
                accounts.insert(recipient, a);
            }
            let escrow = pda(&[
                b"bonding_fee_escrow",
                self.coin.mint.as_ref(),
                recipient.as_ref(),
            ]);
            if let Ok(a) = get(escrow)
                && a.owner == PROGRAM
                && a.data.len() == 73
                && a.data[..8] == disc("account:BondingFeeEscrow")
                && key_at(&a.data, 8) == self.coin.mint
                && key_at(&a.data, 40) == recipient
                && a.data[72]
                    == Pubkey::find_program_address(
                        &[
                            b"bonding_fee_escrow",
                            self.coin.mint.as_ref(),
                            recipient.as_ref(),
                        ],
                        &PROGRAM,
                    )
                    .1
            {
                remaining.push(escrow);
                accounts.insert(escrow, a);
            }
        }
        if self.coin.reserved[32] != 0 {
            ensure(self.coin.reserved[32] == 1, "Invalid creator revenue lock")?;
            let revenue = pda(&[b"creator_revenue", self.coin.mint.as_ref()]);
            let a = get(revenue)?;
            ensure(
                a.owner == PROGRAM
                    && a.data.len() == 414
                    && a.data[..8] == disc("account:CreatorRevenue")
                    && key_at(&a.data, 8) == self.coin.mint
                    && key_at(&a.data, 40) == self.coin.creator_fee_recipient,
                "Invalid creator revenue account",
            )?;
            remaining.push(revenue);
            accounts.insert(revenue, a);
        }
        remaining.sort();
        remaining.dedup();
        self.cached = Some(Cached {
            config,
            vault,
            rewards,
            rent,
            fees,
            accounts,
            remaining,
        });
        Ok(())
    }
    fn quote(&self, p: &QuoteParams) -> Result<Quote> {
        let buy = self.direction(p.input_mint, p.output_mint, p.swap_mode)?;
        let c = self.ready()?;
        let coin = &self.coin;
        let virtual_sol = coin
            .sol_listed
            .checked_add(coin.sol_reserves)
            .ok_or(AmmError::from("Reserve overflow"))?;
        let [platform, creator, rewards] = c.config.shares;
        let epoch = self.clock.epoch.load(Ordering::Relaxed);
        let (out, fee, credits, sol_delta, token_delta) = if buy {
            let q = math::buy_quote(
                p.amount,
                virtual_sol,
                coin.tokens_remaining_on_curve,
                coin.virtual_token_offset,
                c.config.fee,
                platform,
                creator,
                rewards,
                coin.charity_bps,
                coin.transfer_fee_bps,
            )
            .map_err(Self::math_error)?;
            ensure(
                q.tokens_received_by_buyer > 0
                    && c.fees.fee(q.tokens_out, epoch) == q.tokens_out - q.tokens_received_by_buyer,
                "Token fee mismatch or zero net delivery",
            )?;
            if coin.cumulative_token_volume == 0 && c.config.first_buy_cap > 0 {
                ensure(
                    u128::from(q.tokens_out)
                        <= u128::from(coin.total_supply) * u128::from(c.config.first_buy_cap)
                            / 10_000,
                    "First buy cap",
                )?;
            }
            admission::validate_buy_inventory(coin, c.vault, &q, &c.rent)?;
            ensure(
                coin.sol_reserves.checked_add(q.sol_to_curve).is_some(),
                "Reserve overflow",
            )?;
            (
                q.tokens_received_by_buyer,
                p.amount - q.sol_to_curve,
                [q.platform_fee, q.creator_fee, q.rewards_fee, q.charity_fee],
                p.amount,
                q.tokens_out,
            )
        } else {
            let q = math::sell_quote(
                p.amount,
                virtual_sol,
                coin.sol_reserves,
                coin.tokens_remaining_on_curve,
                coin.virtual_token_offset,
                c.config.fee,
                platform,
                creator,
                rewards,
                coin.charity_bps,
                coin.transfer_fee_bps,
            )
            .map_err(Self::math_error)?;
            ensure(
                c.fees.fee(p.amount, epoch) == p.amount - q.tokens_net,
                "Token fee mismatch",
            )?;
            ensure(
                c.vault.checked_add(q.tokens_net).is_some()
                    && coin
                        .tokens_remaining_on_curve
                        .checked_add(q.tokens_net)
                        .is_some(),
                "Token overflow",
            )?;
            (
                q.sol_to_seller,
                q.sol_out_raw - q.sol_to_seller,
                [q.platform_fee, q.creator_fee, q.rewards_fee, q.charity_fee],
                q.sol_out_raw,
                p.amount,
            )
        };
        ensure(
            coin.cumulative_sol_volume
                .checked_add(u128::from(sol_delta))
                .is_some()
                && coin
                    .cumulative_token_volume
                    .checked_add(u128::from(token_delta))
                    .is_some()
                && c.rewards.checked_add(credits[2]).is_some(),
            "Accounting overflow",
        )?;
        // Aggregate aliased fee recipients before checking final balances and escrow fallback.
        let creator_dest = if coin.reserved[32] == 1 {
            pda(&[b"creator_revenue", coin.mint.as_ref()])
        } else {
            coin.creator_fee_recipient
        };
        let mut credit_map = HashMap::<Pubkey, u64>::new();
        for (key, amount) in [
            (c.config.treasury, credits[0]),
            (creator_dest, credits[1]),
            (coin.charity_wallet, credits[3]),
        ] {
            if key == coin.charity_wallet && coin.charity_bps == 0 && amount == 0 {
                continue;
            }
            let prev = *credit_map.get(&key).unwrap_or(&0);
            credit_map.insert(
                key,
                prev.checked_add(amount)
                    .ok_or(AmmError::from("Fee overflow"))?,
            );
        }
        for (key, amount) in credit_map {
            let direct = c.accounts.get(&key).is_some_and(|a| {
                (key == creator_dest && coin.reserved[32] == 1
                    || safe_fee_destination(key, a, self.coin.mint))
                    && a.lamports
                        .checked_add(amount)
                        .is_some_and(|v| amount == 0 || v >= c.rent.minimum_balance(a.data.len()))
            });
            if !direct {
                let escrow = pda(&[b"bonding_fee_escrow", coin.mint.as_ref(), key.as_ref()]);
                ensure(
                    c.accounts
                        .get(&escrow)
                        .is_some_and(|a| a.lamports.checked_add(amount).is_some()),
                    "Fee recipient needs initialized escrow or refreshed account",
                )?;
            }
        }
        Ok(Quote {
            in_amount: p.amount,
            out_amount: out,
            fee_amount: fee,
            fee_mint: WSOL,
            fee_pct: Decimal::from(c.config.fee) / Decimal::from(10_000),
        })
    }
    fn get_swap_and_account_metas(&self, p: &SwapParams) -> Result<SwapAndAccountMetas> {
        let buy = self.direction(p.source_mint, p.destination_mint, p.swap_mode)?;
        let c = self.ready()?;
        let trader = p.token_transfer_authority;
        ensure(
            trader != c.config.treasury
                && (self.coin.reserved[32] != 0 || trader != self.coin.creator_fee_recipient)
                && (self.coin.charity_bps == 0 || trader != self.coin.charity_wallet),
            "Trader aliases a fee recipient; native deltas would not equal generic quote",
        )?;
        let token_account = if buy {
            p.destination_token_account
        } else {
            p.source_token_account
        };
        ensure(
            token_account == ata(&self.coin.mint, &trader),
            "Native curve requires trader-owned associated token account; arbitrary Jupiter intermediate accounts are unsupported",
        )?;
        let mut accounts = vec![
            AccountMeta::new(trader, true),
            AccountMeta::new_readonly(pda(&[b"config"]), false),
            AccountMeta::new(self.key, false),
            AccountMeta::new_readonly(self.coin.mint, false),
            AccountMeta::new(ata(&self.coin.mint, &self.key), false),
            AccountMeta::new(token_account, false),
            AccountMeta::new(c.config.treasury, false),
            AccountMeta::new(self.coin.creator_fee_recipient, false),
            AccountMeta::new(pda(&[b"rewards_pool_v2"]), false),
            AccountMeta::new(
                if self.coin.charity_bps == 0 {
                    trader
                } else {
                    self.coin.charity_wallet
                },
                false,
            ),
            AccountMeta::new_readonly(TOKEN, false),
            AccountMeta::new_readonly(ATA, false),
            AccountMeta::new_readonly(SYSTEM, false),
        ];
        accounts.extend(c.remaining.iter().map(|x| AccountMeta::new(*x, false)));
        Ok(SwapAndAccountMetas {
            swap: Swap::Placeholder {
                data: Some(vec![u8::from(buy)]),
            },
            account_metas: accounts,
        })
    }
    fn get_accounts_len(&self) -> usize {
        13 + self.cached.as_ref().map_or(4, |c| c.remaining.len())
    }
    fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
        vec![
            (TOKEN, "spl_token_2022".into()),
            (ATA, "spl_associated_token_account".into()),
        ]
    }
    fn is_active(&self) -> bool {
        self.ready().is_ok()
    }
}
impl SingleProgramAmm for LimitlessAmm {
    const PROGRAM_ID: Pubkey = PROGRAM;
    const LABEL: AmmLabel = "LIMITLESS";
}
pub fn encode_swap(swap: &Swap, amount: u64, minimum: u64) -> Result<Vec<u8>> {
    let buy = match swap {
        Swap::Placeholder { data: Some(d) } if d == &[1] => true,
        Swap::Placeholder { data: Some(d) } if d == &[0] => false,
        _ => return Err("Unknown swap tag".into()),
    };
    let mut data = disc(if buy { "global:buy" } else { "global:sell" }).to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&minimum.to_le_bytes());
    Ok(data)
}

// Port of live valid_launch_fee_destination for supported system/SPL accounts.
fn safe_fee_destination(key: Pubkey, a: &Account, mint: Pubkey) -> bool {
    if a.executable || key == mint {
        return false;
    }
    if a.owner == SYSTEM {
        return key.is_on_curve();
    }
    let legacy = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    if a.owner != TOKEN && a.owner != legacy {
        return false;
    }
    let d = &a.data;
    let is_mint = d.len() == 82 || d.len() > 165 && d[165] == 1;
    if is_mint {
        return a.owner == TOKEN
            && d.len() >= 82
            && d[..4] == 1u32.to_le_bytes()
            && d[45] == 1
            && key_at(d, 4).is_on_curve();
    }
    let is_token = d.len() == 165 || d.len() > 165 && d[165] == 2;
    is_token && d.len() >= 165 && matches!(d[108], 1 | 2) && key_at(d, 32).is_on_curve()
}
