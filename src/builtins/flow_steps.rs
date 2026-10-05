//! Native step tier, stage N1 (see NATIVE-STEP-DESIGN.md). `flow/map->step*`
//! is the native twin of `core/flow.mova`'s formerly-interpreted `map->step`:
//! it validates the `{:describe :init :transition :transform}` map exactly
//! as before, then returns a `Value::Native` 4-arity step-fn shell instead
//! of a `Value::Fn` closure. `core/flow.mova` now reads
//! `(def map->step flow/map->step*)`, so every EXISTING flow program (every
//! one goes through `map->step`) gets one fewer interpreted call per
//! message per proc -- the shell dispatch itself no longer enters the tree-
//! walker -- with zero API change and zero behavior change.
//!
//! That native's `NativeFn.step` is `None`: N1 only removes the wrapper's
//! own interpreted-call overhead; the user-supplied `:transform` (etc) is
//! still whatever it always was (interpreted or, since the S-series compile
//! tier, compiled).
//!
//! ## N2: the first REAL native steps
//!
//! `flow/step-passthrough`, `flow/step-source` and `flow/step-sink-deliver`
//! each carry a `StepFactory`, so `builtins::flow`'s `run_proc` can promote
//! a proc built from one to `run_proc_fast` -- zero interpreter entry per
//! message. Each is built by ONE call to [`native_step`], which is where
//! NATIVE-STEP-DESIGN.md's "shell = fast, one implementation" rule lives:
//! the `Value::Native` 4-arity shell every non-promoted caller sees
//! (`create-flow`'s `describe()` probe, an unpromotable topology, the
//! `MOVA_NO_FASTSTEP=1` kill switch, or plain user code calling the
//! step-fn directly) is implemented BY the same `FastStep` code the fast
//! path runs -- instantiate, transform/transition, snapshot. Fast and
//! generic can't drift because there is only one implementation; the
//! differential tests then only have to guard the ENGINE plumbing around
//! it.
//!
//! A promoted step OWNS its state shape (the `snapshot()` contract is "the
//! exact Value the generic shell would hold", and here the generic shell IS
//! this code): `step-sink-deliver`'s state is exactly `{:count n}` and
//! `step-source`'s is exactly its `::flow/in-ports` map, so calling those
//! shells' arity-2/3 with some unrelated state map returns the step's own
//! canonical state rather than threading the caller's extra keys through.
//! `step-passthrough`, whose state is opaque to it, does thread it through.
//!
//! ## N3: the rest of the closed catalog + user-fn-parameterized steps
//!
//! `flow/step-count`, `flow/step-sum`, `flow/step-take`, `flow/step-drop`
//! round out tier (ii) (zero interp/msg). `flow/step-map`, `flow/step-filter`
//! and `flow/step-scan` are tier (i): they still call an interpreted (or
//! compiled) user fn once per message via `Interp::call` -- the win over
//! `map->step` for these is the eliminated wrapper-closure call and
//! `{state, {out-id [msgs]}}` result-vector parsing, NOT zero interpreter
//! entry (see each one's doc comment for the exact caveat).
//!
//! ## N4: step-comp (fusion)
//!
//! `flow/step-comp` composes N native steps into ONE `Value::Native`, whose
//! `FastStep` threads a message through every member in a Rust-level loop
//! instead of a chain of separate procs each with its own OS thread and
//! inter-proc channel handoff -- see `ComposedStep`/`ComposedFactory` below.

use std::sync::Arc;

use crate::builtins::flow::{kw as flow_kw, reg_flow};
use crate::builtins::r#async::chan_close;
use crate::builtins::conc::deliver_promise;
use crate::builtins::numbers::add_step;
use crate::error::RjError;
use crate::eval::Interp;
use crate::pvec;
use crate::value::{Chan, FastOut, FastStep, Keyword, NativeFn, PMap, PVec, PromiseCell, StepFactory, StepTransition, Str, Value};

fn kw(name: &str) -> Value {
    Value::Keyword(Keyword::from(name))
}

fn empty_map() -> Value {
    Value::Map(PMap::new())
}

/// Mirrors `Interp::bind_map_pattern`'s `{:keys [...]}` lookup exactly,
/// including its `coerce_map_pattern_source` pass (so an alternating
/// key/value SEQ passed where a map is expected still works, same as the
/// interpreted `map->step` did via ordinary destructuring): `Value::Nil`
/// when `key` is absent OR when `v` isn't map-shaped at all -- never an
/// error. That's the exact condition the interpreted version's `(when
/// (nil? describe) ...)` checks fired on, so this native must fire on
/// precisely the same inputs to keep the validation-error behavior
/// byte-identical.
fn get_key(interp: &mut Interp, v: &Value, key: &str) -> Result<Value, RjError> {
    let coerced = interp.coerce_map_pattern_source(v)?;
    Ok(match coerced {
        Value::Map(m) => m.get(&kw(key)).cloned().unwrap_or(Value::Nil),
        _ => Value::Nil,
    })
}

