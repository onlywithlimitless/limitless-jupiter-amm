use base64::{Engine, engine::general_purpose::STANDARD};
use jupiter_amm_interface::*;
use limitless_jupiter_amm::*;
use solana_account::Account;
use solana_pubkey::Pubkey;
use std::{collections::HashMap, str::FromStr, sync::Arc};
fn fixture() -> (LimitlessAmm, HashMap<Pubkey, Arc<Account>>) {
    let v: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/Atb9wApJzvG9KbFH1UExCzvXfffxGtLgZEmA7RKeLLsU.json"
    ))
    .unwrap();
    let mut map = HashMap::new();
    for (key, a) in v["accounts"].as_object().unwrap() {
        if a.is_null() {
            continue;
        }
        map.insert(
            Pubkey::from_str(key).unwrap(),
            Arc::new(Account {
                lamports: a["lamports"].as_u64().unwrap(),
                data: STANDARD.decode(a["data"][0].as_str().unwrap()).unwrap(),
                owner: Pubkey::from_str(a["owner"].as_str().unwrap()).unwrap(),
                executable: a["executable"].as_bool().unwrap(),
                rent_epoch: 0,
            }),
        );
    }
    let key = Pubkey::from_str(v["pool"].as_str().unwrap()).unwrap();
    let amm = LimitlessAmm::from_keyed_account(
        &KeyedAccount {
            key,
            account: map[&key].as_ref().clone(),
            params: None,
        },
        &AmmContext {
            clock_ref: ClockRef::default(),
        },
    )
    .unwrap();
    (amm, map)
}
fn params(amm: &LimitlessAmm) -> QuoteParams {
    QuoteParams {
        amount: 100_000_000,
        input_mint: WSOL,
        output_mint: amm.get_reserve_mints()[1],
        swap_mode: SwapMode::ExactIn,
        fee_mode: FeeMode::Normal,
    }
}
#[test]
fn no_quote_before_update_and_failed_refresh_invalidates_cache() {
    let (mut amm, map) = fixture();
    let p = params(&amm);
    assert!(amm.quote(&p).is_err());
    amm.update(&map).unwrap();
    assert!(amm.quote(&p).is_ok());
    assert!(amm.update(HashMap::<Pubkey, Arc<Account>>::new()).is_err());
    assert!(amm.quote(&p).is_err());
    assert!(!amm.is_active());
}
#[test]
fn refuses_wrong_pairs_exact_out_zero_and_overflow() {
    let (mut amm, map) = fixture();
    amm.update(&map).unwrap();
    let mut p = params(&amm);
    p.swap_mode = SwapMode::ExactOut;
    assert!(amm.quote(&p).is_err());
    p.swap_mode = SwapMode::ExactIn;
    p.output_mint = WSOL;
    assert!(amm.quote(&p).is_err());
    p = params(&amm);
    p.amount = 0;
    assert!(amm.quote(&p).is_err());
    p.amount = u64::MAX;
    assert!(amm.quote(&p).is_err());
}
#[test]
fn complete_graduated_prepared_threshold_and_underfunded_states_refuse() {
    for offset in [182, 299, 268] {
        let (mut amm, mut map) = fixture();
        let p = params(&amm);
        let mut a = map[&amm.key()].as_ref().clone();
        a.data[offset] = 1;
        a.data[267] = 2;
        map.insert(amm.key(), Arc::new(a));
        amm.update(&map).unwrap();
        assert!(amm.quote(&p).is_err());
        assert!(!amm.is_active());
    }
    let (mut amm, mut map) = fixture();
    let mut a = map[&amm.key()].as_ref().clone();
    a.data[215..223].copy_from_slice(&1u64.to_le_bytes());
    map.insert(amm.key(), Arc::new(a));
    amm.update(&map).unwrap();
    assert!(!amm.is_active());
    let mut a = map[&amm.key()].as_ref().clone();
    a.lamports = 1;
    map.insert(amm.key(), Arc::new(a));
    assert!(amm.update(&map).is_err());
}
#[test]
fn owner_discriminator_and_pda_checks_fail_closed() {
    let (amm, map) = fixture();
    for kind in 0..5 {
        let mut a = map[&amm.key()].as_ref().clone();
        let mut key = amm.key();
        match kind {
            0 => a.owner = SYSTEM,
            1 => a.data[0] ^= 1,
            2 => a.data.truncate(129),
            3 => key = SYSTEM,
            _ => a.data[126..134].copy_from_slice(&0u64.to_le_bytes()),
        }
        assert!(
            LimitlessAmm::from_keyed_account(
                &KeyedAccount {
                    key,
                    account: a,
                    params: None
                },
                &AmmContext {
                    clock_ref: ClockRef::default()
                }
            )
            .is_err()
        );
    }
}
#[test]
fn transfer_fees_select_epoch_and_round_up_with_caps() {
    let (mut amm, map) = fixture();
    let mint = amm.get_reserve_mints()[1];
    let mut data = map[&mint].data.clone();
    let mut i = 166;
    while i + 4 < data.len() {
        let kind = state::u16_at(&data, i);
        let len = state::u16_at(&data, i + 2) as usize;
        i += 4;
        if kind == 1 {
            data[i + 72..i + 80].copy_from_slice(&0u64.to_le_bytes());
            data[i + 80..i + 88].copy_from_slice(&u64::MAX.to_le_bytes());
            data[i + 88..i + 90].copy_from_slice(&50u16.to_le_bytes());
            data[i + 90..i + 98].copy_from_slice(&10u64.to_le_bytes());
            data[i + 98..i + 106].copy_from_slice(&7u64.to_le_bytes());
            data[i + 106..i + 108].copy_from_slice(&100u16.to_le_bytes());
            break;
        }
        i += len;
    }
    let fees = state::TransferFees::decode(&data, 6).unwrap();
    assert_eq!(fees.fee(1, 9), 1);
    assert_eq!(fees.fee(1000, 9), 5);
    assert_eq!(fees.fee(1000, 10), 7);
    amm.update(&map).unwrap();
}
#[test]
fn clone_preserves_quote_without_network() {
    let (mut amm, map) = fixture();
    amm.update(&map).unwrap();
    let p = params(&amm);
    assert_eq!(
        amm.quote(&p).unwrap().out_amount,
        amm.clone().quote(&p).unwrap().out_amount
    );
    assert!(amm.has_dynamic_accounts());
    assert!(!amm.supports_exact_out());
    assert!(!amm.unidirectional());
}
