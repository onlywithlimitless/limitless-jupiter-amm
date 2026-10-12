BLOCKED: native adapter and parity suite are complete; Jupiter routing still needs native SOL/WSOL orchestration and an integrated swap variant.

Standalone local repository: `/home/ubuntu/limitless-jupiter-amm`. Application packet: `APPLICATION.md` in this job folder and the repository. Nothing was published or submitted.

Implemented the upstream Jupiter Amm trait with exact current buy/sell math, 1.25% captured bonding fee, actual Token-2022 epoch/cap checks, completion/graduation refusal, dynamic fee escrow and creator-revenue accounts, and source-derived graduation admission. No network calls exist in the adapter. ExactOut and old 129-byte curve records are rejected.

Verified with networking disabled:

- 76 exact input/output parity executions on four real mainnet curve snapshots, using the actual deployed SBF. Includes four completing buys and eight successful aggregator-CPI buys/sells. Subsequent quotes refuse both directions after completion.
- 23 separately labelled synthetic cases: 50/100-bps Token-2022 fees, cap mismatch rejection by both quote and program, completion states and creator-revenue locking.
- Six Rust adapter tests and strict Clippy pass. Native instruction discriminators/account privileges match the retained IDL.
- Native SOL/WSOL incompatibility is explicitly reproduced: a funded WSOL account remains at 500,000,000 units across native CPI buy and sell. The curve instead debits/credits trader lamports and requires the trader's canonical Token-2022 ATA. No top-level-only restriction blocks CPI; Jupiter needs an appropriate native-SOL route implementation. The current Swap::Placeholder cannot execute on Jupiter.

Fresh read-only chain evidence supersedes the stale source pointer: actual deployment slot 455611295, source release `4cb811f3251fa421e852d1da62254a8f2e1b2c4d`, deployed ELF SHA-256 `cc10058317ec65cf09f5a021a05c9a1901a5b42be49732c860b54c6c3e8a9b9d`. `/etc/limitless/mainnet-launchpad-live-commit` says `ecc8f7e`; the October 3 source archive says `817cf0a`. Neither was modified. The math is unchanged, but the actual release adds creator-revenue remaining accounts, which this adapter includes.

Application: https://support.jup.ag/requests/new/amm-integrators, selection **Bonding Curve**. Packet contains the field answers, program ID, IDL, source/test receipts, internal AIRTIGHT history and a plain statement that no external audit report is available. Entity: Chapter 28 Global Ltd. Public API measurement at 2026-10-12 10:15:03 UTC: 11 publicly listed live coins and 1.082534637 SOL indexed seven-day BUY/SELL volume, including test activity. The API total of 238 records is not represented as 238 live coins.

Orchestrator still must provide the public/forkable URL and explicit fork/license permission, fill the authorized reply email and Telegram contact, and disclose the routing limitation before submission. No support issue was sent; an issue draft is included for the test-kit limitation.

Founder gate exited 0: 85 features, zero unmeasurable, four existing exceptions retained (first-buy-percentage-choices, lmtless-token-page, sidebar-buy-claim, wishes-reward-float). No approved source, copy, sound, asset, style or flow was changed. Protected-removal guard result is recorded in `protected-removal-guard.log` after the local commit.

No mainnet transactions, deployments, production changes, public repository creation or form submission occurred. RPC was read-only; test signatures and lamports existed only inside LiteSVM.
