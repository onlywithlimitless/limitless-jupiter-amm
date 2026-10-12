#!/usr/bin/env python3
"""Prove native CPI execution is not a Jupiter WSOL token-account swap."""
from parity import *
from solders.system_program import transfer,TransferParams
f=json.loads((ROOT/'tests/fixtures/Atb9wApJzvG9KbFH1UExCzvXfffxGtLgZEmA7RKeLLsU.json').read_text());h=Harness(f)
legacy=Pubkey.from_string('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA');mint=Pubkey.from_string('So11111111111111111111111111111111111111112')
a=json.loads((ROOT/'tests/fixtures/wsol-mint.json').read_text())['value'];h.vm.set_account(mint,Account(a['lamports'],raw(a),Pubkey.from_string(a['owner'])))
wsol=Pubkey.find_program_address([bytes(WALLET.pubkey()),bytes(legacy),bytes(mint)],ATA)[0]
ix=Instruction(ATA,bytes([1]),[AccountMeta(PAYER.pubkey(),True,True),AccountMeta(wsol,False,True),AccountMeta(WALLET.pubkey(),False,False),AccountMeta(mint,False,False),AccountMeta(SYSTEM,False,False),AccountMeta(legacy,False,False)])
ok(send(h.vm,ix));ok(send(h.vm,transfer(TransferParams(from_pubkey=PAYER.pubkey(),to_pubkey=wsol,lamports=500_000_000))))
ok(send(h.vm,Instruction(legacy,bytes([17]),[AccountMeta(wsol,False,True)])))
before=struct.unpack_from('<Q',h.vm.get_account(wsol).data,64)[0]
b=h.trade(True,100_000_000,True);after_buy=struct.unpack_from('<Q',h.vm.get_account(wsol).data,64)[0]
s=h.trade(False,b['out']//2,True);after_sell=struct.unpack_from('<Q',h.vm.get_account(wsol).data,64)[0]
assert before==after_buy==after_sell
result={'result':'CONFIRMED_ROUTING_BLOCKER','native_cpi_buy':b,'native_cpi_sell':s,'wsol_before':before,'wsol_after_buy':after_buy,'wsol_after_sell':after_sell,'explanation':'Native trader lamports change exactly as quoted but WSOL token balance never changes. A Jupiter-side unwrap/wrap path or separately reviewed adapter is required; no such on-chain adapter is deployed by this work.'}
(ROOT/'evidence/wsol-blocker.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))
