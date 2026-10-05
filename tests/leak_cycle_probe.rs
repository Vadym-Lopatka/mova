#![cfg(feature = "leak-probe")]
//! fix/closure-env-cycles Step 1: red assertion tests for the documented
//! Closure⇄Env Arc-cycle leak.
//!
//! Each test warms up an `Interp` with a scenario, snapshots the four
//! process-global counters in `mova::internal::leak_probe`, runs the
//! scenario 10,000 more times, and asserts `leaked_closures == 0`.
//!
//! Three scenarios (`letfn_mutual`, `let_plain_nonrecursive`,
//! `loop_bound_closure`) bind a tree-walked closure into the SAME frame it
//! captures -- `Closure.env` points at the frame, and the frame's `vars`
//! map holds a `Value::Fn` back to the closure -- a strong-reference cycle
//! `Arc` can never reclaim on its own. Those three assertions MUST FAIL
//! today: that is the whole point of this file, landing the gate red
//! before the fix.
//!
//! Three controls (`fn_immediate_unbound`, `named_fn_self_recursion`,
//! `compiled_selfref_control`) never form that cycle -- an immediately
//! applied fn is never bound into its own frame, a named self-recursive fn
//! captures itself via a dedicated self-ref slot the tree-walker resolves
//! without a frame binding, and the compiled tier's `SelfRef` capture kind
//! is the same story one tier down. Those three MUST PASS today.
//!
//! MUST run with `--test-threads=1`: the counters are process-global
//! statics, so concurrent tests would corrupt each other's deltas.

use mova::internal::leak_probe::{CLOSURE_CREATED, CLOSURE_DROPPED, FRAME_CREATED, FRAME_DROPPED};
use mova::internal::Interp;
use std::sync::atomic::Ordering::SeqCst;

const ITERS: u64 = 10_000;

struct Snapshot {
    closures_created: u64,
    closures_dropped: u64,
    frames_created: u64,
    frames_dropped: u64,
}

fn snapshot() -> Snapshot {
    Snapshot {
        closures_created: CLOSURE_CREATED.load(SeqCst),
        closures_dropped: CLOSURE_DROPPED.load(SeqCst),
        frames_created: FRAME_CREATED.load(SeqCst),
        frames_dropped: FRAME_DROPPED.load(SeqCst),
    }
}

#[allow(dead_code)] // f_* / c_dropped fields: printed by `report`, not read by every assert
struct Deltas {
    c_created: u64,
    c_dropped: u64,
    c_leaked: u64,
    f_created: u64,
    f_dropped: u64,
    f_leaked: u64,
}

fn report(name: &str, before: &Snapshot, after: &Snapshot) -> Deltas {
    let c_created = after.closures_created - before.closures_created;
    let c_dropped = after.closures_dropped - before.closures_dropped;
    let c_leaked = c_created - c_dropped;
    let f_created = after.frames_created - before.frames_created;
    let f_dropped = after.frames_dropped - before.frames_dropped;
    let f_leaked = f_created - f_dropped;
    println!(
        "PROBE {name}: closures created={c_created} dropped={c_dropped} leaked={c_leaked} | frames created={f_created} dropped={f_dropped} leaked={f_leaked}"
    );
    Deltas {
        c_created,
        c_dropped,
        c_leaked,
        f_created,
        f_dropped,
        f_leaked,
    }
}

fn run_probe(name: &str, src: &str) -> Deltas {
    let mut interp = Interp::new();
    interp.eval_str("t", src).unwrap();
    let before = snapshot();
    for _ in 0..ITERS {
        interp.eval_str("t", src).unwrap();
    }
    let after = snapshot();
    report(name, &before, &after)
}

#[test]
fn letfn_mutual() {
    let d = run_probe(
        "letfn_mutual",
        "(letfn [(even?* [n] (if (zero? n) true (odd?* (dec n)))) (odd?* [n] (if (zero? n) false (even?* (dec n))))] (even?* 10))",
    );
    assert_eq!(
        d.c_leaked, 0,
        "leaked {} of {} closures: closure<->env Arc cycle not reclaimed",
        d.c_leaked, d.c_created
    );
}

#[test]
fn let_plain_nonrecursive() {
    let d = run_probe("let_plain_nonrecursive", "(let [f (fn [x] (inc x))] (f 1))");
    assert_eq!(
        d.c_leaked, 0,
        "leaked {} of {} closures: closure<->env Arc cycle not reclaimed",
        d.c_leaked, d.c_created
    );
}

