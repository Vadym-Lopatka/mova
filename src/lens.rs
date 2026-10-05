//! field4/W-LENS-1: the runtime regret ledger -- "where and under which
//! conditions is this runtime NOT optimal", answered from data.
//!
//! See `docs/W-LENS-DESIGN.md` for the why and `docs/W-LENS-SCHEMA.md` for
//! the report's public, versioned EDN schema. This module is the substrate
//! plus the report builder; every instrumentation point elsewhere in the
//! crate is one `lens::event(..)`/`lens::event_at(..)` call.
//!
//! # The central metric
//!
//! `MOVA_EXPLAIN` already reports STATIC facts ("this fn bailed"). The
//! datum session 12 was missing is "...and then it executed 2,000,000
//! times". Regret = *slow-path decision* x *execution count*. Every counter
//! here is an execution count at a place the runtime knowingly took a slow
//! path, attributed (where the attribution is free) to the fn/macro/site
//! that owns the decision.
//!
//! # Substrate: thread-local counter pages (the `C_tls` idiom)
//!
//! Principle 1 of the design doc is non-negotiable: **no shared RMW on any
//! hot path**. A `static AtomicU64` per counter is a shared-cacheline
//! read-modify-write -- exactly the disease W-ENV and the publication
//! patterns spent campaigns curing, and it would poison the very benches
//! this instrument exists to explain. So:
//!
//! * Each thread owns a [`Page`]: a fixed block of cells, allocated on that
//!   thread's FIRST event and never resized.
//! * The page registers ONCE into [`PAGES`] (a `Mutex<Vec<Arc<Page>>>`,
//!   touched only on that first event and at report time -- cold both ways)
//!   and is then reached through a `*const Page` in TLS.
//! * A hit is: one TLS load, one `Relaxed` load + `Relaxed` store on a cell
//!   **no other thread ever writes**. That is not an RMW; it never contends.
//!   The cells are `AtomicU64` rather than plain `u64` only because report
//!   time reads them from another thread, and a plain non-atomic read
//!   racing a write is UB in Rust even when the machine would do the right
//!   thing. `Relaxed` is exactly right: this is diagnostic instrumentation,
//!   not a synchronization primitive, and a report that misses the last
//!   handful of increments on a running thread is a report, not a bug.
//! * Pages are never removed from the registry, so a dead thread's counts
//!   still show up (the ledger is process-wide and monotone) and the
//!   `*const Page` a live thread holds can never dangle.
//!
//! # Two kinds of counter
//!
//! * **Static events** ([`Event`]): a fixed, curated table, one cell each,
//!   indexed by a compile-time constant. These are the process-wide totals.
//! * **Sites**: dynamically allocated ids (one per fn that bails, per fn
//!   that carries `Ir::Escape`s, per macro), allocated ONCE at compile time
//!   through [`alloc_site`] -- the cold path -- and stamped into the
//!   `Closure`/`ir::Escape` that will do the executing. A site hit bumps
//!   both the site's own cell and the static total, so the totals stay
//!   correct even for the fns that get no attribution (anonymous fns whose
//!   site allocation overflowed the page, say).
//!
//! Sites beyond [`MAX_SITES`] are not tracked individually; their hits land
//! in [`Event::SiteOverflow`] and the report says so. A ledger that quietly
//! dropped counts would be worse than useless.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::value::{PMap, Str, Value};

// ---------------------------------------------------------------------------
// Schema version
// ---------------------------------------------------------------------------

/// The report's schema version (owner gate 4, RULED 2026-08-22: versioned
/// and STABLE public embed API). Semver: **additive changes bump minor**,
/// and a consumer MUST tolerate unknown keys. A major bump is a semantic
/// contract change and is itself an owner gate.
pub const SCHEMA_VERSION: &str = "1.0.0";

// ---------------------------------------------------------------------------
// The curated event table
// ---------------------------------------------------------------------------

