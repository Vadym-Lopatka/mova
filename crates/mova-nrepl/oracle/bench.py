#!/usr/bin/env python3
"""JVM nREPL baseline numbers. Clock: time.perf_counter_ns (monotonic). Stdlib only.

  bench.py startup  [--n 20] [--mode java|clojure|both]
  bench.py latency  [--n 10000]
  bench.py throughput
  bench.py memory
  bench.py all --out results.json
Any command accepts --server-cmd "<cmd>" to bench another server instead of the JVM one
(JVM-only parts: the `clojure` CLI mode).
Notes: client is Python; its own overhead (~20-50 us per round trip) is inside every latency number.
"""
import argparse, json, os, re, shlex, socket, statistics, subprocess, sys, tempfile, time, shutil, threading
import bencode
import oracle

HERE = os.path.dirname(os.path.abspath(__file__))
DEPS = oracle.DEPS


def uptime():
    return subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip()


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def summ(xs):
    return {"n": len(xs), "min": min(xs), "median": statistics.median(xs), "p99": pct(xs, 99), "max": max(xs)}


class Client:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port))
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = bytearray()

    def call(self, msg):
        """send, read until a message with status containing done. returns list of msgs"""
        self.s.sendall(bencode.encode(msg))
        out = []
        while True:
            while True:
                try:
                    v, i = bencode.decode_one(self.buf, 0)
                except bencode.Incomplete:
                    break
                del self.buf[:i]
                out.append(v)
                st = v.get("status") if isinstance(v, dict) else None
                if st and "done" in st:
                    return out
            d = self.s.recv(65536)
            if not d:
                raise EOFError
            self.buf += d

    def close(self):
        self.s.close()