#[test]
fn fn_immediate_unbound() {
    let d = run_probe("fn_immediate_unbound", "((fn [x] (inc x)) 1)");
    assert_eq!(
        d.c_leaked, 0,
        "leaked {} of {} closures: closure<->env Arc cycle not reclaimed",
        d.c_leaked, d.c_created
    );
}

#[test]
fn named_fn_self_recursion() {
    let d = run_probe(
        "named_fn_self_recursion",
        "((fn me [n] (if (zero? n) 0 (me (dec n)))) 3)",
    );
    assert_eq!(
        d.c_leaked, 0,
        "leaked {} of {} closures: closure<->env Arc cycle not reclaimed",
        d.c_leaked, d.c_created
    );
}

#[test]
fn loop_bound_closure() {
    let d = run_probe("loop_bound_closure", "(loop [f (fn [x] x)] (f 1))");
    assert_eq!(
        d.c_leaked, 0,
        "leaked {} of {} closures: closure<->env Arc cycle not reclaimed",
        d.c_leaked, d.c_created
    );
}

#[test]
fn compiled_selfref_control() {
    let mut interp = Interp::new();
    interp
        .eval_str(
            "t",
            "(defn mk [] (fn down [n] (if (zero? n) 0 (down (dec n)))))",
        )
        .unwrap();
    let src = "((mk) 5)";
    interp.eval_str("t", src).unwrap();
    let before = snapshot();
    for _ in 0..ITERS {
        interp.eval_str("t", src).unwrap();
    }
    let after = snapshot();
    let d = report("compiled_selfref_control", &before, &after);
    assert_eq!(
        d.c_leaked, 0,
        "leaked {} of {} closures: closure<->env Arc cycle not reclaimed",
        d.c_leaked, d.c_created
    );
}

/// The island check must never clear a frame a live closure still reads.
/// Both forms below let a closure ESCAPE its defining `let` frame and then
/// call it: the frame's `vars` must still hold the bindings the body
/// resolves against.
#[test]
fn escaped_closure_still_works() {
    let mut interp = Interp::new();

    // Inner `g` escapes its own frame as the outer fn's return value.
    let v = interp
        .eval_str("t", "(let [f (fn [] (let [g (fn [] 42)] g))] ((f)))")
        .unwrap();
    assert!(
        matches!(v, mova::internal::Value::Int(42)),
        "escaped nested closure returned {v:?}, expected 42"
    );

    // `f` escapes as the `let`'s own result and still resolves `x`, which
    // lives in the very frame the guard retired.
    let v2 = interp.eval_str("t", "((let [x 7 f (fn [] x)] f))").unwrap();
    assert!(
        matches!(v2, mova::internal::Value::Int(7)),
        "escaped closure lost its captured binding: got {v2:?}, expected 7"
    );
}

/// A genuinely escaped cycle is left ALONE -- it still leaks, which is the
/// documented conservative behavior -- and must keep working. Asserts
/// correctness only; the counters are printed for the record.
#[test]
fn escaped_cycle_still_leaks_safely() {
    let mut interp = Interp::new();
    let src = "(let [f (letfn [(e [n] (if (zero? n) true (o (dec n)))) (o [n] (if (zero? n) false (e (dec n))))] e)] (f 4))";
    interp.eval_str("t", src).unwrap();
    let before = snapshot();
    let v = interp.eval_str("t", src).unwrap();
    let after = snapshot();
    report("escaped_cycle_still_leaks_safely", &before, &after);
    assert!(
        matches!(v, mova::internal::Value::Bool(true)),
        "escaped mutual-recursion closure returned {v:?}, expected true"
    );
}

/// THE structural test of the recursive-binding-group landing: a `letfn`
/// inside a `defn`, run 10 000 times, leaking nothing AND allocating no
/// tree-walked frame at all.
///
/// `frames created == 0` is the load-bearing half. It says the enclosing fn
/// genuinely COMPILED -- no deferred-read poison bail, so no tree-walked
/// `let` frame, so no `Closure.env -> frame -> vars[name] -> Closure` cycle
/// to reclaim in the first place. Asserting only `leaked == 0` would pass
/// just as well if the fn had bailed and `Env::break_frame_cycles` had
/// cleaned up after it, which is a different (and weaker) claim.
#[test]
fn compiled_letfn_in_defn_no_leak_no_frames() {
    let mut interp = Interp::new();
    interp
        .eval_str(
            "t",
            "(defn lf [] (letfn [(e [n] (if (zero? n) true (o (dec n)))) \
                                 (o [n] (if (zero? n) false (e (dec n))))] \
                           (e 10)))",
        )
        .unwrap();
    let src = "(lf)";
    interp.eval_str("t", src).unwrap();
    let before = snapshot();
    for _ in 0..ITERS {
        interp.eval_str("t", src).unwrap();
    }
    let after = snapshot();
    let d = report("compiled_letfn_in_defn_no_leak_no_frames", &before, &after);
    assert_eq!(
        d.c_leaked, 0,
        "leaked {} of {} closures: a compiled recursive binding group must not cycle",
        d.c_leaked, d.c_created
    );
    assert_eq!(
        d.f_created, 0,
        "created {} env frames: the enclosing fn did not compile -- the letfn bailed to the tree-walker",
        d.f_created
    );
}

