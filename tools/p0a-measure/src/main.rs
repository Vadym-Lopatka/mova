//! P0a timing tool. std::process::Command (posix_spawn on macOS), Instant clock.
//!   p0a-measure floor N cmd [args..]       spawn->exit
//!   p0a-measure ab N cmdA -- cmdB          interleaved spawn->exit (single-word cmds with args after)
//!   p0a-measure start N mova               spawn->banner / connect / describe / eval
//!   p0a-measure warm N mova                warm round trips over one connection
//!   p0a-measure idle mova                  memory after boot
use mova_nrepl::bencode::{decode, encode, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn stats(name: &str, mut v: Vec<f64>, unit: &str) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let p = |q: f64| v[(((n - 1) as f64) * q).round() as usize];
    println!(
        "{name:<34} N={n:<6} min={:.3}{unit} p50={:.3}{unit} p99={:.3}{unit} max={:.3}{unit}",
        v[0], p(0.5), p(0.99), v[n - 1]
    );
}
fn ms(d: Duration) -> f64 { d.as_secs_f64() * 1e3 }
fn us(d: Duration) -> f64 { d.as_secs_f64() * 1e6 }

fn spawn_exit(cmd: &[String]) -> f64 {
    let t = Instant::now();
    let mut c = Command::new(&cmd[0]).args(&cmd[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    c.wait().unwrap();
    ms(t.elapsed())
}

fn req(op: &str, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut items = vec![("op", Value::str(op)), ("id", Value::str("1"))];
    for (k, v) in extra { items.push((k, Value::str(v))); }
    let m = Value::Dict(items.into_iter().map(|(k, v)| (k.as_bytes().to_vec(), v)).collect());
    let mut o = Vec::new();
    encode(&m, &mut o);
    o
}

/// Reads responses until one has a `status` containing "done". Returns the value string if any.
fn read_until_done(s: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
    let mut val = None;
    let mut tmp = [0u8; 8192];
    loop {
        while let Ok(Some((m, used))) = decode(buf) {
            buf.drain(..used);
            if let Some(v) = m.get("value").and_then(|v| v.as_str()) { val = Some(v.to_string()); }
            if let Some(Value::List(l)) = m.get("status") {
                if l.iter().any(|x| x.as_str() == Some("done")) { return val; }
            }
        }
        let n = s.read(&mut tmp).unwrap();
        assert!(n > 0, "eof");
        buf.extend_from_slice(&tmp[..n]);
    }
}


/// Sends one eval; returns "v:<value>" or "e:<err>" (first of each) joined.
fn eval_full(s: &mut TcpStream, buf: &mut Vec<u8>, code: &str) -> String {
    s.write_all(&req("eval", &[("code", code)])).unwrap();
    let mut out = String::new();
    let mut tmp = [0u8; 8192];
    loop {
        while let Ok(Some((m, used))) = decode(buf) {
            buf.drain(..used);
            if let Some(v) = m.get("value").and_then(|v| v.as_str()) { out.push_str(&format!("v:{v}")); }
            if let Some(v) = m.get("err").and_then(|v| v.as_str()) { out.push_str(&format!("e:{v}")); }
            if let Some(Value::List(l)) = m.get("status") {
                if l.iter().any(|x| x.as_str() == Some("done")) { return out; }
            }
        }
        let n = s.read(&mut tmp).unwrap();
        assert!(n > 0, "eof");
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn connect_wait(port: u16) -> TcpStream {
    let t = Instant::now();
    loop {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) { s.set_nodelay(true).unwrap(); return s; }
        if t.elapsed() > Duration::from_secs(5) { panic!("connect timeout"); }
    }
}

const CASES: &[&str] = &[
    "(+ 1 2)", "(map inc [1 2 3])", "(doall (map inc (range 5)))", "(take 5 (iterate #(* 2 %) 1))",
    "(defmacro twice [x] `(do ~x ~x)) (twice (+ 1 1))",
    "(let [{:keys [a b] :or {b 9}} {:a 1}] [a b])", "(let [[x & more] [1 2 3]] [x more])",
    "(reduce + (range 1000))", "(str \"a\" 1 :b 'c)", "(clojure.string/upper-case \"hello\")",
    "(require '[clojure.string :as s]) (s/join \",\" [1 2 3])", "(require '[clojure.set :as st]) (st/union #{1} #{2})",
    "(first (filter even? (range 1 100)))", "(into {} (map (fn [x] [x (* x x)]) (range 4)))",
    "(frequencies \"mississippi\")", "(group-by odd? (range 6))", "(->> (range 10) (filter odd?) (map #(* % %)) (reduce +))",
    "(doc map)", "(/ 1 0)", "(nth [1 2] 5)", "(throw (ex-info \"boom\" {:a 1}))", "undefined-sym",
    "(try (/ 1 0) (catch Exception e :caught))", "(defn fact [n] (if (< n 2) 1 (* n (fact (dec n))))) (fact 20)",
    "(fact 25)", "(def x (atom 0)) (swap! x + 5) @x", "(format \"%05.2f\" 3.14159)", "(pr-str {:a [1 2 {:b #{3}}]})",
    "(subs \"hello\" 1 3)", "(re-find #\"\\d+\" \"abc123\")", "(sort-by - [3 1 2])", "(apply max [1 5 3])",
    "(require '[clojure.core.async :as a]) (let [c (a/chan 1)] (a/>!! c 42) (a/<!! c))",
    "(require '[clojure.core.async :as a]) (let [c (a/chan)] (a/go (a/>! c :hi)) (a/<!! c))",
    "(require '[clojure.core.async :as a]) (a/<!! (a/into [] (a/to-chan! [1 2 3])))",
    "(loop [i 0 acc []] (if (< i 5) (recur (inc i) (conj acc i)) acc))", "(case 3 1 :a 3 :c :z)",
    "(cond-> 1 true inc false (* 100))", "(mapv #(vector %1 %2) [1 2] [:a :b])", "(partition 2 (range 7))",
    "(let [f (fn [& xs] (count xs))] (f 1 2 3))", "(gensym)", "(keyword \"a\" \"b\")", "(type 1.5)", "(bigint 123456789012345678901234567890)",
    "(ns foo.bar) (defn hi [] :hi) (hi)", "(str (ns-name *ns*))", "(range 3)", "(lazy-seq [1 2])", "(vec (take 3 (cycle [1 2])))",
];

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("p0a-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

thread_local! { static EXTRA: std::cell::RefCell<Vec<String>> = Default::default(); static XDG: std::cell::RefCell<Option<std::path::PathBuf>> = Default::default(); }

fn start_server(mova: &str, port: u16, dir: &std::path::Path, stdout: Stdio) -> (Instant, Child) {
    start_server2(mova, port, dir, stdout, Stdio::null(), &[])
}

fn start_server2(mova: &str, port: u16, dir: &std::path::Path, stdout: Stdio, stderr: Stdio, more: &[&str]) -> (Instant, Child) {
    let extra: Vec<String> = EXTRA.with(|e| e.borrow().clone());
    let xdg = XDG.with(|x| x.borrow().clone());
    let mut cmd = Command::new(mova);
    cmd.args(["nrepl", "--port", &port.to_string()]).args(&extra).args(more).current_dir(dir).stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    if let Some(x) = xdg { cmd.env("XDG_CACHE_HOME", x); }
    let t = Instant::now();
    let c = cmd.spawn().unwrap();
    (t, c)
}

fn term(mut c: Child) {
    unsafe { libc_kill(c.id() as i32, 15) };
    let _ = c.wait();
}
extern "C" { #[link_name = "kill"] fn libc_kill(pid: i32, sig: i32) -> i32; }

fn main() {
    let mut a: Vec<String> = std::env::args().collect();
    // trailing `-- args..` = extra args for `mova nrepl` (only for start/warm/idle/inproc)
    if let (Some(sep), true) = (a.iter().position(|x| x == "--"), a.get(1).is_some_and(|m| m != "ab")) {
        EXTRA.with(|e| *e.borrow_mut() = a[sep + 1..].to_vec());
        a.truncate(sep);
    }
    match a[1].as_str() {
        "floor" => {
            let n: usize = a[2].parse().unwrap();
            let cmd = &a[3..];
            for _ in 0..5 { spawn_exit(cmd); }
            stats(&format!("spawn->exit {}", cmd.join(" ")), (0..n).map(|_| spawn_exit(cmd)).collect(), "ms");
        }
        "ab" => {
            // ab N A... -- B...
            let n: usize = a[2].parse().unwrap();
            let sep = a.iter().position(|x| x == "--").unwrap();
            let (ca, cb) = (&a[3..sep], &a[sep + 1..]);
            let (mut va, mut vb) = (vec![], vec![]);
            for _ in 0..5 { spawn_exit(ca); spawn_exit(cb); }
            for _ in 0..n { va.push(spawn_exit(ca)); vb.push(spawn_exit(cb)); }
            stats(&format!("A {}", ca.join(" ")), va, "ms");
            stats(&format!("B {}", cb.join(" ")), vb, "ms");
        }
        "start" => {
            // interleaved variants: normal boot / image warm cache / image cold (empty cache each run)
            let n: usize = a[2].parse().unwrap();
            let mova = &a[3];
            let dir = tmpdir("start");
            let warm = dir.join("xdg-warm");
            // populate warm cache once
            XDG.with(|x| *x.borrow_mut() = Some(warm.clone()));
            { let (t, c) = start_server(mova, 0, &dir, Stdio::null()); let _ = t; std::thread::sleep(Duration::from_millis(300)); term(c); }
            let names = ["normal", "image-warm", "image-cold"];
            let mut res: Vec<[Vec<f64>; 4]> = (0..3).map(|_| [vec![], vec![], vec![], vec![]]).collect();
            for i in 0..n + 3 {
                for v in 0..3 {
                    let cold = dir.join(format!("xdg-cold-{v}"));
                    let _ = std::fs::remove_dir_all(&cold);
                    XDG.with(|x| *x.borrow_mut() = Some(if v == 2 { cold.clone() } else { warm.clone() }));
                    let more: &[&str] = if v == 0 { &["--no-core-image"] } else { &[] };
                    // banner via pipe
                    let (t, mut c) = start_server2(mova, 0, &dir, Stdio::piped(), Stdio::null(), more);
                    let mut line = String::new();
                    BufReader::new(c.stdout.take().unwrap()).read_line(&mut line).unwrap();
                    let tb = ms(t.elapsed());
                    term(c);
                    // fixed port: connect, describe, eval
                    let port = 30000 + ((std::process::id() as usize * 31 + (i * 3 + v) * 7) % 20000) as u16;
                    let (t, c) = start_server2(mova, port, &dir, Stdio::null(), Stdio::null(), more);
                    let mut s = loop {
                        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) { break s; }
                        if t.elapsed() > Duration::from_secs(5) { panic!("connect timeout"); }
                    };
                    let tc = ms(t.elapsed());
                    s.set_nodelay(true).unwrap();
                    let mut buf = Vec::new();
                    s.write_all(&req("describe", &[])).unwrap();
                    read_until_done(&mut s, &mut buf);
                    let td = ms(t.elapsed());
                    s.write_all(&req("eval", &[("code", "(+ 1 2)")])).unwrap();
                    let val = read_until_done(&mut s, &mut buf);
                    let te = ms(t.elapsed());
                    assert_eq!(val.as_deref(), Some("3"));
                    drop(s);
                    term(c);
                    if i >= 3 { let r = &mut res[v]; r[0].push(tb); r[1].push(tc); r[2].push(td); r[3].push(te); }
                    let _ = std::fs::remove_dir_all(&cold);
                }
            }
            for v in 0..3 {
                let r = std::mem::take(&mut res[v]);
                let [b, c, d, e] = r;
                stats(&format!("[{}] spawn->banner (pipe)", names[v]), b, "ms");
                stats(&format!("[{}] spawn->connect", names[v]), c, "ms");
                stats(&format!("[{}] spawn->describe", names[v]), d, "ms");
                stats(&format!("[{}] spawn->eval (+ 1 2)=3", names[v]), e, "ms");
            }
            println!("port file left behind after SIGTERM: {}", dir.join(".nrepl-port").exists());
            let _ = std::fs::remove_dir_all(&dir);
        }
        "inproc" => {
            // in-process times from `--verbose` stderr
            let n: usize = a[2].parse().unwrap();
            let mova = &a[3];
            let dir = tmpdir("inproc");
            let warm = dir.join("xdg-warm");
            XDG.with(|x| *x.borrow_mut() = Some(warm.clone()));
            { let (_t, c) = start_server(mova, 0, &dir, Stdio::null()); std::thread::sleep(Duration::from_millis(300)); term(c); }
            let names = ["normal", "image-warm", "image-cold"];
            let keys = ["listening", "banner", "port file", "interp ready", "natives", "restore(+check)", "save-encode"];
            let mut res: Vec<Vec<Vec<f64>>> = (0..3).map(|_| (0..keys.len()).map(|_| vec![]).collect()).collect();
            let num = |l: &str, key: &str| -> Option<f64> {
                let i = l.find(key)? + key.len();
                l[i..].trim_start().split(|c: char| !(c.is_ascii_digit() || c == '.')).next()?.parse().ok()
            };
            for i in 0..n + 3 {
                for v in 0..3 {
                    let cold = dir.join(format!("xdg-cold-{v}"));
                    let _ = std::fs::remove_dir_all(&cold);
                    XDG.with(|x| *x.borrow_mut() = Some(if v == 2 { cold.clone() } else { warm.clone() }));
                    let more: &[&str] = if v == 0 { &["--no-core-image", "--verbose"] } else { &["--verbose"] };
                    let (_t, mut c) = start_server2(mova, 0, &dir, Stdio::null(), Stdio::piped(), more);
                    let mut rd = BufReader::new(c.stderr.take().unwrap());
                    let mut vals = vec![f64::NAN; keys.len()];
                    loop {
                        let mut l = String::new();
                        if rd.read_line(&mut l).unwrap() == 0 { break; }
                        if l.contains("main->listening") {
                            vals[0] = num(&l, "main->listening").unwrap();
                            vals[1] = num(&l, "->banner flushed").unwrap();
                            vals[2] = num(&l, "->port file").unwrap();
                        }
                        if l.contains("interpreter ready") {
                            vals[3] = num(&l, "main->interpreter ready").unwrap();
                            vals[4] = num(&l, "natives").unwrap_or(f64::NAN);
                            vals[5] = num(&l, "restore").unwrap_or(f64::NAN);
                            vals[6] = num(&l, "save-encode").unwrap_or(f64::NAN);
                            break;
                        }
                    }
                    term(c);
                    if i >= 3 { for k in 0..keys.len() { if !vals[k].is_nan() { res[v][k].push(vals[k]); } } }
                    let _ = std::fs::remove_dir_all(&cold);
                }
            }
            for v in 0..3 { for k in 0..keys.len() {
                let x = std::mem::take(&mut res[v][k]);
                if !x.is_empty() { stats(&format!("[{}] main->{}", names[v], keys[k]), x, "us"); }
            } }
            let _ = std::fs::remove_dir_all(&dir);
        }
        "correct" => {
            // normal-boot server vs image-boot server: byte-identical replies
            let mova = &a[2];
            let dir = tmpdir("correct");
            let xdg = dir.join("xdg");
            XDG.with(|x| *x.borrow_mut() = Some(xdg.clone()));
            let run = |more: &[&str], port: u16, warm_first: bool| -> Vec<String> {
                if warm_first { let (_t, c) = start_server(mova, 0, &dir, Stdio::null()); std::thread::sleep(Duration::from_millis(300)); term(c); }
                let (_t, c) = start_server2(mova, port, &dir, Stdio::null(), Stdio::null(), more);
                let mut s = connect_wait(port);
                let mut buf = Vec::new();
                let r: Vec<String> = CASES.iter().map(|c| eval_full(&mut s, &mut buf, c)).collect();
                term(c);
                r
            };
            let base = 41000 + (std::process::id() % 1000) as u16 * 3;
            let normal = run(&["--no-core-image"], base, false);
            let first = run(&[], base + 1, false); // cold cache: boots normally + saves
            std::thread::sleep(Duration::from_millis(200));
            let image = run(&[], base + 2, false); // warm cache: restored
            let mut diff = 0;
            for (i, c) in CASES.iter().enumerate() {
                let same = normal[i] == image[i] && normal[i] == first[i];
                if !same { diff += 1; }
                println!("{} {:<60} {}", if same { "same" } else { "DIFF" }, c.chars().take(60).collect::<String>(), normal[i].chars().take(70).collect::<String>().replace('\n', "\\n"));
                if !same { println!("   first: {:?}\n   image: {:?}", first[i], image[i]); }
            }
            println!("cases {} differing {}", CASES.len(), diff);
            let _ = std::fs::remove_dir_all(&dir);
        }
        "fallback" => {
            // fallback <mova> [<mova-other-build>]
            let mova = &a[2];
            let other = a.get(3);
            let dir = tmpdir("fallback");
            let xdg = dir.join("xdg");
            XDG.with(|x| *x.borrow_mut() = Some(xdg.clone()));
            let image_files = |d: &std::path::Path| -> Vec<std::path::PathBuf> {
                std::fs::read_dir(d.join("mova")).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "img")).collect()).unwrap_or_default()
            };
            // one run: returns (outcome line, eval results ok?, stderr text)
            let go = |bin: &str, port: u16| -> (String, bool, String) {
                let (_t, mut c) = start_server2(bin, port, &dir, Stdio::null(), Stdio::piped(), &["--verbose"]);
                let mut s = connect_wait(port);
                let mut buf = Vec::new();
                let mut ok = eval_full(&mut s, &mut buf, "(+ 1 2)") == "v:3";
                ok &= eval_full(&mut s, &mut buf, "(map inc [1 2 3])") == "v:(2 3 4)";
                ok &= eval_full(&mut s, &mut buf, "(defn sq [x] (* x x)) (sq 12)") == "v:144";
                std::thread::sleep(Duration::from_millis(250));
                let mut stderr = c.stderr.take().unwrap();
                term(c);
                let mut txt = String::new();
                stderr.read_to_string(&mut txt).unwrap();
                let line = txt.lines().find(|l| l.contains("interpreter ready")).unwrap_or("").to_string();
                let oc = line.split("outcome ").nth(1).and_then(|r| r.chars().next()).map(|c| c.to_string()).unwrap_or("?".into());
                (oc, ok, txt)
            };
            let mut port = 42000 + (std::process::id() % 1000) as u16 * 5;
            let mut next = || { port += 1; port };
            let (oc, ok, _) = go(mova, next());
            println!("populate:                outcome={oc} (2=miss+saved) evals_ok={ok} files={}", image_files(&xdg).len());
            let img = image_files(&xdg).pop().expect("no image written");
            let good = std::fs::read(&img).unwrap();
            println!("image size {} bytes", good.len());
            let (oc, ok, _) = go(mova, next());
            println!("warm reuse:              outcome={oc} (1=restored) evals_ok={ok}");
            let mut cases: Vec<(&str, Vec<u8>)> = vec![];
            cases.push(("truncated (half)", good[..good.len() / 2].to_vec()));
            cases.push(("truncated (-1 byte)", good[..good.len() - 1].to_vec()));
            cases.push(("truncated (first 12 bytes)", good[..12].to_vec()));
            let mut f = good.clone(); let m = f.len() / 2; f[m] ^= 0x01; cases.push(("flipped byte (middle, bit 0)", f));
            let mut f = good.clone(); let m = f.len() - 30; f[m] ^= 0xff; cases.push(("flipped byte (near end)", f));
            let mut f = good.clone(); f[100] ^= 0x80; cases.push(("flipped byte (offset 100)", f));
            let mut f = good.clone(); f.extend_from_slice(b"junk"); cases.push(("appended junk", f));
            cases.push(("zero-length", vec![]));
            cases.push(("garbage text", b"hello world, not an image at all, just text".to_vec()));
            cases.push(("magic only", b"MOVAIMG1".to_vec()));
            let mut all_pass = true;
            for (name, bytes) in cases {
                std::fs::write(&img, &bytes).unwrap();
                let (oc, ok, txt) = go(mova, next());
                let panicked = txt.contains("panicked");
                let healed = std::fs::read(&img).map(|b| b == good).unwrap_or(false);
                let pass = oc == "2" && ok && !panicked;
                all_pass &= pass;
                println!("{:<32} outcome={oc} evals_ok={ok} panicked={panicked} rewritten_valid={healed}  {}", name, if pass { "PASS" } else { "FAIL" });
                // next start must restore from the healed file
                let (oc2, ok2, _) = go(mova, next());
                println!("   next start after heal:  outcome={oc2} evals_ok={ok2}");
                all_pass &= oc2 == "1" && ok2;
            }
            if let Some(other) = other {
                // image written by another build, put under this build's name
                let ox = dir.join("xdg-other");
                XDG.with(|x| *x.borrow_mut() = Some(ox.clone()));
                let (oc, ok, _) = go(other, next());
                let oimg = image_files(&ox).pop().expect("other build wrote no image");
                println!("other build populate:    outcome={oc} evals_ok={ok} name={}", oimg.file_name().unwrap().to_string_lossy());
                XDG.with(|x| *x.borrow_mut() = Some(xdg.clone()));
                println!("this build's image name: {}", img.file_name().unwrap().to_string_lossy());
                std::fs::copy(&oimg, &img).unwrap();
                let (oc, ok, txt) = go(mova, next());
                let pass = oc == "2" && ok && !txt.contains("panicked");
                all_pass &= pass;
                println!("{:<32} outcome={oc} evals_ok={ok}  {}", "image from different build", if pass { "PASS" } else { "FAIL" });
            }
            println!("ALL FALLBACK CASES: {}", if all_pass { "PASS" } else { "FAIL" });
            let _ = std::fs::remove_dir_all(&dir);
        }
        "race" => {
            // N concurrent first starts on an empty cache: image must end valid, no stray temp files
            let mova = &a[2];
            let n: usize = a[3].parse().unwrap();
            let dir = tmpdir("race");
            let xdg = dir.join("xdg");
            XDG.with(|x| *x.borrow_mut() = Some(xdg.clone()));
            let base = 43000 + (std::process::id() % 500) as u16 * 10;
            let kids: Vec<_> = (0..n).map(|i| start_server2(mova, base + i as u16, &dir, Stdio::null(), Stdio::null(), &[]).1).collect();
            let mut oks = 0;
            for i in 0..n {
                let mut s = connect_wait(base + i as u16);
                let mut buf = Vec::new();
                if eval_full(&mut s, &mut buf, "(+ 1 2)") == "v:3" { oks += 1; }
            }
            std::thread::sleep(Duration::from_millis(300));
            for k in kids { term(k); }
            let files: Vec<String> = std::fs::read_dir(xdg.join("mova")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
            println!("{n} concurrent cold starts: evals ok {oks}/{n}; cache dir: {files:?}");
            XDG.with(|x| *x.borrow_mut() = Some(xdg.clone()));
            let (_t, mut c) = start_server2(mova, base + 99, &dir, Stdio::null(), Stdio::piped(), &["--verbose"]);
            let mut s = connect_wait(base + 99);
            let mut buf = Vec::new();
            let ok = eval_full(&mut s, &mut buf, "(+ 1 2)") == "v:3";
            std::thread::sleep(Duration::from_millis(100));
            let mut e = c.stderr.take().unwrap();
            term(c);
            let mut txt = String::new(); e.read_to_string(&mut txt).unwrap();
            println!("next start: eval ok {ok}; {}", txt.lines().find(|l| l.contains("ready")).unwrap_or("?"));
            let _ = std::fs::remove_dir_all(&dir);
        }
        "warm" => {
            let n: usize = a[2].parse().unwrap();
            let mova = &a[3];
            let dir = tmpdir("warm");
            let port = 30000 + (std::process::id() % 20000) as u16;
            let (t, c) = start_server(mova, port, &dir, Stdio::null());
            let mut s = loop {
                if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) { break s; }
                if t.elapsed() > Duration::from_secs(5) { panic!("connect timeout"); }
            };
            s.set_nodelay(true).unwrap();
            let mut buf = Vec::new();
            s.write_all(&req("eval", &[("code", "(+ 1 2)")])).unwrap();
            read_until_done(&mut s, &mut buf);
            for (name, r) in [("describe", req("describe", &[])), ("clone", req("clone", &[])), ("eval (+ 1 2)", req("eval", &[("code", "(+ 1 2)")]))] {
                for _ in 0..500 { s.write_all(&r).unwrap(); read_until_done(&mut s, &mut buf); }
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    let t = Instant::now();
                    s.write_all(&r).unwrap();
                    read_until_done(&mut s, &mut buf);
                    v.push(us(t.elapsed()));
                }
                stats(&format!("warm {name} round trip"), v, "us");
            }
            drop(s);
            term(c);
        }
        "idle" => {
            let mova = &a[2];
            let dir = tmpdir("idle");
            let port = 30000 + (std::process::id() % 20000) as u16;
            let (t, c) = start_server(mova, port, &dir, Stdio::null());
            let mut s = loop {
                if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) { break s; }
                if t.elapsed() > Duration::from_secs(5) { panic!("connect timeout"); }
            };
            let mut buf = Vec::new();
            let pid = c.id().to_string();
            let report = |label: &str| {
                println!("--- {label} (pid {pid})");
                let o = Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().unwrap();
                println!("ps rss KB: {}", String::from_utf8_lossy(&o.stdout).trim());
                let o = Command::new("footprint").args(["-p", &pid]).output().unwrap();
                for l in String::from_utf8_lossy(&o.stdout).lines().filter(|l| l.contains("Footprint") || l.contains("footprint") || l.contains(&pid)) { println!("{l}"); }
            };
            std::thread::sleep(Duration::from_millis(1));
            report("immediately after listen (boot likely still running)");
            s.write_all(&req("eval", &[("code", "(+ 1 2)")])).unwrap();
            read_until_done(&mut s, &mut buf);
            std::thread::sleep(Duration::from_millis(500));
            report("idle after first eval, 500 ms");
            drop(s);
            term(c);
        }
        _ => panic!("mode"),
    }
}
