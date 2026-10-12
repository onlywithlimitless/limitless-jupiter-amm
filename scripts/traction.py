#!/usr/bin/env python3
"""Read-only public API metrics with complete pagination and exact decimal sums."""
import json,datetime,decimal
from pathlib import Path
import requests
ROOT=Path(__file__).resolve().parents[1]
now=datetime.datetime.now(datetime.timezone.utc);cut=now-datetime.timedelta(days=7)
def get(path):
 r=requests.get('https://api.limitless.buzz'+path,timeout=30);r.raise_for_status();return r.json()
stats=get('/stats/platform');(ROOT/'evidence/platform-stats.json').write_text(json.dumps(stats,indent=2))
tokens=[];pages=[];transactions=[]
for page in range(1,100):
 data=get(f'/tokens?limit=100&page={page}');pages.append({**data,'data':[{k:t.get(k) for k in ['mintAddress','status','hidden']} for t in data['data']]});tokens+=data['data'];pagination=data.get('pagination',data.get('meta',{}))
 print('tokens page',page,len(data['data']),pagination,flush=True)
 if len(data['data'])<100:break
 if page>=int(pagination.get('totalPages',9999)):break
for page in range(1,100):
 data=get(f'/transactions?limit=100&page={page}');transactions+=data['data'];(ROOT/f'evidence/transactions-page{page}.json').write_text(json.dumps({**data,'data':[{k:t[k] for k in ['txSignature','type','createdAt','amountSol']} for t in data['data']]},indent=2))
 print('tx page',page,len(data['data']),data.get('pagination',data.get('meta',{})),flush=True)
 if len(data['data'])<100 or (data['data'] and datetime.datetime.fromisoformat(data['data'][-1]['createdAt'].replace('Z','+00:00'))<cut):break
(ROOT/'evidence/token-pages.json').write_text(json.dumps(pages,indent=2))
unique={t['txSignature']:t for t in transactions};recent=[t for t in unique.values() if cut<=datetime.datetime.fromisoformat(t['createdAt'].replace('Z','+00:00'))<=now and t['type'] in ['BUY','SELL']]
mints={t['mintAddress']:t for t in tokens};live=[t for t in mints.values() if t.get('status')=='live']
result={'measured_at':now.isoformat(),'window_start':cut.isoformat(),'api':'https://api.limitless.buzz','api_reported_total_tokens':stats['totalTokens'],'listed_unique_tokens':len(mints),'listed_live_tokens':len(live),'seven_day_volume_sol':str(sum((decimal.Decimal(t['amountSol']) for t in recent),decimal.Decimal(0))),'seven_day_trades':len(recent),'oldest_fetched_trade':min(t['createdAt'] for t in transactions),'notes':'Public API indexed BUY/SELL SOL amounts, deduplicated by signature. Includes test activity; not independently reconstructed from all on-chain transactions. Live status count uses public token list and excludes hidden/unlisted entries.'}
(ROOT/'evidence/traction.json').write_text(json.dumps(result,indent=2));print(result)
