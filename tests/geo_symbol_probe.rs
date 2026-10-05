//! W-GEO kill-probe F (Symbol representation, stage-3 kill-probe): prices
//! the same three shapes Probe D priced for `Keyword`
//! (`tests/geo_intern_probe.rs`) against `Symbol`'s OWN actual call sites,
//! per `docs/W-GEO-STAGE1-DESIGN.md` §6's explicit demand that stage 3 not
//! assume Probe D's numbers transfer: `Symbol { ns: Option<Str>, name: Str }`
//! (`src/value.rs:948-951`, 32 bytes) is the AST's identifier node
//! (`resolve_symbol`, `src/compile/resolve.rs:493`; macro-expansion/
//! `gensym`), not just an interned data value the way a keyword is.
//!
//! ## The three shapes
//!
//! * `Symbol` (baseline) -- today's real two-field struct, used directly
//!   (`mova::internal::Symbol`), not a replica.
//! * `IdRepr` -- `Interned(u32) | Overflow(Arc<Str>)`, mirroring
//!   `src/keyword.rs`'s landed `Keyword` shape field-for-field (lock-free
//!   `AtomicPtr`-published `champ::PersistentHashMap` + `imbl::Vector`
//!   id table, writer serialized by a `Mutex`, superseded snapshots retired
//!   not freed), keyed on the SAME flat `"ns/name"` / `"name"` text
//!   `Value::Keyword`'s own repr comment already uses (`src/value.rs:2170`).
//!   `Overflow` reconstructs the ns/name split from flat text on demand
//!   (`split_flat`, this file's copy of `builtins::strings::symbol_from_str`'s
//!   split logic -- house convention is copy-not-share across probe files).
//! * `WeakRepr` -- reclaimable, `Mutex<HashMap<u64, Vec<Weak<str>>>>` keyed
//!   by content hash, mirroring Probe D's `WeakTable` exactly, also keyed on
//!   flat text (so its ns/name access pays the same split cost id-table's
//!   `Overflow` arm pays -- apples to apples on workload (c)).
//!
//! ## One correctness nuance this file mirrors from the REAL landed code
//!
//! `Keyword`'s `Hash` impl (`src/keyword.rs`, "Equality / hash / ordering")
//! is content-based UNCONDITIONALLY, on BOTH arms -- the `Interned` id-hash
//! shortcut Probe D measured in isolation (~2.3x) is deliberately NOT taken
//! in the real type, because `Eq` must imply equal `Hash` and an `Interned`/
//! `Overflow` pair of the SAME text (unreachable per INV-3, but the type
//! system does not know that) would otherwise hash unequal. `IdRepr` below
//! copies that exact tradeoff: `Eq` gets the id-compare fast path on
//! `Interned`/`Interned`, `Hash` never does. Skipping this nuance would
//! have overstated id-table's real-world win on hash-heavy workloads.
//!
//! ## Measurement-only simplification: one mutable "active table" slot
//!
//! The real `Keyword`/`InternTable` pair is ONE process-wide `static` for
//! the program's whole life. This probe instead needs several FRESH,
//! independently-sized tables in ONE process (a generous-cap table for the
//! "fits comfortably under cap" pricing, a zero-cap table for the "already
//! frozen, forced Overflow" pricing, an isolated table per workload so
//! phases don't contend for the same cap) -- so `IdRepr::text_ref` reads
//! through `set_active_table`/`active_table`, a single mutable global
//! pointer the harness repoints between phases, each phase's table
//! `Box::leak`ed for a `'static` borrow (leaking a handful of small tables
//! is an acceptable cost in a short-lived benchmark process, not a design
//! recommendation). This is a probe-only device with no analogue in the
//! real single-static design; every phase below is careful to construct
//! and consume its `IdRepr` values while ITS OWN table is active, never
//! mixing `IdRepr`s minted under different active tables.
//!
//! Run: `cargo test --release --test geo_symbol_probe -- --ignored --nocapture`
//!
//! ## Serialization: `TEST_LOCK`
//!
//! `ACTIVE_TABLE` above is a single process-global slot (that's the point --
//! it mimics the real single-`static` `Keyword`/`InternTable` design). Rust's
//! default test runner runs `#[test]` fns concurrently in one process, so any
//! two of the three entry points below that call `set_active_table` --
//! `id_repr_hits_return_stable_ids_and_reconstruct_ns_name`,
//! `id_repr_overflow_reconstructs_ns_name_and_content_compares_across_arms`,
//! and `symbol_representation_probe` -- can interleave: one test repoints
//! `ACTIVE_TABLE` to its own (differently-sized) table between another
//! test's `intern` and its later `text_ref`, so the later call indexes the
//! WRONG table's `by_id` and either panics (`Vector::index` out of bounds)
//! or silently reads someone else's text. `TEST_LOCK` below serializes
//! exactly those three entry points -- each acquires it FIRST, before
//! touching `ACTIVE_TABLE` at all, and holds it for its entire body -- so
//! at most one of them ever has a live `ACTIVE_TABLE` value at a time. This
//! changes nothing about what any test measures or asserts, only when it's
//! allowed to run relative to its siblings.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use mova::internal::champ::PersistentHashMap;
use mova::internal::imbl;
use mova::internal::{Str, Symbol};

