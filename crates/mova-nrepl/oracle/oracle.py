#!/usr/bin/env python3
"""nREPL wire oracle: record goldens from JVM nREPL 1.8.0, check any server against them.

  oracle.py record [--only NAME ...]            fresh JVM server -> goldens/*.json
  oracle.py check  --port N | --server-cmd CMD  run scenarios, diff against goldens
  oracle.py check  --jvm                        fresh JVM server (self-check)

Scenario file (scenarios/*.json):
  {"name": ..., "doc": ..., "sessions": ["s1"],   # quietly cloned before steps, closed after
   "files": {"a.clj": "..."},                     # written under $TMP
   "flaky": "reason" (optional),
   "steps": [ ... ]}
Steps:
  {"send": MSG, "conn": "main", "await": true|false|"<status>", "timeout": 10, "save_as": "s1"}
  {"await": {"id": ID, "status": "done", "conn": "main"}, "timeout": 10}
  {"wait": SECONDS}
  {"open": "b"}   {"close_conn": "b"}
  {"write": {"msgs": [MSG, ...], "split": [N, ...]}, "delay": 0.1, "conn": "main",
   "await_ids": [ID, ...]}     # raw TCP writes; msgs are concatenated in ONE write
  {"hex": "...", ...}          # raw bytes variant of write
String values may contain $s1 (saved session ids) and $TMP. {"$repeat": "ab", "n": 3} expands.
"""
import argparse
import difflib
import glob
import json
import os
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

import bencode

HERE = os.path.dirname(os.path.abspath(__file__))
SCEN_DIR = os.path.join(HERE, "scenarios")
GOLD_DIR = os.path.join(HERE, "goldens")


# --------------------------------------------------------------------------
# NORMALIZATION: every rule lives here. Keep it small.
# --------------------------------------------------------------------------
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
STRING_RULES = [
    # (name, regex, replacement)
    ("object-hash", re.compile(r"(?<=[ @])0x[0-9a-f]{3,16}\b|(?<=[^\s@])@[0-9a-f]{5,8}\b"),
     lambda m: "0xHASH" if m.group(0).startswith("0x") else "@HASH"),
    ("eval-counter", re.compile(r"\beval\d+\b"), lambda m: "eval<N>"),
    ("gensym", re.compile(r"(__|--)\d+\b"), lambda m: m.group(1) + "<N>"),
    ("thread-name", re.compile(r"Thread\[#\d+,Thread-\d+"), lambda m: "Thread[#N,Thread-N"),
    ("elapsed-time", re.compile(r"Elapsed time: [0-9.]+ msecs"), lambda m: "Elapsed time: <T> msecs"),
    ("error-map-trace", re.compile(r":trace\s*\[\[.*?\]\]", re.S), lambda m: ":trace <FRAMES>"),
    ("stack-frames", re.compile(r"(?m)(?:^[ \t]+at [^\n]*(?:\n|$))+"), lambda m: "\t<FRAMES>\n"),
]
SORTED_LIST_KEYS = {"sessions"}

# RULE jvm-internal-frame (the only rule applied at compare time, not at record time).
# The `err` header names the frame where the error happened:
#   Execution error (InterruptedException) at java.lang.Thread/sleepNanos0 (Thread.java:-2).
# When the GOLDEN frame is a JVM source file (`(X.java:N)`), the actual header may
# carry any frame there (Mova has no JVM frames; it prints `user/eval<N> (REPL:1)`).
# The text before ` at `, the text after the header line, and the whole message must
# still match exactly. A golden header with a user frame is compared exactly.
ERR_HEADER_RE = re.compile(r"\A([^\n]*? at )([^\n]*)(\.\n)(.*)\Z", re.S)
JVM_FRAME_RE = re.compile(r"\A\S+/\S+ \([A-Za-z0-9_$]+\.java:-?\d+\)\Z")


def _err_parts(msg):
    if isinstance(msg, dict) and isinstance(msg.get("err"), str):
        m = ERR_HEADER_RE.match(msg["err"])
        if m:
            return m.groups()
    return None


def apply_jvm_frame_rule(gold, got):
    """Rewrites the frame of actual `err` headers to the golden JVM frame when
    everything else in the message is equal. Returns (got, times fired)."""
    table = {}
    for e in gold:
        p = _err_parts(e["msg"])
        if p and JVM_FRAME_RE.match(p[1]):
            table.setdefault((p[0], p[2], p[3]), p[1])
    if not table:
        return got, 0
    fired, out = 0, []
    for e in got:
        p = _err_parts(e["msg"])
        if p and p[1] != table.get((p[0], p[2], p[3]), p[1]):
            frame = table[(p[0], p[2], p[3])]
            e = {"conn": e["conn"], "msg": dict(e["msg"], err=p[0] + frame + p[2] + p[3])}
            fired += 1
        out.append(e)
    return out, fired  # ls-sessions returns a set: order is not defined


