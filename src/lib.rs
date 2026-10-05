//! mova: a Clojure dialect hosted on Rust. See ARCHITECTURE.md for the
//! design contract this crate implements.

// ARCHITECTURE.md mandates `Result<Value, RjError>` verbatim (e.g. NativeFn's
// exact signature), so `RjError` can't be boxed just to shrink `Result`; the
// error type is intentionally rich (span, label, stack, errno, thrown value).
#![allow(clippy::result_large_err)]

// SPEC-A-bignum.md: Clojure-exact `Ratio`/`BigInt`/`BigDecimal` value
// types. SPEC-B-bignum-wiring.md wires them into `Value::BigInt`/`Ratio`/
// `BigDec` (see `value.rs`), the reader, the printer, and `=`/hash -- this
// module's public surface is now live, not dead code.
pub(crate) mod bignum;
pub(crate) mod builtins;
/// L5 / P6a (docs/L5-VIRTUAL-TIME-DESIGN.md §2, §9): THE clock, the two
/// seeded SplitMix64 streams, and the sim trace writer. Probe scope — see
/// that module's own doc for what is deliberately not here yet.
pub(crate) mod clock;
pub(crate) mod colvec;
pub(crate) mod compile;
// E1a (docs/JIT.md): Cranelift JIT spike for a small scalar subset of
// compiled-tier fns, opt-in via `MOVA_JIT=1`. Always compiled (the `jit`
// cargo feature only gates the Cranelift dependency itself -- see that
// module's doc), so callers never need a `#[cfg(feature = "jit")]`.
pub(crate) mod jit;
// L1/W1 (docs/L1-LANDING-SPEC.md): execution-context identity -- the key
// dynamic `binding` frames hang off, once `go` blocks stop being threads.
pub(crate) mod ctx;
// edn/fast: direct source -> `Value` EDN reader, bypassing `reader.rs`'s
// `Form` tree for the supported subset -- see that module's doc for the
// golden "bail, never diverge" rule. Wired into `builtins::reflect::
// read_string`'s 1-arity path.
pub(crate) mod edn_fast;
// W-GEO kill-probe C: diagnostic-only PVec/PMap op-mix counters, entirely
// `#[cfg(feature = "geo-census")]` -- see that module's doc. The module
// itself is gated too, so it doesn't even exist in an ordinary build.
#[cfg(feature = "geo-census")]
pub(crate) mod geo_census;
#[cfg(feature = "k2-count")]
pub(crate) mod k2count;
// field4/W-LENS-1: the runtime regret ledger -- thread-local counter pages,
// the curated event table, and the versioned EDN report. Always compiled
// (see `docs/W-LENS-DESIGN.md`'s cost budget and the overhead probe); the
// hit path is one TLS load plus one uncontended `Relaxed` load/store.
pub(crate) mod lens;
/// Minimal embeddable-crate facade: `Engine`/`EngineBuilder`/`Profile`, an
/// opaque `Value`, and an `Error` that doesn't require depending on miette
/// directly. See `embed`'s module doc for the full contract.
pub mod embed;
pub(crate) mod env;
/// Error facts for a host (class, root class, phase, location, clojure.main text).
pub mod errinfo;
pub(crate) mod coredocs;
/// Host form-by-form reader with line/column offsets (nREPL `eval`).
pub mod formfeed;
pub mod nrepl;
pub(crate) mod error;
pub(crate) mod native_macros;
pub(crate) mod eval;
// W3 (LATENCY-CAMPAIGN.md): `Value::HostStruct`'s engine-side machinery
// (`Shape`/`HostObj`/inline cache/materialize choke point). Unconditionally
// compiled (no `serde` gate) -- the public registration surface lives at
// `crate::embed::host` instead, mirroring `value`'s own internal/public
// split.
pub(crate) mod host_struct;
pub(crate) mod lazy_map;
pub(crate) mod memstat;
pub mod compact;
pub(crate) mod shaped_map;
/// S5: `java.util.Random`/`java.util.Date`/`Thread`/a narrow `proxy
/// [ThreadLocal]` -- see `Value::HostInst`'s doc for why these four
/// unrelated host classes share one `Value` variant, and this module's
/// own doc for the construction/method-dispatch surface.
pub(crate) mod hostclass;
/// e2: blocking HTTP(S) GET behind java.net.URL / HttpsURLConnection / slurp of an http(s) URL.
pub(crate) mod http;
pub mod image;
pub mod core_image;
pub mod metrics;
pub mod srcindex;
/// W-GEO stage 1: `Value::Keyword`'s payload type and the process-wide
/// capped-permanent intern table behind it -- see that module's doc and
/// `docs/W-GEO-STAGE1-DESIGN.md`.
pub(crate) mod keyword;
/// Not part of the public API; may change or vanish in any release.
///
/// `mova`'s documented public surface is `embed` (+ `serde_bridge` under
/// the `serde` feature) -- see EMBED-API-PLAN.md Phase D. Everything here
/// exists only so `main.rs` (a separate bin target that sees this crate as
/// external, the same way a real embedder would) and the handful of
/// white-box integration tests that inspect compiler IR / raw `Value`
/// variants / `ErrorKind` directly can keep reaching pieces the facade
/// deliberately does not expose. No semver guarantee: a type, fn, or
/// re-export here can be renamed, reshaped, or removed in ANY release,
/// including a patch release, without that counting as a breaking change to
/// `mova` itself. If you are embedding mova in your own crate, use
/// `mova::embed` instead -- everything below is exempt from that
/// module's stability promise on purpose.
#[doc(hidden)]
pub mod internal {
    // PERF-PROBE: `main.rs` (the CLI binary) needs `print_phase_totals`
    // at exit; re-exported here like the rest of the probe surface below.
    pub use crate::load_trace;
    pub use crate::builtins::map_probe;
    /// metrics: JIT flags for the startup event.
    pub fn jit_enabled() -> bool { crate::jit::enabled() }
    pub fn jit_aot_bound() -> u64 { crate::jit::stats().2 }
    pub use crate::error::{render, ErrorKind, RjError};
    pub use crate::eval::Interp;
    pub use crate::printer::{display_str, pr_str};
    pub use crate::keyword::Keyword;
    // `Arity`/`PrimCast`: D9's `tests/prim_hint_test.rs` asserts the SHAPE
    // of the parsed hints (that an unhinted fn keeps `coerce == None`, i.e.
    // that the call-boundary fast path really is one never-taken branch),
    // not just their behaviour -- see that file's module doc.
    pub use crate::value::{Arity, Doorbell, PMap, PVec, PrimCast, Str, Symbol, Value};
    /// clojure-lsp campaign census/bench (`examples/transit_census.rs`):
    /// the same decode path `mova.transit/read-json` runs, exposed for an
    /// example binary that only sees `pub` crate surface.
    pub use crate::builtins::read_json_str;

