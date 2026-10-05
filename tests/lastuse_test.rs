//! Perceus-lite phase 2 (`src/compile/lastuse.rs`): the targeted edges the
//! randomized differential in `differential_test.rs` covers only by luck,
//! and the liveness nets that make a silently-dead analysis fail loudly.
//!
//! Two kinds of test live here, and the difference matters:
//!
//! * **Behavioural** -- run a program and demand the right answer. These
//!   catch a WRONG take: the analysis moved a value out of a slot something
//!   still reads, and a `nil` shows up where a collection should be.
//! * **Shape** -- compile a fn and inspect its `Ir` for `LoadSlotTake`.
//!   These are needed because the interesting *conservative* rules are, by
//!   design, behaviourally invisible: a captured slot could be taken
//!   perfectly safely (capture is by value), so no program can tell whether
//!   the exclusion is in force. Only the IR can. Every shape test comes
//!   with a CONTROL that proves the probe would have seen a take if one had
//!   been emitted -- otherwise "no take found" would pass for the wrong
//!   reason forever.
//!
//! Plus the production counter (`MOVA_MAP_PROBE=1`, section (D)): the
//! analysis's whole purpose is to raise the unique-hit rate, and a version
//! of it that still produced correct answers while never making a receiver
//! unique would pass every other test in the file.

use std::process::Command;

use mova::internal::compile::ir::Ir;
use mova::internal::Interp;
use mova::internal::Value;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Evaluates `src` in a fresh compiled-tier session and returns `pr-str` of
/// the last form's value.
fn compiled(src: &str) -> String {
    run(&mut Interp::new(), src)
}

/// The same program through the tree-walker, which has no slots and so no
/// takes: the reference answer.
fn walked(src: &str) -> String {
    run(&mut Interp::with_all_tiers(false, false, false, false), src)
}

/// The same program compiled but with the analysis off: separates "the
/// compiled tier is wrong" from "the analysis is wrong".
fn no_lastuse(src: &str) -> String {
    run(&mut Interp::with_all_tiers(true, true, false, true), src)
}

fn run(interp: &mut Interp, src: &str) -> String {
    match interp.eval_str("lastuse-test", src) {
        Ok(v) => match interp.realize_deep(&v) {
            Ok(r) => mova::internal::pr_str(&r),
            Err(e) => format!("ERR: {}", e.message),
        },
        Err(e) => format!("ERR: {}", e.message),
    }
}

/// Every evaluation strategy must agree, and agree with `expected`. Stated
/// as one helper because "the compiled answer is wrong" and "the compiled
/// answer is right but differs from the tree-walker" are the same bug.
#[track_caller]
fn agrees(src: &str, expected: &str) {
    let c = compiled(src);
    assert_eq!(c, expected, "compiled tier: {src}");
    assert_eq!(walked(src), expected, "tree-walker: {src}");
    assert_eq!(no_lastuse(src), expected, "lastuse off: {src}");
}

/// The slots a compiled `fn` value takes from, and the slots it merely
/// reads. `src` must end in an expression evaluating to a compiled fn.
///
/// `None` when a PROCESS-WIDE kill switch has turned emission off, since the
/// gate matrix runs this whole suite under each of them in turn and there is
/// then no shape to look at. The behavioural half of every test still runs
/// under those switches -- which is the half the matrix exists to check.
fn takes_and_reads(src: &str) -> Option<(Vec<u16>, Vec<u16>)> {
    for k in ["MOVA_NO_COMPILE", "MOVA_NO_LASTUSE"] {
        if std::env::var(k).is_ok_and(|v| v == "1") {
            return None;
        }
    }
    let mut interp = Interp::new();
    // Lazy tier-up: this asserts a def-time compile outcome without ever
    // calling the fn, so force eager compilation.
    interp.set_eager_compile(true);
    let v = interp
        .eval_str("lastuse-test", src)
        .unwrap_or_else(|e| panic!("{src}: {}", e.message));
    let Value::Fn(rc) = v else {
        panic!("{src}: not a fn");
    };
    let cc = rc
        .compiled
        .compiled()
        .unwrap_or_else(|| panic!("{src}: fn did not compile -- the shape test is vacuous"));
    let mut takes = Vec::new();
    let mut reads = Vec::new();
    for a in &cc.code.arities {
        for ir in &a.body {
            collect(ir, &mut takes, &mut reads);
        }
    }
    takes.sort_unstable();
    reads.sort_unstable();
    Some((takes, reads))
}

