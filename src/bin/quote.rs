//! Offline fixture adapter used by the independent Python LiteSVM executor.
use base64::{Engine, engine::general_purpose::STANDARD};
use jupiter_amm_interface::*;
use limitless_jupiter_amm::*;
use serde_json::{Value, json};
use solana_account::Account;
use solana_pubkey::Pubkey;
use std::{
    collections::HashMap,
    io::{self, BufRead},
    str::FromStr,
    sync::Arc,
};
fn run(v: Value) -> std::result::Result<Value, Box<dyn std::error::Error>> {
    let fixture = &v["fixture"];
    let pool = Pubkey::from_str(fixture["pool"].as_str().ok_or("pool")?)?;
    let mut accounts = HashMap::<Pubkey, Arc<Account>>::new();
    for (key, a) in fixture["accounts"].as_object().ok_or("accounts")? {
        if a.is_null() {
            continue;
        }
        accounts.insert(
            Pubkey::from_str(key)?,
            Arc::new(Account {
                lamports: a["lamports"].as_u64().ok_or("lamports")?,
                data: STANDARD.decode(a["data"][0].as_str().ok_or("data")?)?,
                owner: Pubkey::from_str(a["owner"].as_str().ok_or("owner")?)?,
                executable: a["executable"].as_bool().unwrap_or(false),
                rent_epoch: 0,
            }),
        );
    }
    let context = AmmContext {
        clock_ref: ClockRef::default(),
    };
    context.clock_ref.epoch.store(
        v["epoch"].as_u64().unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );
    let mut amm = LimitlessAmm::from_keyed_account(
        &KeyedAccount {
            key: pool,
            account: (**accounts.get(&pool).ok_or("curve")?).clone(),
            params: None,
        },
        &context,
    )?;
    amm.update(&accounts)?;
    let buy = v["buy"].as_bool().ok_or("buy")?;
    let mint = amm.get_reserve_mints()[1];
    let (input, output) = if buy { (WSOL, mint) } else { (mint, WSOL) };
    let amount = v["amount"].as_u64().ok_or("amount")?;
    let quote = amm.quote(&QuoteParams {
        amount,
        input_mint: input,
        output_mint: output,
        swap_mode: SwapMode::ExactIn,
        fee_mode: FeeMode::Normal,
    })?;
    let wallet = Pubkey::from_str(v["wallet"].as_str().ok_or("wallet")?)?;
    let user_token = ata(&mint, &wallet);
    let params = SwapParams {
        swap_mode: SwapMode::ExactIn,
        in_amount: amount,
        out_amount: quote.out_amount,
        source_mint: input,
        destination_mint: output,
        source_token_account: if buy { wallet } else { user_token },
        destination_token_account: if buy { user_token } else { wallet },
        token_transfer_authority: wallet,
        user: wallet,
        payer: wallet,
        quote_mint_to_referrer: None,
        jupiter_program_id: &SYSTEM,
        missing_dynamic_accounts_as_default: false,
    };
    let ix = amm.native_instruction(&params)?;
    Ok(
        json!({"in_amount":quote.in_amount,"out_amount":quote.out_amount,"fee_amount":quote.fee_amount,"data":STANDARD.encode(ix.data),"metas":ix.accounts.iter().map(|a|json!({"key":a.pubkey.to_string(),"signer":a.is_signer,"writable":a.is_writable})).collect::<Vec<_>>()}),
    )
}
fn main() {
    for line in io::stdin().lock().lines() {
        let answer = match line
            .map_err(|e| e.to_string())
            .and_then(|l| serde_json::from_str(&l).map_err(|e| e.to_string()))
            .and_then(|v| run(v).map_err(|e| e.to_string()))
        {
            Ok(v) => v,
            Err(e) => json!({"error":e}),
        };
        println!("{answer}");
    }
}