// ---------------------------------------------------------------------------
// Shared helpers (copied per-file convention -- see module doc).
// ---------------------------------------------------------------------------

const M: u64 = 6_364_136_223_846_793_005;
const C: u64 = 1_442_695_040_888_963_407;

#[inline]
fn lcg_next(x: &mut u64) -> u64 {
    *x = x.wrapping_mul(M).wrapping_add(C);
    *x
}

fn content_hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// `(median, min)` of a sample of ns/op readings -- same convention as
/// `geo_intern_probe.rs`/`geo_boxing_probe.rs`.
fn stats(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[v.len() / 2], v[0])
}

/// Splits a flat `"ns/name"` (or bare `"name"`) string the same way
/// `builtins::strings::symbol_from_str` does (that fn is `pub(crate)`, not
/// reachable from a test crate -- copied per this file's own house
/// convention, not re-derived from scratch: same `"/"` special case, same
/// "empty ns or empty name either side of the slash doesn't count as
/// qualified" guard).
fn split_flat(flat: &str) -> (Option<&str>, &str) {
    if flat == "/" {
        return (None, "/");
    }
    if let Some(idx) = flat.find('/') {
        if idx > 0 && idx + 1 < flat.len() {
            return (Some(&flat[..idx]), &flat[idx + 1..]);
        }
    }
    (None, flat)
}

// ---------------------------------------------------------------------------
// (1) Baseline -- the real `Symbol`.
// ---------------------------------------------------------------------------

fn baseline_construct(flat: &str) -> Symbol {
    let (ns, name) = split_flat(flat);
    match ns {
        Some(ns) => Symbol {
            ns: Some(Str::from(ns)),
            name: Str::from(name),
        },
        None => Symbol::simple(name),
    }
}

// ---------------------------------------------------------------------------
// (2) IdRepr -- `Interned(u32) | Overflow(Arc<Str>)`, mirroring
// `src/keyword.rs`'s landed table shape (see module doc).
// ---------------------------------------------------------------------------

struct SymInner {
    by_hash: PersistentHashMap<u64, Vec<(Arc<str>, u32)>>,
    by_id: imbl::Vector<Str>,
}

impl SymInner {
    #[inline]
    fn find(&self, text: &str, h: u64) -> Option<u32> {
        self.by_hash.get(&h)?.iter().find(|(t, _)| t.as_ref() == text).map(|(_, id)| *id)
    }
}

struct SymTable {
    cap: u32,
    published: AtomicPtr<SymInner>,
    writer: Mutex<Vec<Box<SymInner>>>,
}

impl SymTable {
    fn with_cap(cap: u32) -> Self {
        let inner = SymInner {
            by_hash: PersistentHashMap::new(),
            by_id: imbl::Vector::new(),
        };
        SymTable {
            cap,
            published: AtomicPtr::new(Box::into_raw(Box::new(inner))),
            writer: Mutex::new(Vec::new()),
        }
    }

    #[inline]
    fn map(&'static self) -> &'static SymInner {
        // SAFETY: `published` is only ever set from `Box::into_raw` of a
        // live `SymInner`, is never null after `with_cap`, and a
        // superseded snapshot is retired (parked on `writer`), not freed
        // -- see `intern`. `self` itself is always reached through a
        // `Box::leak`ed `&'static` (see module doc), so the pointee
        // outlives this borrow.
        unsafe { &*self.published.load(Ordering::Acquire) }
    }

    #[inline]
    fn text_of(&'static self, id: u32) -> &'static Str {
        &self.map().by_id[id as usize]
    }

    /// Get-or-intern against `self`, mirroring `Keyword::construct` /
    /// `InternTable::insert_or_overflow`'s discipline exactly: lock-free
    /// hit path; miss takes the writer lock, re-checks, then either
    /// inserts (under `self.cap`) or returns a plain `Overflow`.
    fn intern(&'static self, text: &str) -> IdRepr {
        let h = content_hash(text);
        if let Some(id) = self.map().find(text, h) {
            return IdRepr::Interned(id);
        }
        let mut retired = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = self.map().find(text, h) {
            return IdRepr::Interned(id);
        }
        let old = self.map();
        let len = old.by_id.len() as u32;
        if len >= self.cap {
            return IdRepr::Overflow(Arc::new(Str::from(text)));
        }
        let id = len;
        let s = Str::from(text);
        let handle: Arc<str> = Arc::from(text);
        let mut by_id = old.by_id.clone(); // O(1): imbl::Vector, structural sharing
        by_id.push_back(s);
        let mut bucket = old.by_hash.get(&h).cloned().unwrap_or_default();
        bucket.push((handle, id));
        let next = Box::new(SymInner {
            by_hash: old.by_hash.assoc(h, bucket),
            by_id,
        });
        let raw = self.published.swap(Box::into_raw(next), Ordering::Release);
        // SAFETY: `raw` came from a prior `Box::into_raw`; no concurrent
        // swap can race (writer lock held).
        retired.push(unsafe { Box::from_raw(raw) });
        IdRepr::Interned(id)
    }
}

/// Single mutable "which table is `IdRepr::text_ref` allowed to dereference
/// right now" slot -- see module doc's measurement-only-simplification
/// section. Single-threaded test process; `Relaxed` is sufficient (no
/// cross-thread publication to order against).
static ACTIVE_TABLE: AtomicPtr<SymTable> = AtomicPtr::new(std::ptr::null_mut());

/// Serializes every `#[test]` entry point that touches `ACTIVE_TABLE` --
/// see module doc's "Serialization: `TEST_LOCK`" section. Poison-tolerant:
/// a panic while another test held the lock must not wedge the rest of the
/// suite.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn set_active_table(t: &'static SymTable) {
    ACTIVE_TABLE.store(t as *const SymTable as *mut SymTable, Ordering::Relaxed);
}

fn active_table() -> &'static SymTable {
    let p = ACTIVE_TABLE.load(Ordering::Relaxed);
    assert!(!p.is_null(), "geo_symbol_probe bug: no active SymTable set before touching an IdRepr");
    unsafe { &*p }
}

