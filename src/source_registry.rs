//! field5/W-SPAN: process-wide interning of `(name, text)` source buffers,
//! so a `Span` persisted on a diagnostic (`compile::explain::FnTier::
//! TreeWalk`, `compile::explain::LoopExplain`) can be resolved against the
//! ACTUAL buffer it was read from, not whatever `Interp::source`/
//! `source_name` happen to be CURRENT when the diagnostic renders.
//!
//! The bug this exists to fix: `Interp::source`/`source_name` are
//! process-wide fields, overwritten at every `eval_str`/
//! `eval_str_allow_read_cond` entry (see `Interp::eval_str`'s doc). A
//! `Span`'s byte offsets are only meaningful against the buffer they were
//! read from -- rendering them against whatever buffer is current LATER
//! (a `(compile-explain f)` call issued after other files loaded, a
//! `MOVA_EXPLAIN=1` line for a fn whose containing buffer already scrolled
//! past) produces a syntactically valid but WRONG `file:line:col` (measured
//! on a parse.clj run: 91,924 of 91,934 explain lines misattributed, see
//! `docs/OWNER-BRIEF-SESSION13.md` item 9). `Span` itself stays a bare
//! `{start, end}` (Copy, 80 construction sites -- adding an id there was
//! rejected as blast radius); instead, the small number of structs that
//! PERSIST a span past the `eval_str` call that produced it carry a
//! `source_id: u32` alongside it, and `Interp::source_id` -- set alongside
//! `source_name`/`source` at every existing set-site -- is what gets
//! stamped in at construction time.
//!
//! Mirrors `lens::SiteRegistry`'s `OnceLock<Mutex<...>>` discipline
//! (`src/lens.rs`'s `SITES`/`sites()`/`alloc_site`): a cold-path-only
//! process-wide table behind one mutex, with a hard cap and an overflow
//! counter instead of a panic.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::value::Str;

/// Reserved id: "unknown source". Every renderer treats it (and any id
/// that fails to [`resolve`]) as "fall back to `Interp::source`/
/// `source_name`" -- the same best-effort behavior this module replaces
/// for anything that has been taught to carry a real id. Also the id
/// [`intern`] returns once [`MAX_SOURCES`] is exceeded.
pub const UNKNOWN_SOURCE: u32 = 0;

/// Hard cap on distinct interned sources for one process. [`intern`] is a
/// COLD path -- called once per `eval_str`/`eval_str_allow_read_cond`
/// entry (a file load, a `require`, a REPL top-of-buffer submission), never
/// per form or per compile -- and deduplicates on `(name, content-hash)`,
/// so a file `require`d from two namespaces or a REPL buffer resubmitted
/// unchanged reuses its id rather than growing the table. Past the cap,
/// [`intern`] returns [`UNKNOWN_SOURCE`] and counts the overflow rather
/// than panicking: a diagnostic falling back to best-effort rendering is
/// always safer than an eval-path panic over a table that exists purely to
/// make error messages nicer.
const MAX_SOURCES: usize = MAX_LIVE_SLOTS;

/// Slot bits of an id: `id = (generation << SLOT_BITS) | (slot + 1)`. A slot
/// freed by [`SourceLease`] is reused with a bumped generation, so a stale id
/// of a released source never resolves to a different source.
const SLOT_BITS: u32 = 14;
const SLOT_MASK: u32 = (1 << SLOT_BITS) - 1;
/// Slots a process may hold at once (live pinned sources + running transient
/// evals). Past this, [`intern`] gives [`UNKNOWN_SOURCE`].
const MAX_LIVE_SLOTS: usize = SLOT_MASK as usize - 1;

/// One bit per slot: "kept for good". Set for every plain [`intern`] and by
/// [`pin`] (a fn/macro/explain record now refers to the source). A transient
/// source ([`intern_transient`]) whose bit is still clear at the end of its
/// eval is released. Lock-free so [`pin`] is cheap at closure creation.
static PINNED: [std::sync::atomic::AtomicU64; (SLOT_MASK as usize + 1) / 64] =
    [const { std::sync::atomic::AtomicU64::new(0) }; (SLOT_MASK as usize + 1) / 64];

