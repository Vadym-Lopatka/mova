//! Perceus-lite phases 1 and 3: the *consuming calling convention*, the
//! *Rust-caller argument handover*, and their kill switches.
//!
//! # The three phases, and what each one frees
//!
//! A collection builtin can mutate its receiver in place only if it holds
//! the ONLY handle on it. Each phase removes one class of extra handle:
//!
//! | phase | the handle it removes |
//! |---|---|
//! | 1 (this module, below) | the native's own defensive `.clone()` out of a borrowed args slice |
//! | 2 (`compile::lastuse`) | the CALLEE's frame slot, on its provably final read |
//! | 3 (`eval::apply`'s `call_owned`/`call_with_buf`) | the RUST CALLER's args buffer -- `reduce`'s `[acc, item]`, `flow`'s `[state, cid, msg]` |
//!
//! All three must fire for a receiver to be unique, which is why phases 1
//! and 2 each measured 0.00% on `bench/reuse-assoc-*.mova` and phase 3 takes
//! the same benches to 87-89%. What phase 3 also establishes is where the
//! arc STOPS: a handle a caller must keep for correctness -- flow's
//! previous state, `swap!`'s pre-CAS value -- can never be released,
//! because "recoverable after an error" and "the callee may destroy it in
//! place" are the same question with opposite answers. See
//! bench/optimization-log.md's phase-3 entry.
//!
//! # The problem this exists to fix
//!
//! Every native receives `args: &[Value]` (see `value::NativeFn::f`), a
//! borrowed view of an args `Vec` the caller owns and is about to throw
//! away. A collection builtin that wants to produce a modified copy must
//! therefore `.clone()` its receiver out of that slice before mutating it
//! -- so the persistent structure underneath (`PMap`/`PVec`, and imbl
//! below them) *always* sees a refcount >= 2 at the mutation instant and
//! *always* copies. E3's measurements (bench/optimization-log.md) put the
//! resulting gap at 3.65x at n=7 and ~10.9x at n=10000 versus mutating a
//! uniquely-held handle.
//!
//! # The convention
//!
//! A whitelisted native may additionally register a *consuming* entry point
//! (`value::NativeFn::consuming`) taking `args: Vec<Value>` **by value**.
//! `Interp::apply_value_owned` -- the single seam both tiers funnel their
//! freshly-built args `Vec` through -- prefers it when present. The callee
//! may then move its receiver out of the `Vec` (`std::mem::take`) and
//! mutate *that* handle, which for a genuinely temporary receiver is the
//! only handle in existence.
//!
//! # THE INVARIANT
//!
//! > A value may be mutated only through a handle the callee **exclusively
//! > owns**. Whether that handle is *also* the only handle in the program
//! > is neither known nor required: `Arc::make_mut`/`Arc::get_mut` (for
//! > `PMap::Small`/`PVec::Small`) and imbl's own chunk-level copy-on-write
//! > (for `PMap::Big`/`PVec::Big`) copy exactly when some other handle
//! > exists, and mutate in place exactly when none does.
//!
//! The consequence worth stating loudly, because it is what makes this
//! change safe: **observable persistent semantics cannot change, only
//! allocation traffic does.** Correctness does not depend on the whitelist
//! being right, on the caller's expression really being a temporary, or on
//! any last-use analysis. Those only determine whether the fast path
//! *pays*. A wrong guess costs a copy -- exactly what the old code paid
//! unconditionally -- never a wrong answer.
//!
//! # Kill switch
//!
//! `MOVA_NO_REUSE=1` (any value) forces every caller back onto the old
//! borrow-and-clone path. Checked through a `OnceLock<bool>`, and only
//! *after* the seam has already established that the callee is one of the
//! handful of consuming natives -- so an ordinary call pays nothing for the
//! switch's existence, and an `assoc`/`conj`/`dissoc` pays one atomic load
//! against a ~115ns operation.

use std::sync::OnceLock;

static DISABLED: OnceLock<bool> = OnceLock::new();

/// `true` unless `MOVA_NO_REUSE` is set in the environment. Read once per
/// process; every call after the first is a single atomic load.
#[inline]
pub fn enabled() -> bool {
    !*DISABLED.get_or_init(|| std::env::var_os("MOVA_NO_REUSE").is_some())
}

static NO_MOVEARGS: OnceLock<bool> = OnceLock::new();

/// Phase 3's switch: `true` unless `MOVA_NO_MOVEARGS` is set.
///
/// Gates the *argument handover* half of the convention -- whether
/// `apply_value_owned` MOVES an owned args `Vec` into a closure's parameter
/// slots (`compile::exec::run_compiled_body_owned`) / environment bindings
/// (`eval::apply::run_closure_body_owned`), or clones out of it and keeps
/// the `Vec` alive for the whole call as phases 1-2 did.
///
/// Like `enabled()`, this exists for A/B attribution and for the subprocess
/// differential in `tests/moveargs_test.rs`; it is read once per process
/// into an `Interp` field, so the hot path pays one already-hot field read.
#[inline]
pub fn moveargs_enabled() -> bool {
    !*NO_MOVEARGS.get_or_init(|| std::env::var_os("MOVA_NO_MOVEARGS").is_some())
}