/// `Value::Keyword`'s payload shape, retargeted at flat symbol text. See
/// module doc for why `Interned` carries no table handle (the active-table
/// indirection instead).
#[derive(Clone, Debug)]
enum IdRepr {
    Interned(u32),
    Overflow(Arc<Str>),
}

impl IdRepr {
    #[inline]
    fn text_ref(&self) -> &Str {
        match self {
            IdRepr::Interned(id) => active_table().text_of(*id),
            IdRepr::Overflow(s) => s,
        }
    }

    /// The reconstruct-the-split cost workload (c) prices, on both arms.
    #[inline]
    fn ns_name(&self) -> (Option<&str>, &str) {
        split_flat(self.text_ref().as_ref())
    }
}

impl PartialEq for IdRepr {
    /// `Interned`/`Interned`: bare `u32` compare (the fast path). Every
    /// other arm falls back to content compare -- copied verbatim from
    /// `Keyword`'s own `PartialEq` (`src/keyword.rs`), including its
    /// "cross arms cannot occur for matching text but must still
    /// type-check safely" defense-in-depth argument.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (IdRepr::Interned(a), IdRepr::Interned(b)) => a == b,
            _ => self.text_ref() == other.text_ref(),
        }
    }
}
impl Eq for IdRepr {}

impl Hash for IdRepr {
    /// Content-based UNCONDITIONALLY -- see module doc's "one correctness
    /// nuance" section. Deliberately does NOT take the id-hash shortcut.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text_ref().hash(state);
    }
}

// ---------------------------------------------------------------------------
// (3) WeakRepr -- reclaimable, mirrors Probe D's `WeakTable` exactly.
// ---------------------------------------------------------------------------

struct WeakSymTable {
    buckets: Mutex<HashMap<u64, Vec<Weak<str>>>>,
}

impl WeakSymTable {
    fn new() -> Self {
        WeakSymTable {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn intern(&self, text: &str) -> WeakRepr {
        let h = content_hash(text);
        let mut g = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = g.entry(h).or_default();
        let mut i = 0;
        while i < bucket.len() {
            match bucket[i].upgrade() {
                Some(s) if s.as_ref() == text => return WeakRepr(s),
                Some(_) => i += 1,
                None => {
                    bucket.swap_remove(i);
                }
            }
        }
        let fresh: Arc<str> = Arc::from(text);
        bucket.push(Arc::downgrade(&fresh));
        WeakRepr(fresh)
    }
}

#[derive(Clone)]
struct WeakRepr(Arc<str>);

impl WeakRepr {
    #[inline]
    fn ns_name(&self) -> (Option<&str>, &str) {
        split_flat(self.0.as_ref())
    }
}

impl PartialEq for WeakRepr {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_ref() == other.0.as_ref()
    }
}
impl Eq for WeakRepr {}
impl Hash for WeakRepr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_ref().hash(state);
    }
}

// ---------------------------------------------------------------------------
// Correctness (fast, unignored -- gate discipline per module doc).
// ---------------------------------------------------------------------------

#[test]
fn split_flat_matches_symbol_from_str_shape() {
    assert_eq!(split_flat("foo"), (None, "foo"));
    assert_eq!(split_flat("ns.core/foo"), (Some("ns.core"), "foo"));
    assert_eq!(split_flat("/"), (None, "/"));
    assert_eq!(split_flat("/foo"), (None, "/foo")); // idx == 0: not qualified
    assert_eq!(split_flat("foo/"), (None, "foo/")); // idx+1 == len: not qualified
}