fn pin_bit(slot: usize, on: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    let (w, b) = (slot >> 6, 1u64 << (slot & 63));
    if on {
        PINNED[w].fetch_or(b, Relaxed);
    } else {
        PINNED[w].fetch_and(!b, Relaxed);
    }
}

fn is_pinned(slot: usize) -> bool {
    PINNED[slot >> 6].load(std::sync::atomic::Ordering::Relaxed) & (1u64 << (slot & 63)) != 0
}

/// Live references per slot (closures, see [`SrcRef`]).
static REFS: [std::sync::atomic::AtomicU32; SLOT_MASK as usize + 1] = [const { std::sync::atomic::AtomicU32::new(0) }; SLOT_MASK as usize + 1];

/// A counted reference from a fn/macro to the source buffer its spans point
/// into. While one is alive, a transient source is not freed; when the last
/// goes (the fn is dropped) the slot is freed, so the registry grows only with
/// live code. Cheap: one atomic per clone/drop; only the last drop of a
/// transient source takes the registry lock.
pub struct SrcRef(u32);

impl SrcRef {
    pub const NONE: SrcRef = SrcRef(UNKNOWN_SOURCE);

    pub fn new(id: u32) -> SrcRef {
        if id != UNKNOWN_SOURCE {
            REFS[((id & SLOT_MASK) - 1) as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        SrcRef(id)
    }

    #[inline]
    pub fn get(&self) -> u32 {
        self.0
    }
}

impl Clone for SrcRef {
    fn clone(&self) -> SrcRef {
        SrcRef::new(self.0)
    }
}

impl std::fmt::Debug for SrcRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SrcRef({})", self.0)
    }
}

impl Drop for SrcRef {
    fn drop(&mut self) {
        if self.0 == UNKNOWN_SOURCE {
            return;
        }
        let slot = ((self.0 & SLOT_MASK) - 1) as usize;
        let prev = REFS[slot].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        if prev == 1 && !is_pinned(slot) {
            crate::sync::lock_mutex(registry()).try_free(slot, self.0);
        }
    }
}

// RSS-lever (LSP RSS census round 2): this table used to keep a full `text`
// `Str` per entry (~2.7MB across a typical clojure-lsp session's ~300
// interned namespace sources) purely so a LATER `render_at` could recompute
// line/col against the EXACT bytes a `Span` was read from. Most of that
// weight is real namespace/file loads, where `name` is a real absolute
// path (module-path entries are real filesystem dirs) -- for those we now
// keep only the content hash and re-read `name` from disk on demand in
// `resolve_local`, verifying the hash still matches before trusting the
// fresh bytes. A changed/missing file degrades to `None`, same as an
// unresolved id -- render_at's existing fallback (best-effort against the
// CURRENT buffer) already handles that.
//
// But `eval_str` also stamps a fresh `source_id` on ad-hoc, non-file
// buffers (a REPL top-of-buffer submission, a `compile-explain`/
// `eval-string` query) -- exactly the multi-buffer-misattribution case
// this module was built to fix, per its module doc. Those names never
// resolve on disk, so for them we keep `text` in memory as before: they're
// low-volume/small (a REPL session's own buffers, not clojure-lsp's ~300
// namespace files), so the memory cost is negligible, and dropping it
// would silently reintroduce the original bug for exactly the case that
// motivated this table. `is_file()` at intern time decides which shape an
// entry gets.
pub(crate) enum EntryPayload {
    /// A real file: content hash only, re-read from `name` on resolve.
    Disk(u64),
    /// Everything else: the interned text, kept as-is (previous behavior).
    Mem(Str),
}

struct Entry {
    name: Str,
    payload: EntryPayload,
    /// Generation of the id this slot hands out (0 for a never-reused slot,
    /// so the id is `slot + 1`, which the heap image relies on).
    gen: u32,
    /// Running transient evals using this entry.
    users: u32,
    /// Freed slot, waiting in `free`.
    free: bool,
}

