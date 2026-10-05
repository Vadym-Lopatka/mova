use std::path::PathBuf;
use std::sync::{Arc, Weak};

use crate::builtins::Capabilities;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{FlowCell, NativeFn, Value as RawValue};

use super::{Error, Value};

/// Which builtin capability groups an [`Engine`] gets, chosen at
/// [`EngineBuilder::profile`] time -- see `crate::embed`'s module doc for
/// the full rationale and the core.mova-bootstrap interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Numbers, collections, seqs, strings, predicates, atoms, reflection,
    /// regex -- no syscalls (`slurp`/`spit`/`sh`/...), no thread spawning
    /// (`future`/`chan`/`flow/*`/...). Safe to run untrusted script text
    /// against: nothing it evaluates can touch the filesystem, the
    /// network, another process, or spawn an OS thread.
    Pure,
    /// Everything `mova`'s own CLI has: `Pure` plus `sys` plus real-thread
    /// concurrency (futures/promises/core.async) plus `core.async.flow`.
    /// Identical builtin surface to `Interp::new()`.
    Scripting,
    /// Same builtin-capability surface as [`Profile::Pure`] (no sys, no
    /// thread spawning) but with fuel treated as REQUIRED, not merely
    /// available: if [`EngineBuilder::fuel`] was never called,
    /// [`EngineBuilder::build`] applies a documented default budget of
    /// **10,000,000 steps** rather than leaving the engine unbounded.
    ///
    /// This is the profile to reach for when `Engine::eval`/`call` will run
    /// script text you did not write yourself. Three things to know before
    /// relying on it:
    ///
    /// 1. **Fuel counts interpreter steps, not wall-clock.** It decrements
    ///    at loop back-edges and fn-call entry
    ///    (`crate::eval::Interp::tick_fuel`), and the measured
    ///    steps-per-second rate varies roughly 100x across tiers
    ///    (tree-walked vs. compiled vs. compiled+`NumLoop` -- see the probe
    ///    data in `EMBED-API-PLAN.md` §2.3). The 10,000,000 default is a
    ///    reasonable generic ceiling, not a calibrated time budget -- hosts
    ///    that care about a specific wall-clock bound should measure their
    ///    own workload and set an explicit number with
    ///    [`EngineBuilder::fuel`]/[`Engine::set_fuel`].
    /// 2. **Fuel does not bound time spent INSIDE a single native call.** A
    ///    native like `slurp` on an enormous file, a pathological regex, or
    ///    a long-running `sh` spends effectively zero fuel while it runs.
    ///    Untrusted hosting should pair `Profile::Untrusted` with an
    ///    OS-level watchdog (a wall-clock deadline enforced from OUTSIDE the
    ///    engine -- a timeout thread, a request-handler deadline, `alarm(2)`
    ///    -- that can act even if the engine itself never returns). Fuel
    ///    plus `Pure`-equivalent capabilities is not "sandboxed"; it is
    ///    "bounded interpreter steps with no filesystem/network/thread
    ///    access", which is a different and weaker guarantee than a real
    ///    sandbox provides.
    /// 3. **`FuelExhausted` cannot be caught by script-level
    ///    `try`/`catch`.** It unwinds straight through a script's own
    ///    exception handling, exactly like `Recur`, which guarantees the
    ///    HOST always regains control -- a script cannot swallow its own
    ///    exhaustion signal and keep running. [`Error::is_fuel_exhausted`]
    ///    is how the host distinguishes this from an ordinary script error.
    Untrusted,
}

/// The fuel budget [`EngineBuilder::build`] applies to a [`Profile::Untrusted`]
/// engine when [`EngineBuilder::fuel`] was never called -- see that variant's
/// doc for what "fuel" does and does not bound.
const UNTRUSTED_DEFAULT_FUEL: u64 = 10_000_000;

/// Builds an [`Engine`]. `Engine::builder().profile(..).max_depth(..).
/// build()`.
pub struct EngineBuilder {
    profile: Profile,
    max_depth: Option<usize>,
    fuel: Option<u64>,
    module_paths: Option<Vec<PathBuf>>,
}

impl Default for EngineBuilder {
    fn default() -> Self {
        EngineBuilder {
            profile: Profile::Scripting,
            max_depth: None,
            fuel: None,
            module_paths: None,
        }
    }
}

impl EngineBuilder {
    /// Chooses the built-in capability surface (see [`Profile`]'s own doc
    /// for the exact groups each variant registers). Defaults to
    /// [`Profile::Scripting`] -- the same builtin surface `Interp::new()`/
    /// `mova`'s own CLI has.
    pub fn profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    /// Directories searched, in order, for a required namespace's file when
    /// script text this engine evaluates has an `(ns x (:require y.z))` --
    /// `y/z.mova` is looked for in each, exactly like `mova`'s own
    /// `--module-path` CLI flag (`crate::eval::Interp::
    /// module_paths`). Never calling this leaves the default: no search
    /// directories, so a `:require` of anything beyond what's already
    /// bootstrapped fails to resolve.
    pub fn module_paths(mut self, module_paths: Vec<PathBuf>) -> Self {
        self.module_paths = Some(module_paths);
        self
    }