#[test]
fn id_repr_hits_return_stable_ids_and_reconstruct_ns_name() {
    // Must acquire BEFORE touching `ACTIVE_TABLE` -- see module doc.
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(10)));
    set_active_table(t);
    let a = t.intern("ns.a/foo");
    let b = t.intern("bar");
    let a2 = t.intern("ns.a/foo");
    assert!(matches!(a, IdRepr::Interned(_)));
    assert_eq!(a, a2, "re-interning the same text must return an equal repr");
    assert_ne!(a, b);
    assert_eq!(a.ns_name(), (Some("ns.a"), "foo"));
    assert_eq!(b.ns_name(), (None, "bar"));
}

#[test]
fn id_repr_overflow_reconstructs_ns_name_and_content_compares_across_arms() {
    // Must acquire BEFORE touching `ACTIVE_TABLE` -- see module doc.
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(0)));
    set_active_table(t);
    let over = t.intern("ns.q/quux");
    assert!(matches!(over, IdRepr::Overflow(_)));
    assert_eq!(over.ns_name(), (Some("ns.q"), "quux"));

    // Cross-arm equality (unreachable under INV-3 in the real design, but
    // must still be content-correct if it ever happened -- same
    // defense-in-depth `Keyword::eq` documents).
    let t2: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(10)));
    set_active_table(t2);
    let interned_same_text = t2.intern("standalone/text");
    let overflow_same_text = IdRepr::Overflow(Arc::new(Str::from("standalone/text")));
    assert_eq!(interned_same_text, overflow_same_text);
}

#[test]
fn weak_repr_hits_return_the_same_allocation_while_live() {
    let t = WeakSymTable::new();
    let a = t.intern("ns/foo");
    let b = t.intern("ns/foo");
    assert!(Arc::ptr_eq(&a.0, &b.0), "a live hit must return the SAME allocation");
    assert_eq!(a.ns_name(), (Some("ns"), "foo"));
    drop(a);
    drop(b);
    let c = t.intern("ns/foo");
    assert_eq!(c.ns_name(), (Some("ns"), "foo"));
}

// ---------------------------------------------------------------------------
// Workload text generators.
// ---------------------------------------------------------------------------

/// (a)'s resolve-churn population: a realistic mix of unqualified locals
/// and qualified globals -- most symbol references in real source are
/// unqualified (locals, params, unqualified var refs), a minority are
/// namespace-qualified against a SMALL set of namespaces (a program
/// imports/requires far fewer namespaces than it defines symbols).
fn resolve_sym_text(i: usize) -> String {
    if i % 3 == 0 {
        format!("ns{}.core/sym-{}", i % 12, i)
    } else {
        format!("local-sym-{i}")
    }
}

/// (b) distinct-heavy: gensym-shaped (`Compiler`/`quasiquote.rs`'s own
/// counters both produce exactly this `PREFIX__<n>` shape).
fn gensym_text(i: usize) -> String {
    format!("G__{i}")
}

const REPEAT_POOL: usize = 64;

/// (b) repeat-heavy: a small, fixed vocabulary re-created over and over --
/// the macro-re-expansion shape (the same handful of parameter/binding
/// names re-appearing every expansion of a hot macro).
fn repeat_text(i: usize) -> String {
    format!("macro-local-{}", i % REPEAT_POOL)
}

/// (c)/(d) pool population: same realistic qualified/unqualified mix as
/// (a), different literal texts so the pools are independent.
fn pool_sym_text(i: usize) -> String {
    if i % 4 == 0 {
        format!("clojure.core/pool-sym-{i}")
    } else {
        format!("pool-local-{i}")
    }
}

// ---------------------------------------------------------------------------
// (a) resolve churn -- `resolve_symbol` (`src/compile/resolve.rs:493`)'s
// shape: a `HashMap<Sym, u32>` standing in for the global-chain lookup a
// compiled fn's free-symbol reference resolves against, ~5k entries, then
// 500k lookups with an LCG-driven 80/20 hit/miss stream. Construction cost
// is deliberately OUT of the timed region here (a real `resolve_symbol`
// call receives an ALREADY-PARSED `&Symbol`, it never builds one) -- (b)
// prices construction separately.
// ---------------------------------------------------------------------------

const N_RESOLVE_ENTRIES: usize = 5_000;
const N_RESOLVE_MISS_POOL: usize = 2_000;
const RESOLVE_LOOKUPS: usize = 500_000;
const RESOLVE_HIT_PCT: u64 = 80;