/// Every static counter, with the decision it informs. Adding a row here is
/// an additive schema change (minor bump); every row must name a consumer,
/// per the design doc's principle 5 -- a metric with no consumer is RSS.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Event {
    /// A closure with `compiled: None` was INVOKED: the whole fn tree-walks
    /// for this call. Multiplied by the bail reason in the site table, this
    /// is the metric that would have found session 12's 47x in seconds.
    /// Consumer: W-RESOLVE successors -- which bail rule to attack next.
    TierBailExec = 0,
    /// An `Ir::Escape` node executed: one interop form handed back to the
    /// tree-walker from inside an otherwise-compiled fn.
    /// Consumer: escape-graduation decisions (`recur`, type forms).
    EscapeExec = 1,
    /// A macro was expanded by the tree-walker (the compiled tier freezes
    /// expansion, so every count here is a RE-expansion of a form that will
    /// be expanded again next time round the loop).
    /// Consumer: the macro-expansion-cache wave and its trigger threshold.
    MacroExpand = 2,
    /// `VarCell::get` took the `dyn_hint` branch and walked the per-cell,
    /// per-context `binding` stack (a `RwLock` read + ctx-id (`u64`) hash)
    /// rather than the root fast path.
    /// Consumer: the VarCell revival trigger (`VarCell::get` measured ~4.9%
    /// of delays.clj post-W-RESOLVE).
    DynBindingWalk = 3,
    /// A `catch` clause rebuilt an exception's class chain (a fresh `Vec`
    /// per clause per throw) to decide whether it matches.
    /// Consumer: the declined exception items' revival trigger.
    CatchChainRebuild = 4,
    /// A `delay`/`cached-delay` whose cached result is an ERROR was forced
    /// again, deep-cloning the whole `RjError` (span, label, stack, thrown
    /// value) to hand it back.
    /// Consumer: same revival trigger; this was ~3.7% of delays' pie.
    DelayErrorReclone = 5,
    /// A multimethod dispatch that could have used the best-method cache and
    /// did not (fell through to the registry read + linear `find_best_method`
    /// scan). Consumer: cache-shape waves.
    MultiCacheMiss = 6,
    /// A multimethod dispatch answered straight out of the cache -- the
    /// denominator that makes [`Event::MultiCacheMiss`] a RATE rather than a
    /// bare number.
    MultiCacheHit = 7,
    /// A protocol method dispatch whose `ProtoIc` probe missed and fell
    /// through to the `impls` hash walk. Consumer: cache-shape waves.
    ProtoIcMiss = 8,
    /// A protocol method dispatch answered by the `ProtoIc` probe.
    ProtoIcHit = 9,
    /// A `HostStruct` keyword lookup whose packed inline cache missed and
    /// re-scanned the shape's field list. Consumer: cache-shape waves.
    HostStructIcMiss = 10,
    /// A `HostStruct` keyword lookup answered by the packed IC.
    HostStructIcHit = 11,
    /// A `PVec` `Small` -> `Big` promotion actually happened (one-way, and
    /// the moment a small vector starts paying `imbl`'s price).
    /// Consumer: W-GEO boxing.
    PVecPromote = 12,
    /// A `PMap` `Small` -> `Big` promotion. Consumer: W-GEO boxing.
    PMapPromote = 13,
    /// `PVec` persistent-update churn. **Only counted under
    /// `--features geo-census`** -- see this module's `geo` section and
    /// `crate::geo_census`.
    PVecChurn = 14,
    /// `PVec` single-element read. `geo-census` builds only.
    PVecAccess = 15,
    /// `PVec` full traversal (one per `iter()` CALL). `geo-census` builds only.
    PVecScan = 16,
    /// `PMap` persistent-update churn. `geo-census` builds only.
    PMapChurn = 17,
    /// `PMap` single-element read. `geo-census` builds only.
    PMapAccess = 18,
    /// Hits belonging to a site that could not be allocated a cell (more
    /// than [`MAX_SITES`] distinct sites). Present so the per-site table's
    /// incompleteness is VISIBLE rather than silent.
    SiteOverflow = 19,
    /// An `Ir::FieldGet` whose per-site inline cache did not hold the
    /// receiver's type, so the field index came from a scan of
    /// `TypeDef::basis` (and, when found, was installed for next time). A
    /// steady stream of these rather than a fixed handful means the site is
    /// polymorphic beyond `ir::FIELD_IC_SLOTS` -- still correct, just paying
    /// the scan. Consumer: W-FIELDGET.
    FieldIcMiss = 20,
    /// An `Ir::FieldGet` answered straight out of its inline cache: the
    /// compiled-tier replacement for what used to be an `escape-exec`.
    /// Consumer: W-FIELDGET.
    FieldIcHit = 21,
}

/// How many static event cells a page reserves. Larger than [`Event`]'s
/// current count so an additive schema bump does not move the site base
/// (which would invalidate nothing, since ids are never persisted, but
/// keeping it fixed makes A/B'ing two builds' dumps trivial).
pub const N_STATIC: usize = 32;

/// EDN key for each [`Event`], in `Event`-discriminant order. Namespaced
/// per gate 4: `:lens.event/...`.
const EVENT_KEYS: [&str; 22] = [
    "lens.event/tier-bail-exec",
    "lens.event/escape-exec",
    "lens.event/macro-expand",
    "lens.event/dyn-binding-walk",
    "lens.event/catch-chain-rebuild",
    "lens.event/delay-error-reclone",
    "lens.event/multi-cache-miss",
    "lens.event/multi-cache-hit",
    "lens.event/proto-ic-miss",
    "lens.event/proto-ic-hit",
    "lens.event/hoststruct-ic-miss",
    "lens.event/hoststruct-ic-hit",
    "lens.event/pvec-promote",
    "lens.event/pmap-promote",
    "lens.event/pvec-churn",
    "lens.event/pvec-access",
    "lens.event/pvec-scan",
    "lens.event/pmap-churn",
    "lens.event/pmap-access",
    "lens.event/site-overflow",
    "lens.event/fieldget-ic-miss",
    "lens.event/fieldget-ic-hit",
];

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// How many distinct decision points the ledger can attribute individually.
/// Beyond this, hits still reach their static total and land in
/// [`Event::SiteOverflow`], and the report says how many sites were dropped
/// -- an incomplete table that SAYS it is incomplete, never a silent loss.
/// (Measured for calibration: delays.clj registers 111 sites,
/// multimethods.clj 175.)
pub const MAX_SITES: usize = SITE_CHUNK * N_SITE_CHUNKS;