    /// The mova-level call-depth guard (`Interp`'s default is 200, tuned
    /// for a modest OS-default thread stack -- raise it if `Engine::eval`
    /// runs on a thread with a larger stack, same tradeoff `mova`'s own
    /// CLI makes on its dedicated eval thread).
    pub fn max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = Some(max_depth);
        self
    }

    /// Sets the PER-EVAL fuel budget (`crate::eval::Interp::fuel`): every
    /// call to [`Engine::eval`]/[`Engine::eval_named`]/[`Engine::call`]/
    /// [`Engine::call_by_name`] resets the interpreter's remaining fuel to
    /// this budget before running, so a long-lived `Engine` serving many
    /// calls over its lifetime is never left starved by a previous call's
    /// consumption -- each call gets the full budget, independently.
    ///
    /// Never calling this means unlimited fuel for [`Profile::Pure`]/
    /// [`Profile::Scripting`] (the historical default, unchanged); see
    /// [`Profile::Untrusted`]'s doc for what happens when this is left
    /// unset on THAT profile specifically.
    pub fn fuel(mut self, fuel: u64) -> Self {
        self.fuel = Some(fuel);
        self
    }

    /// Finishes construction: registers the chosen [`Profile`]'s builtins,
    /// bootstraps `core.mova` (and, for `Scripting`, `core/async.mova`/
    /// `core/flow.mova`), and applies whatever `max_depth`/`fuel`/
    /// `module_paths` were configured above.
    pub fn build(self) -> Engine {
        let caps = match self.profile {
            Profile::Pure | Profile::Untrusted => Capabilities::CORE_ONLY,
            Profile::Scripting => Capabilities::ALL,
        };
        let budget = match self.profile {
            Profile::Untrusted => Some(self.fuel.unwrap_or(UNTRUSTED_DEFAULT_FUEL)),
            Profile::Pure | Profile::Scripting => self.fuel,
        };
        let mut interp = Interp::with_capabilities(caps, self.max_depth);
        interp.fuel = budget;
        if let Some(module_paths) = self.module_paths {
            interp.module_paths = module_paths;
        }
        // field4/W-LENS-1: the engine-count gauge (a priced liability --
        // see `RootGlobals`' doc §3). A `Relaxed` `fetch_add` at engine
        // CONSTRUCTION is not a hot path by any definition.
        crate::lens::ENGINES_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::lens::mark_start();
        Engine {
            interp,
            budget,
            lens_warn: None,
            keep_script_frame: false,
        }
    }
}

/// An embedded mova interpreter. Not `Clone`: it owns one `Interp` (global
/// env, namespaces, call stack) and every `eval`/`call` mutates it, exactly
/// like a real Clojure runtime session.
///
/// `Engine: Send` (asserted below by a compile-time check): a whole engine
/// may be moved to another thread. It is deliberately NOT `Sync` --
/// `eval`/`call`/`register_fn` all take `&mut self`, so there is no way to
/// call through a shared `&Engine` from multiple threads at once in the
/// first place. The intended multithreaded pattern is snapshot-PER-WORKER
/// ([`Engine::snapshot`], cheap: 84µs-0.79ms per the probe data in
/// `EMBED-API-PLAN.md` §2.2), not sharing one `Engine` behind a
/// `Mutex`/`RwLock` -- that would just serialize every worker onto the same
/// interpreter instead of giving each one an independent world to mutate
/// freely.
///
/// ## Shutdown and background threads
///
/// Script text this engine evaluates can spawn real OS threads --
/// `future`/`go`/`go-loop`/`thread` (`Profile::Scripting`'s `conc` group)
/// and `core.async.flow` (`flow/create-flow` + `flow/start`, the `flow`
/// group). Three facts every host embedding this crate needs before
/// relying on any of that, measured and pinned by `tests/embed_drop_test.rs`
/// and `EMBED-API-PLAN.md` §2.2:
///
/// 1. **Dropping an `Engine` never stops or joins anything.** There is
///    deliberately no `Drop` impl here beyond the default (which just drops
///    `interp`, itself ~300-550ns regardless of live background work --
///    see the drop-lifecycle probe). Any `future`/`go`/`flow` thread the
///    script spawned was spawned DETACHED and keeps running against the
///    now-orphaned globals until it finishes on its own or the process
///    exits -- whichever comes first. A `Drop` impl that silently blocked
///    for up to 5 seconds PER FLOW (see point 2) joining background work
///    behind an innocuous-looking `drop(engine)` would be a footgun, not a
///    convenience; this crate chooses instant, silent drop plus loud
///    documentation instead.
/// 2. **Call [`Engine::shutdown`] to stop flows.** It is the only join path
///    this crate provides, and it is bounded (5 seconds per proc thread,
///    then detach -- exactly `flow/stop`'s own timeout, see that fn's doc
///    in `crate::builtins::flow`).
/// 3. **`future`/`go` are fire-and-forget by design in v1.** There is no
///    cancellation primitive for them at all -- not from script, not from
///    the host, not via `shutdown` (which only ever touches the flow
///    registry). A host that needs a script-spawned `future`/`go-loop` to
///    wind down cooperatively has to build that into the SCRIPT: have the
///    spawned body poll an atom (`(while (not @stop?) ...)`, host flips
///    `stop?` via [`Engine::call_by_name`]/an atom obtained through
///    [`Engine::get`]) or `<!!`/`alts!!` off a control channel it takes
///    from before looping. Neither primitive exists at the language level
///    today -- this is a documented v1 limitation, not an oversight (see
///    `EMBED-API-PLAN.md` Phase C).
pub struct Engine {
    interp: Interp,
    /// The per-eval fuel budget from `EngineBuilder::fuel`/`Engine::set_fuel`
    /// (or `Profile::Untrusted`'s default) -- reapplied to `interp.fuel`
    /// before every `eval`/`eval_named`/`call`/`call_by_name`. `None` means
    /// unlimited.
    budget: Option<u64>,
    /// field4/W-LENS-1: this engine's watchdog callback, or `None`.
    ///
    /// **Per-engine, and a snapshot clone does NOT inherit it** -- see
    /// [`Engine::on_lens_warning`] for the contract and why. `Arc<dyn Fn>`
    /// rather than `Box` so the field stays cheap to hold and the host may
    /// keep its own handle to the same closure.
    lens_warn: Option<Arc<dyn Fn(Value) + Send + Sync>>,
    /// The interactive REPL keeps its script bindings across evals (see `keep_script_bindings`).
    keep_script_frame: bool,
}

const _: () = {
    fn assert_send<T: Send>() {}
    #[allow(dead_code)]
    fn engine_is_send() {
        assert_send::<Engine>();
    }
};

