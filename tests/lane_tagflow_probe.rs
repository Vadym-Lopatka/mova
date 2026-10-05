//! W1 kill-probe 1a (LATENCY-CAMPAIGN.md §3): "run the tag-flow fixpoint
//! (`compile::lanes::feasible_worlds`) over the real `NumLoop` op lists from
//! `compile::tests`' scalar loops + every `NumLoop` the bench corpus
//! produces. Count feasible tag vectors per loop. BAR: if >10% of real
//! NumLoops need >2 lane variants, shrink the design to exactly two
//! hardcoded variants (all-I, all-F-after-first-promotion) before building
//! more."
//!
//! This walks every compiled `fn` body (from the exact loop-shape corpus
//! `compile::mod::tests::MUST_SPECIALIZE` reproduces here, PLUS every real
//! `.mova` file under `bench/`) for `Ir::NumLoop` nodes, runs the fixpoint on
//! each, and reports the distribution of `feasible_worlds().len()`.
//!
//! Run with `cargo test --release --test lane_tagflow_probe -- --nocapture`.

use std::fs;
use std::path::Path;

use mova::internal::compile::ir::{CompiledPattern, Ir, MapPattern, NumLoop, SeqStep};
use mova::internal::compile::lanes;
use mova::internal::{Interp, Value};

/// The exact loop-shape corpus `compile::mod::tests::MUST_SPECIALIZE`
/// checks node-shape for -- duplicated here (not imported: that list lives
/// in a `#[cfg(test)]` module private to the crate) so the probe runs over
/// the same "every grammar shape" set the shape test pins.
const MUST_SPECIALIZE: &[&str] = &[
    "(fn [seed] \
      (loop [i 0 acc seed] \
        (if (< i 2000) \
          (recur (inc i) (+ (* acc 6364136223846793005) 1442695040888963407)) \
          acc)))",
    "(fn [] (loop [i 0] (if (< i 10) (recur (inc i)) i)))",
    "(fn [] (loop [i 10] (if (> i 0) (recur (dec i)) i)))",
    "(fn [] (loop [i 0] (if (>= i 5) i (recur (inc i)))))",
    "(fn [] (loop [i 0] (if (<= i 5) (recur (inc i)) i)))",
    "(fn [] (loop [i 0] (if (= i 5) i (recur (inc i)))))",
    "(fn [n] (loop [i n] (if (< i 5) (recur (inc i)) i)))",
    "(fn [] (loop [i 0.5] (if (< i 5.0) (recur (+ i 1.0)) i)))",
    "(fn [k] (loop [i 0] (if (< i k) (recur (inc i)) i)))",
    "(fn [k] (let [m k] (loop [i 0] (if (< i m) (recur (inc i)) i))))",
    "(fn [k] (fn [] (loop [i 0] (if (< i k) (recur (inc i)) i))))",
    "(fn [] (loop [i 0 a 1.5] (if (< i 3) (recur (inc i) (- a 0.5)) a)))",
    "(fn [] (loop [i 0 a 0] (if (< i 3) (recur (inc i) (+ a 1 2 3)) a)))",
    "(fn [] (loop [i 0 a 1] (if (< i 3) (recur (inc i) (* a 2 3 4)) a)))",
    "(fn [] (loop [i 0 a 1] (if (< i 3) (recur (dec i) (* a 2)) (- a 1))))",
    "(fn [] (loop [i 0 a 1 b 2] (if (< i 3) (recur (inc i) b a) (- a b))))",
    "(fn [] (loop [a 0 b 0 c 0 d 0 e 0 f 0 g 0 h 0] \
              (if (< a 5) (recur (inc a) b c d e f g h) (+ a b c d e f g h))))",
    "(fn [] (loop [a 0 a 1] (if (< a 5) (recur a (inc a)) a)))",
    // W-NUMLOOP nil-terminal shapes (kept in sync with the in-crate list).
    "(fn [n] (loop [i 0] (if (< i n) (recur (inc i)))))",
    "(fn [n] (loop [i 0] (when (< i n) (recur (inc i)))))",
    "(fn [n] (loop [i 0] (if (< i n) (recur (inc i)) nil)))",
    "(fn [n] (loop [i 0] (if (>= i n) nil (recur (inc i)))))",
    "(fn [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc i)) nil)))",
    "(fn [] (loop [i 0.5] (when (< i 5.0) (recur (+ i 1.0)))))",
    "(fn [n] (dotimes [i n]))",
    "(fn [n] (loop [i 0] (if (< i n) (recur (inc i)) (do))))",
    "(fn [] (loop [i 0] (if (< i 5) (do (recur (inc i))) i)))",
    "(fn [] (loop [n 0] (if (< n 3) (recur (loop [k n] (if (< k 9) (recur (inc k)) k))) n)))",
];

