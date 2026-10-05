//! Builtin function registry. Each submodule owns a disjoint slice of the
//! stdlib (see ARCHITECTURE.md's crate layout) so later phases can work on
//! them in parallel without touching this file beyond the initial wiring.

mod arrays;
mod atoms;
// clojure-lsp campaign (mova/PLAN.md): transit-json decode as a native
// codec over `serde_json`, per PLAN's "reuse Rust crates" rule (transit's
// `:json` encoding IS JSON with a tagged-value convention on top -- no
// transit-java port). New file to avoid merge conflicts, same convention
// `io`/`json`/`digest` already follow.
mod transit;
// census/bench re-export (`lib.rs`'s `internal` module, `examples/
// transit_census.rs`): the module itself stays private, only this one
// fn crosses out.
pub use transit::read_json_str;
// `hostclass::stream_read_json` (`mova.transit/read-json-file`'s entry
// point) needs this crossing out too, crate-internal only.
pub(crate) use transit::read_json_reader;
// `async` is a reserved keyword (2018+ edition); `r#async` is the raw-
// identifier escape hatch so the module can still live in `async.rs`.
// `pub(crate)` (not plain `mod`) for ONE item: `timer_threads_spawned`,
// which `lib.rs`'s `internal::async_timer` wraps so
// `tests/timer_service_test.rs` can assert the shared timer thread is
// spawned exactly once -- same white-box-test escape hatch as
// `Doorbell::safety_net_hits`.
pub(crate) mod r#async;
// V05-PERF-PLAN.md E2 probe: pure-Rust transport bench, `#[ignore]`d tests
// only, needs `r#async`'s `pub(crate)` `chan_put`/`chan_take` -- see that
// module's doc header for why it lives here rather than at crate root.
#[cfg(test)]
mod bench_transport;
// C10: `pub(crate)` (not plain `mod`) so `eval::types_forms::
// eval_dot_form`'s `.size` dot-method arm can reach `count_value`
// directly -- same rationale as `sorted`/`structmap`/`predicates`/
// `strings`/`types` above.
pub(crate) mod collections;
pub(crate) mod types;
// `pub(crate)` (not plain `mod`) so `eval::types_forms::eval_dot_form` can
// reach `supplier_dot_method` directly for the `.get`/`.getAsBoolean`/...
// dot-methods on `Atom`/`Delay` receivers (S7 tail wave).
pub(crate) mod conc;
// lsp/host (clojure-lsp-on-Mova campaign): a manually-completable future
// cell (`mova-deferred`/`-resolve!`/`-reject!`), the primitive
// `promesa.core`'s Deferred is built on -- see that module's doc.
mod deferred;
// `pub(crate)` (not plain `mod`) so `crate::embed::engine::Engine::shutdown`
// can reach `stop_flow_cell` directly -- the same stop machinery
// `flow/stop` itself calls, reused rather than re-evaluated as Clojure
// source (see that fn's doc comment).
pub(crate) mod flow;
mod flow_steps;
// kondo-wave: `directory?`/`canonical-path` plus the pure path-string
// helpers `hostclass::call_javafile_method` calls into -- see that
// module's doc header. New file, registered only from `register_sys`
// (same capability group as `sys`, which it complements) so it never
// touches `register_core`'s `Profile::Pure` surface.
pub(crate) mod fileio;
// lsp/kondo: `mova.fs` -- walkdir+globset directory walk/glob natives
// backing the `babashka.fs` shim (see that module's doc header).
mod fs;
// lsp/io (clojure-lsp-on-Mova campaign, mova/PLAN.md): byte-level stdio,
// JSON (serde_json-backed), and md5 natives -- see that module's doc
// header. New file, per PLAN.md's merge-conflict-avoidance rule for a
// Rust addition.
mod io;
mod nx;
mod nx_tap;
// lsp/reader (clojure-lsp-on-Mova campaign, mova/PLAN.md): mova.reader
// native CST parser mirroring clj-kondo's forked rewrite-clj parser --
// new file (merge-conflict-avoidance).
pub mod cst_reader;
// lsp/io: mova.uri percent-encode/decode, new file (merge-conflict-avoidance).
mod uri;
// lsp/io (clojure-lsp-on-Mova campaign, mova/PLAN.md "reuse Rust crates"):
// mova.diff/unified-diff over the `similar` crate, new file
// (merge-conflict-avoidance).
mod difftext;
#[cfg(test)]
mod map_bench;
mod math;
// C3c: `pub(crate)` (was private) -- `crate::types::builtin_classes`'s
// `clojure.lang.IObj` row calls `meta::is_iobj` directly (see that fn's
// own doc for why one predicate answers both `with-meta` eligibility and
// `instance?`). D5 needs `multi` pub(crate) too (`.addMethod` veneer).
pub(crate) mod meta;
pub(crate) mod multi;
pub mod map_probe;
// C3c: `pub(crate)` (was private) -- `eval::types_forms::eval_dot_form`'s
// `.refer` dot-dispatch calls `nsfns::refer_dot_method` directly.
// W3e-4: and `eval::special_forms::warn_if_def_shadows` calls
// `nsfns::write_shim_err`, the one `*err*` channel this runtime has.
pub(crate) mod nsfns;
pub mod numbers;
pub mod reuse;
// V05 Perceus-lite phase 1 tracking probe, `#[ignore]`d tests only -- same
// in-crate rationale as `map_bench` (needs `Interp::with_compile_enabled`).
#[cfg(test)]
mod reuse_bench;
pub(crate) mod predicates;
mod random;
// SPEC-W6a: `clojure.test.check.random`'s native veneer -- an entirely
// different thing from `random` above, which is `clojure.core`'s own
// `rand`/`rand-int`/`rand-nth`. See that module's doc for why the
// namespace is native and what precedence rule its wiring depends on.
mod tcrandom;
mod reflect;
pub(crate) mod regex;
// C10: `pub(crate)` so `eval::types_forms::eval_dot_form`'s `.reduce`
// dot-method arm can reach `reduce_coll` directly -- same rationale as
// `sorted`/`structmap`/`collections`/`predicates`/`strings`/`types` above.
pub(crate) mod seq;
// C10: `pub(crate)` (not plain `mod`) so `eval::apply`'s `Value::SortedSet`
// invoke arm can reach `sorted_set_get` directly -- same rationale as
// `structmap`/`predicates`/`strings`/`types` above.
pub(crate) mod sorted;
/// L5/W4 (docs/L5-VIRTUAL-TIME-DESIGN.md §5/§6, OWNER-BRIEF-L5 RULING 1):
/// the `simulate` native, the per-call sim state, the deadlock tripwire and
/// the leaked-task teardown. `pub(crate)` because `runtime::sim_next_job`'s
/// quiescence point drives it — that is the ONE place in the process that can
/// see the slab, so the policy lives here and the mechanism lives there.
pub(crate) mod sim;
mod statics;
// `pub(crate)` (not plain `mod`) so `eval::apply`'s map-as-fn/keyword-
// lookup arms can reach `struct_map_get` directly -- same rationale as
// `predicates`/`strings`/`types` above (a cross-tree caller outside
// `builtins` needs specific fns, not the whole module's bare-name
// registration surface).
pub(crate) mod structmap;
// S6 (strdot): `pub(crate)` (not plain `mod`) so `eval::types_forms::
// eval_dot_form` can reach `str_dot_method` directly for `(.startsWith s
// ..)`-shaped instance-method dispatch on a `Value::Str` receiver --
// mirrors `types`/`flow`/`predicates` above, same rationale (a cross-tree
// caller outside `builtins` needs one specific fn, not the whole module's
// bare-name registration surface).
pub(crate) mod strings;
mod sys;
mod os;
// C7 (vecveneer): `pub(crate)` for the same reason `strings` above is --
// `eval::types_forms::eval_dot_form` reaches `vec_dot_method` directly for
// `(.rseq v)`/`(.containsKey v ..)`/...-shaped instance-method dispatch on
// a `Vector`/`TypedVec`/`List`(-as-seq)/`VecSeq` receiver.
pub(crate) mod vecdot;
// C14 (protocols): same `pub(crate)` reason as `vecdot` above --
// `eval::types_forms::eval_dot_form` reaches `record_dot_method` directly
// for `(.size rec)`/`(.equals a b)`/...-shaped instance-method dispatch on
// a `defrecord` instance.
pub(crate) mod recorddot;