/// `flow/map->step*` (native). `core/flow.mova`'s interpreted `map->step`
/// was a single-arity `defn` over a `{:keys [...]}` param, so a wrong-arity
/// call produced `"map->step: called with N argument(s) but expects 1"`
/// (mova's `apply_closure`'s own arity-error format, `name` = "map->step"
/// since `defn` names the closure) -- reproduced verbatim here rather than
/// going through `builtins::reg`'s generic `ArityHint` wrapper (whose
/// message shape reads differently), so the outer call's own arity error
/// doesn't regress either.
fn native_map_to_step(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!(
            "map->step: called with {} argument{} but expects 1",
            args.len(),
            if args.len() == 1 { "" } else { "s" }
        )));
    }
    let opts = args[0].clone();

    // EXACT error strings from core/flow.mova's `(throw "...")` -- and,
    // crucially, the EXACT observable value a `catch` sees: a plain thrown
    // string `Value`, not an error-info map (see `error_to_info_map` vs.
    // the `ErrorKind::Thrown` branch in `eval_try`), matching
    // `RjError::thrown`'s contract for `(throw v)`.
    let describe = get_key(interp, &opts, "describe")?;
    if matches!(describe, Value::Nil) {
        return Err(RjError::thrown(Value::Str(Str::from("map->step: :describe is required"))));
    }
    let transform = get_key(interp, &opts, "transform")?;
    if matches!(transform, Value::Nil) {
        return Err(RjError::thrown(Value::Str(Str::from("map->step: :transform is required"))));
    }
    let init = get_key(interp, &opts, "init")?;
    let transition = get_key(interp, &opts, "transition")?;

    // The shell itself: dispatches on argc exactly like the old 4-arity
    // interpreted `fn` did, using the map's fns (or the SAME inline
    // defaults `map->step` used to build as `(fn [_] {})` /
    // `(fn [state _] state)` -- no interpreter entry for those default
    // cases at all now, a small additional win over the old version, which
    // always built and called a real closure even for the common
    // "no :init supplied" case).
    let fns = Arc::new(MapStepFns { describe, init, transition, transform });
    let borrowing = fns.clone();
    let step_fn = move |interp: &mut Interp, call_args: &[Value]| -> Result<Value, RjError> {
        let f = &*borrowing;
        match call_args.len() {
            0 => interp.call(&f.describe, &[]),
            1 => {
                if matches!(f.init, Value::Nil) {
                    Ok(Value::Map(PMap::new()))
                } else {
                    interp.call(&f.init, call_args)
                }
            }
            2 => {
                if matches!(f.transition, Value::Nil) {
                    Ok(call_args[0].clone())
                } else {
                    interp.call(&f.transition, call_args)
                }
            }
            3 => interp.call(&f.transform, call_args),
            n => Err(map_step_arity_error(n)),
        }
    };
    // PHASE 3: the shell is the middleman between the proc loop and the
    // user's `:transform`, so a borrowed-only shell would re-pin every
    // argument the proc loop just handed over -- `Interp::call(&transform,
    // call_args)` keeps the shell's own slice alive for the whole of the
    // user call. The consuming entry point forwards the `Vec` instead, which
    // is what lets `msg`/`cid` reach the transform's parameter slots as the
    // only handles the flow engine is not deliberately keeping. It must stay
    // observably identical to `f` above (`value::NativeFn::consuming`'s
    // contract): same dispatch, same defaults, same arity error.
    let consuming = move |interp: &mut Interp, call_args: &mut [Value]| -> Result<Value, RjError> {
        let f = &*fns;
        match call_args.len() {
            0 => interp.call(&f.describe, &[]),
            1 => {
                if matches!(f.init, Value::Nil) {
                    Ok(Value::Map(PMap::new()))
                } else {
                    interp.call_with_buf(&f.init, call_args)
                }
            }
            2 => {
                if matches!(f.transition, Value::Nil) {
                    // The identity transition: the state is not merely
                    // cloned out of the args, it IS the args' first element.
                    Ok(std::mem::replace(&mut call_args[0], Value::Nil))
                } else {
                    interp.call_with_buf(&f.transition, call_args)
                }
            }
            3 => interp.call_with_buf(&f.transform, call_args),
            n => Err(map_step_arity_error(n)),
        }
    };
    let mut native = NativeFn::new("map->step-fn", step_fn);
    native.consuming = Some(Box::new(consuming));
    Ok(Value::Native(Arc::new(native)))
}

/// The four fns a `map->step` shell dispatches over, shared by `Arc` between
/// its borrowing and consuming entry points (both need them; neither may
/// take them by move).
struct MapStepFns {
    describe: Value,
    init: Value,
    transition: Value,
    transform: Value,
}

/// What `apply_closure`'s `arity_error_message` would print for the old
/// anonymous (unnamed) 4-arity `fn`: `name` is `None` there (the `(fn ([]
/// ...) ...)` form has no leading name symbol), so `arity_error_message`
/// falls back to "anonymous-fn" -- reproduced verbatim.
fn map_step_arity_error(n: usize) -> RjError {
    RjError::arity(format!(
        "anonymous-fn: called with {n} argument{} but expects 0 or 1 or 2 or 3",
        if n == 1 { "" } else { "s" }
    ))
}

// ---------------------------------------------------------------------------
// N2: the "shell = fast" native-step builder
// ---------------------------------------------------------------------------

/// Everything [`native_step`] needs to build a step-fn `Value::Native`
/// whose four arities are implemented by `factory`'s own `FastStep`.
struct StepShell {
    /// Diagnostic name (arity errors), e.g. `"flow/step-passthrough"`.
    name: &'static str,
    /// The arity-0 `describe()` result. Captured once here (it's a
    /// constant per step instance) rather than rebuilt per call.
    describe: Value,
    /// The arity-1 `init(arg-map)` result: this step's canonical initial
    /// state. Ignores the arg map (a closed step takes its config from the
    /// constructor's arguments, not from `:args`) EXCEPT that the engine's
    /// `::flow/pid` addition is simply not needed by any of them.
    init_state: Value,
    /// The out-id this step's `FastOut` maps to in the generic
    /// `{out-id [msgs...]}` shape, or `None` for a step with no out port
    /// (whose `FastOut` is then always `None` anyway).
    out_id: Option<Value>,
    factory: Arc<dyn StepFactory>,
}

/// The generic `{out-id [msgs...]}` map a `FastOut` corresponds to -- the
/// shell's side of the fast path's `FastOut` (see NATIVE-STEP-DESIGN.md's
/// "Shell = fast" section). Only ever built on the SLOW path (a
/// non-promoted call into the 4-arity shell); the promoted loop sends the
/// values straight to its one pre-resolved chan and never materializes this
/// map at all.
fn fast_out_to_map(out: FastOut, out_id: Option<&Value>) -> Value {
    let Some(id) = out_id else { return empty_map() };
    let msgs: PVec = match out {
        FastOut::None => return empty_map(),
        FastOut::One(v) => pvec![v],
        FastOut::Many(vs) => vs.into_iter().collect(),
    };
    let mut m = PMap::new();
    m.insert(id.clone(), Value::Vector(msgs));
    Value::Map(m)
}

/// The engine's `::flow/resume|pause|stop` transition keyword as a
/// `StepTransition`. Anything else (including a plain `:pause`) is `None` =
/// no-op, matching the generic engine, which only ever passes these three.
fn parse_transition(v: &Value) -> Option<StepTransition> {
    let Value::Keyword(k) = v else { return None };
    match k.as_ref() {
        "clojure.core.async.flow/resume" => Some(StepTransition::Resume),
        "clojure.core.async.flow/pause" => Some(StepTransition::Pause),
        "clojure.core.async.flow/stop" => Some(StepTransition::Stop),
        _ => None,
    }
}

/// Builds the `Value::Native` step-fn: a full 4-arity shell (so it works
/// with ZERO engine support, exactly like any other step-fn) that carries
/// its `StepFactory` in `NativeFn.step` (so the engine can promote a
/// qualifying proc past the shell entirely).
fn native_step(shell: StepShell) -> Value {
    let StepShell { name, describe, init_state, out_id, factory } = shell;
    let shell_factory = factory.clone();
    let f = move |interp: &mut Interp, args: &[Value]| -> Result<Value, RjError> {
        match args.len() {
            0 => Ok(describe.clone()),
            1 => Ok(init_state.clone()),
            2 => {
                let mut inst = instantiate_for_shell(&shell_factory, name, &args[0])?;
                if let Some(t) = parse_transition(&args[1]) {
                    inst.transition(t);
                }
                Ok(inst.snapshot())
            }
            3 => {
                let mut inst = instantiate_for_shell(&shell_factory, name, &args[0])?;
                let out = inst.transform(interp, &args[2])?;
                Ok(Value::Vector(pvec![inst.snapshot(), fast_out_to_map(out, out_id.as_ref())]))
            }
            n => Err(RjError::arity(format!(
                "{name}: called with {n} argument{} but expects 0 or 1 or 2 or 3",
                if n == 1 { "" } else { "s" }
            ))),
        }
    };
    Value::Native(Arc::new(NativeFn { name: name.into(), f: Box::new(f), step: Some(factory), consuming: None, image_recipe: None }))
}

