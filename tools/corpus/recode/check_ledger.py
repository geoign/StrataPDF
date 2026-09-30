import json,pickle,re,collections,sys
sys.path.insert(0,'.')
from ledger import L
from mine import fam
res=pickle.load(open('mine_all.pkl','rb'))
# all triples incl. unflagged: rebuild counts from chars2_all
cnt=collections.defaultdict(lambda:[set(),0,set()])
famset=set(L)
def canon(f):
    m=re.match(r'^(TeX_CM_Maths_Symbols)\d*$',f)
    return m.group(1) if m else f
for l in open('chars2_all.jsonl',encoding='utf-8'):
    try: r=json.loads(l)
    except: continue
    f=canon(fam(r['font']))
    if f in famset:
        k=(f,r['ucs'])
        cnt[k][0].add(r['id']); cnt[k][1]+=r['n']; cnt[k][2].add(r['ohash'])
        cnt[(f,r['ucs'],r['ohash'])][0].add(r['id'])
out=[]
tot_docs=0
for f,rows in L.items():
    for ch,to in rows.items():
        k=(f,ord(ch))
        if k not in cnt: out.append((f,ch,to,0,0,'NO DATA')); continue
        docs=len(cnt[k][0]); n=cnt[k][1]; hs=cnt[k][2]
        tri=[(h,len(cnt[(f,ord(ch),h)][0]),res.get((f,ord(ch),h),{}).get('sim')) for h in hs]
        out.append((f,ch,to,docs,n,tri))
json.dump([[a,b,c,d,e,f] for a,b,c,d,e,f in out],open('ledger_check.json','w',encoding='utf-8'),ensure_ascii=False)
bad=0
for f,ch,to,docs,n,tri in out:
    if tri=='NO DATA': print('NODATA',f,ch,to); continue
    multi=len(tri)>1
    hi=[t for t in tri if t[2] is not None and t[2]>=0.5]
    if multi or hi:
        print(f,repr(ch),'->',to,'docs',docs,'n',n,'triples',[(h[:6],d,s) for h,d,s in tri])
