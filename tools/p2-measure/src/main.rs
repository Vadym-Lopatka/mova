//! P2 numbers for `mova nrepl` (eval): round trips, `out` throughput, memory, idle CPU.
//!
//!   p2-measure warm N mova           eval "(+ 1 2)" round trip: persistent session and ephemeral
//!   p2-measure tool N mova           warm `completions` ("ma", "", "clojure.string/") and `lookup` round trips
//!   p2-measure println N mova        N x (println "x") in one eval: out msg/s
//!   p2-measure mem mova              footprint: idle, +K sessions, after 100k evals in one session
//!   p2-measure soak mova [redefn|unique|both]   10 sessions, 100k evals, footprint curve + PASS/FAIL (tools/soak.sh)
//!   p2-measure cpu mova SECS         idle CPU after some evals
//!
//! Footprint = macOS `footprint -p` ("Footprint: N KB|MB"), i.e. `phys_footprint`.
use mova_nrepl::bencode::{decode, encode, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
    off: usize,
}

impl Conn {
    fn open(port: u16) -> Conn {
        let t = Instant::now();
        loop {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                s.set_nodelay(true).unwrap();
                return Conn { s, buf: Vec::with_capacity(1 << 20), off: 0 };
            }
            assert!(t.elapsed() < Duration::from_secs(10), "connect timeout");
        }
    }
    fn send(&mut self, items: &[(&str, &str)]) {
        let d = Value::Dict(items.iter().map(|(k, v)| (k.as_bytes().to_vec(), Value::str(v))).collect());
        let mut o = Vec::new();
        encode(&d, &mut o);
        self.s.write_all(&o).unwrap();
    }
    fn next(&mut self) -> Value {
        loop {
            if let Ok(Some((v, used))) = decode(&self.buf[self.off..]) {
                self.off += used;
                if self.off > (1 << 19) {
                    self.buf.drain(..self.off);
                    self.off = 0;
                }
                return v;
            }
            let mut tmp = [0u8; 1 << 16];
            let n = self.s.read(&mut tmp).unwrap();
            assert!(n > 0, "eof");
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }
    /// Sends and reads until `done`. Returns (out messages, value).
    fn call(&mut self, items: &[(&str, &str)]) -> (usize, Option<String>) {
        self.send(items);
        let (mut outs, mut val) = (0, None);
        loop {
            let m = self.next();
            if m.get("out").is_some() {
                outs += 1;
            }
            if let Some(v) = m.get("value").and_then(|v| v.as_str()) {
                val = Some(v.to_string());
            }
            if let Some(Value::List(l)) = m.get("status") {
                if l.iter().any(|x| x.as_str() == Some("done")) {
                    return (outs, val);
                }
            }
        }
    }
    fn clone_session(&mut self) -> String {
        self.send(&[("op", "clone")]);
        loop {
            let m = self.next();
            if let Some(s) = m.get("new-session").and_then(|v| v.as_str()) {
                return s.to_string();
            }
        }
    }
}