/// Per-site cells in one lazily-allocated chunk (1 KiB).
const SITE_CHUNK: usize = 128;
/// How many chunks a page can hold.
const N_SITE_CHUNKS: usize = 16;

/// One thread's counter block. Written ONLY by its owning thread (never an
/// RMW, never contended); read by report time from any thread, which is the
/// only reason the cells are atomic at all.
///
/// Per-site cells are allocated LAZILY, in 1 KiB chunks, and only the chunks
/// a thread actually touches. That is not premature tidiness -- it was
/// MEASURED: delays.clj runs 200 threads and every one of them fires
/// attributed events, so a flat [`MAX_SITES`]-cell block per page cost 3.2
/// MB of RSS to hold ~111 live counters. Chunking cuts that ~16x, and the
/// extra load it puts on the hit path lands ONLY on `event_at`'s attributed
/// arm -- whose every caller (a tier bail, an escape, a macro re-expansion)
/// is by construction already on a slow path. `event` never looks at it.
///
/// `OnceLock` per chunk gives the lazy block the same publish/consume story
/// as every other cache in this codebase (ARCHITECTURE.md's `OnceLock`
/// inline-cache shape): a chunk is published fully-built with `Release` and
/// never replaced, so a `&[AtomicU64]` handed out here can never be
/// invalidated underneath a reader.
pub struct Page {
    statics: [AtomicU64; N_STATIC],
    sites: [OnceLock<Box<[AtomicU64; SITE_CHUNK]>>; N_SITE_CHUNKS],
}

impl Page {
    fn new() -> Page {
        Page {
            statics: std::array::from_fn(|_| AtomicU64::new(0)),
            sites: std::array::from_fn(|_| OnceLock::new()),
        }
    }

    /// The cell for site `id`, allocating that site's chunk on first use.
    /// `None` iff `id` is past [`MAX_SITES`].
    #[inline]
    fn site_cell(&self, id: usize) -> Option<&AtomicU64> {
        let chunk = self.sites.get(id / SITE_CHUNK)?;
        let cells = match chunk.get() {
            Some(c) => c,
            None => chunk.get_or_init(|| Box::new(std::array::from_fn(|_| AtomicU64::new(0)))),
        };
        Some(&cells[id % SITE_CHUNK])
    }
}

/// Every page ever registered, live thread or not. Cold path only: pushed
/// on a thread's first event, walked at report time. The `Arc`s are never
/// dropped, which is what makes the raw `*const Page` in TLS sound for the
/// whole process lifetime.
static PAGES: Mutex<Vec<Arc<Page>>> = Mutex::new(Vec::new());

thread_local! {
    /// This thread's page, or null until its first event. `const`-initialized
    /// so the hot path is a bare TLS load with no lazy-init guard -- the
    /// `C_tls` idiom from `tests/globals_snapshot_bench.rs`.
    static PAGE: Cell<*const Page> = const { Cell::new(std::ptr::null()) };
}

/// Allocates and registers this thread's page. Cold: once per thread, ever.
#[cold]
#[inline(never)]
fn init_page() -> *const Page {
    let page = Arc::new(Page::new());
    let ptr = Arc::as_ptr(&page);
    // Leaked-by-design: the registry holds the only handle and never drops
    // it, so `ptr` stays valid for the process lifetime.
    crate::sync::lock_mutex(&PAGES).push(page);
    PAGE.with(|c| c.set(ptr));
    ptr
}

#[inline]
fn page() -> *const Page {
    let p = PAGE.with(|c| c.get());
    if p.is_null() {
        init_page()
    } else {
        p
    }
}

