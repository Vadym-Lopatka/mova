#!/usr/bin/env python3
"""wirediff.py MOVA.jsonl JVM.jsonl : compare two proxy recordings op by op.
Reports: ops sent by the client (counts), and for each op the reply key sets / status that differ.
Volatile values (ids, session uuids, times) are ignored; only keys and statuses are compared."""
import json, sys, collections

def load(p):
    reqs, resp = {}, collections.defaultdict(list)
    order = []
    for line in open(p):
        d = json.loads(line); m = d["msg"]
        if d["dir"] == "c2s":
            key = (m.get("id"), )
            reqs[m.get("id")] = m
            order.append(m)
        else:
            resp[m.get("id")].append(m)
    return order, reqs, resp

def shape(op, ms):
    out = []
    for m in ms:
        ks = tuple(sorted(k for k in m if k not in ("id", "session", "new-session")))
        out.append((ks, tuple(m.get("status", []))))
    return out

mo, mr, mresp = load(sys.argv[1]); jo, jr, jresp = load(sys.argv[2])
def ops(o): return collections.Counter(m.get("op") for m in o)
print("ops sent MOVA:", dict(ops(mo))); print("ops sent JVM :", dict(ops(jo)))
# request fields per op
def fields(o):
    f = collections.defaultdict(set)
    for m in o: f[m.get("op")] |= set(m)
    return f
mf, jf = fields(mo), fields(jo)
for op in sorted(set(mf) | set(jf)):
    if mf[op] != jf[op]: print("REQ FIELD DIFF", op, "mova-only", sorted(mf[op]-jf[op]), "jvm-only", sorted(jf[op]-mf[op]))
# reply shape per op (set of distinct shapes)
def shapes(order, resp):
    r = collections.defaultdict(set)
    for m in order:
        r[m.get("op")].add(tuple(shape(m.get("op"), resp.get(m.get("id"), []))))
    return r
ms, js = shapes(mo, mresp), shapes(jo, jresp)
for op in sorted(set(ms) | set(js), key=str):
    if ms[op] != js[op]:
        print("REPLY DIFF op=%s" % op)
        for s in sorted(ms[op]-js[op]): print("  mova:", s)
        for s in sorted(js[op]-ms[op]): print("  jvm :", s)