/// Recursively collects every `NumLoop` reachable from `ir` -- mirrors
/// `compile::mod::tests::contains_num_loop`'s exhaustive match (every `Ir`
/// variant listed on purpose, so a new node that can hold a `loop` fails to
/// compile here until it is classified) but COLLECTS instead of just asking
/// yes/no.
fn collect_num_loops<'a>(ir: &'a Ir, out: &mut Vec<&'a NumLoop>) {
    fn many<'a>(irs: &'a [Ir], out: &mut Vec<&'a NumLoop>) {
        for i in irs {
            collect_num_loops(i, out);
        }
    }
    fn binds<'a>(bs: &'a [(CompiledPattern, Ir)], out: &mut Vec<&'a NumLoop>) {
        for (p, i) in bs {
            in_pattern(p, out);
            collect_num_loops(i, out);
        }
    }
    fn in_pattern<'a>(p: &'a CompiledPattern, out: &mut Vec<&'a NumLoop>) {
        match p {
            CompiledPattern::Slot(_) => {}
            CompiledPattern::Seq(steps) => {
                for s in steps {
                    match s {
                        SeqStep::Elem(q) | SeqStep::Rest(q) | SeqStep::As(q) => in_pattern(q, out),
                    }
                }
            }
            CompiledPattern::Map(m) => {
                let MapPattern { entries, as_pat } = m.as_ref();
                for e in entries {
                    in_pattern(&e.target, out);
                    if let Some(d) = &e.default {
                        collect_num_loops(d, out);
                    }
                }
                if let Some(a) = as_pat {
                    in_pattern(a, out);
                }
            }
        }
    }
    match ir {
        Ir::NumLoop(nl) => {
            out.push(nl);
            // The fallback is the ordinary `Loop` this node was built from
            // -- never itself a `NumLoop` (specialization doesn't recurse),
            // but its body may contain a NESTED loop that specialized on
            // its own terms, so it is still walked.
            collect_num_loops(&nl.fallback, out);
        }
        Ir::Const(_)
        | Ir::LoadSlot(_)
        | Ir::LoadSlotTake(_)
        | Ir::LoadCapture(_)
        | Ir::SelfRef
        // fix/closure-env-cycles: a sibling reference reads the running
        // closure's recursive binding group, not a compiled subtree.
        | Ir::SiblingRef(_)
        | Ir::GlobalRef { .. }
        // field3/W-RESOLVE: an escape's payload is a source `Form`, not
        // `Ir` -- there is no compiled subtree under it to walk, and
        // therefore no `NumLoop` it could hide.
        | Ir::Escape(_)
        // W-FIELDGET: same reasoning -- its fallback IS that `Escape`, and
        // its receiver is a bare frame read, so no `NumLoop` hides under it
        // either.
        | Ir::FieldGet(_)
        | Ir::CreationEnvLookup { .. } => {}
        Ir::If { test, then, els } => {
            collect_num_loops(test, out);
            collect_num_loops(then, out);
            if let Some(e) = els {
                collect_num_loops(e, out);
            }
        }
        Ir::Do(irs) | Ir::VectorLit(irs) | Ir::SetLit(irs) => many(irs, out),
        Ir::Let { binds: bs, body } | Ir::Loop { binds: bs, body, .. } => {
            binds(bs, out);
            many(body, out);
        }
        Ir::Recur { args, .. } => many(args, out),
        Ir::Call { callee, args, .. } => {
            collect_num_loops(callee, out);
            many(args, out);
        }
        Ir::CallGlobal { args, .. }
        | Ir::CallCreationEnv { args, .. }
        | Ir::Intrinsic { args, .. } => many(args, out),
        Ir::MapLit(kvs) => {
            for (k, v) in kvs {
                collect_num_loops(k, out);
                collect_num_loops(v, out);
            }
        }
        Ir::Throw { value, .. } => collect_num_loops(value, out),
        Ir::MakeClosure { template, .. } => {
            for a in template.code.arities.iter() {
                many(&a.body, out);
            }
        }
        // Every member's template is its own fn, walked for the same reason
        // a `MakeClosure` template is.
        Ir::MakeRecGroup { members, .. } => {
            for m in members {
                for a in m.template.code.arities.iter() {
                    many(&a.body, out);
                }
            }
        }
        Ir::Try { body, catches, finally } => {
            many(body, out);
            for arm in catches {
                many(&arm.body, out);
            }
            if let Some(f) = finally {
                many(f, out);
            }
        }
        Ir::Def { value, .. } => {
            if let Some(v) = value {
                collect_num_loops(v, out);
            }
        }
        Ir::SetMutField { value, .. } => collect_num_loops(value, out),
        Ir::DynBind(d) => {
            for (_, _, init) in &d.pairs {
                collect_num_loops(init, out);
            }
            many(&d.body, out);
        }
        Ir::New(n) => {
            many(&n.args, out);
            collect_num_loops(&n.fallback, out);
        }
    }
}