/// The vars `clojure.main` binds around a script, so `(set! *warn-on-reflection* true)`
/// works at top level (`set!` of a var with no thread binding throws, as on the JVM).
const SCRIPT_BOUND_VARS: &[&str] = &[
    "*ns*",
    "*warn-on-reflection*",
    "*math-context*",
    "*print-meta*",
    "*print-length*",
    "*print-level*",
    "*print-namespace-maps*",
    "*data-readers*",
    "*default-data-reader-fn*",
    "*compile-path*",
    "*command-line-args*",
    "*unchecked-math*",
    "*assert*",
    "*1",
    "*2",
    "*3",
    "*e",
];

/// Whether `name` is one of the vars `clojure.main` binds around a script.
pub(crate) fn is_script_bound_var(name: &str) -> bool {
    SCRIPT_BOUND_VARS.contains(&name)
}

/// Opens the script frame: `set!` of a [`SCRIPT_BOUND_VARS`] var binds it on first use.
/// Returns true when this call opened it (a nested eval leaves the outer frame alone).
fn begin_script_frame(interp: &mut crate::eval::Interp) -> bool {
    if interp.script_frame.is_some() {
        return false;
    }
    interp.script_frame = Some(Vec::new());
    true
}

fn end_script_frame(interp: &mut crate::eval::Interp, opened: bool, keep: bool) {
    if !opened || keep {
        return;
    }
    if let Some(bound) = interp.script_frame.take() {
        for cell in bound.iter().rev() {
            cell.pop_binding();
        }
    }
}

impl Engine {
    /// Starts building an [`Engine`]: `Engine::builder().profile(..).
    /// max_depth(..).build()`.
    pub fn builder() -> EngineBuilder {
        EngineBuilder::default()
    }

    /// Makes the script bindings (`set!` of `*print-length*` and the like) last across
    /// `eval`/`eval_named` calls, as in a `clojure.main` REPL, instead of ending with each call.
    pub fn keep_script_bindings(&mut self) {
        self.keep_script_frame = true;
    }

    /// Evaluates `src` (one or more top-level forms) and returns the last
    /// form's value. Equivalent to `eval_named("repl", src)`.
    pub fn eval(&mut self, src: &str) -> Result<Value, Error> {
        self.eval_named("repl", src)
    }

    /// Like [`Engine::eval`], but `name` becomes the diagnostic source
    /// name (what a rendered error's snippet header shows) instead of the
    /// default `"repl"` -- useful when `src` came from a real file or a
    /// host-side template the user should recognize in an error.
    pub fn eval_named(&mut self, name: &str, src: &str) -> Result<Value, Error> {
        self.interp.fuel = self.budget;
        let opened = begin_script_frame(&mut self.interp);
        let r = self.interp.eval_str(name, src);
        end_script_frame(&mut self.interp, opened, self.keep_script_frame);
        match r {
            Ok(v) => Ok(Value::wrap(v)),
            Err(e) => Err(self.wrap_err(e)),
        }
    }

    /// Like [`Engine::eval`], but binds mova's core `*out*`/`*err*` vars
    /// (`core/core.mova`, next to `with-out-str`/`with-err-str`) to fresh
    /// capture buffers for the duration of `src` and returns everything
    /// they collected alongside the ordinary eval result: `(result,
    /// stdout, stderr)`.
    ///
    /// `stdout` collects whatever `print`/`println`/`pr`/`prn`
    /// (`crate::builtins::strings::out_write`) would otherwise send to the
    /// real process stdout; `stderr` collects every interpreter warning
    /// (reflection, boxed-math, `def`-shadowing, the non-dynamic-earmuff
    /// notice, `intern`/`refer`'s "already refers to") that
    /// `crate::builtins::nsfns::write_shim_err` would otherwise send to
    /// the real process stderr. Nothing is EVER silently dropped: a
    /// warning or a print always lands in one of the two returned strings,
    /// never in the process's real stdout/stderr, and never vanishes --
    /// [`Engine::eval`]/[`Engine::eval_named`] are the calls to reach for
    /// when a script's real-stream side effects should stay real; this one
    /// is for a host that wants to capture and inspect them instead
    /// (rendering them in a UI, folding them into a structured log, an
    /// asserting test harness, ...).
    ///
    /// Implemented by pushing a fresh `(atom "")` onto each var's dynamic
    /// binding stack directly (`crate::env::VarCell::push_binding`,
    /// the exact mechanism the `binding` special form itself uses --
    /// see `crate::eval::special_forms`'s `eval_binding`) around the
    /// `eval_str` call, rather than wrapping `src` in a `(binding [*out*
    /// ...] ...)` text form: `src` may be more than one top-level form,
    /// and a textual wrap would need to parse `src` into forms first to
    /// splice a wrapper around all of them -- pushing/popping the binding
    /// from Rust works uniformly for any `src`, with no parsing of its own.
    /// The bindings are always popped before this method returns, even
    /// when `src` errors (mirroring `eval_binding`'s own "popped on ANY
    /// exit" discipline) -- a capture never leaks into a later `eval`/
    /// `call` on this same `Engine`.
    ///
    /// ```
    /// use mova::embed::{Engine, Profile};
    ///
    /// let mut engine = Engine::builder().profile(Profile::Pure).build();
    /// let (result, stdout, stderr) = engine.eval_capture(r#"(println "hi") (+ 1 2)"#);
    /// assert_eq!(result.unwrap().as_i64(), Some(3));
    /// assert_eq!(stdout, "hi\n");
    /// assert_eq!(stderr, "");
    /// ```
    pub fn eval_capture(&mut self, src: &str) -> (Result<Value, Error>, String, String) {
        self.interp.fuel = self.budget;
        let out_cell = self.interp.globals.intern(&crate::value::Symbol::simple("*out*"));
        let err_cell = self.interp.globals.intern(&crate::value::Symbol::simple("*err*"));
        let out_atom = Arc::new(crate::value::AtomCell::new(RawValue::Str(crate::value::Str::from(""))));
        let err_atom = Arc::new(crate::value::AtomCell::new(RawValue::Str(crate::value::Str::from(""))));
        out_cell.push_binding(RawValue::Atom(out_atom.clone()));
        err_cell.push_binding(RawValue::Atom(err_atom.clone()));
        let opened = begin_script_frame(&mut self.interp);
        let result = self.interp.eval_str("repl", src);
        end_script_frame(&mut self.interp, opened, self.keep_script_frame);
        // Popped unconditionally, on both `Ok` and `Err`, before this
        // method returns at all -- see the doc above.
        err_cell.pop_binding();
        out_cell.pop_binding();
        let stdout = match &crate::sync::lock_mutex(&out_atom.state).1 {
            RawValue::Str(s) => s.to_string(),
            _ => String::new(),
        };
        let stderr = match &crate::sync::lock_mutex(&err_atom.state).1 {
            RawValue::Str(s) => s.to_string(),
            _ => String::new(),
        };
        let wrapped = match result {
            Ok(v) => Ok(Value::wrap(v)),
            Err(e) => Err(self.wrap_err(e)),
        };
        (wrapped, stdout, stderr)
    }