fn bench_resolve_churn() -> [(f64, f64); 3] {
    let hit_texts: Vec<String> = (0..N_RESOLVE_ENTRIES).map(resolve_sym_text).collect();
    let miss_texts: Vec<String> = (0..N_RESOLVE_MISS_POOL).map(|i| format!("undefined-sym-{i}")).collect();

    // -- baseline --
    let mut base_map: HashMap<Symbol, u32> = HashMap::new();
    for (i, t) in hit_texts.iter().enumerate() {
        base_map.insert(baseline_construct(t), i as u32);
    }
    let base_hits: Vec<Symbol> = hit_texts.iter().map(|t| baseline_construct(t)).collect();
    let base_misses: Vec<Symbol> = miss_texts.iter().map(|t| baseline_construct(t)).collect();

    // -- id-table: generous cap, so this workload prices the COMMON case
    // (design doc §2.1: an ordinary program's own vocabulary is "hundreds",
    // far under any sane cap) -- every entry here is genuinely `Interned`.
    let id_table: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(64_000)));
    set_active_table(id_table);
    let mut id_map: HashMap<IdRepr, u32> = HashMap::new();
    for (i, t) in hit_texts.iter().enumerate() {
        id_map.insert(id_table.intern(t), i as u32);
    }
    let id_hits: Vec<IdRepr> = hit_texts.iter().map(|t| id_table.intern(t)).collect();
    let id_misses: Vec<IdRepr> = miss_texts.iter().map(|t| id_table.intern(t)).collect();

    // -- weak-table --
    let weak_table = WeakSymTable::new();
    let mut weak_map: HashMap<WeakRepr, u32> = HashMap::new();
    for (i, t) in hit_texts.iter().enumerate() {
        let r = weak_table.intern(t);
        weak_map.insert(r, i as u32);
    }
    let weak_hits: Vec<WeakRepr> = hit_texts.iter().map(|t| weak_table.intern(t)).collect();
    let weak_misses: Vec<WeakRepr> = miss_texts.iter().map(|t| weak_table.intern(t)).collect();

    // Same LCG-driven hit/miss/index stream, reused identically across all
    // three designs (and every round) so they are compared against
    // literally the same query sequence.
    let mut lcg = 0xB0AD_0000u64;
    let stream: Vec<(bool, usize)> = (0..RESOLVE_LOOKUPS)
        .map(|_| {
            let r = lcg_next(&mut lcg);
            let is_hit = (r % 100) < RESOLVE_HIT_PCT;
            let idx = if is_hit {
                (r >> 8) as usize % hit_texts.len()
            } else {
                (r >> 8) as usize % miss_texts.len()
            };
            (is_hit, idx)
        })
        .collect();

    let run_base = || {
        let mut sink = 0u64;
        for &(hit, idx) in &stream {
            let k = if hit { &base_hits[idx] } else { &base_misses[idx] };
            if base_map.get(black_box(k)).is_some() {
                sink += 1;
            }
        }
        sink
    };
    let run_id = || {
        let mut sink = 0u64;
        for &(hit, idx) in &stream {
            let k = if hit { &id_hits[idx] } else { &id_misses[idx] };
            if id_map.get(black_box(k)).is_some() {
                sink += 1;
            }
        }
        sink
    };
    let run_weak = || {
        let mut sink = 0u64;
        for &(hit, idx) in &stream {
            let k = if hit { &weak_hits[idx] } else { &weak_misses[idx] };
            if weak_map.get(black_box(k)).is_some() {
                sink += 1;
            }
        }
        sink
    };

    black_box(run_base());
    black_box(run_id());
    black_box(run_weak());
    let (mut rb, mut ri, mut rw) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..9 {
        let t0 = Instant::now();
        black_box(run_base());
        rb.push(t0.elapsed().as_secs_f64() / RESOLVE_LOOKUPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_id());
        ri.push(t0.elapsed().as_secs_f64() / RESOLVE_LOOKUPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_weak());
        rw.push(t0.elapsed().as_secs_f64() / RESOLVE_LOOKUPS as f64 * 1e9);
    }
    [stats(rb), stats(ri), stats(rw)]
}

// ---------------------------------------------------------------------------
// (b) construction churn -- distinct-heavy (gensym) vs repeat-heavy (macro
// re-expansion). Distinct-heavy is priced TWICE for id-table: once against
// a table with headroom (every construct is a genuine `Interned` insert)
// and once against an already-frozen (cap=0) table (every construct is a
// genuine `Overflow`) -- the two regimes a real gensym burst can land in
// depending on how much of the process-wide cap is already spent.
// ---------------------------------------------------------------------------

const DISTINCT_OPS: usize = 200_000;
const REPEAT_OPS: usize = 200_000;
const CONSTRUCT_ROUNDS: usize = 9;

struct ConstructionBench {
    distinct_fits: [(f64, f64); 3],    // baseline, id-table(fits cap), weak -- (median, min)
    distinct_overflow_id: (f64, f64),  // id-table, cap already frozen -- (median, min)
    repeat: [(f64, f64); 3],           // baseline, id-table, weak -- (median, min)
}