/// `instantiate` returning `None` means "engine, stay generic" on the
/// promotion path -- but on the SHELL path there is no fallback left, so it
/// becomes a real error. None of N2's three factories can actually return
/// `None` (each rehydrates defensively from whatever state it's handed);
/// this exists so a future factory that CAN reject a state still fails
/// loudly rather than silently misbehaving.
fn instantiate_for_shell(
    factory: &Arc<dyn StepFactory>,
    name: &'static str,
    state: &Value,
) -> Result<Box<dyn FastStep>, RjError> {
    factory.instantiate(state).ok_or_else(|| {
        RjError::other(format!("{name}: can't run against state {}", crate::printer::pr_str(state)))
    })
}

fn describe_map(ins: &[&str], outs: &[&str]) -> Value {
    let port_map = |ids: &[&str]| {
        let mut m = PMap::new();
        for id in ids {
            m.insert(kw(id), empty_map());
        }
        Value::Map(m)
    };
    let mut m = PMap::new();
    m.insert(kw("ins"), port_map(ins));
    m.insert(kw("outs"), port_map(outs));
    Value::Map(m)
}

// ---------------------------------------------------------------------------
// (flow/step-passthrough)
// ---------------------------------------------------------------------------

/// State is whatever it was handed (a passthrough has no state of its own,
/// so it threads the caller's through verbatim -- see the module doc).
struct PassthroughStep {
    state: Value,
}

impl FastStep for PassthroughStep {
    fn transform(&mut self, _interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        Ok(FastOut::One(msg.clone()))
    }
    fn snapshot(&self) -> Value {
        self.state.clone()
    }
}

struct PassthroughFactory;

impl StepFactory for PassthroughFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        Some(Box::new(PassthroughStep { state: init_state.clone() }))
    }
}

fn native_step_passthrough(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if !args.is_empty() {
        return Err(RjError::arity(format!(
            "flow/step-passthrough: expected 0 arguments, got {}",
            args.len()
        )));
    }
    Ok(native_step(StepShell {
        name: "flow/step-passthrough",
        describe: describe_map(&["in"], &["out"]),
        init_state: empty_map(),
        out_id: Some(kw("out")),
        factory: Arc::new(PassthroughFactory),
    }))
}

// ---------------------------------------------------------------------------
// (flow/step-source ch)
// ---------------------------------------------------------------------------

/// An out-only source proc fed by an EXTERNAL channel the caller owns: its
/// `init` state is the `::flow/in-ports {:in ch}` map, which the engine
/// merges into the proc's in-ports at spawn (the same mechanism
/// `bench/flow-gen-sink.mova`'s interpreted generator uses). Each message
/// taken from `ch` is forwarded to `:out` unchanged.
///
/// **The STEP closes `ch`, never the engine.** `stop` closes every
/// ENGINE-OWNED chan (`FlowRuntime::engine_owned_chans`) and deliberately
/// never touches a user-supplied `::flow/in-ports` chan -- so a source
/// whose feeder thread is still putting would leave that thread blocked
/// forever on a full buffer after the flow stopped. `transition(Stop)`
/// closing `ch` here is what releases it (`chan_put` on a closed chan
/// returns `false` immediately), and is the exact counterpart of the
/// interpreted generator's `(when (= t ::flow/stop) (close! (:tick s)))`.
/// It also means: DON'T hand the same chan to two `step-source`s, and don't
/// expect to reuse it after the flow stops.
struct SourceStep {
    ch: Arc<Chan>,
    state: Value,
}

impl FastStep for SourceStep {
    fn transform(&mut self, _interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        Ok(FastOut::One(msg.clone()))
    }
    fn snapshot(&self) -> Value {
        self.state.clone()
    }
    fn transition(&mut self, t: StepTransition) {
        if t == StepTransition::Stop {
            chan_close(&self.ch);
        }
    }
}

struct SourceFactory {
    ch: Arc<Chan>,
    state: Value,
}

impl StepFactory for SourceFactory {
    fn instantiate(&self, _init_state: &Value) -> Option<Box<dyn FastStep>> {
        Some(Box::new(SourceStep { ch: self.ch.clone(), state: self.state.clone() }))
    }
}

fn native_step_source(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/step-source: expected 1 argument, got {}", args.len())));
    }
    let Value::Channel(ch) = &args[0] else {
        return Err(RjError::type_err(format!(
            "flow/step-source: expected a channel, got {}",
            args[0].type_name()
        )));
    };
    let mut in_ports = PMap::new();
    in_ports.insert(kw("in"), Value::Channel(ch.clone()));
    let mut state = PMap::new();
    state.insert(flow_kw("in-ports"), Value::Map(in_ports));
    let state = Value::Map(state);
    Ok(native_step(StepShell {
        name: "flow/step-source",
        // No wired `:ins` at all: the ONLY in-port is the external one
        // `init` supplies above (declaring it here too would make the
        // engine wire a second, never-fed chan under the same id).
        describe: describe_map(&[], &["out"]),
        init_state: state.clone(),
        out_id: Some(kw("out")),
        factory: Arc::new(SourceFactory { ch: ch.clone(), state }),
    }))
}

// ---------------------------------------------------------------------------
// (flow/step-sink-deliver n done)
// ---------------------------------------------------------------------------

/// Counts messages and `deliver`s the count to `done` when it reaches `n`
/// -- the terminal proc of a throughput benchmark or a test pipeline, with
/// no out port at all (`FastOut::None` every message).
struct SinkDeliverStep {
    n: i64,
    done: Arc<PromiseCell>,
    count: i64,
}

impl FastStep for SinkDeliverStep {
    fn transform(&mut self, _interp: &mut Interp, _msg: &Value) -> Result<FastOut, RjError> {
        let c = self.count + 1;
        if c == self.n {
            deliver_promise(&self.done, Value::Int(c));
        }
        self.count = c;
        Ok(FastOut::None)
    }
    fn snapshot(&self) -> Value {
        let mut m = PMap::new();
        m.insert(kw("count"), Value::Int(self.count));
        Value::Map(m)
    }
}

struct SinkDeliverFactory {
    n: i64,
    done: Arc<PromiseCell>,
}

impl StepFactory for SinkDeliverFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        // Rehydrate the counter from the state the shell's own `init`
        // produced (`{:count 0}`), so a snapshot/instantiate round-trip
        // through the 4-arity shell is lossless -- that round-trip is
        // exactly what arity-3 does once per call.
        let count = match init_state {
            Value::Map(m) => match m.get(&kw("count")) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            },
            _ => 0,
        };
        Some(Box::new(SinkDeliverStep { n: self.n, done: self.done.clone(), count }))
    }
}