class Normalizer:
    def __init__(self, tmpdirs):
        self.uuids = {}
        self.tmpdirs = sorted(set(tmpdirs), key=len, reverse=True)
        self.home = os.path.expanduser("~")

    def uuid(self, m):
        return "<uuid:%d>" % self.uuids.setdefault(m.group(0), len(self.uuids) + 1)

    def string(self, s):
        for d in self.tmpdirs:
            s = s.replace(d, "<TMP>")
        s = s.replace(self.home, "<HOME>")
        s = UUID_RE.sub(self.uuid, s)
        for _name, rx, rep in STRING_RULES:
            s = rx.sub(rep, s)
        return s

    def value(self, v, key=None):
        if isinstance(v, str):
            return self.string(v)
        if isinstance(v, list):
            out = [self.value(x) for x in v]
            if key in SORTED_LIST_KEYS:
                out.sort(key=str)
            return out
        if isinstance(v, dict):
            return {k: self.value(x, k) for k, x in v.items()}
        return v


# --------------------------------------------------------------------------
# server process
# --------------------------------------------------------------------------
DEPS = '{:paths [] :deps {nrepl/nrepl {:mvn/version "1.8.0"} org.clojure/clojure {:mvn/version "1.12.2"}}}'


def classpath():
    """RELEASED nrepl 1.8.0 + Clojure 1.12.2 (cached in classpath.txt; delete the file to recompute)."""
    p = os.path.join(HERE, "classpath.txt")
    if not os.path.exists(p):
        cp = subprocess.run(["clojure", "-Sdeps", DEPS, "-Spath"], capture_output=True, text=True, check=True, cwd=HERE).stdout.strip()
        open(p, "w").write(cp + "\n")
    return open(p).read().strip()


def default_jvm_cmd():
    return ["java", "-cp", classpath(), "clojure.main", "-m", "nrepl.cmdline"]