fn bench_construction_churn() -> ConstructionBench {
    // ---- distinct-heavy, fits cap ----
    let id_fits: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(3_000_000)));
    let weak_distinct_table = WeakSymTable::new();
    let bench_distinct = |mut f: Box<dyn FnMut(usize)>| -> (f64, f64) {
        let mut r = Vec::new();
        for round in 0..CONSTRUCT_ROUNDS {
            let base = round * DISTINCT_OPS;
            let t0 = Instant::now();
            for i in 0..DISTINCT_OPS {
                f(base + i);
            }
            r.push(t0.elapsed().as_secs_f64() / DISTINCT_OPS as f64 * 1e9);
        }
        stats(r)
    };
    let base_distinct = bench_distinct(Box::new(|i| {
        let s = gensym_text(i);
        black_box(baseline_construct(black_box(&s)));
    }));
    set_active_table(id_fits);
    let id_distinct = bench_distinct(Box::new(|i| {
        let s = gensym_text(i);
        black_box(id_fits.intern(black_box(&s)));
    }));
    let weak_distinct = bench_distinct(Box::new(|i| {
        let s = gensym_text(i);
        black_box(weak_distinct_table.intern(black_box(&s)));
    }));

    // ---- distinct-heavy, id-table already frozen (cap == 0) ----
    let id_overflow: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(0)));
    set_active_table(id_overflow);
    let id_overflow_distinct = bench_distinct(Box::new(|i| {
        let s = gensym_text(i);
        black_box(id_overflow.intern(black_box(&s)));
    }));

    // ---- repeat-heavy: a shared table/pool across warmup + rounds so the
    // timed rounds measure genuine steady-state HITS (the macro-
    // re-expansion shape re-creates the SAME handful of names). ----
    let id_repeat: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(1_000)));
    let weak_repeat = WeakSymTable::new();
    let bench_repeat = |mut f: Box<dyn FnMut(usize)>| -> (f64, f64) {
        for i in 0..REPEAT_OPS {
            f(i); // warmup: populates the table/pool
        }
        let mut r = Vec::new();
        for _ in 0..CONSTRUCT_ROUNDS {
            let t0 = Instant::now();
            for i in 0..REPEAT_OPS {
                f(i);
            }
            r.push(t0.elapsed().as_secs_f64() / REPEAT_OPS as f64 * 1e9);
        }
        stats(r)
    };
    let base_repeat_ns = bench_repeat(Box::new(|i| {
        let s = repeat_text(i);
        black_box(baseline_construct(black_box(&s)));
    }));
    set_active_table(id_repeat);
    let id_repeat_ns = bench_repeat(Box::new(|i| {
        let s = repeat_text(i);
        black_box(id_repeat.intern(black_box(&s)));
    }));
    let weak_repeat_ns = bench_repeat(Box::new(|i| {
        let s = repeat_text(i);
        black_box(weak_repeat.intern(black_box(&s)));
    }));

    ConstructionBench {
        distinct_fits: [base_distinct, id_distinct, weak_distinct],
        distinct_overflow_id: id_overflow_distinct,
        repeat: [base_repeat_ns, id_repeat_ns, weak_repeat_ns],
    }
}

// ---------------------------------------------------------------------------
// (c) ns/name access -- the printer/analyzer shape: given ALREADY
// constructed symbols, read `.ns`/`.name`. Baseline pays neither (fields
// are already split at construction); id-table prices the reconstruct cost
// on the `Interned` AND `Overflow` arms separately; weak-table always pays
// the split (it never has an id shortcut).
// ---------------------------------------------------------------------------

const ACCESS_POOL: usize = 5_000;
const ACCESS_OPS: usize = 200_000;
const ACCESS_ROUNDS: usize = 9;

fn bench_ns_name_access() -> [(f64, f64); 4] {
    let texts: Vec<String> = (0..ACCESS_POOL).map(pool_sym_text).collect();

    let base_pool: Vec<Symbol> = texts.iter().map(|t| baseline_construct(t)).collect();

    let id_interned_table: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(64_000)));
    set_active_table(id_interned_table);
    let id_interned_pool: Vec<IdRepr> = texts.iter().map(|t| id_interned_table.intern(t)).collect();

    // `id_overflow_table`'s pool is built below, once its active-table slot
    // is switched in -- every one of ITS constructs must land in `Overflow`
    // (cap 0), so it must not share a construction pass with `id_interned_table`.
    let id_overflow_table: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(0)));

    let weak_table = WeakSymTable::new();
    let weak_pool: Vec<WeakRepr> = texts.iter().map(|t| weak_table.intern(t)).collect();

    let bench = |mut f: Box<dyn FnMut(usize) -> usize>| -> (f64, f64) {
        let mut r = Vec::new();
        for _ in 0..ACCESS_ROUNDS {
            let t0 = Instant::now();
            let mut acc = 0usize;
            for i in 0..ACCESS_OPS {
                acc = acc.wrapping_add(f(i % ACCESS_POOL));
            }
            black_box(acc);
            r.push(t0.elapsed().as_secs_f64() / ACCESS_OPS as f64 * 1e9);
        }
        stats(r)
    };

    let base_ns = bench(Box::new(|i| {
        let s = &base_pool[i];
        let nslen = s.ns.as_ref().map_or(0, |n| n.as_ref().len());
        black_box(nslen + s.name.as_ref().len())
    }));

    set_active_table(id_interned_table);
    let id_interned_ns = bench(Box::new(|i| {
        let (ns, name) = id_interned_pool[i].ns_name();
        black_box(ns.map_or(0, str::len) + name.len())
    }));

    set_active_table(id_overflow_table);
    let id_overflow_pool: Vec<IdRepr> = texts.iter().map(|t| id_overflow_table.intern(t)).collect();
    let id_overflow_ns = bench(Box::new(|i| {
        let (ns, name) = id_overflow_pool[i].ns_name();
        black_box(ns.map_or(0, str::len) + name.len())
    }));

    let weak_ns = bench(Box::new(|i| {
        let (ns, name) = weak_pool[i].ns_name();
        black_box(ns.map_or(0, str::len) + name.len())
    }));

    [base_ns, id_interned_ns, id_overflow_ns, weak_ns]
}