/// The shape half of a test: the `(takes, reads)` pair, or an early `return`
/// when a process-wide kill switch has turned emission off. Put the
/// BEHAVIOURAL assertions before it, so they keep running under the gate
/// matrix -- they are what the matrix is checking.
macro_rules! shapes {
    ($src:expr) => {
        match takes_and_reads($src) {
            Some(v) => v,
            None => return,
        }
    };
}

fn collect(ir: &Ir, takes: &mut Vec<u16>, reads: &mut Vec<u16>) {
    let all = |irs: &[Ir], t: &mut Vec<u16>, r: &mut Vec<u16>| {
        for i in irs {
            collect(i, t, r);
        }
    };
    match ir {
        Ir::LoadSlotTake(i) => takes.push(*i),
        Ir::LoadSlot(i) => reads.push(*i),
        Ir::If { test, then, els } => {
            collect(test, takes, reads);
            collect(then, takes, reads);
            if let Some(e) = els {
                collect(e, takes, reads);
            }
        }
        Ir::Do(irs) | Ir::VectorLit(irs) | Ir::SetLit(irs) => all(irs, takes, reads),
        Ir::Let { binds, body } | Ir::Loop { binds, body, .. } => {
            for (_, init) in binds {
                collect(init, takes, reads);
            }
            all(body, takes, reads);
        }
        Ir::Recur { args, .. }
        | Ir::CallGlobal { args, .. }
        | Ir::CallCreationEnv { args, .. }
        | Ir::Intrinsic { args, .. } => all(args, takes, reads),
        Ir::Call { callee, args, .. } => {
            collect(callee, takes, reads);
            all(args, takes, reads);
        }
        Ir::MapLit(kvs) => {
            for (k, v) in kvs {
                collect(k, takes, reads);
                collect(v, takes, reads);
            }
        }
        Ir::Throw { value, .. } => collect(value, takes, reads),
        Ir::Try {
            body,
            catches,
            finally,
        } => {
            all(body, takes, reads);
            for arm in catches {
                all(&arm.body, takes, reads);
            }
            if let Some(f) = finally {
                all(f, takes, reads);
            }
        }
        Ir::Def {
            value: Some(v), ..
        } => collect(v, takes, reads),
        // A nested fn has its own slot space: its takes are not this fn's.
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// S3: conditional paths -- the case a "last textual occurrence" rule fails
// ---------------------------------------------------------------------------

#[test]
fn a_read_on_the_other_if_branch_is_not_a_last_use() {
    // Both reads are last uses ON THEIR OWN PATH, so both may be taken --
    // and a rule that only took the textually-last one would be leaving the
    // `then` branch's win on the table.
    // W4D-TIERS: both expected strings were stale sorted-key order, from
    // before a W4 printer fix made small-map INSERTION order visible;
    // `assoc`ing a new key appends it, so `{:x 1}` + `:a`/`:b` prints
    // `:x` first (measured against the release binary, all three
    // evaluation strategies `agrees` checks).
    agrees("((fn [m c] (if c (assoc m :a 1) (assoc m :b 2))) {:x 1} true)", "{:x 1, :a 1}");
    agrees("((fn [m c] (if c (assoc m :a 1) (assoc m :b 2))) {:x 1} false)", "{:x 1, :b 2}");
    let (takes, reads) = shapes!("(fn [m c] (if c (assoc m :a 1) (assoc m :b 2)))");
    // Slot 0 is `m` (read once per branch, both final on their own path),
    // slot 1 is `c` (the test, read once).
    assert_eq!(takes, vec![0, 0, 1], "both branch reads of `m` should take");
    assert!(reads.is_empty(), "nothing here should stay a clone: {reads:?}");
}

#[test]
fn a_read_after_the_if_suppresses_the_take_on_both_branches() {
    // THE miscompile this test exists for: `m` is read after the `if`, so
    // neither branch's read is final even though each is the last one on its
    // own path. A take in either branch makes `(count m)` see `nil`.
    const SRC: &str = "(fn [m c] (let [x (if c (assoc m :a 1) (assoc m :b 2))] [(count x) (count m)]))";
    agrees(&format!("({SRC} {{:x 1}} true)"), "[2 1]");
    agrees(&format!("({SRC} {{:x 1}} false)"), "[2 1]");
    let (takes, reads) = shapes!(SRC);
    // `m` (slot 0) is read three times: once per branch and once after the
    // `if`. Only the last one is final, so exactly two reads must stay
    // clones -- taking either branch's read would land `nil` in the slot
    // before `(count m)` sees it.
    assert_eq!(
        reads,
        vec![0, 0],
        "both branch reads of `m` must stay clones: reads={reads:?} takes={takes:?}"
    );
    assert_eq!(
        takes.iter().filter(|s| **s == 0).count(),
        1,
        "exactly one read of `m` -- the one after the `if` -- is final: {takes:?}"
    );
}

#[test]
fn sequential_reads_in_one_branch_only_take_the_last() {
    const SRC: &str = "(fn [m] (do (count m) (assoc m :a 1)))";
    // W4D-TIERS: expected string was stale sorted-key order (see the `if`
    // test above for the same fix).
    agrees("((fn [m] (do (count m) (assoc m :a 1))) {:x 1})", "{:x 1, :a 1}");
    let (takes, reads) = shapes!(SRC);
    assert_eq!(takes, vec![0], "only the second read is final");
    assert_eq!(reads, vec![0], "the first read must stay a clone");
}

// ---------------------------------------------------------------------------
// S2: a captured slot is never taken
// ---------------------------------------------------------------------------

#[test]
fn a_slot_a_nested_fn_captures_is_never_taken() {
    // Behaviourally invisible by design (`ir::CaptureSrc` captures by
    // VALUE, so a later take could not disturb the closure), which is
    // exactly why it needs a shape test: the exclusion is defence against a
    // future change to that one `.clone()` in `make_closure`, and nothing
    // else would notice it disappearing.
    const SRC: &str = "(fn [m] (let [g (fn [] (count m))] [(g) (count m)]))";
    agrees("((fn [m] (let [g (fn [] (count m))] [(g) (count m)])) {:x 1})", "[1 1]");
    let (takes, reads) = shapes!(SRC);
    assert!(
        !takes.contains(&0),
        "captured slot 0 was taken: takes={takes:?}"
    );
    assert!(reads.contains(&0), "slot 0 should still be read");
    // THE CONTROL: the identical shape without the capture DOES take, so the
    // assertion above is about the exclusion and not about a probe that
    // never sees anything.
    let (control, _) = shapes!("(fn [m] (count m))");
    assert_eq!(control, vec![0], "control: an uncaptured final read takes");
}

// ---------------------------------------------------------------------------
// S7: the loop-carried receiver -- THE flow-shaped pattern
// ---------------------------------------------------------------------------

/// The accumulator loop, pinned both ways: the answer, and the fact that the
/// receiver's slot really is taken on the `recur` path.
///
/// This is the shape the whole landing exists for. Its take is legal only
/// because `exec_loop`'s back edge rewrites the binding slot from the
/// scratch block before the body can read it again (the kill of safety
/// condition S7); if that reasoning is ever wrong, `m` reads `nil` on the
/// second iteration and the count collapses.
#[test]
fn the_loop_carried_accumulator_takes_its_receiver() {
    const SRC: &str =
        "(fn [n] (loop [m {} i 0] (if (< i n) (recur (assoc m i i) (inc i)) m)))";
    agrees(&format!("({SRC} 4)"), "{0 0, 1 1, 2 2, 3 3}");
    agrees(&format!("({SRC} 0)"), "{}");
    agrees(&format!("({SRC} 1)"), "{0 0}");
    let (takes, _) = shapes!(SRC);
    // Slot layout: n=0 (param), scratch 1, m=2, i=3, loop scratch 4/5.
    assert!(
        takes.contains(&2),
        "the loop-carried receiver was not taken: takes={takes:?}"
    );
}

#[test]
fn the_fn_level_recur_accumulator_takes_its_receiver() {
    // The same back edge, through `run_compiled_body`'s trampoline rather
    // than `exec_loop`: params are rewritten from the scratch block before
    // the body re-runs, so a param's final read of an iteration is final.
    const SRC: &str = "(fn go [m i n] (if (< i n) (recur (assoc m i i) (inc i) n) m))";
    agrees(&format!("({SRC} {{}} 0 3)"), "{0 0, 1 1, 2 2}");
    let (takes, _) = shapes!(SRC);
    assert!(
        takes.contains(&0),
        "the recur'd param was not taken: takes={takes:?}"
    );
}

#[test]
fn recur_argument_order_is_respected() {
    // S4. `(recur (assoc m ..) (+ c (count m)) ..)`: the SECOND recur
    // argument reads `m` after the first has already been evaluated, so the
    // first must not take. Evaluation order, not textual order, decides.
    const SRC: &str = "(fn [n] (loop [m {} c 0 i 0] \
                       (if (< i n) (recur (assoc m i i) (+ c (count m)) (inc i)) [c (count m)])))";
    agrees(&format!("({SRC} 3)"), "[3 3]");
    agrees(&format!("({SRC} 0)"), "[0 0]");
}

#[test]
fn a_destructured_loop_binding_is_never_taken_loop_carried() {
    // S7's conservative half: a destructuring pattern interleaves `:or`
    // default expressions and `uncons` calls between the back edge's writes,
    // so nothing loop-carried is taken in such a loop. Behaviour must be
    // unaffected either way -- this test is here so the RULE has a witness
    // that would notice if a later change started taking these.
    const SRC: &str = "(fn [n] (loop [[a m] [0 {}] i 0] \
                       (if (< i n) (recur [(inc a) (assoc m i i)] (inc i)) [a (count m)])))";
    agrees(&format!("({SRC} 3)"), "[3 3]");
}

// ---------------------------------------------------------------------------
// S6: try / catch / finally
// ---------------------------------------------------------------------------

#[test]
fn an_unwind_mid_expression_leaves_a_slot_the_catch_reads_intact() {
    // THE try miscompile. `m` is read while building `conj`'s arguments and
    // the very next argument throws, so the take -- if it happened -- would
    // land `nil` in the slot before `catch` reads it.
    const SRC: &str = "(fn [m] (try (conj m (throw :boom)) (catch e m)))";
    agrees("((fn [m] (try (conj m (throw :boom)) (catch e m))) {:a 1})", "{:a 1}");
    let (takes, reads) = shapes!(SRC);
    // Two reads of `m`: one in the body, one in the catch. The catch's is
    // final and takes; the body's must not, and that is what `reads` says.
    assert_eq!(
        reads,
        vec![0],
        "the try body's read of `m` must stay a clone: reads={reads:?} takes={takes:?}"
    );
    assert_eq!(takes, vec![0], "the catch's own final read still takes");
}

#[test]
fn a_finally_that_reads_the_slot_also_suppresses_the_take() {
    const SRC: &str = "(fn [m] (try (assoc m :a 1) (finally (count m))))";
    // W4D-TIERS: expected string was stale sorted-key order.
    agrees("((fn [m] (try (assoc m :a 1) (finally (count m)))) {:x 1})", "{:x 1, :a 1}");
    let (takes, reads) = shapes!(SRC);
    assert_eq!(
        reads,
        vec![0],
        "the try body's read must stay a clone -- `finally` reads it after: \
         reads={reads:?} takes={takes:?}"
    );
    assert_eq!(takes, vec![0], "`finally`'s own final read still takes");
}

#[test]
fn a_try_body_may_still_take_when_no_handler_reads_the_slot() {
    // The other half of S6: `try` is not a blanket veto. Nothing reads `m`
    // after this read on any path -- catch, finally and the code after the
    // `try` all ignore it -- so the take stands.
    const SRC: &str = "(fn [m] (try (assoc m :a 1) (catch e :x) (finally :f)))";
    // W4D-TIERS: expected string was stale sorted-key order.
    agrees("((fn [m] (try (assoc m :a 1) (catch e :x) (finally :f))) {:x 1})", "{:x 1, :a 1}");
    let (takes, _) = shapes!(SRC);
    assert_eq!(takes, vec![0], "a try body with no handler read should take");
}

#[test]
fn an_outer_catch_reading_the_slot_suppresses_a_take_in_an_inner_try() {
    const SRC: &str =
        "(fn [m] (try (try (conj m (throw :boom)) (catch e (throw :again))) (catch e2 m)))";
    agrees(&format!("({SRC} {{:a 1}})"), "{:a 1}");
    let (takes, reads) = shapes!(SRC);
    assert_eq!(
        reads,
        vec![0],
        "the inner try body's read must stay a clone -- an ENCLOSING catch \
         reads it: reads={reads:?} takes={takes:?}"
    );
    assert_eq!(takes, vec![0], "the outer catch's own final read still takes");
}

// ---------------------------------------------------------------------------
// The production counter: the analysis actually raises the unique-hit rate
// ---------------------------------------------------------------------------

/// Runs the real binary on `program` with `MOVA_MAP_PROBE=1` and returns
/// the `assoc` row of section (D) as `(unique, shared)`.
fn assoc_unique_shared(program: &str, extra_env: &[(&str, &str)]) -> (u64, u64) {
    let dir = std::env::temp_dir().join(format!("mova-lastuse-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("p{}.mova", extra_env.len()));
    std::fs::write(&path, program).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mova"));
    // The gate matrix runs this suite under each kill switch in turn; a
    // child that inherited one would be asserting about an environment it
    // did not choose, so each test states the child's switches outright.
    cmd.arg(&path)
        .env("MOVA_MAP_PROBE", "1")
        .env_remove("MOVA_NO_REUSE")
        .env_remove("MOVA_NO_LASTUSE")
        .env_remove("MOVA_NO_COMPILE");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run mova");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "mova failed: {err}");
    let section = err
        .split("(D): consuming-path unique-hit rate")
        .nth(1)
        .unwrap_or_else(|| panic!("no (D) section:\n{err}"));
    let row = section
        .lines()
        .find(|l| l.starts_with("assoc "))
        .unwrap_or_else(|| panic!("no assoc row in (D):\n{err}"));
    let mut f = row.split_whitespace().skip(1);
    let parse = |s: Option<&str>| s.and_then(|x| x.parse().ok()).unwrap_or(u64::MAX);
    (parse(f.next()), parse(f.next()))
}

/// The accumulator loop, run for real. Eight assocs, on receivers of 0..7
/// entries -- deliberately all below `PMap`'s Small/Big boundary, because
/// probe (D) can only measure uniqueness EXACTLY for the Small tier (a
/// `Big` receiver's imbl root refcount is not observable, so those calls
/// land in their own column and would make this assertion meaningless).
///
/// The first assoc receives the `{}` out of the IR's own constant, which
/// still holds it -- so exactly one call is shared and the other seven get
/// the previous iteration's map out of a slot nothing else points at.
const ACCUMULATOR: &str = "(defn build [n] (loop [m {} i 0] \
                           (if (< i n) (recur (assoc m i i) (inc i)) m))) \
                           (println (count (build 8)))";

#[test]
fn the_analysis_actually_makes_the_receiver_unique() {
    // THE liveness net for the whole landing. Every other test here would
    // still pass with an analysis that emitted takes which never HELPED:
    // this is the one that fails if a moving read stops making its receiver
    // unique -- e.g. because some caller started holding a second handle
    // again, which is precisely what the scratch-block clone was doing.
    let (unique, shared) = assoc_unique_shared(ACCUMULATOR, &[]);
    assert_eq!(
        (unique, shared),
        (7, 1),
        "expected 7 unique / 1 shared from an 8-iteration accumulator \
         (only the seed literal is shared)"
    );
}

#[test]
fn mova_no_lastuse_actually_stops_the_takes() {
    // Guards against a SILENTLY DEAD KILL SWITCH: the gate matrix
    // ("the suite passes identically with MOVA_NO_LASTUSE=1") is worthless
    // if the variable does nothing.
    let (unique, shared) = assoc_unique_shared(ACCUMULATOR, &[("MOVA_NO_LASTUSE", "1")]);
    assert_eq!(
        (unique, shared),
        (0, 8),
        "MOVA_NO_LASTUSE=1 did not stop the moving reads"
    );
}

#[test]
fn the_two_switches_compose() {
    // Phase 2 hands over the only handle; phase 1 is what uses it. With
    // phase 1 off there is no consuming call to count at all, no matter what
    // phase 2 proved -- which is the sense in which they compose rather than
    // either one being sufficient.
    let dir = std::env::temp_dir().join(format!("mova-lastuse-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("compose.mova");
    std::fs::write(&path, ACCUMULATOR).expect("write program");
    let mut outs = Vec::new();
    for env in [
        vec![],
        vec![("MOVA_NO_LASTUSE", "1")],
        vec![("MOVA_NO_REUSE", "1")],
        vec![("MOVA_NO_LASTUSE", "1"), ("MOVA_NO_REUSE", "1")],
        vec![("MOVA_NO_COMPILE", "1")],
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mova"));
        cmd.arg(&path)
            .env_remove("MOVA_NO_REUSE")
            .env_remove("MOVA_NO_LASTUSE")
            .env_remove("MOVA_NO_COMPILE");
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run mova");
        assert!(out.status.success(), "{env:?}: {}", String::from_utf8_lossy(&out.stderr));
        outs.push(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    for (i, o) in outs.iter().enumerate() {
        assert_eq!(o, &outs[0], "switch setting #{i} changed the ANSWER");
    }
    assert_eq!(outs[0].trim(), "8");
}