class Server:
    def __init__(self, cmd, extra_args=()):
        self.cwd = tempfile.mkdtemp(prefix="nrepl-oracle-srv-")
        self.cmd = list(cmd) + list(extra_args)
        self.t0 = time.perf_counter()
        self.proc = subprocess.Popen(self.cmd, cwd=self.cwd, stdout=subprocess.PIPE,
                                     stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        self.lines = []
        self.banner_t = None
        self.t_portfile = None
        self._lock = threading.Condition()
        threading.Thread(target=self._pump, daemon=True).start()
        pf = os.path.join(self.cwd, ".nrepl-port")
        while True:
            if self.t_portfile is None and os.path.exists(pf):
                try:
                    if open(pf).read().strip().isdigit():
                        self.t_portfile = time.perf_counter() - self.t0
                except OSError:
                    pass
            if self.t_portfile is not None and self.banner_t is not None:
                break
            if self.proc.poll() is not None:
                raise RuntimeError("server died: " + b"\n".join(self.lines).decode(errors="replace"))
            if time.perf_counter() - self.t0 > 60:
                self.stop()
                raise RuntimeError("server start timeout")
            time.sleep(0.0005)
        try:
            self.port = int(open(pf).read().strip())
        except Exception:
            self.stop()
            raise

    def _pump(self):
        for line in self.proc.stdout:
            if self.banner_t is None and b"nREPL server started" in line:
                self.banner_t = time.perf_counter() - self.t0
            self.lines.append(line.rstrip(b"\n"))
        # eof: mark banner absent so waiters can notice via poll()

    def stop(self):
        try:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        finally:
            shutil.rmtree(self.cwd, ignore_errors=True)


# --------------------------------------------------------------------------
# client
# --------------------------------------------------------------------------
class Log:
    def __init__(self):
        self.entries = []  # (conn, msg)
        self.cv = threading.Condition()

    def add(self, conn, msg):
        with self.cv:
            self.entries.append((conn, msg))
            self.cv.notify_all()


class Conn:
    def __init__(self, name, port, log):
        self.name, self.log = name, log
        self.sock = socket.create_connection(("127.0.0.1", port))
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.closed = False
        self.t = threading.Thread(target=self._read, daemon=True)
        self.t.start()

    def _read(self):
        buf = bytearray()
        while True:
            try:
                d = self.sock.recv(65536)
            except OSError:
                d = b""
            if not d:
                self.log.add(self.name, {"$eof": True}) if not self.closed else None
                return
            buf += d
            i = 0
            while i < len(buf):
                try:
                    v, i2 = bencode.decode_one(buf, i)
                except bencode.Incomplete:
                    break
                except ValueError as e:
                    self.log.add(self.name, {"$decode-error": str(e)})
                    return
                self.log.add(self.name, v)
                i = i2
            del buf[:i]

    def send_bytes(self, b):
        self.sock.sendall(b)

    def close(self):
        self.closed = True
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.sock.close()


def expand(v, vars_):
    if isinstance(v, str):
        for k, x in vars_.items():
            v = v.replace("$" + k, x)
        return v
    if isinstance(v, list):
        return [expand(x, vars_) for x in v]
    if isinstance(v, dict):
        if "$repeat" in v:
            return v.get("prefix", "") + expand(v["$repeat"], vars_) * v["n"] + v.get("suffix", "")
        return {k: expand(x, vars_) for k, x in v.items()}
    return v


def has_status(msg, st):
    s = msg.get("status")
    return isinstance(s, list) and st in s


def wait_for(log, pred, start, timeout):
    end = time.monotonic() + timeout
    with log.cv:
        while True:
            for idx in range(start, len(log.entries)):
                if pred(*log.entries[idx]):
                    return True
            left = end - time.monotonic()
            if left <= 0:
                return False
            log.cv.wait(min(left, 0.05))


def run_scenario(port, sc):
    log = Log()
    vars_ = {}
    tmp = tempfile.mkdtemp(prefix="nrepl-oracle-files-")
    tmp = os.path.realpath(tmp)
    vars_["TMP"] = tmp
    for fname, content in sc.get("files", {}).items():
        p = os.path.join(tmp, fname)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(content)
    conns = {"main": Conn("main", port, log)}
    quiet_ids = set()

    def quiet_call(msg, conn="main", timeout=10):
        msg = dict(msg)
        quiet_ids.add(msg["id"])
        pos = len(log.entries)
        conns[conn].send_bytes(bencode.encode(msg))
        wait_for(log, lambda c, m: isinstance(m, dict) and m.get("id") == msg["id"] and has_status(m, "done"), pos, timeout)

    def get_session(mid):
        for c, m in log.entries:
            if isinstance(m, dict) and m.get("id") == mid and "new-session" in m:
                return m["new-session"]

    for s in sc.get("sessions", []):
        quiet_call({"op": "clone", "id": "_q-" + s})
        vars_[s] = get_session("_q-" + s)

    def match_await(a, pos):
        aid, st, cn = a.get("id"), a.get("status", "done"), a.get("conn")
        return lambda c, m: (isinstance(m, dict) and (cn is None or c == cn)
                             and (aid is None or m.get("id") == aid) and has_status(m, st))

    try:
        for step in sc["steps"]:
            tmo = step.get("timeout", 10)
            if "open" in step:
                conns[step["open"]] = Conn(step["open"], port, log)
            elif "close_conn" in step:
                conns.pop(step["close_conn"]).close()
            elif "wait" in step:
                time.sleep(step["wait"])
            elif "send" in step or "write" in step or "hex" in step:
                conn = step.get("conn", "main")
                pos = len(log.entries)
                msg_id = None
                if "send" in step:
                    msg = expand(step["send"], vars_)
                    msg_id = msg.get("id")
                    conns[conn].send_bytes(bencode.encode(msg))
                    aw = step.get("await", True)
                    ids = [msg_id] if aw is not False else []
                    awst = "done" if aw is True else aw
                else:
                    if "hex" in step:
                        data = bytes.fromhex(step["hex"])
                        chunks = [data]
                    else:
                        w = expand(step["write"], vars_)
                        data = b"".join(bencode.encode(m) for m in w["msgs"])
                        cuts = [0] + w.get("split", []) + [len(data)]
                        chunks = [data[a:b] for a, b in zip(cuts, cuts[1:])]
                    for i, ch in enumerate(chunks):
                        if i:
                            time.sleep(step.get("delay", 0.1))
                        conns[conn].send_bytes(ch)
                    ids, awst = step.get("await_ids", []), "done"
                for i in ids:
                    ok = wait_for(log, match_await({"id": i, "status": awst, "conn": conn}, pos), pos, tmo)
                    if not ok:
                        log.add(conn, {"$timeout": "waiting %s on id %s" % (awst, i)})
                if not ids and msg_id is None and "send" in step and step.get("await", True) is True:
                    # no id on message: wait for any done on this conn
                    if not wait_for(log, lambda c, m: c == conn and isinstance(m, dict) and has_status(m, "done"), pos, tmo):
                        log.add(conn, {"$timeout": "waiting done on message without id"})
                if "save_as" in step:
                    vars_[step["save_as"]] = get_session(msg_id)
            elif "await" in step:
                a = step["await"]
                if not wait_for(log, match_await(a, 0), 0, tmo):
                    log.add(a.get("conn", "main"), {"$timeout": "await %s" % json.dumps(a)})
            else:
                raise ValueError("bad step %r" % (step,))
        time.sleep(sc.get("settle", 0.3))
    finally:
        sessions = [v for k, v in vars_.items() if k != "TMP" and v]
        for i, s in enumerate(sessions):
            try:
                quiet_call({"op": "close", "id": "_qc-%d" % i, "session": s}, timeout=3)
            except Exception:
                pass
        for c in conns.values():
            c.close()
        shutil.rmtree(tmp, ignore_errors=True)
    norm = Normalizer([tmp, tmp.replace("/private", "")])
    out = []
    for c, m in log.entries:
        if isinstance(m, dict) and m.get("id") in quiet_ids:
            continue
        out.append({"conn": c, "msg": norm.value(m)})
    return out


# --------------------------------------------------------------------------
def load_scenarios(only=None):
    res = []
    for p in sorted(glob.glob(os.path.join(SCEN_DIR, "*.json"))):
        sc = json.load(open(p))
        sc.setdefault("name", os.path.basename(p)[:-5])
        if only and not any(o in sc["name"] for o in only):
            continue
        res.append(sc)
    return res


def dump(entries):
    return [json.dumps(e, ensure_ascii=False) for e in entries]


def cmd_record(args):
    os.makedirs(GOLD_DIR, exist_ok=True)
    srv = Server(shlex.split(args.server_cmd) if args.server_cmd else default_jvm_cmd())
    try:
        for sc in load_scenarios(args.only):
            ent = run_scenario(srv.port, sc)
            json.dump({"scenario": sc["name"], "doc": sc.get("doc", ""), "messages": ent},
                      open(os.path.join(GOLD_DIR, sc["name"] + ".json"), "w"), ensure_ascii=False, indent=0)
            print("%-28s %5d messages" % (sc["name"], len(ent)))
    finally:
        srv.stop()


def cmd_check(args):
    srv = None
    if args.port:
        port = args.port
    else:
        cmd = shlex.split(args.server_cmd) if args.server_cmd else default_jvm_cmd()
        srv = Server(cmd)
        port = srv.port
    bad = 0
    try:
        for sc in load_scenarios(args.only):
            gp = os.path.join(GOLD_DIR, sc["name"] + ".json")
            if not os.path.exists(gp):
                print("%-28s NO GOLDEN" % sc["name"])
                continue
            gold = json.load(open(gp))["messages"]
            got = run_scenario(port, sc)
            got, fired = apply_jvm_frame_rule(gold, got)
            rule = "  [rule jvm-internal-frame fired %dx]" % fired if fired else ""
            a, b = dump(gold), dump(got)
            if args.loose_status_order:
                def srt(e):
                    m = e["msg"]
                    if isinstance(m, dict) and isinstance(m.get("status"), list):
                        m = dict(m, status=sorted(m["status"]))
                    return {"conn": e["conn"], "msg": m}
                a, b = dump([srt(e) for e in gold]), dump([srt(e) for e in got])
            if a != b and sc.get("flaky") and sorted(a) == sorted(b):
                print("%-28s equal as multiset, order differs (%d messages)  [flaky-listed: %s]%s" % (sc["name"], len(a), sc["flaky"], rule))
            elif a == b:
                print("%-28s equal   (%d messages)%s%s" % (sc["name"], len(a), "  [flaky-listed: " + sc["flaky"] + "]" if sc.get("flaky") else "", rule))
            else:
                bad += 1
                print("%-28s DIFFER%s%s" % (sc["name"], "  [flaky-listed: " + sc["flaky"] + "]" if sc.get("flaky") else "", rule))
                d = list(difflib.unified_diff(a, b, "golden", "actual", lineterm="", n=1))
                for line in d[:args.max_diff]:
                    print("    " + (line if len(line) < 400 else line[:400] + " ...[cut]"))
                if len(d) > args.max_diff:
                    print("    ... %d more diff lines" % (len(d) - args.max_diff))
    finally:
        if srv:
            srv.stop()
    print("differ: %d" % bad)
    return 1 if bad else 0


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("record")
    r.add_argument("--only", nargs="*")
    r.add_argument("--server-cmd")
    c = sub.add_parser("check")
    c.add_argument("--port", type=int)
    c.add_argument("--server-cmd")
    c.add_argument("--jvm", action="store_true")
    c.add_argument("--only", nargs="*")
    c.add_argument("--max-diff", type=int, default=30)
    c.add_argument("--loose-status-order", action="store_true")
    args = ap.parse_args()
    if args.cmd == "record":
        cmd_record(args)
    else:
        sys.exit(cmd_check(args))


if __name__ == "__main__":
    main()
