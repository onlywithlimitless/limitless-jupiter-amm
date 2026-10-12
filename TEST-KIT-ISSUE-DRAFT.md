# Unsubmitted issue draft: native SOL settlement in a bonding-curve parity suite

The LIMITLESS curve's native buy/sell instructions move lamports from/to a signing system account and Token-2022 tokens through the signer's ATA. They execute successfully both top-level and through CPI. Jupiter identifies the SOL side using the WSOL mint, but the curve instruction does not consume or credit WSOL token accounts.

The current test kit drives one native instruction and measures token-account deltas. That cannot represent the curve's native SOL input/output and associated unwrap/wrap orchestration. Our standalone LiteSVM suite isolates network fees/account rent to a separate payer and measures the native SOL and Token-2022 deltas exactly, using the Rust Amm implementation to produce quotes and instructions. An additional test shows the funded WSOL balance stays unchanged across successful native CPI swaps.

Could the test kit support a native-SOL balance mode with a separate fee payer, or explicit setup/settlement instructions and delta accounting? Which Jupiter integration path should a new native-SOL bonding curve target?

Evidence: evidence/parity.json, evidence/wsol-blocker.json; reproduction: README.md.

This draft has not been posted or sent.