fn native_step_sink_deliver(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 2 {
        return Err(RjError::arity(format!(
            "flow/step-sink-deliver: expected 2 arguments (n done), got {}",
            args.len()
        )));
    }
    let Value::Int(n) = args[0] else {
        return Err(RjError::type_err(format!(
            "flow/step-sink-deliver: expected an int n, got {}",
            args[0].type_name()
        )));
    };
    let Value::Promise(done) = &args[1] else {
        return Err(RjError::type_err(format!(
            "flow/step-sink-deliver: expected a promise, got {}",
            args[1].type_name()
        )));
    };
    let mut init = PMap::new();
    init.insert(kw("count"), Value::Int(0));
    Ok(native_step(StepShell {
        name: "flow/step-sink-deliver",
        describe: describe_map(&["in"], &[]),
        init_state: Value::Map(init),
        out_id: None,
        factory: Arc::new(SinkDeliverFactory { n, done: done.clone() }),
    }))
}

// ---------------------------------------------------------------------------
// N3 (NATIVE-STEP-DESIGN.md): the rest of the CLOSED (zero interp/msg)
// catalog -- step-count / step-sum / step-take / step-drop -- each built the
// same way as N2's three: one [`native_step`] call wrapping a small
// `FastStep`/`StepFactory` pair whose `snapshot()` IS the state shape
// `instantiate()` rehydrates from, so the shell (arity-3, non-promoted) and
// the promoted fast loop are, again, the exact same code.
// ---------------------------------------------------------------------------

/// `(flow/step-count)`: `{:count n}`, incrementing on every message and
/// passing it through unchanged (the count is observable ONLY via
/// `snapshot()` -- a ping's `::flow/state`, or the shell's own arity-2/3
/// return -- never via the output stream itself).
struct CountStep {
    count: i64,
}

impl FastStep for CountStep {
    fn transform(&mut self, _interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        self.count += 1;
        Ok(FastOut::One(msg.clone()))
    }
    fn snapshot(&self) -> Value {
        let mut m = PMap::new();
        m.insert(kw("count"), Value::Int(self.count));
        Value::Map(m)
    }
}

struct CountFactory;

impl StepFactory for CountFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        let count = match init_state {
            Value::Map(m) => match m.get(&kw("count")) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            },
            _ => 0,
        };
        Some(Box::new(CountStep { count }))
    }
}

fn native_step_count(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if !args.is_empty() {
        return Err(RjError::arity(format!("flow/step-count: expected 0 arguments, got {}", args.len())));
    }
    let mut init = PMap::new();
    init.insert(kw("count"), Value::Int(0));
    Ok(native_step(StepShell {
        name: "flow/step-count",
        describe: describe_map(&["in"], &["out"]),
        init_state: Value::Map(init),
        out_id: Some(kw("out")),
        factory: Arc::new(CountFactory),
    }))
}

// ---------------------------------------------------------------------------
// (flow/step-sum)
// ---------------------------------------------------------------------------

/// `(flow/step-sum)`: `{:sum s}`, folding every message into the running sum
/// via `numbers.rs`'s OWN `add_step` -- the exact fold step `+`'s native and
/// the compile tier's `(+ a b)` intrinsic both use, so overflow promotion
/// (`Int` -> `Float` on overflow) and the blended `Int`/`Float` arithmetic
/// are one implementation, not a second copy that could drift. A non-number
/// message is a type error from `add_step` itself: per the `FastStep`
/// contract, `self.sum` is only written AFTER `add_step` succeeds, so a
/// rejected message leaves the running sum exactly where it was (the
/// error-chan's `:state` reflects the pre-message sum, unchanged).
struct SumStep {
    sum: Value,
}

impl FastStep for SumStep {
    fn transform(&mut self, interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        let next = add_step(interp, &self.sum, msg)?;
        self.sum = next.clone();
        Ok(FastOut::One(next))
    }
    fn snapshot(&self) -> Value {
        let mut m = PMap::new();
        m.insert(kw("sum"), self.sum.clone());
        Value::Map(m)
    }
}

struct SumFactory;

impl StepFactory for SumFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        let sum = match init_state {
            Value::Map(m) => m.get(&kw("sum")).cloned().unwrap_or(Value::Int(0)),
            _ => Value::Int(0),
        };
        Some(Box::new(SumStep { sum }))
    }
}

fn native_step_sum(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if !args.is_empty() {
        return Err(RjError::arity(format!("flow/step-sum: expected 0 arguments, got {}", args.len())));
    }
    let mut init = PMap::new();
    init.insert(kw("sum"), Value::Int(0));
    Ok(native_step(StepShell {
        name: "flow/step-sum",
        describe: describe_map(&["in"], &["out"]),
        init_state: Value::Map(init),
        out_id: Some(kw("out")),
        factory: Arc::new(SumFactory),
    }))
}

// ---------------------------------------------------------------------------
// (flow/step-take n) / (flow/step-drop n)
// ---------------------------------------------------------------------------

/// `(flow/step-take n)`: `{:remaining k}`, passing a message through and
/// decrementing `k` while `k > 0`, then silently dropping (`FastOut::None`)
/// every message after that -- a take never "closes" anything on its own
/// (matching upstream's step-fn contract: no step can unilaterally stop its
/// own proc), it just stops forwarding.
struct TakeStep {
    remaining: i64,
}

impl FastStep for TakeStep {
    fn transform(&mut self, _interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        if self.remaining > 0 {
            self.remaining -= 1;
            Ok(FastOut::One(msg.clone()))
        } else {
            Ok(FastOut::None)
        }
    }
    fn snapshot(&self) -> Value {
        let mut m = PMap::new();
        m.insert(kw("remaining"), Value::Int(self.remaining));
        Value::Map(m)
    }
}

struct TakeFactory {
    n: i64,
}

impl StepFactory for TakeFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        let remaining = match init_state {
            Value::Map(m) => match m.get(&kw("remaining")) {
                Some(Value::Int(k)) => *k,
                _ => self.n,
            },
            _ => self.n,
        };
        Some(Box::new(TakeStep { remaining }))
    }
}

fn native_step_take(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/step-take: expected 1 argument (n), got {}", args.len())));
    }
    let Value::Int(n) = args[0] else {
        return Err(RjError::type_err(format!("flow/step-take: expected an int n, got {}", args[0].type_name())));
    };
    let mut init = PMap::new();
    init.insert(kw("remaining"), Value::Int(n));
    Ok(native_step(StepShell {
        name: "flow/step-take",
        describe: describe_map(&["in"], &["out"]),
        init_state: Value::Map(init),
        out_id: Some(kw("out")),
        factory: Arc::new(TakeFactory { n }),
    }))
}

