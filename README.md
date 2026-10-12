# LIMITLESS Jupiter AMM adapter

Local integration candidate for the LIMITLESS Token-2022 bonding curve. **Not yet a routable Jupiter venue.** Native instruction parity and CPI execution pass; Jupiter must supply native SOL/WSOL orchestration and register the swap before routing it.

Program: `LimitQGBbcHeGJZd3FbQEejiCRuk1rfvwhbv8XCtAoN`.

The library implements the upstream `Amm` trait, including cached account decoding, ExactIn buy/sell quotes, dynamic account discovery, native instruction metadata, program dependencies, cloning and activity checks. It makes no network calls. The only RPC code is in explicitly invoked capture scripts; those permit read methods only.

## Reproduce offline

Requirements: Rust stable (tested 1.99.0), Python 3, `solders==0.28.0`. Install Rust/Python dependencies once before entering an offline environment. Cargo.lock pins the Rust graph; the Jupiter interface is pinned by Git commit because the published 0.6.1 package fails with its unconstrained current decoder dependency.

```sh
cargo build --locked --offline --bin quote
cargo test --locked --offline
python3 tests/parity.py
python3 tests/edge_cases.py
python3 tests/wsol_blocker.py
cargo clippy --locked --offline --all-targets -- -D warnings
```

`tests/parity.py` runs the compiled Rust `Amm::quote` and its native instruction metadata against deployed SBF in LiteSVM. Four immutable mainnet account snapshots produce 76 exact input/output assertions, including 8 CPI swaps and 4 curve-completing buys. Transaction fees and account creation rent are isolated to a separate synthetic payer. Every sell uses genuinely purchased tokens and refreshed state. No production keypair is loaded.

The tests use the deployed Token-2022 and associated-token program binaries, captured Clock and Rent, and the deployed launchpad binary. Test-only CPI relay source and its binary are included. To rebuild that relay with the installed Solana platform toolchain:

```sh
cargo +1.89.0-sbpf-solana-v1.54 build --locked \
  --manifest-path tests/cpi-relay/Cargo.toml --target sbpf-solana-solana --release
cp tests/cpi-relay/target/sbpf-solana-solana/release/cpi_relay.so tests/fixtures/cpi_relay.so
```

## Supported behavior

- Exact checked integer buy/sell math extracted from the deployed source, including ceiling reserve division, floored 125-bps bonding fees, rounded-up Token-2022 withholding, and the one-lamport sell reserve bound.
- Current Config fee and shares are read on every update; 1.25% is verified in captured Config, not hard-coded against future admin changes.
- Actual mint epoch schedules and maximum transfer fees are decoded in `update`. `quote` selects the schedule using Jupiter's shared clock. A mint fee differing from the live handler's snapshotted formula is rejected, matching the handler's settlement refusal.
- Graduated, prepared, permanently complete, threshold-complete and qualifying sold-out curves refuse both directions. A buy that completes an active curve remains valid.
- Pure graduation-inventory admission checks retain the source's legacy/v2 rules and Meteora rent budgeting. Heavy account/extension decoding stays in `update`; quote-dependent arithmetic is in the SDK math/admission modules.
- Current creator-revenue lock and existing fee escrow remaining accounts are retained. Uninitialized required escrows cause refusal; the adapter never creates accounts or performs RPC.
- Config/recipient changes can change the account list. Fetch `get_accounts_to_update`, call `update`, and fetch again until dependencies stabilize. A failed update clears the quote cache.

## Routing limitations requiring Jupiter review

The mint pair uses WSOL as the conventional SOL identifier, but native buys debit the signer's **lamports**, and sells credit lamports. The native instructions do not spend or credit a WSOL ATA. `get_swap_and_account_metas` returns upstream's `Swap::Placeholder` and native instruction account order; it is **not an executable Jupiter routing variant**. The test kit's single-instruction token-delta model cannot represent that native-SOL flow, so this repository uses a separate LiteSVM harness and includes an unsubmitted issue draft.

The curve requires the Token-2022 ATA owned by the signing trader. Arbitrary intermediate token accounts fail. Normal signers propagate through CPI successfully; there is no top-level-only or Jupiter-specific caller restriction in buy/sell. The integration must arrange a funded, system-owned native-SOL signer for buys and canonical ATAs, then settle WSOL if required by the route.

ExactOut is unsupported. Historical 129-byte accounts are rejected; the current program uses the 344-byte layout. A trader aliasing a fee recipient is rejected by metadata construction because self-payments change wallet deltas. User account funding/balance/frozen-state/slippage remain transaction-level checks. Quotes require a coherent account snapshot and clock; they are not guarantees against state changes before execution.

No public remote, form submission, mainnet transaction, deployment, or production change was made. See [APPLICATION.md](APPLICATION.md), [evidence/source-verification.json](evidence/source-verification.json), and [evidence/parity.json](evidence/parity.json).
