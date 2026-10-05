//! P0c spike harness: one thread evals, main thread sets the interrupt flag
//! after 50 ms. Reports stopped?, latency (flag set -> eval returns), finally ran?
//!
//! Run: cargo run --release --example p0c_interrupt -- [N] [mode] [filter]
//!   mode: soft | hard | two (soft, then hard 100 ms later)
//! Env: MOVA_JIT=1 etc. are honoured as usual.
use mova::embed::Engine;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn new_engine() -> (Engine, std::sync::Arc<mova::interrupt::Interrupt>) {
    let mut e = Engine::builder().build();
    let intr = {
        let i = e.image_interp_mut();
        i.intr_armed = std::env::var("P0C_UNARMED").is_err();
        i.intr.clone()
    };
    e.eval("(def fin (atom false)) (def ch (chan)) (def prm (promise))").unwrap();
    (e, intr)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let mode = args.get(2).cloned().unwrap_or_else(|| "two".into());
    let filter = args.get(3).cloned().unwrap_or_default();
    if mode == "poison" {
        return poison();
    }
    if mode == "child" {
        return child();
    }
    // (name, code, wrap-in-finally)
    let cases: Vec<(&str, &str, bool)> = vec![
        ("loop-recur-plain", "(loop [] (recur))", false),
        ("loop-recur", "(loop [] (recur))", true),
        ("while-true", "(while true)", true),
        ("numloop-inc", "(loop [i 0] (recur (inc i)))", true),
        ("numloop-inc-plain", "(loop [i 0] (recur (inc i)))", false),
        ("defn-hot-loop", "(defn hot [n] (loop [i 0 a 0] (if (< i n) (recur (inc i) (+ a (* i 2))) a))) (hot 1000000000000)", true),
        ("defn-int-spin", "(defn spin [n] (loop [i 0] (if (< i n) (recur (inc i)) i))) (spin 1000000000000)", true),
        ("defn-selfrecur", "(defn sr [i n] (if (< i n) (recur (inc i) n) i)) (sr 0 1000000000000)", true),
        ("defn-selfrecur-warm", "(defn sw [i n] (if (< i n) (recur (inc i) n) i)) (dotimes [_ 200] (sw 0 10)) (sw 0 1000000000000)", true),
        ("defn-int-spin-warm", "(defn spw [n] (loop [i 0] (if (< i n) (recur (inc i)) i))) (dotimes [_ 200] (spw 10)) (spw 1000000000000)", true),
        ("defn-hot-loop-float", "(defn hotf [n] (loop [i 0.0 a 0.0] (if (< i n) (recur (+ i 1.0) (+ a (* i 0.5))) a))) (hotf 1.0e18)", true),
        ("reduce-range", "(reduce + (range))", true),
        ("count-repeat", "(count (repeat 1))", true),
        ("dorun-iterate", "(dorun (iterate inc 0))", true),
        ("mutual-rec", "(declare pong) (defn ping [n] (pong (inc n))) (defn pong [n] (ping (inc n))) (ping 0)", true),
        ("trampoline-mutual", "(declare po) (defn pi- [n] (fn [] (po (inc n)))) (defn po [n] (fn [] (pi- (inc n)))) (trampoline pi- 0)", true),
        ("mutual-rec-tail", "(defn pa [n] (if (< n 0) n (recur (inc n)))) (pa 0)", true),
        ("thread-sleep", "(Thread/sleep 100000)", true),
        ("sleep-ms", "(sleep-ms 100000)", true),
        ("take-bang", "(<!! ch)", true),
        ("put-bang", "(>!! ch 1)", true),
        ("promise-deref", "@prm", true),
        ("future-deref", "@(future (sleep-ms 100000))", true),
        ("read-line", "(read-line)", true),
        ("sh-sleep", "(sh \"sleep\" \"100\")", true),
        ("locking", "(locking :a (loop [] (recur)))", true),
        ("catch-throwable", "(try (loop [] (recur)) (catch Throwable _ (reset! fin :caught) :caught))", false),
        ("catch-throwable-then-loop", "(try (loop [] (recur)) (catch Throwable _ (loop [] (recur))))", true),
        ("future-child", "(def fch (future (loop [] (recur)))) (loop [] (recur))", true),
        ("go-child", "(def gch (go (loop [] (recur)))) (loop [] (recur))", true),
        ("sort-big", "(count (sort (map (fn [i] (mod (* i 7919) 1000003)) (range 3000000))))", true),
        ("regex-catastrophic", "(re-find #\"^(?=a)(a+)+$\" (str (apply str (repeat 40 \"a\")) \"b\"))", true),
    ];
    println!("# N={n} mode={mode} jit={:?}", std::env::var("MOVA_JIT").ok());
    println!("{:<28} {:<8} {:>9} {:>9} {:>7} {}", "case", "stopped", "min_ms", "med_ms", "finally", "result");
    let (mut eng, mut intr) = new_engine();
    for (name, code, wrap) in cases {
        if !filter.is_empty() && !name.contains(&filter) {
            continue;
        }
        let src = if wrap { format!("(try {code} (finally (reset! fin true)))") } else { code.to_string() };
        let src = if name == "locking" { code.to_string() } else { src };
        let mut lat: Vec<f64> = vec![];
        let mut fin_count = 0;
        let mut last = String::new();
        let mut stuck = false;
        let trials = n;
        for _ in 0..trials {
            eng.eval("(reset! fin false)").unwrap();
            intr.clear();
            let (tx, rx) = mpsc::channel();
            let src2 = src.clone();
            let h = std::thread::Builder::new().stack_size(64 << 20).spawn(move || {
                let r = eng.eval(&src2);
                let s = match &r {
                    Ok(v) => format!("OK {v}"),
                    Err(e) => format!("ERR {e}"),
                };
                let _ = tx.send(Instant::now());
                (eng, s)
            }).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            let t0 = Instant::now();
            match mode.as_str() {
                "hard" => intr.hard(),
                _ => intr.soft(),
            }
            let mut done = rx.recv_timeout(Duration::from_millis(if mode == "two" { 100 } else { 1500 })).ok();
            if done.is_none() && mode == "two" {
                intr.hard();
                done = rx.recv_timeout(Duration::from_millis(1500)).ok();
            }
            match done {
                Some(t1) => {
                    lat.push(t1.duration_since(t0).as_secs_f64() * 1000.0);
                    let (e2, s) = h.join().unwrap();
                    eng = e2;
                    last = s;
                    if let Ok(v) = eng.eval("@fin") {
                        if format!("{v}") != "false" { fin_count += 1; }
                    }
                }
                None => {
                    stuck = true;
                    // leak the stuck thread; fresh engine
                    std::mem::forget(h);
                    let (e2, i2) = new_engine();
                    eng = e2;
                    intr = i2;
                    break;
                }
            }
        }
        if stuck {
            println!("{:<28} {:<8} {:>9} {:>9} {:>7} stuck >1.6s (thread leaked)", name, "NO", "-", "-", "-");
        } else {
            lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let med = lat[lat.len() / 2];
            println!("{:<28} {:<8} {:>9.3} {:>9.3} {:>3}/{:<3} {}", name, "yes", lat[0], med, fin_count, lat.len(), last.chars().take(60).collect::<String>());
        }
    }
    std::process::exit(0);
}