    /// Looks up a global by name, `"name"` or `"ns/name"` (the same
    /// resolution order `Engine::eval`'d code itself uses -- see
    /// `crate::ns`'s module doc). `None` if unbound.
    pub fn get(&self, name: &str) -> Option<Value> {
        let sym = crate::reader::parse_symbol(name);
        self.interp.lookup_global(&sym).map(Value::wrap)
    }

    /// Calls `f` (a fn/native `Value` -- typically one returned by `eval`
    /// or `get`) with `args`. Like `eval`/`eval_named`, this resets the
    /// engine's fuel to the configured per-eval budget first -- a `call`
    /// gets the same fresh-budget-per-invocation treatment as a script
    /// eval, not a shared/decaying allowance across repeated calls.
    pub fn call(&mut self, f: &Value, args: &[Value]) -> Result<Value, Error> {
        self.interp.fuel = self.budget;
        let raw_args: Vec<RawValue> = args.iter().map(|v| v.inner().clone()).collect();
        match self.interp.call(f.inner(), &raw_args) {
            Ok(v) => Ok(Value::wrap(v)),
            Err(e) => Err(self.wrap_err(e)),
        }
    }

    /// `call`, looking `name` up first (`"name"` or `"ns/name"`, same as
    /// [`Engine::get`]). Errors (rather than panics) if `name` is unbound.
    pub fn call_by_name(&mut self, name: &str, args: &[Value]) -> Result<Value, Error> {
        let f = self
            .get(name)
            .ok_or_else(|| Error::other(format!("unresolved var: {name}")))?;
        self.call(&f, args)
    }

    /// Deep-realizes `v` (`crate::eval::Interp::realize_deep`): forces any
    /// lazy-seq chain reachable from `v` -- iteratively, recursing into
    /// nested collection elements -- into concrete data. `Engine::eval`/
    /// `call` deliberately do NOT do this automatically (matching every
    /// other mova evaluation path -- `pr_str`/`=` don't auto-realize a
    /// lazy-seq either, see `builtins::collections`'s module doc), so a host
    /// that wants `Display`/`Debug` on the result to show real elements
    /// instead of `#<lazy-seq>` calls this explicitly first, exactly like
    /// `mova`'s own CLI/REPL does before printing. Errors (capped at
    /// 100,000 realized elements) the same way an infinite/huge lazy seq
    /// would if the host tried to print it directly.
    pub fn realize(&mut self, v: &Value) -> Result<Value, Error> {
        match self.interp.realize_deep(v.inner()) {
            Ok(v) => Ok(Value::wrap(v)),
            Err(e) => Err(self.wrap_err(e)),
        }
    }

    /// Binds `name` (`"name"` or `"ns/name"`, same resolution rules as
    /// [`Engine::get`]) directly to `value` as a global -- no script text
    /// involved, so this works for ANY `Value`, including ones that don't
    /// have valid re-readable `pr_str` syntax (a fn, an atom, a channel) and
    /// preserves the exact value's identity (an atom bound this way is the
    /// SAME atom a later `swap!` through script mutates), unlike round-
    /// tripping it through `eval` as printed source text would. The
    /// embedder-facing equivalent of a top-level `(def name value)`: a host
    /// wiring up a config object, a callback table, or (as `mova`'s own
    /// REPL uses it for `*1`/`*2`/`*3`) carrying a just-computed result
    /// forward as a script-visible global without a print/read round trip.
    /// Heap-image gate-1 probe hook (src/image.rs). Not a public API.
    #[doc(hidden)]
    pub fn image_interp_mut(&mut self) -> &mut crate::eval::Interp {
        &mut self.interp
    }

    /// Defines a global (the doc above `image_interp_mut` belongs here).
    pub fn def(&mut self, name: &str, value: Value) {
        let sym = crate::reader::parse_symbol(name);
        let raw = value.into_inner();
        // a var the REPL holds a thread binding for (`keep_script_bindings`) must show the new value
        if let Some(cell) = self.interp.globals.find_any_cell(&sym) {
            cell.set_binding(raw.clone());
        }
        self.interp.globals.set(sym, raw);
    }

    /// Changes the per-eval fuel budget after construction -- same
    /// semantics as [`EngineBuilder::fuel`], applied starting with the NEXT
    /// `eval`/`eval_named`/`call`/`call_by_name` (the currently-remaining
    /// fuel, if any evaluation is not literally in progress, is overwritten
    /// at that point, not before). `None` removes the budget entirely
    /// (unlimited fuel from here on); `Some(n)` sets or replaces it --
    /// including moving a [`Profile::Untrusted`] engine off its default
    /// 10,000,000 onto a different explicit number, or turning it off
    /// (`None`) if the host has decided it no longer needs the limit for
    /// this engine.
    pub fn set_fuel(&mut self, fuel: Option<u64>) {
        self.budget = fuel;
    }