    /// Lazy tier-up (v0.6): forces `v`'s compile decision now, if it has
    /// not settled already, for a test/tool that needs the def-time
    /// answer for a fn/macro it never called (or that it already CALLED
    /// through `interp` but needs to re-check under a non-default
    /// `MOVA_LAZY_TIER_N`). A thin `pub` wrapper because `compile_fn`
    /// itself is deliberately `pub(crate)` -- see `reader::try_read_edn`'s
    /// doc just above for why a re-export through this module can't do
    /// it. Mirrors `(compile-explain f)`'s own force
    /// (`builtins::meta::compile_explain_value`).
    pub fn force_compile(interp: &mut Interp, v: &Value) {
        if let Value::Fn(rc) | Value::Macro(rc) = v {
            let _ = rc.compiled.on_call(0, || {
                let saved_source = std::mem::replace(&mut interp.source_id, rc.def_source_id.get());
                let r = crate::compile::compile_fn(interp, rc.name.as_ref(), rc.arities.as_slice(), &rc.env, rc.def_span);
                interp.source_id = saved_source;
                r
            });
        }
    }

    /// EDN split probe (`benches/edn_split_probe.rs`, competitor review
    /// 2026-08-23): the reader's two materialization phases -- source ->
    /// `Form` (`read_one`) and `Form` -> `Value` (`form_to_value`) --
    /// measured separately against the edn.c fast-edn corpus, so the 5-8x
    /// end-to-end gap can be attributed before any vectorization work.
    pub mod reader {
        pub use crate::reader::{form_to_value, Form, FormValue, Span};