use std::sync::Arc;

use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{NativeFn, Symbol, Value};

/// Re-exported so `eval::Interp::realize_deep` can walk lazy-seq cons
/// chains without duplicating `collections.rs`'s cons-cell convention (see
/// that module's doc comment for the encoding).
pub(crate) use collections::uncons;

/// C11 (quasiquote): re-exported so `eval::Interp::seq_items` -- the third
/// seq walker in the codebase, alongside `uncons`/`materialize` -- can
/// apply the SAME improper-list rule instead of (as it did before C11)
/// silently ignoring it. See [`collections::lazy_tail_split`]'s doc for
/// the rule itself and the bug that not sharing it caused.
pub(crate) use collections::{lazy_tail_split, materialize};

/// Capability-group split of `register_all`, for `crate::embed::Profile`
/// (see that module's doc): every profile gets [`register_core`]
/// unconditionally, and picks among [`register_sys`]/[`register_conc`]/
/// [`register_flow`] independently. `register_all` itself is unchanged --
/// exactly these four in this order -- so `main.rs`/every non-embed test
/// keeps seeing today's full registration untouched.
pub fn register_all(i: &mut Interp) {
    register_core(i);
    register_sys(i);
    register_conc(i);
    register_flow(i);
}

/// Numbers/collections/seq/strings/predicates/atoms/reflect/regex: no
/// syscalls, no thread spawning, nothing that reaches outside the process.
/// This is the ENTIRE builtin surface `crate::embed::Profile::Pure` grants.
pub fn register_core(i: &mut Interp) {
    numbers::register(i);
    collections::register(i);
    seq::register(i);
    sorted::register(i);
    structmap::register(i);
    strings::register(i);
    // lsp/io: mova.uri percent-encode/decode -- pure, no capability, same
    // group as strings/math above.
    uri::register(i);
    crate::memstat::register(i);
    // lsp/io: mova.diff/unified-diff -- pure text diffing, same group.
    difftext::register(i);
    crate::metrics::register(i);
    predicates::register(i);
    atoms::register(i);
    meta::register(i);
    crate::errinfo::register(i);
    reflect::register(i);
    regex::register(i);
    random::register(i);
    // SPEC-W6a. In `register_core`, beside `strings`/`math`, for the
    // same reason those are: `clojure.test.check.random` is pure
    // arithmetic with no capability of its own, and an embedder on
    // `Profile::Pure` that uses `clojure.spec.alpha`'s generators needs
    // it just as much as a scripting one does.
    tcrandom::register(i);
    math::register(i);
    statics::register(i);
    types::install(i);
    crate::hostclass::register(i);
    nsfns::register(i);
    arrays::register(i);
    multi::install(i);
    transit::register(i);
}

