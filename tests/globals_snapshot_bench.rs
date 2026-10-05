//! W-ENV kill-probe: is the globals `Env` root `RwLock` worth replacing
//! with a read-lock-free snapshot?
//!
//! ## The workload this models
//!
//! `tests/clojure-suite/vendor/delays.clj` runs 100 threads x 10k
//! tree-walked `(is (= 1 @d))`. Every symbol the tree-walker resolves goes
//! `Interp::resolve_symbol` (ns.rs:838) -> `get_local` (lexical, root-free)
//! -> `lookup_global` (ns.rs:453) -> `for_each_global_candidate`
//! (ns.rs:365), which probes 1-3 candidate SPELLINGS per symbol, and EACH
//! candidate is one `Env::get_exact` (env.rs:569) = one acquire+drop of the
//! single root `RwLock<EnvInner>` shared by every thread in the process.
//! `get_exact` then reads the hit's value through `VarCell::get`
//! (env.rs:294) -- a SECOND shared lock, taken while the map guard is still
//! held.
//!
//! Read locks do not serialize the critical section, but each acquire and
//! each release is an atomic read-modify-write on ONE cacheline, and an RMW
//! needs that line exclusive. At 14 cores every one of those is a coherence
//! round trip, so the aggregate cost is ~6 exclusive-line handoffs per
//! symbol resolution no matter how short the body is. That is the
//! collapse the suite works around with `CLOJURE_SUITE_TIMEOUT=400`.
//!
//! ## What is measured
//!
//! One read API (`Globals`), five reprs, two modes, two thread counts:
//!
//! * `A_env`   -- the REAL `Env::get_exact`, in-process ground truth.
//! * `A_repl`  -- `RwLock<std::HashMap>` replica of it (controls for
//!                cross-crate inlining: `A_env` lives in the mova rlib,
//!                everything else is in this test crate).
//! * `B_snap`  -- `RwLock<Arc<persistent map>>`: readers take the read lock
//!                only long enough to CLONE the `Arc`, then read the map
//!                lock-free. This is the shape an `arc-swap` drop-in would
//!                have, hand-rolled.
//! * `C_tls`   -- generation-gated per-thread snapshot cache over
//!                `imbl::HashMap`: readers do ONE `Acquire` load of a
//!                counter and then read a map they already hold. No shared
//!                RMW at all on the hit path.
//! * `C_champ` -- same design over `champ::PersistentHashMap` (the
//!                in-tree CHAMP behind `Value::Map`/`Value::Set`), whose
//!                `DefaultBuildHasher` is a stateless fast hash rather than
//!                `RandomState`'s SipHash.
//!
//! `B_snap`'s `Arc` clone is itself an RMW on a shared refcount, so it is
//! NOT expected to fix contention -- it is in the table precisely to show
//! that "snapshot" alone is not the win; "no shared RMW" is.
//!
//! Modes: `value` (map probe + `VarCell::get`, i.e. the whole real read)
//! and `map` (map probe only, returning the cell's ADDRESS so not even the
//! refcount is touched). The gap between them prices the per-cell lock,
//! which this wave does NOT own -- reported so the conductor can see how
//! much headroom is left after the map lock is gone.
//!
//! BAR (owner): decisive = contended win >= 3x AND uncontended regression
//! <= 10%.
//!
//! POST-LANDING NOTE: the W-ENV repr has since been LANDED, so the `A_env`
//! row now measures the shipped design through the real `Env` instead of
//! the lock it replaced. `A_repl` -- the `RwLock<std::HashMap>` replica --
//! is the pre-W-ENV baseline every verdict is taken against, which is why
//! it stays in the table.
//!
//! Run: `cargo test --release --test globals_snapshot_bench -- --ignored
//! --nocapture --test-threads=1` (the measurements are `#[ignore]`d; only
//! the cheap `candidate_reprs_agree_with_real_env` correctness check runs in
//! the ordinary gate).
//!
//! ## W-CELL verdict (2026-08-22): the cell lock is real, and irrelevant
//!
//! `cellfree_kill_probe` adds `E_cellfree` -- `VarCell`'s
//! `RwLock<Option<Value>>` replaced by `AtomicPtr` over an immutable boxed
//! payload with a retire list, `RootGlobals`' idiom one level down -- and
//! crosses it with the payload kind. Measured, 13 readers, M4 Pro:
//!
//! ```text
//!  set  payload      Env::get_exact  D_atom value  E_cellfree value  E addr   D addr
//!    6  Arc(Str)          13.28M/s      13.25M/s        31.09M/s    345.4M/s 335.1M/s
//!    6  inline(Int)       13.62M/s      12.80M/s       296.84M/s    317.8M/s 333.1M/s
//!   64  Arc(Str)          43.77M/s      44.21M/s        85.96M/s    274.5M/s 290.8M/s
//!   64  inline(Int)       70.22M/s      73.22M/s       267.48M/s    272.1M/s 283.4M/s
//!  800  Arc(Str)          52.60M/s      57.33M/s        90.24M/s    179.0M/s 175.4M/s
//!  800  inline(Int)       69.49M/s      76.67M/s       160.84M/s    166.5M/s 180.9M/s
//! ```
//!
//! Reading the cross at the hot-set size the file actually has (6 cells):
//! removing the lock alone is worth **23.2x** (inline row), and the shared
//! `Arc` refcount on the payload then caps the real read at **31M/s**, an
//! 11.1x cliff below the map-only ceiling. So BOTH halves are walls, the
//! lock is the larger one, and lock removal alone delivers 2.35x -- W-ENV's
//! "no shared RMW is the win" lesson holding a second time, now for the
//! payload's refcount rather than the map's.
//!
//! And it does not matter. `sample`ing the real `delays.clj` run (124.51s
//! wall / 1579s user, two windows covering both parallel tests) puts
//! `VarCell::get` at **0.6% of self time**. The file's cost is macro
//! RE-EXPANSION in the tree-walker: `eval::Interp::eval_list` (mod.rs:1384-
//! 1390) expands a `Value::Macro` head on EVERY evaluation, paying
//! `form_to_value` on the whole call and `value_to_form` on the expansion
//! each time -- `value_to_form` alone is 10.6%/11.4% of the two windows,
//! and the `Value`/`Symbol`/`Form` clone-and-drop plus malloc traffic it
//! drives is ~40% more. The compiled tier expands once (resolve.rs:591);
//! this loop reaches the tree-walker because `binding` and `try` hand back
//! to it, so every `(is (= 1 @d))` re-expands `is` 100 threads wide.
//!
//! `MOVA_EXPLAIN=1` names the trigger exactly, 101 times (once per worker
//! thread plus the template): the worker `(fn [] (.await barrier) (dotimes
//! [_ 10000] (is ...)) (.await barrier))` tree-walks because it contains an
//! interop dot-form, and `compile::resolve`'s bail poisons the WHOLE fn, so
//! two `.await` calls per thread cost 10k tree-walked, re-expanded `is`
//! bodies each. Hoisting the loop into an interop-free `defn` -- the same
//! 25 assertions, same passes -- takes the file from **124.51s to 2.95s**
//! (1579s -> 31.6s user), i.e. 42x, clearing the owner's <8s bar outright.
//! The bar is reachable; it is reachable in `compile::resolve`, not here.
//!
//! Amdahl therefore caps this entire wave at 124.51s -> ~123.7s. The lock
//! removal is NOT landed: it is unsafe surgery on the interpreter's one
//! identity-critical structure, and its retire list would be strictly worse
//! than `RootGlobals`' (which grows per NEW NAME; a per-cell one grows per
//! WRITE, so `alter-var-root`/`with-redefs`/root `set!` in a loop retains a
//! boxed `Value` each). Priced, refuted, and left in the tree as a probe so
//! the next wave that thinks the cell lock is the wall can re-run it in 8s.
//!
//! ## Load-sensitivity re-verification (2026-08-22, post-merge, owner-requested)
//!
//! The verdict above was measured while other agents were building; the
//! owner asked whether load contaminated it. Re-measured under four
//! controlled conditions -- quiet, 7 spinners, 14 spinners, and a cold
//! `cargo build --release` storm (the session-realistic load) -- with the
//! pre-session binary preserved in the `f3/cell` worktree:
//!
//! * Profile share of `VarCell::get` in the real delays.clj run (leaf
//!   samples, kernel-wait excluded): session-era captures 0.27-0.44%,
//!   QUIET 0.36%/0.63% (two windows), build-storm 0.14%/0.27%. Identical
//!   within window-to-window variance in every condition; the 0.6% figure
//!   was not a load artifact. (`value_to_form` likewise reproduces:
//!   10.2-11.8% loaded vs 11.5-12.8% quiet/storm.)
//! * This probe's cross, quiet vs loaded: E/D(Arc,set=6) 3.00x quiet vs
//!   2.35x session / 2.63x spin7 / 1.90x spin14; E/D(inline,set=6) 21.8x
//!   quiet vs 23.2x session / 15-16x spinning. Structure invariant: the
//!   lock matters enormously for inline payloads, the shared refcount caps
//!   Arc payloads at ~3x, under every load level.
//! * The causal anchor never depended on a profile at all: the loop-hoist
//!   A/B (both sides measured on a quiet machine) moved 124.6s -> 2.75s
//!   without changing VarCell traffic by one read.
//!
//! Post-W-RESOLVE footnote: with the tree-walk wall gone (2.63s file),
//! `VarCell::get` is now ~4.9% of the file's CPU self time -- ten times its
//! old share of a 47x-smaller pie. Top leaves are now `apply_closure_buf`
//! ~11%, `Value` drop+clone ~17.5% (the W-GEO campaign's territory),
//! compiled-tier dispatch, and `RjError::clone` ~3.7%. If a future workload
//! is resolution-bound, this probe is the 8-second re-run that says whether
//! the cell is finally the wall.
//!
//! The same profile refutes the exception-construction hypothesis for this
//! file: `RjError::clone` is 0.006% and `force_delay` 0.001% of self time.

