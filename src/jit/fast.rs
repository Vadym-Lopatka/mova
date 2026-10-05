//! K1 (docs/JIT.md "Fast call path (K1)"): the slot stack and the fast
//! closure call. A fast call pushes NO `Frame`: it records a `FastFrame`
//! (raw pointers, no refcounts) and every stack reader merges them back in
//! (`stack_snapshot`, `merged_stack`), so traces are identical.

use std::sync::Arc;

use crate::compile::exec::Locals;
use crate::compile::CompiledClosure;
use crate::error::RjError;
use crate::eval::{Frame, Interp};
use crate::reader::Span;
use crate::value::{Closure, Str, Value};

use super::{TCtx, ThreadedEntry};

const CHUNK: usize = 16 * 1024;

/// Per-`Interp` `Value` slots with stable addresses (chunks never realloc).
/// Invariant: every slot above the top is `Value::Nil`.
pub struct SlotStack {
    chunks: Vec<(*mut Value, usize)>,
    cur: usize,
    top: usize,
}

// Chunks are owned by this struct alone; raw pointers only avoid `Box` aliasing rules.
unsafe impl Send for SlotStack {}
unsafe impl Sync for SlotStack {}

#[derive(Clone, Copy)]
pub struct SlotMark {
    cur: usize,
    top: usize,
}

impl SlotStack {
    pub fn new() -> Self {
        Self { chunks: Vec::new(), cur: 0, top: 0 }
    }

    /// `n` Nil slots; hand the mark + pointer back to `pop`.
    #[inline(always)]
    pub fn push(&mut self, n: usize) -> (SlotMark, *mut Value) {
        let mark = SlotMark { cur: self.cur, top: self.top };
        if let Some(&(p, cap)) = self.chunks.get(self.cur) {
            if self.top + n <= cap {
                let ptr = unsafe { p.add(self.top) };
                self.top += n;
                return (mark, ptr);
            }
        }
        self.push_slow(mark, n)
    }

    #[cold]
    fn push_slow(&mut self, mark: SlotMark, n: usize) -> (SlotMark, *mut Value) {
        let mut i = if self.chunks.is_empty() { 0 } else { self.cur + 1 };
        loop {
            if i == self.chunks.len() {
                let cap = CHUNK.max(n);
                let b: Box<[Value]> = (0..cap).map(|_| Value::Nil).collect();
                self.chunks.push((Box::into_raw(b) as *mut Value, cap));
            }
            if self.chunks[i].1 >= n {
                break;
            }
            i += 1;
        }
        self.cur = i;
        self.top = n;
        (mark, self.chunks[i].0)
    }

    /// Resets the frame's slots to Nil (restoring the invariant), then the top.
    ///
    /// # Safety
    /// `mark`/`ptr`/`n` must be the matching `push`'s, popped in LIFO order.
    #[inline(always)]
    pub unsafe fn pop(&mut self, mark: SlotMark, ptr: *mut Value, n: usize) {
        #[cfg(feature = "k2-count")]
        {
            use std::sync::atomic::Ordering::Relaxed;
            let k = &crate::k2count::POPS;
            k[0].fetch_add(1, Relaxed);
            k[1].fetch_add(n as u64, Relaxed);
            for i in 0..n {
                match &*ptr.add(i) {
                    Value::Nil => {}
                    Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Keyword(crate::keyword::Keyword::Interned(_)) => {
                        k[2].fetch_add(1, Relaxed);
                    }
                    _ => {
                        k[2].fetch_add(1, Relaxed);
                        k[3].fetch_add(1, Relaxed);
                    }
                }
            }
        }
        for i in 0..n {
            let s = &mut *ptr.add(i);
            match s {
                Value::Nil => {}
                // K3: owns nothing, so no drop glue call.
                Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Keyword(crate::keyword::Keyword::Interned(_)) => {
                    std::ptr::write(s, Value::Nil)
                }
                _ => *s = Value::Nil,
            }
        }
        self.cur = mark.cur;
        self.top = mark.top;
    }
}

impl Default for SlotStack {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SlotStack {
    fn drop(&mut self) {
        for &(p, cap) in &self.chunks {
            unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, cap))) };
        }
    }
}

/// A live fast call: the `Frame` the old path would have pushed at
/// `interp.stack[base]`, kept as raw pointers (both outlive the call).
pub struct FastFrame {
    pub rc: *const Closure,
    pub span: Span,
    pub source_id: u32,
    pub base: u32,
    /// The ns this call displaced (the old path's `ns_stack` entry).
    pub caller_ns: *const Str,
}

unsafe impl Send for FastFrame {}
unsafe impl Sync for FastFrame {}

impl FastFrame {
    fn frame(&self) -> Frame {
        let rc = unsafe { &*self.rc };
        Frame {
            name: rc.name.clone().unwrap_or_else(Interp::anon_frame_name),
            span: self.span,
            source_id: self.source_id,
        }
    }
}

impl Interp {
    /// `self.stack.clone()` with live fast frames merged in at their `base`.
    pub(crate) fn stack_snapshot(&self) -> Vec<Frame> {
        if self.fast_frames.is_empty() {
            return self.stack.clone();
        }
        self.merged_stack().into_iter().map(|(f, _)| f).collect()
    }