/// `slurp`/`spit`/`sh`/`getenv`/... -- direct `libc` wrappers, excluded from
/// `Profile::Pure`.
pub fn register_sys(i: &mut Interp) {
    sys::register(i);
    os::register(i);
    fileio::register(i);
    fs::register(i);
    // lsp/io: byte-level stdio/JSON/md5 natives are I/O capabilities, same
    // capability group as `sys` above.
    io::register(i);
    nx::register(i);
    nx_tap::register(i);
    // lsp/reader: mova.reader/parse-string native CST parser.
    cst_reader::register(i);
}

/// Real-OS-thread concurrency (`future*`/`promise`/`delay*`) plus
/// core.async's channels (`chan`/`>!!`/`<!!`/`go*`/...) -- grouped together
/// because `core/flow.mova`'s bootstrap (see `register_flow`) needs
/// `Value::Channel` support already registered. Excluded from
/// `Profile::Pure`: both spawn real OS threads.
pub fn register_conc(i: &mut Interp) {
    conc::register(i);
    deferred::register(i);
    r#async::register(i);
    // L5/W4: `simulate` is a concurrency native — it runs its thunk as the
    // root TASK of the sim world — so it belongs to exactly the group whose
    // absence would make it meaningless (`Profile::Pure` has no tasks to
    // schedule and no channels to schedule them around).
    sim::register(i);
}

/// `core.async.flow`'s native proc-graph engine. Depends on `register_conc`
/// having already run (flow procs are channels + threads underneath) --
/// `Interp::with_capabilities` enforces that ordering, not this fn.
/// Excluded from `Profile::Pure`.
pub fn register_flow(i: &mut Interp) {
    flow::register(i);
    flow_steps::register(i);
}

/// Which optional builtin-capability groups (beyond [`register_core`],
/// always on) an `Interp` should register, and correspondingly which
/// `core/*.mova` bootstrap files it's safe to load -- `core/async.mova` and
/// `core/flow.mova` both `def`-bind a native from their own group
/// UNQUALIFIED at top-level bootstrap time (e.g. `core/async.mova`'s
/// `(def >! >!!)`, `core/flow.mova`'s `(def map->step flow/map->step*)`), so
/// loading either bootstrap without its native group is a hard bootstrap
/// panic, not a graceful per-call "unresolved symbol" -- see
/// `crate::eval::Interp::with_capabilities`, which skips the bootstrap file
/// entirely rather than loading it and letting it fail.
#[derive(Clone, Copy)]
pub(crate) struct Capabilities {
    pub sys: bool,
    pub conc: bool,
    pub flow: bool,
}