// `Symbol`/`Str` hash and compare purely on immutable text; their
// interior-mutable ASCII/char-count caches never participate, so the lint's
// premise does not apply (same allow as `Env::interned_namespaces`).
#![allow(clippy::mutable_key_type)]

use std::cell::RefCell;
use std::collections::HashMap as StdMap;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, RwLock, RwLockReadGuard};
use std::time::Instant;

use mova::internal::env::{Env, VarCell};
use mova::internal::{champ, imbl};
use mova::internal::{Str, Symbol, Value};

/// The same poisoned-lock policy `crate::sync::lock_read` implements
/// (src/sync.rs:26): recover the guard rather than propagate poisoning.
/// Re-stated here because `sync` is `pub(crate)`.
fn rd<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

/// `champ::PersistentHashMap::assoc` requires `V: PartialEq` (it
/// returns a pointer-identical map when a write changes nothing). A var
/// cell's equality IS its identity -- two cells with the same name are
/// still different vars -- so this compares by `Arc::ptr_eq`, which is also
/// exactly the contract `env.rs`'s module doc states.
#[derive(Clone)]
struct CellRef(Arc<VarCell>);

impl PartialEq for CellRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

type ImMap = imbl::HashMap<Symbol, CellRef>;
type ChampMap = champ::PersistentHashMap<Symbol, CellRef>;

// ---------------------------------------------------------------------------
// The read API every repr is measured through.
// ---------------------------------------------------------------------------

trait Globals: Sync + Send {
    /// The whole real read: candidate -> cell -> value.
    fn get_value(&self, sym: &Symbol) -> Option<Value>;
    /// Map probe only. Returns the cell's ADDRESS, deliberately: cloning
    /// the `Arc` would add a shared-refcount RMW and re-contaminate the
    /// measurement we are trying to isolate.
    fn get_addr(&self, sym: &Symbol) -> Option<usize>;
}

/// `A_env`: the real thing.
struct RealEnv(Env);

impl Globals for RealEnv {
    fn get_value(&self, sym: &Symbol) -> Option<Value> {
        self.0.get_exact(sym)
    }
    /// `Env` exposes no address-only probe; the `map` row for this variant
    /// is reported as n/a and `A_repl` stands in for it.
    fn get_addr(&self, _sym: &Symbol) -> Option<usize> {
        None
    }
}

/// `A_repl`: today's shape, rebuilt locally.
struct LockedStd {
    map: RwLock<StdMap<Symbol, Arc<VarCell>>>,
}

impl Globals for LockedStd {
    fn get_value(&self, sym: &Symbol) -> Option<Value> {
        // Guard held ACROSS the cell read, exactly as `Env::get_exact` ->
        // `read_binding` -> `VarCell::get` does (env.rs:569-582).
        let g = rd(&self.map);
        g.get(sym).and_then(|c| c.get())
    }
    fn get_addr(&self, sym: &Symbol) -> Option<usize> {
        let g = rd(&self.map);
        g.get(sym).map(|c| Arc::as_ptr(c) as usize)
    }
}

/// `B_snap`: `RwLock<Arc<snapshot>>`, readers clone the `Arc` out and read
/// the map with no lock held.
struct SnapLocked {
    snap: RwLock<Arc<ImMap>>,
}

impl SnapLocked {
    #[inline]
    fn pin(&self) -> Arc<ImMap> {
        rd(&self.snap).clone()
    }
}

impl Globals for SnapLocked {
    fn get_value(&self, sym: &Symbol) -> Option<Value> {
        let m = self.pin();
        m.get(sym).and_then(|c| c.0.get())
    }
    fn get_addr(&self, sym: &Symbol) -> Option<usize> {
        let m = self.pin();
        m.get(sym).map(|c| Arc::as_ptr(&c.0) as usize)
    }
}

// ---------------------------------------------------------------------------
// `C_tls` / `C_champ`: the design this probe exists to price.
//
// ORDERING ARGUMENT (the whole safety case, stated once):
//
//   Reader:  g = gen.load(Acquire); if cached_gen == g -> read cached map.
//            else take the read lock, clone the Arc, cache it under g.
//   Writer:  take the write lock, publish the new Arc into `snap`,
//            gen.fetch_add(1, Release) (still under the lock).
//
// * Staleness is bounded by happens-before, not by luck. If thread A
//   interns a symbol and then synchronizes with thread B by ANY means
//   (channel, join, lock -- and an unsynchronized `def` has no defined
//   visibility today either, `Env::get` already documents that race), then
//   A's `Release` bump happens-before B's `Acquire` load, so B reads a
//   generation at least as new as A's, misses its cache, and reloads. B
//   therefore cannot fail to see a `def` it is causally after.
// * Reading `gen` BEFORE the snapshot is the conservative direction: a
//   writer landing in between makes the reader cache a NEWER map under an
//   OLDER generation number, so the next probe merely reloads once more.
//   The reverse (caching an older map under a newer number, which WOULD be
//   stale forever) cannot happen, because the number is read first.
// * The hit path performs no shared read-modify-write whatsoever: one
//   `Acquire` load leaves the counter's cacheline Shared in every core's
//   cache, so it scales flat. That is the entire hypothesis under test.
// * `id` distinguishes root envs: mova supports several `Engine`s (and so
//   several root `Env`s) in one process, and the cache is per-THREAD, so
//   it must not answer one root's probe out of another's snapshot.
// ---------------------------------------------------------------------------

static NEXT_ROOT_ID: AtomicU64 = AtomicU64::new(1);

struct TlsSnap<M> {
    id: u64,
    gen: AtomicU64,
    snap: RwLock<Arc<M>>,
}