        /// Heap-image kill-probe (`examples/heap_image_probe.rs`,
        /// `docs/HEAP-IMAGE-DESIGN.md`): read every top-level form of a
        /// `.clj`/`.cljc`/`.mova` file the way `require` does (reader
        /// conditionals allowed, `user` ns context), for codec timing.
        pub fn read_all_allow_cond_user(src: &str) -> Result<Vec<Form>, crate::error::RjError> {
            let mut r = crate::reader::Reader::new_allow_read_cond(src);
            r.set_ns_ctx(crate::reader::NsContext::user());
            let mut out = Vec::new();
            while let Some(f) = r.next_form()? {
                out.push(f);
            }
            Ok(out)
        }

        /// `read-string`'s core read against the default `user` ns context
        /// (the builtin rebuilds its context per call via
        /// `Interp::reader_ns_context`, so building one here per call is
        /// the faithful cost model).
        pub fn read_one_user(
            src: &str,
        ) -> Result<Option<Form>, crate::error::RjError> {
            crate::reader::read_one_with_ns(src, crate::reader::NsContext::user())
        }

        /// edn/fast (`benches/edn_split_probe.rs` phase D, `tests/
        /// edn_fast_test.rs`): the direct-to-`Value` fast path. A thin
        /// `pub` wrapper (same shape as `read_one_user` just above) rather
        /// than `pub use crate::edn_fast::try_read_edn` directly -- that
        /// function is deliberately `pub(crate)` (its one real caller,
        /// `builtins::reflect::read_string`, is inside this crate), and a
        /// bench/test binary sees this crate as EXTERNAL, so re-exporting
        /// a `pub(crate)` item through this `pub mod internal` would be
        /// `E0364`.
        pub fn try_read_edn(src: &str) -> Option<crate::value::Value> {
            crate::edn_fast::try_read_edn(src)
        }
    }

    /// `builtins::async`'s shared `timeout` timer service
    /// (`tests/timer_service_test.rs`): how many timer threads the process
    /// has ever spawned, which is the entire claim that change makes -- 1,
    /// however many `timeout`s get armed. A wrapper fn rather than a
    /// `pub use`, for the same reason `reader::try_read_edn` is one: the
    /// underlying item is deliberately `pub(crate)`, and re-exporting a
    /// `pub(crate)` item through this `pub mod internal` would be `E0364`.
    pub mod async_timer {
        pub fn threads_spawned() -> u64 {
            crate::builtins::r#async::timer_threads_spawned()
        }

        /// Total timer-heap arms ever, over both entry kinds, for
        /// `tests/l35_deref_park_probe.rs` and `tests/l3_future_task_test.rs`
        /// (L3.5 item 1). The probe used it to price the OLD polling
        /// `deref`-with-timeout arm, which re-armed the shared heap once per
        /// 1ms tick per waiter; the gate test now uses it the other way
        /// round, to assert that one timeout-`deref` arms EXACTLY ONE entry.
        /// A wrapper fn for the same `E0364` reason as its sibling above.
        pub fn arms_total() -> u64 {
            crate::builtins::r#async::timer_arms_total()
        }
    }

    /// L5 / P6a: the deterministic-simulation kernel's white-box hooks, for
    /// `src/bin/sim_probe.rs`. Wrapper fns for the same `E0364` reason
    /// `async_timer`'s are: `crate::clock` is `pub(crate)`.
    pub mod sim {
        /// True when `MOVA_SIM_SEED` put this process in sim mode.
        pub fn enabled() -> bool {
            crate::clock::sim_enabled()
        }
        /// Virtual nanoseconds elapsed (0 in real mode).
        pub fn virtual_ns() -> u64 {
            crate::clock::sim_now_ns()
        }
        /// The seed the process (or, inside a `simulate` call, that call) is
        /// running under. Meaningless when [`enabled`] is false.
        pub fn seed() -> u64 {
            crate::clock::sim_seed()
        }
        /// L5/W4: total timer entries fired process-wide — `simulate`'s
        /// `:timer-fires` before it is turned into a per-call delta.
        pub fn timer_fires() -> u64 {
            crate::builtins::r#async::timer_fires_total()
        }
        /// Trace lines emitted so far — poll for quiescence before flushing.
        pub fn trace_events() -> u64 {
            crate::clock::trace_events()
        }
        /// Flush the trace `BufWriter`. A `static` writer is never dropped,
        /// so a driver that does not call this leaves a truncated file.
        pub fn trace_flush() {
            crate::clock::trace_flush()
        }
    }