impl Capabilities {
    /// `Profile::Scripting`: identical registration to `register_all`.
    pub(crate) const ALL: Capabilities = Capabilities {
        sys: true,
        conc: true,
        flow: true,
    };
    /// `Profile::Pure`: `register_core` and nothing else.
    pub(crate) const CORE_ONLY: Capabilities = Capabilities {
        sys: false,
        conc: false,
        flow: false,
    };
}

/// Declares the expected argument count for a native, so `reg` can
/// generate a consistent arity-checking wrapper (and error message)
/// instead of every builtin hand-rolling its own check.
#[derive(Clone, Copy)]
pub enum ArityHint {
    Exact(usize),
    Min(usize),
    #[allow(dead_code)] // not needed yet by P2's seed set; kept for P3a
    Range(usize, usize),
    Any,
}

impl ArityHint {
    fn matches(self, n: usize) -> bool {
        match self {
            ArityHint::Exact(k) => n == k,
            ArityHint::Min(k) => n >= k,
            ArityHint::Range(lo, hi) => n >= lo && n <= hi,
            ArityHint::Any => true,
        }
    }

    fn expected_desc(self) -> String {
        fn plural(n: usize) -> &'static str {
            if n == 1 {
                ""
            } else {
                "s"
            }
        }
        match self {
            ArityHint::Exact(k) => format!("exactly {k} argument{}", plural(k)),
            ArityHint::Min(k) => format!("at least {k} argument{}", plural(k)),
            ArityHint::Range(lo, hi) => format!("{lo} to {hi} arguments"),
            ArityHint::Any => "any number of arguments".to_string(),
        }
    }
}

/// Registers a native fn `name` in `i.globals`, wrapping `f` with an
/// arity check derived from `arity` so every builtin gets a consistent,
/// well-labeled arity error for free.
#[track_caller]
pub fn reg(
    i: &mut Interp,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let native = NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            // C3c (errors.clj's `arity-exception` deftest, `(assoc)` ->
            // `.-actual` = 0): native builtins go through THIS arity
            // check, not `apply.rs`'s user-closure one -- see
            // `RjError::arity_actual`'s doc for why both sites populate
            // it.
            return Err(RjError::arity(format!(
                "{name}: expected {}, got {}",
                arity.expected_desc(),
                args.len()
            ))
            .with_stack(interp.stack_snapshot(), interp.source_id)
            .with_arity_actual(args.len() as i64));
        }
        f(interp, args)
    });
    i.globals.set_builtin(Symbol::simple(name), Value::Native(Arc::new(native)));
}

/// S5 / M3: THE metadata-preservation shim.
///
/// Clojure's `IObj` contract splits collection operations in two, and the
/// split is per-operation and MEASURED, not derivable from first
/// principles (see `tests/conformance/pending/metadata.corpus` and the
/// table in `CLOJURE-COMPAT-PLAN.md`'s M3 row):
///
/// - an **update** on a collection returns a value carrying the
///   receiver's metadata -- `conj`, `assoc`, `dissoc`, `disj`, `pop`,
///   `empty`, `into`, `update`, `assoc-in`, `update-in`, `select-keys`,
///   `merge` (the FIRST map's, not the second's), `sort`;
/// - a **rebuild** drops it -- `map`, `filter`, `seq`, `vec`, `keys`,
///   `vals`, `reverse`, `distinct`, `concat`, `zipmap`, `cons`.
///
/// This wrapper implements the first column for a builtin that dispatches
/// on `args[0]`: the receiver is unwrapped on the way in (so the body
/// below never has to know metadata exists) and the metadata re-attached
/// to the result on the way out.
///
/// COST when the receiver has no metadata -- i.e. essentially every call
/// -- is one enum-discriminant test, taken before anything is cloned or
/// allocated. The `Vec` on the other branch is deliberate: it buys every
/// wrapped builtin's body the right to stay written as if metadata did
/// not exist, and it is only ever paid by a call that actually passes a
/// metadata-carrying receiver.
fn preserving_receiver_meta<F>(interp: &mut Interp, args: &[Value], f: &F) -> Result<Value, RjError>
where
    F: Fn(&mut Interp, &[Value]) -> Result<Value, RjError>,
{
    if !args.first().is_some_and(Value::has_meta) {
        return f(interp, args);
    }
    let recv = args[0].clone();
    let mut unwrapped: Vec<Value> = Vec::with_capacity(args.len());
    unwrapped.push(recv.unmeta().clone());
    unwrapped.extend_from_slice(&args[1..]);
    f(interp, &unwrapped).map(|r| r.with_meta_of(&recv))
}