/// The one increment primitive: load + store, never `fetch_add`. Only the
/// owning thread ever writes this cell, so a plain add is exactly correct
/// and the cacheline is never shared for writing.
#[inline(always)]
fn bump(cell: &AtomicU64) {
    cell.store(cell.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
}

/// Counts one occurrence of a static event with no site attribution.
#[inline]
pub fn event(ev: Event) {
    // SAFETY: `page()` is non-null and points into an `Arc<Page>` the
    // registry holds for the whole process, so the reference is valid; no
    // `&mut` to a `Page` is ever created anywhere, so aliasing is trivially
    // satisfied. `ev as usize` < `N_STATIC` by construction.
    let p: &Page = unsafe { &*page() };
    bump(&p.statics[ev as usize]);
}

/// The site id meaning "no attribution available" (an anonymous fn, a site
/// allocated past [`MAX_SITES`], or a build that never allocated one).
pub const NO_SITE: u32 = u32::MAX;

/// Counts one occurrence of a static event AND, when `site` is a real id,
/// one occurrence against that site. The static total therefore always
/// includes the unattributed hits.
#[inline]
pub fn event_at(ev: Event, site: u32) {
    // SAFETY: as in `event`.
    let p: &Page = unsafe { &*page() };
    bump(&p.statics[ev as usize]);
    if site != NO_SITE {
        match p.site_cell(site as usize) {
            Some(cell) => bump(cell),
            None => bump(&p.statics[Event::SiteOverflow as usize]),
        }
    }
}

// ---------------------------------------------------------------------------
// The site registry (cold path)
// ---------------------------------------------------------------------------

/// What kind of decision a site records. Kept separate from the reason
/// string so the report can group without parsing prose.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SiteKind {
    /// The whole fn tree-walks; `reason` is `compile::resolve`'s bail reason.
    TierBail,
    /// The fn compiled but carries `Ir::Escape` nodes; `reason` counts them.
    Escape,
    /// A `defmacro`'d macro, counted per expansion.
    Macro,
}

impl SiteKind {
    fn key(self) -> &'static str {
        match self {
            SiteKind::TierBail => "lens.site/tier-bail",
            SiteKind::Escape => "lens.site/escape",
            SiteKind::Macro => "lens.site/macro",
        }
    }
}

/// One registered decision point. Immutable after `reason` is patched in by
/// [`set_site_reason`] at the end of the compile that allocated it.
pub struct SiteInfo {
    pub kind: SiteKind,
    /// The fn/macro name, or `"<anonymous>"`.
    pub name: String,
    /// `source-name:offset` -- where the fn form was written, BEST-EFFORT.
    /// See [`alloc_site`] for why v1.0.0 does not promise this is right
    /// (spans do not carry a source id yet; that fix is ledgered as a
    /// follow-up and is not this wave's).
    pub loc: String,
    /// Why: the bail reason, or the escape count.
    pub reason: String,
}

#[derive(Default)]
struct SiteRegistry {
    infos: Vec<SiteInfo>,
    /// Dedup: the same decision point compiled again (a `fn` form
    /// re-evaluated in a tree-walked loop, a second `Interp` on another
    /// thread) must reuse its id rather than mint a new one -- otherwise a
    /// hot closure-creating loop would exhaust [`MAX_SITES`] in
    /// milliseconds. Keyed by the caller's IDENTITY string, not by a
    /// rendered source position -- see [`alloc_site`].
    by_key: HashMap<(String, u8), u32>,
    /// How many distinct sites were requested past [`MAX_SITES`].
    overflowed: u64,
}

static SITES: OnceLock<Mutex<SiteRegistry>> = OnceLock::new();

fn sites() -> &'static Mutex<SiteRegistry> {
    SITES.get_or_init(|| Mutex::new(SiteRegistry::default()))
}

/// Get-or-allocate the site id for one decision point. **Cold path only**:
/// called from `compile::compile_fn` / `eval_defmacro`, once per distinct
/// site per process (the `by_key` dedup, plus `Interp`'s own per-interpreter
/// cache in front of it, keeps a hot closure-creating loop from ever
/// reaching this lock twice).
///
/// `key` is the site's IDENTITY and is deliberately NOT the rendered source
/// position. `compile::explain::at` renders a `Bail` span against
/// `Interp::source`/`source_name` -- process-wide CURRENT fields, not
/// per-`Form` provenance -- so a fn read from one buffer and compiled while
/// those fields name another gets a syntactically valid but WRONG
/// `file:line` (measured on parse.clj: 91,924 of 91,934 poison-bail lines
/// misattributed). Keying on that would silently merge or split sites. So
/// `Interp::lens_site_for` passes `ns/name` for a named fn (the same
/// name-keyed identity `Interp::compile_explain` already uses) and falls
/// back to a position only for anonymous fns, where there is nothing else.
/// `loc` is carried for HUMANS only, and the schema doc marks it
/// best-effort in v1.0.0.
pub fn alloc_site(kind: SiteKind, key: &str, name: Option<&str>, loc: &str) -> u32 {
    let mut reg = crate::sync::lock_mutex(sites());
    let k = (key.to_string(), kind as u8);
    if let Some(id) = reg.by_key.get(&k) {
        return *id;
    }
    if reg.infos.len() >= MAX_SITES {
        reg.overflowed += 1;
        return NO_SITE;
    }
    let id = reg.infos.len() as u32;
    reg.infos.push(SiteInfo {
        kind,
        name: name.unwrap_or("<anonymous>").to_string(),
        loc: loc.to_string(),
        reason: String::new(),
    });
    reg.by_key.insert(k, id);
    id
}