/// Item 6: is the interpreter usable after an interrupt?
fn poison() {
    let (mut eng, mut intr) = new_engine();
    eng.eval("(def ^:dynamic *x* :root) (def lz (range)) (defn f [] :orig) (def lk (atom 0))").unwrap();
    // (interrupted code, follow-up check, expected)
    let cases: Vec<(&str, &str, &str)> = vec![
        ("(loop [] (recur))", "(+ 1 2)", "3"),
        ("(binding [*x* :inner] (loop [] (recur)))", "*x*", ":root"),
        ("(with-redefs [f (fn [] :mock)] (loop [] (recur)))", "(f)", ":orig"),
        ("(dorun lz)", "(vec (take 3 lz))", "[0 1 2]"),
        ("(locking :a (loop [] (recur)))", "(locking :a :got-lock)", ":got-lock"),
        ("(locking :a (Thread/sleep 100000))", "(locking :a :got-lock)", ":got-lock"),
        ("(try (loop [] (recur)) (finally (swap! lk inc)))", "@lk", "1"),
        ("(defn deep [n] (if (zero? n) 0 (inc (deep (dec n))))) (deep 150000)", "(deep 100)", "100"),
        ("(<!! ch)", "(do (>!! (chan 1) 1) (let [c (chan 1)] (>!! c 5) (<!! c)))", "5"),
        ("@(promise)", "@(future 42)", "42"),
        ("(sh \"sleep\" \"100\")", "(:out (sh \"echo\" \"hi\"))", "\"hi\\n\""),
        ("(loop [i 0] (recur (inc i)))", "(loop [i 0] (if (< i 1000000) (recur (inc i)) i))", "1000000"),
    ];
    let mut ok = 0;
    for (code, check, want) in &cases {
        intr.clear();
        let (tx, rx) = mpsc::channel();
        let c = code.to_string();
        let h = std::thread::Builder::new().stack_size(64 << 20).spawn(move || {
            let r = eng.eval(&c);
            let _ = tx.send(());
            (eng, format!("{:?}", r.map(|v| v.to_string()).map_err(|e| e.to_string())))
        }).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        intr.soft();
        if rx.recv_timeout(Duration::from_millis(500)).is_err() {
            intr.hard();
            if rx.recv_timeout(Duration::from_millis(1500)).is_err() {
                println!("STUCK     {code}");
                std::mem::forget(h);
                let (e2, i2) = new_engine();
                eng = e2; intr = i2;
                continue;
            }
        }
        let (e2, r1) = h.join().unwrap();
        eng = e2;
        let got = match eng.eval(check) { Ok(v) => v.to_string(), Err(e) => format!("ERR {e}") };
        let pass = got == *want;
        if pass { ok += 1; }
        println!("{} {}  -> interrupted: {}  | check {} = {} (want {})", if pass { "PASS" } else { "FAIL" }, code, r1.chars().take(50).collect::<String>(), check.chars().take(30).collect::<String>(), got, want);
    }
    println!("{ok}/{} ok", cases.len());
    std::process::exit(0);
}

/// Does a future/go child started by the interrupted eval keep running?
fn child() {
    for (label, spawn) in [("future", "(future (loop [] (swap! cnt inc) (recur)))"), ("go", "(go (loop [] (swap! cnt inc) (recur)))"), ("future-sleep", "(future (loop [] (swap! cnt inc) (sleep-ms 5) (recur)))")] {
        let (mut eng, intr) = new_engine();
        eng.eval("(def cnt (atom 0))").unwrap();
        let src = format!("(def ch1 {spawn}) (loop [] (recur))");
        let h = std::thread::spawn(move || { let r = eng.eval(&src); (eng, r.is_err()) });
        std::thread::sleep(Duration::from_millis(100));
        intr.soft();
        let (mut eng, was_err) = h.join().unwrap();
        let a: i64 = eng.eval("@cnt").unwrap().to_string().parse().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let b: i64 = eng.eval("@cnt").unwrap().to_string().parse().unwrap();
        println!("{label}: parent interrupted={was_err}; child counter {a} -> {b} => child {}", if b > a { "KEEPS RUNNING" } else { "stopped" });
    }
    std::process::exit(0);
}