    /// Builds an independent clone of this engine's whole world -- the
    /// "one-engine-per-worker" concurrency primitive alluded to in
    /// `Engine`'s own doc. Wraps `crate::eval::Interp::snapshot`'s FORK
    /// semantics and carries this engine's `Profile`-driven fuel
    /// configuration along with it.
    ///
    /// - **Var isolation**: a `def`/redefinition done through the snapshot
    ///   afterwards is invisible to `self`, and vice versa -- each side gets
    ///   its own re-wrapped var cell per global, not a shared table.
    /// - **Atoms stay Arc-shared, by design**: an atom captured inside a
    ///   value that was already `def`'d before the snapshot is the SAME
    ///   atom on both sides afterwards -- `swap!` through either side is
    ///   visible through the other. This matches real Clojure
    ///   reference-identity semantics (`(identical? a a)` survives a
    ///   snapshot); `snapshot` forks the var TABLE, not the heap reachable
    ///   from it.
    /// - **Remaining fuel is copied independently**: the clone starts with
    ///   whatever fuel `self` currently has left over from its last
    ///   eval/call (not reset to the full configured budget), but from that
    ///   point on each engine's fuel ticks entirely on its own -- consuming
    ///   one side's budget never touches the other's. The configured
    ///   per-eval budget itself (`EngineBuilder::fuel`/`Engine::set_fuel`,
    ///   including a [`Profile::Untrusted`] engine's default) carries over
    ///   too, so the snapshot's OWN next eval still resets to that same
    ///   budget, just as `self`'s would.
    /// - **Caller contract**: like `Interp::snapshot`, this assumes no other
    ///   thread is concurrently `def`ing through this engine while the
    ///   snapshot walks its globals -- snapshot BEFORE spawning workers,
    ///   not while they are already running against the same live `Engine`.
    pub fn snapshot(&self) -> Engine {
        crate::lens::ENGINES_SNAPSHOTTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Engine {
            interp: self.interp.snapshot(),
            budget: self.budget,
            // field4/W-LENS-1: deliberately NOT inherited -- see
            // `Engine::on_lens_warning`'s contract note. The host
            // re-registers on each clone (one line in its engine factory).
            lens_warn: None,
            keep_script_frame: false,
        }
    }

    /// field4/W-LENS-1: the regret ledger as a data `Value` -- the same EDN
    /// map the `(runtime-report)` script fn returns, and the hand-off point
    /// to a host's own telemetry (it scrapes on its own schedule, tags the
    /// map with context the engine cannot know, and ships it through its own
    /// pipeline). The engine never writes a file and never opens a socket.
    ///
    /// The ledger is **process-wide**, not per-engine: counters live in
    /// thread-local pages summed across every thread that ever ran mova
    /// code, so two `Engine`s in one process see the same numbers. That is
    /// deliberate -- regret is a property of the RUNTIME, and a host running
    /// one engine per window would otherwise have to sum the windows itself
    /// to answer "is this build slow". The per-engine facts that ARE
    /// per-engine (`:lens.gauge/globals-retired`) are read from `self`.
    ///
    /// Counters are MONOTONE `u64`s and the schema is versioned
    /// (`:lens/schema`, semver, additive = minor) -- windowing is the
    /// consumer's subtraction, and a consumer MUST tolerate unknown keys.
    /// See `docs/W-LENS-SCHEMA.md` for every key and its meaning.
    ///
    /// Calling this also SAMPLES the watchdog: any threshold crossed since
    /// the last call is delivered to the callback registered with
    /// [`Engine::on_lens_warning`], once per site per crossing.
    pub fn lens_report(&mut self) -> Value {
        self.lens_report_inner(false)
    }

    /// [`Engine::lens_report`] plus a windowing reset: the returned map's
    /// `:lens/window` sub-map is relative to the PREVIOUS reset, and this
    /// call records a new baseline for the next one.
    ///
    /// A reset never writes another thread's counter page (that would be
    /// the shared read-modify-write the whole substrate exists to avoid,
    /// and it would break the monotone contract): it records a baseline
    /// that the report subtracts. `:lens/events` keeps climbing forever.
    /// The baseline is process-wide, like the counters.
    pub fn lens_report_reset(&mut self) -> Value {
        self.lens_report_inner(true)
    }

    fn lens_report_inner(&mut self, reset: bool) -> Value {
        let report = crate::lens::report(reset, self.interp.globals.retired_root_maps());
        if let Some(cb) = self.lens_warn.clone() {
            for w in crate::lens::check_thresholds(&crate::lens::totals()) {
                cb(Value::wrap(w.value));
            }
        }
        Value::wrap(report)
    }

    /// field4/W-LENS-1: registers this engine's watchdog callback -- "tell
    /// me when I'm not optimal". `cb` receives one EDN map per threshold
    /// crossing (`:lens.warn/kind`, `:lens.warn/subject`,
    /// `:lens.warn/count`, `:lens.warn/threshold`, `:lens.warn/message`),
    /// shaped so a host telemetry layer can hand it straight to its own
    /// journal without translation.
    ///
    /// Three contract points, all deliberate:
    ///
    /// 1. **Per-engine, NOT inherited by a snapshot clone.**
    ///    [`Engine::snapshot`] hands back an engine with no callback; the
    ///    host re-registers on each clone (one line in its engine factory).
    ///    Inheriting would silently fan one host closure out across every
    ///    window/plugin engine, which is a delivery topology the host --
    ///    not this crate -- must choose.
    /// 2. **Rate-limited: once per site per threshold crossing.** State is
    ///    process-wide, so a given fn warns once, not once per engine.
    /// 3. **Sampled at report time, not fired mid-loop.** The callback is
    ///    invoked from [`Engine::lens_report`]/[`Engine::lens_report_reset`]
    ///    (which a host drives on its own cadence anyway). Firing from the
    ///    hit path would mean a threshold COMPARE on the hit path, which is
    ///    exactly the cost the W-LENS overhead gate forbids.
    ///
    /// Thresholds themselves are placeholder constants
    /// (`crate::lens::thresholds`) pending the owner's ruling on the
    /// numbers; treat the current values as provisional.
    pub fn on_lens_warning(&mut self, cb: impl Fn(Value) + Send + Sync + 'static) {
        self.lens_warn = Some(Arc::new(cb));
    }

