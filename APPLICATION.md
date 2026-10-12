# Jupiter AMM integration application — LIMITLESS

**Prepared 2026-10-12. Not submitted. Repository remains local and private.**

The Rust adapter and native execution parity suite are complete for the current 344-byte curve layout. Request Jupiter review of the native-SOL routing requirements below before enabling the venue. A public/forkable repository URL and an authorized email/Telegram contact must be supplied by the orchestrator before submission.

## Destination and exact form answers

Form: **https://support.jup.ag/requests/new/amm-integrators**

Verified against the live form and its client on 2026-10-12. The relevant selection is **Bonding Curve**, underlying value `bonding_curve`.

| Form field | Answer |
|---|---|
| Email address | **REQUIRED — orchestrator to supply the authorized team's reply address. No verified submission contact was provided.** |
| Project / Company Name | **LIMITLESS / Chapter 28 Global Ltd** |
| Telegram username | **REQUIRED — orchestrator to supply the authorized maintainer's Telegram username.** |
| Integration Type | **Bonding Curve** (`bonding_curve`) |
| Subject | **LIMITLESS bonding curve — native CPI parity verified; request Metis integration review** |
| Description | Paste the description below after replacing the repository/contact placeholders. |

### Description to paste

LIMITLESS is a Solana Token-2022 launchpad operated by Chapter 28 Global Ltd. We request integration of our bonding-curve venue into Jupiter Metis.

Program ID: `LimitQGBbcHeGJZd3FbQEejiCRuk1rfvwhbv8XCtAoN`.
Website: https://limitless.buzz
SDK repository: **[ORCHESTRATOR: insert published/forkable repository URL]**.
IDL: **[same repository]/idl/launchpad_program.json**.

Specific features: ExactIn constant-product virtual-reserve curves; 1.25% current SOL bonding fee; immutable per-mint Token-2022 transfer fees; creator/treasury/rewards/optional charity allocations; creator-revenue custody; automatic refusal once the curve is complete, prepared or graduated. The quote math resembles the virtual-reserve approach used by other launchpad curves, but our ABI, fees, graduation admission and account requirements are specific to LIMITLESS.

Top sampled curve by real SOL reserves: `Atb9wApJzvG9KbFH1UExCzvXfffxGtLgZEmA7RKeLLsU`; token mint `C75S1CEGznahXZDMhizQfTKQPcETbVj9DkgAk3dxoK94` (LTEST). The curve held 0.034522727 SOL of real reserves when sampled. This is small, test-heavy traction; virtual reserves are not deposited liquidity.

Validation: 76 exact input/output parity executions across four real mainnet curve snapshots using the deployed SBF in LiteSVM, including eight CPI swaps and four curve-completing trades. Both directions reject after completion. A separate 23-case synthetic suite covers 50/100-bps Token-2022 fees, capped-fee divergence, frozen curve lifecycle states and creator-revenue locking. Six Rust adapter tests cover account/cache validation and quote refusal. Fixtures, SBF hashes and outputs are committed with the SDK. Synthetic fee variants are not claimed to be taxable mainnet markets.

Integration consideration: native buys/sells pass CPI and have no top-level-only caller restriction. They debit/credit signer lamports, however, and require that signer's canonical Token-2022 ATA. Our explicit WSOL test proves WSOL balances do not change during those native swaps. Jupiter needs a native-SOL unwrap/wrap implementation and compatible account routing; the SDK currently uses upstream `Swap::Placeholder`, not an integrated Jupiter swap variant. We use direct LiteSVM because the public test kit's single-instruction token-account model does not express this native-SOL settlement. An issue draft is included; no issue was submitted.

Security: we have internal AIRTIGHT source reviews, regression tests, adversarial SBF tests and reproducible-build evidence. **There is no external audit report available for this application; no external audit is claimed for the program or this adapter.** Internal automated/AI-assisted reviews are not an external security audit.

Traction from api.limitless.buzz at 2026-10-12 10:15:03 UTC: 11 publicly listed live coins, 1.082534637 SOL indexed BUY/SELL volume across 36 trades during the preceding seven days. The API's total record count is 238 and includes records outside the public live list; it is not our live-coin count. Activity includes test launches/trades. No backer or funding claim is made.

Team/entity: Chapter 28 Global Ltd; founder-led LIMITLESS team. **[ORCHESTRATOR: insert authorized maintainer identity/contact and any verifiable team links.]** We can provide the internal review and reproducible build materials, but request guidance on external-audit requirements and the native-SOL integration path.

## Evidence and provenance for the orchestrator

- [Parity output](evidence/parity.log), [all 76 executions](evidence/parity.json), [synthetic cases](evidence/edge-cases.json), [Rust tests](evidence/cargo-test.log).
- [Confirmed native-SOL / WSOL blocker](evidence/wsol-blocker.json): a funded WSOL ATA stays at 500,000,000 units across native CPI buy and sell, while wallet lamports change exactly as quoted.
- [Fresh chain capture](evidence/snapshot.json): deployment slot **455611295**, captured at finalized slot **456007933**. ELF SHA-256 **`cc10058317ec65cf09f5a021a05c9a1901a5b42be49732c860b54c6c3e8a9b9d`**, **1,646,928 bytes**.
- [Source verification](evidence/source-verification.json): actual deployed release binds to source commit **`4cb811f3251fa421e852d1da62254a8f2e1b2c4d`**. The requested October 3 archive records `817cf0a`; `/etc/limitless/mainnet-launchpad-live-commit` still says `ecc8f7e`. Neither marker was changed. The actual deployed binary matches the retained Upgrade C release receipt exactly. This task did not rebuild the launchpad itself.
- [IDL](idl/launchpad_program.json): retained generated Upgrade C IDL, 43 instructions, matching program address. Native buy/sell instruction schemas are independently checked by the parity suite against this IDL.
- [Traction calculation](evidence/traction.json), [capture script](scripts/traction.py): paginate public token/transaction GETs, deduplicate trade signatures, filter the exact UTC seven-day window, sum decimal `amountSol`. No extrapolation from 24h volume. Public list: 33 records, 22 pending creation, 11 live. Program-account capture separately found 31 current-layout curves and 80 old 129-byte records; those are not substituted for API live status.
- [Jupiter requirements](https://developers.jup.ag/docs/swap/routing/amm/integration), [upstream interface/test kit](https://github.com/jup-ag/jupiter-amm-interface). The interface Git revision is pinned in Cargo.toml and Cargo.lock.

Internal review history retained locally:

1. `AIRTIGHT-AUDIT/release-1003f`: October 3 deterministic builds, lifecycle, fee-destination and transaction regression work for the earlier release.
2. `AIRTIGHT-AUDIT/upgrade-c-plan-1010` and `upgrade-c-build-1010`: preservation, native swap/lifecycle, race and creator-revenue work.
3. `AIRTIGHT-AUDIT/upgrade-c-fix-1011`: payout review corrections, release/recovery reproducible builds and adversarial tests. Its report records 429 native tests and historical parity checks; these are historical report claims, not all rerun by this task.
4. This repository: fresh mainnet binary/account capture, quote parity, CPI execution and adapter validation; outputs are directly reproduced here.

Before submission the orchestrator must publish privately/publicly as authorized, provide Jupiter access and explicit fork/licensing permission, fill the reply contact fields, and disclose the native-SOL routing limitation and absent external audit. No public repository, support ticket, form, mainnet transaction, deployment or production configuration change was created by this task.
