//! W-GEO stage 1: capped-permanent keyword interning
//! (`docs/W-GEO-STAGE1-DESIGN.md`, owner-ruled 2026-08-22 —
//! `docs/W-GEO-PROBE-VERDICTS.md`'s "Owner rulings 2026-08-22" appendix).
//!
//! [`Value::Keyword`](crate::value::Value::Keyword)'s payload used to be a
//! bare [`Str`]. It is now a [`Keyword`]: either an `Interned(u32)` id into
//! the process-wide table below (the fast, common case — equality is a
//! `u32` compare, `clone` is a register copy with no refcount traffic at
//! all), or an `Overflow(Arc<Str>)` that is byte-for-byte the OLD shape,
//! used for text that arrived after the table froze at its cap.
//!
//! # Why this shape (shape "A", §1.2 of the design doc)
//!
//! `size_of::<Keyword>() == 16`, exactly what `Str` cost before — this
//! stage is *size-neutral* and does not shrink `Value` (still 72; the
//! `PVec`/`PMap` inline roots are the driver, and boxing them is stage 4).
//! What it buys is the operations Probe D measured a keyword-heavy runtime
//! actually repeats: `Interned`/`Interned` equality ~3x cheaper than
//! content compare. The alternative shape (B) — one manually tagged
//! `usize`, 8 bytes, `host_struct::ic_pack`-style — is deferred to stage 5
//! behind its own round-trip kill-probe.
//!
//! `Arc<Str>` is a THIN (8-byte) pointer despite `Str` being 16 bytes,
//! because `Str` is `Sized`; that is what lets the `Overflow` arm carry a
//! full `Str` — with all of `Str`'s existing rope/flat/ascii-cache/`ptr_eq`
//! machinery reused unchanged — for the cost of one pointer.
//!
//! # Text access goes through ONE chokepoint
//!
//! No call site outside this module pattern-matches [`Keyword`]'s variants.
//! Everything that needs the characters goes through [`Keyword::text_ref`]
//! (borrowing) or [`Keyword::text`] (owning) — and the `Deref`/`AsRef`/
//! `Display`/`Ord`/`Hash` impls at the bottom of this file are all thin
//! wrappers over `text_ref`, so the overwhelming majority of the ~330
//! pre-existing keyword call sites in this crate kept compiling verbatim.
//!
//! # Relationship to `KeywordRegistry` — two tables, two questions
//!
//! [`crate::value::KeywordRegistry`] (per-`Interp`, `interp.keywords`,
//! backing `find-keyword`) is NOT this table and is deliberately untouched
//! by stage 1. It answers "has *this interpreter's script* ever constructed
//! this exact keyword", which is genuine per-engine isolation
//! (`Engine::snapshot` still resets it). This table answers "what number
//! does this text map to" — a pure text-identity question with no
//! per-engine semantic content, so it is a process-wide `static`, exactly
//! like `env::GLOBAL_GENERATION`. Neither `fork` nor `snapshot` needs a
//! verb for it: it was never an `Interp` field.

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::sync::lock_mutex;
use crate::value::Str;

/// The maximum number of DISTINCT keyword texts that will ever be given an
/// interned id, process-wide, for the whole life of the process.
///
/// A compile-time `const`, not a runtime knob (YAGNI until a concrete
/// embedder need for tuning surfaces; threading a cap parameter through
/// later is a small, localized change).
///
/// Rationale (design doc §2.1, grounded in Probe D's measured numbers):
/// Probe D's ~2.9KB/keyword prices the cost of GROWING a permanent table,
/// dominated by the retired-snapshot list — one retained path-copy per NEW
/// keyword ever interned. Used as a deliberately pessimistic upper bound,
/// `65_536 × 2,900B ≈ 190MB` is the worst-case peak the table can ever
/// reach, once, ever, for the process's entire life. That is *bounded*,
/// which is the entire point of "capped": the unbounded-RSS hazard a naive
/// intern-forever table poses to a long-lived hosted-editor engine cannot
/// recur here, because once the cap is hit there are no more inserts, so no
/// more publishes, so no more retirement. 1M was rejected by the same
/// arithmetic (~2.9GB peak); a much tighter cap (1,000) was rejected
/// because 65,536 sits far above the distinct-keyword vocabulary of an
/// ordinary mova program (hundreds, measured across this repo's own
/// `bench/*.mova` + vendored suite corpus) while staying a known, bounded,
/// disclosed number rather than an arbitrarily large one picked "to be
/// safe".
///
/// `u32` ids are used regardless of the cap fitting in `u16`: shape (A)'s
/// `Interned(u32)` costs the same 16 bytes `Interned(u16)` would (the enum
/// is padded to the pointer-sized `Overflow` slot either way), so there is
/// no size reason to narrow the id, and `u32` leaves headroom to raise the
/// cap later without an id-width migration.
pub const KEYWORD_INTERN_CAP: u32 = 65_536;