    /// Stops every flow this engine's script created (via `flow/create-flow`
    /// -- through `self` directly, or through a `future*`-spawned sibling
    /// thread, which shares `self`'s flow registry; see
    /// `crate::eval::Interp::flow_registry`'s doc) and is still alive and
    /// still `Running`. See `Engine`'s own "Shutdown and background
    /// threads" doc section for the full lifecycle story this is one part
    /// of.
    ///
    /// Reuses the exact stop machinery `flow/stop` itself calls
    /// (`crate::builtins::flow::stop_flow_cell`, invoked directly against
    /// each tracked flow) rather than evaluating `(flow/stop ...)` as
    /// Clojure source -- so this works correctly even on an engine with no
    /// `flow` capability registered at all ([`Profile::Pure`]/
    /// [`Profile::Untrusted`], whose registry is simply always empty since
    /// `flow/create-flow` never runs), and cannot be confused by a script
    /// that has shadowed or redefined `flow/stop`.
    ///
    /// A flow already stopped, never started (`flow/create-flow` without a
    /// matching `flow/start`), or already dropped by the script (no live
    /// reference anywhere -- the registry holds only a `Weak`, see the
    /// field doc) counts toward NEITHER report field: those are silently
    /// skipped, matching `flow/stop`'s own idempotence. A stop attempt that
    /// panics counts toward `flows_failed` and does not abort the rest of
    /// the walk.
    ///
    /// **Bounded, not instant**: each stop inherits `flow/stop`'s existing
    /// per-proc join -- up to 5 SECONDS waiting for that proc's OS thread to
    /// exit before giving up and detaching it (the thread keeps running
    /// against the now-closed chans). With several tracked flows this call
    /// can take up to `(flow count) * 5s` in the worst case (every proc of
    /// every flow wedged); there is no cross-flow parallelism, matching
    /// `flow/stop`'s own sequential join loop.
    ///
    /// **Idempotent**: this call clears the registry as it goes, so a
    /// second call finds it empty and returns
    /// `ShutdownReport { flows_stopped: 0, flows_failed: 0 }` immediately.
    /// The `Engine` remains fully usable afterwards -- `eval`/`call`/
    /// `register_fn` are untouched by this method; a script can even
    /// `flow/create-flow` a brand new flow post-shutdown and a later
    /// `shutdown()` call will find and stop that one.
    pub fn shutdown(&mut self) -> ShutdownReport {
        let tracked: Vec<Weak<FlowCell>> = {
            let mut registry = crate::sync::lock_mutex(&self.interp.flow_registry);
            std::mem::take(&mut *registry)
        };
        let mut report = ShutdownReport::default();
        for weak in tracked {
            // Already dropped by the script (no live reference anywhere):
            // neither stopped nor failed, just gone. Skip silently.
            let Some(cell) = weak.upgrade() else { continue };
            // `stop_flow_cell` is ordinary, non-panicking Rust under
            // correct operation (see its own doc), but a shutdown sweep is
            // exactly the wrong place to let one wedged/misbehaving flow
            // (e.g. a poisoned lock from an earlier panic elsewhere) abort
            // every flow after it -- so each stop is individually isolated.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::builtins::flow::stop_flow_cell(&cell))) {
                Ok(true) => report.flows_stopped += 1,
                Ok(false) => {} // already stopped / never started: skip silently
                Err(_) => report.flows_failed += 1,
            }
        }
        report
    }

    /// Registers a host-side Rust closure as a global mova fn callable
    /// from script as `(name args...)`. `name` may be namespaced
    /// (`"db/lookup"`), exactly like a script-visible `(defn db/lookup
    /// ...)` would be -- see `crate::ns`'s module doc for why a qualified
    /// name needs no `require`/alias to be reachable: the qualified
    /// candidate is probed directly against the exact spelling it was
    /// registered under.
    ///
    /// Places no constraint on the argument count -- equivalent to
    /// [`Engine::register_fn_with_arity`] with [`Arity::Any`]. Use
    /// `register_fn_with_arity` directly for a checked arity with a
    /// generated error message instead of hand-rolling an `args.len()`
    /// check inside `f`.
    pub fn register_fn(
        &mut self,
        name: &str,
        f: impl Fn(&[Value]) -> Result<Value, Error> + Send + Sync + 'static,
    ) {
        self.register_fn_with_arity(name, Arity::Any, f);
    }

    /// Like [`Engine::register_fn`], but with an explicit [`Arity`] check
    /// applied BEFORE `f` runs: a script call with the wrong argument count
    /// gets a normal mova arity `Error` naming `name` (the same shape a
    /// builtin's own arity mismatch produces, see `crate::builtins::reg`),
    /// instead of `f` having to validate `args.len()` itself and invent its
    /// own message.
    pub fn register_fn_with_arity(
        &mut self,
        name: &str,
        arity: Arity,
        f: impl Fn(&[Value]) -> Result<Value, Error> + Send + Sync + 'static,
    ) {
        let sym = crate::reader::parse_symbol(name);
        let err_name: Box<str> = name.into();
        let native = NativeFn::new(name.to_string(), move |interp: &mut Interp, args: &[RawValue]| {
            if !arity.matches(args.len()) {
                return Err(RjError::arity(format!(
                    "{err_name}: expected {}, got {}",
                    arity.expected_desc(),
                    args.len()
                ))
                .with_stack(interp.stack_snapshot(), interp.source_id));
            }
            let wrapped: Vec<Value> = args.iter().cloned().map(Value::wrap).collect();
            f(&wrapped).map(Value::into_inner).map_err(Error::into_rj_error)
        });
        self.interp.globals.set_builtin(sym, RawValue::Native(Arc::new(native)));
        // Idempotent (`crate::ns::Interp::seed_builtin_namespaces`): marks
        // a newly-registered namespace (e.g. "db" above) as already
        // provided, so a script's `(:require [db ...])` -- if it ever
        // writes one -- records the alias instead of hunting the module
        // path for a file that will never exist.
        self.interp.seed_builtin_namespaces();
    }

    /// Like [`Engine::register_fn_with_arity`], but `f` additionally gets a
    /// [`Reentry`] handle onto the **in-flight interpreter**, so a host
    /// native can synchronously call back INTO script while it runs --
    /// `(host-native f 1 2)` where the native's Rust body does
    /// `reentry.call(&args[0], &args[1..])`. Without this, a registered
    /// native is a leaf: it can compute and return, but it cannot invoke a
    /// script closure it was handed (or stashed earlier), because
    /// [`Engine::call`] needs `&mut Engine` and the engine is already
    /// borrowed by the very eval that is running the native.
    ///
    /// The motivating shape is a host event pump: a native stores a script
    /// closure in a host-side cell, and a later native -- possibly re-entered
    /// from an FFI callback on the same thread, mid-eval -- reads that cell
    /// and calls it. Same `Interp`, same globals, same call stack; script
    /// frames raised by the reentrant call nest inside the outer call's stack
    /// exactly as if script had made the call itself.
    ///
    /// **Fuel is SHARED with the in-flight eval, not refreshed.** This is the
    /// one deliberate semantic difference from [`Engine::call`] and the known
    /// hazard of opening this door -- see [`Reentry`]'s own doc for the full
    /// argument. In short: `Engine::call` resets `interp.fuel` to the
    /// configured budget because it starts a new top-level metered call;
    /// `Reentry::call` cannot, because it runs INSIDE one that is already
    /// being metered. Refreshing there would let any script that can reach a
    /// reentrant native mint itself an unlimited budget by recursing through
    /// it, which would silently void [`Profile::Untrusted`]'s only real
    /// guarantee.
    ///
    /// The arity check behaves exactly as in
    /// [`Engine::register_fn_with_arity`]: it fires before `f` runs, with the
    /// same generated message naming `name`.
    pub fn register_fn_with_reentry(
        &mut self,
        name: &str,
        arity: Arity,
        f: impl Fn(&mut Reentry<'_>, &[Value]) -> Result<Value, Error> + Send + Sync + 'static,
    ) {
        let sym = crate::reader::parse_symbol(name);
        let err_name: Box<str> = name.into();
        let native = NativeFn::new(name.to_string(), move |interp: &mut Interp, args: &[RawValue]| {
            if !arity.matches(args.len()) {
                return Err(RjError::arity(format!(
                    "{err_name}: expected {}, got {}",
                    arity.expected_desc(),
                    args.len()
                ))
                .with_stack(interp.stack_snapshot(), interp.source_id));
            }
            let wrapped: Vec<Value> = args.iter().cloned().map(Value::wrap).collect();
            // The ONLY difference from `register_fn_with_arity`'s wrapper:
            // the `&mut Interp` the native dispatcher hands us is not
            // dropped on the floor, it is lent to `f` for the duration of
            // the call as a `Reentry`. The borrow is scoped to this call --
            // there is no way for `f` to stash the handle (`Reentry<'_>`'s
            // lifetime is the closure's `&mut` parameter, so it cannot
            // outlive this invocation) and re-enter a dead interpreter later.
            let mut reentry = Reentry { interp };
            f(&mut reentry, &wrapped)
                .map(Value::into_inner)
                .map_err(Error::into_rj_error)
        });
        self.interp.globals.set_builtin(sym, RawValue::Native(Arc::new(native)));
        // Same namespace seeding as `register_fn_with_arity` -- see there.
        self.interp.seed_builtin_namespaces();
    }

    fn wrap_err(&self, e: crate::error::RjError) -> Error {
        Error::from_engine(e, self.interp.source_name.as_ref(), self.interp.source.as_ref())
    }
}