thread_local! {
    /// `(root id, generation, snapshot)`, newest-used first. A `Vec` and a
    /// linear scan, not a `HashMap`: the length is the number of live root
    /// envs this thread has touched, which is 1 in every real program and
    /// is bounded below by hand anyway.
    static IM_CACHE: RefCell<Vec<(u64, u64, Arc<ImMap>)>> = const { RefCell::new(Vec::new()) };
    static CHAMP_CACHE: RefCell<Vec<(u64, u64, Arc<ChampMap>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Shared body of both `C_*` readers: `cache` is the caller's thread-local
/// slot vector. Held `borrow_mut` across `f` deliberately -- a globals map
/// probe is a pure lookup and can never re-enter this function (it does not
/// evaluate anything), and cloning the `Arc` out to release the borrow
/// would put back exactly the shared refcount RMW this design removes.
macro_rules! tls_read {
    ($self:expr, $cache:expr, $f:expr) => {{
        let gen = $self.gen.load(Ordering::Acquire);
        $cache.with(|c| {
            let mut c = c.borrow_mut();
            match c.iter().position(|(id, _, _)| *id == $self.id) {
                Some(i) if c[i].1 == gen => $f(&*c[i].2),
                Some(i) => {
                    c[i] = ($self.id, gen, rd(&$self.snap).clone());
                    $f(&*c[i].2)
                }
                None => {
                    // Bound the vector: a dropped root env's entry would
                    // otherwise pin its snapshot on this thread forever.
                    if c.len() >= 4 {
                        c.clear();
                    }
                    c.push(($self.id, gen, rd(&$self.snap).clone()));
                    let i = c.len() - 1;
                    $f(&*c[i].2)
                }
            }
        })
    }};
}

impl Globals for TlsSnap<ImMap> {
    fn get_value(&self, sym: &Symbol) -> Option<Value> {
        tls_read!(self, IM_CACHE, |m: &ImMap| m
            .get(sym)
            .and_then(|c| c.0.get()))
    }
    fn get_addr(&self, sym: &Symbol) -> Option<usize> {
        tls_read!(self, IM_CACHE, |m: &ImMap| m
            .get(sym)
            .map(|c| Arc::as_ptr(&c.0) as usize))
    }
}

impl Globals for TlsSnap<ChampMap> {
    fn get_value(&self, sym: &Symbol) -> Option<Value> {
        tls_read!(self, CHAMP_CACHE, |m: &ChampMap| m
            .get(sym)
            .and_then(|c| c.0.get()))
    }
    fn get_addr(&self, sym: &Symbol) -> Option<usize> {
        tls_read!(self, CHAMP_CACHE, |m: &ChampMap| m
            .get(sym)
            .map(|c| Arc::as_ptr(&c.0) as usize))
    }
}

// ---------------------------------------------------------------------------
// `C2_lean`: the same generation-gated cache with the bookkeeping stripped
// to the bone -- ONE slot instead of a scanned `Vec` (a thread that has
// touched two root envs simply reloads on alternation, which no real
// program does), so the hit path is a TLS access, one `Cell` compare and a
// borrow-flag check.
// ---------------------------------------------------------------------------

thread_local! {
    static LEAN_SLOT: RefCell<Option<(u64, u64, Arc<ChampMap>)>> = const { RefCell::new(None) };
}

struct LeanTls {
    id: u64,
    gen: AtomicU64,
    snap: RwLock<Arc<ChampMap>>,
}

impl LeanTls {
    #[inline]
    fn with<R>(&self, f: impl FnOnce(&ChampMap) -> R) -> R {
        let gen = self.gen.load(Ordering::Acquire);
        LEAN_SLOT.with(|s| {
            let mut s = s.borrow_mut();
            match &*s {
                Some((id, g, m)) if *id == self.id && *g == gen => return f(m),
                _ => {}
            }
            let fresh = rd(&self.snap).clone();
            *s = Some((self.id, gen, fresh));
            let m = &s.as_ref().expect("just stored").2;
            f(m)
        })
    }
}

impl Globals for LeanTls {
    fn get_value(&self, sym: &Symbol) -> Option<Value> {
        self.with(|m| m.get(sym).and_then(|c| c.0.get()))
    }
    fn get_addr(&self, sym: &Symbol) -> Option<usize> {
        self.with(|m| m.get(sym).map(|c| Arc::as_ptr(&c.0) as usize))
    }
}

// ---------------------------------------------------------------------------
// `D_atomic`: no thread-local at all. The published snapshot is a raw
// pointer; readers do ONE `Acquire` load and dereference it.
//
// ## Ordering argument (the `ProtoIc`/`host_struct::install` idiom, ported)
//
// 1. A writer builds the ENTIRE next snapshot off to the side, boxes it,
//    and installs it with a single `Release` swap. A reader's `Acquire`
//    load therefore observes either the old map or a fully-constructed new
//    one -- there is no window in which the pointer is visible but the
//    nodes it names are not, because publishing the pointer IS publishing
//    the map (same "the pin is part of the published value" property
//    `ProtoIcEntry` documents).
// 2. Writers are serialized by a mutex, so read-modify-write of the map
//    (clone the current snapshot, add one key, publish) cannot lose an
//    update. Writes are RARE by construction: `Env::intern`'s slow path
//    fires only for a genuinely NEW name, and `bind_alias` only at
//    ns-load time -- ordinary `def`/redefinition writes THROUGH the
//    existing `Arc<VarCell>` and never touches the map at all.
// 3. Reclamation: the superseded snapshot is deliberately LEAKED. A reader
//    holds a bare `&Map` with no refcount and no epoch, so nothing can
//    prove when the last one is done -- and the cost of never proving it
//    is bounded by the path-copy of one insert (a handful of CHAMP nodes)
//    per NEW GLOBAL NAME, next to which the `VarCell` that intern also
//    allocates, and which is likewise never reclaimed while the env lives,
//    is the larger permanent allocation. This is the design's one real
//    liability and it is priced, not hidden: see the write-path test.
// ---------------------------------------------------------------------------

struct AtomicSnap {
    ptr: std::sync::atomic::AtomicPtr<ChampMap>,
    writer: std::sync::Mutex<()>,
}

impl AtomicSnap {
    fn new(m: ChampMap) -> Self {
        AtomicSnap {
            ptr: std::sync::atomic::AtomicPtr::new(Box::into_raw(Box::new(m))),
            writer: std::sync::Mutex::new(()),
        }
    }
    #[inline]
    fn map(&self) -> &ChampMap {
        // SAFETY: the pointer is only ever set from `Box::into_raw` of a
        // live map and is never freed (point 3 above), so it is valid for
        // as long as `self` is, which is what the returned lifetime says.
        unsafe { &*self.ptr.load(Ordering::Acquire) }
    }
    fn intern(&self, sym: Symbol, cell: Arc<VarCell>) {
        let _g = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let next = Box::new(self.map().assoc(sym, CellRef(cell)));
        let _leaked = self.ptr.swap(Box::into_raw(next), Ordering::Release);
    }
}

impl Globals for AtomicSnap {
    fn get_value(&self, sym: &Symbol) -> Option<Value> {
        self.map().get(sym).and_then(|c| c.0.get())
    }
    fn get_addr(&self, sym: &Symbol) -> Option<usize> {
        self.map().get(sym).map(|c| Arc::as_ptr(&c.0) as usize)
    }
}

// ---------------------------------------------------------------------------
// `E_cellfree`: W-CELL's candidate. The MAP repr is held fixed at `D_atom`
// (the shipped one) and only the CELL changes: `VarCell`'s
// `RwLock<Option<Value>>` becomes an `AtomicPtr<Value>` over an immutable,
// boxed payload, published by `Release` swap under a writer mutex that also
// owns the retire list -- `RootGlobals`' idiom (env.rs:523-551) applied one
// level down. Null = unbound, which is the same monotone "never unbinds"
// contract `VarCell::bound` already documents.
//
// Measured in BOTH modes and against BOTH payload kinds, because the lock
// is only HALF the shared traffic on this path:
//
// * `value` mode returns an owned `Value`, so an `Arc`-backed payload
//   (`Str`/`Native`/`Fn` -- what a real global holds) pays a refcount
//   increment on clone and a decrement on drop, both on the SAME heap
//   allocation for all 100 threads. That is a shared RMW the lock removal
//   does not touch.
// * `addr` mode returns the payload's address, so it isolates the map probe
//   plus the cell load with no refcount at all.
// * The INLINE payload (`Value::Int`) is the control that separates the two:
//   its clone is a register move, so `E_cellfree` value-vs-addr collapses to
//   zero and any remaining gap against `D_atom` value IS the per-cell
//   `RwLock`, priced alone.
// ---------------------------------------------------------------------------

struct FreeCell {
    /// Null = unbound. Non-null = a `Box::into_raw`'d, never-mutated
    /// `Value`; a store publishes a NEW box and retires the old one.
    published: std::sync::atomic::AtomicPtr<Value>,
    /// Serializes publishes AND owns the retired payloads, one lock, exactly
    /// as `RootGlobals::writer` does for the root map.
    #[allow(clippy::vec_box)] // the Box IS the allocation a reader is inside
    writer: std::sync::Mutex<Vec<Box<Value>>>,
}

impl FreeCell {
    fn bound(v: Value) -> Self {
        FreeCell {
            published: std::sync::atomic::AtomicPtr::new(Box::into_raw(Box::new(v))),
            writer: std::sync::Mutex::new(Vec::new()),
        }
    }
    #[inline]
    fn get(&self) -> Option<Value> {
        let p = self.published.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        // SAFETY: non-null implies a live `Box::into_raw` payload; a
        // superseded one is retired, not freed, until `Drop`.
        Some(unsafe { &*p }.clone())
    }
    #[inline]
    fn addr(&self) -> Option<usize> {
        let p = self.published.load(Ordering::Acquire);
        if p.is_null() {
            None
        } else {
            Some(p as usize)
        }
    }
    #[allow(dead_code)] // exercised by the write-path half only
    fn store(&self, v: Value) {
        let mut retired = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let old = self
            .published
            .swap(Box::into_raw(Box::new(v)), Ordering::Release);
        if !old.is_null() {
            retired.push(unsafe { Box::from_raw(old) });
        }
    }
}

impl Drop for FreeCell {
    fn drop(&mut self) {
        let p = *self.published.get_mut();
        if !p.is_null() {
            drop(unsafe { Box::from_raw(p) });
        }
        self.writer
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// Identity equality, same reason as [`CellRef`].
#[derive(Clone)]
struct FreeRef(Arc<FreeCell>);

impl PartialEq for FreeRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

type FreeMap = champ::PersistentHashMap<Symbol, FreeRef>;

/// `D_atom`'s map repr verbatim, holding `FreeCell`s instead of `VarCell`s
/// so the ONLY difference between the `D_*` and `E_*` rows is the cell.
struct FreeSnap {
    ptr: std::sync::atomic::AtomicPtr<FreeMap>,
}

impl FreeSnap {
    fn new(m: FreeMap) -> Self {
        FreeSnap {
            ptr: std::sync::atomic::AtomicPtr::new(Box::into_raw(Box::new(m))),
        }
    }
    #[inline]
    fn map(&self) -> &FreeMap {
        // SAFETY: as `AtomicSnap::map` -- set once from `Box::into_raw`, and
        // this probe never publishes over it.
        unsafe { &*self.ptr.load(Ordering::Acquire) }
    }
}

impl Drop for FreeSnap {
    fn drop(&mut self) {
        let p = *self.ptr.get_mut();
        if !p.is_null() {
            drop(unsafe { Box::from_raw(p) });
        }
    }
}

impl Globals for FreeSnap {
    fn get_value(&self, sym: &Symbol) -> Option<Value> {
        self.map().get(sym).and_then(|c| c.0.get())
    }
    fn get_addr(&self, sym: &Symbol) -> Option<usize> {
        self.map().get(sym).and_then(|c| c.0.addr())
    }
}

// ---------------------------------------------------------------------------
// Fixture: a realistic bootstrap-sized globals table.
// ---------------------------------------------------------------------------

/// mova boots roughly this many global names (hundreds of Rust builtins
/// plus every `core/*.mova` def) before user code runs.
const N_SYMS: usize = 800;

struct Fixture {
    /// Pre-built candidate spellings per symbol, in
    /// `for_each_global_candidate` order: 1-2 misses then the bare hit.
    /// Pre-built because the real path CLONES `Str` handles into the
    /// candidate (`ns.rs:373-380`) rather than allocating -- keeping symbol
    /// construction out of the timed region isolates lock + hash + probe,
    /// which is what varies between the reprs.
    cands: Vec<Vec<Symbol>>,
    cells: Vec<(Symbol, Arc<VarCell>)>,
}

fn fixture() -> (Fixture, Env) {
    fixture_n(N_SYMS)
}

/// `n` distinct globals. `fixture()` uses a bootstrap-sized table; the
/// hot-set probe uses a handful, which is the shape `delays.clj` actually
/// has (100 threads all resolving the SAME `is`/`=`/`deref` cells).
fn fixture_n(n: usize) -> (Fixture, Env) {
    fixture_n_payload(n, true)
}

/// `arc_backed = false` gives every cell a `Value::Int` instead: the SAME
/// map, the same locks, the same probes, but a clone that touches no shared
/// refcount. W-CELL's probe uses it as the control that separates the
/// per-cell lock from the payload's refcount (see `E_cellfree` above).
fn fixture_n_payload(n: usize, arc_backed: bool) -> (Fixture, Env) {
    let env = Env::new_root();
    let mut cells = Vec::with_capacity(n);
    let mut cands = Vec::with_capacity(n);
    for i in 0..n {
        let sym = Symbol::simple(format!("g{i}"));
        // An `Arc`-backed value, like the `Value::Native`/`Value::Fn` a
        // real global holds: reading it clones the `Arc`, which is one more
        // shared-refcount RMW on the SAME few cells every thread hits.
        let payload = if arc_backed {
            Value::Str(Str::from(format!("body-of-g{i}")))
        } else {
            Value::Int(i as i64)
        };
        env.set(sym.clone(), payload);
        let cell = env.intern(&sym);
        let mut c = vec![Symbol {
            ns: Some(Str::from("user")),
            name: sym.name.clone(),
        }];
        if i % 4 == 0 {
            // The third-candidate case (alias-expanded spelling, or a
            // `:rename`d refer source) -- a minority of lookups.
            c.push(Symbol {
                ns: Some(Str::from("clojure.string")),
                name: sym.name.clone(),
            });
        }
        c.push(sym.clone());
        cands.push(c);
        cells.push((sym, cell));
    }
    (Fixture { cands, cells }, env)
}

fn build_std(f: &Fixture) -> LockedStd {
    LockedStd {
        map: RwLock::new(f.cells.iter().cloned().collect()),
    }
}

fn build_im(f: &Fixture) -> ImMap {
    let mut m = ImMap::new();
    for (s, c) in &f.cells {
        m.insert(s.clone(), CellRef(c.clone()));
    }
    m
}

fn build_champ(f: &Fixture) -> ChampMap {
    let mut m = ChampMap::new();
    for (s, c) in &f.cells {
        m = m.assoc(s.clone(), CellRef(c.clone()));
    }
    m
}

/// The `E_cellfree` fixture: the same symbols, the same map repr, and --
/// crucially -- the SAME payload `Value`s cloned out of the real cells, so an
/// `Arc`-backed payload's refcount is shared between the `D_*` and `E_*` rows
/// exactly as one var's value is shared between all its readers.
fn build_free(f: &Fixture) -> FreeSnap {
    let mut m = FreeMap::new();
    for (s, c) in &f.cells {
        let v = c.get().expect("fixture cells are bound");
        m = m.assoc(s.clone(), FreeRef(Arc::new(FreeCell::bound(v))));
    }
    FreeSnap::new(m)
}

fn new_tls<M>(m: M) -> TlsSnap<M> {
    TlsSnap {
        id: NEXT_ROOT_ID.fetch_add(1, Ordering::Relaxed),
        gen: AtomicU64::new(0),
        snap: RwLock::new(Arc::new(m)),
    }
}

// ---------------------------------------------------------------------------
// Driver.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Value,
    Addr,
}

/// One thread's share of the work: `iters` symbol RESOLUTIONS (each = the
/// 1-3 candidate probes the real resolver would issue, stopping at the
/// first hit), over a fixed pseudo-random symbol sequence derived from the
/// thread's seed by the same LCG the rest of this repo's probes use.
fn run_lookups<G: Globals + ?Sized>(g: &G, f: &Fixture, iters: u64, mode: Mode, seed: u64) -> u64 {
    let mut x = seed | 1;
    let mut hits = 0u64;
    for _ in 0..iters {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let idx = (x >> 33) as usize % f.cands.len();
        for cand in &f.cands[idx] {
            match mode {
                Mode::Value => {
                    if let Some(v) = g.get_value(black_box(cand)) {
                        black_box(&v);
                        hits += 1;
                        break;
                    }
                }
                Mode::Addr => {
                    if let Some(a) = g.get_addr(black_box(cand)) {
                        hits += black_box(a) as u64 & 1;
                        break;
                    }
                }
            }
        }
    }
    hits
}

/// Aggregate resolutions/sec across `threads` barrier-started readers.
fn measure<G: Globals + 'static>(
    g: &Arc<G>,
    f: &Arc<Fixture>,
    threads: usize,
    iters: u64,
    mode: Mode,
) -> (f64, f64) {
    let barrier = Arc::new(Barrier::new(threads + 1));
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let g = Arc::clone(g);
            let f = Arc::clone(f);
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                // Touch the thread-local cache before the barrier so the
                // first-probe cold miss is not inside the timed region.
                run_lookups(&*g, &f, 32, mode, 0x1234 + t as u64);
                b.wait();
                run_lookups(&*g, &f, iters, mode, 0x9E37_79B9 + t as u64 * 1013)
            })
        })
        .collect();
    barrier.wait();
    let t0 = Instant::now();
    let mut hits = 0u64;
    for h in handles {
        hits += h.join().expect("reader thread panicked");
    }
    let dt = t0.elapsed().as_secs_f64();
    black_box(hits);
    ((threads as u64 * iters) as f64 / dt, dt)
}

