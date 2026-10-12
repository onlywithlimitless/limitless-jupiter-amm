from snapshot import *
opts={'encoding':'base64','commitment':'finalized'}
receipts=[]
for name,key in [('spl_token_2022',TOKEN),('spl_associated_token_account',ATA)]:
 a=rpc('getAccountInfo',[key,opts]);d=raw(a['value'])
 if a['value']['owner']=='BPFLoaderUpgradeab1e11111111111111111111111':
  pd=str(Pubkey.from_bytes(d[4:36]));a=rpc('getAccountInfo',[pd,opts]);d=raw(a['value'])[45:]
 (ROOT/f'tests/fixtures/{name}.so').write_bytes(d)
 receipts.append({'label':name,'program':key,'slot':a['context']['slot'],'sha256':hashlib.sha256(d).hexdigest(),'bytes':len(d)})
(ROOT/'evidence/dependencies.json').write_text(json.dumps(receipts,indent=2));print(receipts)

(ROOT/'tests/fixtures/wsol-mint.json').write_text(json.dumps(rpc('getAccountInfo',['So11111111111111111111111111111111111111112',opts]),indent=2))