/// `Value::Keyword`'s payload. See this module's doc.
///
/// `Interned` is the fast, common case (id compare, `Copy`-cheap clone, no
/// text access at all unless the keyword is printed / `name`d / sorted);
/// `Overflow` is exactly the pre-stage-1 `Value::Keyword(Str)` shape,
/// unchanged, for text that missed the capped table.
#[derive(Clone)]
pub enum Keyword {
    Interned(u32),
    Overflow(Arc<Str>),
}

impl Keyword {
    /// Get-or-intern `text` against the process-wide table, falling back to
    /// a plain heap-allocated `Overflow` once the table is frozen at
    /// [`KEYWORD_INTERN_CAP`]. THE construction entry point — every site
    /// that used to write `Value::Keyword(Str::from(text))` now routes
    /// here (directly or through one of the `From` impls below).
    ///
    /// This is step 1 of §2.2's discipline (the lock-free probe, which
    /// allocates nothing at all on a hit — including no `Str`); steps 2-5
    /// live in [`InternTable::insert_or_overflow`], where the invariants
    /// are documented.
    pub fn construct(text: &str) -> Keyword {
        let h = content_hash(text);
        // Wave 4: L1 first -- one array index + text compare, no atomic
        // load, no CHAMP-map traversal. See the L1 section's doc above
        // `l1_lookup` for why a hit here is always correct forever.
        if let Some(kw) = l1_lookup(text, h) {
            return kw;
        }
        let t = table();
        if let Some((text_arc, id)) = t.map().find(text, h) {
            l1_store(h, id, text_arc);
            return Keyword::Interned(id);
        }
        let kw = t.insert_or_overflow(Str::from(text), h);
        // Populate L1 for the cold path too (a fresh insert, or a hit only
        // found after taking the writer lock) -- `Overflow` is deliberately
        // never cached (see the L1 section's doc: L1 only ever holds
        // `Interned` ids).
        if let Keyword::Interned(id) = kw {
            l1_store(h, id, Arc::from(text));
        }
        kw
    }

    /// [`Keyword::construct`], reusing `owned`'s existing allocation rather
    /// than rebuilding one, for the call sites that already hold a `Str`
    /// (`host_struct` shape-field keys, `Str`→keyword builtins). On a table
    /// HIT `owned` is simply dropped and the canonical entry wins — which
    /// is what makes the `host_struct` inline cache work (see
    /// [`Keyword::text_ref`]'s note on allocation identity); on the first
    /// MISS for this text, `owned` itself becomes the canonical entry, so
    /// e.g. a `Shape`'s field-key `Str` and the table's copy of it are one
    /// allocation and the IC hits on its very first lookup.
    pub fn from_owned(owned: Str) -> Keyword {
        // `as_ref` is O(1)/zero-copy for a `Flat` `Str` (and keyword text
        // is always far below the rope threshold in practice); a `Rope`
        // pays its one cached materialize here, "materialize with care".
        let h = content_hash(owned.as_ref());
        if let Some(kw) = l1_lookup(owned.as_ref(), h) {
            return kw;
        }
        let t = table();
        if let Some((text_arc, id)) = t.map().find(owned.as_ref(), h) {
            l1_store(h, id, text_arc);
            return Keyword::Interned(id);
        }
        // Built BEFORE `insert_or_overflow` consumes `owned` by value.
        let text_for_l1: Arc<str> = Arc::from(owned.as_ref());
        let kw = t.insert_or_overflow(owned, h);
        if let Keyword::Interned(id) = kw {
            l1_store(h, id, text_for_l1);
        }
        kw
    }

    /// THE text chokepoint (design doc §1.3), in its borrowing form — the
    /// primitive the owning [`Keyword::text`] and every trait impl below is
    /// written in terms of.
    ///
    /// # Why an `Interned` keyword can hand out a borrow at all
    ///
    /// The table is a process-wide `static` whose superseded snapshots are
    /// RETIRED, never freed (see [`InternTable`]), and which itself never
    /// drops. A `&Str` reached through any published snapshot is therefore
    /// valid for `'static`; the elided signature only claims `&self`'s
    /// (shorter) lifetime, so no call site can observe the difference.
    ///
    /// # Allocation identity (why `host_struct`'s IC needed ZERO changes)
    ///
    /// For a given `Interned` id this always returns a reference to the ONE
    /// canonical `Str` stored in `by_id` — the same allocation, hence the
    /// same [`Str::identity_addr`], on every call, forever. `host_struct`'s
    /// per-`Shape` inline cache keys on exactly that address, so its ptr-eq
    /// fast path keeps working unchanged (and in fact gets *better*: two
    /// separately-constructed `:foo`s used to be two addresses and burned
    /// two IC slots; now they are one). `Overflow` keywords were always
    /// address-stable per value by construction.
    #[inline]
    pub fn text_ref(&self) -> &Str {
        match self {
            Keyword::Interned(id) => table().text_of(*id),
            Keyword::Overflow(s) => s,
        }
    }