    /// P6a spec spelling of [`sim::trace_flush`], kept as its own name
    /// because the probe brief names it.
    pub fn sim_trace_flush() {
        crate::clock::trace_flush()
    }

    /// `builtins::flow`'s mult (fan-out) spawn census, for
    /// `tests/l3_task_procs_test.rs` (L3.5 item 2): how many mults this
    /// process has spawned as runtime TASKS and how many as OS THREADS.
    /// The claim it gates is a differential no behavioral test can see --
    /// a fan-out flow works either way -- namely that a fan-out conn costs
    /// an OS thread under `MOVA_FLOW_THREAD_PROCS=1` and costs ZERO of
    /// them by default. A wrapper fn for the same `E0364` reason as
    /// `async_timer`'s.
    pub mod flow_probe {
        /// `(mults spawned as tasks, mults spawned as OS threads)`,
        /// process-global and monotonic -- measure a delta across a
        /// `flow/start`, never an absolute.
        pub fn mult_spawn_census() -> (usize, usize) {
            crate::builtins::flow::mult_spawn_census()
        }
    }

    /// W-GEO stage 1's capped-permanent keyword intern table, for
    /// `tests/geo_intern_probe.rs`'s capped-RSS harness (design doc §2.1's
    /// open item: measure the STEADY-STATE, post-cap-freeze per-entry
    /// footprint against the REAL table, not a probe-local replica).
    pub mod keyword_table {
        pub use crate::keyword::{interned_count, KEYWORD_INTERN_CAP};
    }

    /// W-ENV kill-probe (`tests/globals_snapshot_bench.rs`): the globals
    /// `Env` read path measured directly, plus the `imbl` map type the
    /// candidate snapshot repr is built out of, so the probe's replica
    /// variants are A/B'd against the REAL `Env::get_exact` in the same
    /// process rather than against a hand-modeled stand-in of it.
    pub mod env {
        pub use crate::env::{Env, VarCell};
    }

    /// fix/closure-env-cycles Step 1 (`tests/leak_cycle_probe.rs`): raw
    /// create/drop counters for `Closure` and `EnvInner` so a per-scenario
    /// test can measure whether a self-referential closure (e.g. `letfn`
    /// mutual recursion, named-fn self-recursion) leaks -- created-dropped
    /// > 0 across a warm loop means a reference cycle is holding
    /// frames/closures alive past their last use.
    #[cfg(feature = "leak-probe")]
    pub mod leak_probe {
        pub use crate::env::{FRAME_CREATED, FRAME_DROPPED};
        pub use crate::value::{CLOSURE_CREATED, CLOSURE_DROPPED};
    }

    /// L1/W1 (`tests/ctx_binding_test.rs`): the context id dynamic bindings
    /// are keyed on. Exposed because the ONE thing worth testing about it --
    /// two contexts multiplexed onto a single OS thread keeping separate
    /// binding stacks -- has no Mova-level expression until the scheduler
    /// lands, so the test drives `set_ctx` by hand the way a shard will.
    pub mod ctx {
        pub use crate::ctx::{
            current_ctx, fresh_ctx, install_binding_locals, set_ctx, swap_ctx,
            take_binding_locals, BindingLocals,
        };
    }
    /// See [`env`] -- the probe's snapshot variants need the same
    /// persistent maps the real repr would be built out of.
    pub use {champ, imbl};

    /// field4/W-LENS-1 (`tests/lens_test.rs`): the regret ledger's
    /// substrate, so the white-box unit tests can drive events, allocate
    /// sites, and read the aggregated totals without going through a
    /// script. The PUBLIC surface is `embed::Engine::lens_report` and the
    /// `(runtime-report)` builtin; this is the same no-semver-guarantee
    /// escape hatch every other `internal` re-export is.
    pub mod lens {
        pub use crate::lens::{
            alloc_site, check_thresholds, event, event_at, mode, report, totals, warning_lines,
            Event, Mode, SiteKind, Totals, MAX_SITES, NO_SITE, SCHEMA_VERSION,
        };