/// The in-flight-interpreter handle a
/// [`Engine::register_fn_with_reentry`]-registered native receives: the
/// native→script direction of the embedding boundary, i.e. calling a script
/// fn `Value` from Rust *while a script eval is already on the stack*.
///
/// Borrowed, never owned: a `Reentry` is `&mut Interp` behind a private
/// field, handed to the closure for exactly the duration of one native
/// invocation. It is deliberately not `Clone` and carries no escape hatch to
/// the underlying `Interp` -- the only thing a host can do with it is
/// [`Reentry::call`]. Its lifetime is tied to the `&mut Reentry<'_>`
/// parameter the closure is called with, so it cannot be stashed in a
/// host-side cell and re-entered after the interpreter has moved on (that is
/// what stashing the script `Value` is for -- see the doc on
/// [`Engine::register_fn_with_reentry`]).
///
/// ## Fuel: shared with the in-flight eval, NOT refreshed
///
/// [`Engine::call`] resets the interpreter's remaining fuel to the
/// configured per-eval budget (see [`EngineBuilder::fuel`]) before running,
/// because it *is* the top-level metered call. `Reentry::call` deliberately
/// does not: it runs nested inside an eval that is already being metered, so
/// it continues to spend the SAME counter the outer call is spending. Three
/// consequences a host should have in mind:
///
/// 1. A budget that is comfortable for the outer script and comfortable for
///    the reentrant script *separately* can still be exhausted by the two
///    together -- the split between them is whatever the script chooses, not
///    something the host allots.
/// 2. Exhaustion can surface as an `Err` *inside* the native (from
///    `Reentry::call`), before the outer eval ever gets to see it. That
///    `Error` answers `true` to [`Error::is_fuel_exhausted`], and the
///    idiomatic handling is to propagate it: return it (or any `Err`) from
///    the native so it keeps unwinding.
/// 3. **A reentrant native CAN swallow a fuel exhaustion** -- catch the
///    `Err` and return `Ok` -- which is the one place `FuelExhausted`'s
///    "unwinds straight through script `try`/`catch`" property (see
///    [`Profile::Untrusted`]) does not hold, because the frame doing the
///    swallowing is host Rust, not script. It is not unbounded, though: the
///    interpreter's remaining fuel stays at zero, so the very next checked
///    back-edge or fn-call entry raises it again. Swallowing delays the
///    signal by one step-ish; it cannot buy the script more work. A host
///    that wants the guarantee back propagates every `Err` for which
///    `is_fuel_exhausted()` holds instead of absorbing it.
///
/// The alternative -- giving each reentrant call a fresh budget -- was
/// rejected: script that can reach a reentrant native could then recurse
/// through it to renew its allowance indefinitely, turning a bounded
/// `Profile::Untrusted` engine into an unbounded one with no diagnostic.
///
/// ## Depth and recursion
///
/// A reentrant call nests on the SAME mova call stack as its caller, so
/// `EngineBuilder::max_depth` counts host↔script ping-pong exactly like
/// ordinary script recursion and a runaway native→script→native cycle
/// terminates with the normal depth error rather than blowing the OS stack.
/// Note that the Rust side of each bounce is a real native stack frame too;
/// hosts running deep reentrancy on a small thread stack should size
/// `max_depth` accordingly, the same tradeoff `Engine`'s own doc describes.
pub struct Reentry<'a> {
    /// The live interpreter the native was dispatched on. Private: exposing
    /// it would hand embedders the whole crate-internal `Interp` surface
    /// through the one module that promises a minimal, stable facade.
    interp: &'a mut Interp,
}