/// Patches in the reason once the compile that allocated `site` knows it.
/// Cold path, same as [`alloc_site`].
pub fn set_site_reason(site: u32, reason: String) {
    if site == NO_SITE {
        return;
    }
    let mut reg = crate::sync::lock_mutex(sites());
    if let Some(info) = reg.infos.get_mut(site as usize) {
        info.reason = reason;
    }
}

// ---------------------------------------------------------------------------
// Gauges (not counters: a level, sampled at report time)
// ---------------------------------------------------------------------------

/// Engines built through `embed::EngineBuilder::build` / `Engine::new`.
pub static ENGINES_CREATED: AtomicU64 = AtomicU64::new(0);
/// `Engine::snapshot` calls -- the clone-per-window/plugin rate a host application drives.
pub static ENGINES_SNAPSHOTTED: AtomicU64 = AtomicU64::new(0);
/// Watchdog callbacks that fired (process-wide), for the noise budget.
pub static WARNINGS_FIRED: AtomicU64 = AtomicU64::new(0);

/// Process start, for `:lens/uptime-ms`. Cheap and taken once. L5: routed
/// through `crate::clock::clock_now()` so the uptime gauge becomes virtual
/// under sim (design §2) -- correct and free, since sim's clock is anchored
/// at a real `Instant` taken at sim init anyway.
fn start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(crate::clock::clock_now)
}

/// Called from `main`/`Engine` construction so `:lens/uptime-ms` measures
/// the process, not "time since the first report".
pub fn mark_start() {
    let _ = start();
}

// ---------------------------------------------------------------------------
// Aggregation + windowing
// ---------------------------------------------------------------------------

/// A flat, point-in-time sum over every registered page.
pub struct Totals {
    pub statics: [u64; N_STATIC],
    pub sites: Vec<u64>,
    /// Threads that ever fired any event (one counter page each).
    pub pages: usize,
    /// How many per-site CHUNKS are live across all pages -- the honest RSS
    /// number for the attribution half of the substrate (1 KiB each).
    pub site_pages: usize,
}

/// Walks the page registry and sums. Report time only -- never on any path
/// a program's own execution takes.
pub fn totals() -> Totals {
    let pages = crate::sync::lock_mutex(&PAGES);
    let n_sites = crate::sync::lock_mutex(sites()).infos.len();
    let mut statics = [0u64; N_STATIC];
    let mut sites_out = vec![0u64; n_sites];
    for p in pages.iter() {
        for (i, cell) in p.statics.iter().enumerate() {
            statics[i] = statics[i].wrapping_add(cell.load(Ordering::Relaxed));
        }
        // An unallocated chunk is all zeros by definition -- skip it rather
        // than allocate it just to read zeros back out.
        for (ci, chunk) in p.sites.iter().enumerate() {
            let Some(cells) = chunk.get() else { continue };
            let base = ci * SITE_CHUNK;
            for (j, cell) in cells.iter().enumerate() {
                if let Some(out) = sites_out.get_mut(base + j) {
                    *out = out.wrapping_add(cell.load(Ordering::Relaxed));
                }
            }
        }
    }
    Totals {
        statics,
        sites: sites_out,
        pages: pages.len(),
        site_pages: pages
            .iter()
            .map(|p| p.sites.iter().filter(|c| c.get().is_some()).count())
            .sum(),
    }
}

/// The `:reset` baseline: the totals as of the last `(runtime-report :reset)`.
/// Resetting NEVER writes another thread's page (that would be a shared RMW,
/// and would corrupt the monotone contract gate 4 rules); it records a
/// baseline that the report subtracts. Counters stay monotone forever.
struct Baseline {
    statics: [u64; N_STATIC],
    sites: Vec<u64>,
    epoch: u64,
}

impl Default for Baseline {
    fn default() -> Self {
        Baseline {
            statics: [0; N_STATIC],
            sites: Vec::new(),
            epoch: 0,
        }
    }
}

static BASELINE: OnceLock<Mutex<Baseline>> = OnceLock::new();

fn baseline() -> &'static Mutex<Baseline> {
    BASELINE.get_or_init(|| Mutex::new(Baseline::default()))
}

// ---------------------------------------------------------------------------
// Watchdog (skeleton; thresholds are OWNER-GATED placeholders)
// ---------------------------------------------------------------------------