        /// Wraps a raw report `Value` in the embed facade's opaque `Value`,
        /// so a test can read the report with the SAME accessors a host
        /// uses (`get_kw`/`entries`/`as_str`) instead of duplicating them
        /// against the internal enum.
        pub fn as_embed_value(v: crate::value::Value) -> crate::embed::Value {
            crate::embed::Value::wrap(v)
        }
    }

    /// W-GEO kill-probe C (`tests/geo_census_probe.rs`): the PVec/PMap
    /// op-mix counters, exposed only when the crate itself is built with
    /// `--features geo-census` (an ordinary build has no `geo_census`
    /// module to re-export here at all).
    #[cfg(feature = "geo-census")]
    pub mod geo_census {
        pub use crate::geo_census::{snapshot, Snapshot};
    }

    /// L1/W3 (`tests/task_chan_test.rs`): the raw `Chan` primitives the task
    /// park arms live in, plus the real blocking-`alts!!` protocol and the
    /// `Doorbell`. Thin wrappers because `chan_put`/`chan_take`/`chan_close`
    /// are `pub(crate)` and an integration test is a separate crate (the
    /// same `E0364` reason `reader::try_read_edn`'s wrapper exists). W3
    /// landed the task arms this exercises directly at the `Chan` level; W4
    /// is what makes `go`/`put!`/`take!` reach them through the language
    /// (`tests/task_runtime_go_test.rs`).
    pub mod task_chan {
        use std::sync::Arc;

        use crate::value::Value;
        pub use crate::value::{BufferPolicy, Chan, Doorbell};