const TARGET_SECS: f64 = 0.06;
/// Rounds per configuration. Higher than the usual 5 in this repo's probes
/// because this machine is NOT quiet (concurrent agent builds), and the
/// statistic that survives that is the MAX -- the round least disturbed by
/// a neighbour -- not the median. Both are reported.
const ROUNDS: usize = 9;

/// Iterations that make one round span ~`TARGET_SECS`. The reprs differ by
/// two orders of magnitude, so a single fixed count cannot serve them all.
fn calibrate<G: Globals + 'static>(
    g: &Arc<G>,
    f: &Arc<Fixture>,
    threads: usize,
    mode: Mode,
) -> u64 {
    let (_, dt) = measure(g, f, threads, 2_000, mode);
    ((2_000.0 * TARGET_SECS / dt.max(1e-6)) as u64).clamp(2_000, 40_000_000)
}

/// `(median, max)` of a sample vector.
fn stats(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN rates"));
    (v[v.len() / 2], v[v.len() - 1])
}

/// Best-of-5 aggregate rate for one configuration (the secondary tests,
/// which compare one repr against itself and so do not need the main
/// probe's round-level interleaving).
fn bench<G: Globals + 'static>(g: &Arc<G>, f: &Arc<Fixture>, threads: usize, mode: Mode) -> f64 {
    let it = calibrate(g, f, threads, mode);
    stats((0..5).map(|_| measure(g, f, threads, it, mode).0).collect()).1
}