    /// The owning form of [`Keyword::text_ref`] — one `Arc` bump, sharing
    /// the canonical allocation (NOT a fresh copy of the characters).
    #[inline]
    pub fn text(&self) -> Str {
        self.text_ref().clone()
    }

    /// This keyword's interned id, or `None` for an `Overflow`. Exposed for
    /// tests and for future stages that want the id as a key; deliberately
    /// NOT used by equality/hash for anything but the `Interned`/`Interned`
    /// fast path (see [`PartialEq`]/[`Hash`] below).
    #[inline]
    pub fn interned_id(&self) -> Option<u32> {
        match self {
            Keyword::Interned(id) => Some(*id),
            Keyword::Overflow(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// One published snapshot of the intern table.
///
/// Both halves are persistent/structurally-shared, and that is load-bearing
/// rather than stylistic — see [`InternInner::by_id`].
struct InternInner {
    /// content-hash → collision list of `(text, id)`.
    ///
    /// Bucketed by a `u64` content hash rather than keyed by the text
    /// itself because `champ::PersistentHashMap::get` takes `&K`
    /// exactly (no `Borrow<str>`-style unsized lookup), so keying by an
    /// owned string type would force allocating a throwaway key just to
    /// LOOK UP an existing entry — defeating the entire point of a
    /// lock-free hit path. `u64` is `Copy`; a hit allocates nothing.
    by_hash: champ::PersistentHashMap<u64, Vec<(Arc<str>, u32)>>,
    /// id → canonical text; append-only, index == id.
    ///
    /// **Must** be `imbl::Vector`, not `std::Vec`. Probe D's own module doc
    /// (`tests/geo_intern_probe.rs`) records the exact bug this avoids: an
    /// earlier draft used a plain `Vec`, cloned whole on every insert (O(n)
    /// per op) and — because every retired snapshot pins its own
    /// full-length copy — O(n²) TOTAL retained bytes across N inserts,
    /// which produced a first, wrong reading of ~900MB RSS / ~230us per
    /// miss before being fixed. `imbl::Vector::clone` is O(1) (structural
    /// sharing), so a retired snapshot pins only the handful of nodes its
    /// OWN insert touched. This is not a style preference; it is the
    /// difference between the design working and not.
    by_id: imbl::Vector<Str>,
}

impl InternInner {
    /// Wave 4: returns the bucket's own `Arc<str>` handle alongside the id
    /// (a refcount bump, not a content copy) rather than just the `u32` --
    /// so a caller on the HOT hit path (`Keyword::construct`/`from_owned`)
    /// can populate the thread-local L1 cache (below) without allocating a
    /// fresh `Arc<str>` copy of the text just to cache it.
    #[inline]
    fn find(&self, text: &str, h: u64) -> Option<(Arc<str>, u32)> {
        self.by_hash
            .get(&h)?
            .iter()
            .find(|(t, _)| t.as_ref() == text)
            .map(|(t, id)| (t.clone(), *id))
    }
}

// ---------------------------------------------------------------------------
// Wave 4: thread-local L1 intern cache
// ---------------------------------------------------------------------------
//
// Step 0's attribution (session report) measured the global table's
// lock-free hit path (`content_hash` + `InternInner::find`'s CHAMP-map
// probe + collision-`Vec` linear scan) at ~72% of `Keyword::construct`'s
// own cost on a keyword-heavy corpus -- the CHAMP probe and bucket scan
// dominate, not the hash. This cache sits in front of that path: a
// direct-mapped, fixed-size, per-THREAD table checked first in
// `Keyword::construct`/`from_owned`. A hit is one array index plus one
// text compare -- no atomic load, no CHAMP-map traversal, no bucket scan.
//
// # Why no invalidation logic is needed
//
// The global table is append-only and entries are never renumbered or
// removed (`InternTable::insert_or_overflow`'s doc, INV-1: "nothing
// anywhere removes an entry"; `by_id` only ever grows via `push_back`). A
// cached `(hash, id, text)` triple is therefore valid FOREVER once
// populated -- there is no scenario in which `id`'s meaning changes out
// from under a stale L1 entry. The only thing that ever evicts a slot is
// a DIFFERENT text's hash landing on the same `hash & L1_MASK` index
// later, handled by the direct-mapped "overwrite on collision" policy
// below, not by any freshness check.
//
// # Why per-thread rather than shared
//
// A shared cache would need synchronization (the whole point of L1 is to
// avoid exactly that on the hit path), and mova's `Interp`s are typically
// one-per-thread already (`ARCHITECTURE.md`'s embedding model) -- so a
// thread-local cache gets each `Interp`'s own working set warm with no
// cross-thread traffic at all, at the cost of the same keyword warming up
// N independent caches for N concurrently-active threads. That trade is
// the right one here: the global table (shared, `Interned` ids stable
// process-wide) already deduplicates the expensive part (text ->
// canonical allocation); L1 only needs to be right for THIS thread's own
// repeated lookups.
// Sizing: measured directly, not guessed. `edn.c`'s `keywords_10000.edn`
// corpus (926 DISTINCT keyword texts across 10,000 constructions, no
// temporal locality -- a synthetic, close-to-worst-case shuffle) was tried
// at 512 first (the coordinator's own starting suggestion): direct-mapped
// with 926 keys competing for 512 slots thrashes badly enough that
// `Keyword::construct` came out ~19% SLOWER than with no L1 cache at all
// (every miss pays the L1 probe on top of the full global-table probe it
// still has to fall through to). At 2048 -- comfortably above 926, and
// this design doc's own `KEYWORD_INTERN_CAP` note that an ordinary mova
// program's real vocabulary is "hundreds" of distinct keywords -- the same
// file measured ~12% FASTER than no-L1, and the smaller `keywords_1000.edn`
// (430 distinct, well under 2048) benefits more since it barely collides
// at all. 2048 entries costs ~64KB/thread (`size_of::<Option<L1Entry>>()`
// is niche-optimized to `size_of::<L1Entry>()`, ~32 bytes) -- negligible
// next to mova's one-`Interp`-per-thread embedding model.
const L1_LEN: usize = 2048;
const L1_MASK: usize = L1_LEN - 1; // L1_LEN is a power of two.

/// One L1 slot. `text` is compared byte-for-byte on a hash hit (a `u64`
/// match alone is not proof of identity -- two different texts can share a
/// hash) before trusting `id`.
#[derive(Clone)]
struct L1Entry {
    hash: u64,
    id: u32,
    text: Arc<str>,
}

thread_local! {
    static L1_CACHE: std::cell::RefCell<[Option<L1Entry>; L1_LEN]> =
        std::cell::RefCell::new(std::array::from_fn(|_| None));
}

/// One array index plus (on a hash match) one text compare. `None` on a
/// miss OR a hash-collision-with-a-different-text -- both fall through to
/// the existing global-table probe, exactly as if L1 didn't exist.
#[inline]
fn l1_lookup(text: &str, h: u64) -> Option<Keyword> {
    L1_CACHE.with(|cache| {
        let idx = (h as usize) & L1_MASK;
        match &cache.borrow()[idx] {
            Some(e) if e.hash == h && &*e.text == text => Some(Keyword::Interned(e.id)),
            _ => None,
        }
    })
}

/// Direct-mapped, overwrite-on-collision -- no probing, no eviction policy
/// beyond "whoever's hash lands here last owns the slot". `text` is an
/// existing `Arc<str>` handle (see `InternInner::find`'s doc) so populating
/// L1 on the common hit-path allocates nothing; the two colder callers (a
/// fresh insert, or a hit found only after taking the writer lock) pay one
/// `Arc<str>` allocation to populate it, which is negligible next to the
/// lock they already just took.
#[inline]
fn l1_store(h: u64, id: u32, text: Arc<str>) {
    L1_CACHE.with(|cache| {
        let idx = (h as usize) & L1_MASK;
        cache.borrow_mut()[idx] = Some(L1Entry { hash: h, id, text });
    });
}

/// The process-wide keyword intern table.
///
/// publication pattern: see ARCHITECTURE.md, "Publication patterns
/// (lock-free read paths)" — AtomicPtr + retire-list snapshot. This is the
/// "future site needing a sixth" that section explicitly invites to copy
/// the shape verbatim rather than invent a new one; the shape is
/// `env::RootGlobals`' (`src/env.rs`), field for field, including the
/// `clippy::vec_box` allow and the reason for it.
///
/// # Ordering argument
///
/// Identical to `RootGlobals`', and for the same reasons:
///
/// 1. A writer builds the ENTIRE next snapshot off to the side and installs
///    it with a single `Release` swap, so a reader's `Acquire` load
///    observes either the old snapshot or a fully-constructed new one.
/// 2. Writers are serialized by `writer`, so the read-modify-write a
///    publish performs (probe, append one entry, swap) cannot lose an
///    update — and, uniquely to this table, that same serialization is what
///    makes the cap check race-free: `by_id.len() < CAP` is tested under
///    the writer lock, so there is no window at the boundary in which two
///    threads could both believe they are claiming slot N.
/// 3. A reader can hold a `&InternInner` while a writer publishes over it,
///    so the superseded snapshot must outlive that borrow. It does: it is
///    moved onto the `writer` mutex's retire list instead of being dropped.
///
/// # Lifetime: this table never drops, deliberately
///
/// Unlike `RootGlobals` (whose retire list is bounded by its `Env`'s life),
/// this one is a `static` and has NO `Drop` impl: neither the published
/// snapshot nor the retire list is ever freed, which is precisely what
/// makes [`Keyword::text_ref`]'s `'static`-in-fact borrow sound. The cost
/// of that is bounded by [`KEYWORD_INTERN_CAP`] and priced there; it is
/// fixed process overhead in the same category as `GLOBAL_GENERATION`'s 8
/// bytes or the binary's own `.text` section, not per-engine variable cost.
/// The population that actually drives adversarial RSS growth — a hosted
/// editor evaluating untrusted or generated content that mints far more
/// distinct keyword texts than any real program's vocabulary — lands in
/// `Overflow`, which is an ordinary `Arc`-refcounted heap value freed
/// normally when its last reference drops, with no permanent record kept
/// anywhere of the construction ever having happened.
struct InternTable {
    published: AtomicPtr<InternInner>,
    /// Serializes publishes AND owns the retired snapshots — one lock,
    /// because retiring is part of publishing and nothing else may touch
    /// either.
    // `clippy::vec_box` is wrong here, and dangerously so: the `Box` is not
    // a removable indirection over `Vec<InternInner>`, it is the SAME
    // allocation a reader may still be inside, reconstructed from the raw
    // pointer we published. Storing the snapshot by value would MOVE it,
    // invalidating exactly the pointers this list exists to keep valid.
    #[allow(clippy::vec_box)]
    writer: Mutex<Vec<Box<InternInner>>>,
}

/// The one, process-wide table. `OnceLock` (not a `Drop`-less `static mut`
/// or a lazy null `AtomicPtr`) so the accessor below can hand out genuine
/// `&'static` references — see [`Keyword::text_ref`].
static KEYWORD_TABLE: OnceLock<InternTable> = OnceLock::new();

#[inline]
fn table() -> &'static InternTable {
    KEYWORD_TABLE.get_or_init(InternTable::new)
}

/// Wave 4 (edn/fast, keyword-heavy attribution): FNV-1a over `s`'s bytes,
/// replacing `DefaultHasher` (SipHash-1-3). Measured on `keywords_10000.edn`
/// (10,000 keyword constructions, almost all table HITS): hash computation
/// alone was ~40% of `Keyword::construct`'s per-token cost, so a cheaper
/// hash function is a direct win on the hit path this table exists to make
/// fast, at zero allocation either way.
///
/// # Why this is safe -- `content_hash` is INTERNAL to this table
///
/// Verified (`grep -rn content_hash src/`): this function's only two
/// callers are [`Keyword::construct`]/[`Keyword::from_owned`], both in this
/// file, both using the result ONLY to bucket the `by_hash` map. It is NOT
/// `Keyword`'s `Hash` impl (that streams `text_ref()` through `Str`'s own
/// `Hash`, unconditionally content-based -- see that impl's doc) and never
/// reaches `Value`'s equality/hash or any `HashMap<Value, _>` a script can
/// build. `src/source_registry.rs` has an unrelated same-named private
/// `content_hash` for a different table entirely; the two never share code
/// or a call site.
///
/// # DoS consideration (disclosed, accepted)
///
/// FNV-1a has none of SipHash's keyed resistance to a crafted string that
/// forces every key into one bucket. Two things bound the damage here where
/// they wouldn't for e.g. a general-purpose `HashMap<String, _>` exposed to
/// attacker input: keyword TEXT in a real program is source code, not
/// runtime-attacker-controlled data (an embedder handing mova untrusted
/// keyword text at all is already trusting it not to be malicious in far
/// more consequential ways than a bucket scan); and even under a
/// pathological hash-flooding attempt, [`KEYWORD_INTERN_CAP`] means the
/// table can be forced into degenerate buckets only ONCE, up to at most
/// `KEYWORD_INTERN_CAP` entries total, after which every further
/// construction falls to `Overflow` (an ordinary heap `Arc`, no `by_hash`
/// involvement, no bucket to flood) -- a bounded, one-time cost, not an
/// unbounded denial of service.
#[inline]
fn content_hash(s: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h = FNV_OFFSET_BASIS;
    for &b in s.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

impl InternTable {
    fn new() -> Self {
        let inner = InternInner {
            by_hash: champ::PersistentHashMap::new(),
            by_id: imbl::Vector::new(),
        };
        InternTable {
            published: AtomicPtr::new(Box::into_raw(Box::new(inner))),
            writer: Mutex::new(Vec::new()),
        }
    }

    /// The currently published snapshot. See the ordering argument above.
    #[inline]
    fn map(&'static self) -> &'static InternInner {
        // SAFETY: `published` is only ever set from `Box::into_raw` of a
        // live `InternInner` (here and in `publish`), is never null, and a
        // superseded snapshot is retired rather than freed — and this table
        // itself is a `static` that never drops, so the pointee outlives
        // `&'static self`, which is exactly the lifetime claimed.
        unsafe { &*self.published.load(Ordering::Acquire) }
    }

    #[inline]
    fn text_of(&'static self, id: u32) -> &'static Str {
        &self.map().by_id[id as usize]
    }

    /// Installs `next` and retires the snapshot it replaces. Caller must
    /// hold `writer` (it passes the guard's contents in, so that cannot be
    /// forgotten).
    #[allow(clippy::vec_box)] // see the `writer` field
    fn publish(&self, retired: &mut Vec<Box<InternInner>>, next: InternInner) {
        let old = self.published.swap(Box::into_raw(Box::new(next)), Ordering::Release);
        // SAFETY: `old` is the pointer a previous `Box::into_raw` produced
        // and no other thread can be swapping concurrently (we hold
        // `writer`), so reclaiming ownership of it here is sound; it is
        // parked on the retire list rather than dropped because readers may
        // still be inside it.
        retired.push(unsafe { Box::from_raw(old) });
    }

    /// Steps 2-5 of the construction discipline of design doc §2.2 — the
    /// cold half, reached only when the caller's step-1 lock-free probe
    /// missed. Takes `canonical` by value so the caller can hand over a
    /// `Str` it already had (see [`Keyword::from_owned`]) without either
    /// side needing to copy the characters. Also the whole of what
    /// INV-1/2/3 below rest on.
    ///
    /// 1. Lock-free lookup in the published snapshot. Hit → `Interned`.
    ///    (In the two callers, [`Keyword::construct`]/
    ///    [`Keyword::from_owned`], so that a hit allocates nothing.)
    /// 2. Miss → take the writer lock (mirrors `RootGlobals::get_or_intern`
    ///    exactly, including...
    /// 3. ...the re-check under the lock: another thread may have inserted
    ///    `text` between step 1 and the lock acquire). Hit → `Interned`.
    /// 4. Still a miss AND `by_id.len() < CAP` → insert (append `by_id`,
    ///    add to the `by_hash` bucket, publish), return the new id.
    /// 5. Still a miss AND the table is at cap → do NOT insert; return a
    ///    plain `Overflow` (fresh allocation, content-based, ordinary `Arc`
    ///    lifecycle, no permanent record kept of this construction).
    ///
    /// # The invariants (design doc §2.2)
    ///
    /// * **INV-1 (membership is monotone and eventually frozen).** The
    ///   table's key set only ever grows, and stops growing forever once it
    ///   reaches `CAP` members: step 4 is the only insert path and it is
    ///   gated on `len() < CAP`; nothing anywhere removes an entry.
    /// * **INV-2 (classification is decided once, at first construction,
    ///   and never revisited).** For any given text, Interned-vs-Overflow
    ///   is decided by whichever of steps 3/4/5 applies at the moment of
    ///   THAT TEXT'S OWN first-ever `construct` call. Because membership is
    ///   monotone (INV-1) the decision cannot later be invalidated: an
    ///   Interned text stays a permanent member, so every later
    ///   construction of it hits at step 1 or 3 and returns Interned with
    ///   the SAME id; an Overflow text was by definition not a member when
    ///   the table was ALREADY frozen at cap, and the table never
    ///   un-freezes, so every later construction of it misses both lookups
    ///   and again returns Overflow.
    /// * **INV-3 (cross-boundary coexistence is structurally
    ///   unreachable).** A single text therefore has exactly one
    ///   classification for the life of the table — never both, never a
    ///   flip in either direction. "An interned `:foo` and an overflow
    ///   `:foo`" is not a race or an edge case to defend against at the
    ///   equality layer; it is a state this construction discipline cannot
    ///   produce, full stop. (Equality and hash are nonetheless written so
    ///   that a FUTURE bug violating INV-3 fails safe rather than silently
    ///   corrupting a `HashMap` — see [`PartialEq`]/[`Hash`] below.)
    fn insert_or_overflow(&'static self, canonical: Str, h: u64) -> Keyword {
        // 2. take the writer lock.
        let mut retired = lock_mutex(&self.writer);
        // 3. re-check under the lock. Text handle discarded here (`_`) --
        // this call's own caller (`construct`/`from_owned`) already has an
        // `Arc<str>` (or builds one) to populate L1 with; duplicating that
        // here would be redundant work under the lock.
        if let Some((_, id)) = self.map().find(canonical.as_ref(), h) {
            return Keyword::Interned(id);
        }
        let old = self.map();
        let len = old.by_id.len() as u32;
        // 5. at cap: frozen forever (INV-1), so this text is Overflow now
        // and on every future construction (INV-2).
        if len >= KEYWORD_INTERN_CAP {
            return Keyword::Overflow(Arc::new(canonical));
        }
        // 4. insert. The cap test above ran under `writer`, serialized with
        // every other insert, so no two threads can claim slot `len`.
        let id = len;
        let handle: Arc<str> = Arc::from(canonical.as_ref());
        let mut by_id = old.by_id.clone(); // O(1): structural sharing
        by_id.push_back(canonical);
        let mut bucket = old.by_hash.get(&h).cloned().unwrap_or_default();
        bucket.push((handle, id));
        let next = InternInner {
            by_hash: old.by_hash.assoc(h, bucket),
            by_id,
        };
        self.publish(&mut retired, next);
        Keyword::Interned(id)
    }
}

/// S4: intern many texts under one lock and ONE publish (heap-image restore):
/// per-insert publishing retires a whole snapshot each time (~2 KB/keyword).
pub fn intern_bulk(texts: &[&str]) {
    let t = table();
    let mut retired = lock_mutex(&t.writer);
    let old = t.map();
    let (mut by_hash, mut by_id) = (old.by_hash.clone(), old.by_id.clone());
    let n0 = by_id.len();
    for &s in texts {
        if by_id.len() as u32 >= KEYWORD_INTERN_CAP {
            break;
        }
        let h = content_hash(s);
        let mut bucket = by_hash.get(&h).cloned().unwrap_or_default();
        if bucket.iter().any(|(x, _)| x.as_ref() == s) {
            continue;
        }
        bucket.push((Arc::from(s), by_id.len() as u32));
        by_id.push_back(Str::from(s));
        by_hash = by_hash.assoc(h, bucket);
    }
    if by_id.len() != n0 {
        t.publish(&mut retired, InternInner { by_hash, by_id });
    }
}

/// Calls `f` with the text (no colon) of every interned keyword. One pass
/// over the published snapshot, no allocation (nREPL `completions`).
pub fn for_each_interned(f: &mut dyn FnMut(&str)) {
    for s in table().map().by_id.iter() {
        f(s.as_ref());
    }
}

/// How many distinct keyword texts currently hold an interned id — i.e.
/// how close the table is to [`KEYWORD_INTERN_CAP`]. Diagnostic only
/// (`tests/geo_intern_probe.rs`'s capped-RSS harness); nothing in the
/// interpreter branches on it.
pub fn interned_count() -> usize {
    table().map().by_id.len()
}

// ---------------------------------------------------------------------------
// Equality / hash / ordering — design doc §2.2
// ---------------------------------------------------------------------------

impl PartialEq for Keyword {
    /// `Interned`/`Interned` is a bare `u32` compare — the whole point of
    /// the design (Probe D: ~3x a content compare).
    ///
    /// EVERY other arm falls back to a content compare. `Overflow`/
    /// `Overflow` genuinely needs it; the cross arms cannot occur for
    /// matching text (INV-3) but must still type-check, and writing them as
    /// a content compare means a future bug that violated INV-3 would be
    /// merely slower than necessary rather than silently wrong. Same
    /// defense-in-depth habit as `host_struct`'s ordering argument.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Keyword::Interned(a), Keyword::Interned(b)) => a == b,
            _ => self.text_ref() == other.text_ref(),
        }
    }
}

impl Eq for Keyword {}

impl Hash for Keyword {
    /// Content-based UNCONDITIONALLY, for both arms.
    ///
    /// This is the one place `Interned`'s id-hash speedup (Probe D: ~2.3x)
    /// is deliberately NOT taken. Two values that are `==` must hash equal;
    /// an `Interned` and an `Overflow` of the same text are `==` under the
    /// impl above (unreachable per INV-3, but the type system does not know
    /// that), so hashing the bare id would break `HashMap<Value, _>` the
    /// moment the two ever met. Probe D's 2.3x was an ID-vs-ID measurement
    /// — what a `HashMap` keyed on the *id itself* would get, not what
    /// `Value`'s own `Hash` can get. A real, disclosed constraint, not an
    /// oversight.
    ///
    /// Streams exactly `<str as Hash>::hash` (via `Str`'s own impl), which
    /// is byte-for-byte what `Value::Keyword`'s hash arm produced before
    /// stage 1 — required, since keyword-keyed maps are everywhere.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text_ref().hash(state)
    }
}

impl PartialOrd for Keyword {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Keyword {
    /// Content order, NEVER id order — and this is not an oversight to
    /// "optimize" later. Interning ids are assigned in CONSTRUCTION order;
    /// sorted-map/sorted-set order is a CONTENT property (lexicographic by
    /// name, matching real Clojure). Comparing by id would silently
    /// scramble `sorted-map`/`sorted-set` iteration order the moment two
    /// keywords' construction order disagreed with their text order —
    /// which is the common case. See `builtins::sorted`'s keyword arm.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.text_ref().cmp(other.text_ref())
    }
}

// ---------------------------------------------------------------------------
// Text-shaped conveniences — all thin wrappers over `text_ref`, mirroring
// the impls `Str` itself carries, so keyword call sites that predate stage
// 1 keep reading (and compiling) exactly as they did.
// ---------------------------------------------------------------------------

impl std::ops::Deref for Keyword {
    type Target = str;
    fn deref(&self) -> &str {
        self.text_ref().as_ref()
    }
}

impl AsRef<str> for Keyword {
    fn as_ref(&self) -> &str {
        self.text_ref().as_ref()
    }
}

impl std::borrow::Borrow<str> for Keyword {
    fn borrow(&self) -> &str {
        self.text_ref().as_ref()
    }
}

impl PartialEq<str> for Keyword {
    fn eq(&self, other: &str) -> bool {
        self.text_ref() == other
    }
}

impl PartialEq<&str> for Keyword {
    fn eq(&self, other: &&str) -> bool {
        self.text_ref() == *other
    }
}

impl std::fmt::Debug for Keyword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Delegates to `Str`'s `Debug` (which delegates to `str`'s), so
        // `{:?}` on a keyword prints byte-for-byte what it did pre-stage-1.
        std::fmt::Debug::fmt(self.text_ref(), f)
    }
}