/// `(flow/step-drop n)`: `{:remaining k}`, the mirror image of
/// `step-take` -- silently drops while `k > 0` (decrementing), then passes
/// every message through unchanged once `k` reaches 0.
struct DropStep {
    remaining: i64,
}

impl FastStep for DropStep {
    fn transform(&mut self, _interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        if self.remaining > 0 {
            self.remaining -= 1;
            Ok(FastOut::None)
        } else {
            Ok(FastOut::One(msg.clone()))
        }
    }
    fn snapshot(&self) -> Value {
        let mut m = PMap::new();
        m.insert(kw("remaining"), Value::Int(self.remaining));
        Value::Map(m)
    }
}

struct DropFactory {
    n: i64,
}

impl StepFactory for DropFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        let remaining = match init_state {
            Value::Map(m) => match m.get(&kw("remaining")) {
                Some(Value::Int(k)) => *k,
                _ => self.n,
            },
            _ => self.n,
        };
        Some(Box::new(DropStep { remaining }))
    }
}

fn native_step_drop(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/step-drop: expected 1 argument (n), got {}", args.len())));
    }
    let Value::Int(n) = args[0] else {
        return Err(RjError::type_err(format!("flow/step-drop: expected an int n, got {}", args[0].type_name())));
    };
    let mut init = PMap::new();
    init.insert(kw("remaining"), Value::Int(n));
    Ok(native_step(StepShell {
        name: "flow/step-drop",
        describe: describe_map(&["in"], &["out"]),
        init_state: Value::Map(init),
        out_id: Some(kw("out")),
        factory: Arc::new(DropFactory { n }),
    }))
}

// ---------------------------------------------------------------------------
// N3 tier (i): user-fn-parameterized steps -- step-map / step-filter /
// step-scan. HONEST CAVEAT (verbatim, per NATIVE-STEP-DESIGN.md): with an
// interpreted f the per-message cost is one interpreter call; the win over
// map->step is the eliminated wrapper call and result parsing. These are
// NOT zero-interp-per-message steps like tier (ii) above -- `f`/`pred`
// still go through `Interp::call` once per message when they're interpreted
// closures (a compiled `f` still pays whatever the compile tier's own call
// dispatch costs, which is cheaper, but that's the compile tier's win, not
// this one's).
// ---------------------------------------------------------------------------

/// `(flow/step-map f)`. With an interpreted f the per-message cost is one
/// interpreter call; the win over map->step is the eliminated wrapper call
/// and result parsing. State is opaque to this step (like
/// `step-passthrough`, it has none of its own), so it threads whatever it's
/// handed through unchanged.
struct MapStep {
    f: Value,
    state: Value,
}

impl FastStep for MapStep {
    fn transform(&mut self, interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        let out = interp.call(&self.f, std::slice::from_ref(msg))?;
        Ok(FastOut::One(out))
    }
    fn snapshot(&self) -> Value {
        self.state.clone()
    }
}

struct MapFactory {
    f: Value,
}

impl StepFactory for MapFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        Some(Box::new(MapStep { f: self.f.clone(), state: init_state.clone() }))
    }
}

fn native_step_map(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/step-map: expected 1 argument (f), got {}", args.len())));
    }
    Ok(native_step(StepShell {
        name: "flow/step-map",
        describe: describe_map(&["in"], &["out"]),
        init_state: empty_map(),
        out_id: Some(kw("out")),
        factory: Arc::new(MapFactory { f: args[0].clone() }),
    }))
}

/// `(flow/step-filter pred)`. With an interpreted pred the per-message cost
/// is one interpreter call; the win over map->step is the eliminated
/// wrapper call and result parsing. Truthy (Clojure truthiness: everything
/// but `nil`/`false`) passes the ORIGINAL message through; falsy drops it
/// silently.
struct FilterStep {
    pred: Value,
    state: Value,
}

impl FastStep for FilterStep {
    fn transform(&mut self, interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        let keep = interp.call(&self.pred, std::slice::from_ref(msg))?;
        Ok(if keep.truthy() { FastOut::One(msg.clone()) } else { FastOut::None })
    }
    fn snapshot(&self) -> Value {
        self.state.clone()
    }
}

struct FilterFactory {
    pred: Value,
}

impl StepFactory for FilterFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        Some(Box::new(FilterStep { pred: self.pred.clone(), state: init_state.clone() }))
    }
}

fn native_step_filter(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/step-filter: expected 1 argument (pred), got {}", args.len())));
    }
    Ok(native_step(StepShell {
        name: "flow/step-filter",
        describe: describe_map(&["in"], &["out"]),
        init_state: empty_map(),
        out_id: Some(kw("out")),
        factory: Arc::new(FilterFactory { pred: args[0].clone() }),
    }))
}

/// `(flow/step-scan f init)`. With an interpreted f the per-message cost is
/// one interpreter call; the win over map->step is the eliminated wrapper
/// call and result parsing. `acc' = f(acc, msg)` is computed BEFORE
/// mutating `self.acc` (error-safety: a throwing `f` leaves the running
/// accumulator exactly where it was, and `self.acc` is what `snapshot()`
/// reports), then committed and forwarded downstream. State is `{:acc
/// acc}`; `instantiate` reads `:acc` back out of it, falling back to the
/// constructor's own `init` value if the state doesn't have that shape
/// (mirroring `step-sink-deliver`'s defensive rehydration).
struct ScanStep {
    f: Value,
    acc: Value,
}

impl FastStep for ScanStep {
    fn transform(&mut self, interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        let next = interp.call(&self.f, &[self.acc.clone(), msg.clone()])?;
        self.acc = next.clone();
        Ok(FastOut::One(next))
    }
    fn snapshot(&self) -> Value {
        let mut m = PMap::new();
        m.insert(kw("acc"), self.acc.clone());
        Value::Map(m)
    }
}

struct ScanFactory {
    f: Value,
    init: Value,
}

impl StepFactory for ScanFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        let acc = match init_state {
            Value::Map(m) => m.get(&kw("acc")).cloned().unwrap_or_else(|| self.init.clone()),
            _ => self.init.clone(),
        };
        Some(Box::new(ScanStep { f: self.f.clone(), acc }))
    }
}

fn native_step_scan(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 2 {
        return Err(RjError::arity(format!("flow/step-scan: expected 2 arguments (f init), got {}", args.len())));
    }
    let f = args[0].clone();
    let init_val = args[1].clone();
    let mut init = PMap::new();
    init.insert(kw("acc"), init_val.clone());
    Ok(native_step(StepShell {
        name: "flow/step-scan",
        describe: describe_map(&["in"], &["out"]),
        init_state: Value::Map(init),
        out_id: Some(kw("out")),
        factory: Arc::new(ScanFactory { f, init: init_val }),
    }))
}