fn readers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1))
        .unwrap_or(3)
        .max(2)
}

fn mrate(r: f64) -> String {
    format!("{:>9.2}M/s", r / 1e6)
}

/// The one part of this file that is a TEST and not a measurement: every
/// candidate repr must answer exactly what the real `Env` answers, or the
/// numbers the measurements print are comparing different work. Cheap, so
/// it runs in the ordinary `cargo test` gate while the four benchmarks
/// below are `#[ignore]`d.
#[test]
fn candidate_reprs_agree_with_real_env() {
    let (f, env) = fixture();
    let f = Arc::new(f);
    let a_env = RealEnv(env);
    let a_repl = build_std(&f);
    let b_snap = SnapLocked {
        snap: RwLock::new(Arc::new(build_im(&f))),
    };
    let c_tls = new_tls(build_im(&f));
    let c_champ = new_tls(build_champ(&f));
    let c2_lean = LeanTls {
        id: NEXT_ROOT_ID.fetch_add(1, Ordering::Relaxed),
        gen: AtomicU64::new(0),
        snap: RwLock::new(Arc::new(build_champ(&f))),
    };
    let d_atomic = AtomicSnap::new(build_champ(&f));
    let e_free = build_free(&f);
    for cands in f.cands.iter() {
        for cand in cands {
            let want = a_env.get_value(cand);
            assert_eq!(want, e_free.get_value(cand), "E_cellfree must answer the same VALUE, not merely the same hit/miss -- it is the only variant whose cells are separate objects from the real env's");
            assert_eq!(want.is_some(), e_free.get_addr(cand).is_some());
            assert_eq!(want.is_some(), a_repl.get_value(cand).is_some());
            assert_eq!(want.is_some(), b_snap.get_value(cand).is_some());
            assert_eq!(want.is_some(), c_tls.get_value(cand).is_some());
            assert_eq!(want.is_some(), c_champ.get_value(cand).is_some());
            assert_eq!(want.is_some(), c2_lean.get_value(cand).is_some());
            assert_eq!(want.is_some(), d_atomic.get_value(cand).is_some());
            assert_eq!(a_repl.get_addr(cand), c_tls.get_addr(cand));
            assert_eq!(a_repl.get_addr(cand), c_champ.get_addr(cand));
            assert_eq!(a_repl.get_addr(cand), b_snap.get_addr(cand));
            assert_eq!(a_repl.get_addr(cand), c2_lean.get_addr(cand));
            assert_eq!(a_repl.get_addr(cand), d_atomic.get_addr(cand));
        }
    }
}