class Spawned:
    """Spawn server, time: banner, port file, first describe reply, first eval reply."""
    def __init__(self, cmd, cwd):
        for f in (".nrepl-port",):
            try:
                os.remove(os.path.join(cwd, f))
            except FileNotFoundError:
                pass
        self.cwd = cwd
        self.t0 = time.perf_counter_ns()
        self.p = subprocess.Popen(cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        self.t = {}
        self.banner = None
        threading.Thread(target=self._pump, daemon=True).start()
        pf = os.path.join(cwd, ".nrepl-port")
        while "portfile" not in self.t:
            try:
                if os.path.getsize(pf) > 0 and open(pf).read().strip().isdigit():
                    self.t["portfile"] = time.perf_counter_ns() - self.t0
            except OSError:
                pass
            if self.p.poll() is not None:
                raise RuntimeError("server exited early")
            if time.perf_counter_ns() - self.t0 > 90e9:
                self.kill()
                raise RuntimeError("timeout")
        self.port = int(open(pf).read().strip())
        c = Client(self.port)
        c.call({"op": "describe", "id": "1"})
        self.t["describe"] = time.perf_counter_ns() - self.t0
        c.call({"op": "eval", "id": "2", "code": "(+ 1 2)"})
        self.t["eval"] = time.perf_counter_ns() - self.t0
        self.c = c
        t_end = time.perf_counter_ns()
        while "banner" not in self.t and time.perf_counter_ns() - t_end < 5e9:
            time.sleep(0.001)

    def _pump(self):
        for line in self.p.stdout:
            if "banner" not in self.t and b"nREPL server started" in line:
                self.t["banner"] = time.perf_counter_ns() - self.t0
                self.banner = line.decode().rstrip("\n")

    def kill(self):
        try:
            self.p.terminate()
            try:
                self.p.wait(5)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()
        except Exception:
            pass
        try:
            os.remove(os.path.join(self.cwd, ".nrepl-port"))
        except OSError:
            pass


def jvm_cmds():
    cp = oracle.classpath()
    return {
        "java": ["java", "-cp", cp, "clojure.main", "-m", "nrepl.cmdline"],
        "clojure": ["clojure", "-Sdeps", DEPS, "-M", "-m", "nrepl.cmdline"],
    }


def work_cwd(name):
    d = os.path.join(tempfile.gettempdir(), "nrepl-oracle-bench-" + name)
    os.makedirs(d, exist_ok=True)
    return d


def bench_startup(n, modes, extra_cmd=None):
    res = {}
    cmds = {"custom": shlex.split(extra_cmd)} if extra_cmd else {m: jvm_cmds()[m] for m in modes}
    for mode, cmd in cmds.items():
        cwd = work_cwd(mode)
        sp = Spawned(cmd, cwd)  # warm-up (fills .cpcache / OS caches), not counted
        sp.kill()
        runs = {"banner": [], "portfile": [], "describe": [], "eval": []}
        loads = [uptime()]
        for _ in range(n):
            sp = Spawned(cmd, cwd)
            for k in runs:
                runs[k].append(sp.t.get(k, float("nan")) / 1e6)
            sp.kill()
            time.sleep(0.3)
        loads.append(uptime())
        res[mode] = {"ms": {k: summ(v) for k, v in runs.items()}, "raw_ms": runs, "uptime": loads}
    return res


def bench_latency(n, cmd):
    sp = Spawned(cmd, work_cwd("lat"))
    res = {"uptime_before": uptime()}
    try:
        c = sp.c
        sid = c.call({"op": "clone", "id": "c"})[0]["new-session"]
        cases = {
            "describe": {"op": "describe", "id": "d"},
            "clone": {"op": "clone", "id": "c"},
            "eval (+ 1 2)": {"op": "eval", "id": "e", "code": "(+ 1 2)", "session": sid},
        }
        for name, msg in cases.items():
            for _ in range(500):  # warm-up
                r = c.call(msg)
                if name == "clone":
                    c.call({"op": "close", "id": "x", "session": r[0]["new-session"]})
            xs = []
            for _ in range(n):
                t = time.perf_counter_ns()
                r = c.call(msg)
                xs.append((time.perf_counter_ns() - t) / 1e3)
                if name == "clone":  # keep session count flat (not timed)
                    c.call({"op": "close", "id": "x", "session": r[0]["new-session"]})
            res[name] = {"us": summ(xs), "us_p50": pct(xs, 50), "us_p99": pct(xs, 99)}
    finally:
        res["uptime_after"] = uptime()
        sp.kill()
    return res


def footprint(pid):
    out = subprocess.run(["footprint", "-p", str(pid)], capture_output=True, text=True).stdout
    m = re.search(r"Footprint:\s+([\d.]+)\s+(KB|MB|GB)", out)
    mult = {"KB": 1 / 1024, "MB": 1, "GB": 1024}
    return float(m.group(1)) * mult[m.group(2)] if m else None


def rss_mb(pid):
    return int(subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()) / 1024


def run_throughput(sp, count=1000000):
    """client reads all bytes; messages counted by the 7:session36: marker (each message has a session)."""
    c = sp.c
    sid = c.call({"op": "clone", "id": "c"})[0]["new-session"]
    code = "(dotimes [_ %d] (println \"x\"))" % count
    msg = bencode.encode({"op": "eval", "id": "t", "code": code, "session": sid})
    marker = b"7:session36:"
    tail = b""
    n_msgs = n_out = 0
    t = time.perf_counter_ns()
    c.s.sendall(msg)
    while True:
        d = c.s.recv(1 << 20)
        if not d:
            raise EOFError
        data = tail + d
        n_msgs += data.count(marker) - tail.count(marker)
        n_out += data.count(b"3:out2:x\n") - tail.count(b"3:out2:x\n")
        tail = data[-64:]
        if b"6:statusl4:donee" in data[-80:]:
            break
    dt = (time.perf_counter_ns() - t) / 1e9
    return {"wall_s": dt, "messages": n_msgs, "out_messages": n_out, "count": count}


def validate_counter(sp, count=20000):
    """prove fast counter == full decoder on a smaller run"""
    c = sp.c
    out = c.call({"op": "eval", "id": "v", "code": "(dotimes [_ %d] (println \"x\"))" % count})
    full = len(out)
    fast = run_throughput(sp, count)
    return {"full_decoder_messages": full, "fast_counter_messages": fast["messages"]}


def bench_throughput(cmd, runs=3):
    res = []
    for _ in range(runs):
        sp = Spawned(cmd, work_cwd("thr"))
        try:
            val = validate_counter(sp) if not res else None
            up = uptime()
            r = run_throughput(sp)
            r["uptime"] = up
            if val:
                r["counter_validation"] = val
            res.append(r)
        finally:
            sp.kill()
    return res


def bench_memory(cmd, runs=3):
    res = []
    for _ in range(runs):
        sp = Spawned(cmd, work_cwd("mem"))
        try:
            pid = sp.p.pid
            time.sleep(2)
            r = {"idle_after_first_eval": {"footprint_mb": footprint(pid), "rss_mb": rss_mb(pid)}}
            th = run_throughput(sp)
            r["after_throughput_immediate"] = {"footprint_mb": footprint(pid), "rss_mb": rss_mb(pid)}
            time.sleep(3)
            r["after_throughput_3s"] = {"footprint_mb": footprint(pid), "rss_mb": rss_mb(pid)}
            r["throughput_wall_s"] = th["wall_s"]
            r["uptime"] = uptime()
            res.append(r)
        finally:
            sp.kill()
    return res


def bench_calibrate(n=10000):
    """client-side floor: round trip to a trivial Python server that answers with a canned `done` message
    (same Client.call path, same encode/decode). Not JVM time; shows what the Python client itself costs."""
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    canned = bencode.encode({"id": "e", "ns": "user", "session": "x" * 36, "value": "3"}) + bencode.encode({"id": "e", "session": "x" * 36, "status": ["done"]})

    def serve():
        c, _ = srv.accept()
        c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        while True:
            d = c.recv(65536)
            if not d:
                return
            c.sendall(canned)
    threading.Thread(target=serve, daemon=True).start()
    c = Client(srv.getsockname()[1])
    msg = {"op": "eval", "id": "e", "code": "(+ 1 2)", "session": "x" * 36}
    for _ in range(500):
        c.call(msg)
    xs = []
    for _ in range(n):
        t = time.perf_counter_ns()
        c.call(msg)
        xs.append((time.perf_counter_ns() - t) / 1e3)
    return {"us": summ(xs), "uptime": uptime()}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("what", choices=["startup", "latency", "throughput", "memory", "calibrate", "all"])
    ap.add_argument("--n", type=int)
    ap.add_argument("--mode", default="both")
    ap.add_argument("--server-cmd")
    ap.add_argument("--out")
    a = ap.parse_args()
    cmd = shlex.split(a.server_cmd) if a.server_cmd else jvm_cmds()["java"]
    out = {"when": time.strftime("%F %T"), "uname": os.uname().release, "uptime_start": uptime()}
    if a.what in ("startup", "all"):
        modes = ["java", "clojure"] if a.mode == "both" else [a.mode]
        out["startup"] = bench_startup(a.n or 20, modes, a.server_cmd)
    if a.what in ("latency", "all"):
        out["latency"] = bench_latency(a.n or 10000, cmd)
    if a.what in ("calibrate", "all"):
        out["calibrate"] = bench_calibrate()
    if a.what in ("throughput", "all"):
        out["throughput"] = bench_throughput(cmd)
    if a.what in ("memory", "all"):
        out["memory"] = bench_memory(cmd)
    out["uptime_end"] = uptime()
    s = json.dumps(out, indent=1)
    if a.out:
        open(a.out, "w").write(s)
    print(json.dumps({k: v for k, v in out.items()}, indent=1)[:6000] if not a.out else "written " + a.out)


if __name__ == "__main__":
    main()