#[derive(Default)]
struct Registry {
    entries: Vec<Entry>,
    /// Dedup key: `(name, content-hash)`. Keyed on content hash rather than
    /// a full text comparison so a hot REPL loop resubmitting the same
    /// buffer over and over is a cheap hash lookup, not a string compare
    /// against every prior entry.
    by_key: HashMap<(String, u64), u32>,
    /// How many distinct sources were requested past [`MAX_SOURCES`].
    overflowed: u64,
    /// Slots released by [`SourceLease`], ready for reuse.
    free: Vec<usize>,
}

impl Registry {
    /// Core of [`intern`], parameterized on the cap so unit tests can drive
    /// a tiny LOCAL registry to overflow without touching the real
    /// process-wide table (which every other test in this binary also
    /// interns into via `Interp::eval_str`) -- same reasoning as
    /// `lens_test.rs`'s `serial()` guard, taken one step further since a
    /// [`MAX_SOURCES`]-sized fill would starve every other test in this
    /// process of real ids for the rest of its run.
    fn intern_with_cap(&mut self, name: &str, text: &str, cap: usize) -> u32 {
        self.intern_inner(name, text, cap, false).unwrap_or(UNKNOWN_SOURCE)
    }

    fn id_of(&self, slot: usize) -> u32 {
        (self.entries[slot].gen << SLOT_BITS) | (slot as u32 + 1)
    }

    /// `transient`: the caller holds a lease and the entry may be freed
    /// when the lease ends (unless pinned). A plain intern pins its entry.
    fn intern_inner(&mut self, name: &str, text: &str, cap: usize, transient: bool) -> Option<u32> {
        let key = (name.to_string(), content_hash(text));
        if let Some(id) = self.by_key.get(&key).copied() {
            let slot = ((id & SLOT_MASK) - 1) as usize;
            if transient {
                self.entries[slot].users += 1;
            } else {
                pin_bit(slot, true);
            }
            return Some(id);
        }
        let hash = key.1;
        // Only a REAL file gets the disk-backed, text-dropping shape --
        // see the module's `EntryPayload` doc.
        let payload = if std::path::Path::new(name).is_file() { EntryPayload::Disk(hash) } else { EntryPayload::Mem(Str::from(text)) };
        let slot = if let Some(slot) = self.free.pop() {
            let e = &mut self.entries[slot];
            e.gen = e.gen.wrapping_add(1) & (u32::MAX >> SLOT_BITS);
            e.name = Str::from(name);
            e.payload = payload;
            e.free = false;
            e.users = 0;
            slot
        } else {
            let live = self.entries.len();
            if live >= cap.min(MAX_LIVE_SLOTS) {
                self.overflowed += 1;
                return None;
            }
            self.entries.push(Entry { name: Str::from(name), payload, gen: 0, users: 0, free: false });
            live
        };
        if transient {
            self.entries[slot].users = 1;
            pin_bit(slot, false);
        } else {
            pin_bit(slot, true);
        }
        let id = self.id_of(slot);
        self.by_key.insert(key, id);
        Some(id)
    }

    /// Ends a transient use: frees the slot when nobody else uses it and no
    /// live fn refers to it.
    fn release(&mut self, id: u32) {
        let slot = ((id & SLOT_MASK) as usize).wrapping_sub(1);
        if self.id_of_checked(slot) != Some(id) {
            return;
        }
        let e = &mut self.entries[slot];
        e.users = e.users.saturating_sub(1);
        self.try_free(slot, id);
    }

