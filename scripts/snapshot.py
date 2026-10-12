#!/usr/bin/env python3
"""Explicit read-only mainnet capture; never called by the AMM or offline tests."""
import base64, hashlib, json, os, struct, time
from pathlib import Path
import requests
from solders.pubkey import Pubkey
ROOT=Path(__file__).resolve().parents[1]
PROGRAM='LimitQGBbcHeGJZd3FbQEejiCRuk1rfvwhbv8XCtAoN'
TOKEN='TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb'
ATA='ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'
RPC=os.environ.get('RPC','https://api.mainnet-beta.solana.com')
def rpc(method,params):
 assert method in ['getAccountInfo','getMultipleAccounts','getProgramAccounts','getSlot']
 for attempt in range(5):
  r=requests.post(RPC,json={'jsonrpc':'2.0','id':1,'method':method,'params':params},timeout=60)
  if r.status_code==429: time.sleep(2**attempt);continue
  r.raise_for_status();d=r.json()
  if 'error' in d: raise RuntimeError(d['error'])
  return d['result']
 raise RuntimeError('RPC rate limited')
def pda(*seeds):return str(Pubkey.find_program_address(seeds,Pubkey.from_string(PROGRAM))[0])
def ata(mint,owner):return str(Pubkey.find_program_address([bytes(Pubkey.from_string(owner)),bytes(Pubkey.from_string(TOKEN)),bytes(Pubkey.from_string(mint))],Pubkey.from_string(ATA))[0])
def raw(a):return base64.b64decode(a['data'][0])
if __name__=='__main__':
 opts={'encoding':'base64','commitment':'finalized'}
 info=rpc('getAccountInfo',[PROGRAM,opts]);pd=str(Pubkey.from_bytes(raw(info['value'])[4:36]))
 prog=rpc('getAccountInfo',[pd,opts]);b=raw(prog['value'])
 # Preserve the complete deployed ProgramData payload, including trailing allocation padding.
 elf=b[45:];(ROOT/'tests/fixtures/limitless.so').write_bytes(elf)
 alphabet='123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
 n=int.from_bytes(hashlib.sha256(b'account:LaunchedCoin').digest()[:8],'big');disc=''
 while n: n,r=divmod(n,58);disc=alphabet[r]+disc
 pools=rpc('getProgramAccounts',[PROGRAM,{**opts,'withContext':True,'filters':[{'memcmp':{'offset':0,'bytes':disc}}]}])
 (ROOT/'evidence/mainnet-pools.json').write_text(json.dumps(pools,indent=2))
 selected=[]
 for row in pools['value']:
  if len(raw(row['account'])) != 344: continue
  d=raw(row['account']);remaining,reserves=struct.unpack_from('<QQ',d,134);threshold=struct.unpack_from('<Q',d,215)[0]
  if not d[182] and not d[299] and reserves<threshold and remaining:
   selected.append(row)
 selected.sort(key=lambda x:(-struct.unpack_from('<H',raw(x['account']),107)[0],-struct.unpack_from('<Q',raw(x['account']),142)[0]))
 selected=selected[:4]
 if not selected:raise RuntimeError('No active mainnet pools')
 cfg=pda(b'config');rew=pda(b'rewards_pool_v2');config=rpc('getAccountInfo',[cfg,opts])['value'];treasury=str(Pubkey.from_bytes(raw(config)[40:72]))
 for row in selected:
  d=raw(row['account']);mint=str(Pubkey.from_bytes(d[8:40]));creator=str(Pubkey.from_bytes(d[72:104]));charity=str(Pubkey.from_bytes(d[232:264]));pool=row['pubkey']
  keys=[pool,cfg,rew,mint,ata(mint,pool),treasury,creator,charity,'SysvarC1ock11111111111111111111111111111111','SysvarRent111111111111111111111111111111111']
  for recipient in [treasury,creator,charity]:keys.append(pda(b'bonding_fee_escrow',bytes(Pubkey.from_string(mint)),bytes(Pubkey.from_string(recipient))))
  keys=list(dict.fromkeys(keys));result=rpc('getMultipleAccounts',[keys,opts]);fixture={'slot':result['context']['slot'],'pool':pool,'mint':mint,'accounts':dict(zip(keys,result['value']))}
  (ROOT/f'tests/fixtures/{pool}.json').write_text(json.dumps(fixture,indent=2));print('Captured',pool,mint,'fee bps',struct.unpack_from('<H',d,107)[0],flush=True)
 receipt={'program':PROGRAM,'programdata':pd,'slot':prog['context']['slot'],'deployment_slot':struct.unpack_from('<Q',b,4)[0],'elf_sha256':hashlib.sha256(elf).hexdigest(),'elf_bytes':len(elf),'pools_total':len(pools['value']),'selected':[x['pubkey'] for x in selected]}
 (ROOT/'evidence/snapshot.json').write_text(json.dumps(receipt,indent=2));print(receipt)