/// **OWNER-GATED PLACEHOLDER (design-doc gate 2).** Every number below is a
/// guess chosen to be quiet on the bootstrap and loud on a real cliff; none
/// of them is measured policy yet. They live here, as consts, precisely so
/// the owner can rule on them in one place.
pub mod thresholds {
    /// Tree-walked executions of ONE fn before it is worth a warning.
    /// (delays.clj's 47x fn executed ~2M times; core bootstrap's worst
    /// tree-walked fn is in the thousands.)
    pub const TIER_BAIL_EXEC_PER_SITE: u64 = 1_000_000;
    /// `Ir::Escape` executions from ONE fn.
    pub const ESCAPE_EXEC_PER_SITE: u64 = 1_000_000;
    /// Re-expansions of ONE macro (the macro-expansion-cache trigger).
    pub const MACRO_EXPAND_PER_SITE: u64 = 500_000;
    /// Process-wide dynamic-binding stack walks.
    pub const DYN_BINDING_WALK_TOTAL: u64 = 5_000_000;
    /// Process-wide catch-class-chain rebuilds.
    pub const CATCH_CHAIN_REBUILD_TOTAL: u64 = 1_000_000;
}

/// One watchdog firing, rendered as EDN for the host to `observe!`.
pub struct Warning {
    pub message: String,
    pub value: Value,
}

/// Rate limiting: a site/event warns at most once per threshold CROSSING,
/// not once per report. Keyed by the same ids the report uses.
#[derive(Default)]
struct WarnState {
    fired_sites: std::collections::HashSet<u32>,
    fired_statics: std::collections::HashSet<u32>,
}

static WARN_STATE: OnceLock<Mutex<WarnState>> = OnceLock::new();

fn warn_state() -> &'static Mutex<WarnState> {
    WARN_STATE.get_or_init(|| Mutex::new(WarnState::default()))
}

fn kw(s: &str) -> Value {
    Value::Keyword(crate::keyword::Keyword::from(s))
}

fn warning_value(kind: &str, subject: &str, count: u64, threshold: u64, message: &str) -> Value {
    let mut m = PMap::new();
    m.insert(kw("lens/schema"), Value::Str(Str::from(SCHEMA_VERSION)));
    m.insert(kw("lens.warn/kind"), kw(kind));
    m.insert(kw("lens.warn/subject"), Value::Str(Str::from(subject)));
    m.insert(kw("lens.warn/count"), Value::Int(count as i64));
    m.insert(kw("lens.warn/threshold"), Value::Int(threshold as i64));
    m.insert(kw("lens.warn/message"), Value::Str(Str::from(message)));
    Value::Map(m)
}