/// `#[ignore]`: a MEASUREMENT, not a gate. It takes ~40s of a mostly-idle
/// machine and spawns `available_parallelism()-1` threads, so running it
/// alongside the rest of `cargo test` would both slow the gate down and
/// measure a machine the rest of the gate is busy loading. Run it with
/// `cargo test --release --test globals_snapshot_bench -- --ignored
/// --nocapture --test-threads=1`.
#[test]
#[ignore = "measurement, not a gate -- run with --ignored"]
fn globals_read_path_snapshot_kill_probe() {
    let (f, env) = fixture();
    let f = Arc::new(f);

    let a_env = Arc::new(RealEnv(env));
    let a_repl = Arc::new(build_std(&f));
    let b_snap = Arc::new(SnapLocked {
        snap: RwLock::new(Arc::new(build_im(&f))),
    });
    let c_tls = Arc::new(new_tls(build_im(&f)));
    let c_champ = Arc::new(new_tls(build_champ(&f)));
    let c2_lean = Arc::new(LeanTls {
        id: NEXT_ROOT_ID.fetch_add(1, Ordering::Relaxed),
        gen: AtomicU64::new(0),
        snap: RwLock::new(Arc::new(build_champ(&f))),
    });
    let d_atomic = Arc::new(AtomicSnap::new(build_champ(&f)));

    let n = readers();
    println!("\n=== W-ENV kill-probe: globals read path ===");
    println!(
        "  {N_SYMS} globals, 1-3 candidate probes per resolution, \
         {ROUNDS} interleaved rounds, {n} contended readers"
    );

    // Interleaved A/B: all variants measured inside the same round, so a
    // load spike from a neighbouring process hits every variant alike.
    let names = [
        "A_env  (real Env::get_exact)",
        "A_repl (RwLock<std::HashMap>)",
        "B_snap (RwLock<Arc<imbl>>)",
        "C_tls  (TLS snapshot, imbl)",
        "C_champ(TLS snapshot, champ)",
        "C2_lean(TLS 1-slot, champ)",
        "D_atom (AtomicPtr+leak, champ)",
    ];
    const V: usize = 7;
    // samples[threads][mode][variant] = one rate per round.
    let mut samples: Vec<Vec<Vec<Vec<f64>>>> = vec![vec![vec![Vec::new(); V]; 2]; 2];
    let mut iters = [[[0u64; V]; 2]; 2];
    const UNCONT: usize = 0;
    const CONT: usize = 1;
    const VAL: usize = 0;
    const ADDR: usize = 1;

    // Expands its argument macro once per variant, so calibration and every
    // round hit all seven reprs through the SAME monomorphized `measure`
    // (no trait object: a virtual call per probe would be ~8% of the
    // uncontended budget and would land on the fast variants hardest).
    macro_rules! each_variant {
        ($m:ident) => {
            $m!(0, a_env);
            $m!(1, a_repl);
            $m!(2, b_snap);
            $m!(3, c_tls);
            $m!(4, c_champ);
            $m!(5, c2_lean);
            $m!(6, d_atomic);
        };
    }

    macro_rules! calib {
        ($i:expr, $g:ident) => {
            iters[UNCONT][VAL][$i] = calibrate(&$g, &f, 1, Mode::Value);
            iters[CONT][VAL][$i] = calibrate(&$g, &f, n, Mode::Value);
            if $i != 0 {
                iters[UNCONT][ADDR][$i] = calibrate(&$g, &f, 1, Mode::Addr);
                iters[CONT][ADDR][$i] = calibrate(&$g, &f, n, Mode::Addr);
            }
        };
    }
    each_variant!(calib);

    for _ in 0..ROUNDS {
        macro_rules! one_round {
            ($i:expr, $g:ident) => {
                samples[UNCONT][VAL][$i]
                    .push(measure(&$g, &f, 1, iters[UNCONT][VAL][$i], Mode::Value).0);
                samples[CONT][VAL][$i]
                    .push(measure(&$g, &f, n, iters[CONT][VAL][$i], Mode::Value).0);
                if $i != 0 {
                    samples[UNCONT][ADDR][$i]
                        .push(measure(&$g, &f, 1, iters[UNCONT][ADDR][$i], Mode::Addr).0);
                    samples[CONT][ADDR][$i]
                        .push(measure(&$g, &f, n, iters[CONT][ADDR][$i], Mode::Addr).0);
                }
            };
        }
        each_variant!(one_round);
    }

    // `med` for the typical-on-a-loaded-machine number, `max` for the
    // least-disturbed round -- the verdict is taken on `max`, which is the
    // closest this machine can get to the quiet-machine capability the
    // owner's bar is written against.
    let mut med = [[[0.0f64; V]; 2]; 2];
    let mut best = [[[0.0f64; V]; 2]; 2];
    for t in [UNCONT, CONT] {
        for m in [VAL, ADDR] {
            for i in 0..V {
                if samples[t][m][i].is_empty() {
                    continue;
                }
                let (a, b) = stats(samples[t][m][i].clone());
                med[t][m][i] = a;
                best[t][m][i] = b;
            }
        }
    }

    let table = |mode: usize, from: usize, base: usize, title: &str, vs: &str| {
        println!("\n  MODE={title}");
        println!(
            "  {:<30} {:>22} {:>22} {:>8} {:>9}",
            "variant",
            "1 thread  med / max",
            format!("{n} threads  med / max"),
            "scale",
            vs
        );
        for i in from..V {
            println!(
                "  {:<30} {} /{} {} /{} {:>7.2}x {:>8.2}x",
                names[i],
                mrate(med[UNCONT][mode][i]),
                mrate(best[UNCONT][mode][i]),
                mrate(med[CONT][mode][i]),
                mrate(best[CONT][mode][i]),
                best[CONT][mode][i] / best[UNCONT][mode][i],
                best[CONT][mode][i] / best[CONT][mode][base]
            );
        }
    };
    table(
        VAL,
        0,
        0,
        "value (map probe + VarCell::get -- the whole real read)",
        "vs A_env",
    );
    table(
        ADDR,
        1,
        1,
        "map (map probe only, no cell read -- isolates the map lock)",
        "vs A_repl",
    );

    // The shipped candidate is the lock-free repr with the best
    // UNCONTENDED number -- every candidate clears the contended bar by an
    // order of magnitude, so single-thread cost is what decides between
    // them (most mova scripts never spawn a thread at all).
    let pick_i = (3..V)
        .max_by(|a, b| {
            best[UNCONT][VAL][*a]
                .partial_cmp(&best[UNCONT][VAL][*b])
                .expect("no NaN rates")
        })
        .expect("nonempty");
    let (pick, pu, pc, pua, pca) = (
        names[pick_i],
        best[UNCONT][VAL][pick_i],
        best[CONT][VAL][pick_i],
        best[UNCONT][ADDR][pick_i],
        best[CONT][ADDR][pick_i],
    );
    let uncont = best[UNCONT][VAL];
    let cont = best[CONT][VAL];
    let cont_addr = best[CONT][ADDR];
    // BASELINE = `A_repl`, not `A_env`. Before the W-ENV landing those two
    // were the same shape and `A_env` was the honest baseline; now that the
    // repr has LANDED, `A_env` IS the shipped design and comparing against
    // it would compare the winner with itself. `A_repl` is the pre-W-ENV
    // `RwLock<std::HashMap>` shape, preserved in this file precisely so the
    // measurement stays reproducible after the change went in -- and it is
    // the fairer uncontended baseline too, since like `D_atom` it lives in
    // this test crate and is inlinable, while `A_env` pays a real
    // cross-crate call.
    const BASE: usize = 1;
    let cont_win = pc / cont[BASE];
    let uncont_delta = pu / uncont[BASE] - 1.0;
    let map_only_win = pca / cont_addr[BASE];

    println!("\n  --- VERDICT (baseline = A_repl, the pre-W-ENV shape) ---");
    println!("  candidate to ship: {pick}");
    println!(
        "  A_env row above is the LANDED design measured through the real \
         `Env` (post-W-ENV),\n  so it confirms the landing rather than \
         serving as the baseline."
    );
    println!(
        "  contended (value): {} vs A_repl {} = {:.2}x   [BAR: >= 3.00x]",
        mrate(pc),
        mrate(cont[BASE]),
        cont_win
    );
    println!(
        "  uncontended (value): {} vs A_repl {} = {:+.1}%   [BAR: >= -10.0%]",
        mrate(pu),
        mrate(uncont[BASE]),
        uncont_delta * 100.0
    );
    println!(
        "  map-lock alone (mode=map, contended): {:.2}x over A_repl {} -- \
         the ceiling this wave can reach;\n  the value-mode number is lower by \
         exactly the per-cell VarCell RwLock, which this wave does NOT own \
         (uncontended map-only {})",
        map_only_win,
        mrate(cont_addr[1]),
        mrate(pua)
    );
    if cont_win >= 3.0 && uncont_delta >= -0.10 {
        println!("*** W-ENV KILL-PROBE: DECISIVE. Land the snapshot repr. ***");
    } else if cont_win >= 3.0 {
        println!("*** W-ENV KILL-PROBE: NOT DECISIVE -- contended bar cleared but single-thread regressed. ***");
    } else {
        println!("*** W-ENV KILL-PROBE: NOT DECISIVE -- contended bar MISSED. Do not land. ***");
    }
    println!(
        "  NOTE: machine load is not controlled by this probe; the conductor \
         re-runs on a quiet machine."
    );

    // Reported, never asserted: this is a measurement, and a probe that
    // fails its bar is a RESULT, not a broken test. The suite gate that
    // must stay green is `lane_hand_wire_bench`, not this file.
    assert!(cont[BASE] > 0.0 && pc > 0.0, "probe produced no measurement");
}