// ---------------------------------------------------------------------------
// N4 (NATIVE-STEP-DESIGN.md): (flow/step-comp s1 s2 ...) -- fusion. Every
// arg must itself be a native flow step (a `Value::Native` carrying a
// `StepFactory`); the composed step's own state is a `Vector` of its
// members' snapshots, in order, so the shell's arity-1 (`init_state`) and
// arity-3 (`instantiate`) agree on that canonical shape exactly like every
// other step here.
// ---------------------------------------------------------------------------

/// True iff `describe`'s `key` (`"ins"` or `"outs"`) names a non-empty port
/// map -- what `step-comp`'s own `describe()` needs to know about its FIRST
/// member (does it declare an `:ins`?) and its LAST member (does it declare
/// an `:outs`?) to decide its own port shape.
fn describe_has_ports(describe: &Value, key: &str) -> bool {
    let Value::Map(m) = describe else { return false };
    matches!(m.get(&kw(key)), Some(Value::Map(ports)) if !ports.is_empty())
}

/// A composed step's live instance: each member owns its own Rust-side
/// state, threaded through in a loop below.
struct ComposedStep {
    members: Vec<Box<dyn FastStep>>,
}

impl FastStep for ComposedStep {
    /// Threads `msg` through every member in declaration order. A member
    /// returning `FastOut::None` short-circuits the whole chain for that
    /// message (nothing downstream of it ever runs); a member returning
    /// `FastOut::Many` feeds EACH of those messages through the remaining
    /// members independently (a small scratch `Vec` per hop -- correctness
    /// over cleverness, per NATIVE-STEP-DESIGN.md). The empty-vec-at-the-end
    /// case collapses to `FastOut::None`; a single survivor to `One`;
    /// anything else to `Many`.
    fn transform(&mut self, interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
        let mut current: Vec<Value> = vec![msg.clone()];
        for member in self.members.iter_mut() {
            if current.is_empty() {
                break;
            }
            let mut next = Vec::with_capacity(current.len());
            for m in &current {
                match member.transform(interp, m)? {
                    FastOut::None => {}
                    FastOut::One(v) => next.push(v),
                    FastOut::Many(vs) => next.extend(vs),
                }
            }
            current = next;
        }
        Ok(match current.len() {
            0 => FastOut::None,
            1 => FastOut::One(current.into_iter().next().expect("len checked above")),
            _ => FastOut::Many(current),
        })
    }
    fn snapshot(&self) -> Value {
        Value::Vector(self.members.iter().map(|m| m.snapshot()).collect())
    }
    fn transition(&mut self, t: StepTransition) {
        for m in &mut self.members {
            m.transition(t);
        }
    }
}

struct ComposedFactory {
    members: Vec<Arc<dyn StepFactory>>,
}

impl StepFactory for ComposedFactory {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>> {
        let Value::Vector(states) = init_state else { return None };
        if states.len() != self.members.len() {
            return None;
        }
        let mut instances = Vec::with_capacity(self.members.len());
        for (factory, state) in self.members.iter().zip(states.iter()) {
            instances.push(factory.instantiate(state)?);
        }
        Some(Box::new(ComposedStep { members: instances }))
    }
}

fn native_step_comp(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.is_empty() {
        return Err(RjError::arity(
            "flow/step-comp: expected at least 1 argument (a native flow step), got 0".to_string(),
        ));
    }
    let mut factories: Vec<Arc<dyn StepFactory>> = Vec::with_capacity(args.len());
    let mut init_states: Vec<Value> = Vec::with_capacity(args.len());
    for a in args {
        let Value::Native(n) = a else {
            return Err(RjError::type_err("step-comp: every argument must be a native flow step".to_string()));
        };
        let Some(factory) = n.step.clone() else {
            return Err(RjError::type_err("step-comp: every argument must be a native flow step".to_string()));
        };
        factories.push(factory);
        // Each member's own arity-1 canonical init state -- the arg map is
        // ignored by every N2/N3 step's shell (see `StepShell`'s doc), so an
        // empty map is exactly as good as any other here.
        init_states.push(interp.call(a, &[empty_map()])?);
    }
    let first_describe = interp.call(&args[0], &[])?;
    let last_describe = interp.call(&args[args.len() - 1], &[])?;
    let has_ins = describe_has_ports(&first_describe, "ins");
    let has_outs = describe_has_ports(&last_describe, "outs");
    let ins: &[&str] = if has_ins { &["in"] } else { &[] };
    let outs: &[&str] = if has_outs { &["out"] } else { &[] };
    let out_id = if has_outs { Some(kw("out")) } else { None };
    Ok(native_step(StepShell {
        name: "flow/step-comp",
        describe: describe_map(ins, outs),
        init_state: Value::Vector(init_states.into_iter().collect()),
        out_id,
        factory: Arc::new(ComposedFactory { members: factories }),
    }))
}

// ---------------------------------------------------------------------------
// (flow/feed-range! ch n) -- bench plumbing, NOT upstream API
// ---------------------------------------------------------------------------

/// Spawns a detached thread that blocking-puts `0..n` onto `ch` and exits
/// (early, if `ch` closes first -- `chan_put` returns `false`). This is the
/// native twin of the interpreted feeder loop `bench/flow-gen-sink.mova`
/// runs inside a `(future ...)`, which alone caps the whole scenario at the
/// interpreter's own dispatch rate: a native-step pipeline can't be
/// measured through an interpreted producer. Documented bench plumbing, not
/// part of the `core.async.flow` surface.
fn native_feed_range(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 2 {
        return Err(RjError::arity(format!(
            "flow/feed-range!: expected 2 arguments (ch n), got {}",
            args.len()
        )));
    }
    let Value::Channel(ch) = &args[0] else {
        return Err(RjError::type_err(format!(
            "flow/feed-range!: expected a channel, got {}",
            args[0].type_name()
        )));
    };
    let Value::Int(n) = args[1] else {
        return Err(RjError::type_err(format!(
            "flow/feed-range!: expected an int n, got {}",
            args[1].type_name()
        )));
    };
    let ch = ch.clone();
    let body = move || {
        for i in 0..n {
            if !crate::builtins::r#async::chan_put(&ch, Value::Int(i)) {
                break;
            }
        }
    };
    // **L5/W3 fence #3 (design §4): in sim, a TASK, not an OS thread.** Same
    // reason as `flow/inject` (see `builtins::flow`'s `native_inject`): an
    // in-flight OS-thread producer is invisible to the sim advance rule,
    // which reads "no runnable task" as quiescence and jumps virtual time
    // past messages this feeder has not delivered yet (P6b F2). As a task
    // its blocking `chan_put`s become parks and its puts join the seeded
    // schedule. Real mode keeps the detached OS thread verbatim -- this is
    // bench plumbing, and the bench is a real-mode instrument.
    if crate::clock::sim_enabled() {
        crate::runtime::spawn(body);
        return Ok(Value::Nil);
    }
    std::thread::Builder::new()
        // Same `flow-<what>` thread-naming convention the engine's own
        // proc/mult/inject threads follow.
        .name("flow-feed-range".to_string())
        .spawn(crate::memstat::drained(body))
        .map_err(|e| RjError::other(format!("flow/feed-range!: couldn't spawn thread: {e}")))?;
    Ok(Value::Nil)
}