/// Every `NumLoop` reachable from every compiled arity of every `Def`/`fn`
/// evaluated so far in `interp` -- there is no registry of compiled fns, so
/// this walks the values the source handed back plus, for the bench corpus,
/// every top-level `def`'d `Value::Fn`. For a single last-form fn (the
/// `MUST_SPECIALIZE` corpus) that is just `v` itself.
fn num_loops_in_value<'a>(v: &'a Value, out: &mut Vec<&'a NumLoop>) {
    if let Value::Fn(rc) = v {
        if let Some(cc) = rc.compiled.compiled() {
            for a in cc.code.arities.iter() {
                for ir in &a.body {
                    collect_num_loops(ir, out);
                }
            }
        }
    }
}

/// Splits `src` into top-level forms by bracket depth (treating `(`/`[`/`{`
/// uniformly, which is enough to find where a top-level form ends in
/// well-formed source), skipping `;` line comments and `"..."` strings.
/// Good enough for the known `bench/*.mova` corpus -- not a general reader.
fn split_top_level_forms(src: &str) -> Vec<String> {
    let mut forms = Vec::new();
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    let mut in_string = false;
    let mut in_comment = false;
    let mut escape = false;
    let chars: Vec<char> = src.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if in_comment {
            if c == '\n' {
                in_comment = false;
            }
            continue;
        }
        if in_string {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            ';' if depth == 0 => in_comment = true,
            '"' => in_string = true,
            '(' | '[' | '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start.take() {
                        forms.push(chars[s..=i].iter().collect());
                    }
                }
            }
            _ => {}
        }
    }
    forms
}

/// `Some(name)` when `form` is `(defn name ..)` or `(def name ..)` --
/// pure-definition top-level forms, safe to evaluate without running any of
/// the bench script's actual I/O.
fn def_name(form: &str) -> Option<String> {
    let rest = form
        .strip_prefix("(defn ")
        .or_else(|| form.strip_prefix("(def "))?;
    let name: String = rest
        .trim_start()
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '(' && *c != ')')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn report(label: &str, worlds: usize) -> usize {
    println!("  {label:60} feasible worlds = {worlds}");
    worlds
}

