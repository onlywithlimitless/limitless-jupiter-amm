#!/usr/bin/env python3
"""Offline: execute deployed SBF against immutable mainnet account snapshots.
Only test users are synthesized; quotes come from the compiled Rust Amm trait.
"""
import base64,copy,hashlib,json,struct,subprocess,sys
from pathlib import Path
from solders.account import Account
from solders.clock import Clock
from solders.rent import Rent
from solders.compute_budget import set_compute_unit_limit
from solders.instruction import AccountMeta,Instruction
from solders.keypair import Keypair
from solders.litesvm import LiteSVM
from solders.pubkey import Pubkey
from solders.transaction import Transaction
from solders.transaction_metadata import FailedTransactionMetadata
ROOT=Path(__file__).resolve().parents[1]
ELF=(ROOT/'tests/fixtures/limitless.so').read_bytes()
assert hashlib.sha256(ELF).hexdigest()==json.loads((ROOT/'evidence/snapshot.json').read_text())['elf_sha256']
for dependency in json.loads((ROOT/'evidence/dependencies.json').read_text()):
 assert hashlib.sha256((ROOT/f"tests/fixtures/{dependency['label']}.so").read_bytes()).hexdigest()==dependency['sha256']
PROGRAM=Pubkey.from_string('LimitQGBbcHeGJZd3FbQEejiCRuk1rfvwhbv8XCtAoN')
TOKEN=Pubkey.from_string('TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb')
ATA=Pubkey.from_string('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL')
SYSTEM=Pubkey.default()
WALLET=Keypair.from_seed(bytes([193])*32) # local-only deterministic user, never an RPC signer
PAYER=Keypair.from_seed(bytes([194])*32) # isolate tx fees from trader deltas
CLOCK='SysvarC1ock11111111111111111111111111111111'
BINARY=ROOT/'target/debug/quote'
def raw(a):return base64.b64decode(a['data'][0])
def ata(mint,owner):return Pubkey.find_program_address([bytes(owner),bytes(TOKEN),bytes(mint)],ATA)[0]
def quote(fixture,buy,amount):
 epoch=struct.unpack_from('<Q',raw(fixture['accounts'][CLOCK]),16)[0]
 req={'fixture':fixture,'buy':buy,'amount':amount,'epoch':epoch,'wallet':str(WALLET.pubkey())}
 proc=subprocess.run([str(BINARY)],input=json.dumps(req)+'\n',text=True,capture_output=True,check=True)
 return json.loads(proc.stdout)
def send(vm,ix,extra=()):
 vm.expire_blockhash()
 tx=Transaction.new_signed_with_payer([set_compute_unit_limit(1_400_000),ix],PAYER.pubkey(),[PAYER,*extra],vm.latest_blockhash())
 return vm.send_transaction(tx)
def ok(result):
 if isinstance(result,FailedTransactionMetadata):raise AssertionError(str(result))
 return result
