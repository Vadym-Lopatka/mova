import json
def dec(path):
    cache=[]
    def rd(x, key=False):
        if isinstance(x,str):
            if x.startswith('^') and x!='^ ' and len(x)>=2:
                c=x[1:]
                i=(ord(c[0])-48) if len(c)==1 else (ord(c[0])-48)*44+(ord(c[1])-48)
                return cache[i]
            cacheable = len(x)>3 and (key or x[:2] in ('~:','~$','~#'))
            v=x
            if x.startswith('~'):
                t=x[1]
                if t==':': v=('kw',x[2:])
                elif t=='$': v=('sym',x[2:])
                elif t=='i': v=int(x[2:])
                elif t=='#': v=('tag',x[2:])
                elif t=='~': v=x[1:]
            if cacheable: cache.append(v)
            return v
        if isinstance(x,list):
            if x and x[0]=='^ ':
                out={}
                it=x[1:]
                for i in range(0,len(it),2):
                    k=rd(it[i],True); out[k]=rd(it[i+1])
                return out
            if len(x)==2 and isinstance(x[0],str) and (x[0].startswith('~#') or (x[0].startswith('^') and x[0]!='^ ')):
                t=rd(x[0]); v=rd(x[1])
                if isinstance(t,tuple) and t[0]=='tag': return (t[1], v)
                return [t, v]
            return [rd(e) for e in x]
        return x
    return rd(json.load(open(path)))