/// The case the island-break commit could NOT cover: the member ESCAPES its
/// defining scope, so `Env::break_frame_cycles`' "nothing outside the frame
/// holds this closure" test correctly declines to retire the frame -- and
/// the tree-walked shape therefore still leaks (see
/// `escaped_cycle_still_leaks_safely`). Compiled as a recursive binding
/// group there is no frame and no cycle at all: the escaped member holds
/// the group strongly, the group holds its members only weakly.
#[test]
fn escaped_letfn_closure_no_leak() {
    let mut interp = Interp::new();
    interp
        .eval_str(
            "t",
            "(defn mk [] (letfn [(e [n] (if (zero? n) true (o (dec n)))) \
                                 (o [n] (if (zero? n) false (e (dec n))))] \
                           e))",
        )
        .unwrap();
    let src = "((mk) 4)";
    interp.eval_str("t", src).unwrap();
    let before = snapshot();
    for _ in 0..ITERS {
        interp.eval_str("t", src).unwrap();
    }
    let after = snapshot();
    let d = report("escaped_letfn_closure_no_leak", &before, &after);
    assert_eq!(
        d.c_leaked, 0,
        "leaked {} of {} closures: an escaped group member must still be reclaimable",
        d.c_leaked, d.c_created
    );
    assert_eq!(
        d.f_created, 0,
        "created {} env frames: the enclosing fn did not compile",
        d.f_created
    );
}

#[test]
fn strong_count_probe() {
    let mut interp = Interp::new();

    let result = interp.eval_str("t", "(let [f (fn [x] x)] f)").unwrap();
    let mova::internal::Value::Fn(c) = result else {
        panic!("expected a Value::Fn, got {result:?}");
    };
    let n = std::sync::Arc::strong_count(&c);
    println!("PROBE strong_count let_bound: {n}");

    let result2 = interp
        .eval_str("t", "(letfn [(e [n] n) (o [n] (e n))] e)")
        .unwrap();
    let mova::internal::Value::Fn(c2) = result2 else {
        panic!("expected a Value::Fn, got {result2:?}");
    };
    let n2 = std::sync::Arc::strong_count(&c2);
    println!("PROBE strong_count letfn_bound: {n2}");
}

/// M-leak helper: `defn` the body so it compiles (kondo's analyzer does),
/// then call it ITERS times.
fn run_compiled(name: &str, body: &str) -> Deltas {
    let mut interp = Interp::new();
    interp.eval_str("t", &format!("(defn step [] {body})")).unwrap();
    interp.eval_str("t", "(step)").unwrap();
    let before = snapshot();
    for _ in 0..ITERS {
        interp.eval_str("t", "(step)").unwrap();
    }
    report(name, &before, &snapshot())
}

/// M-leak: clj-kondo's `:deferred-conditions` shape. `ctx` holds a queue atom
/// and the queue stores `{:ctx ctx}` -- ctx -> atom -> vector -> map -> ctx is
/// a data cycle through a mutable cell; with Arc and no cycle collector it
/// can never drop (JVM GC would reclaim it). Pins the diagnosis: 1 closure
/// (the fn in ctx) leaks per iteration.
#[test]
fn atom_queue_holding_its_ctx_cycle_leaks() {
    let d = run_compiled(
        "atom_queue_holding_its_ctx_cycle_leaks",
        "(let [q (atom []) ctx {:q q :f (fn [] 1)}] (swap! q conj {:ctx ctx}) nil)",
    );
    assert_eq!(d.c_leaked, ITERS, "expected exactly one leaked closure per iteration");
}

/// M-leak fix shape (mova/overlay/clj_kondo/impl/analyzer.mova): the queued
/// ctx drops its back-reference to the queue, so nothing cycles and every
/// closure created in the loop is dropped.
#[test]
fn atom_queue_without_back_ref_no_leak() {
    let d = run_compiled(
        "atom_queue_without_back_ref_no_leak",
        "(let [q (atom []) ctx {:q q :f (fn [] 1)}] (swap! q conj {:ctx (dissoc ctx :q)}) nil)",
    );
    assert!(d.c_created >= ITERS, "closures not created: {}", d.c_created);
    assert_eq!(d.c_leaked, 0, "leaked {} closures", d.c_leaked);
}