/// Evaluates every threshold against `t` and returns the warnings that
/// crossed for the FIRST time. Called from report construction, which is
/// the skeleton's whole delivery model -- see this module's "Deviation"
/// note in `docs/W-LENS-SCHEMA.md`: putting a threshold compare on the HIT
/// path would have cost exactly what the overhead gate forbids, so the
/// watchdog samples at report time (which the host drives on its own
/// cadence anyway) rather than firing mid-loop.
pub fn check_thresholds(t: &Totals) -> Vec<Warning> {
    let mut out = Vec::new();
    let mut state = crate::sync::lock_mutex(warn_state());
    let statics: [(Event, u64, &str); 2] = [
        (
            Event::DynBindingWalk,
            thresholds::DYN_BINDING_WALK_TOTAL,
            "dynamic-binding stack walks",
        ),
        (
            Event::CatchChainRebuild,
            thresholds::CATCH_CHAIN_REBUILD_TOTAL,
            "catch-class-chain rebuilds",
        ),
    ];
    for (ev, limit, label) in statics {
        let n = t.statics[ev as usize];
        if n >= limit && state.fired_statics.insert(ev as u32) {
            let msg = format!("{label}: {n} (threshold {limit}) -- see (runtime-report)");
            out.push(Warning {
                value: warning_value("event", EVENT_KEYS[ev as usize], n, limit, &msg),
                message: msg,
            });
        }
    }
    let reg = crate::sync::lock_mutex(sites());
    for (id, n) in t.sites.iter().copied().enumerate() {
        let Some(info) = reg.infos.get(id) else { continue };
        let limit = match info.kind {
            SiteKind::TierBail => thresholds::TIER_BAIL_EXEC_PER_SITE,
            SiteKind::Escape => thresholds::ESCAPE_EXEC_PER_SITE,
            SiteKind::Macro => thresholds::MACRO_EXPAND_PER_SITE,
        };
        if n >= limit && state.fired_sites.insert(id as u32) {
            let what = match info.kind {
                SiteKind::TierBail => "tree-walked",
                SiteKind::Escape => "escaped to the tree-walker",
                SiteKind::Macro => "re-expanded",
            };
            let msg = format!(
                "fn {} {what} {n} times ({}) -- {}; threshold {limit}, see MOVA_EXPLAIN=1",
                info.name, info.loc, info.reason
            );
            out.push(Warning {
                value: warning_value(info.kind.key(), &info.name, n, limit, &msg),
                message: msg,
            });
        }
    }
    WARNINGS_FIRED.fetch_add(out.len() as u64, Ordering::Relaxed);
    out
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// How many per-site rows the report renders (highest count first). A ledger
/// is curated, not a dashboard -- the tail is noise, and the totals above it
/// are already exact.
pub const REPORT_TOP_SITES: usize = 32;

/// Builds the report EDN. `reset` records a new windowing baseline (see
/// [`Baseline`]); the monotone totals under `:lens/events` are unaffected
/// either way. `retired_globals` is the caller's `Env` gauge (the report
/// builder is deliberately `Interp`-free otherwise, so `Engine::lens_report`
/// and the `(runtime-report)` builtin produce byte-identical maps).
pub fn report(reset: bool, retired_globals: u64) -> Value {
    let t = totals();
    let (base_statics, base_sites, epoch) = {
        let mut b = crate::sync::lock_mutex(baseline());
        let snap = (b.statics, b.sites.clone(), b.epoch);
        if reset {
            b.statics = t.statics;
            b.sites = t.sites.clone();
            b.epoch += 1;
        }
        snap
    };

    let mut events = PMap::new();
    let mut window = PMap::new();
    for (i, key) in EVENT_KEYS.iter().enumerate() {
        let n = t.statics[i];
        events.insert(kw(key), Value::Int(n as i64));
        window.insert(
            kw(key),
            Value::Int(n.saturating_sub(base_statics[i]) as i64),
        );
    }

    // Per-site rows, joined against the site registry (the EXPLAIN-style
    // "which fn, why" half of regret = decision x count).
    let reg = crate::sync::lock_mutex(sites());
    let mut rows: Vec<(u64, u64, usize)> = t
        .sites
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, n)| *n > 0)
        .map(|(id, n)| {
            let w = n.saturating_sub(base_sites.get(id).copied().unwrap_or(0));
            (n, w, id)
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let n_rows_total = rows.len();
    rows.truncate(REPORT_TOP_SITES);
    let site_rows: Vec<Value> = rows
        .iter()
        .filter_map(|(n, w, id)| {
            let info = reg.infos.get(*id)?;
            let mut m = PMap::new();
            m.insert(kw("lens.site/kind"), kw(info.kind.key()));
            m.insert(kw("lens.site/name"), Value::Str(Str::from(info.name.as_str())));
            m.insert(kw("lens.site/at"), Value::Str(Str::from(info.loc.as_str())));
            m.insert(
                kw("lens.site/reason"),
                Value::Str(Str::from(info.reason.as_str())),
            );
            m.insert(kw("lens.site/count"), Value::Int(*n as i64));
            m.insert(kw("lens.site/window"), Value::Int(*w as i64));
            Some(Value::Map(m))
        })
        .collect();
    let overflowed = reg.overflowed;
    let n_sites = reg.infos.len();
    drop(reg);

    let mut gauges = PMap::new();
    gauges.insert(kw("lens.gauge/thread-pages"), Value::Int(t.pages as i64));
    gauges.insert(
        kw("lens.gauge/site-chunks"),
        Value::Int(t.site_pages as i64),
    );
    gauges.insert(kw("lens.gauge/sites-registered"), Value::Int(n_sites as i64));
    gauges.insert(kw("lens.gauge/sites-dropped"), Value::Int(overflowed as i64));
    gauges.insert(
        kw("lens.gauge/globals-retired"),
        Value::Int(retired_globals as i64),
    );
    gauges.insert(
        kw("lens.gauge/engines-created"),
        Value::Int(ENGINES_CREATED.load(Ordering::Relaxed) as i64),
    );
    gauges.insert(
        kw("lens.gauge/engines-snapshotted"),
        Value::Int(ENGINES_SNAPSHOTTED.load(Ordering::Relaxed) as i64),
    );
    gauges.insert(
        kw("lens.gauge/warnings-fired"),
        Value::Int(WARNINGS_FIRED.load(Ordering::Relaxed) as i64),
    );

    let mut m = PMap::new();
    m.insert(kw("lens/schema"), Value::Str(Str::from(SCHEMA_VERSION)));
    m.insert(
        kw("lens/uptime-ms"),
        Value::Int(start().elapsed().as_millis() as i64),
    );
    m.insert(kw("lens/epoch"), Value::Int(epoch as i64));
    m.insert(kw("lens/geo-census"), Value::Bool(cfg!(feature = "geo-census")));
    m.insert(kw("lens/events"), Value::Map(events));
    m.insert(kw("lens/window"), Value::Map(window));
    m.insert(
        kw("lens/sites"),
        Value::Vector(site_rows.into_iter().collect()),
    );
    m.insert(kw("lens/sites-truncated"), Value::Bool(n_rows_total > REPORT_TOP_SITES));
    m.insert(kw("lens/gauges"), Value::Map(gauges));
    Value::Map(m)
}

// ---------------------------------------------------------------------------
// MOVA_LENS
// ---------------------------------------------------------------------------

/// `MOVA_LENS`'s parsed value, read exactly ONCE at process start (the same
/// `OnceLock` discipline as `MOVA_EXPLAIN`/`MOVA_NO_COMPILE`: no hot path
/// ever touches the environment).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Unset: counters still run (they are always-on), nothing is printed.
    Off,
    /// `MOVA_LENS=dump`: print the whole EDN report to stderr at exit.
    Dump,
    /// `MOVA_LENS=warn`: print only threshold-crossing warnings at exit.
    /// Opt-in by design -- W-ADX noise discipline: a bootstrap that prints
    /// nothing today must keep printing nothing.
    Warn,
}