// ---------------------------------------------------------------------------
// (d) clone/drop -- the `Value::Sym` embedding shape: `Symbol` is cloned on
// every `Value::Sym` clone (env captures, `Ir::Const` bakes, collection
// element copies, ...). Baseline pays a 32B copy plus, for a qualified
// symbol, TWO `Arc` refcount bumps; `IdRepr::Interned` is `Copy` (a bare
// `u32`, no atomic traffic at all); `IdRepr::Overflow` and `WeakRepr` both
// pay one `Arc` bump, same order as baseline's unqualified case.
// ---------------------------------------------------------------------------

const CLONE_POOL: usize = 5_000;
const CLONE_OPS: usize = 200_000;
const CLONE_ROUNDS: usize = 9;

fn bench_clone_drop() -> [(f64, f64); 4] {
    let texts: Vec<String> = (0..CLONE_POOL).map(pool_sym_text).collect();

    let base_pool: Vec<Symbol> = texts.iter().map(|t| baseline_construct(t)).collect();

    let id_interned_table: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(64_000)));
    set_active_table(id_interned_table);
    let id_interned_pool: Vec<IdRepr> = texts.iter().map(|t| id_interned_table.intern(t)).collect();

    let id_overflow_table: &'static SymTable = Box::leak(Box::new(SymTable::with_cap(0)));
    set_active_table(id_overflow_table);
    let id_overflow_pool: Vec<IdRepr> = texts.iter().map(|t| id_overflow_table.intern(t)).collect();

    let weak_table = WeakSymTable::new();
    let weak_pool: Vec<WeakRepr> = texts.iter().map(|t| weak_table.intern(t)).collect();

    fn bench_pool<T: Clone>(pool: &[T]) -> (f64, f64) {
        let mut r = Vec::new();
        for _ in 0..CLONE_ROUNDS {
            let t0 = Instant::now();
            for i in 0..CLONE_OPS {
                let c = black_box(pool[i % pool.len()].clone());
                drop(black_box(c));
            }
            r.push(t0.elapsed().as_secs_f64() / CLONE_OPS as f64 * 1e9);
        }
        stats(r)
    }

    let base_ns = bench_pool(&base_pool);
    let id_interned_ns = bench_pool(&id_interned_pool);
    let id_overflow_ns = bench_pool(&id_overflow_pool);
    let weak_ns = bench_pool(&weak_pool);

    [base_ns, id_interned_ns, id_overflow_ns, weak_ns]
}