// ---- nREPL path: compiled top-level-loop wrapper vs the tree-walker ----

mod nrepl_path {
    use super::*;
    use mova::nrepl::{Config, MovaBackend};
    use mova_nrepl::bencode::{decode, encode, Value};
    use mova_nrepl::{Endpoint, Listeners, Server};
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn eval_wire(s: &mut TcpStream, buf: &mut Vec<u8>, code: &str) -> String {
        let m = Value::Dict(
            [("op", "eval"), ("code", code), ("id", "1")].iter().map(|(k, v)| (k.as_bytes().to_vec(), Value::str(v))).collect(),
        );
        let mut out = Vec::new();
        encode(&m, &mut out);
        s.write_all(&out).unwrap();
        let mut value = String::new();
        loop {
            while let Ok(Some((v, used))) = decode(buf) {
                buf.drain(..used);
                if let Some(x) = v.get("value").and_then(|x| x.as_str()) {
                    value = x.to_string();
                }
                if let Some(Value::List(l)) = v.get("status") {
                    if l.iter().any(|x| x.as_str() == Some("done")) {
                        return value;
                    }
                }
            }
            let mut tmp = [0u8; 65536];
            let n = s.read(&mut tmp).unwrap();
            assert!(n > 0, "eof");
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Runs `code` ITERS times over the wire on one session; returns the leak deltas.
    fn probe(name: &str, code: &str) -> Deltas {
        let l = Listeners::bind(&Endpoint::Tcp { host: "127.0.0.1".into(), port: 0 }).unwrap();
        let port = l.port().unwrap();
        let backend = MovaBackend::new(Config::default());
        let server = Server::new(l, backend.clone(), false).unwrap();
        let handle = server.handle();
        let join = std::thread::spawn(move || server.run().unwrap());
        backend.boot_thread(|| {}).unwrap();
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(30))).unwrap();
        let mut buf = Vec::new();
        eval_wire(&mut s, &mut buf, code);
        let before = snapshot();
        for _ in 0..ITERS {
            eval_wire(&mut s, &mut buf, code);
        }
        let after = snapshot();
        handle.shutdown();
        let _ = join.join();
        report(name, &before, &after)
    }

    // A fn whose body loops: compiled when called. `letfn` binds a closure into
    // the frame it captures (the documented cycle); the loop is what makes the
    // nREPL path wrap the form into a compiled `(fn* [] ...)`.
    const LOOP_FORM: &str = "(loop [i 0] (if (< i 2) (do (letfn [(ev [n] (if (zero? n) true (od (dec n)))) (od [n] (if (zero? n) false (ev (dec n))))] (ev 4)) (recur (inc i))) i))";
    // The same cyclic closure without a loop: stays in the tree-walker.
    const PLAIN_FORM: &str = "(letfn [(ev [n] (if (zero? n) true (od (dec n)))) (od [n] (if (zero? n) false (ev (dec n))))] (ev 4))";

    #[test]
    fn nrepl_toplevel_loop_compiled_wrapper() {
        let d = probe("nrepl_toplevel_loop_compiled", LOOP_FORM);
        assert_eq!(d.c_leaked, 0, "compiled top-level-loop wrapper leaked {} of {} closures", d.c_leaked, d.c_created);
    }

    #[test]
    fn nrepl_plain_form_treewalker() {
        let d = probe("nrepl_plain_treewalker", PLAIN_FORM);
        assert_eq!(d.c_leaked, 0, "tree-walked form leaked {} of {} closures", d.c_leaked, d.c_created);
    }

    // A data cycle through a mutable cell (closure -> env -> atom -> closure):
    // Arc cannot reclaim it in either tier. Pinned so a change in the wrapper
    // shows up: the loop form (compiled wrapper) must leak no MORE than the plain one.
    const ATOM_LOOP: &str = "(loop [i 0] (if (< i 1) (do (let [a (atom nil) f (fn [] a)] (reset! a f)) (recur (inc i))) i))";
    const ATOM_PLAIN: &str = "(let [a (atom nil) f (fn [] a)] (reset! a f))";

    #[test]
    fn nrepl_atom_cycle_wrapper_vs_treewalker() {
        let w = probe("nrepl_atom_cycle_loop_compiled", ATOM_LOOP);
        let t = probe("nrepl_atom_cycle_plain_treewalker", ATOM_PLAIN);
        assert!(w.c_leaked <= t.c_leaked, "wrapper leaked {} closures per {} iters, tree-walker {}", w.c_leaked, ITERS, t.c_leaked);
    }
}
