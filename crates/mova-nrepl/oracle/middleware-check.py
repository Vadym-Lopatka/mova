#!/usr/bin/env python3
"""P6 gate: plain-Clojure middleware runs unchanged on JVM nREPL 1.8.0 and on `mova nrepl`.

  python3 crates/mova-nrepl/oracle/middleware-check.py [--mova PATH] [--case NAME] [-v]

Each case starts a JVM nREPL 1.8.0 and a `mova nrepl` server with the same
`--middleware` flag and the fixtures of `middleware/` (`fx/*.clj`, plain
Clojure, the very same files on both sides), sends the same requests, and
compares the replies after masking session ids and the `versions` of
`describe` (they name the JVM). Exit code 0 when every case is equal.
Needs `java` on the PATH (the classpath is `classpath.txt`).
"""
import argparse
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import bencode  # noqa: E402

FIXTURES = os.path.join(HERE, "middleware")
DEFAULT_MOVA = os.path.join(HERE, "..", "..", "..", "target", "release", "mova")


def classpath():
    return open(os.path.join(HERE, "classpath.txt")).read().strip()


class Server:
    def __init__(self, cmd, cwd):
        self.cwd = cwd
        self.proc = subprocess.Popen(cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     stdin=subprocess.DEVNULL, start_new_session=True)
        self.lines = []
        threading.Thread(target=lambda: [self.lines.append(l) for l in self.proc.stdout], daemon=True).start()
        pf = os.path.join(cwd, ".nrepl-port")
        t0 = time.time()
        while True:
            if os.path.exists(pf) and open(pf).read().strip().isdigit():
                self.port = int(open(pf).read().strip())
                break
            if self.proc.poll() is not None:
                raise RuntimeError("server died:\n" + b"".join(self.lines).decode(errors="replace"))
            if time.time() - t0 > 90:
                self.stop()
                raise RuntimeError("server start timeout")
            time.sleep(0.01)

    def stop(self):
        try:
            os.killpg(self.proc.pid, 15)
        except OSError:
            pass
        try:
            self.proc.wait(5)
        except subprocess.TimeoutExpired:
            os.killpg(self.proc.pid, 9)
            self.proc.wait()

    def log(self):
        return b"".join(self.lines).decode(errors="replace")


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=8)
        self.buf = b""

    def send(self, msg):
        self.s.sendall(bencode.encode(msg))

    def reply(self):
        """Replies of the last request, up to and including the one with status `done`."""
        out = []
        while True:
            while True:
                try:
                    v, i = bencode.decode_one(self.buf)
                    self.buf = self.buf[i:]
                    break
                except bencode.Incomplete:
                    d = self.s.recv(65536)
                    if not d:
                        raise RuntimeError("connection closed; got %r" % (out,))
                    self.buf += d
            out.append(v)
            if "done" in (v.get("status") or []):
                return out

    def ask(self, msg):
        self.send(msg)
        return self.reply()


def mask(replies):
    """Same shape on both servers: session ids (top level only) are masked, `versions`
    of describe is dropped (it names the JVM), `err` is dropped (Mova's error text
    differs on purpose, design 8.5), statuses are sorted."""
    def walk(v, key=None):
        if isinstance(v, dict):
            return {k: walk(x, k) for k, x in v.items() if k != "versions"}
        if isinstance(v, list):
            return sorted(v) if key == "status" else [walk(x) for x in v]
        return v

    out = []
    for r in replies:
        r = walk(r)
        for k in ("session", "new-session"):
            if k in r:
                r[k] = "<sid>"
        if "err" in r:
            r["err"] = "<err>"
        if "sessions" in r:
            r["sessions"] = ["<sid>"] * len(r["sessions"])
        out.append(r)
    return out