        pub fn chan(policy: BufferPolicy) -> Arc<Chan> {
            Arc::new(Chan::new(policy))
        }
        pub fn put_int(ch: &Chan, n: i64) -> bool {
            crate::builtins::r#async::chan_put(ch, Value::Int(n))
        }
        pub fn take_int(ch: &Chan) -> Option<i64> {
            match crate::builtins::r#async::chan_take(ch) {
                Some(Value::Int(n)) => Some(n),
                _ => None,
            }
        }
        pub fn close(ch: &Chan) {
            crate::builtins::r#async::chan_close(ch)
        }
        /// `(alts!! [a b])` over two take ops, through the REAL
        /// registration/scan/park path. `Some(n)`: that int arrived.
        /// `None`: a closed chan resolved the select (`[nil ch]`).
        pub fn alts_take2_int(a: &Arc<Chan>, b: &Arc<Chan>) -> Option<i64> {
            match crate::builtins::r#async::alts_take_two(a.clone(), b.clone()) {
                Value::Vector(v) if v.len() == 2 => match &v[0] {
                    Value::Int(n) => Some(*n),
                    _ => None,
                },
                other => panic!("alts!! returned a non-op shape: {other:?}"),
            }
        }
    }

    /// `mova::internal::compile::ir::Ir` -- the compiler's typed IR enum,
    /// for the two white-box test files that inspect what a `defn` actually
    /// compiled to (`compile::ir::NumLoop`/`LoadSlotTake` shape checks) --
    /// see `tests/lastuse_test.rs` and `tests/differential_test.rs`.
    pub mod compile {
        pub use crate::compile::ir;
        /// W1's lane tag-flow fixpoint (`compile::lanes`), for the
        /// kill-probe in `tests/lane_tagflow_probe.rs`.
        pub use crate::compile::lanes;
    }
    pub use crate::builtins::numbers::Num;
}
/// S4: `defmulti`/`defmethod` and the derivation-hierarchy machinery
/// (`derive`/`underive`/`isa?`/`parents`/`ancestors`/`descendants`/
/// `make-hierarchy`) -- see this module's doc for the measured 1.13.0-
/// alpha6 semantics.
pub(crate) mod multi;
pub(crate) mod ns;
/// L1/W2 (docs/L1-LANDING-SPEC.md): the task runtime -- N pinned shard
/// threads multiplexing stackful coroutines, the park/wake handshake, and
/// the per-shard stack pool. Always compiled; `pub` rather than
/// `pub(crate)` because `tests/runtime_test.rs` drives `spawn`/`TaskWaker`
/// directly, and because a `spawn` an embedder can reach is the point of
/// the module.
pub mod runtime;
pub mod interrupt;
pub(crate) mod types;
pub(crate) mod printer;
pub(crate) mod reader;
// W4B-WARNINGS: `*warn-on-reflection*`/`*unchecked-math*` :warn-on-boxed
// analysis, hooked from `eval::Interp::eval_form` -- see that module's own
// doc for why it lives at the crate root next to `hostclass`/`ns` rather
// than under `eval` or `builtins` (it consults `Interp` and `reader::Form`
// both, and calls into `builtins::nsfns::write_shim_err`, so it sits beside
// its peers rather than inside either).
pub(crate) mod reflwarn;
// field5/W-SPAN: process-wide source-buffer interning, so a persisted
// `Span` (`compile::explain::FnTier::TreeWalk`/`LoopExplain`) can be
// rendered against the buffer it was actually read from instead of
// whatever `Interp::source`/`source_name` are current at render time --
// see that module's own doc for the full mechanism.
pub(crate) mod source_registry;
// PERF-PROBE: per-namespace load timing/RSS + read/eval phase totals,
// gated by `MOVA_LOAD_TRACE=1`, off (and zero-cost) by default. See that
// module doc.
pub mod load_trace;
// Clojure-level sampling profiler, gated by `MOVA_PROFILE=<file>`, off
// (and zero-cost) by default. See that module doc.
pub mod profile;
// SPEC-W1 task 2: the embedded stdlib module table -- namespaces
// `require`-able with no `--module-path`, loaded LAZILY through
// `ns::Interp::require_ns` (disk wins on a name clash). Sits at the crate
// root beside `ns` for the same reason `hostclass` does: it is engine
// plumbing that `ns` consults, with no builtin of its own.
pub(crate) mod stdlib;
// SPEC-W6a: `clojure.test.check.random`'s arithmetic -- splitmix64, the
// bit-exact transcription of the vendored `random.clj`. At the crate
// root rather than under `builtins` because `value::Value` carries its
// state type inline (`Value::TcRandom`) and must be able to name it
// without depending on the builtin layer; `builtins::tcrandom` is the
// script-facing half.
pub(crate) mod splitrandom;
// Embedding-API probe (branch embed/probe-serde): serde Serializer/
// Deserializer bridge over `Value`, entirely opt-in behind the `serde`
// feature (default off) so the dependency and its generated code never
// touch a plain `cargo build`.
#[cfg(feature = "serde")]
pub mod serde_bridge;
pub(crate) mod sync;
// V05-PERF-PLAN.md E2: the 1:1 SPSC/handoff transport kernel. A crate-root
// module (not a `builtins` submodule) because it is engine plumbing carrying
// `Value`s, with no builtin registered against it -- same placement rationale
// as `sync`.
//
// `allow(dead_code)`: privatizing this module (Phase D) turned on real
// dead-code analysis for it for the first time -- as a `pub mod` every item
// was exempt (assumed reachable from outside the crate). The module's own
// doc lays out TWO transports by design (`SpscRing`, wired into
// `builtins::flow` today, and `Handoff`, the unbuffered-conn counterpart
// the flow engine doesn't route through yet) plus each side's full
// blocking `put`/`take` API alongside the `try_*` fast path `flow.rs`
// actually calls. The blocking path and all of `Handoff` are exercised
// today only by this module's own `#[cfg(test)]` unit tests and
// `builtins::bench_transport`'s `#[cfg(test)]`-gated E2 comparison bench --
// both invisible to a plain `cargo build`. Not dead in the "delete it"
// sense: it's designed-but-not-yet-wired production code (see the module
// doc's "Deliberate non-goal" section for what IS finished), so the
// allowance lives here rather than deleting or `#[cfg(test)]`-gating
// working code as a side effect of a visibility change.
#[allow(dead_code)]
pub(crate) mod transport;
// Phase E1 (EMBED-API-PLAN.md, merge of embed/probe-tovalue-perf): this was
// `#[doc(hidden)] pub` only because `benches/ab_tovalue_bench.rs` and
// `benches/serde_bridge_bench.rs` (on the perf branch, before it landed
// here) imported `mova::value::{Value, PMap, PVec, Str}` directly. Now
// that both branches share one tree, those benches are repointed at
// `mova::internal::{Value, PMap, PVec, Str}` (see `internal`'s re-exports
// above) and nothing outside the crate needs this path anymore, so it goes
// back to `pub(crate)` like its siblings.
pub(crate) mod value;
