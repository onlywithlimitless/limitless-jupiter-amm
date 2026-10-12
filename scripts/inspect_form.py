import re,json,requests
from pathlib import Path
r=Path(__file__).resolve().parents[1]
s=requests.get('https://support.jup.ag/requests/new/amm-integrators',timeout=30).text;hits=[]
for path in re.findall(r'<script[^>]*src="([^"]+)"',s):
 body=requests.get('https://support.jup.ag'+path,timeout=30).text
 if 'integrationTypeSelect' not in body:continue
 for term in ['integrationTypeSelect','AMM','DEX','audit','Github','GitHub']:
  for match in list(re.finditer(term,body))[:12]:hits.append({'term':term,'context':body[max(0,match.start()-180):match.end()+650]})
(r/'evidence/form-fields.json').write_text(json.dumps(hits,indent=2))
for x in hits[:15]:print(x)