// ---------------------------------------------------------------------------
// The kill-probe proper.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "measurement, not a gate -- run with --ignored --nocapture"]
fn symbol_representation_probe() {
    // Must acquire BEFORE touching `ACTIVE_TABLE` -- see module doc. Held
    // for the whole measurement run since `bench_resolve_churn` /
    // `bench_construction_churn` / `bench_ns_name_access` / `bench_clone_drop`
    // below all set `ACTIVE_TABLE` on this same thread.
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    println!("\n=== W-GEO kill-probe F: Symbol representation pricing (stage-3) ===");
    println!("size_of::<Symbol>() (baseline) = {}B; size_of::<IdRepr>() (mock) = {}B", std::mem::size_of::<Symbol>(), std::mem::size_of::<IdRepr>());

    let resolve = bench_resolve_churn();
    println!("\n-- (a) resolve churn: HashMap<Sym,u32> ~{N_RESOLVE_ENTRIES} entries, {RESOLVE_LOOKUPS} lookups, {RESOLVE_HIT_PCT}/{}% hit/miss --", 100 - RESOLVE_HIT_PCT);
    println!("  {:<24} {:>18} {:>18} {:>18}", "op", "baseline", "id-table", "weak-table");
    println!(
        "  {:<24} {:>14.2}ns {:>14.2}ns {:>14.2}ns   (median)",
        "lookup (mixed)", resolve[0].0, resolve[1].0, resolve[2].0
    );
    println!(
        "  {:<24} {:>14.2}ns {:>14.2}ns {:>14.2}ns   (min)",
        "lookup (mixed)", resolve[0].1, resolve[1].1, resolve[2].1
    );

    let cons = bench_construction_churn();
    println!("\n-- (b) construction churn: {DISTINCT_OPS} ops/round x {CONSTRUCT_ROUNDS} rounds --");
    println!("  {:<34} {:>16} {:>16} {:>16} {:>16}", "shape (stat)", "baseline", "id(fits cap)", "id(frozen)", "weak-table");
    println!(
        "  {:<34} {:>12.2}ns {:>12.2}ns {:>12.2}ns {:>12.2}ns",
        "distinct-heavy/gensym (median)", cons.distinct_fits[0].0, cons.distinct_fits[1].0, cons.distinct_overflow_id.0, cons.distinct_fits[2].0
    );
    println!(
        "  {:<34} {:>12.2}ns {:>12.2}ns {:>12.2}ns {:>12.2}ns",
        "distinct-heavy/gensym (min)", cons.distinct_fits[0].1, cons.distinct_fits[1].1, cons.distinct_overflow_id.1, cons.distinct_fits[2].1
    );
    println!(
        "  {:<34} {:>12.2}ns {:>12.2}ns {:>16} {:>12.2}ns",
        "repeat-heavy/macro-re-exp (median)", cons.repeat[0].0, cons.repeat[1].0, "n/a", cons.repeat[2].0
    );
    println!(
        "  {:<34} {:>12.2}ns {:>12.2}ns {:>16} {:>12.2}ns",
        "repeat-heavy/macro-re-exp (min)", cons.repeat[0].1, cons.repeat[1].1, "n/a", cons.repeat[2].1
    );

    let access = bench_ns_name_access();
    println!("\n-- (c) ns/name access: {ACCESS_POOL}-symbol pool, {ACCESS_OPS} ops/round x {ACCESS_ROUNDS} rounds --");
    println!("  {:<24} {:>16} {:>16} {:>16} {:>16}", "op (stat)", "baseline", "id(Interned)", "id(Overflow)", "weak-table");
    println!(
        "  {:<24} {:>12.2}ns {:>12.2}ns {:>12.2}ns {:>12.2}ns",
        ".ns/.name read (median)", access[0].0, access[1].0, access[2].0, access[3].0
    );
    println!(
        "  {:<24} {:>12.2}ns {:>12.2}ns {:>12.2}ns {:>12.2}ns",
        ".ns/.name read (min)", access[0].1, access[1].1, access[2].1, access[3].1
    );

    let clone = bench_clone_drop();
    println!("\n-- (d) clone+drop: {CLONE_POOL}-symbol pool, {CLONE_OPS} ops/round x {CLONE_ROUNDS} rounds --");
    println!("  {:<24} {:>16} {:>16} {:>16} {:>16}", "op (stat)", "baseline (32B)", "id(Interned)", "id(Overflow)", "weak-table");
    println!(
        "  {:<24} {:>12.2}ns {:>12.2}ns {:>12.2}ns {:>12.2}ns",
        "clone+drop (median)", clone[0].0, clone[1].0, clone[2].0, clone[3].0
    );
    println!(
        "  {:<24} {:>12.2}ns {:>12.2}ns {:>12.2}ns {:>12.2}ns",
        "clone+drop (min)", clone[0].1, clone[1].1, clone[2].1, clone[3].1
    );

    println!(
        "\n  Read (stated plainly, see docs/W-GEO-PROBE-VERDICTS.md's Probe F section for the full \
         write-up): id-table's `Hash` is content-based on BOTH arms (mirroring the real landed `Keyword`'s \
         INV-3-driven tradeoff, see this file's module doc) -- so id-table's win on (a)'s HashMap-lookup \
         workload, if any, comes ONLY from its `Interned`/`Interned` equality fast path (checked after the \
         hash already matched), never from a cheaper hash. (b)'s two id-table columns show the honest split a \
         real gensym burst can land in: comfortably under cap (fast lock-free insert) vs an already-frozen \
         table (forced `Overflow`, an ordinary allocation, same shape baseline pays for a fresh symbol). (c) \
         is the one workload baseline structurally cannot lose: its ns/name are already-split fields, zero \
         reconstruction; both id-table arms and weak-table all pay a live split of flat text on every access, \
         every time -- there is no id shortcut for TEXT, only for equality. (d) is where a genuinely narrower \
         repr should show up most plainly: `IdRepr::Interned` is `Copy`, no atomic traffic, against baseline's \
         32B copy plus up to two `Arc` bumps."
    );

    assert!(resolve[0].1 > 0.0 && resolve[1].1 > 0.0 && resolve[2].1 > 0.0, "probe produced no measurement");
}