/// This kill-probe asserts NumLoop specialization actually fired for the
/// corpus, so it has nothing to say under a switch that suppresses emission
/// entirely -- same discipline as `compile::mod::tests`' own kill-switch
/// tests (`the_numloop_kill_switch_suppresses_every_specialization`).
/// `MOVA_NO_LANES=1` alone is fine: the probe calls `lanes::
/// feasible_worlds` directly, bypassing `resolve`'s emission gate.
fn numloop_emission_disabled() -> bool {
    ["MOVA_NO_NUMLOOP", "MOVA_NO_COMPILE"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| v == "1"))
}

#[test]
fn tag_vector_count_kill_probe() {
    if numloop_emission_disabled() {
        println!("NumLoop emission disabled by env; kill-probe 1a has nothing to check.");
        return;
    }
    let mut total = 0usize;
    let mut over_two = 0usize;
    let mut max_seen = 0usize;

    println!("\n=== W1 kill-probe 1a: tag-vector count per real NumLoop ===");
    println!("-- compile::tests' MUST_SPECIALIZE corpus --");
    for src in MUST_SPECIALIZE {
        let mut interp = Interp::new();
        interp.set_eager_compile(true);
        let v = interp
            .eval_str("lane-probe", src)
            .unwrap_or_else(|e| panic!("{src}: {}", e.message));
        let mut loops = Vec::new();
        num_loops_in_value(&v, &mut loops);
        assert!(!loops.is_empty(), "expected at least one NumLoop for: {src}");
        for nl in loops {
            let worlds = lanes::feasible_worlds(nl);
            let n = worlds.len();
            total += 1;
            max_seen = max_seen.max(n);
            if n > 2 {
                over_two += 1;
            }
            report(src, n);
        }
    }

    println!("-- real bench/*.mova corpus --");
    let bench_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench");
    let mut clj_files: Vec<_> = fs::read_dir(&bench_dir)
        .expect("bench/ must exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "clj"))
        .collect();
    clj_files.sort();
    for path in clj_files {
        let src = fs::read_to_string(&path).unwrap();
        // Bench scripts drive real I/O (flow/futures/promises/`time-ms`);
        // running the whole file would hang or diverge the probe. We only
        // need the compiled `Ir` shapes a top-level `defn`/`def` builds, so
        // split into top-level forms (bracket-depth splitter, adequate for
        // this corpus -- no char-literal parens/brackets in it) and keep
        // only `(defn ..)`/`(def name ..)` forms, which are pure
        // definitions here: `flow/map->step`/`flow/create-flow` just build
        // data structures, nothing runs until the `(flow/start ..)` /
        // `(deref ..)` forms this filter naturally excludes.
        let mut interp = Interp::new();
        interp.set_eager_compile(true);
        let mut names = Vec::new();
        for form in split_top_level_forms(&src) {
            let Some(name) = def_name(&form) else { continue };
            if interp.eval_str("lane-probe-bench", &form).is_ok() {
                names.push(name);
            }
        }
        let values: Vec<Value> = names
            .iter()
            .filter_map(|name| interp.eval_str("lane-probe-bench", name).ok())
            .collect();
        let mut loops = Vec::new();
        for v in &values {
            num_loops_in_value(v, &mut loops);
        }
        if loops.is_empty() {
            continue;
        }
        for nl in loops {
            let worlds = lanes::feasible_worlds(nl);
            let n = worlds.len();
            total += 1;
            max_seen = max_seen.max(n);
            if n > 2 {
                over_two += 1;
            }
            report(&path.file_name().unwrap().to_string_lossy(), n);
        }
    }

    let pct_over_two = if total == 0 { 0.0 } else { 100.0 * over_two as f64 / total as f64 };
    println!(
        "\n=== TOTAL: {total} real NumLoops, max feasible worlds = {max_seen}, {over_two} ({pct_over_two:.1}%) need >2 lane variants ==="
    );
    println!("BAR: >10% needing >2 variants => shrink to the two-variant design (all-I, all-F-after-first-promotion).");
    if pct_over_two > 10.0 {
        println!("*** KILL-PROBE 1a: BAR FAILED -- shrink to two hardcoded variants. ***");
    } else {
        println!("*** KILL-PROBE 1a: BAR CLEARED. ***");
    }
}