impl Reentry<'_> {
    /// Calls `f` (a fn/native `Value` -- typically one passed in as an
    /// argument to this native, or stashed by an earlier one) with `args`,
    /// on the SAME in-flight interpreter, and returns its result.
    ///
    /// Mechanically identical to [`Engine::call`] -- same
    /// `crate::eval::Interp::call` entry point, same argument wrapping, same
    /// source-context error wrapping, so [`Error::render_plain`] on a failure
    /// shows the same labeled snippet a top-level call would -- with one
    /// deliberate exception: **no per-call fuel reset**. The remaining budget
    /// of the eval that is already running is what this call spends; see
    /// [`Reentry`]'s own doc for why, and for the swallowing caveat.
    ///
    /// A script-level `throw` inside `f` comes back as an ordinary `Err`
    /// here, carrying the thrown value; returning it from the native
    /// propagates it to the outer script unchanged, where a script `catch`
    /// can still handle it.
    pub fn call(&mut self, f: &Value, args: &[Value]) -> Result<Value, Error> {
        let raw_args: Vec<RawValue> = args.iter().map(|v| v.inner().clone()).collect();
        let result = self.interp.call(f.inner(), &raw_args);
        match result {
            Ok(v) => Ok(Value::wrap(v)),
            // `Engine::wrap_err`'s body, spelled out: a `Reentry` has the
            // `Interp` but not the `Engine`, and the source context comes
            // from the interpreter anyway (it is whatever text is currently
            // loaded -- i.e. the script that reached this native).
            Err(e) => Err(Error::from_engine(
                e,
                self.interp.source_name.as_ref(),
                self.interp.source.as_ref(),
            )),
        }
    }

    /// Like [`Engine::eval_named`], but evaluates `src` against the SAME
    /// in-flight interpreter a native was dispatched on -- reentrant the
    /// same way [`Reentry::call`] is: **no per-call fuel reset** (see that
    /// method's doc for why), remaining budget of the eval already running
    /// is what this spends too.
    ///
    /// `name` becomes the diagnostic/`:file` source attribution for
    /// anything `src` defines, exactly like [`Engine::eval_named`] -- and,
    /// same as that method, this PERSISTENTLY overwrites `source_name`/
    /// `source` on the interpreter rather than restoring them afterward.
    /// That is the opposite of the script-level `load-string` builtin
    /// (`builtins::reflect::load_string_native`), which DOES save/restore
    /// around the identical `eval_str` call -- and deliberately so: an
    /// embedder reaching for this method wants the eval to genuinely
    /// become "the current file" for subsequent diagnostics, the same way
    /// loading a new top-level file would; `load-string` invoked mid-file
    /// from SCRIPT must NOT leave later `def`s in that same file
    /// misattributed to the loaded snippet's name.
    pub fn eval_named(&mut self, name: &str, src: &str) -> Result<Value, Error> {
        match self.interp.eval_str(name, src) {
            Ok(v) => Ok(Value::wrap(v)),
            Err(e) => Err(Error::from_engine(
                e,
                self.interp.source_name.as_ref(),
                self.interp.source.as_ref(),
            )),
        }
    }
}

/// What [`Engine::shutdown`] did. Both counts are per-flow (not per-proc):
/// a flow with several procs still counts as ONE toward `flows_stopped`
/// when its stop succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShutdownReport {
    /// Flows this call actually stopped: phase was `Running`, every proc
    /// thread was joined (or, past the bounded 5s-per-proc timeout,
    /// detached -- see [`Engine::shutdown`]'s doc) and every engine-owned
    /// chan/transport link closed.
    pub flows_stopped: usize,
    /// Flows whose stop attempt panicked. Isolated per-flow (one panic
    /// cannot prevent the rest of the shutdown sweep from running) and
    /// counted separately from the silently-skipped "nothing to do" case
    /// (already stopped, never started, or already dropped by the script).
    pub flows_failed: usize,
}

/// How many arguments a [`Engine::register_fn_with_arity`]-registered
/// native accepts. A public, stable, minimal surface -- deliberately NOT a
/// re-export of the crate-internal `builtins::ArityHint` (which is wired
/// into `builtins::reg`'s generated error format and is free to change
/// shape as the builtin-registration machinery evolves); `Arity` checks the
/// same four shapes independently, so the internal type stays free to
/// churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// Exactly `n` arguments.
    Exact(usize),
    /// `n` or more arguments.
    AtLeast(usize),
    /// Between `lo` and `hi` arguments, inclusive.
    Range(usize, usize),
    /// Any number of arguments -- no check at all. What
    /// [`Engine::register_fn`] uses under the hood.
    Any,
}

impl Arity {
    fn matches(self, n: usize) -> bool {
        match self {
            Arity::Exact(k) => n == k,
            Arity::AtLeast(k) => n >= k,
            Arity::Range(lo, hi) => n >= lo && n <= hi,
            Arity::Any => true,
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
            Arity::Exact(k) => format!("exactly {k} argument{}", plural(k)),
            Arity::AtLeast(k) => format!("at least {k} argument{}", plural(k)),
            Arity::Range(lo, hi) => format!("{lo} to {hi} arguments"),
            Arity::Any => "any number of arguments".to_string(),
        }
    }
}