class Harness:
 def __init__(self,fixture):
  self.fixture=copy.deepcopy(fixture);self.vm=LiteSVM().with_default_programs()
  self.vm.add_program(PROGRAM,ELF)
  for label,key in [('spl_token_2022',TOKEN),('spl_associated_token_account',ATA)]:
   path=ROOT/f'tests/fixtures/{label}.so'
   if path.exists():self.vm.add_program(key,path.read_bytes())
  for key,a in fixture['accounts'].items():
   if a is not None and key not in [str(SYSTEM),CLOCK,'SysvarRent111111111111111111111111111111111']:
    self.vm.set_account(Pubkey.from_string(key),Account(a['lamports'],raw(a),Pubkey.from_string(a['owner']),a['executable']))
  self.vm.set_rent(Rent.from_bytes(raw(fixture['accounts']['SysvarRent111111111111111111111111111111111'])))
  self.vm.set_clock(Clock.from_bytes(raw(fixture['accounts'][CLOCK])))
  ok(self.vm.airdrop(WALLET.pubkey(),1_000_000_000_000));ok(self.vm.airdrop(PAYER.pubkey(),1_000_000_000))
  self.mint=Pubkey.from_string(fixture['mint']);self.token=ata(self.mint,WALLET.pubkey())
  ix=Instruction(ATA,bytes([1]),[AccountMeta(PAYER.pubkey(),True,True),AccountMeta(self.token,False,True),AccountMeta(WALLET.pubkey(),False,False),AccountMeta(self.mint,False,False),AccountMeta(SYSTEM,False,False),AccountMeta(TOKEN,False,False)])
  ok(send(self.vm,ix))
 def current(self):
  f=copy.deepcopy(self.fixture)
  for key in f['accounts']:
   a=self.vm.get_account(Pubkey.from_string(key))
   if a is not None:f['accounts'][key]={'lamports':a.lamports,'data':[base64.b64encode(a.data).decode(),'base64'],'owner':str(a.owner),'executable':a.executable}
  return f
 def trade(self,buy,amount,cpi=False):
  f=self.current();q=quote(f,buy,amount)
  if 'error' in q:raise AssertionError(f'quote {buy=} {amount=}: {q}')
  metas=[AccountMeta(Pubkey.from_string(a['key']),a['signer'],a['writable']) for a in q['metas']]
  idl=json.loads((ROOT/'idl/launchpad_program.json').read_text())
  schema=next(x for x in idl['instructions'] if x['name']==('buy' if buy else 'sell'))
  assert base64.b64decode(q['data'])[:8]==bytes(schema['discriminator'])
  assert len(metas)>=len(schema['accounts'])
  assert [(a.is_signer,a.is_writable) for a in metas[:len(schema['accounts'])]]==[(a.get('signer',False),a.get('writable',False)) for a in schema['accounts']]
  ix=Instruction(PROGRAM,base64.b64decode(q['data']),metas)
  if cpi:
   relay=Pubkey.from_bytes(hashlib.sha256(b'LIMITLESS local test CPI relay').digest())
   self.vm.add_program(relay,(ROOT/'tests/fixtures/cpi_relay.so').read_bytes())
   ix=Instruction(relay,bytes(ix.data),[AccountMeta(PROGRAM,False,False),*metas])
  tokens_before=struct.unpack_from('<Q',self.vm.get_account(self.token).data,64)[0]
  sol_before=self.vm.get_balance(WALLET.pubkey())
  result=ok(send(self.vm,ix,[WALLET]))
  tokens_after=struct.unpack_from('<Q',self.vm.get_account(self.token).data,64)[0]
  sol_after=self.vm.get_balance(WALLET.pubkey())
  actual_in=sol_before-sol_after if buy else tokens_before-tokens_after
  actual_out=tokens_after-tokens_before if buy else sol_after-sol_before
  assert actual_in==q['in_amount'],(actual_in,q)
  assert actual_out==q['out_amount'],(actual_out,q)
  return {'buy':buy,'in':actual_in,'out':actual_out,'fee':q['fee_amount'],'compute_units':result.compute_units_consumed(),'cpi':cpi}
def main():
 rows=[]
 for path in sorted((ROOT/'tests/fixtures').glob('*.json')):
  fixture=json.loads(path.read_text())
  if 'pool' not in fixture:continue
  for amount in [1_000,1_000_000,100_000_000,1_000_000_000]:
   h=Harness(fixture);buy=h.trade(True,amount);rows.append({'pool':fixture['pool'],**buy})
   # Sell genuine purchased inventory, after refreshing every mutated account.
   for divisor in [100,10,2]:
    h=Harness(fixture);b=h.trade(True,amount);sell_amount=b['out']//divisor
    if sell_amount:rows.append({'pool':fixture['pool'],**h.trade(False,sell_amount)})
  h=Harness(fixture);d=raw(fixture['accounts'][fixture['pool']]);reserves=struct.unpack_from('<Q',d,142)[0];threshold=struct.unpack_from('<Q',d,215)[0]
  config_key=str(Pubkey.find_program_address([b'config'],PROGRAM)[0]);fee=struct.unpack_from('<H',raw(fixture['accounts'][config_key]),104)[0]
  virtual=struct.unpack_from('<Q',d,126)[0]+reserves;remaining=struct.unpack_from('<Q',d,134)[0];offset=struct.unpack_from('<Q',d,224)[0]
  # Largest net input that cannot oversell. Threshold may lie beyond sold-out completion.
  max_net=(virtual*(remaining+offset)-1)//(offset-1)-virtual if offset>1 else threshold-reserves
  net=min(threshold-reserves,max_net);amount=(net*10_000)//(10_000-fee)
  while amount-amount*fee//10_000<net:amount+=1
  rows.append({'pool':fixture['pool'],'completion':True,**h.trade(True,amount)})
  assert h.vm.get_account(Pubkey.from_string(fixture['pool'])).data[299]==1
  for buy in [True,False]:assert 'error' in quote(h.current(),buy,1_000_000),'complete curve quoted'
  assert (ROOT/'tests/fixtures/cpi_relay.so').exists(), 'CPI fixture missing; do not silently omit CPI verification'
  h=Harness(fixture);b=h.trade(True,100_000_000,True);rows.append({'pool':fixture['pool'],**b});rows.append({'pool':fixture['pool'],**h.trade(False,b['out']//2,True)})
  print('PASS',fixture['pool'],'buy/sell sizes; exact completion; post-completion refusal',flush=True)
 receipt={'result':'PASS','scope':'Real mainnet snapshots, deployed ELF; synthetic users only; native SOL deltas (not WSOL routing)','cases':rows}
 (ROOT/'evidence/parity.json').write_text(json.dumps(receipt,indent=2));print('PASS',len(rows),'exact input/output parity executions')
if __name__=='__main__':main()
