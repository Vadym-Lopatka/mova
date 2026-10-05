#!/usr/bin/env python3
"""Replay what Calva (src/nrepl/index.ts, checked 2026-10) sends to a plain nREPL server.
usage: calva-replay.py PORT OUT.jsonl WORKDIR   (OUT.jsonl in the same format as proxy.py)
Calva sends cider-nrepl-only ops (complete, info) only if `describe` lists them, so against plain nREPL
completion/doc are not sent; we also probe the plain `completions`/`lookup` ops it does not use."""
import socket, sys, json, os, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import bencode

port, out, work = int(sys.argv[1]), sys.argv[2], sys.argv[3]
sock = socket.create_connection(("127.0.0.1", port)); sock.settimeout(0.2)
log = open(out, "w"); buf = b""; nid = [0]; ALL = []
def send(m):
    log.write(json.dumps({"dir": "c2s", "msg": m}) + "\n"); sock.sendall(bencode.encode(m))
def pump(secs):
    global buf
    got = []; end = time.time() + secs
    while time.time() < end:
        try:
            d = sock.recv(65536)
            if not d: break
            buf += d
        except socket.timeout: pass
        while buf:
            try: m, n = bencode._decode(buf, 0)
            except bencode.Incomplete: break
            buf = buf[n:]; got.append(m); ALL.append(m); log.write(json.dumps({"dir": "s2c", "msg": m}) + "\n")
    return got
def req(m, secs=1.0, until_done=True):
    nid[0] += 1; m = dict(m, id=str(nid[0])); send(m)
    got = []; end = time.time() + secs
    while time.time() < end:
        got += [g for g in pump(0.1) if g.get("id") == m["id"]]
        if until_done and any("done" in g.get("status", []) for g in got): break
    return got
def res(step, ok, detail=""): print("RESULT calva %s %s %s" % (step, ok if isinstance(ok, str) else ("PASS" if ok else "FAIL"), detail.replace("\n", "|")[:300]))
def vals(g): return [x["value"] for x in g if "value" in x]
def has(g, key, needle): return any(needle in str(x.get(key, "")) for x in g)

# connect sequence (index.ts ~255-300)
g = req({"op": "eval", "code": "*ns*"})
sess0 = next((x["session"] for x in g if "session" in x), None)
g = req({"op": "clone", "client-name": "Calva", "client-version": "2.0.0"})
sess = next((x["new-session"] for x in g if "new-session" in x), None)
d = req({"op": "describe", "verbose": "true", "session": sess})
res("connect", sess is not None and any("ops" in x for x in d))
ops = next((x["ops"] for x in d if "ops" in x), {})
sess2 = next((x["new-session"] for x in req({"op": "clone", "session": sess, "client-name": "Calva", "client-version": "2.0.0"}) if "new-session" in x), None)
ls = req({"op": "ls-sessions", "session": sess})
print("INFO ls-sessions:", [x.get("sessions") and len(x["sessions"]) for x in ls])
print("INFO describe ops:", sorted(ops))
# Calva's own describe-based gating: complete/info are only sent when listed
pp = {"nrepl.middleware.print/print": "cider.nrepl.pprint/pprint", "nrepl.middleware.print/options": {"right-margin": 120}, "nrepl.middleware.print/quota": 1048576}
def ev(code, **kw): return req(dict({"op": "eval", "ns": "user", "session": sess, "code": code, "line": 1, "column": 1, "file": os.path.join(work, "a.clj")}, **pp, **kw), 5)
g = ev("(+ 1 2)");                       res("eval", "3" in vals(g), str(vals(g)))
g = ev('(println "hello-out")');         res("print", has(g, "out", "hello-out"))
g = ev("(/ 1 0)");                       res("error", has(g, "err", "Divide by zero"), "status=%s" % [x.get("status") for x in g if "status" in x])
g = ev("(def a 1) (def b 2) (+ a b)");   res("multi", vals(g)[-1:] == ["3"], str(vals(g)))
f = os.path.join(work, "load.clj"); src = "(ns user)\n(defn sq [x]\n  (* x x))\n(println \"loaded\")\n"
open(f, "w").write(src)
g = req(dict({"op": "load-file", "session": sess, "file": src, "file-name": "load.clj", "file-path": f}, **pp), 5)
g2 = ev("(sq 9)");                       res("load", has(g, "out", "loaded") and vals(g2) == ["81"], str(vals(g2)))
if "complete" in ops: g = req({"op": "complete", "ns": "user", "symbol": "ma", "session": sess}); res("complete", bool(g[0].get("completions")))
else:
    g = req({"op": "completions", "ns": "user", "prefix": "ma", "session": sess})
    res("complete", "SKIP", " Calva sends op complete only if describe lists it (cider-nrepl); plain completions probe returned %d" % len(g[0].get("completions", [])))
if "info" in ops: g = req({"op": "info", "ns": "user", "symbol": "map", "session": sess}); res("doc", has(g, "doc", "lazy"))
else:
    g = req({"op": "lookup", "ns": "user", "sym": "map", "session": sess})
    res("doc", "SKIP", " Calva sends op info only if listed; plain lookup probe status=%s" % g[0].get("status"))
# interrupt
nid[0] += 1; sid = str(nid[0]); send(dict({"op": "eval", "ns": "user", "session": sess, "code": "(Thread/sleep 60000)", "id": sid}))
pump(1.0)
ig = req({"op": "interrupt", "session": sess, "interrupt-id": sid})
pump(1.5)
st = [s for x in ALL if x.get("id") == sid for s in x.get("status", [])]
g = ev("(+ 20 22)")
res("interrupt", "interrupted" in st and vals(g) == ["42"], "status=%s interrupt-reply=%s" % (st, [x.get("status") for x in ig]))
# stdin
nid[0] += 1; rid = str(nid[0]); send({"op": "eval", "ns": "user", "session": sess, "code": "(read-line)", "id": rid})
need = False; t0 = time.time(); allr = []
while time.time() - t0 < 4:
    for x in pump(0.2):
        allr.append(x)
        if x.get("id") == rid and "need-input" in x.get("status", []) and not need:
            need = True; send({"op": "stdin", "stdin": "typed-text\n", "session": sess})
    if any(x.get("id") == rid and "done" in x.get("status", []) for x in allr): break
res("stdin", need and '"typed-text"' in [x.get("value") for x in allr], "need-input=%s" % need)
g = req({"op": "close", "session": sess2 or sess}); res("close", any("session-closed" in x.get("status", []) for x in g), str([x.get("status") for x in g]))