    /// Frees `slot` (id `id`) if it is a transient source with no users and no refs.
    fn try_free(&mut self, slot: usize, id: u32) {
        if self.id_of_checked(slot) != Some(id) {
            return;
        }
        let e = &mut self.entries[slot];
        if e.users > 0 || is_pinned(slot) || REFS[slot].load(std::sync::atomic::Ordering::Relaxed) > 0 {
            return;
        }
        let key = match &e.payload {
            EntryPayload::Mem(t) => (e.name.to_string(), content_hash(t)),
            EntryPayload::Disk(h) => (e.name.to_string(), *h),
        };
        self.by_key.remove(&key);
        e.free = true;
        e.name = Str::from("");
        e.payload = EntryPayload::Mem(Str::from(""));
        self.free.push(slot);
    }

    fn id_of_checked(&self, slot: usize) -> Option<u32> {
        let e = self.entries.get(slot)?;
        (!e.free).then(|| self.id_of(slot))
    }

    /// For a disk-backed entry, re-reads `name` and checks it against the
    /// hash recorded at intern time, `None` on any mismatch (missing file,
    /// changed content). For an in-memory entry, just clones the kept
    /// text. See the module's `EntryPayload` doc.
    fn resolve_local(&self, id: u32) -> Option<(Str, Str)> {
        if id == UNKNOWN_SOURCE {
            return None;
        }
        let slot = ((id & SLOT_MASK) as usize).checked_sub(1)?;
        let e = self.entries.get(slot)?;
        if e.free || self.id_of(slot) != id {
            return None;
        }
        match &e.payload {
            EntryPayload::Mem(text) => Some((e.name.clone(), text.clone())),
            EntryPayload::Disk(hash) => {
                let fresh = std::fs::read_to_string(e.name.as_ref()).ok()?;
                if content_hash(&fresh) != *hash {
                    return None;
                }
                Some((e.name.clone(), Str::from(fresh)))
            }
        }
    }
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// FNV-1a over the raw bytes. Not a security boundary (dedup only), so a
/// simple well-known hash beats pulling in a crate for it.
fn content_hash(text: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Interns `(name, text)`, returning a stable id (never [`UNKNOWN_SOURCE`]
/// for a successful intern). Dedups on `(name, content-hash)`: re-loading
/// the SAME name with the SAME content reuses its id; the same name with
/// DIFFERENT content (a REPL buffer edited and resubmitted) mints a new
/// one, since it is genuinely a different source now. `text` is retained
/// as one `Arc`-backed [`Str`] clone (cheap; see `Str`'s own doc) capped at
/// [`MAX_SOURCES`] total entries -- see that constant's doc for the
/// overflow behavior.
pub fn intern(name: &str, text: &str) -> u32 {
    let mut reg = crate::sync::lock_mutex(registry());
    reg.intern_with_cap(name, text, MAX_SOURCES)
}

/// A running transient source: made by [`intern_transient`], released when
/// dropped unless a fn or other live code was pinned to it meanwhile.
pub struct SourceLease(u32);

impl Drop for SourceLease {
    fn drop(&mut self) {
        if self.0 != UNKNOWN_SOURCE {
            crate::sync::lock_mutex(registry()).release(self.0);
        }
    }
}

/// Like [`intern`], for a source that is only needed while one eval runs (an
/// nREPL request): the slot is freed when the lease drops, unless live code
/// ([`pin`]) refers to it by then. Keeps a long session from filling the table.
pub fn intern_transient(name: &str, text: &str) -> (u32, SourceLease) {
    let id = crate::sync::lock_mutex(registry()).intern_inner(name, text, MAX_SOURCES, true).unwrap_or(UNKNOWN_SOURCE);
    (id, SourceLease(id))
}

/// Resolves `id` back to the `(name, text)` it was interned with. `None`
/// for [`UNKNOWN_SOURCE`] or an id that doesn't resolve (should not happen
/// for an id this module minted, but callers treat it exactly like
/// `UNKNOWN_SOURCE` either way: fall back to the current-buffer legacy
/// rendering).
pub fn resolve(id: u32) -> Option<(Str, Str)> {
    let reg = crate::sync::lock_mutex(registry());
    reg.resolve_local(id)
}

/// Renders `span` (byte offsets only, no buffer identity of its own -- see
/// `reader::Span`) as `name:line:col` against the buffer `source_id`
/// names, or against `interp.source`/`source_name` (today's CURRENT
/// buffer) when `source_id` is [`UNKNOWN_SOURCE`] or fails to [`resolve`]
/// -- the pre-fix best-effort fallback, kept for anything that hasn't been
/// taught to carry a real id (id 0 on a fresh `Interp`, an overflowed
/// intern, or a caller that passes `interp.source_id` for something
/// rendered synchronously in the same `eval_str` call it was produced in,
/// where the fallback and the correct answer are the same value anyway).
pub fn render_at(interp: &crate::eval::Interp, source_id: u32, span: crate::reader::Span) -> String {
    match resolve(source_id) {
        Some((name, text)) => {
            let (line, col) = crate::error::line_col(text.as_ref(), span.start);
            format!("{name}:{line}:{col}")
        }
        None => {
            let (line, col) = crate::error::line_col(interp.source.as_ref(), span.start);
            format!("{}:{line}:{col}", interp.source_name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real process-wide `intern`/`resolve` are exercised here with
    /// names unlikely to collide with any OTHER test in this binary that
    /// also interns via `Interp::eval_str` -- unlike the cap test below,
    /// filling only a handful of entries is safe to run against the shared
    /// singleton.
    #[test]
    fn dedup_by_name_and_content() {
        let id1 = intern("w-span-dedup-test.mova", "(+ 1 2)");
        let id2 = intern("w-span-dedup-test.mova", "(+ 1 2)");
        assert_eq!(id1, id2, "same name + same content must reuse the id");
    }

    #[test]
    fn distinct_content_gets_distinct_ids() {
        let id1 = intern("w-span-distinct-test.mova", "(+ 1 2)");
        let id2 = intern("w-span-distinct-test.mova", "(+ 1 3)");
        assert_ne!(id1, id2, "same name + DIFFERENT content must mint a new id");
    }

    #[test]
    fn resolve_roundtrips_name_and_text() {
        // `resolve` now re-reads `name` off disk (see `Entry`'s doc), so
        // the round trip needs a real file whose content matches what was
        // interned.
        let path = std::env::temp_dir().join("w-span-roundtrip-test.mova");
        std::fs::write(&path, "(defn f [] 1)").unwrap();
        let path_str = path.to_str().unwrap();
        let id = intern(path_str, "(defn f [] 1)");
        let (name, text) = resolve(id).expect("an id intern() just minted must resolve");
        assert_eq!(name.as_ref(), path_str);
        assert_eq!(text.as_ref(), "(defn f [] 1)");
        let _ = std::fs::remove_file(&path);
    }

    /// Slots of transient sources are reused; a pinned one stays; a stale id
    /// of a released source never resolves to the new owner of its slot.
    #[test]
    fn transient_release_reuses_slots_and_pinned_stay() {
        let mut reg = Registry::default();
        // The pin/ref tables are process-wide: put this local registry's slots
        // where the real one never goes.
        for _ in 0..16000 {
            reg.entries.push(Entry { name: Str::from(""), payload: EntryPayload::Mem(Str::from("")), gen: 0, users: 0, free: true });
        }
        let a = reg.intern_inner("t-a", "1", 20000, true).unwrap();
        reg.release(a);
        assert!(reg.resolve_local(a).is_none());
        let b = reg.intern_inner("t-b", "2", 20000, true).unwrap();
        assert_ne!(a, b, "a reused slot gets a new generation");
        assert_eq!(reg.entries.len(), 16001);
        assert!(reg.resolve_local(a).is_none());
        assert_eq!(reg.resolve_local(b).unwrap().1.as_ref(), "2");
        let slot = ((b & SLOT_MASK) - 1) as usize;
        REFS[slot].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        reg.release(b);
        assert_eq!(reg.resolve_local(b).unwrap().1.as_ref(), "2", "a source with a live ref stays");
        REFS[slot].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        reg.try_free(slot, b);
        assert!(reg.resolve_local(b).is_none(), "freed when the last ref goes");
        // many transient evals never grow the table
        for i in 0..5000 {
            let id = reg.intern_inner("t-loop", &i.to_string(), 20000, true).unwrap();
            reg.release(id);
        }
        assert!(reg.entries.len() <= 16003, "{}", reg.entries.len());
    }

    #[test]
    fn unknown_source_never_resolves() {
        assert!(resolve(UNKNOWN_SOURCE).is_none());
    }

    #[test]
    fn id_is_stable_across_many_lookups() {
        let id = intern("w-span-stability-test.mova", "(+ 40 2)");
        for _ in 0..50 {
            assert_eq!(intern("w-span-stability-test.mova", "(+ 40 2)"), id);
        }
    }

    /// Cap + overflow, on a fresh LOCAL registry (not the process-wide
    /// singleton -- see `Registry::intern_with_cap`'s doc for why): past a
    /// cap of 2, a third DISTINCT entry must overflow to `UNKNOWN_SOURCE`
    /// and count itself, never panic.
    #[test]
    fn cap_overflow_returns_unknown_not_panic() {
        // `resolve_local` now re-reads off disk, so "a"/"b" need to be real
        // files for the resolve assertions below to see fresh content.
        let dir = std::env::temp_dir();
        let (pa, pb) = (dir.join("w-span-cap-a.tmp"), dir.join("w-span-cap-b.tmp"));
        std::fs::write(&pa, "1").unwrap();
        std::fs::write(&pb, "2").unwrap();
        let (a, b) = (pa.to_str().unwrap(), pb.to_str().unwrap());
        let mut reg = Registry::default();
        let id1 = reg.intern_with_cap(a, "1", 2);
        let id2 = reg.intern_with_cap(b, "2", 2);
        assert_ne!(id1, UNKNOWN_SOURCE);
        assert_ne!(id2, UNKNOWN_SOURCE);
        assert_ne!(id1, id2);
        let id3 = reg.intern_with_cap("c", "3", 2);
        assert_eq!(id3, UNKNOWN_SOURCE, "a third distinct entry past cap=2 must overflow to id 0");
        assert_eq!(reg.overflowed, 1);
        // Overflow must never wedge re-resolution of the entries that DID
        // fit under the cap.
        assert_eq!(reg.resolve_local(id1).unwrap().1.as_ref(), "1");
        assert_eq!(reg.resolve_local(id2).unwrap().1.as_ref(), "2");
        // A REPEAT of an already-fit entry still hits the dedup path even
        // once the registry is "full" -- overflow only applies to
        // genuinely NEW keys.
        assert_eq!(reg.intern_with_cap(a, "1", 2), id1);
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }
}

// ---- heap-image gate-1 accessors (src/image.rs) ----
pub(crate) fn img_len() -> usize {
    registry().lock().unwrap_or_else(|e| e.into_inner()).entries.len()
}
pub(crate) fn img_entries_from(n: usize) -> Vec<(Str, EntryPayload)> {
    let g = registry().lock().unwrap_or_else(|e| e.into_inner());
    g.entries[n.min(g.entries.len())..]
        .iter()
        .map(|e| {
            let payload = match &e.payload {
                EntryPayload::Disk(h) => EntryPayload::Disk(*h),
                EntryPayload::Mem(t) => EntryPayload::Mem(t.clone()),
            };
            (e.name.clone(), payload)
        })
        .collect()
}
/// Appends without hashing (restore path); a later identical intern just
/// gets a fresh id, which only costs a duplicate entry.
pub(crate) fn img_name_at(i: usize) -> Option<Str> {
    registry().lock().unwrap_or_else(|e| e.into_inner()).entries.get(i).map(|e| e.name.clone())
}
pub(crate) fn img_push(name: Str, payload: EntryPayload) {
    let mut g = registry().lock().unwrap_or_else(|e| e.into_inner());
    let slot = g.entries.len();
    pin_bit(slot & SLOT_MASK as usize, true);
    g.entries.push(Entry { name, payload, gen: 0, users: 0, free: false });
}