impl std::fmt::Display for Keyword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.text_ref(), f)
    }
}

impl From<&str> for Keyword {
    fn from(s: &str) -> Self {
        Keyword::construct(s)
    }
}

impl From<String> for Keyword {
    fn from(s: String) -> Self {
        Keyword::construct(&s)
    }
}

impl From<&String> for Keyword {
    fn from(s: &String) -> Self {
        Keyword::construct(s.as_str())
    }
}

impl From<Str> for Keyword {
    fn from(s: Str) -> Self {
        Keyword::from_owned(s)
    }
}

impl From<&Str> for Keyword {
    fn from(s: &Str) -> Self {
        Keyword::from_owned(s.clone())
    }
}

impl Default for Keyword {
    fn default() -> Self {
        Keyword::construct("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_a_is_size_neutral_against_str() {
        // Design doc §1.2: shape (A) measures 16 bytes, exactly what the
        // `Str` payload it replaces cost. If this ever changes, `Value`'s
        // own pinned 72 (`value.rs`) is the next thing to check.
        assert_eq!(std::mem::size_of::<Keyword>(), 16);
        assert_eq!(std::mem::size_of::<Keyword>(), std::mem::size_of::<Str>());
    }

    #[test]
    fn same_text_interns_to_the_same_id_and_the_same_allocation() {
        let a = Keyword::construct("geo-stage1-same-text");
        let b = Keyword::construct("geo-stage1-same-text");
        assert_eq!(a.interned_id(), b.interned_id(), "INV-2: one classification per text");
        assert!(a.interned_id().is_some());
        assert_eq!(a, b);
        // The `host_struct` IC's premise: one canonical allocation, so one
        // stable `identity_addr`, forever.
        assert!(Str::ptr_eq(a.text_ref(), b.text_ref()));
    }

    #[test]
    fn distinct_texts_get_distinct_ids_and_compare_unequal() {
        let a = Keyword::construct("geo-stage1-alpha");
        let b = Keyword::construct("geo-stage1-beta");
        assert_ne!(a.interned_id(), b.interned_id());
        assert_ne!(a, b);
        assert_eq!(a.text_ref().as_ref(), "geo-stage1-alpha");
        assert_eq!(b.text_ref().as_ref(), "geo-stage1-beta");
    }

    #[test]
    fn from_owned_reuses_the_str_and_still_dedups() {
        let s = Str::from("geo-stage1-from-owned");
        let a = Keyword::from_owned(s.clone());
        let b = Keyword::construct("geo-stage1-from-owned");
        assert_eq!(a, b);
        assert_eq!(a.interned_id(), b.interned_id());
    }

    #[test]
    fn overflow_and_interned_of_the_same_text_still_compare_and_hash_equal() {
        // INV-3 says this pairing is unreachable through `construct`; the
        // point of the test is that equality/hash FAIL SAFE if a future bug
        // ever produced it (design doc §2.2's defense-in-depth clause), so
        // it builds the `Overflow` arm by hand deliberately.
        let interned = Keyword::construct("geo-stage1-cross-arm");
        assert!(interned.interned_id().is_some());
        let overflow = Keyword::Overflow(Arc::new(Str::from("geo-stage1-cross-arm")));
        assert_eq!(interned, overflow);
        assert_eq!(overflow, interned);
        assert_eq!(hash_of(&interned), hash_of(&overflow));
    }

    #[test]
    fn hash_matches_the_bare_str_stream_it_replaced() {
        // Pre-stage-1, `Value`'s keyword hash arm streamed `k.as_ref()`
        // (i.e. `<str as Hash>::hash`). It still does; this pins that the
        // `Keyword` impl agrees, so keyword-keyed maps built before and
        // after this change probe identically.
        let k = Keyword::construct("geo-stage1-hash-stream");
        assert_eq!(hash_of(&k), hash_of(&Str::from("geo-stage1-hash-stream")));
    }

    #[test]
    fn ordering_is_content_not_construction_order() {
        // Constructed in reverse-lexicographic order on purpose: if `Ord`
        // ever took the id fast path, this would come out backwards.
        let z = Keyword::construct("geo-stage1-ord-zzz");
        let a = Keyword::construct("geo-stage1-ord-aaa");
        assert!(z.interned_id().unwrap() < a.interned_id().unwrap(), "construction order");
        assert!(a < z, "but content order wins");
    }

    fn hash_of<T: Hash>(v: &T) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }
}