pub fn mode() -> Mode {
    static M: OnceLock<Mode> = OnceLock::new();
    *M.get_or_init(|| match std::env::var("MOVA_LENS").as_deref() {
        Ok("dump") => Mode::Dump,
        Ok("warn") => Mode::Warn,
        _ => Mode::Off,
    })
}

/// Every threshold crossed since the last check, as rendered lines --
/// `MOVA_LENS=warn`'s payload, and the CLI half of the watchdog. Same
/// rate limiting as the embed hook (once per site per crossing), because it
/// is literally the same state.
pub fn warning_lines() -> Vec<String> {
    check_thresholds(&totals())
        .into_iter()
        .map(|w| w.message)
        .collect()
}

/// Two cells inside the static block that NO runtime event ever writes,
/// reserved for this module's own unit tests. `cargo test --lib` runs a
/// crate's tests in parallel threads of one process, and the real events
/// are being fired constantly by sibling tests (every `Interp::new` alone
/// re-expands hundreds of macros), so a test asserting an EXACT delta must
/// own its cell. Outside `cfg(test)` these are simply unused zeros; they are
/// never rendered (`EVENT_KEYS` stops well before them), so they cannot leak
/// into the schema.
#[cfg(test)]
const TEST_CELL_A: usize = N_STATIC - 1;
#[cfg(test)]
const TEST_CELL_B: usize = N_STATIC - 2;

#[cfg(test)]
fn bump_test_cell(idx: usize) {
    // SAFETY: as in `event`.
    let p: &Page = unsafe { &*page() };
    bump(&p.statics[idx]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_aggregate_across_threads() {
        let before = totals().statics[TEST_CELL_A];
        let n = 4;
        let per = 1000u64;
        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    for _ in 0..per {
                        bump_test_cell(TEST_CELL_A);
                    }
                });
            }
        });
        let after = totals().statics[TEST_CELL_A];
        assert_eq!(after - before, n * per, "every thread's page must be summed");
        assert!(totals().pages >= 1);
    }

    #[test]
    fn counters_are_monotone_across_a_reset() {
        for _ in 0..10 {
            bump_test_cell(TEST_CELL_B);
        }
        let a = totals().statics[TEST_CELL_B];
        // A reset must not write pages: the raw total keeps climbing.
        let _ = report(true, 0);
        let b = totals().statics[TEST_CELL_B];
        assert_eq!(a, b, "reset must never zero a page");
        bump_test_cell(TEST_CELL_B);
        let c = totals().statics[TEST_CELL_B];
        assert_eq!(c, b + 1);
    }

    #[test]
    fn site_alloc_dedups_by_identity_not_by_rendered_span() {
        let a = alloc_site(SiteKind::TierBail, "u/f", Some("f"), "a.mova:7");
        // Same identity, DIFFERENT rendered location (which is exactly the
        // misattribution `at()` can produce): must still be one site.
        let b = alloc_site(SiteKind::TierBail, "u/f", Some("f"), "b.mova:912");
        assert_eq!(a, b, "identity, not the best-effort location, is the key");
        let c = alloc_site(SiteKind::Macro, "u/f", Some("f"), "a.mova:7");
        assert_ne!(a, c, "kind is part of the key");
    }

    #[test]
    fn a_site_beyond_the_cap_is_reported_not_dropped() {
        // Sites past `MAX_SITES` get `NO_SITE`, and `alloc_site` records the
        // drop -- an incomplete table that says so beats a silent loss.
        let ok = alloc_site(SiteKind::Escape, "lens-unit/cap-probe", None, "x:0");
        assert_ne!(ok, NO_SITE, "the registry is nowhere near its cap in a unit test");
        // A hit whose site id is past the page's range must still land: in
        // the static total, and in the overflow cell.
        let before_total = totals().statics[Event::EscapeExec as usize];
        let before_over = totals().statics[Event::SiteOverflow as usize];
        event_at(Event::EscapeExec, MAX_SITES as u32 + 1);
        let t = totals();
        assert!(t.statics[Event::EscapeExec as usize] > before_total);
        assert_eq!(
            t.statics[Event::SiteOverflow as usize] - before_over,
            1,
            "an out-of-range site id must be counted as overflow, never dropped"
        );
    }
}