pub fn register(i: &mut Interp) {
    reg_flow(i, "map->step*", "flow/map->step*", native_map_to_step);
    reg_flow(i, "step-passthrough", "flow/step-passthrough", native_step_passthrough);
    reg_flow(i, "step-source", "flow/step-source", native_step_source);
    reg_flow(i, "step-sink-deliver", "flow/step-sink-deliver", native_step_sink_deliver);
    reg_flow(i, "step-count", "flow/step-count", native_step_count);
    reg_flow(i, "step-sum", "flow/step-sum", native_step_sum);
    reg_flow(i, "step-take", "flow/step-take", native_step_take);
    reg_flow(i, "step-drop", "flow/step-drop", native_step_drop);
    reg_flow(i, "step-map", "flow/step-map", native_step_map);
    reg_flow(i, "step-filter", "flow/step-filter", native_step_filter);
    reg_flow(i, "step-scan", "flow/step-scan", native_step_scan);
    reg_flow(i, "step-comp", "flow/step-comp", native_step_comp);
    reg_flow(i, "feed-range!", "flow/feed-range!", native_feed_range);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::Interp;

    fn eval_ok(src: &str) -> Value {
        let mut interp = Interp::new();
        interp
            .eval_str("test", src)
            .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", crate::error::render(&e, "test", src)))
    }

    fn pr(v: &Value) -> String {
        crate::printer::pr_str(v)
    }

    fn ps(src: &str) -> String {
        pr(&eval_ok(src))
    }

    fn eval_err(src: &str) -> String {
        let mut interp = Interp::new();
        match interp.eval_str("test", src) {
            Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
            Err(e) => e.message,
        }
    }

    #[test]
    fn native_steps_carry_a_step_factory_while_map_to_step_does_not() {
        // The engine's ONE promotion question (`NativeFn.step.is_some()`),
        // asked here directly: N2's three steps answer yes, N1's
        // `map->step*` shell answers no.
        for src in [
            "(flow/step-passthrough)",
            "(flow/step-source (chan 1))",
            "(flow/step-sink-deliver 1 (promise))",
        ] {
            let Value::Native(n) = eval_ok(src) else { panic!("{src}: expected a native") };
            assert!(n.step.is_some(), "{src} must carry a StepFactory");
        }
        let Value::Native(n) =
            eval_ok("(flow/map->step* {:describe (fn [] {}) :transform (fn [s _ m] [s {}])})")
        else {
            panic!("expected a native")
        };
        assert!(n.step.is_none(), "map->step* is N1: no factory, never promoted");
    }

    #[test]
    fn fast_out_maps_to_the_generic_outs_map_shape() {
        let out = kw("out");
        assert_eq!(pr(&fast_out_to_map(FastOut::None, Some(&out))), "{}");
        assert_eq!(pr(&fast_out_to_map(FastOut::One(Value::Int(1)), Some(&out))), "{:out [1]}");
        assert_eq!(
            pr(&fast_out_to_map(FastOut::Many(vec![Value::Int(1), Value::Int(2)]), Some(&out))),
            "{:out [1 2]}"
        );
        // No out port: every FastOut collapses to the empty outs map.
        assert_eq!(pr(&fast_out_to_map(FastOut::One(Value::Int(1)), None)), "{}");
    }

    #[test]
    fn transition_keywords_parse_only_in_the_flow_namespace() {
        assert_eq!(parse_transition(&flow_kw("resume")), Some(StepTransition::Resume));
        assert_eq!(parse_transition(&flow_kw("pause")), Some(StepTransition::Pause));
        assert_eq!(parse_transition(&flow_kw("stop")), Some(StepTransition::Stop));
        assert_eq!(parse_transition(&kw("stop")), None);
        assert_eq!(parse_transition(&Value::Nil), None);
    }

    #[test]
    fn sink_deliver_instantiate_snapshot_round_trips_its_counter() {
        // What arity-3 does once per call (rehydrate -> transform ->
        // snapshot): the counter must survive the round trip, or the
        // non-promoted shell would silently count nothing.
        let factory = SinkDeliverFactory {
            n: 3,
            done: Arc::new(PromiseCell::pending()),
        };
        let mut state = Value::Map(PMap::new());
        let mut interp = Interp::new();
        for expected in 1..=3 {
            let mut inst = factory.instantiate(&state).expect("sink-deliver always instantiates");
            assert!(matches!(inst.transform(&mut interp, &Value::Int(0)), Ok(FastOut::None)));
            state = inst.snapshot();
            assert_eq!(pr(&state), format!("{{:count {expected}}}"));
        }
        // ...and the promise was delivered exactly at n.
        assert!(matches!(
            &*crate::sync::lock_mutex(&factory.done.state),
            crate::value::PromiseState::Delivered(Value::Int(3))
        ));
    }

    #[test]
    fn step_source_stop_transition_closes_its_chan_through_the_shell() {
        let result = eval_ok(
            r#"(let [c (chan 1)
                     src (flow/step-source c)
                     before (pr-str c)
                     _ (src {} :clojure.core.async.flow/pause)
                     during (pr-str c)
                     _ (src {} :clojure.core.async.flow/stop)]
                 [before during (pr-str c)])"#,
        );
        assert_eq!(pr(&result), r##"["#<chan open>" "#<chan open>" "#<chan closed>"]"##);
    }

    #[test]
    fn map_to_step_star_is_registered_and_builds_a_native() {
        let v = eval_ok(
            r#"(flow/map->step* {:describe (fn [] {:ins {} :outs {}}) :transform (fn [s _ m] [s {}])})"#,
        );
        assert!(matches!(v, Value::Native(_)), "expected a native, got {v:?}");
    }

    // -----------------------------------------------------------------
    // N3 catalog: shell-level semantics for the arithmetic/counting steps.
    // -----------------------------------------------------------------

    #[test]
    fn step_count_increments_and_passes_msg_through_unchanged() {
        let result = ps(r#"(let [sf (flow/step-count)] [(sf {:count 0} :in :a) (sf {:count 1} :in :b)])"#);
        assert_eq!(result, "[[{:count 1} {:out [:a]}] [{:count 2} {:out [:b]}]]");
    }

    #[test]
    fn step_sum_folds_via_the_numbers_rs_add_step_and_rejects_non_numbers() {
        let result = ps(r#"(let [sf (flow/step-sum)] [(sf {:sum 0} :in 3) (sf {:sum 3} :in 4)])"#);
        assert_eq!(result, "[[{:sum 3} {:out [3]}] [{:sum 7} {:out [7]}]]");
        // A non-number message errors out of `add_step` itself; the shell
        // never gets to build a `[state' outs]` vector at all, so the
        // caller's OWN state (never updated) is what's still observable.
        let msg = eval_err(r#"((flow/step-sum) {:sum 3} :in :not-a-number)"#);
        assert!(msg.contains("+: expected a number"), "message was: {msg}");
    }

    #[test]
    fn step_take_passes_n_then_silently_drops() {
        let result = ps(
            r#"(let [sf (flow/step-take 2)]
                 [(sf {:remaining 2} :in :a) (sf {:remaining 1} :in :b) (sf {:remaining 0} :in :c)])"#,
        );
        assert_eq!(result, "[[{:remaining 1} {:out [:a]}] [{:remaining 0} {:out [:b]}] [{:remaining 0} {}]]");
    }

    #[test]
    fn step_drop_silently_drops_n_then_passes_through() {
        let result = ps(
            r#"(let [sf (flow/step-drop 2)]
                 [(sf {:remaining 2} :in :a) (sf {:remaining 1} :in :b) (sf {:remaining 0} :in :c)])"#,
        );
        assert_eq!(result, "[[{:remaining 1} {}] [{:remaining 0} {}] [{:remaining 0} {:out [:c]}]]");
    }

    #[test]
    fn step_scan_computes_acc_before_committing_and_errors_keep_the_running_acc() {
        let result = ps(
            r#"(let [sf (flow/step-scan + 0)] [(sf {:acc 0} :in 1) (sf {:acc 1} :in 2) (sf {:acc 3} :in 3)])"#,
        );
        assert_eq!(result, "[[{:acc 1} {:out [1]}] [{:acc 3} {:out [3]}] [{:acc 6} {:out [6]}]]");
        let msg = eval_err(r#"((flow/step-scan + 0) {:acc 5} :in :not-a-number)"#);
        assert!(msg.contains("+: expected a number"), "message was: {msg}");
    }

    #[test]
    fn step_map_and_step_filter_call_the_user_fn_once_per_message() {
        let result = ps(
            r#"[((flow/step-map inc) {} :in 1)
                ((flow/step-filter even?) {} :in 2)
                ((flow/step-filter even?) {} :in 3)]"#,
        );
        assert_eq!(result, "[[{} {:out [2]}] [{} {:out [2]}] [{} {}]]");
    }

    #[test]
    fn step_comp_rejects_a_non_native_argument_and_a_step_none_native() {
        assert_eq!(eval_err("(flow/step-comp 5)"), "step-comp: every argument must be a native flow step");
        assert_eq!(
            eval_err(
                r#"(flow/step-comp (flow/map->step {:describe (fn [] {:ins {} :outs {}}) :transform (fn [s _ m] [s {}])}))"#
            ),
            "step-comp: every argument must be a native flow step"
        );
        assert_eq!(eval_err("(flow/step-comp)"), "flow/step-comp: expected at least 1 argument (a native flow step), got 0");
    }

    #[test]
    fn step_comp_shell_threads_a_message_through_members_in_order() {
        // count -> take 2: count tracks every message; take passes the
        // first 2 through then silently drops. Verifies (a) the composed
        // init state is a Vector of member init states, (b) transform
        // threads state/out correctly hop by hop, and (c) a member
        // returning `None` (take, once exhausted) short-circuits so the
        // comp's own `FastOut` is `None` too.
        let result = ps(
            r#"(let [c (flow/step-comp (flow/step-count) (flow/step-take 2))
                     s0 (c {})
                     r1 (c s0 :in :a)
                     r2 (c (first r1) :in :b)
                     r3 (c (first r2) :in :c)]
                 [(c) s0 r1 r2 r3])"#,
        );
        assert_eq!(
            result,
            concat!(
                "[{:ins {:in {}}, :outs {:out {}}} ",
                "[{:count 0} {:remaining 2}] ",
                "[[{:count 1} {:remaining 1}] {:out [:a]}] ",
                "[[{:count 2} {:remaining 0}] {:out [:b]}] ",
                "[[{:count 3} {:remaining 0}] {}]]"
            )
        );
    }

    #[test]
    fn step_comp_describe_is_source_shaped_when_the_first_member_declares_no_ins() {
        let describe = eval_ok(r#"((flow/step-comp (flow/step-source (chan 1)) (flow/step-passthrough)))"#);
        assert_eq!(pr(&describe), "{:ins {}, :outs {:out {}}}");
    }

    #[test]
    fn step_comp_describe_has_no_outs_when_the_last_member_is_a_sink() {
        let describe =
            eval_ok(r#"((flow/step-comp (flow/step-passthrough) (flow/step-sink-deliver 1 (promise))))"#);
        assert_eq!(pr(&describe), "{:ins {:in {}}, :outs {}}");
    }

    /// White-box test of `ComposedStep::transform`'s `FastOut::Many`
    /// fan-out threading (NATIVE-STEP-DESIGN.md: "Many from a member feeds
    /// each msg through the remaining members"), using a test-only step
    /// that doubles every message into two -- no step in the public
    /// catalog emits `Many` today, so this is the only way to exercise
    /// that branch at all.
    struct DoublerStep;
    impl FastStep for DoublerStep {
        fn transform(&mut self, _interp: &mut Interp, msg: &Value) -> Result<FastOut, RjError> {
            Ok(FastOut::Many(vec![msg.clone(), msg.clone()]))
        }
        fn snapshot(&self) -> Value {
            Value::Nil
        }
    }
    struct DoublerFactory;
    impl StepFactory for DoublerFactory {
        fn instantiate(&self, _init_state: &Value) -> Option<Box<dyn FastStep>> {
            Some(Box::new(DoublerStep))
        }
    }

    #[test]
    fn composed_step_fans_a_many_output_through_every_remaining_member() {
        let mut comp = ComposedStep {
            members: vec![
                DoublerFactory.instantiate(&Value::Nil).unwrap(),
                PassthroughFactory.instantiate(&empty_map()).unwrap(),
            ],
        };
        let mut interp = Interp::new();
        let out = comp.transform(&mut interp, &Value::Int(7)).expect("doubler->passthrough never errors");
        match out {
            FastOut::Many(vs) => assert_eq!(vs, vec![Value::Int(7), Value::Int(7)]),
            _ => panic!("expected Many, got a differently-shaped FastOut"),
        }
    }

    #[test]
    fn composed_step_none_from_any_member_short_circuits_the_remaining_chain() {
        // take(0) drops everything immediately; a passthrough placed AFTER
        // it in the chain must never even be reached.
        let mut comp = ComposedStep {
            members: vec![
                TakeFactory { n: 0 }.instantiate(&Value::Nil).unwrap(),
                PassthroughFactory.instantiate(&empty_map()).unwrap(),
            ],
        };
        let mut interp = Interp::new();
        assert!(matches!(comp.transform(&mut interp, &Value::Int(1)), Ok(FastOut::None)));
    }
}