/// The UNCONTENDED half of the bar, measured in a way that survives a busy
/// machine: no threads at all, and the variants alternate in SHORT bursts
/// (~20k resolutions, sub-millisecond) so competing load lands on all of
/// them within microseconds of each other instead of on whichever variant
/// happened to be running during a spike. The reported number is the BEST
/// burst per variant -- the one that got a clean slice of a core -- which
/// is the closest a loaded machine can come to a quiet-machine reading.
#[test]
#[ignore = "measurement, not a gate -- run with --ignored"]
fn single_thread_duel_vs_real_env() {
    let (f, env) = fixture();
    let f = Arc::new(f);
    let a_env = RealEnv(env);
    let a_repl = build_std(&f);
    let c_tls = new_tls(build_im(&f));
    let c_champ = new_tls(build_champ(&f));
    let c2_lean = LeanTls {
        id: NEXT_ROOT_ID.fetch_add(1, Ordering::Relaxed),
        gen: AtomicU64::new(0),
        snap: RwLock::new(Arc::new(build_champ(&f))),
    };
    let d_atomic = AtomicSnap::new(build_champ(&f));

    const BURST: u64 = 20_000;
    const BURSTS: usize = 400;
    let names = [
        "A_env  (real Env::get_exact)",
        "A_repl (RwLock<std::HashMap>)",
        "C_tls  (TLS snapshot, imbl)",
        "C_champ(TLS snapshot, champ)",
        "C2_lean(TLS 1-slot, champ)",
        "D_atom (AtomicPtr+leak, champ)",
    ];
    let mut best = [0.0f64; 6];
    let burst = |i: usize, g: &dyn Globals, seed: u64, best: &mut [f64; 6]| {
        // The `dyn` call is on the BURST, not on the probe -- `run_lookups`
        // is generic over `?Sized`, so the per-probe call inside the burst
        // is still a direct one through the vtable-resolved receiver.
        let t0 = Instant::now();
        let h = run_lookups(g, &f, BURST, Mode::Value, seed);
        let dt = t0.elapsed().as_secs_f64();
        black_box(h);
        let r = BURST as f64 / dt;
        if r > best[i] {
            best[i] = r;
        }
    };
    for b in 0..BURSTS {
        let s = 0x51ED_2701 + b as u64 * 7919;
        burst(0, &a_env, s, &mut best);
        burst(1, &a_repl, s, &mut best);
        burst(2, &c_tls, s, &mut best);
        burst(3, &c_champ, s, &mut best);
        burst(4, &c2_lean, s, &mut best);
        burst(5, &d_atomic, s, &mut best);
    }

    println!("\n=== W-ENV single-thread duel (best of {BURSTS} interleaved bursts) ===");
    for i in 0..6 {
        println!(
            "  {:<30} {}   {:+6.1}% vs A_repl",
            names[i],
            mrate(best[i]),
            (best[i] / best[1] - 1.0) * 100.0
        );
    }
    println!("  BAR: the repr to ship must be >= -10.0% here.");
    assert!(best[0] > 0.0);
}

/// W-ENV follow-up probe: where is the wall AFTER the map lock is gone?
///
/// `delays.clj` is not the bootstrap-sized shape the main probe measures.
/// Its 100 threads resolve the SAME handful of names over and over (`is`,
/// `=`, `deref`, `d`), so every hit lands on ONE of a few `VarCell`s -- and
/// `VarCell::get` takes that cell's own `RwLock<Option<Value>>` and then
/// CLONES the `Value` out, which for an `Arc`-backed value is a second RMW
/// on one shared refcount. The main probe's 800 cells spread both costs
/// over 800 cachelines and hid them.
///
/// This run holds the map repr fixed and varies only the working-set size,
/// so the value-vs-map gap IS the per-cell cost. It measures something this
/// wave does not own (`VarCell`, not `Env`), which is exactly why it is
/// worth reporting: it tells the next wave whether there is anything left
/// to win.
#[test]
#[ignore = "measurement, not a gate -- run with --ignored"]
fn hot_set_contention_after_the_map_lock_is_gone() {
    let n = readers();
    println!("\n=== W-ENV hot-set probe: {n} threads, working set 6 vs 64 vs 800 ===");
    println!("  (map repr held fixed; value-vs-map gap = the per-cell VarCell cost)");
    for size in [6usize, 64, 800] {
        let (f, env) = fixture_n(size);
        let f = Arc::new(f);
        let a_env = Arc::new(RealEnv(env));
        let a_repl = Arc::new(build_std(&f));
        let d = Arc::new(AtomicSnap::new(build_champ(&f)));
        let old = bench(&a_repl, &f, n, Mode::Value);
        let new_v = bench(&a_env, &f, n, Mode::Value);
        let d_v = bench(&d, &f, n, Mode::Value);
        let d_m = bench(&d, &f, n, Mode::Addr);
        println!(
            "  set={size:<4} old-shape(RwLock,value) {}  NEW Env::get_exact {}  \
             D_atom value {}  D_atom map-only {}  (cell cost = {:.1}x)",
            mrate(old),
            mrate(new_v),
            mrate(d_v),
            mrate(d_m),
            d_m / d_v
        );
    }
    println!(
        "  Read: if `map-only` stays flat as the set shrinks while `value` collapses,\n  \
         the remaining wall is VarCell's own lock + the Value refcount, NOT the Env map."
    );
}

/// W-CELL kill-probe: is making the per-cell value read LOCK-FREE worth
/// landing, or is the payload's refcount the real wall?
///
/// The hot-set probe above proved the remaining cost is per-CELL, but it
/// could not say which half: `VarCell::get` pays a `RwLock` acquire+release
/// AND an `Arc` refcount increment (plus the caller's decrement on drop),
/// and at a working set of 6 cells all four land on the same two cachelines
/// for all 100 threads. This run separates them by crossing two axes:
///
/// * cell repr -- `D_atom` (today's `RwLock<Option<Value>>`) vs `E_cellfree`
///   (`AtomicPtr` + immutable payload + retire list), the map repr held
///   fixed at the shipped `AtomicPtr` snapshot in both;
/// * payload -- `Arc`-backed (`Str`, what a real global holds) vs inline
///   (`Int`, whose clone touches nothing shared).
///
/// The four value-mode cells of that cross ARE the verdict: inline `D_atom`
/// vs inline `E_cellfree` prices the per-cell lock alone, and `Arc`
/// `E_cellfree` vs inline `E_cellfree` prices the refcount alone. If the
/// second gap dwarfs the first, removing the lock cannot deliver the
/// file-level win on its own, and this probe says so BEFORE the work.
#[test]
#[ignore = "measurement, not a gate -- run with --ignored"]
fn cellfree_kill_probe() {
    let n = readers();
    println!("\n=== W-CELL kill-probe: {n} threads, cell repr x payload kind ===");
    println!("  (map repr held fixed at the shipped AtomicPtr snapshot in every row)");
    for size in [6usize, 64, 800] {
        for arc_backed in [true, false] {
            let kind = if arc_backed { "Arc(Str)" } else { "inline(Int)" };
            let (f, env) = fixture_n_payload(size, arc_backed);
            let f = Arc::new(f);
            let a_env = Arc::new(RealEnv(env));
            let d = Arc::new(AtomicSnap::new(build_champ(&f)));
            let e = Arc::new(build_free(&f));
            let env_v = bench(&a_env, &f, n, Mode::Value);
            let d_v = bench(&d, &f, n, Mode::Value);
            let d_m = bench(&d, &f, n, Mode::Addr);
            let e_v = bench(&e, &f, n, Mode::Value);
            let e_m = bench(&e, &f, n, Mode::Addr);
            println!(
                "  set={size:<4} payload={kind:<12} Env::get_exact {}  D_atom value {}  \
                 E_cellfree value {}  E_cellfree addr {}  D_atom addr {}\n{:>28}\
                 E/D value = {:.2}x   E addr/value = {:.2}x",
                mrate(env_v),
                mrate(d_v),
                mrate(e_v),
                mrate(e_m),
                mrate(d_m),
                "",
                e_v / d_v,
                e_m / e_v
            );
        }
    }
    println!(
        "  BAR: `E/D value` at set=6 with an Arc payload must project delays.clj under 8s\n  \
         (~15.6x on the file). If it is ~1x while the inline row's E/D is large, the lock\n  \
         is not the wall -- the shared refcount is, and no lock removal can reach the bar."
    );
    println!("  VERDICT (2026-08-22, 13 readers): microbench DECISIVE, file-level REFUTED --");
    println!("  see this file's `## W-CELL verdict` module-doc section. Do NOT land.");
    println!("  NOTE: machine load is not controlled by this probe; re-run on a quiet machine.");
}