fn start(mova: &str, port: u16) -> Child {
    let dir = std::env::temp_dir().join(format!("p2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Command::new(mova)
        .args(["nrepl", "--port", &port.to_string()])
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

fn stop(mut c: Child) {
    let _ = c.kill();
    let _ = c.wait();
}

fn port() -> u16 {
    30000 + (std::process::id() % 20000) as u16
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[(((v.len() - 1) as f64) * q).round() as usize]
}

fn footprint_kb(pid: u32) -> f64 {
    let o = Command::new("footprint").args(["-p", &pid.to_string()]).output().unwrap();
    let t = String::from_utf8_lossy(&o.stdout);
    for l in t.lines() {
        if let Some(i) = l.find("Footprint:") {
            let r: Vec<&str> = l[i + 10..].split_whitespace().collect();
            let n: f64 = r[0].parse().unwrap();
            return match r[1] {
                "KB" => n,
                "MB" => n * 1024.0,
                "GB" => n * 1024.0 * 1024.0,
                u => panic!("unit {u}"),
            };
        }
    }
    panic!("no footprint in: {t}")
}

fn cpu_seconds(pid: u32) -> f64 {
    let o = Command::new("ps").args(["-o", "cputime=", "-p", &pid.to_string()]).output().unwrap();
    let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
    // [[h:]m:]s.cc
    t.split(':').fold(0.0, |a, p| a * 60.0 + p.parse::<f64>().unwrap())
}

fn threads(pid: u32) -> usize {
    let o = Command::new("ps").args(["-M", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).lines().count().saturating_sub(1)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "warm" => {
            let n: usize = a[2].parse().unwrap();
            let c = start(&a[3], port());
            let mut k = Conn::open(port());
            let sess = k.clone_session();
            for (label, session) in [("persistent session", Some(sess.as_str())), ("ephemeral", None)] {
                let mut items = vec![("op", "eval"), ("code", "(+ 1 2)")];
                if let Some(s) = session {
                    items.push(("session", s));
                }
                for _ in 0..2000 {
                    k.call(&items);
                }
                let mut v: Vec<f64> = (0..n)
                    .map(|_| {
                        let t = Instant::now();
                        k.call(&items);
                        t.elapsed().as_secs_f64() * 1e6
                    })
                    .collect();
                let (min, p50, p99) = (v.iter().cloned().fold(f64::MAX, f64::min), pct(&mut v, 0.5), pct(&mut v, 0.99));
                println!("warm eval (+ 1 2), {label:<19} N={n} min={min:.1}us p50={p50:.1}us p99={p99:.1}us");
            }
            stop(c);
        }
        "tool" => {
            let n: usize = a[2].parse().unwrap();
            let c = start(&a[3], port());
            let mut k = Conn::open(port());
            let sess = k.clone_session();
            k.call(&[("op", "eval"), ("code", "(+ 1 2)"), ("session", &sess)]);
            let cases: [(&str, Vec<(&str, &str)>); 5] = [
                ("completions ma", vec![("op", "completions"), ("prefix", "ma"), ("session", &sess)]),
                ("completions clojure.string/", vec![("op", "completions"), ("prefix", "clojure.string/"), ("session", &sess)]),
                ("completions \"\" (all vars)", vec![("op", "completions"), ("prefix", ""), ("session", &sess)]),
                ("lookup map", vec![("op", "lookup"), ("sym", "map"), ("session", &sess)]),
                ("eval (+ 1 2)", vec![("op", "eval"), ("code", "(+ 1 2)"), ("session", &sess)]),
            ];
            for (label, items) in &cases {
                for _ in 0..500 {
                    k.call(items);
                }
                let mut v: Vec<f64> = (0..n)
                    .map(|_| {
                        let t = Instant::now();
                        k.call(items);
                        t.elapsed().as_secs_f64() * 1e6
                    })
                    .collect();
                let (min, p50, p99) = (v.iter().cloned().fold(f64::MAX, f64::min), pct(&mut v, 0.5), pct(&mut v, 0.99));
                println!("{label:<32} N={n} min={min:.1}us p50={p50:.1}us p99={p99:.1}us");
            }
            stop(c);
        }
        "println" => {
            let n: usize = a[2].parse().unwrap();
            let c = start(&a[3], port());
            let mut k = Conn::open(port());
            let sess = k.clone_session();
            k.call(&[("op", "eval"), ("code", "(+ 1 2)"), ("session", &sess)]);
            for round in 0..3 {
                let code = std::env::var("P2_CODE").unwrap_or_else(|_| format!("(dotimes [i {n}] (println \"x\"))"));
                let t = Instant::now();
                let (outs, _) = k.call(&[("op", "eval"), ("code", &code), ("session", &sess)]);
                let s = t.elapsed().as_secs_f64();
                println!("round {round}: {n} println -> {outs} out msgs in {s:.3}s = {:.2}M msg/s", outs as f64 / s / 1e6);
            }
            // what the interpreter alone costs: same loop with output discarded
            let code = format!("(dotimes [i {n}] (with-out-str (println \"x\")))");
            let t = Instant::now();
            k.call(&[("op", "eval"), ("code", &code), ("session", &sess)]);
            println!("interpreter only (with-out-str, no wire): {:.3}s = {:.2}M println/s", t.elapsed().as_secs_f64(), n as f64 / t.elapsed().as_secs_f64() / 1e6);
            stop(c);
        }
        "mem" => {
            let c = start(&a[2], port());
            let pid = c.id();
            let mut k = Conn::open(port());
            let s0 = k.clone_session();
            k.call(&[("op", "eval"), ("code", "(+ 1 2)"), ("session", &s0)]);
            std::thread::sleep(Duration::from_millis(500));
            let base = footprint_kb(pid);
            println!("idle after first eval (1 session): {base:.0} KB, threads {}", threads(pid));
            // extra idle sessions that ran one eval
            let kk = 100;
            let mut ss = vec![];
            for _ in 0..kk {
                let s = k.clone_session();
                k.call(&[("op", "eval"), ("code", "(+ 1 2)"), ("session", &s)]);
                ss.push(s);
            }
            std::thread::sleep(Duration::from_millis(500));
            let f1 = footprint_kb(pid);
            println!("+{kk} sessions, one eval each:        {f1:.0} KB, delta/session {:.1} KB, threads {}", (f1 - base) / kk as f64, threads(pid));
            // second eval on each: does it grow?
            for s in &ss {
                k.call(&[("op", "eval"), ("code", "(inc 1)"), ("session", s)]);
            }
            std::thread::sleep(Duration::from_millis(300));
            let f1b = footprint_kb(pid);
            println!("after a 2nd eval in each:           {f1b:.0} KB, delta/session vs base {:.1} KB", (f1b - base) / kk as f64);
            // close them
            for s in &ss {
                k.send(&[("op", "close"), ("session", s)]);
                loop {
                    let m = k.next();
                    if let Some(Value::List(l)) = m.get("status") {
                        if l.iter().any(|x| x.as_str() == Some("session-closed")) {
                            break;
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
            let f2 = footprint_kb(pid);
            println!("after closing them:                 {f2:.0} KB, threads {}", threads(pid));
            // soak: 100k evals in one session (a def each 10th, so the globals churn)
            let t = Instant::now();
            for i in 0..100_000u32 {
                let code = if i % 10 == 0 { format!("(defn f{} [x] (+ x {i}))", i % 50) } else { "(+ 1 2)".to_string() };
                k.call(&[("op", "eval"), ("code", &code), ("session", &s0)]);
                if i == 9_999 || i == 49_999 || i == 99_999 {
                    println!("after {:>6} evals in one session:    {:.0} KB  ({:.1}s)", i + 1, footprint_kb(pid), t.elapsed().as_secs_f64());
                }
            }
            // 100k evals with unique code (source registry, symbols)
            let t = Instant::now();
            for i in 0..100_000u32 {
                let code = format!("(+ {i} 1)");
                k.call(&[("op", "eval"), ("code", &code), ("session", &s0)]);
                if i == 49_999 || i == 99_999 {
                    println!("after {:>6} unique-code evals:       {:.0} KB  ({:.1}s)", i + 1, footprint_kb(pid), t.elapsed().as_secs_f64());
                }
            }
            // 100k ephemeral evals
            for i in 0..100_000u32 {
                let code = format!("(+ {} 1)", i % 100);
                k.call(&[("op", "eval"), ("code", &code)]);
                if i == 99_999 {
                    println!("after 100000 ephemeral evals:       {:.0} KB", footprint_kb(pid));
                }
            }
            // 1M println through the session, then settle
            k.call(&[("op", "eval"), ("code", "(dotimes [i 1000000] (println \"x\"))"), ("session", &s0)]);
            std::thread::sleep(Duration::from_millis(500));
            println!("after 1M println:                   {:.0} KB", footprint_kb(pid));
            stop(c);
        }
        "soak" => {
            // 10 sessions, 100k evals round robin. `redefn`: every 10th eval re-defines one of 50 fns
            // (10k re-defns), the rest are (+ 1 2). `unique`: every eval is a DIFFERENT defn.
            // PASS = last footprint <= CAP_MB and last-third slope <= SLOPE_MB_PER_10K (least squares
            // over the samples at 70k..100k evals).
            const CAP_MB: f64 = 50.0;
            const SLOPE_MB_PER_10K: f64 = 0.5;
            let which = a.get(3).map(|s| s.as_str()).unwrap_or("both");
            let total: u32 = std::env::var("SOAK_N").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000);
            let variants: Vec<&str> = if which == "both" { vec!["redefn", "unique"] } else { vec![which] };
            let mut all_ok = true;
            for variant in variants {
                let c = start(&a[2], port());
                let pid = c.id();
                let mut k = Conn::open(port());
                let ss: Vec<String> = (0..10).map(|_| k.clone_session()).collect();
                for s in &ss {
                    k.call(&[("op", "eval"), ("code", "(+ 1 2)"), ("session", s)]);
                }
                std::thread::sleep(Duration::from_millis(300));
                let base = footprint_kb(pid) / 1024.0;
                let mut curve: Vec<(u32, f64)> = vec![(0, base)];
                let t = Instant::now();
                for i in 0..total {
                    let code = match variant {
                        "redefn" if i % 10 == 0 => format!("(defn f{} [x] (+ x {i}))", i % 50),
                        "redefn" => "(+ 1 2)".to_string(),
                        _ => format!("(defn g{i} [x] (+ x {i}))"),
                    };
                    k.call(&[("op", "eval"), ("code", &code), ("session", &ss[(i % 10) as usize])]);
                    if (i + 1) % (total / 10) == 0 {
                        curve.push((i + 1, footprint_kb(pid) / 1024.0));
                    }
                }
                let secs = t.elapsed().as_secs_f64();
                println!("soak {variant}: 10 sessions, {total} evals ({secs:.0}s)");
                for (n, mb) in &curve {
                    println!("  {n:>7} evals  {mb:>7.1} MB");
                }
                let pts: Vec<(f64, f64)> = curve.iter().filter(|(n, _)| *n as f64 >= total as f64 * 0.69).map(|(n, mb)| (*n as f64 / (total as f64 / 10.0), *mb)).collect();
                let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / pts.len() as f64, pts.iter().map(|p| p.1).sum::<f64>() / pts.len() as f64);
                let slope = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / pts.iter().map(|p| (p.0 - mx).powi(2)).sum::<f64>() * (10_000.0 / (total as f64 / 10.0));
                let last = curve.last().unwrap().1;
                let ok = last <= CAP_MB && slope <= SLOPE_MB_PER_10K;
                all_ok &= ok;
                println!("  last {last:.1} MB (cap {CAP_MB}), last-third slope {slope:.2} MB per 10k evals (bound {SLOPE_MB_PER_10K}) => {}", if ok { "PASS" } else { "FAIL" });
                stop(c);
            }
            std::process::exit(if all_ok { 0 } else { 1 });
        }
        "intr" => {
            // interrupt latency over the wire: send `interrupt` -> `interrupted` reply, and
            // -> the next eval's value (the session is free again)
            let c = start(&a[2], port());
            let mut k = Conn::open(port());
            let s = k.clone_session();
            k.call(&[("op", "eval"), ("code", "(def prm (promise))"), ("session", &s)]);
            let cases = [("tight loop (loop [] (recur))", "(loop [] (recur))"), ("Thread/sleep", "(Thread/sleep 100000)"), ("@(promise)", "@prm"), ("(read-line)", "(read-line)")];
            for (name, code) in cases {
                let (mut reply, mut freed) = (vec![], vec![]);
                for _ in 0..30 {
                    k.send(&[("op", "eval"), ("code", code), ("session", &s), ("id", "evalid")]);
                    std::thread::sleep(Duration::from_millis(60));
                    let t = Instant::now();
                    k.send(&[("op", "interrupt"), ("session", &s), ("id", "intr")]);
                    k.send(&[("op", "eval"), ("code", "(+ 1 2)"), ("session", &s), ("id", "next")]);
                    let (mut got_reply, mut got_free) = (false, false);
                    while !got_free {
                        let m = k.next();
                        let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if id == "evalid" && !got_reply {
                            if let Some(Value::List(l)) = m.get("status") {
                                if l.iter().any(|x| x.as_str() == Some("interrupted")) {
                                    reply.push(t.elapsed().as_secs_f64() * 1e6);
                                    got_reply = true;
                                }
                            }
                        }
                        if id == "next" && m.get("value").is_some() {
                            freed.push(t.elapsed().as_secs_f64() * 1e6);
                            got_free = true;
                        }
                    }
                    // drain until the `next` done
                    loop {
                        let m = k.next();
                        if m.get("id").and_then(|v| v.as_str()) == Some("next") {
                            if let Some(Value::List(l)) = m.get("status") {
                                if l.iter().any(|x| x.as_str() == Some("done")) { break; }
                            }
                        }
                    }
                }
                println!("{name:<30} interrupt->`interrupted` p50={:.0}us max={:.0}us | interrupt->session free p50={:.0}us p99={:.0}us max={:.0}us", pct(&mut reply.clone(), 0.5), reply.iter().cloned().fold(0.0, f64::max), pct(&mut freed.clone(), 0.5), pct(&mut freed.clone(), 0.99), freed.iter().cloned().fold(0.0, f64::max));
            }
            stop(c);
        }
        "cpu" => {
            let secs: u64 = a[3].parse().unwrap();
            let c = start(&a[2], port());
            let pid = c.id();
            let mut k = Conn::open(port());
            for _ in 0..5 {
                let s = k.clone_session();
                k.call(&[("op", "eval"), ("code", "(+ 1 2)"), ("session", &s)]);
            }
            k.call(&[("op", "eval"), ("code", "(future (Thread/sleep 100) 1)")]);
            // one eval blocked on a promise, another on read-line
            let (bs, rs) = (k.clone_session(), k.clone_session());
            k.call(&[("op", "eval"), ("code", "(def prm (promise))"), ("session", &bs)]);
            k.send(&[("op", "eval"), ("code", "@prm"), ("session", &bs)]);
            k.send(&[("op", "eval"), ("code", "(read-line)"), ("session", &rs)]);
            std::thread::sleep(Duration::from_secs(1));
            let t0 = cpu_seconds(pid);
            std::thread::sleep(Duration::from_secs(secs));
            let t1 = cpu_seconds(pid);
            println!("idle CPU over {secs} s with 5 session threads + 1 worker: {:.2} s (ps granularity 0.01 s), threads {}", t1 - t0, threads(pid));
            stop(c);
        }
        _ => panic!("mode"),
    }
}