def run_script(port, script):
    """`script`: list of requests. A request may use {"$new": N} for the Nth cloned session."""
    c = Conn(port)
    results = []
    cloned = []
    for msg in script:
        m = dict(msg)
        for k, v in list(m.items()):
            if isinstance(v, dict) and "$new" in v:
                m[k] = cloned[v["$new"]]
        r = c.ask(m)
        for x in r:
            if "new-session" in x:
                cloned.append(x["new-session"])
        results.append(mask(r))
    c.s.close()
    return results


EVAL = lambda code, **kw: dict(op="eval", code=code, id="1", **kw)

SCRIPT_COMMON = [
    {"op": "describe", "id": "1"},
    {"op": "describe", "id": "2", "verbose?": "1"},
    EVAL("(+ 1 2)"),
    EVAL('(do (println "hi") (print "x") 5)'),
    EVAL("(/ 1 0)"),
    EVAL("(def zz 41)", session="x") if False else EVAL("(def zz 41)"),
    {"op": "clone", "id": "3"},
    EVAL("(inc 1)", session={"$new": 0}),
    {"op": "ls-sessions", "id": "4"},
    {"op": "nosuchop", "id": "5"},
    {"op": "fx/add", "id": "6", "a": "40", "b": "2"},
    {"op": "close", "id": "7", "session": {"$new": 0}},
]

CASES = [
    {"name": "add-op", "mw": "[fx.add-op/wrap-add-op]", "script": SCRIPT_COMMON},
    {"name": "tag-eval", "mw": "[fx.tag-eval/wrap-tag-eval]", "script": SCRIPT_COMMON},
    {"name": "ordered", "mw": "[fx.ordered/wrap-audit fx.ordered/wrap-log]", "script": SCRIPT_COMMON},
    {"name": "ordered-reversed", "mw": "[fx.ordered/wrap-log fx.ordered/wrap-audit]", "script": SCRIPT_COMMON},
    {"name": "all-three", "mw": "[fx.add-op/wrap-add-op fx.tag-eval/wrap-tag-eval fx.ordered/wrap-audit]",
     "script": SCRIPT_COMMON},
]


def run_case(case, mova, verbose):
    out = {}
    tmp = tempfile.mkdtemp(prefix="nrepl-mw-")
    try:
        jvm_cwd = os.path.join(tmp, "jvm")
        mova_cwd = os.path.join(tmp, "mova")
        os.mkdir(jvm_cwd)
        shutil.copytree(FIXTURES, mova_cwd)
        jvm = Server(["java", "-cp", classpath() + ":" + FIXTURES, "clojure.main", "-m", "nrepl.cmdline",
                      "-m", case["mw"]], jvm_cwd)
        mv = Server([mova, "nrepl", "--errors=jvm", "-m", case["mw"]], mova_cwd)
        try:
            for name, srv in (("jvm", jvm), ("mova", mv)):
                out[name] = run_script(srv.port, case["script"])
        finally:
            jl, ml = jvm.log(), mv.log()
            jvm.stop()
            mv.stop()
        if verbose:
            print("jvm log:", jl)
            print("mova log:", ml)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mova", default=os.environ.get("MOVA", DEFAULT_MOVA))
    ap.add_argument("--case")
    ap.add_argument("-v", action="store_true")
    a = ap.parse_args()
    bad = 0
    for case in CASES:
        if a.case and a.case != case["name"]:
            continue
        try:
            r = run_case(case, os.path.abspath(a.mova), a.v)
        except Exception as e:  # noqa: BLE001
            print("FAIL %-18s %s" % (case["name"], e))
            bad += 1
            continue
        diffs = [(i, case["script"][i], r["jvm"][i], r["mova"][i]) for i in range(len(case["script"]))
                 if r["jvm"][i] != r["mova"][i]]
        if not diffs:
            print("PASS %-18s %d requests equal" % (case["name"], len(case["script"])))
        else:
            bad += 1
            print("FAIL %-18s %d of %d requests differ" % (case["name"], len(diffs), len(case["script"])))
            for i, req, j, m in diffs:
                print("  request %d: %s" % (i, req))
                print("    jvm : %s" % (j,))
                print("    mova: %s" % (m,))
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