// ---------------------------------------------------------------------------
// Write-path price. The read design is only viable if the WRITER stays
// cheap, because boot interns ~800 brand-new symbols one at a time and
// mova is a latency-first project (LATENCY-CAMPAIGN.md). A snapshot repr
// over a plain `std::HashMap` would clone the whole table per intern --
// O(n^2) over boot -- which is why both candidates are persistent maps.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "measurement, not a gate -- run with --ignored"]
fn globals_write_path_price() {
    let (f, _env) = fixture();
    let base_std: StdMap<Symbol, Arc<VarCell>> = f.cells.iter().cloned().collect();
    let base_im = build_im(&f);
    let base_champ = build_champ(&f);
    let extra: Vec<Symbol> = (0..N_SYMS)
        .map(|i| Symbol::simple(format!("new{i}")))
        .collect();
    let cell = f.cells[0].1.clone();

    let time = |label: &str, mut op: Box<dyn FnMut()>| {
        op();
        let t0 = Instant::now();
        for _ in 0..5 {
            op();
        }
        let per = t0.elapsed().as_secs_f64() / 5.0 / N_SYMS as f64;
        println!("  {label:<44} {:>9.0} ns/intern", per * 1e9);
        per
    };

    println!("\n=== W-ENV write path: cost of ONE brand-new intern ===");
    println!("  (table already holds {N_SYMS} names; {N_SYMS} fresh interns per round)");
    let mutate = {
        let base = base_std.clone();
        let extra = extra.clone();
        let cell = cell.clone();
        time(
            "in-place insert (today, under write lock)",
            Box::new(move || {
                let mut m = base.clone();
                for s in &extra {
                    m.insert(s.clone(), cell.clone());
                }
                black_box(&m);
            }),
        )
    };
    let clone_world = {
        let base = base_std.clone();
        let extra = extra.clone();
        let cell = cell.clone();
        time(
            "clone-the-world std::HashMap snapshot",
            Box::new(move || {
                let mut m = base.clone();
                for s in &extra {
                    let mut next = m.clone();
                    next.insert(s.clone(), cell.clone());
                    m = next;
                }
                black_box(&m);
            }),
        )
    };
    let im = {
        let base = base_im.clone();
        let extra = extra.clone();
        let cell = cell.clone();
        time(
            "imbl persistent snapshot",
            Box::new(move || {
                let mut m = base.clone();
                for s in &extra {
                    m = m.update(s.clone(), CellRef(cell.clone()));
                }
                black_box(&m);
            }),
        )
    };
    let champ = {
        let base = base_champ.clone();
        let extra = extra.clone();
        let cell = cell.clone();
        time(
            "champ persistent snapshot",
            Box::new(move || {
                let mut m = base.clone();
                for s in &extra {
                    m = m.assoc(s.clone(), CellRef(cell.clone()));
                }
                black_box(&m);
            }),
        )
    };
    println!(
        "  => boot cost of {N_SYMS} interns: today {:.2}ms, clone-world {:.2}ms, \
         imbl {:.2}ms, champ {:.2}ms",
        mutate * N_SYMS as f64 * 1e3,
        clone_world * N_SYMS as f64 * 1e3,
        im * N_SYMS as f64 * 1e3,
        champ * N_SYMS as f64 * 1e3
    );
    assert!(mutate > 0.0);
}

// ---------------------------------------------------------------------------
// Rare-writer sanity: does an intern mid-run (which invalidates EVERY
// thread's cached snapshot at once) cost the readers anything measurable?
// ---------------------------------------------------------------------------

#[test]
#[ignore = "measurement, not a gate -- run with --ignored"]
fn globals_reader_throughput_under_rare_writer() {
    let (f, _env) = fixture();
    let f = Arc::new(f);
    let g = Arc::new(new_tls(build_champ(&f)));
    let n = readers();

    let d = Arc::new(AtomicSnap::new(build_champ(&f)));

    let quiet_tls = bench(&g, &f, n, Mode::Value);
    let quiet_atom = bench(&d, &f, n, Mode::Value);

    let stop = Arc::new(AtomicU64::new(0));
    let w_tls = {
        let g = Arc::clone(&g);
        let stop = Arc::clone(&stop);
        let cell = f.cells[0].1.clone();
        std::thread::spawn(move || {
            let mut i = 0u64;
            while stop.load(Ordering::Relaxed) == 0 {
                let sym = Symbol::simple(format!("w{i}"));
                {
                    let mut w = g.snap.write().unwrap_or_else(|e| e.into_inner());
                    let next = w.assoc(sym, CellRef(cell.clone()));
                    *w = Arc::new(next);
                    // Release, published AFTER the new snapshot is in place
                    // and while the write lock is still held -- see the
                    // ordering argument above.
                    g.gen.fetch_add(1, Ordering::Release);
                }
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            i
        })
    };
    let noisy_tls = bench(&g, &f, n, Mode::Value);
    stop.store(1, Ordering::Relaxed);
    let writes_tls = w_tls.join().expect("writer panicked");

    let stop2 = Arc::new(AtomicU64::new(0));
    let w_atom = {
        let d = Arc::clone(&d);
        let stop = Arc::clone(&stop2);
        let cell = f.cells[0].1.clone();
        std::thread::spawn(move || {
            let mut i = 0u64;
            while stop.load(Ordering::Relaxed) == 0 {
                d.intern(Symbol::simple(format!("w{i}")), cell.clone());
                i += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            i
        })
    };
    let noisy_atom = bench(&d, &f, n, Mode::Value);
    stop2.store(1, Ordering::Relaxed);
    let writes_atom = w_atom.join().expect("writer panicked");

    println!("\n=== W-ENV rare-writer (1 intern/ms) vs {n} readers ===");
    println!(
        "  C_champ quiet {} -> with writer {}  ({:+.1}%, {writes_tls} interns)",
        mrate(quiet_tls),
        mrate(noisy_tls),
        (noisy_tls / quiet_tls - 1.0) * 100.0
    );
    println!(
        "  D_atom  quiet {} -> with writer {}  ({:+.1}%, {writes_atom} interns)",
        mrate(quiet_atom),
        mrate(noisy_atom),
        (noisy_atom / quiet_atom - 1.0) * 100.0
    );
    assert!(noisy_tls > 0.0 && noisy_atom > 0.0);
}
