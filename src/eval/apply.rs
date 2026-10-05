//! Function application: closures (with the `recur` self-tail trampoline),
//! natives, and callable collections (keyword/map/set/vector as fn).

use std::sync::{Arc, OnceLock};

use super::{Frame, Interp};
use crate::builtins::map_probe;
use crate::builtins::numbers::{coerce_prim_params, coerce_prim_params_in_place};
use crate::error::{ErrorKind, RjError};
use crate::reader::{Form, Span};
use crate::value::{Arity, Closure, Str, Symbol, Value};

impl Interp {
    /// Applies any callable `Value` (closure, native, or a keyword/map/
    /// set/vector used as a fn) to already-evaluated `args`.
    pub(crate) fn apply_value(&mut self, f: &Value, args: &[Value], span: Span) -> Result<Value, RjError> {
        match f {
            // Invocable vars (R2): `(#'f 1)`/`(map #'f xs)`/natives that
            // `interp.call` a stored fn value (`flow/process`'s step-fn) all
            // go through this one recursion point, so a `Value::Var` is
            // indistinguishable from calling its CURRENT value directly --
            // except late-bound through the cell on every call.
            Value::Var(cell) => {
                let current = cell.get().ok_or_else(|| {
                    self.other_here(format!("var {} is unbound", crate::printer::pr_str(&Value::Sym(cell.name.clone()))), span)
                })?;
                self.apply_value(&current, args, span)
            }
            Value::Fn(rc) => self.apply_closure(rc, args, span),
            Value::Native(native) => {
                #[cfg(feature = "k2-count")]
                crate::k2count::NATIVE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::profile::push(&native.name);
                let out = (native.f)(self, args).map_err(|e| self.decorate_native_err(e, span));
                crate::profile::pop();
                out
            }
            Value::Keyword(k) => {
                if args.is_empty() || args.len() > 2 {
                    return Err(self
                        .arity_here(keyword_arity_error_message(&format!(":{k}"), args.len()), span)
                        .with_arity_actual(args.len() as i64));
                }
                let key = Value::Keyword(k.clone());
                // SPEC-W1 task 6: `(:kw obj)`/`(:kw obj nf)` on an
                // instance that declared `clojure.lang.ILookup` routes to
                // its `valAt`, exactly as `Keyword.invoke` -> `RT.get`
                // does on the JVM. One enum-tag test guards it, so the hot
                // map/host-struct path is untouched; everything else still
                // goes through `named_lookup` unchanged.
                //
                // SPEC-W3: the shape test peels metadata, because
                // `(with-meta reified m)` is the same class on the JVM
                // and `clojure.spec.alpha` wraps every registered spec
                // that way -- see `builtins::types::ilookup_val_at`.
                if matches!(args[0].unmeta(), Value::Inst(_)) {
                    if let Some(r) = crate::builtins::types::ilookup_val_at(
                        self,
                        &args[0],
                        &key,
                        args.get(1).cloned(),
                    ) {
                        return r.map_err(|e| self.decorate_native_err(e, span));
                    }
                }
                Ok(named_lookup(&key, &args[0], args.get(1).cloned()))
            }
            // §5/M2 (1.13 destructuring exposed this: `syms-bang`'s own
            // check code calls a quoted symbol as a fn, `('b sample-map)`,
            // the symbol-keyed sibling of `(:k m)`/keyword-as-fn just
            // above -- NOT part of the 1.13 destructuring spec itself, but
            // a real, general, oracle-confirmed Clojure semantic
            // (`clojure.lang.Symbol` implements `IFn` exactly like
            // `Keyword`: 1 or 2 args, `(get coll sym not-found)`, 0 args is
            // an arity error). Was previously entirely missing ("symbol is
            // not callable").
            Value::Sym(s) => {
                if args.is_empty() || args.len() > 2 {
                    return Err(self.arity_here(
                        format!(
                            "{}: called with {} argument(s), expects 1 or 2",
                            crate::printer::pr_str(&Value::Sym(s.clone())),
                            args.len()
                        ),
                        span,
                    ));
                }
                let key = Value::Sym(s.clone());
                // SPEC-W1 task 6: symbol-as-fn is `(get coll sym
                // not-found)` (see this arm's own doc), so it takes the
                // same ILookup route the keyword arm above does --
                // including SPEC-W3's metadata peeling.
                if matches!(args[0].unmeta(), Value::Inst(_)) {
                    if let Some(r) = crate::builtins::types::ilookup_val_at(
                        self,
                        &args[0],
                        &key,
                        args.get(1).cloned(),
                    ) {
                        return r.map_err(|e| self.decorate_native_err(e, span));
                    }
                }
                Ok(named_lookup(&key, &args[0], args.get(1).cloned()))
            }
            Value::Map(m) => {
                if args.is_empty() || args.len() > 2 {
                    return Err(self.arity_here(
                        format!("map: called with {} argument(s), expects 1 or 2", args.len()),
                        span,
                    ));
                }
                map_probe::record("map-as-fn", m.len());
                Ok(m.get(&args[0])
                    .cloned()
                    .unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::Nil)))
            }
            // C10: measured, `((sorted-map :a 1) :a)` => `1`, `((sorted-map
            // :a 1) :c 99)` => `99` -- same 1-or-2-arity invoke shape as
            // a plain map, just via the sorted lookup (honors a `-by`
            // comparator).
            Value::SortedMap(m) => {
                if args.is_empty() || args.len() > 2 {
                    return Err(self.arity_here(
                        format!("sorted-map: called with {} argument(s), expects 1 or 2", args.len()),
                        span,
                    ));
                }
                let m = m.clone();
                Ok(crate::builtins::sorted::sorted_map_get(self, &m, &args[0])?
                    .unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::Nil)))
            }
            // C2 (defstruct), measured: `(s :a)` works exactly like a plain
            // map invoked as a fn.
            Value::StructMap(sm) => {
                if args.is_empty() || args.len() > 2 {
                    return Err(self.arity_here(
                        format!("struct-map: called with {} argument(s), expects 1 or 2", args.len()),
                        span,
                    ));
                }
                Ok(crate::builtins::structmap::struct_map_get(sm, &args[0])
                    .cloned()
                    .unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::Nil)))
            }
            // C10: a set (transient sets are the same value -- `transient`
            // is identity, see core.mova) supports 2-arg invoke with a
            // not-found default, same as map/keyword. Measured:
            // `(#{:a} :b 99)` => `99`, `((transient #{:a}) :a 1)` => `:a`.
            Value::Set(s) => {
                if args.is_empty() || args.len() > 2 {
                    return Err(self.arity_here(
                        format!("set: called with {} argument(s), expects 1 or 2", args.len()),
                        span,
                    ));
                }
                Ok(if s.contains(&args[0]) {
                    args[0].clone()
                } else {
                    args.get(1).cloned().unwrap_or(Value::Nil)
                })
            }
            // C10: `sorted-set` as fn -- measured identical to a plain
            // set's invoke semantics (`(sorted-set :a) :b 99)` => `99`).
            Value::SortedSet(s) => {
                if args.is_empty() || args.len() > 2 {
                    return Err(self.arity_here(
                        format!("set: called with {} argument(s), expects 1 or 2", args.len()),
                        span,
                    ));
                }
                let s = s.clone();
                Ok(crate::builtins::sorted::sorted_set_get(self, &s, &args[0])
                    .unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::Nil)))
            }
            // S7: an entry is invocable by index exactly like the
            // vector it is -- measured, `((first {:a 1}) 0)` => `:a`,
            // `((first {:a 1}) 1)` => `1`.
            Value::Vector(v) | Value::MapEntry(v) => {
                if args.len() != 1 {
                    return Err(self.arity_here(
                        format!("vector: called with {} argument(s), expects 1", args.len()),
                        span,
                    ));
                }
                let idx = match &args[0] {
                    Value::Int(n) => *n,
                    other => {
                        return Err(self
                            .type_err_here(format!("vector index must be an int, got {}", other.type_name()), span))
                    }
                };
                if idx < 0 || idx as usize >= v.len() {
                    return Err(self.other_here(
                        format!("index {idx} out of bounds for vector of length {}", v.len()),
                        span,
                    ));
                }
                Ok(v.get_owned(idx as usize).expect("bounds checked"))
            }
            // S5/M3: a metadata wrapper is transparent to invocation --
            // `((with-meta (fn [] 1) {:a 1}))` calls the fn, and a
            // metadata-carrying vector/map/keyword is still usable as a
            // fn. Last arm (not an unwrap at the top) so the ~100%
            // common non-`Meta` callee pays nothing.
            Value::Meta(m) => self.apply_value(&m.inner, args, span),
            other => Err(self.type_err_here(format!("{} is not callable", other.type_name()), span)),
        }
    }

    /// The stack-frame name for a closure with no name. Cached: W4's
    /// attribution census (bench/RESULTS-w4-alloc-attrib.md) measured the
    /// old per-call `Str::from("anonymous-fn")` at ~3.1 allocs per flow
    /// message (both of flow-gen-sink's transforms are anonymous fns) --
    /// a constant rebuilt on every single anonymous call. A clone of a
    /// cached `Str` is one refcount bump.
    #[inline]
    pub(crate) fn anon_frame_name() -> Str {
        static ANON: OnceLock<Str> = OnceLock::new();
        ANON.get_or_init(|| Str::from("anonymous-fn")).clone()
    }

    /// Profiler-only name (see `crate::profile`): `ns/name` for a defined
    /// fn, `ns/anon@source:line:col` for a literal -- unlike
    /// `anon_frame_name` above (used for error-trace `Frame`s, where every
    /// anonymous closure collapsing into one literal `"anonymous-fn"` is
    /// long-standing, unrelated behavior this does not touch), a sampling
    /// profiler needs anonymous closures told apart or its single biggest
    /// bucket is uninformative. Only called when `profile::enabled()`.
    fn profile_frame_name(&self, rc: &Arc<Closure>) -> String {
        match &rc.name {
            Some(n) => format!("{}/{}", rc.ns, n),
            None => format!(
                "{}/anon@{}",
                rc.ns,
                crate::source_registry::render_at(self, rc.def_source_id.get(), rc.def_span)
            ),
        }
    }

    /// A native's error picks up its call-site span and the current stack
    /// here, so both the borrowing and the consuming entry point produce
    /// byte-identical errors.
    #[inline]
    pub(crate) fn decorate_native_err(&self, mut e: RjError, span: Span) -> RjError {
        if e.span.is_none() {
            e.span = Some(span);
        }
        if e.stack.is_empty() {
            e.stack = self.stack_snapshot();
        }
        e
    }

    /// THE CONSUMING SEAM (Perceus-lite phase 1 -- read `builtins::reuse`
    /// for the invariant this rests on).
    ///
    /// Identical to [`Self::apply_value`] in every observable way, but takes
    /// the args `Vec` **by value**. Both tiers build exactly such a `Vec`
    /// immediately before applying and drop it immediately after
    /// (`eval::eval_list`'s `arg_values`, `compile::exec`'s `argv`), so
    /// routing them through here hands whitelisted natives an args buffer
    /// they may cannibalise -- which is what lets `assoc`/`conj`/`dissoc`
    /// mutate a temporary receiver in place instead of cloning it out of a
    /// borrowed slice.
    ///
    /// CALLER CONTRACT: `args` must be a `Vec` the caller is discarding.
    /// (Passing by value rather than `&mut [Value]` makes that contract
    /// unbreakable rather than merely documented -- a caller that still
    /// needs its arguments cannot call this function at all.)
    ///
    /// Cost when the callee is not whitelisted (i.e. essentially every
    /// call): one already-hot field's `Option` discriminant check.
    ///
    /// W4 allocation diet: because every caller is BY CONTRACT discarding
    /// `args`, this function is the one death site for all of those
    /// per-call `Vec`s -- so each terminal arm below returns the emptied
    /// buffer to [`Interp::buf_pool`] (`put_buf` clears it; the values were
    /// either moved out by the callee or are dropped right here, exactly as
    /// the old implicit `drop(args)` did) instead of freeing it, and
    /// `compile::exec::exec_args` / `eval_list` allocate from the same pool.
    /// The `Var` arm recurses with ownership, so the inner call pools it.
    pub(crate) fn apply_value_owned(&mut self, f: &Value, mut args: Vec<Value>, span: Span) -> Result<Value, RjError> {
        match f {
            // PHASE 3: a closure gets the args MOVED into its parameter
            // slots/bindings instead of cloned out of the buffer, which stops
            // the caller from holding a second handle on every argument for
            // the whole of the call. See `Self::apply_closure_buf`.
            Value::Fn(rc) => {
                if self.moveargs_enabled {
                    let out = self.apply_closure_buf(rc, &mut args, span);
                    self.put_buf(args);
                    return out;
                }
            }
            Value::Native(native) => {
                #[cfg(feature = "k2-count")]
                crate::k2count::NATIVE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::profile::push(&native.name);
                if let Some(consuming) = &native.consuming {
                    // Kill switch checked here, not at registration: only
                    // the handful of whitelisted natives ever reach this
                    // line, so `MOVA_NO_REUSE` stays dynamically
                    // observable (the differential-guard tests depend on
                    // it) without putting a check on every call. The flag is
                    // per-`Interp` and was ANDed with `reuse::enabled()` at
                    // construction, so this is one already-hot field read
                    // rather than the `OnceLock`'s atomic load.
                    if self.reuse_enabled {
                        let out = consuming(self, &mut args).map_err(|e| self.decorate_native_err(e, span));
                        self.put_buf(args);
                        crate::profile::pop();
                        return out;
                    }
                }
                // Straight to the borrowing entry point rather than back
                // through `apply_value`: an ordinary native reached from the
                // compiled tier (`finish_call`) or from a Rust caller would
                // otherwise pay a SECOND dispatch over this wide enum for
                // nothing. Byte-identical to `apply_value`'s own `Native`
                // arm, error decoration included.
                let out = (native.f)(self, &args).map_err(|e| self.decorate_native_err(e, span));
                self.put_buf(args);
                crate::profile::pop();
                return out;
            }
            // Invocable vars forward ownership too, so `(#'assoc m :k 1)`
            // and `(assoc m :k 1)` take the same path.
            Value::Var(cell) => {
                if let Some(current) = cell.get() {
                    return self.apply_value_owned(&current, args, span);
                }
            }
            _ => {}
        }
        let out = self.apply_value(f, &args, span);
        self.put_buf(args);
        out
    }

    /// [`Self::apply_value_owned`] for a caller whose argument buffer is a
    /// STACK ARRAY it refills per iteration rather than a `Vec` it is about
    /// to drop -- `reduce`'s `[acc, item]`, `reduce-kv`'s `[acc, k, v]`.
    ///
    /// Same handover, same invariant; the only difference is who owns the
    /// backing store. A `Vec` here would cost a malloc/free pair per element
    /// (measured: ~7% on `(reduce + 0 (range 3000000))`, a shape the handover
    /// cannot help at all because `+` is a native with no consuming entry
    /// point), and reusing one `Vec` across iterations still cost ~3% in
    /// `clear`/`push` bookkeeping against a fixed two-slot array.
    ///
    /// CALLER CONTRACT: `args`' elements are the callee's to take. After the
    /// call the caller may overwrite them but must not read them -- what they
    /// hold is unspecified (`Value::Nil` when the callee took them, the
    /// original values when it took a borrowing path).
    pub(crate) fn apply_value_slice(&mut self, f: &Value, args: &mut [Value], span: Span) -> Result<Value, RjError> {
        match f {
            Value::Fn(rc) => {
                if self.moveargs_enabled {
                    return self.apply_closure_buf(rc, args, span);
                }
            }
            Value::Native(native) => {
                #[cfg(feature = "k2-count")]
                crate::k2count::NATIVE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::profile::push(&native.name);
                if let Some(consuming) = &native.consuming {
                    if self.reuse_enabled {
                        let out = consuming(self, args).map_err(|e| self.decorate_native_err(e, span));
                        crate::profile::pop();
                        return out;
                    }
                }
                // One dispatch, not two -- see `apply_value_owned`.
                let out = (native.f)(self, args).map_err(|e| self.decorate_native_err(e, span));
                crate::profile::pop();
                return out;
            }
            Value::Var(cell) => {
                if let Some(current) = cell.get() {
                    return self.apply_value_slice(&current, args, span);
                }
            }
            _ => {}
        }
        self.apply_value(f, args, span)
    }

    /// Calls any callable `Value` (closure, native, or a keyword/map/set/
    /// vector used as a fn) with already-evaluated `args`. Used by builtins
    /// (`swap!`, `map`/`filter`/`reduce`, `sort-by`'s comparator, `apply`,
    /// etc.) that need to invoke a user-supplied function programmatically,
    /// where there's no real call-site span to attach (errors raised
    /// *inside* the callee still carry their own accurate spans).
    /// Public: embedders (host crates registering their own builtins and
    /// native callbacks) need exactly this entry point.
    pub fn call(&mut self, f: &Value, args: &[Value]) -> Result<Value, RjError> {
        self.apply_value(f, args, Span { start: 0, end: 0 })
    }

    /// THE RUST-CALLER HANDOVER (Perceus-lite phase 3).
    ///
    /// [`Self::call`] with the args `Vec` taken **by value**, for the native
    /// callers that build a fresh argument list per iteration and drop it
    /// immediately after the call -- `reduce`'s accumulator pair, `swap!`'s
    /// `[current, ..extra]`, `flow`'s `[state, cid, msg]`. Those callers were
    /// the *pin* phase 2 measured and could not remove: `interp.call(&f,
    /// &[acc, h])` keeps `acc` alive in a caller-owned temporary for the
    /// whole of the call, so no amount of last-use analysis inside the callee
    /// can make the handle unique.
    ///
    /// CALLER CONTRACT, identical to [`Self::apply_value_owned`]'s: `args`
    /// must be a `Vec` the caller is discarding. A caller that still needs an
    /// argument after the call must keep its own clone -- and *that clone is
    /// then a handle*, which is the whole reason `flow`'s state and `swap!`'s
    /// current value cannot become unique (see `builtins::flow`'s
    /// `process_message_to`).
    pub fn call_owned(&mut self, f: &Value, args: Vec<Value>) -> Result<Value, RjError> {
        self.apply_value_owned(f, args, Span { start: 0, end: 0 })
    }

    /// [`Self::call_owned`] for a caller that will call again in a moment
    /// from a REUSED buffer -- typically a stack array refilled per
    /// iteration, which is what `reduce`/`reduce-kv` use. The arguments are
    /// handed over; the backing store is not.
    ///
    /// CALLER CONTRACT: after the call the caller may overwrite `args` but
    /// must not read it -- the callee has taken its contents.
    pub fn call_with_buf(&mut self, f: &Value, args: &mut [Value]) -> Result<Value, RjError> {
        self.apply_value_slice(f, args, Span { start: 0, end: 0 })
    }

    /// Applies a macro's closure to *unevaluated* call-site arg forms
    /// (converted to `Value` via `form_to_value`, i.e. quoted data). This
    /// reuses all of `apply_closure`'s arity/variadic/recur/stack-frame
    /// machinery — a macro is just a fn whose "arguments" happen to be
    /// unevaluated forms rather than evaluated results.
    ///
    /// `raw_call_form` is the WHOLE unevaluated call -- `(head arg-forms...)`
    /// as data, head included -- real Clojure's implicit `&form` macro
    /// param (see `Interp::macro_form_stack`'s doc). Pushed for the
    /// duration of this call only (popped on every exit path, `Ok` or
    /// `Err`, via the `?`-unfriendly explicit match below) so a nested
    /// macro-expanding-a-macro sees its OWN call form and the outer one's
    /// is restored the instant this call returns.
    pub(crate) fn apply_macro(
        &mut self,
        closure: &Arc<Closure>,
        arg_forms: &[Form],
        raw_call_form: Value,
        call_span: Span,
    ) -> Result<Value, RjError> {
        // field4/W-LENS-1: one re-expansion. The compiled tier freezes macro
        // expansion, so every count here is a form the tree-walker will
        // expand AGAIN next time it evaluates it -- the trigger metric for
        // the macro-expansion-cache wave.
        crate::lens::event_at(crate::lens::Event::MacroExpand, closure.lens_site());
        let arg_values: Vec<Value> = arg_forms.iter().map(crate::reader::form_to_value).collect();
        self.macro_form_stack.push(raw_call_form);
        // D12: the dynamic `*ns*` reads as the expansion SITE's LEXICAL
        // namespace (`self.current_ns`, not yet swapped to the macro's own
        // `ns` -- `apply_closure` does that below) for the whole expansion,
        // which is what the JVM compiler's per-load `*ns*` binding amounts
        // to. Skipped outright when the two already agree; see
        // `Interp::enter_expansion_ns` for the defect, the oracle
        // measurements and the perf note. This is THE macro-expansion funnel
        // -- the tree-walker (`eval::eval_list`), the compiler's freezing
        // expansion (`compile::resolve`) and `macroexpand-1`
        // (`eval::special_forms::macroexpand_1_value`) are its only three
        // callers, and `compile::exec`'s `finish_call` raises `late_macro`
        // rather than expanding -- so one bracket here covers every entry
        // point.
        let saved_ns = self.enter_expansion_ns();
        // Freshly built and dead after the call, so it is handed over rather
        // than lent (phase 3) -- a macro's quoted arg forms can be large.
        let result = if self.moveargs_enabled {
            let mut arg_values = arg_values;
            self.apply_closure_buf(closure, &mut arg_values, call_span)
        } else {
            self.apply_closure(closure, &arg_values, call_span)
        };
        // Unwound on the `Err` path too -- `result` is a value, not a `?`,
        // which is exactly what `macro_form_stack`'s own pop already relied
        // on (see this fn's doc).
        self.leave_expansion_ns(saved_ns);
        self.macro_form_stack.pop();
        result
    }

    /// Arity selection, the depth guard and the stack frame are shared by
    /// BOTH tiers -- only the body execution differs (v0.3 / S2). Selecting
    /// the arity once, here, from the legacy `arities` is what guarantees a
    /// compiled fn can never disagree with a tree-walked one about which
    /// arity ran or what an arity error says: `compiled.code.arities` is
    /// built 1:1 with `arities`, so the same index addresses both.
    fn apply_closure(&mut self, rc: &Arc<Closure>, args: &[Value], call_span: Span) -> Result<Value, RjError> {
        // NOT factored into `enter_closure`/`leave_closure` helpers shared
        // with `apply_closure_buf`, tempting as that is: doing so cost ~4.5%
        // on `bench_call_dispatch`'s compiled arm even at `#[inline(always)]`
        // (measured; the probe's whole subject is this function's per-call
        // overhead). The twin below is a deliberate, and deliberately
        // adjacent, copy -- keep them in step.
        let idx = select_arity_index(&rc.arities, args.len()).ok_or_else(|| {
            // C3c (errors.clj's `arity-exception` deftest, `.-actual`):
            // the real count of arguments THIS call passed -- what
            // `clojure.lang.ArityException.actual` carries (see
            // `RjError::arity_actual`'s own doc). Shared by both
            // user-fn/macro call sites (`apply_closure`/`apply_closure_
            // buf` -- macros run through here too, `apply_macro`'s own
            // doc: "a macro is just a fn whose arguments happen to be
            // unevaluated forms").
            self.arity_here(arity_error_message(&rc.ns, rc.name.as_deref(), args.len()), call_span)
                .with_arity_actual(args.len() as i64)
        })?;

        // K1 (docs/JIT.md "Fast call path"): threaded entry on a slot-stack frame.
        if let Some(r) = self.fast_call_with(rc, idx, args.len(), call_span, |base| {
            for (i, a) in args.iter().enumerate() {
                unsafe { std::ptr::write(base.add(i), a.clone()) };
            }
        }) {
            return r;
        }
        if self.call_depth() > self.max_depth {
            return Err(self.other_here("stack overflow", call_span));
        }
        // Fuel: CALL ENTRY, shared by both tiers -- a compiled fn reaches
        // this same function via `apply_value_owned`/`apply_value`, so one
        // check here covers "every fn call" for the tree-walker and the
        // compiled tier alike. Loop/self-recur back-edges are checked
        // separately (`eval_loop`, `run_closure_trampoline`,
        // `compile::exec`'s `exec_loop`/`compiled_call_body!`) since they
        // never pass back through here.
        self.tick_fuel().map_err(|e| e.with_span(call_span))?;

        // D9: `^long`/`^double` parameter coercion -- the ONE thing that
        // happens between arity selection and tier dispatch, so a compiled
        // body and a tree-walked one can no more disagree about it than
        // they can about which arity ran. See `Arity::coerce`.
        //
        // The unhinted case (essentially every fn in every program) is one
        // never-taken branch on an `Option` discriminant that is already in
        // cache -- `rc.arities[idx]` was just read by `select_arity_index`.
        // It cannot allocate, cannot clone an argument, and cannot touch
        // `args` at all. The hinted case buys a `Vec` because `args` is a
        // borrowed slice here; `apply_closure_buf` below, which owns its
        // buffer, coerces in place instead.
        let coerced_buf;
        let args: &[Value] = match &rc.arities[idx].coerce {
            None => args,
            Some(casts) => {
                coerced_buf = coerce_prim_params(casts, args).map_err(|e| e.with_span(call_span))?;
                &coerced_buf
            }
        };

        let frame_name: Str = rc.name.clone().unwrap_or_else(Self::anon_frame_name);
        if crate::profile::enabled() {
            crate::profile::push(&self.profile_frame_name(rc));
        }
        // A fn body always runs in the namespace it was WRITTEN in, in both
        // tiers -- see `crate::ns`. Restored on every path, including an
        // error unwinding out of the body.
        self.stack.push(Frame {
            name: frame_name,
            span: call_span,
            source_id: self.source_id,
        });
        // SPEC-W5: the DISPLACED namespace is parked in `ns_stack` rather
        // than in a Rust local -- exactly the one `rc.ns.clone()` this
        // bracket always made, no extra refcount traffic, and `callstack*`
        // can then name every live frame. See `Interp::ns_stack`. It moves
        // back out at the matching pop below.
        self.ns_stack.push(std::mem::replace(&mut self.current_ns, rc.ns.clone()));
        // field2/W-NS: same bracket, same three call sites -- see
        // `Interp::closure_depth`'s field doc. A fn BODY is running from
        // here to the restore below, so a `ns`/`in-ns` executed inside it
        // moves only the dynamic `*ns*`, never this body's lexical
        // resolution namespace.
        self.closure_depth += 1;
        // W4C: same shape, same reason -- see `Interp::current_unchecked`'s
        // field doc. A plain `bool` swap, unconditional like `current_ns`'s
        // `Str` swap right above it: cheap enough not to need the
        // conditional-touch-only-when-true shape a costlier field would.
        let caller_unchecked = std::mem::replace(&mut self.current_unchecked, rc.unchecked_math);
        // Lazy tier-up: `on_call` is the ONE hook that turns "N calls have
        // now happened" into "compile, once, and cache it forever" -- see
        // `CompileSlot::on_call`. Already-settled (compiled OR bailed OR
        // eager-mode) closures hit its fast path and never re-enter
        // `compile_fn`.
        let compiled = rc.compiled.on_call(crate::compile::lazy_tier_n(), || {
            // The compile attempt must report positions against the fn's
            // OWN buffer, not whatever buffer THIS call happens to be
            // running from -- see `Closure::def_source_id`'s doc.
            let saved_source = std::mem::replace(&mut self.source_id, rc.def_source_id.get());
            let r = crate::compile::compile_fn(self, rc.name.as_ref(), rc.arities.as_slice(), &rc.env, rc.def_span);
            self.source_id = saved_source;
            r
        });
        let result = match compiled {
            Some(cc) => crate::compile::exec::run_compiled_body(self, rc, cc, idx, args),
            None => {
                // field4/W-LENS-1: THE headline regret event -- this whole
                // fn is tree-walking, for this call, again. Multiplied by
                // the bail reason in the site table it is `reason x count`,
                // the number that would have found session 12's 47x in
                // seconds. It sits on the SLOW arm only: a compiled fn
                // never reaches it.
                crate::lens::event_at(crate::lens::Event::TierBailExec, rc.lens_site());
                self.run_closure_body(rc, &rc.arities[idx], args, call_span)
            }
        };
        self.current_unchecked = caller_unchecked;
        self.closure_depth -= 1;
        // SPEC-W5: the caller's namespace comes back OUT of `ns_stack` --
        // the move that pairs with the one at the push above.
        if let Some(ns) = self.ns_stack.pop() {
            self.current_ns = ns;
        }
        self.stack.pop();
        crate::profile::pop();
        result
    }

    /// [`Self::apply_closure`] with the arguments handed over: identical in
    /// every observable way (same arity selection, same depth guard, same
    /// frame, same namespace swap -- all of it shared through
    /// `enter_closure`/`leave_closure`), but the arguments are MOVED into the
    /// callee's slots/bindings instead of cloned out of a slice the caller
    /// keeps.
    ///
    /// Why that is sound, and why it is only a *performance* change: each
    /// argument index is consumed exactly once by the binder (positional
    /// params take disjoint indices from the `& rest` tail, and the recur
    /// trampoline rebinds from its own scratch block, never from `args`), and
    /// the `Vec` is dropped on return. So every value ends up in exactly the
    /// same binding holding exactly the same `Value`; the only difference is
    /// that the args buffer no longer holds a *second* handle on each of them
    /// for the duration of the body. Handle counts are invisible to the
    /// language -- `builtins::reuse`'s invariant means a handle that turns out
    /// to be non-unique merely copies, exactly as before.
    fn apply_closure_buf(&mut self, rc: &Arc<Closure>, args: &mut [Value], call_span: Span) -> Result<Value, RjError> {
        #[cfg(feature = "k2-count")]
        crate::k2count::CLJ_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // The deliberate twin of `apply_closure` above -- same arity
        // selection, same depth guard, same frame, same namespace swap, in
        // the same order. See that function for why this is a copy and not a
        // shared helper.
        let idx = select_arity_index(&rc.arities, args.len()).ok_or_else(|| {
            // C3c (errors.clj's `arity-exception` deftest, `.-actual`):
            // the real count of arguments THIS call passed -- what
            // `clojure.lang.ArityException.actual` carries (see
            // `RjError::arity_actual`'s own doc). Shared by both
            // user-fn/macro call sites (`apply_closure`/`apply_closure_
            // buf` -- macros run through here too, `apply_macro`'s own
            // doc: "a macro is just a fn whose arguments happen to be
            // unevaluated forms").
            self.arity_here(arity_error_message(&rc.ns, rc.name.as_deref(), args.len()), call_span)
                .with_arity_actual(args.len() as i64)
        })?;

        // E1a (docs/JIT.md): the entry hook, at the earliest point the fn
        // AND its argc are known -- before any of the bookkeeping below
        // (frame push, ns swap, fuel tick), because a successful native
        // call needs none of it: the subset has no side effects and cannot
        // itself error, so there is nothing for a stack frame to describe.
        // `self.fuel.is_none()`: fuel_test.rs relies on every call ticking
        // fuel, which native code does not do, so a fuel-limited interp
        // never enters it. `!profile::enabled()`: profiling wants every
        // call, native or not, to show up in its trace.
        if crate::jit::enabled() && self.fuel.is_none() && !self.intr_armed && !crate::profile::enabled() {
            if let Some(cc) = rc.compiled.compiled() {
                let arity = &cc.code.arities[idx];
                // D9: a `^long`/`^double` param hint coerces the argument
                // at the call boundary (see `coerce_prim_params_in_place`
                // just below) -- the native entry never lowered that cast,
                // so it must not shortcut an arity that has one.
                if rc.arities[idx].coerce.is_none()
                    && !arity.jit.is_disabled()
                    && args.iter().all(|v| matches!(v, Value::Int(_)))
                {
                    if let Some(entry) = arity.jit.get_or_lower(&cc.code, idx) {
                        let remaining = self.max_depth.saturating_sub(self.call_depth()) as i64;
                        if remaining > 0 {
                            match crate::jit::call_native(entry, args, remaining) {
                                Some(v) => return Ok(v),
                                // BAIL: no side effects happened, so `args`
                                // (untouched -- native only READS them) is
                                // still exactly what the interpreter needs
                                // to redo this call from scratch. E1b: a
                                // single bail no longer permanently disables
                                // -- a direct-call miss just means the
                                // callee wasn't JIT'd yet, so give it 8
                                // tries to warm up (docs/JIT.md).
                                None => arity.jit.record_bail(),
                            }
                        }
                    }
                }
            }
        }

        // K1 (docs/JIT.md "Fast call path"): args MOVED into the slot-stack frame.
        if let Some(r) = self.fast_call_with(rc, idx, args.len(), call_span, |base| {
            for (i, a) in args.iter_mut().enumerate() {
                unsafe { std::ptr::write(base.add(i), std::mem::replace(a, Value::Nil)) };
            }
        }) {
            return r;
        }
        if self.call_depth() > self.max_depth {
            return Err(self.other_here("stack overflow", call_span));
        }
        // Fuel: CALL ENTRY -- see the identical check + comment in
        // `apply_closure` above (this is its deliberate twin).
        self.tick_fuel().map_err(|e| e.with_span(call_span))?;

        // D9: the twin of `apply_closure`'s coercion, in the same slot --
        // see there. This buffer is OWNED by the caller and about to be
        // moved into the callee's slots/bindings anyway, so the coerced
        // value is written straight back over the argument instead of into
        // a fresh `Vec`.
        if let Some(casts) = &rc.arities[idx].coerce {
            coerce_prim_params_in_place(casts, args).map_err(|e| e.with_span(call_span))?;
        }

        let frame_name: Str = rc.name.clone().unwrap_or_else(Self::anon_frame_name);
        if crate::profile::enabled() {
            crate::profile::push(&self.profile_frame_name(rc));
        }
        self.stack.push(Frame {
            name: frame_name,
            span: call_span,
            source_id: self.source_id,
        });
        // SPEC-W5: the DISPLACED namespace is parked in `ns_stack` rather
        // than in a Rust local -- exactly the one `rc.ns.clone()` this
        // bracket always made, no extra refcount traffic, and `callstack*`
        // can then name every live frame. See `Interp::ns_stack`. It moves
        // back out at the matching pop below.
        self.ns_stack.push(std::mem::replace(&mut self.current_ns, rc.ns.clone()));
        // field2/W-NS: same bracket, same three call sites -- see
        // `Interp::closure_depth`'s field doc. A fn BODY is running from
        // here to the restore below, so a `ns`/`in-ns` executed inside it
        // moves only the dynamic `*ns*`, never this body's lexical
        // resolution namespace.
        self.closure_depth += 1;
        // W4C: the deliberate twin of `apply_closure`'s identical swap --
        // see `Interp::current_unchecked`'s field doc.
        let caller_unchecked = std::mem::replace(&mut self.current_unchecked, rc.unchecked_math);
        // Lazy tier-up: see `apply_closure`'s twin arm.
        let compiled = rc.compiled.on_call(crate::compile::lazy_tier_n(), || {
            // The compile attempt must report positions against the fn's
            // OWN buffer, not whatever buffer THIS call happens to be
            // running from -- see `Closure::def_source_id`'s doc.
            let saved_source = std::mem::replace(&mut self.source_id, rc.def_source_id.get());
            let r = crate::compile::compile_fn(self, rc.name.as_ref(), rc.arities.as_slice(), &rc.env, rc.def_span);
            self.source_id = saved_source;
            r
        });
        let result = match compiled {
            // E3a (docs/NATIVE-TIER-DESIGN.md): the generic (any-`Value`)
            // tier executes just the BODY here -- AFTER the depth
            // check/frame push/ns-swap above, unlike E1's int entry (which
            // returns before all of that for speed). It must: E1's self-
            // recursion and CallGlobal never re-enter Rust (native-to-
            // native `call`/loop, bounded by `JitCtx.depth`'s own cap), but
            // this tier's `CallGlobal` genuinely calls back into
            // `apply_value_owned` -> this same fn, so it needs the SAME
            // `self.stack.len() > self.max_depth` guard the interpreter
            // uses, or unbounded non-tail recursion through it would blow
            // the real Rust stack instead of raising "stack overflow".
            Some(cc) if crate::jit::generic_enabled() && rc.arities[idx].coerce.is_none() => {
                match cc.code.arities[idx].jit.get_or_lower_generic(&cc.code, idx) {
                    Some(gentry) => {
                        let remaining = self.max_depth.saturating_sub(self.call_depth()) as i64;
                        // F1: `Ir::SelfRef` needs the ACTUAL running closure
                        // (not a fresh global lookup -- see `JitCtx::self_val`).
                        let self_val = Value::Fn(rc.clone());
                        crate::jit::call_native_generic(gentry, self, args, remaining, &self_val)
                    }
                    None => crate::compile::exec::run_compiled_body_buf(self, rc, cc, idx, args),
                }
            }
            Some(cc) => crate::compile::exec::run_compiled_body_buf(self, rc, cc, idx, args),
            None => {
                // field4/W-LENS-1: see `apply_closure`'s twin arm.
                crate::lens::event_at(crate::lens::Event::TierBailExec, rc.lens_site());
                self.run_closure_body_buf(rc, &rc.arities[idx], args, call_span)
            }
        };
        self.current_unchecked = caller_unchecked;
        self.closure_depth -= 1;
        // SPEC-W5: the caller's namespace comes back OUT of `ns_stack` --
        // the move that pairs with the one at the push above.
        if let Some(ns) = self.ns_stack.pop() {
            self.current_ns = ns;
        }
        self.stack.pop();
        crate::profile::pop();
        result
    }

    /// W3e-3: the `apply` shape of `f`, when `f` is (or derefs to) a closure
    /// with a `& rest` arity -- `(closure, required, probe_limit)`.
    ///
    /// `required` is that arity's fixed-param count (the JVM's
    /// `RestFn.getRequiredArity`). `probe_limit` is how many arguments
    /// `apply` may realize before it KNOWS the variadic arity is the one
    /// that will be selected: one more than the largest fixed arity
    /// (`select_arity_index` prefers an exact fixed match, so `(fn ([a b]
    /// ..) ([a & r] ..))` applied to 2 things must take the 2-arg body, and
    /// only a 3rd argument settles it), and at least `required + 1` so a
    /// non-empty tail is always detectable.
    ///
    /// `None` -- meaning "realize the whole seq, exactly as before" -- for
    /// natives, keywords, maps, and any closure with no variadic arity.
    /// Natives take a `&[Value]` slice by construction, so there is nothing
    /// to hand an unrealized tail to.
    pub(crate) fn variadic_apply_shape(&self, f: &Value) -> Option<(Arc<Closure>, usize, usize)> {
        let rc = match f {
            Value::Fn(rc) => rc.clone(),
            Value::Var(cell) => match cell.get()? {
                Value::Fn(rc) => rc,
                _ => return None,
            },
            _ => return None,
        };
        let required = rc.arities.iter().find(|a| a.rest.is_some())?.params.len();
        let max_fixed = rc
            .arities
            .iter()
            .filter(|a| a.rest.is_none())
            .map(|a| a.params.len())
            .max()
            .unwrap_or(0);
        let probe_limit = required.max(max_fixed) + 1;
        Some((rc, required, probe_limit))
    }

    /// W3e-3: call `rc`'s variadic arity with `fixed` bound positionally and
    /// `rest` handed to the `& rest` parameter AS IS -- crucially without
    /// walking it. This is the JVM's `RestFn.doInvoke(..., arglist)`: a
    /// variadic fn's rest parameter is a SEQ, and `apply` has no business
    /// realizing it. `(defn sample [& args] 0)` `(apply sample (range))` is
    /// `0` on real Clojure and used to hang mova forever
    /// (`clojure.test-clojure.vars/test-vars-apply-lazily`).
    ///
    /// Tier selection is the SAME as `apply_closure`'s (`rc.compiled` decides)
    /// and must stay that way: a nested fn created inside a compiled body
    /// carries `env = <enclosing closure's creation env>` with its free
    /// variables in `captures`, not in that env (see `compile::exec::
    /// make_closure`'s doc), so tree-walking a compiled closure's body would
    /// fail to resolve them. Measured, before this was a branch: `((apply
    /// juxt [inc dec (partial * 2)]) 10)` died with "Unable to resolve
    /// symbol: f" inside `partial`'s returned closure.
    pub(crate) fn apply_closure_lazy_rest(
        &mut self,
        rc: &Arc<Closure>,
        fixed: Vec<Value>,
        rest: Value,
        call_span: Span,
    ) -> Result<Value, RjError> {
        let idx = rc
            .arities
            .iter()
            .position(|a| a.rest.is_some())
            .expect("variadic_apply_shape only answers Some for a closure with a variadic arity");
        if self.call_depth() > self.max_depth {
            return Err(self.other_here("stack overflow", call_span));
        }
        self.tick_fuel().map_err(|e| e.with_span(call_span))?;

        // D9: the third and last call boundary -- `apply` onto a variadic
        // arity. Real Clojure cannot even COMPILE a hinted variadic arity
        // ("fns taking primitives cannot be variadic"), so `coerce` is
        // `None` here for every program the oracle would accept; mova
        // honours the hint anyway rather than leaving one of its three
        // entry points silently inconsistent with the other two. `casts` is
        // parallel to the FIXED params only, which is exactly `fixed` --
        // the rest parameter never carries a cast (see `Arity::coerce`).
        let mut fixed = fixed;
        if let Some(casts) = &rc.arities[idx].coerce {
            coerce_prim_params_in_place(casts, &mut fixed).map_err(|e| e.with_span(call_span))?;
        }

        let frame_name: Str = rc.name.clone().unwrap_or_else(Self::anon_frame_name);
        if crate::profile::enabled() {
            crate::profile::push(&self.profile_frame_name(rc));
        }
        self.stack.push(Frame {
            name: frame_name,
            span: call_span,
            source_id: self.source_id,
        });
        // SPEC-W5: the DISPLACED namespace is parked in `ns_stack` rather
        // than in a Rust local -- exactly the one `rc.ns.clone()` this
        // bracket always made, no extra refcount traffic, and `callstack*`
        // can then name every live frame. See `Interp::ns_stack`. It moves
        // back out at the matching pop below.
        self.ns_stack.push(std::mem::replace(&mut self.current_ns, rc.ns.clone()));
        // field2/W-NS: same bracket, same three call sites -- see
        // `Interp::closure_depth`'s field doc. A fn BODY is running from
        // here to the restore below, so a `ns`/`in-ns` executed inside it
        // moves only the dynamic `*ns*`, never this body's lexical
        // resolution namespace.
        self.closure_depth += 1;
        // W4C: same shape as `apply_closure`'s swap -- see
        // `Interp::current_unchecked`'s field doc.
        let caller_unchecked = std::mem::replace(&mut self.current_unchecked, rc.unchecked_math);
        // Lazy tier-up: see `apply_closure`'s twin arm.
        let compiled = rc.compiled.on_call(crate::compile::lazy_tier_n(), || {
            // The compile attempt must report positions against the fn's
            // OWN buffer, not whatever buffer THIS call happens to be
            // running from -- see `Closure::def_source_id`'s doc.
            let saved_source = std::mem::replace(&mut self.source_id, rc.def_source_id.get());
            let r = crate::compile::compile_fn(self, rc.name.as_ref(), rc.arities.as_slice(), &rc.env, rc.def_span);
            self.source_id = saved_source;
            r
        });
        let result = match compiled {
            Some(cc) => {
                crate::compile::exec::run_compiled_body_lazy_rest(self, rc, cc, idx, fixed, rest)
            }
            None => {
                // field4/W-LENS-1: see `apply_closure`'s twin arm.
                crate::lens::event_at(crate::lens::Event::TierBailExec, rc.lens_site());
                let arity = &rc.arities[idx];
                let call_env = rc.env.child();
                // Same binding discipline as `bind_params`, minus its
                // rest-building walk: one positional `set` per fixed param,
                // then the rest symbol.
                for (p, v) in arity.params.iter().zip(fixed) {
                    call_env.set(p.clone(), v);
                }
                if let Some(r) = &arity.rest {
                    call_env.set(r.clone(), rest);
                }
                self.run_closure_trampoline(rc, arity, call_env, call_span)
            }
        };
        self.current_unchecked = caller_unchecked;
        self.closure_depth -= 1;
        // SPEC-W5: the caller's namespace comes back OUT of `ns_stack` --
        // the move that pairs with the one at the push above.
        if let Some(ns) = self.ns_stack.pop() {
            self.current_ns = ns;
        }
        self.stack.pop();
        crate::profile::pop();
        result
    }

    /// The recur trampoline: evaluates `arity`'s body against freshly bound
    /// params, and on an `ErrorKind::Recur` signal, rebinds and loops
    /// in place instead of growing the Rust call stack.
    fn run_closure_body(&mut self, rc: &Arc<Closure>, arity: &Arity, args: &[Value], call_span: Span) -> Result<Value, RjError> {
        let call_env = rc.env.child();
        bind_params(&call_env, &arity.params, &arity.rest, args.len(), args.iter().cloned());
        self.run_closure_trampoline(rc, arity, call_env, call_span)
    }

    /// [`Self::run_closure_body`] with the initial binding MOVING out of the
    /// args buffer the caller handed over (phase 3). Everything after the
    /// first `bind_params` -- including the `recur` trampoline, which rebinds
    /// from its own freshly built `new_args` and never re-reads the original
    /// arguments -- is the shared `run_closure_trampoline`.
    fn run_closure_body_buf(
        &mut self,
        rc: &Arc<Closure>,
        arity: &Arity,
        args: &mut [Value],
        call_span: Span,
    ) -> Result<Value, RjError> {
        let call_env = rc.env.child();
        let argc = args.len();
        bind_params(
            &call_env,
            &arity.params,
            &arity.rest,
            argc,
            args.iter_mut().map(|v| std::mem::replace(v, Value::Nil)),
        );
        self.run_closure_trampoline(rc, arity, call_env, call_span)
    }

    /// K5 census: tree-walked closure bodies as `TreeWalk[name]` frames (k2-count only).
    #[inline(always)]
    fn run_closure_trampoline(
        &mut self,
        rc: &Arc<Closure>,
        arity: &Arity,
        call_env: crate::env::Env,
        call_span: Span,
    ) -> Result<Value, RjError> {
        #[cfg(feature = "k2-count")]
        if crate::k2count::census::on() {
            let t = crate::k2count::census::enter();
            let r = self.run_closure_trampoline0(rc, arity, call_env, call_span);
            crate::k2count::census::exit(t, || format!("TreeWalk[{}]", rc.name.as_deref().unwrap_or("?")));
            return r;
        }
        self.run_closure_trampoline0(rc, arity, call_env, call_span)
    }

    fn run_closure_trampoline0(
        &mut self,
        rc: &Arc<Closure>,
        arity: &Arity,
        mut call_env: crate::env::Env,
        call_span: Span,
    ) -> Result<Value, RjError> {
        if let Some(name) = &rc.name {
            call_env.set(Symbol::simple(name.clone()), Value::Fn(rc.clone()));
        }
        loop {
            match self.eval_do_body(&arity.body, &call_env) {
                Ok(v) => return Ok(v),
                Err(e) if e.kind == ErrorKind::Recur => {
                    // Fuel: fn SELF-RECUR back-edge (tree-walker). This loop
                    // never re-enters `apply_closure`, so the call-entry
                    // check above does not see these iterations -- without
                    // this, `(defn f [] (recur))` would run forever under a
                    // finite fuel budget even though `(loop [] (recur))`
                    // would not.
                    self.tick_edge().map_err(|fe| fe.with_span(e.span.unwrap_or(call_span)))?;
                    let new_args = self.recur_args(&e)?;
                    let expected = arity.params.len() + usize::from(arity.rest.is_some());
                    if new_args.len() != expected {
                        return Err(self.arity_here(
                            format!(
                                "recur: expected {expected} argument(s) to match fn params, got {}",
                                new_args.len()
                            ),
                            e.span.unwrap_or(call_span),
                        ));
                    }
                    let mut new_args = new_args;
                    // D9: a self-recur is still a call to this same arity, so
                    // its arguments need the identical `^long`/`^double`
                    // coercion `apply_closure` gives the initial call -- see
                    // `coerce_prim_params_in_place`'s doc. `casts` is
                    // parallel to the FIXED params only; the trailing rest
                    // value `new_args` may carry when `arity.rest` is `Some`
                    // is left untouched, since the zip stops at `casts.len()`
                    // on its own.
                    if let Some(casts) = &arity.coerce {
                        coerce_prim_params_in_place(casts, &mut new_args)
                            .map_err(|fe| fe.with_span(e.span.unwrap_or(call_span)))?;
                    }
                    let next_env = rc.env.child();
                    // `new_args` is this trampoline's own freshly built
                    // vector, dead after the rebind, so the loop-carried
                    // values move rather than clone (phase 3; the compiled
                    // tier's back edge already moved out of its scratch
                    // block for the same reason -- see `compile::exec`).
                    // Gated so `MOVA_NO_MOVEARGS=1` really does restore
                    // every phase-3 handle, which is what makes the
                    // same-binary A/B an attribution rather than a guess.
                    // mova campaign (clojure-lsp): `bind_params` is the
                    // wrong binder here -- see `bind_params_recur`'s doc
                    // for why a `recur` to a variadic arity must bind its
                    // trailing value to `& rest` AS-IS, not wrapped in a
                    // fresh one-element `List` the way a genuinely EXTRA
                    // positional argument on a normal call would be.
                    if self.moveargs_enabled {
                        bind_params_recur(
                            &next_env,
                            &arity.params,
                            &arity.rest,
                            new_args.iter_mut().map(|v| std::mem::replace(v, Value::Nil)),
                        );
                    } else {
                        bind_params_recur(&next_env, &arity.params, &arity.rest, new_args.iter().cloned());
                    }
                    if let Some(name) = &rc.name {
                        next_env.set(Symbol::simple(name.clone()), Value::Fn(rc.clone()));
                    }
                    call_env = next_env;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn arity_here(&self, msg: impl Into<String>, span: Span) -> RjError {
        RjError::arity(msg).with_span(span).with_stack(self.stack_snapshot(), self.source_id)
    }
    fn type_err_here(&self, msg: impl Into<String>, span: Span) -> RjError {
        RjError::type_err(msg).with_span(span).with_stack(self.stack_snapshot(), self.source_id)
    }
    fn other_here(&self, msg: impl Into<String>, span: Span) -> RjError {
        RjError::other(msg).with_span(span).with_stack(self.stack_snapshot(), self.source_id)
    }
}

/// Binds `n_args` arguments into `env`: positional params by index, and the
/// `& rest` param as `nil` when there are no extras (NOT an empty list),
/// otherwise a `List` of them.
///
/// The arguments arrive as an ITERATOR consumed strictly in order -- cloning
/// for a borrowed call, `std::mem::replace`-ing for a handed-over one. That
/// is what lets both callers share this one copy of the rule (the rest-wrap
/// in particular is load-bearing: the tiers must not disagree about it), and
/// an iterator rather than an index getter is deliberate -- indexing a slice
/// per parameter reintroduces a bounds check per argument on the hottest
/// path in the tree-walked tier, which `bench_call_dispatch` notices.
fn bind_params(
    env: &crate::env::Env,
    params: &[Symbol],
    rest: &Option<Symbol>,
    n_args: usize,
    mut vals: impl Iterator<Item = Value>,
) {
    for p in params.iter().take(n_args) {
        env.set(p.clone(), vals.next().unwrap_or(Value::Nil));
    }
    if let Some(r) = rest {
        let start = params.len().min(n_args);
        let val = if start >= n_args {
            Value::Nil
        } else {
            Value::List(vals.take(n_args - start).collect())
        };
        env.set(r.clone(), val);
    }
}

/// S8 (mova campaign, real bug fix): binds a `recur`-to-self-variadic-
/// arity's arguments, WITHOUT `bind_params`'s list-wrapping of the rest
/// slot. Oracle-verified divergence (`compat/` transcript, and this
/// task's own repro): for `(defn f [m k v & kvs] ... (recur m k2 v2
/// (nnext kvs)))`, real Clojure binds the new `kvs` to EXACTLY the value
/// `(nnext kvs)` produced -- it does NOT wrap it in a fresh one-element
/// `List`. `bind_params` (used for every NORMAL call, where "one extra
/// positional arg" and "the rest value itself" are genuinely different
/// things) is correct to wrap; a self-`recur` to a variadic arity is not
/// a normal call in this respect -- the compiler requires it to supply
/// exactly `params.len() + 1` args, and that last one IS the rest
/// param's next value, verbatim (an old fn-body-self-recur/compiled-tier
/// comment near `bind_param_slots` called the wrapping behavior
/// "long-standing" and "load-bearing" -- it was actually just a bug both
/// tiers happened to agree on; fixed in lockstep, see
/// `compile::exec::bind_param_slots_recur`). The caller's own arity
/// check already guarantees exactly `params.len() + (1 if rest.is_some
/// else 0)` values are available, so no extra bookkeeping is needed
/// here.
fn bind_params_recur(env: &crate::env::Env, params: &[Symbol], rest: &Option<Symbol>, mut vals: impl Iterator<Item = Value>) {
    for p in params.iter() {
        env.set(p.clone(), vals.next().unwrap_or(Value::Nil));
    }
    if let Some(r) = rest {
        env.set(r.clone(), vals.next().unwrap_or(Value::Nil));
    }
}

/// Returns the *index* (not a reference) of the arity that handles `argc`,
/// so the compiled tier can address its own parallel `CompiledArity` list
/// with the identical choice. Exact-match arities win over variadic ones.
pub(crate) fn select_arity_index(arities: &[Arity], argc: usize) -> Option<usize> {
    if let Some(i) = arities.iter().position(|a| a.rest.is_none() && a.params.len() == argc) {
        return Some(i);
    }
    arities.iter().position(|a| a.rest.is_some() && argc >= a.params.len())
}

/// C3c (errors.clj's `arity-exception` deftest, ALSO measured against
/// `keywords.clj`'s pre-existing, unrelated keyword-as-fn arity checks --
/// see below): real `clojure.lang.ArityException`'s message is
/// `"Wrong number of args (<actual>) passed to: <ns>/<name>"` -- no
/// expected-arity count at all (measured via `.getMessage`, `.oracle`
/// probe, on named top-level fns AND macros: `(f0 1)` -> `Wrong number of
/// args (1) passed to: user/f0`, a 0-arg macro invoked with 1 arg via
/// `macroexpand` -> `Wrong number of args (1) passed to: user/m0`). This
/// used to be a mova-invented shape ("`f: called with 1 argument but
/// expects 2`") that was MORE informative than the real message but
/// didn't match it textually, silently passing every `thrown-with-msg?
/// ArityException`/`IllegalArgumentException` check in scope only
/// because `ex-message` had no way to read a message off a plain arity
/// error map at all (see `RjError::arity_actual`'s doc) -- now that an
/// arity error catch-binds to a REAL exception whose `.getMessage`
/// resolves, the text must actually match, so this switched to the
/// oracle's exact shape. `ns` is `rc.ns` at both call sites (the
/// namespace the fn/macro was DEFINED in, matching the JVM's own
/// `var-name`-shaped `ArityException` message); an anonymous fn has no
/// bare name to qualify, so it keeps the `anonymous-fn` placeholder
/// unqualified (untested either way -- no vendored form ever regex-
/// matches an ANONYMOUS closure's arity message).
fn arity_error_message(ns: &str, name: Option<&str>, got: usize) -> String {
    match name {
        Some(fname) => format!("Wrong number of args ({got}) passed to: {ns}/{fname}"),
        None => format!("Wrong number of args ({got}) passed to: anonymous-fn"),
    }
}

/// W4B-MESSAGES (keywords.clj's `arity-exceptions`): the SAME
/// `ArityException` shape `arity_error_message` above already gives
/// closures, for keyword-as-fn -- `(:kw)` / `(apply :foo/bar (range N))`.
/// The one difference from a closure's message: a keyword has no `ns/`
/// var qualification to report (it isn't a Var), so `printed_name` is
/// just the keyword's own printed form (already including its leading
/// `:`, e.g. `:kw` or `:foo/bar`) -- passed in fully formed rather than
/// split into `ns`/`name` parts the way `arity_error_message` wants,
/// because a keyword genuinely has no separate "ns" to slot in there
/// (`:foo/bar`'s `foo` is the keyword's OWN namespace, already part of
/// its printed form, not a defining namespace the way a Var's is).
/// `> 20` (rather than the exact count) once `got` exceeds 20 is measured
/// directly against the oracle: `clojure.lang.AFn.applyToHelper` (what
/// `apply`'s > 20-arg fallback goes through for every `IFn`, keywords
/// included) reports the arg count via `RT.boundedLength(argList, 20)`,
/// which caps at exactly this text for anything past the 20th arg --
/// this is not a keyword-specific quirk, but only `keywords.clj` in this
/// corpus feeds an `IFn` more than 20 args, so it's implemented only
/// where measured.
fn keyword_arity_error_message(printed_name: &str, got: usize) -> String {
    if got > 20 {
        format!("Wrong number of args (> 20) passed to: {printed_name}")
    } else {
        format!("Wrong number of args ({got}) passed to: {printed_name}")
    }
}

/// Shared by keyword-as-fn (`(:k m)`) and symbol-as-fn (`('sym m)`, §5/M2)
/// -- both `Named` types implement `IFn` identically in real Clojure, a
/// 1-or-2-arg `(get coll key not-found)`. `key` is the full callee VALUE
/// (`Value::Keyword`/`Value::Sym`), not just its name, so a lookup against
/// a `Value::Map` keyed by the callee's own type matches.
fn named_lookup(key: &Value, coll: &Value, default: Option<Value>) -> Value {
    match coll {
        // S5/M3: `(:x (with-meta {:x 1} {:a 1}))` is `1`, measured --
        // keyword lookup is a READ and sees through the wrapper. NOTE the
        // fallthrough arm below returns the DEFAULT rather than erroring,
        // so without this arm the bug would be a silent wrong answer
        // (`nil`), not an exception -- which is exactly how it was caught.
        Value::Meta(m) => named_lookup(key, &m.inner, default),
        Value::Map(m) => {
            map_probe::record("keyword-lookup", m.len());
            m.get(key).cloned().unwrap_or_else(|| default.unwrap_or(Value::Nil))
        }
        // clojure-lsp campaign (mova/PLAN.md): a `sorted-map` is
        // associative like `Map`, but `(:kw sorted-map)` fell to the
        // catch-all below (always `nil`/default, contents ignored) --
        // real Clojure's `PersistentTreeMap` answers keyword-as-fn
        // lookup identically to a hash map. No comparator search here
        // (that needs `&mut Interp`, unavailable in this free fn): a
        // linear scan by `=` matches every OTHER named_lookup arm's own
        // key-equality semantics (`Map`'s `PMap::get`, `Set::contains`).
        Value::SortedMap(m) => m
            .entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| default.unwrap_or(Value::Nil)),
        // W3: the hot path -- shape-dispatch via the per-`Shape` inline
        // cache, no `PMap` materialize. See `crate::host_struct`'s doc.
        // Keyword-keyed only (a `HostStruct`'s shape is keywords), so a
        // symbol callee falls through to the default arm below.
        Value::HostStruct(hs) => match key {
            Value::Keyword(k) => crate::host_struct::lookup(hs, k.text_ref()).unwrap_or_else(|| default.unwrap_or(Value::Nil)),
            _ => default.unwrap_or(Value::Nil),
        },
        Value::LazyMap(hs) => match key {
            Value::Keyword(k) => crate::lazy_map::lookup(hs, k.text_ref()).unwrap_or_else(|| default.unwrap_or(Value::Nil)),
            _ => default.unwrap_or(Value::Nil),
        },
        // S3: records answer keyword lookup like maps (measured: `(:a
        // (->R 1 2))` is 1); deftypes don't (fall to the default arm).
        Value::Inst(inst) if inst.tdef.is_record => {
            inst.data.get(key).cloned().unwrap_or_else(|| default.unwrap_or(Value::Nil))
        }
        Value::Set(s) => {
            if s.contains(key) {
                key.clone()
            } else {
                default.unwrap_or(Value::Nil)
            }
        }
        // C2 (defstruct), measured: `(:z s)` looks up like any other map.
        Value::StructMap(sm) => crate::builtins::structmap::struct_map_get(sm, key)
            .cloned()
            .unwrap_or_else(|| default.unwrap_or(Value::Nil)),
        _ => default.unwrap_or(Value::Nil),
    }
}