    /// Every live frame (old-path order) with the ns it displaced.
    pub(crate) fn merged_stack(&self) -> Vec<(Frame, &Str)> {
        let ff = &self.fast_frames;
        let mut out = Vec::with_capacity(self.stack.len() + ff.len());
        let mut j = 0;
        for (i, f) in self.stack.iter().enumerate() {
            while j < ff.len() && ff[j].base as usize <= i {
                out.push((ff[j].frame(), unsafe { &*ff[j].caller_ns }));
                j += 1;
            }
            out.push((f.clone(), &self.ns_stack[i]));
        }
        for fr in &ff[j..] {
            out.push((fr.frame(), unsafe { &*fr.caller_ns }));
        }
        out
    }

    /// Live call depth: interpreter frames + fast frames.
    #[inline(always)]
    pub(crate) fn call_depth(&self) -> usize {
        self.stack.len() + self.fast_frames.len()
    }

    /// Fast-path eligibility of arity `idx` for `argc` args (process-static
    /// gates + this closure's shape); `None` = old path. Per-interp gates
    /// (fuel, depth) are checked by the caller.
    #[inline(always)]
    pub(crate) fn fast_target(rc: &Arc<Closure>, idx: usize, argc: usize) -> Option<(&CompiledClosure, ThreadedEntry, usize)> {
        if !crate::jit::enabled() || crate::profile::enabled() || crate::jit::generic_enabled() {
            return None;
        }
        let cc = rc.compiled.compiled()?;
        if rc.arities[idx].coerce.is_some() {
            return None;
        }
        let arity = &cc.code.arities[idx];
        if arity.variadic || argc != arity.n_params {
            return None;
        }
        let &(entry, n_extra) = arity.threaded.get_or_lower(&cc.code, idx, true)?;
        Some((cc, entry, arity.n_slots + n_extra as usize))
    }

    /// K1 fast call of arity `idx`: `fill` writes the `argc` args into the
    /// fresh (all-Nil) param slots. `None` = not eligible, use the old path.
    #[inline(always)]
    pub(crate) fn fast_call_with(
        &mut self,
        rc: &Arc<Closure>,
        idx: usize,
        argc: usize,
        span: Span,
        fill: impl FnOnce(*mut Value),
    ) -> Option<Result<Value, RjError>> {
        let (cc, entry, n) = Self::fast_target(rc, idx, argc)?;
        // The old path raises the (identical) overflow error.
        if self.fuel.is_some() || self.intr_armed || self.call_depth() > self.max_depth {
            return None;
        }
        let mut out = Value::Nil;
        let mut err = None;
        let status = unsafe { self.fast_invoke(rc, cc, entry, n, span, fill, &mut out, &mut err) };
        Some(if status == 0 { Ok(out) } else { Err(err.expect("status 2 must set err")) })
    }

    /// The fast call proper: slot frame, args via `fill`, `FastFrame`, ns /
    /// unchecked swap, entry, restore. Writes `*out` (status 0) or `*err` (2).
    ///
    /// # Safety
    /// `cc`/`entry`/`n` must come from `fast_target(rc, ..)`; fuel/depth checked.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn fast_invoke(
        &mut self,
        rc: &Arc<Closure>,
        cc: &CompiledClosure,
        entry: ThreadedEntry,
        n: usize,
        span: Span,
        fill: impl FnOnce(*mut Value),
        out: *mut Value,
        err: &mut Option<RjError>,
    ) -> u32 {
        let (mark, base) = self.slot_stack.push(n);
        fill(base);
        let swapped: Option<Str> = if Str::ptr_eq(&self.current_ns, &rc.ns) {
            None
        } else {
            Some(std::mem::replace(&mut self.current_ns, rc.ns.clone()))
        };
        let caller_ns: *const Str = match &swapped {
            Some(s) => s,
            None => &rc.ns,
        };
        self.fast_frames.push(FastFrame {
            rc: Arc::as_ptr(rc),
            span,
            source_id: self.source_id,
            base: self.stack.len() as u32,
            caller_ns,
        });
        self.closure_depth += 1;
        let caller_unchecked = std::mem::replace(&mut self.current_unchecked, rc.unchecked_math);
        let mut l = Locals { slots: std::slice::from_raw_parts_mut(base, n), caps: &cc.captures, me: rc };
        let mut ctx = TCtx {
            interp: self as *mut Interp,
            locals: &mut l as *mut Locals as *mut Locals<'static>,
            slots: base,
            out: Value::Nil,
            err: None,
        };
        #[cfg(feature = "k2-count")]
        let tok = crate::k2count::census::on().then(crate::k2count::census::enter);
        let status = (entry.0)(&mut ctx as *mut TCtx);
        #[cfg(feature = "k2-count")]
        if let Some(t) = tok {
            crate::k2count::census::exit(t, || "Native".to_string());
        }
        debug_assert!(status != 1, "K1: fn-level recur is native in a fast entry");
        self.current_unchecked = caller_unchecked;
        self.closure_depth -= 1;
        self.fast_frames.pop();
        match swapped {
            Some(ns) => self.current_ns = ns,
            // The callee may have run `in-ns`: the old path restores the caller's ns.
            None if !Str::ptr_eq(&self.current_ns, &rc.ns) => self.current_ns = rc.ns.clone(),
            None => {}
        }
        self.slot_stack.pop(mark, base, n);
        if status == 0 {
            *out = std::mem::replace(&mut ctx.out, Value::Nil);
            0
        } else {
            *err = ctx.err.take();
            2
        }
    }
}
