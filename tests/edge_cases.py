#!/usr/bin/env python3
"""Explicitly synthetic edge variants, separate from real-mainnet parity claims."""
from parity import *
def patch(f,key,offset,data):
 a=f['accounts'][key];b=bytearray(raw(a));b[offset:offset+len(data)]=data;a['data'][0]=base64.b64encode(b).decode()
def tax_variant(base,bps,maxfee=(1<<64)-1):
 f=copy.deepcopy(base);patch(f,f['pool'],107,struct.pack('<H',bps))
 d=raw(f['accounts'][f['mint']]);i=166
 while i+4<=len(d):
  kind,size=struct.unpack_from('<HH',d,i);i+=4
  if kind==1:
   for offset in [72,90]:patch(f,f['mint'],i+offset,struct.pack('<QQH',0,maxfee,bps))
   break
  i+=size
 return f
def main():
 base=json.loads((ROOT/'tests/fixtures/Atb9wApJzvG9KbFH1UExCzvXfffxGtLgZEmA7RKeLLsU.json').read_text());rows=[]
 for bps in [50,100]:
  f=tax_variant(base,bps)
  for amount in [1_000,1_000_000,100_000_000,1_000_000_000]:
   h=Harness(f);b=h.trade(True,amount);rows.append({'synthetic_transfer_fee_bps':bps,**b});rows.append({'synthetic_transfer_fee_bps':bps,**h.trade(False,b['out']//2)})
  # The actual fee cap diverges from the live program's immutable bps-only math.
  # Both quote and program must fail closed rather than return a capped quote.
  capped=tax_variant(base,bps,1);assert 'error' in quote(capped,True,100_000_000)
  h=Harness(capped);q=quote(f,True,100_000_000);ix=Instruction(PROGRAM,base64.b64decode(q['data']),[AccountMeta(Pubkey.from_string(a['key']),a['signer'],a['writable']) for a in q['metas']]);result=send(h.vm,ix,[WALLET]);assert isinstance(result,FailedTransactionMetadata);assert 'TokenSettlementMismatch' in str(result)
  rows.append({'synthetic_transfer_fee_bps':bps,'capped_mismatch_rejected_by_quote_and_program':True})
 for name,offset,value in [('graduated',182,1),('complete',299,1),('prepared',268,1)]:
  f=copy.deepcopy(base);patch(f,f['pool'],offset,bytes([value]));
  if name=='prepared':patch(f,f['pool'],267,bytes([2]))
  for buy in [True,False]:assert 'error' in quote(f,buy,100_000_000)
  rows.append({'refused_state':name})
 # Revenue lock: synthesize only the newly required custody account from the source layout.
 f=copy.deepcopy(base);patch(f,f['pool'],300,bytes([1]));coin=raw(f['accounts'][f['pool']]);key=str(Pubkey.find_program_address([b'creator_revenue',bytes(Pubkey.from_string(f['mint']))],PROGRAM)[0]);rev=hashlib.sha256(b'account:CreatorRevenue').digest()[:8]+coin[8:40]+coin[72:104]+bytes([1])+coin[72:104]+struct.pack('<H',10000)+bytes(9*34)+bytes([Pubkey.find_program_address([b'creator_revenue',bytes(Pubkey.from_string(f['mint']))],PROGRAM)[1]])
 f['accounts'][key]={'lamports':10_000_000,'data':[base64.b64encode(rev).decode(),'base64'],'owner':str(PROGRAM),'executable':False}
 h=Harness(f);b=h.trade(True,100_000_000);rows.append({'synthetic_revenue_lock':True,**b});rows.append({'synthetic_revenue_lock':True,**h.trade(False,b['out']//2)})
 (ROOT/'evidence/edge-cases.json').write_text(json.dumps({'result':'PASS','scope':'Synthetic variants of mainnet fixtures, executed with deployed SBF','cases':rows},indent=2));print('PASS',len(rows),'edge cases, including 50/100 bps Token-2022 fees and creator revenue lock')
if __name__=='__main__':main()