/// S5 / M3: [`reg`] for a READ op that dispatches on `args[0]` and must
/// see THROUGH `IObj` metadata.
///
/// The default rule for the whole subsystem: metadata never changes what
/// a value *is*, only what it carries alongside. `(map? (with-meta {:x 1}
/// {:a 1}))` is `true`, `(get (with-meta {:x 1} {:a 1}) :x)` is `1`,
/// `(count (with-meta [1 2] {:a 1}))` is `2` -- all measured. A builtin
/// registered through here gets an unwrapped receiver and can be written
/// as if `Value::Meta` did not exist.
///
/// Same zero-cost-when-absent shape as [`preserving_receiver_meta`]: one
/// discriminant test on the common path.
///
/// Ops whose receiver ISN'T `args[0]` (`instance?`'s is `args[1]`,
/// `reduce-kv`'s is `args[2]`) can't use this and unwrap by hand at their
/// own site.
#[track_caller]
pub fn reg_unmeta(
    i: &mut Interp,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    reg(i, name, arity, move |interp, args| {
        if !args.first().is_some_and(Value::has_meta) {
            return f(interp, args);
        }
        let mut unwrapped: Vec<Value> = Vec::with_capacity(args.len());
        unwrapped.push(args[0].unmeta().clone());
        unwrapped.extend_from_slice(&args[1..]);
        f(interp, &unwrapped)
    });
}

/// [`reg`] for a metadata-PRESERVING op. See [`preserving_receiver_meta`]
/// for which operations those are and why the list is measured rather
/// than inferred.
#[track_caller]
pub fn reg_preserving_meta(
    i: &mut Interp,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    reg(i, name, arity, move |interp, args| {
        preserving_receiver_meta(interp, args, &f)
    });
}

/// [`reg_consuming`] for a metadata-PRESERVING op -- both entry points
/// get the same treatment, since they are required to agree observably.
#[track_caller]
pub fn reg_consuming_preserving_meta(
    i: &mut Interp,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
    consuming: impl Fn(&mut Interp, &mut [Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    reg_consuming(
        i,
        name,
        arity,
        move |interp, args| preserving_receiver_meta(interp, args, &f),
        move |interp, args| {
            // The consuming path can't hand its `&mut [Value]` buffer to
            // `preserving_receiver_meta` (which needs a shared slice), so
            // it takes the receiver's metadata off here and puts the
            // buffer back the way the callee expects: an unwrapped
            // receiver in slot 0.
            if !args.first().is_some_and(Value::has_meta) {
                return consuming(interp, args);
            }
            let recv = std::mem::replace(&mut args[0], Value::Nil);
            args[0] = recv.unmeta().clone();
            consuming(interp, args).map(|r| r.with_meta_of(&recv))
        },
    );
}

/// Registers a native that has BOTH entry points (Perceus-lite phase 1 --
/// see `builtins::reuse`): `f` for every caller that only lends its args,
/// `consuming` for the two tier call sites that own theirs and are about to
/// drop them.
///
/// Both get the same generated arity check, from the same `ArityHint`, so
/// the two paths cannot disagree about arity errors -- the only thing they
/// are permitted to differ in is how much they allocate.
#[track_caller]
pub fn reg_consuming(
    i: &mut Interp,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
    consuming: impl Fn(&mut Interp, &mut [Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    // C3c (errors.clj's `arity-exception` deftest, `(assoc)` -> `.-
    // actual` = 0): `assoc`/`conj`/... register through THIS wrapper
    // (`reg_consuming`/`reg_consuming_preserving_meta`), a separate arity
    // check from the plain `reg` wrapper above -- both must populate
    // `arity_actual` or a native fn registered the "consuming" way would
    // catch-bind to the old plain info map instead of a real
    // `ArityException`.
    let arity_err = move |interp: &Interp, n: usize| {
        RjError::arity(format!("{name}: expected {}, got {n}", arity.expected_desc()))
            .with_stack(interp.stack_snapshot(), interp.source_id)
            .with_arity_actual(n as i64)
    };
    let mut native = NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            return Err(arity_err(interp, args.len()));
        }
        f(interp, args)
    });
    native.consuming = Some(Box::new(move |interp: &mut Interp, args: &mut [Value]| {
        if !arity.matches(args.len()) {
            return Err(arity_err(interp, args.len()));
        }
        consuming(interp, args)
    }));
    i.globals.set_builtin(Symbol::simple(name), Value::Native(Arc::new(native)));
}
