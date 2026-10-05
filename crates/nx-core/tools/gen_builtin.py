#!/usr/bin/env python3
"""Regenerates src/analyzer/builtin.txt and varinfo.txt from the clj-kondo sources.
Usage: python3 tools/gen_builtin.py <clj_kondo/impl dir>
  e.g. <vendor>/clj-kondo-2026.08.05-SNAPSHOT/clj_kondo/impl
builtin.txt: '@<src> <ns>' header (src = clj|cljs|cljc-clj|cljc-cljs), then 'name<TAB>flags<TAB>fixed,csv<TAB>varargs<TAB>deprecated'.
varinfo.txt: '@core-clj', '@core-cljs' (names), '@imports' (simple<TAB>fq), '@fq-imports' (fq names)."""
import sys, glob, os, re
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from transit import dec

impl = sys.argv[1]
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'src', 'analyzer')
B = impl + '/cache/built_in/'

def val(x):
    return x[1] if isinstance(x, tuple) else x

def emit(f, src, ns, defs):
    f.write('@%s %s\n' % (src, ns))
    for n, v in sorted(defs.items(), key=lambda kv: val(kv[0])):
        if not isinstance(v, dict):
            continue
        g = lambda k: v.get(('kw', k))
        flags = ''.join(c for c, k in (('m', 'macro'), ('p', 'private'), ('c', 'class'), ('f', 'fixed-arities')) if g(k) is not None and (k != 'fixed-arities' or True) and (g(k) or k == 'fixed-arities'))
        fx = g('fixed-arities')
        fx = ','.join(str(i) for i in sorted(fx[1])) if fx else ''
        vm = g('varargs-min-arity')
        dp = g('deprecated')
        dp = '' if dp is None else ('true' if dp is True else str(dp))
        f.write('%s\t%s\t%s\t%s\t%s\n' % (val(n), flags, fx, '' if vm is None else vm, dp))

with open(out + '/builtin.txt', 'w') as f:
    for lang in ('clj', 'cljs', 'cljc'):
        for p in sorted(glob.glob(B + lang + '/*.transit.json')):
            ns = os.path.basename(p)[:-len('.transit.json')]
            d = dec(p)
            if lang == 'cljc':
                for l in ('clj', 'cljs'):
                    emit(f, 'cljc-' + l, ns, d.get(('kw', l), {}))
            else:
                emit(f, lang, ns, d)

txt = open(impl + '/var_info_gen.clj').read()
def block(name):
    m = re.search(r"\(def %s '[#{]\{?(.*?)\}\)" % re.escape(name), txt, re.S)
    return m.group(1)
with open(out + '/varinfo.txt', 'w') as f:
    f.write('@core-clj\n' + '\n'.join(block('clojure-core-syms').split()) + '\n')
    f.write('@core-cljs\n' + '\n'.join(block('cljs-core-syms').split()) + '\n')
    toks = block('default-import->qname').split()
    f.write('@imports\n')
    for i in range(0, len(toks), 2):
        f.write('%s\t%s\n' % (toks[i], toks[i + 1]))
    f.write('@fq-imports\n' + '\n'.join(block('default-fq-imports').split()) + '\n')

# cachespecs.txt: per-arity arg/ret tags of the built-in cache (derived from type hints), '<clj|cljs> <ns> <name> <edn arities>'
def edn(x):
    if x is None:
        return ':any'
    if isinstance(x, bool):
        return 'true' if x else 'false'
    if isinstance(x, int):
        return str(x)
    if isinstance(x, tuple):
        if x[0] == 'kw':
            return ':' + x[1]
        if x[0] == 'set':
            return '#{' + ' '.join(sorted(edn(y) for y in x[1])) + '}'
        return ':any'
    if isinstance(x, list):
        return '[' + ' '.join(edn(y) for y in x) + ']'
    if isinstance(x, dict):
        return '{' + ', '.join(edn(k) + ' ' + edn(v) for k, v in x.items()) + '}'
    return ':any'

with open(out + '/lint/cachespecs.txt', 'w') as f:
    for lang in ('clj', 'cljs'):
        for p in sorted(glob.glob(B + lang + '/*.transit.json')):
            d = dec(p)
            for n, v in sorted(d.items(), key=lambda kv: str(kv[0])):
                if not isinstance(v, dict):
                    continue
                ar = v.get(('kw', 'arities')); ns = v.get(('kw', 'ns'))
                if not ar or not ns:
                    continue
                f.write('%s %s %s %s\n' % (lang, ns[1], val(n), edn(ar)))
