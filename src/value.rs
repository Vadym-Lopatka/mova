//! Core `Value` type, `Symbol`, and the manual equality/hash rules that
//! bind the rest of mova together. See ARCHITECTURE.md "Core types" for
//! the contract these types must follow exactly.
//!
//! v0.2 / A1: `Value` (and everything it owns) is `Send + Sync` --
//! `Rc`/`RefCell` became `Arc`/`Mutex`/`RwLock` throughout so a `Value` can
//! safely cross threads (`future*`-spawned threads share the same `globals`
//! `Env` and can pass closures/atoms/etc. back and forth). See
//! `_assert_send_sync` at the bottom of this file and `src/sync.rs` for the
//! poisoned-lock policy every lock acquisition here goes through.

use std::borrow::{Borrow, Cow};
use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::iter::FromIterator;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::Duration;

pub use crate::keyword::Keyword;
use crate::sync::{cv_wait_timeout, lock_mutex};

/// Sentinel for [`StrInner::ascii`]/[`RopeInner::ascii`]: not yet computed.
const ASCII_UNKNOWN: u8 = 0;
/// Sentinel: computed, the text's `is_ascii()` was true.
const ASCII_TRUE: u8 = 1;
/// Sentinel: computed, the text's `is_ascii()` was false.
const ASCII_FALSE: u8 = 2;
/// Sentinel for [`StrInner::char_count`]: not yet computed. `text.len()`
/// (the byte length, an upper bound on char count) can never reach
/// `usize::MAX` for any string that fits in memory, so this is safe as an
/// "unknown" marker alongside genuine counts.
const CHAR_COUNT_UNKNOWN: usize = usize::MAX;

/// M8: strings at or above this many bytes are represented as a
/// `champ::PText` rope instead of a flat `Arc<str>` -- see
/// `StrRepr::Rope` and SPEC-M8-TEXT-INTEGRATION.md. One-way ratchet, same
/// policy as `PVEC_SMALL_MAX`/`PMAP_SMALL_MAX`: an op on an already-`Rope`
/// value stays `Rope` even if its result shrinks back below this
/// threshold (only the handful of ops that build a brand new `Str` from a
/// plain byte source re-check it). Tunable; picked to match the spec's
/// starting point.
pub const STR_ROPE_MIN: usize = 64 * 1024;

/// Interned-ish string handle used throughout the interpreter for anything
/// that doesn't need mutation: symbol/keyword text, error messages, etc.
///
/// v0.5 / perf: strings are char-indexed everywhere (`subs`/`index-of`/
/// `nth`/`count`), and the hot builtins used to re-derive "is this ASCII"
/// and "how many chars" on every single call via a fresh O(n) scan --
/// quadratic when a hosted editor calls them O(n) times over one document
/// (see `builtins::strings`' module doc). Strings are immutable once
/// built, so both facts are safe to compute once, lazily, on first need,
/// and cache for the life of the `Arc` -- every clone of a `Str` shares
/// the same cache. Plain atomics (Relaxed everywhere): the computation is
/// pure and idempotent (derived only from the text), so if two threads
/// race to compute it they simply agree and one write is redundant, never
/// wrong -- no lock needed.
///
/// M8: above `STR_ROPE_MIN` bytes, `Str` is backed by a `champ::
/// PText` rope instead of a flat buffer -- see `StrRepr` and
/// SPEC-M8-TEXT-INTEGRATION.md for the dual-representation contract this
/// type must uphold (representation-blind `Eq`/`Hash`/print;
/// representation-AWARE `ptr_eq`). `Deref`/`AsRef`/`Borrow<str>` (and
/// therefore every existing call site that leans on them) keep compiling
/// and stay correct across both variants, materializing (and caching) a
/// flat copy of a `Rope` on first touch -- see `as_str_slow`/
/// `as_contiguous`. The handful of ops the M8 spec calls out as needing to
/// stay rope-native (never materialize) on the editor's hot path --
/// `subs`/`nth`/`count`/`str`-concat/`blank?`/print -- go through
/// dedicated methods below instead of `Deref`.
#[derive(Clone)]
pub struct Str(StrRepr);

#[derive(Clone)]
enum StrRepr {
    Flat(Arc<StrInner>),
    Rope(Arc<RopeInner>),
}

pub struct StrInner {
    ascii: AtomicU8,
    char_count: AtomicUsize,
    text: Box<str>,
}

struct RopeInner {
    rope: champ::PText,
    /// Same lazy ascii cache as `StrInner`'s, computed by a chunk-wise scan
    /// (see [`Str::is_ascii_cached`]) instead of one flat byte scan --
    /// never materializes. No separate `char_count` cache is needed here:
    /// `PText::len_chars` is already O(1) (cached in the rope's own
    /// summary), unlike `str::chars().count()`.
    ascii: AtomicU8,
    /// [`Str::as_contiguous`]/`Deref`'s materialize-once-and-cache buffer.
    /// `OnceLock` rather than eager: plenty of `Rope`-backed `Str`s (a
    /// hot-typed `:editor/text`, most of all) are never `Deref`'d in their
    /// lifetime at all -- every splice makes a new persistent `Str`, so
    /// paying to flatten one that's about to be discarded would be pure
    /// waste. See SPEC-M8-TEXT-INTEGRATION.md's "materialize-with-care"
    /// section.
    flat: OnceLock<Box<str>>,
}

impl Str {
    /// True iff `self` and `other` are the exact same allocation (share one
    /// `Arc`), mirroring `Arc::ptr_eq` for every other cell-backed `Value`
    /// variant (see `builtins::predicates::identical`).
    ///
    /// Representation-AWARE by design (per SPEC-M8-TEXT-INTEGRATION.md):
    /// a `Flat` and a `Rope` handle are never the same allocation, so this
    /// is `false` across variants even when their content is equal --
    /// unlike [`PartialEq`] below, which is representation-BLIND.
    pub fn ptr_eq(a: &Str, b: &Str) -> bool {
        match (&a.0, &b.0) {
            (StrRepr::Flat(x), StrRepr::Flat(y)) => Arc::ptr_eq(x, y),
            (StrRepr::Rope(x), StrRepr::Rope(y)) => Arc::ptr_eq(x, y),
            _ => false,
        }
    }

    /// `true` iff this `Str` is `Rope`-backed. Exposed for the small
    /// number of builtins (`str`-concat's ratchet check, tests) that need
    /// to branch on representation directly rather than through one of
    /// the rope-native accessors below.
    pub fn is_rope(&self) -> bool {
        matches!(self.0, StrRepr::Rope(_))
    }

    /// The underlying allocation's address, as a `usize` -- the same
    /// identity [`Self::ptr_eq`] compares, exposed as a plain integer for
    /// `host_struct`'s per-`Shape` inline cache (keyed on a keyword `Str`'s
    /// allocation address, packed into one atomic word alongside a field
    /// index -- see that module's doc). Never `0` for a real `Str` (a
    /// valid `Arc` allocation is never at address zero), which is what
    /// lets the IC use `0` as its "empty slot" sentinel.
    pub(crate) fn identity_addr(&self) -> usize {
        match &self.0 {
            StrRepr::Flat(inner) => Arc::as_ptr(inner) as usize,
            StrRepr::Rope(inner) => Arc::as_ptr(inner) as usize,
        }
    }

    /// Lazily-computed, cached `str::is_ascii()`. O(1) after the first
    /// call for `Flat`; for `Rope`, the first call is one allocation-free
    /// chunk-wise scan (`PText::chunks`), cached afterward exactly like
    /// `Flat`'s -- never touches [`Self::as_contiguous`]'s materialize
    /// cache.
    pub fn is_ascii_cached(&self) -> bool {
        match &self.0 {
            StrRepr::Flat(inner) => match inner.ascii.load(Ordering::Relaxed) {
                ASCII_TRUE => true,
                ASCII_FALSE => false,
                _ => {
                    let ascii = inner.text.is_ascii();
                    inner.ascii.store(if ascii { ASCII_TRUE } else { ASCII_FALSE }, Ordering::Relaxed);
                    ascii
                }
            },
            StrRepr::Rope(inner) => match inner.ascii.load(Ordering::Relaxed) {
                ASCII_TRUE => true,
                ASCII_FALSE => false,
                _ => {
                    let ascii = inner.rope.chunks().all(|c| c.is_ascii());
                    inner.ascii.store(if ascii { ASCII_TRUE } else { ASCII_FALSE }, Ordering::Relaxed);
                    ascii
                }
            },
        }
    }

    /// Lazily-computed, cached char count for `Flat` (ASCII strings take
    /// the O(1) byte-length path without ever touching the `char_count`
    /// cache; non-ASCII strings compute `chars().count()` once and cache
    /// it, as before M8). For `Rope`, always O(1): `PText` already
    /// maintains this in its own summary, so there is nothing to cache on
    /// this side.
    pub fn char_count_cached(&self) -> usize {
        match &self.0 {
            StrRepr::Flat(inner) => {
                if self.is_ascii_cached() {
                    return inner.text.len();
                }
                let cached = inner.char_count.load(Ordering::Relaxed);
                if cached != CHAR_COUNT_UNKNOWN {
                    return cached;
                }
                let n = inner.text.chars().count();
                inner.char_count.store(n, Ordering::Relaxed);
                n
            }
            StrRepr::Rope(inner) => inner.rope.len_chars(),
        }
    }

    /// Byte length. O(1) for both variants (`Rope`'s summary caches it,
    /// same as `char_count_cached`) -- unlike going through `Deref`, never
    /// materializes a `Rope`.
    pub fn byte_len(&self) -> usize {
        match &self.0 {
            StrRepr::Flat(inner) => inner.text.len(),
            StrRepr::Rope(inner) => inner.rope.len_bytes(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.0 {
            StrRepr::Flat(inner) => inner.text.is_empty(),
            StrRepr::Rope(inner) => inner.rope.is_empty(),
        }
    }

    /// `clojure.string/blank?`'s predicate, rope-native: for `Rope`, a
    /// chunk-wise scan that short-circuits on the first non-whitespace
    /// char (the common "this huge document is not blank" case returns
    /// almost immediately) instead of `Deref`'s materialize-then-`trim`.
    pub fn is_blank(&self) -> bool {
        match &self.0 {
            StrRepr::Flat(inner) => inner.text.trim().is_empty(),
            StrRepr::Rope(inner) => inner.rope.chunks().all(|c| c.trim().is_empty()),
        }
    }

    /// `clojure.string/replace`/`replace-first`'s LITERAL-pattern path
    /// (never called for a `Value::Regex` pattern -- the caller in
    /// `builtins::strings` already dispatched on that). `first_only ==
    /// false` replaces every non-overlapping occurrence (matching `str::
    /// replace`'s left-to-right, non-overlapping scan exactly); `true`
    /// replaces only the first.
    ///
    /// Rope-native for a `Rope` source (M8.1): streams `self`'s chunks,
    /// finding matches without ever materializing the whole document into
    /// one contiguous buffer. A `carry` buffer holds the trailing bytes
    /// that can't yet be ruled in or out of a match (at most `pattern.
    /// len() - 1` once a chunk has been fully processed, growing
    /// temporarily mid-chunk for a pattern that spans more than one whole
    /// chunk -- correct either way, since `carry` simply keeps absorbing
    /// chunks until a match completes or is ruled out): after appending a
    /// new chunk, everything in `carry` up to `carry.len() minus (pattern.
    /// len() - 1)` is "safe" (a match can't still be forming there, since
    /// fewer than a full pattern's worth of bytes remain after it), so that
    /// prefix is scanned for matches and either emitted as a replacement
    /// or (if no match starts there) emitted verbatim; only the unsafe
    /// tail carries forward. This bounds work to O(chunk size + pattern
    /// length) per chunk -- the same shape `find_char_from`'s fix uses,
    /// generalized from a single char to an arbitrary literal pattern.
    /// Matches straddling a chunk seam, overlapping-adjacent matches, an
    /// empty replacement, and a pattern longer than one chunk are all
    /// handled by this same logic (no special-casing) -- see value.rs's
    /// own tests for each shape. The result is built incrementally via
    /// `PText::concat` (per confirmed-safe span, roughly chunk-sized), so
    /// no more than one chunk's worth of content is ever held as a flat
    /// buffer at a time. Per the ratchet, a `Rope` source always produces
    /// a `Rope` result (even a same-length/no-op replace) -- EXCEPT the
    /// zero-matches case, which instead returns `self.clone()` directly
    /// (an O(1) `Arc` refcount bump, `ptr_eq` to `self`) rather than
    /// rebuilding a content-identical tree via `concat` for nothing --
    /// this is the actual `normalize-plain` shape for the FIRST call in
    /// a paste whose content already uses LF line endings only (the E11/
    /// E12/M8 benchmark corpus among them): the whole document must still
    /// be scanned once (no way to know there's zero matches otherwise),
    /// but the expensive rebuild is skipped.
    ///
    /// An empty `pattern` is a degenerate case Rust's own `str::replace`
    /// gives unusual semantics to (insert `replacement` between every
    /// char) that `normalize-plain` never exercises (its patterns are
    /// `"\r\n"`/`"\r"`) -- not worth a rope-native path, so it always
    /// materializes via [`Self::as_contiguous`].
    pub fn replace_literal(&self, pattern: &str, replacement: &str, first_only: bool) -> Str {
        if pattern.is_empty() {
            let s = self.as_contiguous();
            let out = if first_only {
                format!("{replacement}{s}")
            } else {
                s.replace(pattern, replacement)
            };
            return Str::from(out);
        }
        match &self.0 {
            StrRepr::Flat(inner) => {
                let s: &str = &inner.text;
                let out = if first_only {
                    match s.find(pattern) {
                        Some(idx) => format!("{}{}{}", &s[..idx], replacement, &s[idx + pattern.len()..]),
                        None => return self.clone(),
                    }
                } else {
                    s.replace(pattern, replacement)
                };
                Str::from(out)
            }
            StrRepr::Rope(inner) => {
                fn push_piece(acc: &mut Option<champ::PText>, piece: champ::PText) {
                    *acc = Some(match acc.take() {
                        None => piece,
                        Some(a) => champ::PText::concat(a, piece),
                    });
                }
                // `ascii == true` (checked ONCE via the already-cached
                // `is_ascii_cached` -- O(1) after this method's own first
                // call, since a 2-call `normalize-plain`-shaped chain
                // reuses the cache on its second call) means char index ==
                // byte index throughout, skipping a full `.chars().count()`
                // scan that would otherwise roughly DOUBLE this method's
                // total work (one pass for `find`, one for char-counting)
                // on the overwhelmingly-common all-ASCII document.
                fn chars_upto(s: &str, byte_idx: usize, ascii: bool) -> usize {
                    if ascii {
                        byte_idx
                    } else {
                        s[..byte_idx].chars().count()
                    }
                }
                // Scans `carry[..limit]` (bounding where a match may
                // START -- see the char-boundary-snapping comment below;
                // the search itself covers the full remaining `carry`,
                // since a match starting before `limit` may legitimately
                // extend past it) for non-overlapping matches. Unlike an
                // earlier version of this method, literal (non-matching)
                // spans are NOT flushed here as they're found -- flushing
                // eagerly per chunk meant the all-too-common "scanned the
                // whole document, found zero matches" case (`normalize-
                // plain`'s real shape on an LF-only document, see this
                // method's own doc comment) still paid a full `PText::
                // from`+`concat` rebuild for content that turned out
                // unchanged. Instead, `pending_start_chars` marks where
                // the current not-yet-flushed literal run began (in the
                // ORIGINAL rope's char coordinates); a run is flushed via
                // `source.slice` -- O(log n), sharing structure, no byte
                // copy -- only once an actual match forces the issue, and
                // the very last run is flushed once, after the whole scan,
                // by the caller. Zero matches means zero slices, zero
                // concats: just `self.clone()` (see below).
                #[allow(clippy::too_many_arguments)]
                fn scan_chunk(
                    acc: &mut Option<champ::PText>,
                    source: &champ::PText,
                    carry: &str,
                    carry_start_chars: usize,
                    limit: usize,
                    pattern: &str,
                    pat_len: usize,
                    pat_chars: usize,
                    replacement: &str,
                    first_only: bool,
                    ascii: bool,
                    pending_start_chars: &mut usize,
                    done: &mut bool,
                    replaced_any: &mut bool,
                ) -> usize {
                    let mut pos = 0usize;
                    while pos < limit {
                        match carry[pos..].find(pattern) {
                            Some(rel) => {
                                let match_start = pos + rel;
                                if match_start >= limit {
                                    // Starts too close to the end of what's
                                    // been read so far to be sure yet --
                                    // leave it (and everything before it)
                                    // for the caller to carry forward.
                                    break;
                                }
                                let match_start_chars = carry_start_chars + chars_upto(carry, match_start, ascii);
                                if match_start_chars > *pending_start_chars {
                                    push_piece(acc, source.slice(*pending_start_chars..match_start_chars));
                                }
                                if !replacement.is_empty() {
                                    push_piece(acc, champ::PText::from(replacement));
                                }
                                *replaced_any = true;
                                pos = match_start + pat_len;
                                *pending_start_chars = match_start_chars + pat_chars;
                                if first_only {
                                    *done = true;
                                    return pos;
                                }
                            }
                            None => break,
                        }
                    }
                    pos
                }
                let pat_len = pattern.len();
                let pat_chars = pattern.chars().count();
                let source = &inner.rope;
                let total_chars = source.len_chars();
                let ascii = self.is_ascii_cached();
                let mut acc: Option<champ::PText> = None;
                let mut carry = String::new();
                let mut carry_start_chars = 0usize; // where `carry` begins, in `source`'s char coordinates
                let mut pending_start_chars = 0usize; // where the not-yet-flushed literal run begins
                let mut done = false; // first_only: stop scanning after one match
                let mut replaced_any = false;
                for chunk in inner.rope.chunks() {
                    if done {
                        break; // the final slice below picks up everything remaining in one shot
                    }
                    carry.push_str(chunk);
                    // Bytes up to `carry.len() minus (pattern.len() - 1)`
                    // can't still be the start of a not-yet-complete match
                    // (fewer than a full pattern's worth of bytes would
                    // remain after them), so they're safe to resolve now --
                    // but that arithmetic cutoff can land mid-character for
                    // a multi-byte `carry`; snap down to the nearest real
                    // char boundary (never forward -- that could skip past
                    // a genuine match start).
                    let mut safe_end = carry.len().saturating_sub(pat_len - 1);
                    while safe_end > 0 && !carry.is_char_boundary(safe_end) {
                        safe_end -= 1;
                    }
                    let pos = scan_chunk(
                        &mut acc,
                        source,
                        &carry,
                        carry_start_chars,
                        safe_end,
                        pattern,
                        pat_len,
                        pat_chars,
                        replacement,
                        first_only,
                        ascii,
                        &mut pending_start_chars,
                        &mut done,
                        &mut replaced_any,
                    );
                    // Advance the SEARCH cursor (independent of the OUTPUT
                    // flush cursor, `pending_start_chars` above) regardless
                    // of whether any match was found this round.
                    if pos <= safe_end {
                        carry_start_chars += chars_upto(&carry, safe_end, ascii);
                        carry.drain(..safe_end); // shifts the tail down in place -- no fresh allocation
                    } else {
                        // The last accepted match started before
                        // `safe_end` but (correctly) extended past it.
                        carry_start_chars += chars_upto(&carry, pos, ascii);
                        carry.drain(..pos);
                    }
                }
                // End of stream: no more chunks can arrive to complete a
                // match, so the ENTIRE remaining carry (not just its own
                // truncated `safe_end`) is now safe to search -- this is
                // what a mid-stream chunk's `safe_end` cutoff deliberately
                // deferred, and skipping this pass would silently miss any
                // match whose start fell in that last held-back tail
                // (including, degenerately, a document that's exactly one
                // chunk long, where every match lives in this final pass).
                if !done && !carry.is_empty() {
                    let limit = carry.len();
                    scan_chunk(
                        &mut acc,
                        source,
                        &carry,
                        carry_start_chars,
                        limit,
                        pattern,
                        pat_len,
                        pat_chars,
                        replacement,
                        first_only,
                        ascii,
                        &mut pending_start_chars,
                        &mut done,
                        &mut replaced_any,
                    );
                }
                if !replaced_any {
                    return self.clone();
                }
                if pending_start_chars < total_chars {
                    push_piece(&mut acc, source.slice(pending_start_chars..total_chars));
                }
                match acc {
                    Some(rope) => Str::wrap_rope(rope),
                    None => Str::from(""),
                }
            }
        }
    }

    /// Char at char index `idx` (`nth`'s implementation for strings), or
    /// `None` if out of bounds. Rope-native: `PText::char_at` is `O(log
    /// n)`, no materialization, unlike `Deref`'s `.chars().nth(idx)`. For
    /// `Flat`, identical to the pre-M8 ASCII-fast-path-then-scan logic.
    pub fn char_at(&self, idx: usize) -> Option<char> {
        match &self.0 {
            StrRepr::Flat(inner) => {
                if self.is_ascii_cached() {
                    return inner.text.as_bytes().get(idx).map(|&b| b as char);
                }
                inner.text.chars().nth(idx)
            }
            StrRepr::Rope(inner) => (idx < inner.rope.len_chars()).then(|| inner.rope.char_at(idx)),
        }
    }

    /// `subs`' implementation: the char range `[range.start, range.end)`
    /// as a new `Str`. Caller-checked bounds (`range.end <=
    /// char_count_cached()`, `range.start <= range.end`) -- panics
    /// (via the underlying byte-offset lookups) otherwise, same contract
    /// `subs`'s own bounds checks upheld pre-M8.
    ///
    /// Rope-native for a `Rope` source: `PText::slice` is `O(log n)` and
    /// shares every subtree entirely inside/outside the range, so this
    /// never copies the parts of a big document that aren't in the
    /// range -- and, per the ratchet policy, the result STAYS `Rope` even
    /// when the slice itself is small (an already-rope document's `subs`
    /// never demotes). For `Flat`, identical byte-offset-conversion logic
    /// to pre-M8 (`char_byte_offset`/`char_range_byte_offsets` in
    /// `builtins::strings`), just expressed as one function instead of
    /// two call sites.
    pub fn char_slice(&self, range: Range<usize>) -> Str {
        debug_assert!(range.start <= range.end);
        match &self.0 {
            StrRepr::Rope(inner) => Str::wrap_rope(inner.rope.slice(range)),
            StrRepr::Flat(inner) => {
                let s: &str = &inner.text;
                let (sb, eb) = if self.is_ascii_cached() {
                    (range.start, range.end)
                } else {
                    let mut start_b: Option<usize> = None;
                    let mut end_b: Option<usize> = None;
                    for (n, (b, _)) in s.char_indices().enumerate() {
                        if n == range.start {
                            start_b = Some(b);
                        }
                        if n == range.end {
                            end_b = Some(b);
                            break;
                        }
                    }
                    let start_b = start_b.unwrap_or(s.len());
                    let end_b = end_b.unwrap_or(s.len());
                    (start_b, end_b)
                };
                Str::from(&s[sb..eb])
            }
        }
    }

    /// Materializes (and, for `Rope`, caches) a contiguous view of the
    /// whole string. O(1)/zero-copy for `Flat`. This is the sanctioned
    /// "materialize with care" escape hatch (regex, FFI/native builtins
    /// that need a real `&str`, anything that used to lean on `Deref`) --
    /// see SPEC-M8-TEXT-INTEGRATION.md's op-routing section. `Deref`/
    /// `AsRef<str>`/`Borrow<str>` below all go through this, which is what
    /// keeps every pre-M8 call site (including native/mova-host, which
    /// must keep compiling unchanged) working without modification.
    pub fn as_contiguous(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.as_str_slow())
    }

    fn as_str_slow(&self) -> &str {
        match &self.0 {
            StrRepr::Flat(inner) => &inner.text,
            StrRepr::Rope(inner) => inner.flat.get_or_init(|| {
                let mut s = String::with_capacity(inner.rope.len_bytes());
                for chunk in inner.rope.chunks() {
                    s.push_str(chunk);
                }
                s.into_boxed_str()
            }),
        }
    }

    /// Writes this string's content into `out` without ever materializing
    /// a `Rope` into one contiguous buffer first -- the "print via
    /// `chunks()`" op the M8 spec calls out (`println`/`print`/`pr`/`str`
    /// on a big `:editor/text` must not pay an extra full-document copy
    /// just to hand it to `println!`).
    pub fn write_into(&self, out: &mut String) {
        match &self.0 {
            StrRepr::Flat(inner) => out.push_str(&inner.text),
            StrRepr::Rope(inner) => {
                for chunk in inner.rope.chunks() {
                    out.push_str(chunk);
                }
            }
        }
    }

    /// `pr-str`'s quoted/escaped form of [`Self::write_into`] -- same
    /// chunk-wise walk, no materialize, used by `printer::write_quoted_string`.
    pub fn write_quoted_into(&self, out: &mut String) {
        out.push('"');
        let push_escaped = |c: char| match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            _ => out.push(c),
        };
        match &self.0 {
            StrRepr::Flat(inner) => inner.text.chars().for_each(push_escaped),
            StrRepr::Rope(inner) => inner.rope.chunks().flat_map(str::chars).for_each(push_escaped),
        }
        out.push('"');
    }

    /// Char iterator (`seq`/`first`/`rest`/destructuring on a string).
    /// Rope-native: `PText::chars` is an allocation-free chunk walk, so
    /// this avoids `Deref`'s materialize step even though the caller
    /// (`Interp::seq_items`) still has to visit and collect every char
    /// into a `Vec<Value>` either way (unavoidable for a full seq
    /// realization, `Rope` or not).
    pub fn chars_iter(&self) -> Box<dyn Iterator<Item = char> + '_> {
        match &self.0 {
            StrRepr::Flat(inner) => Box::new(inner.text.chars()),
            StrRepr::Rope(inner) => Box::new(inner.rope.chars()),
        }
    }

    /// First byte-index occurrence of a single-`char` `needle` at or after
    /// char index `from` (`clojure.string/index-of`'s single-char-needle
    /// case -- the one `oma.core.text/line-start`/`line-end` actually
    /// drive, searching for `'\n'`, once per line, over the WHOLE
    /// document). Rope-native and genuinely bounded by the distance from
    /// `from` to the match (or to the end), NOT by `from`'s absolute
    /// position: `PText::slice(from..len)` is `O(log n)` and shares every
    /// subtree of the untouched prefix (no bytes of it are ever visited),
    /// so the chunk-wise `str::find` walk below only ever touches the
    /// suffix that's actually being searched -- a version of this method
    /// that instead walked `self`'s full `chunks()` from the start,
    /// filtering by byte offset, would cost `O(from)` regardless of where
    /// the match is (an M8 profiling regression this method's own
    /// `mode/skip-hidden`-driven caller exposed: it made a mid-document
    /// keystroke's total command cost ~2.3x worse than an equivalent
    /// keystroke at the document's start, entirely from this asymmetry,
    /// even though the string composition itself is position-independent
    /// -- see mova-INTEGRATION-RESULTS.md's M8 section). A single-char
    /// needle can never straddle a leaf boundary (leaves always split on
    /// char boundaries), so no cross-chunk bookkeeping is needed either
    /// way. Multi-char needles on a `Rope` fall back to
    /// [`Self::as_contiguous`] in `builtins::strings` instead of
    /// duplicating a boundary-straddling chunked search here -- see that
    /// module's doc comment.
    pub fn find_char_from(&self, needle: char, from: usize) -> Option<usize> {
        match &self.0 {
            StrRepr::Flat(_) => unreachable!("find_char_from is only called on the Rope fast path; Flat goes through Deref"),
            StrRepr::Rope(inner) => {
                let total = inner.rope.len_chars();
                if from > total {
                    return None;
                }
                let suffix = inner.rope.slice(from..total);
                let mut buf = [0u8; 4];
                let needle_str: &str = needle.encode_utf8(&mut buf);
                let mut acc_chars = 0usize;
                for chunk in suffix.chunks() {
                    if let Some(pos) = chunk.find(needle_str) {
                        return Some(from + acc_chars + chunk[..pos].chars().count());
                    }
                    acc_chars += chunk.chars().count();
                }
                None
            }
        }
    }

    /// Last char-index occurrence of a single-`char` `needle` at or before
    /// char index `from`, searching backward -- see
    /// [`Self::find_char_from`]'s doc comment for the same "bounded by
    /// distance to the match, not by absolute position" argument and the
    /// regression it fixes. Since [`champ::Chunks`] has no reverse
    /// iterator to walk backward from an arbitrary point directly, this
    /// instead slices a small trailing window ending at `from`
    /// (`O(log n)`) and searches it forward for the LAST match (there can
    /// be more than one occurrence inside one window); if the window
    /// doesn't reach a match, it doubles and retries. Bounded by `O(log
    /// (distance from `from` back to the match)) * O(window)` slices, each
    /// `O(log n)` -- for `oma.core.text/line-start`'s real usage (the
    /// nearest `'\n'` behind the caret), the match is almost always within
    /// the first, smallest window (one line's length away).
    pub fn rfind_char_from(&self, needle: char, from: usize) -> Option<usize> {
        match &self.0 {
            StrRepr::Flat(_) => unreachable!("rfind_char_from is only called on the Rope fast path; Flat goes through Deref"),
            StrRepr::Rope(inner) => {
                let total = inner.rope.len_chars();
                if total == 0 {
                    return None;
                }
                let upper = from.min(total - 1); // last char index to consider (inclusive)
                let mut buf = [0u8; 4];
                let needle_str: &str = needle.encode_utf8(&mut buf);
                let mut window = 256usize;
                loop {
                    let window_start = upper.saturating_sub(window);
                    let slice = inner.rope.slice(window_start..upper + 1);
                    let mut acc_chars = 0usize;
                    let mut last_match: Option<usize> = None;
                    for chunk in slice.chunks() {
                        let mut scanned = 0usize;
                        while let Some(pos) = chunk[scanned..].find(needle_str) {
                            last_match = Some(acc_chars + chunk[..scanned + pos].chars().count());
                            scanned += pos + needle_str.len();
                        }
                        acc_chars += chunk.chars().count();
                    }
                    if let Some(m) = last_match {
                        return Some(window_start + m);
                    }
                    if window_start == 0 {
                        return None;
                    }
                    window = window.saturating_mul(4);
                }
            }
        }
    }

    /// Wraps an already-built rope as a `Str`, unconditionally `Rope`
    /// (never re-checks `STR_ROPE_MIN`) -- the ratchet: every op that
    /// starts from an existing `Rope` value and produces another `PText`
    /// (slice, concat) uses this, so the representation never demotes
    /// itself back to `Flat` just because one particular edit's result
    /// happened to be small. Mirrors `PVec`/`PMap`'s own "stays `Big`
    /// once promoted" policy.
    pub fn wrap_rope(rope: champ::PText) -> Str {
        Str(StrRepr::Rope(Arc::new(RopeInner {
            rope,
            ascii: AtomicU8::new(ASCII_UNKNOWN),
            flat: OnceLock::new(),
        })))
    }

    /// Borrows the underlying rope, if this `Str` is `Rope`-backed --
    /// used by `str`-concat's rope-native accumulation path
    /// (`builtins::strings`) to grab an O(1) `PText::clone` without a
    /// `char_slice`/materialize detour.
    pub(crate) fn as_rope(&self) -> Option<&champ::PText> {
        match &self.0 {
            StrRepr::Rope(inner) => Some(&inner.rope),
            StrRepr::Flat(_) => None,
        }
    }

    fn build(s: &str) -> Str {
        if s.len() >= STR_ROPE_MIN {
            Str::wrap_rope(champ::PText::from(s))
        } else {
            Str::build_flat(s.into())
        }
    }

    fn build_flat(text: Box<str>) -> Str {
        Str(StrRepr::Flat(Arc::new(StrInner {
            ascii: AtomicU8::new(ASCII_UNKNOWN),
            char_count: AtomicUsize::new(CHAR_COUNT_UNKNOWN),
            text,
        })))
    }

    /// Test-only: force `Flat` representation regardless of
    /// `STR_ROPE_MIN`, so cross-variant tests can exercise `Flat`-vs-
    /// `Rope` equality/hash/print on convenient small strings instead of
    /// needing genuine 64KiB+ literals for every case.
    #[cfg(test)]
    fn force_flat(s: &str) -> Str {
        Str::build_flat(s.into())
    }

    /// Test-only: force `Rope` representation regardless of size --
    /// the small-string twin of [`Self::force_flat`].
    #[cfg(test)]
    fn force_rope(s: &str) -> Str {
        Str::wrap_rope(champ::PText::from(s))
    }
}

impl std::ops::Deref for Str {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str_slow()
    }
}

impl AsRef<str> for Str {
    fn as_ref(&self) -> &str {
        self.as_str_slow()
    }
}

impl Borrow<str> for Str {
    fn borrow(&self) -> &str {
        self.as_str_slow()
    }
}

impl From<&str> for Str {
    fn from(s: &str) -> Self {
        Str::build(s)
    }
}

impl From<String> for Str {
    fn from(s: String) -> Self {
        if s.len() >= STR_ROPE_MIN {
            Str::wrap_rope(champ::PText::from(s))
        } else {
            Str::build_flat(s.into_boxed_str())
        }
    }
}

impl From<Box<str>> for Str {
    fn from(s: Box<str>) -> Self {
        if s.len() >= STR_ROPE_MIN {
            Str::wrap_rope(champ::PText::from(&*s))
        } else {
            Str::build_flat(s)
        }
    }
}

impl From<&String> for Str {
    fn from(s: &String) -> Self {
        Str::from(s.as_str())
    }
}

impl Default for Str {
    fn default() -> Self {
        Str::build_flat("".into())
    }
}

impl std::fmt::Debug for Str {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Delegate to `str`'s own `Debug` (quoted, escaped) so any existing
        // debug-format expectations (e.g. in error messages built with
        // `{:?}`) print byte-for-byte identically to the old `Arc<str>`.
        // Debug isn't one of the spec's named rope-native ops (error/repl
        // diagnostics, not the editor hot path) -- materializing here is
        // the accepted "materialize with care" tradeoff.
        std::fmt::Debug::fmt(self.as_str_slow(), f)
    }
}

impl std::fmt::Display for Str {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Rope-native (`write/print via chunks()`, spec op-routing list):
        // does NOT go through `as_str_slow`'s materialize-and-cache path.
        match &self.0 {
            StrRepr::Flat(inner) => f.write_str(&inner.text),
            StrRepr::Rope(inner) => {
                for chunk in inner.rope.chunks() {
                    f.write_str(chunk)?;
                }
                Ok(())
            }
        }
    }
}

impl PartialEq for Str {
    /// Representation-blind (per SPEC-M8-TEXT-INTEGRATION.md): a `Flat`
    /// and a `Rope` `Str` with the same content compare equal. `Flat`-vs-
    /// `Flat` and `Rope`-vs-`Rope` both check pointer identity first
    /// (`Rope`'s via `PText`'s own `PartialEq`, which does the same);
    /// cross-variant falls back to a chunked byte compare against the
    /// flat side's buffer without needing `PText`'s own `PartialEq` (which
    /// only knows how to compare two `PText`s).
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (StrRepr::Flat(a), StrRepr::Flat(b)) => Arc::ptr_eq(a, b) || a.text == b.text,
            (StrRepr::Rope(a), StrRepr::Rope(b)) => Arc::ptr_eq(a, b) || a.rope == b.rope,
            (StrRepr::Flat(flat), StrRepr::Rope(rope)) | (StrRepr::Rope(rope), StrRepr::Flat(flat)) => {
                if flat.text.len() != rope.rope.len_bytes() {
                    return false;
                }
                let mut rest: &str = &flat.text;
                for chunk in rope.rope.chunks() {
                    if !rest.starts_with(chunk) {
                        return false;
                    }
                    rest = &rest[chunk.len()..];
                }
                rest.is_empty()
            }
        }
    }
}

impl Eq for Str {}

impl PartialEq<str> for Str {
    fn eq(&self, other: &str) -> bool {
        match &self.0 {
            StrRepr::Flat(inner) => &*inner.text == other,
            StrRepr::Rope(inner) => {
                if inner.rope.len_bytes() != other.len() {
                    return false;
                }
                let mut rest = other;
                for chunk in inner.rope.chunks() {
                    if !rest.starts_with(chunk) {
                        return false;
                    }
                    rest = &rest[chunk.len()..];
                }
                rest.is_empty()
            }
        }
    }
}

impl PartialEq<&str> for Str {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl Hash for Str {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Must match `str::hash` exactly (same as `Hash for Arc<str>`
        // delegating to the pointee), so a `Str` and an equal `&str` hash
        // identically -- required for `HashMap<Str, _>::get(&str)` (via
        // `Borrow<str>` above) to find entries. `PText`'s own `Hash` impl
        // (M7) is documented to stream the exact same byte-then-`0xff`
        // sequence `<str as Hash>::hash` does, which is what makes a
        // `Flat` and an equal `Rope` `Str` hash equal -- required for the
        // representation-blind contract above (`Eq` must imply equal
        // hash).
        match &self.0 {
            StrRepr::Flat(inner) => (*inner.text).hash(state),
            StrRepr::Rope(inner) => inner.rope.hash(state),
        }
    }
}

impl PartialOrd for Str {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Str {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Not a named rope-native op (only used for sorting/printer
        // determinism over small keys in practice) -- materializes both
        // sides, "materialize with care".
        self.as_str_slow().cmp(other.as_str_slow())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Symbol {
    pub ns: Option<Str>,
    pub name: Str,
}

impl Symbol {
    /// Convenience constructor for a namespace-less symbol.
    pub fn simple(name: impl Into<Str>) -> Self {
        Symbol {
            ns: None,
            name: name.into(),
        }
    }
}

/// Stage 2 (dual representation, see docs/mova-port.md "chunk geometry"
/// experiment): a small-vector optimization for `Value::List`/
/// `Value::Vector`, mirroring Clojure's own PersistentArrayMap-style
/// small/big split. Stage 1 found a single global imbl chunk-size knob
/// can't serve both masters -- shrinking it wins small collections
/// (mapv/conj-heavy flow state) but loses badly on the RRB-tree depth a
/// large document's line/char vector needs. `Small` sidesteps chunk
/// bookkeeping entirely below the threshold (one contiguous `Arc<[Value]>`,
/// COW'd by a `PVEC_SMALL_MAX`-element memcpy -- cheaper than any chunk
/// node at this size); `Big` is a `champ::PVector<Value>` (M10.1:
/// swapped in for the previously-vendored `imbl::Vector` RRB-tree -- a
/// 32-ary dense trie + tail + `(offset, len)` view header, see
/// `champ/src/vec/mod.rs`'s module docs; `pop_front`/`slice` are
/// O(1) views instead of RRB front-restructuring), entered once an
/// operation would grow past the threshold and NEVER exited (monotonic
/// promotion -- matches Clojure's own array-map -> hash-map promotion
/// policy, and keeps this type's invariants simple: once `Big`, always
/// `Big`).
///
/// Every method here is written to be a drop-in replacement for the
/// `imbl::Vector<Value>` API subset the rest of the crate actually calls
/// (discovered by `cargo build` error-driven iteration, per the stage-2
/// brief) -- same names, same `&mut self` COW-mutation style as imbl's own
/// `push_back`/`set`/etc, same panic-on-OOB indexing.
///
/// W-GEO stage 4 (split-box, `docs/W-GEO-STAGE4-SPLITBOX.md`): `Big`'s
/// payload lives behind an `Arc` so that `size_of::<PVec>()` is 16 (the
/// width of `Small`'s fat `Arc<[Value]>` pointer) instead of the width of
/// an inline `Big` payload. `Small` is deliberately UNTOUCHED: Probe E
/// measured that wrapping `MapEntry`'s always-`Small` `PVec` in a second
/// `Arc` costs +34-40% on map iteration (KILL); Probe G measured this
/// narrower box at +0.2%/-3.0% on that same workload (PROCEED). The one
/// real cost is `Arc::make_mut` on a SHARED `Big` handle (one handle
/// clone -- for `champ::PVector` this is a cheap header clone, two
/// refcount bumps, NOT a deep tree copy), bounded by how rare `Big`
/// receivers are -- see the doc. M10.1: `&mut self` ops on `Big` route
/// through `Arc::make_mut` (as before) and then thread the freshly-uniqued
/// handle through `champ`'s OWNED (`_owned`, consuming) entry points
/// via a `std::mem::replace(inner, PVector::new())` swap -- the same
/// move-out-and-thread-through shape `PMap::insert` already uses for
/// `assoc_owned_replacing` above -- so a uniquely-held `Big` mutates its
/// tail/spine in place instead of path-copying, and a shared one still
/// falls back to `champ`'s own copy-on-first-shared-node path.
#[derive(Clone)]
pub enum PVec {
    Small(Arc<[Value]>),
    Big(Arc<champ::PVector<Value>>),
    /// M6: columnar packed vector of maps (`crate::colvec`).
    Col(Arc<crate::colvec::ColVec>),
}

/// Small holds at most this many elements before any growing op promotes
/// to `Big`. 16 * (approx 72-byte `Value`) = ~1.2KB memcpy per COW, which
/// the stage-1 numbers showed beats RRB chunk-node overhead at this scale.
/// (W-GEO stage 4 shrank `Value` to 32 bytes, so that COW is now ~512B --
/// the threshold is deliberately LEFT at 16: moving it would change
/// promotion behavior, which this wave holds fixed. Re-tuning it against
/// the new width is a separate, separately measured question.)
pub const PVEC_SMALL_MAX: usize = 16;

impl PVec {
    pub fn new() -> Self {
        PVec::Small(Arc::from(Vec::new()))
    }

    pub fn len(&self) -> usize {
        match self {
            PVec::Small(a) => a.len(),
            PVec::Big(v) => v.len(),
            PVec::Col(c) => c.len(),
        }
    }

    /// M6: swap a `Col` for its materialized `PVec` before any mutation.
    #[inline]
    fn demat(&mut self) {
        if let PVec::Col(c) = self {
            // uncached copy: a mutated copy must not pin the packed source's cache
            let m: PVec = if c.is_mat() { c.mat().clone() } else { PVec::Col(c.clone()).into_iter().collect() };
            *self = m;
        }
    }

    /// M6: owned element access that never materializes a `Col`.
    #[inline]
    pub fn get_owned(&self, index: usize) -> Option<Value> {
        match self {
            PVec::Col(c) => c.elem(index),
            _ => self.get(index).cloned(),
        }
    }

    /// M6: cloned elements; a `Col` yields owned elements without materializing.
    pub fn iter_cloned(&self) -> PVecCloned<'_> {
        match self {
            PVec::Col(c) => PVecCloned::Col(PVecIntoIter::Col(c.clone(), 0)),
            _ => PVecCloned::Ref(self.iter()),
        }
    }

    pub fn is_col(&self) -> bool {
        matches!(self, PVec::Col(_))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&Value> {
        #[cfg(feature = "geo-census")]
        crate::geo_census::pvec_access();
        match self {
            PVec::Small(a) => a.get(index),
            PVec::Big(v) => v.get(index),
            PVec::Col(c) => c.mat().get(index),
        }
    }

    pub fn first(&self) -> Option<&Value> {
        self.get(0)
    }

    pub fn front(&self) -> Option<&Value> {
        self.get(0)
    }

    pub fn last(&self) -> Option<&Value> {
        #[cfg(feature = "geo-census")]
        crate::geo_census::pvec_access();
        match self {
            PVec::Small(a) => a.last(),
            // M10.1: `champ::PVector` has no `last()` of its own --
            // `get` on the final index is the O(log32 n) equivalent.
            PVec::Big(v) => v.len().checked_sub(1).and_then(|i| v.get(i)),
            PVec::Col(c) => c.mat().last(),
        }
    }

    pub fn back(&self) -> Option<&Value> {
        self.last()
    }

    pub fn iter(&self) -> PVecIter<'_> {
        #[cfg(feature = "geo-census")]
        crate::geo_census::pvec_scan();
        match self {
            PVec::Small(a) => PVecIter::Small(a.iter()),
            PVec::Big(v) => PVecIter::Big(v.iter()),
            PVec::Col(c) => c.mat().iter(),
        }
    }

    /// M13 (SPEC-M13-EQWITH.md wiring): reach the inner
    /// `champ::PVector` when `self` is already `Big`, so callers
    /// that want the crate's pruned `try_eq_by`/`eq_by` (rather than a
    /// manual `len`+`zip` walk) don't have to match on the enum
    /// themselves. `None` on `Small` -- the caller's fast path only
    /// fires when BOTH sides are `Big`.
    pub(crate) fn as_big(&self) -> Option<&champ::PVector<Value>> {
        match self {
            PVec::Big(v) => Some(v),
            PVec::Small(_) | PVec::Col(_) => None,
        }
    }

    /// Build the `Big` representation from `self`'s current elements
    /// without consuming `self` (used by every promoting op below).
    ///
    /// Returns the BARE `champ::PVector` (not the boxed `Arc<...>`
    /// `Big` holds): every caller mutates the result before storing it,
    /// and a freshly built/cloned handle is exactly what they want to
    /// mutate. The `Big` arm clones the `PVector` handle (cheap, a header
    /// clone, structural sharing) -- the same clone this method did
    /// before split-box, since `self` must stay valid for the caller.
    fn to_big(&self) -> champ::PVector<Value> {
        match self {
            PVec::Small(a) => champ::PVector::from_slice(a),
            PVec::Big(v) => (**v).clone(),
            PVec::Col(c) => c.mat().to_big(),
        }
    }

    /// M10.1: `make_mut` (as before split-box) to reach a uniquely-owned
    /// `Arc<PVector<Value>>` handle, then MOVE the `PVector` out of it
    /// (leaving a cheap, allocation-light `PVector::new()` placeholder
    /// behind) and thread it through `push_back_owned` -- the owned/
    /// consuming entry point that mutates a uniquely-owned tail in place
    /// instead of path-copying. Same shape as `PMap::insert`'s
    /// `assoc_owned_replacing` swap above.
    pub fn push_back(&mut self, value: Value) {
        self.demat();
        match self {
            PVec::Big(v) => {
                let inner = Arc::make_mut(v);
                let owned = std::mem::replace(inner, champ::PVector::new());
                *inner = owned.push_back_owned(value);
            }
            PVec::Small(a) => {
                #[cfg(feature = "geo-census")]
                crate::geo_census::pvec_churn();
                if a.len() < PVEC_SMALL_MAX {
                    let mut nv: Vec<Value> = Vec::with_capacity(a.len() + 1);
                    nv.extend(a.iter().cloned());
                    nv.push(value);
                    *self = PVec::Small(Arc::from(nv));
                } else {
                    // field4/W-LENS-1: promotion is UNCONDITIONALLY
                    // counted (unlike its `geo-census` siblings below): it
                    // is the one-way `Small` -> `Big` transition, it is
                    // rare, and it sits directly in front of a full
                    // `to_big()` rebuild -- so the counter is free next to
                    // what it measures, and W-GEO boxing gets a promotion
                    // signal from every ordinary build, not only from a
                    // diagnostic one.
                    crate::lens::event(crate::lens::Event::PVecPromote);
                    let big = self.to_big().push_back_owned(value);
                    *self = PVec::Big(Arc::new(big));
                }
            }
            PVec::Col(_) => unreachable!("demat"),
        }
    }

    /// `push_front` is cold (no external callers besides this promotion
    /// path) and `champ::PVector` has no owned variant of it (only
    /// `push_back`/`pop_front`/`pop_back`/`set` do) -- `make_mut` to reach
    /// a unique handle, then the ordinary persistent call, is correct and
    /// sufficient here.
    pub fn push_front(&mut self, value: Value) {
        self.demat();
        match self {
            PVec::Big(v) => {
                let inner = Arc::make_mut(v);
                *inner = inner.push_front(value);
            }
            PVec::Small(a) => {
                if a.len() < PVEC_SMALL_MAX {
                    let mut nv: Vec<Value> = Vec::with_capacity(a.len() + 1);
                    nv.push(value);
                    nv.extend(a.iter().cloned());
                    *self = PVec::Small(Arc::from(nv));
                } else {
                    let big = self.to_big().push_front(value);
                    *self = PVec::Big(Arc::new(big));
                }
            }
            PVec::Col(_) => unreachable!("demat"),
        }
    }

    /// M10.1: routes through `pop_front_owned` (O(1) offset/len bump, zero
    /// refcount traffic on a uniquely-owned handle) rather than the
    /// persistent `pop_front` -- this is mova's `uncons`/seq-walk hot
    /// path.
    pub fn pop_front(&mut self) -> Option<Value> {
        if let PVec::Col(c) = self {
            let h = c.elem(0)?;
            let rest = c.view(1, c.len());
            *self = rest;
            return Some(h);
        }
        match self {
            PVec::Big(v) => {
                let inner = Arc::make_mut(v);
                let owned = std::mem::replace(inner, champ::PVector::new());
                let (rest, popped) = owned.pop_front_owned();
                *inner = rest;
                popped
            }
            PVec::Small(a) => {
                if a.is_empty() {
                    return None;
                }
                let first = a[0].clone();
                let rest: Vec<Value> = a[1..].to_vec();
                *self = PVec::Small(Arc::from(rest));
                Some(first)
            }
            PVec::Col(_) => unreachable!("demat"),
        }
    }

    pub fn pop_back(&mut self) -> Option<Value> {
        if let PVec::Col(c) = self {
            let h = c.elem(c.len().checked_sub(1)?)?;
            let rest = c.view(0, c.len() - 1);
            *self = rest;
            return Some(h);
        }
        match self {
            PVec::Big(v) => {
                let inner = Arc::make_mut(v);
                let owned = std::mem::replace(inner, champ::PVector::new());
                let (rest, popped) = owned.pop_back_owned();
                *inner = rest;
                popped
            }
            PVec::Small(a) => {
                if a.is_empty() {
                    return None;
                }
                let last = a[a.len() - 1].clone();
                let rest: Vec<Value> = a[..a.len() - 1].to_vec();
                *self = PVec::Small(Arc::from(rest));
                Some(last)
            }
            PVec::Col(_) => unreachable!("demat"),
        }
    }

    /// Mirrors `imbl::Vector::set`: replace the element at `index`,
    /// returning the OLD value. Panics on OOB, same as imbl. M10.1: reads
    /// the old value first (champ's `set`/`set_owned` are
    /// persistent-style, returning the new vector, not the old element),
    /// then routes through `set_owned` for the same owned/last-use fast
    /// path as `push_back`/`pop_front`/`pop_back` above.
    pub fn set(&mut self, index: usize, value: Value) -> Value {
        self.demat();
        match self {
            PVec::Big(v) => {
                let inner = Arc::make_mut(v);
                let old = inner.get(index).expect("PVec::set: index out of bounds").clone();
                let owned = std::mem::replace(inner, champ::PVector::new());
                *inner = owned.set_owned(index, value);
                old
            }
            PVec::Small(a) => {
                // REUSE (Perceus-lite phase 1, see `builtins::reuse`):
                // `Arc<[Value]>` is unsized, so there is no `make_mut` for
                // it -- but `get_mut` gives the same effect for a
                // length-preserving update: mutate the existing allocation
                // when this is the only handle, fall back to the original
                // clone-and-replace when it isn't. (Length-*changing* ops
                // below cannot reuse: an `Arc<[T]>` has no spare capacity.)
                if let Some(slice) = Arc::get_mut(a) {
                    return std::mem::replace(&mut slice[index], value);
                }
                let old = a[index].clone();
                let mut nv: Vec<Value> = a.to_vec();
                nv[index] = value;
                *self = PVec::Small(Arc::from(nv));
                old
            }
            PVec::Col(_) => unreachable!("demat"),
        }
    }

    /// Mirrors `imbl::Vector::update`: non-mutating `set`, returns a new
    /// `PVec` with the element replaced.
    pub fn update(&self, index: usize, value: Value) -> Self {
        let mut c = self.clone();
        c.set(index, value);
        c
    }

    /// `append`/`insert`/`remove`/`swap`/`sort_by` on `Big` are confirmed
    /// DEAD API (zero external callers anywhere in mova or the
    /// omawritejank host -- `imbl`'s own RRB `append`/`split_off`/
    /// `insert`/`remove`/`swap`/`sort` were unused too). Kept for API
    /// completeness/correctness (some are exercised by this module's own
    /// tests) via a plain `Vec` round-trip rather than a hand-tuned
    /// `champ` sequence -- `champ::PVector` has no native
    /// concat/insert-in-middle primitive, and there is no perf bar to hit
    /// on a path nothing calls.
    pub fn append(&mut self, other: Self) {
        self.demat();
        let total = self.len() + other.len();
        if total <= PVEC_SMALL_MAX {
            let mut nv: Vec<Value> = Vec::with_capacity(total);
            nv.extend(self.iter_cloned());
            nv.extend(other.iter_cloned());
            *self = PVec::Small(Arc::from(nv));
        } else {
            let mut items: Vec<Value> = self.iter_cloned().collect();
            items.extend(other.iter_cloned());
            *self = PVec::Big(Arc::new(champ::PVector::from_vec(items)));
        }
    }

    /// `skip`/`take` on `Big` route through `champ::PVector::slice`
    /// (an O(1) view) -- `slice` panics on an out-of-bounds range (unlike
    /// imbl's own saturating `skip`/`take`), so counts are clamped to
    /// `len` first.
    pub fn skip(&self, count: usize) -> Self {
        match self {
            PVec::Small(a) => PVec::Small(Arc::from(a.iter().skip(count).cloned().collect::<Vec<_>>())),
            PVec::Big(v) => {
                let len = v.len();
                let start = count.min(len);
                PVec::Big(Arc::new(v.slice(start..len)))
            }
            PVec::Col(c) => c.view(count, c.len()),
        }
    }

    pub fn take(&self, count: usize) -> Self {
        match self {
            PVec::Small(a) => PVec::Small(Arc::from(a.iter().take(count).cloned().collect::<Vec<_>>())),
            PVec::Big(v) => {
                let end = count.min(v.len());
                PVec::Big(Arc::new(v.slice(0..end)))
            }
            PVec::Col(c) => c.view(0, count),
        }
    }

    /// Mirrors `imbl::Vector::slice` for a plain `Range<usize>` (the only
    /// shape this crate calls it with). `Big` goes straight through
    /// `champ::PVector::slice` (a single O(1) view) rather than
    /// `skip`+`take` chained -- same clamping rationale as those two.
    pub fn slice(&self, range: std::ops::Range<usize>) -> Self {
        match self {
            PVec::Small(_) => self.skip(range.start).take(range.end.saturating_sub(range.start)),
            PVec::Big(v) => {
                let len = v.len();
                let start = range.start.min(len);
                let end = range.end.max(start).min(len);
                PVec::Big(Arc::new(v.slice(start..end)))
            }
            PVec::Col(c) => c.view(range.start, range.end.max(range.start)),
        }
    }

    /// `Big` has no native `split_off` -- two `slice()` views (both O(1))
    /// stand in for it.
    pub fn split_off(&mut self, index: usize) -> Self {
        self.demat();
        match self {
            PVec::Big(v) => {
                let len = v.len();
                let tail = PVec::Big(Arc::new(v.slice(index..len)));
                *v = Arc::new(v.slice(0..index));
                tail
            }
            PVec::Small(a) => {
                let tail: Vec<Value> = a[index..].to_vec();
                let head: Vec<Value> = a[..index].to_vec();
                *self = PVec::Small(Arc::from(head));
                PVec::Small(Arc::from(tail))
            }
            PVec::Col(_) => unreachable!("demat"),
        }
    }

    pub fn insert(&mut self, index: usize, value: Value) {
        self.demat();
        let total = self.len() + 1;
        if total <= PVEC_SMALL_MAX {
            if let PVec::Small(a) = self {
                let mut nv: Vec<Value> = Vec::with_capacity(total);
                nv.extend(a[..index].iter().cloned());
                nv.push(value);
                nv.extend(a[index..].iter().cloned());
                *self = PVec::Small(Arc::from(nv));
                return;
            }
        }
        let mut items: Vec<Value> = self.iter_cloned().collect();
        items.insert(index, value);
        *self = PVec::Big(Arc::new(champ::PVector::from_vec(items)));
    }

    pub fn remove(&mut self, index: usize) -> Value {
        self.demat();
        match self {
            PVec::Big(v) => {
                let mut items: Vec<Value> = v.iter().cloned().collect();
                let removed = items.remove(index);
                *v = Arc::new(champ::PVector::from_vec(items));
                removed
            }
            PVec::Small(a) => {
                let removed = a[index].clone();
                let mut nv: Vec<Value> = Vec::with_capacity(a.len() - 1);
                nv.extend(a[..index].iter().cloned());
                nv.extend(a[index + 1..].iter().cloned());
                *self = PVec::Small(Arc::from(nv));
                removed
            }
            PVec::Col(_) => unreachable!("demat"),
        }
    }

    pub fn swap(&mut self, i: usize, j: usize) {
        self.demat();
        match self {
            PVec::Big(v) => {
                let mut items: Vec<Value> = v.iter().cloned().collect();
                items.swap(i, j);
                *v = Arc::new(champ::PVector::from_vec(items));
            }
            PVec::Small(a) => {
                let mut nv: Vec<Value> = a.to_vec();
                nv.swap(i, j);
                *self = PVec::Small(Arc::from(nv));
            }
            PVec::Col(_) => unreachable!("demat"),
        }
    }

    /// Bound is `Fn` (not `FnMut`), matching `imbl::Vector::sort_by`
    /// exactly -- the DEAD-API `Big` arm below round-trips through a
    /// plain `Vec::sort_by`, which offers no weaker a guarantee, but the
    /// bound is kept as-is so a caller can't tell dead code from live by
    /// probing the signature.
    pub fn sort_by<F>(&mut self, cmp: F)
    where
        F: Fn(&Value, &Value) -> std::cmp::Ordering,
    {
        self.demat();
        match self {
            PVec::Big(v) => {
                let mut items: Vec<Value> = v.iter().cloned().collect();
                items.sort_by(cmp);
                *v = Arc::new(champ::PVector::from_vec(items));
            }
            PVec::Small(a) => {
                let mut nv: Vec<Value> = a.to_vec();
                nv.sort_by(cmp);
                *self = PVec::Small(Arc::from(nv));
            }
            PVec::Col(_) => unreachable!("demat"),
        }
    }

    pub fn contains(&self, value: &Value) -> bool {
        self.iter().any(|v| v == value)
    }

    pub fn index_of(&self, value: &Value) -> Option<usize> {
        self.iter().position(|v| v == value)
    }

    /// `identical?` support (mirrors `imbl::Vector::ptr_eq`): true iff `self`
    /// and `other` are the SAME underlying allocation -- same `Arc<[Value]>`
    /// when both are `Small`, same `champ::PVector` root/tail when
    /// both are `Big`. SEMANTIC DECISION (not specified by the stage-2
    /// brief): a `Small` and a `Big` holding equal elements are never
    /// `ptr_eq`, even though they're `==` -- this matches every other
    /// cell-backed `Value` variant here (two `Arc`s with equal contents but
    /// different allocations are `==` but not `identical?`), and matches
    /// imbl's own `Vector::ptr_eq` contract (structural-sharing identity,
    /// not value identity).
    pub fn ptr_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (PVec::Small(a), PVec::Small(b)) => Arc::ptr_eq(a, b),
            // split-box: still the INNER handle's own `ptr_eq` (root/tail
            // identity), NOT `Arc::ptr_eq` on the outer box -- two `PVec`s
            // that came from cloning one `Arc` share the root either way,
            // but two separately boxed clones of the same tree must keep
            // reporting `identical?` exactly as they did before this wave.
            // M10.1: `champ::PVector::ptr_eq` is an associated
            // function (`PVector::ptr_eq(a, b)`), not a `.ptr_eq()` method
            // the way imbl's is -- deref coercion turns the `&Arc<...>`s
            // here into the `&PVector<Value>`s it expects.
            (PVec::Big(a), PVec::Big(b)) => champ::PVector::ptr_eq(a, b),
            (PVec::Col(a), PVec::Col(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Instrumentation only (`MOVA_MAP_PROBE=1`; see `builtins::map_probe`'s
    /// section (D)): is this the only handle to the underlying allocation?
    /// `None` for `Big` -- `champ::PVector`'s root/tail refcounts are
    /// private and reachable through neither its public API nor `PoolRef`,
    /// and cloning to find out would destroy the very uniqueness being
    /// measured.
    /// metrics: refcount == 1 on any tier.
    #[inline]
    pub fn is_unique(&self) -> bool {
        match self {
            PVec::Small(a) => Arc::strong_count(a) == 1,
            PVec::Big(a) => Arc::strong_count(a) == 1,
            PVec::Col(a) => Arc::strong_count(a) == 1,
        }
    }

    pub fn small_is_unique(&self) -> Option<bool> {
        match self {
            PVec::Small(a) => Some(Arc::strong_count(a) == 1 && Arc::weak_count(a) == 0),
            PVec::Big(_) | PVec::Col(_) => None,
        }
    }
}

impl Default for PVec {
    fn default() -> Self {
        PVec::new()
    }
}

impl std::ops::Index<usize> for PVec {
    type Output = Value;
    fn index(&self, index: usize) -> &Value {
        self.get(index).expect("PVec index out of bounds")
    }
}

pub enum PVecIter<'a> {
    Small(std::slice::Iter<'a, Value>),
    Big(champ::PVecIter<'a, Value>),
}

impl<'a> Iterator for PVecIter<'a> {
    type Item = &'a Value;
    fn next(&mut self) -> Option<&'a Value> {
        match self {
            PVecIter::Small(i) => i.next(),
            PVecIter::Big(i) => i.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            PVecIter::Small(i) => i.size_hint(),
            PVecIter::Big(i) => i.size_hint(),
        }
    }
}

// M10.1 deviation (documented, not silently dropped): `DoubleEndedIterator`
// is no longer implemented -- `champ::PVecIter` is a leaf-chunk
// iterator (the fast path for equality/reduce/join) with no `next_back`,
// and a grep across mova confirmed zero callers of `.rev()`/`next_back`
// on a `PVec`-derived iterator -- nothing in the API surface actually
// needed it.

impl<'a> ExactSizeIterator for PVecIter<'a> {}

impl<'a> IntoIterator for &'a PVec {
    type Item = &'a Value;
    type IntoIter = PVecIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

pub enum PVecIntoIter {
    // `(array/vector, next-index)` rather than an eager `to_vec().
    // into_iter()`: clones each element lazily as it's consumed instead of
    // cloning the whole collection upfront (clippy::unnecessary_to_owned
    // catches the eager form -- and the lazy form is strictly cheaper for
    // a partially-drained iterator too). M10.1: `champ::PVector` has
    // no owned/consuming iterator (unlike `champ::PersistentHashMap`'s
    // `IntoIter`, which `PMapIntoIter::Big` uses) -- mirrors the `Small`
    // arm's own lazy-`get`-and-clone shape instead; `PVector::get` is
    // O(log32 n), cheap enough that this is not a hot-path concern.
    Small(Arc<[Value]>, usize),
    Big(champ::PVector<Value>, usize),
    Col(Arc<crate::colvec::ColVec>, usize),
}

/// M6: see [`PVec::iter_cloned`].
pub enum PVecCloned<'a> {
    Ref(PVecIter<'a>),
    Col(PVecIntoIter),
}

impl<'a> Iterator for PVecCloned<'a> {
    type Item = Value;
    #[inline]
    fn next(&mut self) -> Option<Value> {
        match self {
            PVecCloned::Ref(it) => it.next().cloned(),
            PVecCloned::Col(it) => it.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            PVecCloned::Ref(it) => it.size_hint(),
            PVecCloned::Col(it) => it.size_hint(),
        }
    }
}

impl<'a> ExactSizeIterator for PVecCloned<'a> {}

impl Iterator for PVecIntoIter {
    type Item = Value;
    fn next(&mut self) -> Option<Value> {
        match self {
            PVecIntoIter::Small(a, i) => {
                let v = a.get(*i)?.clone();
                *i += 1;
                Some(v)
            }
            PVecIntoIter::Big(v, i) => {
                let x = v.get(*i)?.clone();
                *i += 1;
                Some(x)
            }
            PVecIntoIter::Col(c, i) => {
                let x = c.elem(*i)?;
                *i += 1;
                Some(x)
            }
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            PVecIntoIter::Small(a, i) => {
                let rem = a.len() - i;
                (rem, Some(rem))
            }
            PVecIntoIter::Big(v, i) => {
                let rem = v.len() - i;
                (rem, Some(rem))
            }
            PVecIntoIter::Col(c, i) => {
                let rem = c.len() - *i;
                (rem, Some(rem))
            }
        }
    }
}

impl IntoIterator for PVec {
    type Item = Value;
    type IntoIter = PVecIntoIter;
    fn into_iter(self) -> Self::IntoIter {
        match self {
            PVec::Small(a) => PVecIntoIter::Small(a, 0),
            // split-box: take the `PVector` OUT of the box when this is
            // the only handle (the ordinary case for a consumed `PVec`),
            // and clone the handle only when a sibling still holds it --
            // never a blind `(*arc).clone()`.
            PVec::Big(v) => PVecIntoIter::Big(Arc::unwrap_or_clone(v), 0),
            PVec::Col(c) => PVecIntoIter::Col(c, 0),
        }
    }
}

impl FromIterator<Value> for PVec {
    fn from_iter<T: IntoIterator<Item = Value>>(iter: T) -> Self {
        let v: Vec<Value> = iter.into_iter().collect();
        if v.len() <= PVEC_SMALL_MAX {
            PVec::Small(Arc::from(v))
        } else {
            PVec::Big(Arc::new(champ::PVector::from_vec(v)))
        }
    }
}

impl PVec {
    /// Builds a `PVec` by CLONING out of a borrowed slice, for a caller
    /// that keeps (and recycles) its staging buffer -- the W4-diet
    /// counterpart of `From<Vec<Value>>` (`compile::exec::exec_vector`).
    /// One `Arc<[Value]>` allocation for the `Small` case, per-element
    /// refcount bumps for the copies.
    pub fn from_slice(s: &[Value]) -> Self {
        if s.len() <= PVEC_SMALL_MAX {
            PVec::Small(Arc::from(s))
        } else {
            PVec::Big(Arc::new(champ::PVector::from_slice(s)))
        }
    }

    /// S7: exactly two elements, in ONE allocation.
    ///
    /// `pvec![k, v]` (the shape every map-entry construction site used
    /// before [`Value::MapEntry`]) expands to
    /// `PVec::from_iter(vec![k, v])`, which allocates a `Vec` and then
    /// `Arc::from`s it -- TWO allocations plus a 144-byte copy, per
    /// entry, on `Interp::seq_items`'s per-entry loop. `Arc::new([k, v])`
    /// builds the `Arc<[Value; 2]>` directly and unsize-coerces to
    /// `Arc<[Value]>` for free (the length moves into the fat pointer at
    /// the coercion, no runtime work), so this is one allocation and no
    /// copy. See `bench/mapseq-iter.mova` for the instrument.
    #[inline]
    pub fn pair(k: Value, v: Value) -> Self {
        let boxed: Arc<[Value; 2]> = Arc::new([k, v]);
        PVec::Small(boxed)
    }
}

impl Extend<Value> for PVec {
    fn extend<T: IntoIterator<Item = Value>>(&mut self, iter: T) {
        for v in iter {
            self.push_back(v);
        }
    }
}

impl From<Vec<Value>> for PVec {
    fn from(v: Vec<Value>) -> Self {
        PVec::from_iter(v)
    }
}

/// REPRESENTATION-BLIND (stage-2 invariant): a `Small` and a `Big` holding
/// the same elements in the same order must be `==`. M10.1: `Big`-vs-`Big`
/// delegates to `champ::PVector`'s own `PartialEq` (a `ptr_eq`
/// shortcut, then a subtree-identity-pruned walk -- imbl's own `PartialEq`
/// had no such shortcut, so this is a free win the swap unlocks) rather
/// than the manual `len`+`zip` walk, which would bypass it. `Arc<T>`'s own
/// `PartialEq` compares through the pointee, so `a == b` below calls
/// straight into that. `Small`-vs-`Small`/mixed still falls through to the
/// manual element-wise walk (representation-blind, never short-circuits
/// on which variant either side is in).
impl PartialEq for PVec {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (PVec::Big(a), PVec::Big(b)) => a == b,
            _ if self.ptr_eq(other) => true,
            (PVec::Col(_), _) | (_, PVec::Col(_)) => {
                self.len() == other.len() && self.clone().into_iter().zip(other.clone()).all(|(a, b)| a == b)
            }
            _ => self.len() == other.len() && self.iter().zip(other.iter()).all(|(a, b)| a == b),
        }
    }
}

impl Eq for PVec {}

impl std::fmt::Debug for PVec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Drop-in replacement for `crate::pvec![...]` literals against `PVec`.
///
/// Expands through `$crate::internal::PVec` (not `$crate::value::PVec`):
/// `value` is `pub(crate)` (EMBED-API-PLAN.md Phase E1 merge), and this
/// macro is `#[macro_export]`ed so white-box test files that invoke it as
/// `mova::pvec![...]` are expanding it from outside the crate -- the path
/// it expands to must be reachable from there too, same reasoning as the
/// `internal` re-exports themselves.
#[macro_export]
macro_rules! pvec {
    () => { $crate::internal::PVec::new() };
    ($($x:expr),+ $(,)?) => {
        $crate::internal::PVec::from_iter(vec![$($x),+])
    };
}

/// Stage 2c (dual representation, same rationale as `PVec` above, applied
/// to `Value::Map`): mirrors Clojure's own `PersistentArrayMap` ->
/// `PersistentHashMap` split. `Small` is a linear-scan assoc vector (cheap
/// at this size -- no HAMT node/hashing overhead, and `get` on <=8 entries
/// via `Value` equality is faster than hashing, L1-resident); `Big` is a
/// `champ::PersistentHashMap<Value, Value>` CHAMP HAMT (M5: swapped
/// in for the previously-vendored `imbl::HashMap`; deterministic default
/// hasher, so structural equality is meaningful -- see that type's own
/// equality-contract doc), entered once an `assoc` would grow past the
/// threshold and NEVER exited (`dissoc` on `Big` never demotes -- matches
/// Clojure's own array-map/hash-map promotion policy exactly, including the
/// one-way ratchet).
///
/// W-GEO stage 4 (split-box, `docs/W-GEO-STAGE4-SPLITBOX.md`): `Big`'s
/// payload lives behind an `Arc`, taking `size_of::<PMap>()` from 24 to
/// 16. Same rationale (and same `Arc::make_mut`-on-shared caveat) as
/// [`PVec`]'s doc above; `Small` is untouched.
#[derive(Clone)]
pub enum PMap {
    Small(Arc<Vec<(Value, Value)>>),
    Big(Arc<champ::PersistentHashMap<Value, Value>>),
    /// M3: 9..=SHAPED_MAX interned-keyword keys; shared shape + values only. Iterates in CHAMP order (same as `Big`).
    Shaped(crate::shaped_map::ShapedMap),
}

/// Fast path for `PMap::Small`'s linear key scan (measured top cost, native
/// floor probe): same address, or both `Keyword`s with an interned id, skip
/// straight past `Value::eq`'s full ~30-arm match. Exact semantics -- falls
/// back to `a == b` for everything else, so this is a speedup only.
#[inline]
fn pmap_key_eq(a: &Value, b: &Value) -> bool {
    if std::ptr::eq(a, b) {
        return true;
    }
    if let (Value::Keyword(ka), Value::Keyword(kb)) = (a, b) {
        if let (Some(ia), Some(ib)) = (ka.interned_id(), kb.interned_id()) {
            return ia == ib;
        }
    }
    a == b
}

/// Small holds at most this many entries before `assoc` promotes to `Big`
/// -- Clojure's own `PersistentArrayMap` threshold (`clojure.lang.RT`'s
/// `HASHTABLE_THRESHOLD` counterpart), not a stage-1-derived number like
/// `PVEC_SMALL_MAX`.
pub const PMAP_SMALL_MAX: usize = 8;

impl PMap {
    pub fn new() -> Self {
        // M3: one shared empty-map allocation (31k empty maps in lib/'s analysis).
        static EMPTY: std::sync::OnceLock<Arc<Vec<(Value, Value)>>> = std::sync::OnceLock::new();
        PMap::Small(EMPTY.get_or_init(|| Arc::new(Vec::new())).clone())
    }

    pub fn len(&self) -> usize {
        match self {
            PMap::Small(a) => a.len(),
            PMap::Big(m) => m.len(),
            PMap::Shaped(s) => s.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, k: &Value) -> Option<&Value> {
        #[cfg(feature = "geo-census")]
        crate::geo_census::pmap_access();
        match self {
            // Linear scan with `Value` equality, per the stage-2 brief --
            // deliberately NOT hashing even though `k` could be hashed;
            // at <=8 entries the scan is the whole point (L1-resident,
            // beats HAMT descent).
            PMap::Small(a) => a.iter().find(|(k2, _)| pmap_key_eq(k2, k)).map(|(_, v)| v),
            PMap::Big(m) => m.get(k),
            PMap::Shaped(s) => s.get(k),
        }
    }

    pub fn contains_key(&self, k: &Value) -> bool {
        self.get(k).is_some()
    }

    /// Builds the `Big` representation via `TransientMap` (M5: champ's
    /// owned builder mutates uniquely-owned nodes in place as it goes,
    /// rather than doing one path-copy per inserted entry the way a chain
    /// of persistent `assoc`s would).
    fn to_big(&self) -> champ::PersistentHashMap<Value, Value> {
        match self {
            PMap::Small(a) => {
                let mut t = champ::PersistentHashMap::new().transient();
                for (k, v) in a.iter() {
                    t.assoc(k.clone(), v.clone());
                }
                t.persistent()
            }
            // split-box: the BARE map, same as `PVec::to_big` -- callers
            // mutate the result before boxing it, and `self` must stay
            // valid, so the CHAMP handle is cloned exactly as before.
            PMap::Big(m) => (**m).clone(),
            PMap::Shaped(_) => {
                let mut t = champ::PersistentHashMap::new().transient();
                for (k, v) in self.iter() {
                    t.assoc(k.clone(), v.clone());
                }
                t.persistent()
            }
        }
    }

    /// Mirrors `imbl::HashMap::insert`: returns the OLD value if `k` was
    /// already present. An update to an EXISTING key in `Small` replaces
    /// in place (copy-on-write of the small `Vec`, length unchanged, never
    /// promotes on its own) -- only a genuinely NEW key can push `Small`
    /// past the threshold.
    pub fn insert(&mut self, k: Value, v: Value) -> Option<Value> {
        #[cfg(feature = "geo-census")]
        crate::geo_census::pmap_churn();
        match self {
            // M5: MOVE the map out of `self` (leaving a cheap, allocation-
            // free `PersistentHashMap::new()` placeholder behind) and thread
            // it through `assoc_owned_replacing` -- the owned/consuming
            // entry point that mutates uniquely-owned nodes along the
            // update path in place instead of path-copying. Going through
            // `&self`-`assoc` here (clone-then-assoc) would silently defeat
            // that fast path for every `Big` receiver reached via
            // `builtins::reuse`'s owned calling convention.
            // split-box: `make_mut` first, then the SAME move-through-
            // owned-op shape. When this box is uniquely held (the common
            // case) `make_mut` is one refcount check and the CHAMP map is
            // moved out with its node refcounts untouched, so
            // `assoc_owned_replacing` keeps its in-place fast path exactly
            // as before. When a sibling holds the box, `make_mut` clones
            // the CHAMP handle -- which bumps the node refcounts, so the
            // owned op path-copies, which is precisely what the un-boxed
            // code did for a shared map too.
            PMap::Big(m) => {
                let slot = Arc::make_mut(m);
                let owned = std::mem::replace(slot, champ::PersistentHashMap::new());
                let (new_map, old) = owned.assoc_owned_replacing(k, v);
                *slot = new_map;
                old
            }
            PMap::Shaped(s) => {
                if let Some(i) = s.shape().index_of(&k) {
                    return Some(s.set(i, v));
                }
                if let Some(n) = s.with_new(&k, v.clone()) {
                    *self = PMap::Shaped(n);
                    return None;
                }
                let (big, old) = self.to_big().assoc_owned_replacing(k, v);
                *self = PMap::Big(Arc::new(big));
                old
            }
            PMap::Small(a) => {
                if let Some(pos) = a.iter().position(|(k2, _)| pmap_key_eq(k2, &k)) {
                    // REUSE (Perceus-lite phase 1, see `builtins::reuse`):
                    // `make_mut` mutates the entry vector in place when this
                    // `Arc` is the only handle to it, and clones it (the
                    // pre-existing behaviour, <=8 entries) when it isn't --
                    // so a caller that owns its receiver exclusively pays no
                    // copy at all, and a caller that doesn't is unaffected.
                    let nv = Arc::make_mut(a);
                    Some(std::mem::replace(&mut nv[pos].1, v))
                } else if a.len() < PMAP_SMALL_MAX {
                    // REUSE, but deliberately NOT via `make_mut` here: for a
                    // GROWING insert `make_mut` would clone the vector at
                    // exactly its current capacity and the `push` would then
                    // immediately reallocate it -- two allocations where the
                    // pre-reuse code did one. So: push in place only when the
                    // handle is unique (amortized O(1), the whole point), and
                    // otherwise fall back to the original exact-capacity
                    // clone-and-extend, which is unchanged.
                    if let Some(entries) = Arc::get_mut(a) {
                        // `reserve_exact`, NOT a bare `push`: `Vec`'s growth
                        // policy allocates a MINIMUM of 4 slots on the first
                        // push (`RawVec`'s floor for `T` <= 1024 bytes), and
                        // a `(Value, Value)` pair is ~144 bytes -- so a bare
                        // push would hand every one-entry map literal
                        // (`{:c c2}`: E3 (A) found these are ~100% of the
                        // flow benches' map traffic) a 4-slot allocation
                        // where the pre-reuse code allocated exactly 1.
                        // Measured: 1.29x REGRESSION on flow-gen-sink before
                        // this line existed. Exact-sizing keeps allocation
                        // volume identical to the old path while still
                        // mutating in place.
                        if entries.len() == entries.capacity() {
                            entries.reserve_exact(1);
                        }
                        entries.push((k, v));
                        return None;
                    }
                    let mut nv: Vec<(Value, Value)> = Vec::with_capacity(a.len() + 1);
                    nv.extend(a.iter().cloned());
                    nv.push((k, v));
                    *self = PMap::Small(Arc::new(nv));
                    None
                } else {
                    // Promotion: `big` is a fresh, uniquely-owned map built
                    // by `to_big()` (nobody else can hold a reference to
                    // it yet), so the owned/`assoc_owned_replacing` entry
                    // point gets the in-place fast path for free. `k` is
                    // never already present here (the `position` check
                    // above already ruled that out for the `Small` source),
                    // so `old` is always `None` in practice; go through the
                    // general path anyway rather than assume it.
                    // field4/W-LENS-1: unconditional, for the same reason
                    // as `PVec::push_back`'s promotion counter.
                    crate::lens::event(crate::lens::Event::PMapPromote);
                    let pairs = a.iter().map(|(k, v)| (k, v)).chain(std::iter::once((&k, &v)));
                    if let Some(sm) = crate::shaped_map::ShapedMap::from_pairs(pairs) {
                        *self = PMap::Shaped(sm);
                        return None;
                    }
                    let big = self.to_big();
                    let (big, old) = big.assoc_owned_replacing(k, v);
                    *self = PMap::Big(Arc::new(big));
                    old
                }
            }
        }
    }

    /// Non-mutating `insert` (mirrors `imbl::HashMap::update`). `self` must
    /// remain valid for the caller either way, so `Big` goes straight
    /// through champ's `&self` `assoc` (pure path-copy, or a
    /// zero-allocation pointer-identical return when the key/value pair is
    /// already exactly present) rather than through `insert`'s owned/
    /// `mem::replace` machinery -- that machinery exists specifically to
    /// unlock in-place mutation, which a non-mutating op has no use for.
    pub fn update(&self, k: Value, v: Value) -> Self {
        match self {
            PMap::Big(m) => PMap::Big(Arc::new(m.assoc(k, v))),
            PMap::Small(_) | PMap::Shaped(_) => {
                let mut c = self.clone();
                c.insert(k, v);
                c
            }
        }
    }

    /// `dissoc`. Per the stage-2 brief: removing from `Big` NEVER demotes
    /// back to `Small`, even if the result has <=8 entries -- Clojure
    /// parity (a `PersistentHashMap` that shrinks stays a hash-map).
    pub fn remove(&mut self, k: &Value) -> Option<Value> {
        match self {
            // M5: same MOVE-through-owned-op shape as `insert` above, so a
            // caller reaching this through the owned calling convention
            // (`builtins::reuse`, e.g. `dissoc`'s consuming entry point)
            // still gets `dissoc_owned`'s in-place fast path. champ
            // has no `dissoc_owned_replacing` (only `assoc` needed one, to
            // avoid a second descent on the hot `assoc`/`update` path), so
            // the old value is read via one extra `get` before the move.
            // split-box: `make_mut` first, same reasoning as `insert`'s
            // `Big` arm above.
            PMap::Big(m) => {
                let old = m.get(k).cloned();
                let slot = Arc::make_mut(m);
                let owned = std::mem::replace(slot, champ::PersistentHashMap::new());
                *slot = owned.dissoc_owned(k);
                old
            }
            PMap::Shaped(s) => {
                let i = s.shape().index_of(k)?;
                let old = s.vals()[i].clone();
                *self = match s.without_slot(i) {
                    Some(n) => PMap::Shaped(n),
                    None => PMap::Big(Arc::new(self.to_big().dissoc_owned(k))),
                };
                Some(old)
            }
            PMap::Small(a) => {
                let pos = a.iter().position(|(k2, _)| pmap_key_eq(k2, k))?;
                // REUSE: in place when uniquely held, clone-then-remove
                // otherwise. See `insert` above and `builtins::reuse`.
                let (_, old) = Arc::make_mut(a).remove(pos);
                Some(old)
            }
        }
    }

    /// Non-mutating `remove` (mirrors `imbl::HashMap::without`). Same
    /// rationale as `update` above: `Big` goes straight through
    /// champ's `&self` `dissoc` rather than `remove`'s owned/
    /// `mem::replace` machinery.
    pub fn without(&self, k: &Value) -> Self {
        match self {
            PMap::Big(m) => PMap::Big(Arc::new(m.dissoc(k))),
            PMap::Small(_) | PMap::Shaped(_) => {
                let mut c = self.clone();
                c.remove(k);
                c
            }
        }
    }

    pub fn iter(&self) -> PMapIter<'_> {
        match self {
            PMap::Small(a) => PMapIter::Small(a.iter()),
            PMap::Big(m) => PMapIter::Big(Box::new(m.iter())),
            PMap::Shaped(s) => PMapIter::Shaped(s.shape().keys.iter().zip(s.vals().iter())),
        }
    }

    pub fn keys(&self) -> PMapKeys<'_> {
        PMapKeys(self.iter())
    }

    pub fn values(&self) -> PMapValues<'_> {
        PMapValues(self.iter())
    }

    /// Same representation-blind-false-across-variants contract as
    /// `PVec::ptr_eq` -- see that method's doc for the rationale.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (PMap::Small(a), PMap::Small(b)) => Arc::ptr_eq(a, b),
            // split-box: champ's own root identity, not the box's --
            // see `PVec::ptr_eq`'s `Big` arm for the full reasoning.
            (PMap::Big(a), PMap::Big(b)) => (**a).ptr_eq(b),
            (PMap::Shaped(a), PMap::Shaped(b)) => a.ptr_eq(b),
            _ => false,
        }
    }

    /// Instrumentation only -- same contract and same `None`-for-`Big`
    /// caveat as [`PVec::small_is_unique`].
    /// metrics: refcount == 1 on any tier.
    #[inline]
    pub fn is_unique(&self) -> bool {
        match self {
            PMap::Small(a) => Arc::strong_count(a) == 1,
            PMap::Big(a) => Arc::strong_count(a) == 1,
            PMap::Shaped(m) => m.is_unique(),
        }
    }

    pub fn small_is_unique(&self) -> Option<bool> {
        match self {
            PMap::Small(a) => Some(Arc::strong_count(a) == 1 && Arc::weak_count(a) == 0),
            PMap::Big(_) | PMap::Shaped(_) => None,
        }
    }

    /// Bulk-builds a `PMap` from an already-collected `Vec<(Value,
    /// Value)>` in one pass, for callers (the `serde` embedding bridge's
    /// struct/map serializers, so far) that build their entries into a
    /// correctly-capacity'd `Vec` up front and just need it turned into a
    /// `PMap` -- WITHOUT going through [`FromIterator::from_iter`]'s
    /// repeated single-key [`Self::insert`] calls.
    ///
    /// That repeated-`insert` path is real, measured waste for a bulk
    /// build: starting from an empty `Small`, each of the first
    /// [`PMAP_SMALL_MAX`] inserts individually `reserve_exact(1)`s the
    /// backing `Vec` (by design -- see `insert`'s doc: a bare `push` would
    /// over-allocate the common one-entry map-literal case), so a 10-field
    /// struct's `to_value` was reconstructing an already-correctly-sized
    /// `Vec` via 8 separate reallocations, THEN promoting to `Big` via
    /// `to_big()`, THEN inserting the remaining entries one at a time --
    /// measured at ~9 of a flat 10-field struct's ~34 allocator calls
    /// (`benches/serde_alloc_profile.rs` in the `embed/probe-tovalue-perf`
    /// branch). Building `Small` directly from the already-sized `Vec`
    /// (one `Arc::new`, zero extra reallocations) or `Big` directly via one
    /// transient pass (skipping the wasted Small-then-promote detour)
    /// avoids all of that.
    ///
    /// Despite the name, this does NOT assume `pairs` has no duplicate
    /// keys -- it's just optimized for the common case (all of
    /// `serde_bridge`'s callers: struct fields are distinct identifiers by
    /// construction; a generic map's keys come from a real `HashMap`/
    /// `BTreeMap`, unique unless a pathological custom `Serialize` maps two
    /// distinct keys to the same encoded `Value`) where there aren't any.
    /// A genuine duplicate is still handled correctly either way: the
    /// `<= PMAP_SMALL_MAX` path does a linear-scan dedup (identical
    /// last-value-wins semantics to `PMap::insert`, and just as cheap at
    /// this size as `insert`'s own linear scan); the `Big` path's
    /// `TransientMap::assoc` naturally overwrites on a repeated key, same
    /// as `to_big()`'s existing use of it elsewhere in this file.
    ///
    /// Not `#[cfg(feature = "serde")]`-gated (despite the doc above being
    /// written for `serde_bridge`'s callers): `host_struct::as_pmap` --
    /// unconditionally compiled, `Value::HostStruct`'s ONE materialize
    /// choke point -- needs this exact bulk-build shape too, and it has no
    /// dependency on `serde` itself (only on `Value`/`PMap`).
    pub(crate) fn from_unique_pairs(pairs: Vec<(Value, Value)>) -> PMap {
        if pairs.len() <= PMAP_SMALL_MAX {
            let mut out: Vec<(Value, Value)> = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                if let Some(slot) = out.iter_mut().find(|(k2, _)| pmap_key_eq(&*k2, &k)) {
                    slot.1 = v;
                } else {
                    out.push((k, v));
                }
            }
            PMap::Small(Arc::new(out))
        } else {
            let mut t = champ::PersistentHashMap::new().transient();
            for (k, v) in pairs {
                t.assoc(k, v);
            }
            PMap::Big(Arc::new(t.persistent()))
        }
    }
}

impl Default for PMap {
    fn default() -> Self {
        PMap::new()
    }
}

pub enum PMapIter<'a> {
    Small(std::slice::Iter<'a, (Value, Value)>),
    // Boxed: `champ::Iter` carries its own depth-7 frame-stack cursor
    // (~264 bytes), which would otherwise more than 10x the whole `PMapIter`
    // enum's size just for the rarely-relevant `Small` arm's sake too
    // (`clippy::large_enum_variant`).
    Big(Box<champ::Iter<'a, Value, Value>>),
    Shaped(std::iter::Zip<std::slice::Iter<'a, Value>, std::slice::Iter<'a, Value>>),
}

impl<'a> Iterator for PMapIter<'a> {
    type Item = (&'a Value, &'a Value);
    fn next(&mut self) -> Option<(&'a Value, &'a Value)> {
        match self {
            PMapIter::Small(i) => i.next().map(|(k, v)| (k, v)),
            PMapIter::Big(i) => i.next(),
            PMapIter::Shaped(i) => i.next(),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            PMapIter::Small(i) => i.size_hint(),
            PMapIter::Big(i) => i.size_hint(),
            PMapIter::Shaped(i) => i.size_hint(),
        }
    }
}

pub struct PMapKeys<'a>(PMapIter<'a>);
impl<'a> Iterator for PMapKeys<'a> {
    type Item = &'a Value;
    fn next(&mut self) -> Option<&'a Value> {
        self.0.next().map(|(k, _)| k)
    }
}

pub struct PMapValues<'a>(PMapIter<'a>);
impl<'a> Iterator for PMapValues<'a> {
    type Item = &'a Value;
    fn next(&mut self) -> Option<&'a Value> {
        self.0.next().map(|(_, v)| v)
    }
}

impl<'a> IntoIterator for &'a PMap {
    type Item = (&'a Value, &'a Value);
    type IntoIter = PMapIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

pub enum PMapIntoIter {
    Small(Arc<Vec<(Value, Value)>>, usize),
    Big(champ::IntoIter<Value, Value>),
    Shaped(crate::shaped_map::ShapedMap, usize),
}

impl Iterator for PMapIntoIter {
    type Item = (Value, Value);
    fn next(&mut self) -> Option<(Value, Value)> {
        match self {
            PMapIntoIter::Small(a, i) => {
                let pair = a.get(*i)?.clone();
                *i += 1;
                Some(pair)
            }
            PMapIntoIter::Big(i) => i.next(),
            PMapIntoIter::Shaped(s, i) => {
                let j = *i;
                let v = s.vals().get(j)?.clone();
                *i += 1;
                Some((s.shape().keys[j].clone(), v))
            }
        }
    }
}

impl IntoIterator for PMap {
    type Item = (Value, Value);
    type IntoIter = PMapIntoIter;
    fn into_iter(self) -> Self::IntoIter {
        match self {
            PMap::Small(a) => PMapIntoIter::Small(a, 0),
            // split-box: unwrap the box when uniquely held, clone the
            // CHAMP handle only when it isn't (see `PVec`'s `IntoIterator`).
            PMap::Big(m) => PMapIntoIter::Big(Arc::unwrap_or_clone(m).into_iter()),
            PMap::Shaped(s) => PMapIntoIter::Shaped(s, 0),
        }
    }
}

impl FromIterator<(Value, Value)> for PMap {
    fn from_iter<T: IntoIterator<Item = (Value, Value)>>(iter: T) -> Self {
        let mut out = PMap::new();
        for (k, v) in iter {
            out.insert(k, v);
        }
        out
    }
}

impl Extend<(Value, Value)> for PMap {
    fn extend<T: IntoIterator<Item = (Value, Value)>>(&mut self, iter: T) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

/// REPRESENTATION-BLIND (stage-2 invariant, mirrors `PVec`'s): unordered --
/// same length and every key in `self` maps to an equal value in `other`.
/// A `Small` and a `Big` holding the same pairs must be `==`.
impl PartialEq for PMap {
    fn eq(&self, other: &Self) -> bool {
        // K1: identity shortcut -- champ's assoc compares old/new values with `==`.
        self.ptr_eq(other) || (self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v)))
    }
}

impl Eq for PMap {}

impl std::fmt::Debug for PMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Drop-in replacement for `imbl::hashmap!{...}` literals against `PMap`.
///
/// Expands through `$crate::internal::PMap` (not `$crate::value::PMap`) --
/// see `pvec!`'s doc just above for why.
#[macro_export]
macro_rules! pmap {
    () => { $crate::internal::PMap::new() };
    ($($k:expr => $v:expr),+ $(,)?) => {
        $crate::internal::PMap::from_iter(vec![$(($k, $v)),+])
    };
}

/// S4 (sorted-colls + vector-of): the ordering rule a `Value::SortedMap`/
/// `Value::SortedSet` was built with. `Default` is Clojure's own "natural
/// order" comparator (`builtins::sorted::natural_compare`), which -- MEASURED
/// against 1.13.0-alpha6 -- is stricter than the public `compare` fn: it
/// eagerly rejects any key that isn't `nil`/a number/a string/keyword/
/// symbol/char/bool/vector on EVERY insert (even the very first, into an
/// otherwise-empty collection -- measured `(sorted-set {})` throws
/// `ClassCastException` despite never needing to compare two keys), where
/// `Fn` (a `sorted-map-by`/`sorted-set-by` comparator) only ever throws when
/// two keys are actually compared against each other. `Fn` stores the raw
/// mova callable and is invoked through the interpreter (`builtins::sorted::
/// cmp_via`) using the exact two-call bool/number protocol real Clojure's
/// `AFunction.compare` uses (same protocol `builtins::seq`'s `sort`
/// comparator already reproduces, just widened to return `Equal` instead of
/// collapsing everything non-`Less` to `Greater`).
#[derive(Clone)]
pub enum Comparator {
    Default,
    Fn(Value),
}

/// S4: `sorted-map`/`sorted-map-by`'s backing store -- ascending `Vec` of
/// `(key, val)` pairs, comparator-sorted, kept sorted incrementally by
/// `builtins::sorted`'s insert/remove helpers (binary search, since v1 is
/// "sorted-Vec + binary-search repr", conformance before perf -- see that
/// module's doc). Comparator-EQUAL-but-not-`=`-equal keys dedup to the
/// FIRST-inserted key on a later `assoc` (measured: Java `TreeMap` keeps the
/// old key object, only replaces the value -- `(assoc (sorted-map 1 :a) 1.0
/// :b)` is `{1 :b}`, not `{1.0 :b}`). `PartialEq`/`Hash` on the owning
/// `Value::SortedMap` deliberately do NOT consult `cmp` at all -- content
/// equality is unordered-pairs-of-`entries`, exactly like a plain `Map`
/// (measured: `(= (sorted-map-by > 1 :a) (sorted-map-by < 1 :a))` is true),
/// so no interpreter access is needed to compare or hash a sorted map.
pub struct SortedMapVal {
    pub cmp: Comparator,
    pub entries: Vec<(Value, Value)>,
}

/// S4: `sorted-set`/`sorted-set-by`'s backing store -- same shape/policy as
/// `SortedMapVal` above, just elements instead of pairs.
pub struct SortedSetVal {
    pub cmp: Comparator,
    pub entries: Vec<Value>,
}

/// C2 (defstruct): `struct`/`struct-map`'s backing store -- legacy
/// `clojure.lang.PersistentStructMap`. Follows `SortedMapVal`'s precedent
/// (flat `Vec<(Value, Value)>`, `=`/hash REUSE `Map`'s tag/algorithm --
/// see the `Value::StructMap` arms in `PartialEq`/`Hash` below, and
/// `value::SortedMapVal`'s own doc for why that's safe: content-only
/// equality, no interpreter access needed), but with a fixed LAYOUT
/// instead of a sort order: `entries[0..basis.len()]` are ALWAYS the
/// basis keys, in basis declaration order, one slot per basis key,
/// keyword identity never changing after construction (only the paired
/// value does, e.g. under `assoc`); `entries[basis.len()..]` are
/// extension keys, in first-introduction order (measured:
/// `(struct-map basis :ext 1 :a 2 :b 3)` still prints basis-first --
/// `{:a 2, :b 3, :ext 1}` -- call-order does not reorder the basis
/// slots). A basis value defaults to `nil` when a `struct`/`struct-map`
/// call didn't supply one (measured: `(struct basis 1)` with a 2-key
/// basis is `{:a 1, :b nil}`). `basis` is `Arc`-shared with the
/// `Value::StructBasis` `create-struct` produced it from -- `accessor`
/// keys its "Accessor/struct mismatch" check on `Arc::ptr_eq` against
/// that SAME basis object (measured: two `create-struct` calls with
/// identical keys are NOT accessor-compatible; two structs built from the
/// SAME `defstruct`/`create-struct` call ARE, regardless of `struct` vs
/// `struct-map` construction or which extension keys either carries).
/// See `builtins::structmap`'s module doc for the full measured matrix
/// (dissoc-base-key error text, `struct` arg-count overflow, `conj`/
/// `into`/`empty` behavior, `(s :key)` map-as-fn, ...).
/// W4-EVAL task 2: `basis` is `Vec<Value>`, not `Vec<Str>` -- real
/// Clojure's `create-struct` never type-checks its keys (`.oracle/
/// clojure-src/.../PersistentStructMap.java`'s `createSlotMap` takes an
/// `ISeq` of arbitrary key OBJECTS), and `entryAt`/`seq` hand back that
/// EXACT stored key object, metadata and all -- measured: `(defstruct s
/// (with-meta 'k {:a "A"}))` then `(meta (key (find (struct s 1) 'k)))`
/// is `{:a "A"}`, even though `find` was called with a bare (unmeta'd)
/// `'k`. A prior pass (D3, see `builtins::structmap::require_keyword`'s
/// history) widened key acceptance to a symbol but stored only its bare
/// NAME as a `Str`, which is what threw the metadata away; `Value` is the
/// minimum representation that can carry it back out. A plain keyword
/// basis (the overwhelmingly common case, and every OTHER struct-map
/// corpus form) behaves identically either way, since `Value::Keyword`
/// implements the same equality it always did.
pub struct StructMapVal {
    pub basis: Arc<Vec<Value>>,
    pub entries: Vec<(Value, Value)>,
}

/// S4: the primitive element kind a `(vector-of :kind ...)` was built with
/// (`clojure.core/vector-of`'s first argument). Coercion rules per kind live
/// in `builtins::sorted::coerce_for_kind` (measured against 1.13.0-alpha6:
/// truncating numeric casts sharing `builtins::numbers::cast_integral`'s
/// exact range-check table, `nil` always `NullPointerException`-flavored
/// regardless of kind, `:char` accepting an in-range `Int` as a Unicode
/// code point the same direction `:int` accepts a `Char` as its code point).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VecOfKind {
    Boolean,
    Byte,
    Short,
    Int,
    Long,
    Float,
    Double,
    Char,
}

/// S4: `(vector-of :kind & elems)`'s backing store. `data` holds elements
/// ALREADY coerced to `kind`'s `Value` representation (e.g. every element of
/// a `:char` vec is a `Value::Char`, every element of an `:int` vec a
/// `Value::Int` in `i32` range) -- deliberately NOT a separate unboxed
/// buffer, so printing/`=`/hash/iteration are free rides on `List`/
/// `Vector`'s own machinery (a `TypedVec` is `=` to a plain `Vector` with
/// the same elements, measured, and must hash identically per `Hash`'s
/// contract -- see the `Value::TypedVec` arms in `PartialEq`/`Hash` below).
/// `kind` is consulted ONLY at construction/`conj`/`assoc`/`into` time, to
/// coerce (or reject) the incoming element -- never at read time.
pub struct TypedVecVal {
    pub kind: VecOfKind,
    pub data: PVec,
}

/// C7 (vecveneer): which of the two vector-derived `ISeq` shapes a
/// [`VecSeqVal`] is -- see `Value::VecSeq`'s doc for why they share one
/// variant. `RSeq` is `(.rseq v)`'s result (real class
/// `clojure.lang.APersistentVector$RSeq`); `Chunked` is `(.chunkedNext
/// (seq v))`'s (real class `clojure.core.VecSeq`, `PersistentVector`'s own
/// chunked-seq type). Both print/compare/walk identically (plain
/// element-order `ISeq`s); only `class` and a couple of dot-methods
/// (`.index` is RSeq-only in real Clojure, though nothing in scope calls
/// it on a `Chunked` value either way) read this tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VecSeqKind {
    RSeq,
    Chunked,
}

/// C7 (vecveneer): `Value::VecSeq`'s cell. `items` is the REMAINING
/// elements in seq order (head-first) -- for `RSeq`, already reversed
/// relative to the source vector (measured: `(.rseq [0 1 2])` is `(2 1
/// 0)`, `items` = `[2 1 0]`); for `Chunked`, the tail past the skipped
/// first chunk (measured: `(.chunkedNext (seq (vec (range 100))))` has
/// `.count` 68 = 100 - 32, `items` holds those 68 elements). `.index`
/// (RSeq) and `.count` (both kinds) are `items.len() - 1` /`items.len()`
/// -- DERIVED from `items`, not a separate field, since both are exactly
/// "how many elements are left", which `items.len()` already answers
/// (see `builtins::vecdot`'s dot-method table). `.next` peels one element
/// off the FRONT and returns another `VecSeq` of the SAME `kind` (`Nil`
/// once `items` would become empty) -- matches real Clojure, where
/// `(rest (.rseq v))` stays an `RSeq` for as long as it has elements.
pub struct VecSeqVal {
    pub kind: VecSeqKind,
    pub items: PVec,
}

/// Registry of every keyword string (`Value::Keyword`'s flat `"ns/name"` or
/// `"name"` repr) this interpreter has constructed, backing `find-keyword`
/// (`clojure.core`: `Keyword/find` looks up the JVM-wide intern table --
/// mova keywords (above) are plain values with no such table, so this is
/// the closest per-`Interp` equivalent). Lives as an `Interp` field
/// (`eval::Interp::keywords`), SHARED across `fork` (a keyword minted on
/// either side of a `future*` boundary must be findable from both,
/// mirroring `types::Protocols`/`multi::Multimethods`), DEEP-COPIED on
/// `snapshot` (own `Arc<RwLock<..>>` around a `Clone` of the current set,
/// the same "own copy of shared-shape state" contract as
/// `types::Protocols`/`types::Interfaces`/`multi::Multimethods`): a
/// keyword minted before the snapshot stays `find-keyword`-able on BOTH
/// engines afterwards, while one minted on either side AFTER the snapshot
/// is invisible to the other -- see `Interp::fork`/`Interp::snapshot`.
///
/// Deliberately NEVER a Rust `static`: a process-wide table would leak
/// across `Engine::snapshot`'s independent engines (one engine's keywords
/// would spuriously be `find-keyword`-able from another's), which is
/// exactly the sharing bug `flow_registry`/`protocols`/`multimethods` all
/// document avoiding by living on `Interp` instead.
///
/// W-GEO stage 1 added a SECOND, genuinely process-wide keyword table
/// ([`crate::keyword`]'s capped-permanent intern table) and the two must
/// not be confused -- they answer different questions (design doc §3).
/// THIS one answers "has this interpreter's script ever constructed this
/// exact keyword", which is real per-engine semantics and stays scoped
/// exactly as the paragraph above describes: `Engine::snapshot` still
/// resets it, `fork` still shares it, and stage 1 changed neither its call
/// sites nor its behavior -- only the `Str` those call sites hand
/// [`KeywordRegistry::intern`] now comes from `Keyword::text_ref` instead
/// of straight out of the `Value::Keyword` payload. The intern table
/// answers "what number does this text map to", which has no per-engine
/// content at all, so it is a `static` and needs neither a `fork` nor a
/// `snapshot` verb.
///
/// Registration coverage (a documented deviation from real Clojure, not a
/// silent gap): real Clojure's reader interns EVERY keyword token the
/// instant it is READ, so any keyword appearing anywhere in loaded source
/// becomes `find-keyword`-able forever, whether or not it's ever
/// evaluated. mova instead interns at the points that actually construct
/// a `Value::Keyword` with an `Interp` in hand: the `keyword` builtin
/// (`builtins::strings::register`), and keyword LITERALS as they are
/// EVALUATED -- tree walk (`eval::Interp::eval_form_in`'s self-evaluating
/// atom arm) and the compiled-fn tier (`compile::resolve::Compiler`'s
/// `Ir::Const` bake, once per compile). A keyword literal that is read but
/// never evaluated (e.g. dead code, or unforced data nested in a `quote`d
/// form that itself is never walked back out through eval) is NOT
/// `find-keyword`-able under this narrower rule. Measured against the
/// vendored `keywords.clj` conformance test (session 5) -- see that
/// session's report for the pass/fail this coverage buys.
#[derive(Clone, Default)]
pub struct KeywordRegistry(Arc<RwLock<std::collections::HashSet<Str>>>);

impl KeywordRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `s` as an interned keyword string. Idempotent; cheap on the
    /// (overwhelmingly common) already-present path via a read-lock probe
    /// before taking the write lock.
    pub fn intern(&self, s: &Str) {
        if self.0.read().unwrap().contains(s) {
            return;
        }
        self.0.write().unwrap().insert(s.clone());
    }

    /// Whether `s` has been interned (`intern`ed) on this registry.
    pub fn contains(&self, s: &str) -> bool {
        self.0.read().unwrap().contains(s)
    }

    /// A deep, independent copy for `Interp::snapshot`'s FORK semantics --
    /// own `Arc<RwLock<..>>` around a `Clone` of the current set, the same
    /// shape as `types::Interfaces::snapshot` (a plain `HashSet<Str>` has
    /// no interior mutable state to worry about sharing), so an `intern`
    /// on either engine after the snapshot only grows that engine's copy.
    pub fn snapshot(&self) -> KeywordRegistry {
        KeywordRegistry(Arc::new(RwLock::new(self.0.read().unwrap().clone())))
    }
}

#[derive(Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Str),
    Sym(Symbol),
    /// ":foo" stored WITHOUT the colon.
    ///
    /// W-GEO stage 1: the payload is a [`crate::keyword::Keyword`] --
    /// either an interned `u32` id into the process-wide capped-permanent
    /// table, or (past that table's cap) an `Arc<Str>` holding exactly the
    /// `Str` this variant used to hold directly. Same 16 bytes either way;
    /// see that module's doc and `docs/W-GEO-STAGE1-DESIGN.md`. Text is
    /// reached through `Keyword::text_ref`/`text` (or the `Deref`/`AsRef`
    /// wrappers over them), never by matching the payload's variants.
    Keyword(Keyword),
    Char(char),
    List(PVec),   // call position + '(...) literals
    Vector(PVec), // [...] literals
    Map(PMap),
    /// M5: `champ::PersistentHashSet<Value>` CHAMP set (swapped in
    /// for the previously-vendored `imbl::HashSet`; same deterministic-
    /// hasher equality contract as `PMap::Big` above -- no `Small`/`Big`
    /// split for sets today, see `SPEC-INTEGRATION.md`).
    Set(champ::PersistentHashSet<Value>),
    Fn(Arc<Closure>),
    Native(Arc<NativeFn>),
    Macro(Arc<Closure>),
    /// See [`AtomCell`].
    Atom(Arc<AtomCell>),
    /// M4b: `volatile!` -- a plain mutable single-value cell, deliberately
    /// WITHOUT `Atom`'s `(version, value)` CAS-retry pairing (measured:
    /// `vswap!`'s macroexpansion is `(. v reset (inc (.deref v)))`, a bare
    /// read-then-write with no compare -- real Clojure's `Volatile` is a
    /// plain mutable field, not an atomic reference; a racing writer can
    /// lose an update, same as here). `RwLock` (not `Mutex`) so `deref`/`@`
    /// -- the hot path -- takes a read lock instead of contending with
    /// itself; `vswap!`/`vreset!` take the write lock only for the
    /// assignment itself, never held across the (interpreter-reentrant)
    /// call to `vswap!`'s fn, for the same reentrancy reason `Atom::swap!`
    /// never holds its lock across `f` (see `builtins::atoms`).
    Volatile(Arc<RwLock<Value>>),
    Lazy(Arc<LazySeq>), // lazy-seq cell: thunk OR realized
    /// C3e: **the improper-list marker** -- "the REST of this sequence
    /// continues here", as opposed to [`Value::Lazy`], which is a lazy
    /// seq considered as one ordinary DATUM.
    ///
    /// # Why a distinct variant and not just a `Lazy` in the last slot
    ///
    /// mova encodes a realized head (or a whole realized chunk) in front
    /// of a still-unforced tail as a `Value::List` whose LAST slot is the
    /// continuation -- see [`crate::builtins::lazy_tail_split`], the one
    /// place that rule is defined. Before C3e that last slot held a plain
    /// `Value::Lazy`, which made the encoding AMBIGUOUS with real user
    /// data: a list whose final element genuinely IS a lazy seq
    /// (`(seq [:x (range 2 5)])`, `(list 1 (range 3))`, `` `(a ~(map f
    /// xs)) ``) is byte-for-byte the same shape. The ambiguity was
    /// visible in both directions and could not be papered over at the
    /// consumer end, because THE SAME `Arc<LazySeq>` can legitimately be
    /// either role in two different lists:
    ///
    /// * data read as a continuation -- `(count (seq [:x (range 2 5)]))`
    ///   was `4` (want `2`), `(next (seq [:x (range 2 5)]))` was
    ///   `(2 3 4)` (want `((2 3 4))`);
    /// * a continuation read as data -- `(pr-str (cons 1 (range 3)))` was
    ///   `"(1 (0 1 2))"` (want `"(1 0 1 2)"`), because `realize_deep`
    ///   cannot tell the two apart and deliberately chose "data".
    ///
    /// A `Value::Vector` used to be exempted from the rule by hand (the
    /// C7 "vectors never carry the marker" special case) purely to dodge
    /// half of that ambiguity; the exemption died at the `seq` boundary,
    /// where `Interp::seq_items` turns a vector into a `List`. With the
    /// marker carrying its own discriminant, a plain `List`/`Vector` is
    /// unambiguously proper data everywhere, so the special case, and the
    /// documented "literal 2-element list whose second element is a raw
    /// `Lazy`" deviation it came from, are both gone.
    ///
    /// # Invariants (narrow by construction)
    ///
    /// * Constructed in exactly TWO places -- `collections::cons_builtin`
    ///   (`(cons x <lazy>)`) and `builtins::seq::gen_chunk` (the native
    ///   `range`/`repeat`/`iterate` chunk generators).
    /// * Only ever occupies the LAST slot of a `Value::List` of >= 2
    ///   slots. It is never an element, never nested, never in a
    ///   `Vector`/`Map`/`Set`, and never a variable's value.
    /// * Unwrapped back to a plain `Value::Lazy` the instant it is peeled
    ///   off (`uncons`), so it can never escape into user-visible
    ///   territory -- `(rest (cons 1 (range 3)))` hands back a `Lazy`,
    ///   exactly as before.
    ///
    /// `Arc<LazySeq>` payload (not `Arc<Value>`, not a fresh cell struct):
    /// a continuation is always a lazy cell, and reusing `Lazy`'s own
    /// payload type makes wrap/unwrap a pointer clone with no allocation,
    /// which is what keeps `uncons`'s peel exactly as cheap as it was.
    /// `size_of::<Value>()` is unchanged by this variant (pinned by
    /// `size_of_value_unchanged_by_bignum_variants` -- 32 since W-GEO
    /// stage 4's split-box, 72 before it).
    ///
    /// `=`/`hash` deliberately share [`Value::Lazy`]'s arms verbatim
    /// rather than taking a fresh tag: the wrapper is an internal role
    /// marker on an otherwise identical cell, so swapping one in for a
    /// `Lazy` must not perturb any structural comparison that happens to
    /// walk raw list slots (and a fresh tag would silently change the
    /// hash of every improper list).
    LazyTail(Arc<LazySeq>),
    Future(Arc<FutureCell>),
    Promise(Arc<PromiseCell>),
    Delay(Arc<DelayCell>),
    Channel(Arc<Chan>),
    Flow(Arc<FlowCell>),
    /// R3: `#"pattern"` reader literal / `re-pattern`. Compiled once (at
    /// read time for the literal, at call time for `re-pattern`) and shared
    /// by `Arc` thereafter -- `regex::Regex` compilation is the expensive
    /// part, matching is cheap and `&self`. Equality/hashing are by
    /// `.as_str()` (the source pattern text), matching Clojure's
    /// `java.util.regex.Pattern` identity-free `=`.
    Regex(Arc<LazyRegex>),
    /// R2: `#'x` / `(var x)` -- a first-class reference to a global var's
    /// storage cell (`crate::ns::Interp::resolve_var_cell`), not a
    /// snapshot of its current value. Equality/hashing are by `Arc`
    /// pointer identity, matching every other cell-backed variant
    /// (`Fn`/`Native`/`Atom`/...) below. INVOCABLE: `Interp::apply_value`
    /// derefs a `Value::Var` callee to its current value and recurses --
    /// this is what makes `(#'f 1)`/`(map #'f xs)`/`(flow/process #'f)`
    /// behave like calling `f` directly, just late-bound through the cell.
    Var(Arc<crate::env::VarCell>),
    /// W3 (LATENCY-CAMPAIGN.md): the zero-copy host-embedding boundary --
    /// a host Rust struct wrapped via `crate::embed::wrap_struct`, never
    /// constructible from script (see `crate::host_struct`'s module doc,
    /// "Conformance-by-construction"). `keyword_lookup` dispatches to it
    /// via a shared `Shape` descriptor without materializing a `PMap`;
    /// every map-WIDE op (`=`, hash, `merge`, `assoc`/`dissoc`, `into`,
    /// `reduce-kv`) delegates through `host_struct::as_pmap`'s ONE
    /// materialize choke point instead.
    HostStruct(Arc<crate::host_struct::HostStructInner>),
    /// Read-only lazy top-level EDN map (source text + key index); see `crate::lazy_map`.
    LazyMap(Arc<crate::lazy_map::LazyMapInner>),
    /// SPEC-B-bignum-wiring.md: `7N` -- arbitrary-precision integer that
    /// nonetheless cross-`=`/hashes with `Value::Int` when it fits an
    /// `i64` (measured: `(= 7N 7)` is true, `(hash 7N) == (hash 7)`).
    /// Arc-boxed so this variant doesn't grow `size_of::<Value>()` past
    /// the pointer-sized slot every other boxed variant already costs.
    BigInt(Arc<crate::bignum::BigIntVal>),
    /// S5 (SPEC-numtower): `java.math.BigInteger` -- the SAME arbitrary-
    /// precision integer representation as [`Value::BigInt`] but a
    /// genuinely distinct JVM type, which is observable in two measured
    /// places and therefore cannot be folded into `BigInt`: it prints
    /// WITHOUT the `N` suffix (`(numerator 1/3)` => `1`, not `1N`) and
    /// `(class (biginteger 5))` is `java.math.BigInteger`. It is produced
    /// only by `numerator`/`denominator`/`biginteger`/`.toBigInteger`.
    /// For `=`/`hash`/arithmetic it is a full member of the INTEGER
    /// category alongside `Int` and `BigInt` (measured: `(= (biginteger 5)
    /// 5)` and `(= (biginteger 5) 5N)` are both true, `(hash (biginteger
    /// 5))` == `(hash 5)`, and `(+ (biginteger 5) 1)` is `6N` -- a
    /// `BigInt`, since arithmetic never PRODUCES a `BigInteger`).
    BigInteger(Arc<crate::bignum::BigIntVal>),
    /// SPEC-B-bignum-wiring.md: `1/3` -- always-reduced arbitrary-precision
    /// rational. Cross-`=` ONLY with other `Ratio`s (measured: `(= 1/2
    /// 0.5)` is false) -- never collapses toward `Int`/`Float` the way
    /// `BigInt` does toward `Int`.
    Ratio(Arc<crate::bignum::RatioVal>),
    /// SPEC-B-bignum-wiring.md: `1.5M` -- Java `BigDecimal` semantics,
    /// scale-preserving on print but scale-INSENSITIVE on `=`/hash
    /// (measured: `(= 1.5M 1.50M)` is true even though `(pr-str 1.50M)`
    /// keeps the trailing zero).
    BigDec(Arc<crate::bignum::BigDecVal>),
    /// S3: what `class` returns and `instance?` tests -- a builtin
    /// platform class (with membership predicate) or a `defrecord`/
    /// `deftype` class. Equality by name for builtins, `Arc` identity for
    /// user classes (re-`defrecord` makes a NEW class, measured). See
    /// `crate::types`.
    Class(Arc<crate::types::ClassVal>),
    /// S3: a `defrecord`/`deftype` instance. Records carry their full map
    /// view (basis + ext keys) and integrate with every map-wide op via
    /// `HostStruct`'s exact precedent; deftypes are opaque field holders
    /// with `Arc`-identity equality (measured: `(= (T. 1) (T. 1))` is
    /// false). See `crate::types`.
    Inst(Arc<crate::types::InstVal>),
    /// S4 (everyday3): `(re-matcher re s)` -- a stateful `java.util.regex.
    /// Matcher` stand-in (pattern + input + search cursor + cached last-
    /// match result), mutated in place by `re-find`'s 1-arity form and read
    /// by `re-groups`. `Mutex`-guarded like `Atom`/`Volatile` above (same
    /// reentrancy non-concern: nothing here ever calls back into the
    /// interpreter while the lock is held). Equality/hash are `Arc`
    /// identity, matching every other cell-backed variant -- a matcher has
    /// no meaningful structural equality (Java's `Matcher` doesn't override
    /// `equals` either).
    Matcher(Arc<Mutex<MatcherState>>),
    /// S4/1D: a JVM-style mutable array (`int-array`, `object-array`,
    /// `into-array`, `make-array`, ...). Follows `Atom`'s precedent above
    /// -- `Arc`-shared, lock-guarded mutable storage -- but WITHOUT
    /// `Atom`'s `(version, value)` CAS pairing (arrays have no
    /// compare-and-swap API, only raw `aset`). Equality/hash are `Arc`
    /// pointer identity (measured: `(= (into-array [1]) (into-array
    /// [1]))` is false, `.equals` likewise -- real Java arrays never
    /// override `Object.equals`/`hashCode`). See `crate::value::ArrayVal`.
    Array(Arc<ArrayVal>),
    /// S4: `sorted-map`/`sorted-map-by`. See `SortedMapVal`'s doc.
    SortedMap(Arc<SortedMapVal>),
    /// S4: `sorted-set`/`sorted-set-by`. See `SortedSetVal`'s doc.
    SortedSet(Arc<SortedSetVal>),
    /// C2 (defstruct): `struct`/`struct-map`. See `StructMapVal`'s doc.
    StructMap(Arc<StructMapVal>),
    /// C2 (defstruct): `create-struct`'s return value -- the ordered
    /// basis-key list a `defstruct`/`struct`/`struct-map`/`accessor` call
    /// closes over (real Clojure: `clojure.lang.PersistentStructMap$Def`).
    /// `Arc`-identity only (measured: `(= (create-struct :a :b)
    /// (create-struct :a :b))` is `false` even with byte-identical keys --
    /// no structural equality at all), which is also exactly what
    /// `accessor` keys its "Accessor/struct mismatch" check on -- see
    /// `StructMapVal::basis`'s doc. Printed like `Matcher`/`HostInst`
    /// (`#<struct-basis>`): real Clojure's own `#object[clojure.lang.
    /// PersistentStructMap$Def 0x<hash> ...]` bakes in a nondeterministic
    /// JVM identity hash, so no corpus/golden could pin the exact text
    /// either way -- this is just SOME stable, non-reader-syntax string.
    StructBasis(Arc<Vec<Value>>),
    /// S4: `vector-of`. See `TypedVecVal`'s doc.
    TypedVec(Arc<TypedVecVal>),
    /// C7 (vecveneer): `(.rseq v)`/`(.chunkedNext (seq v))` -- vector-
    /// specific `ISeq` shapes that must report a DIFFERENT `class` than a
    /// plain `List` (measured: `(class (.rseq v))` is
    /// `clojure.lang.APersistentVector$RSeq`, `(class (.chunkedNext (seq
    /// v)))` is `clojure.core.VecSeq`), which `class_of` can only ever
    /// answer per-VARIANT (see `builtins::types::class_of`) -- a plain
    /// `List` has no way to remember "I came from `.rseq`". One variant,
    /// `VecSeqKind`-discriminated, covers both shapes (same "one cell, an
    /// internal kind tag" budget `Value::HostInst` above set for four
    /// unrelated host classes). See `VecSeqVal`'s doc for the exact
    /// per-kind semantics.
    VecSeq(Arc<VecSeqVal>),
    /// S5 (host-class shims): one cell covering FOUR unrelated host
    /// classes -- `java.util.Random` (bit-exact JVM LCG), `java.util.Date`
    /// (epoch millis), `Thread` (the `Thread/currentThread` stand-in), and
    /// a narrow `proxy [ThreadLocal] [] (initialValue [] ...)` instance --
    /// rather than four new `Value` variants, per this task's own budget
    /// (one new variant max). They share nothing behaviorally; `HostKind`
    /// on `HostInstVal` says which one a given cell is, and
    /// `crate::hostclass` is the ONE place that dispatches on it (this
    /// module never does). `Arc`-shared, `Mutex`-guarded mutable state --
    /// same cell shape as `Matcher`/`Array` above, and for the same
    /// reason: `.nextInt`/`.set`/etc mutate in place. Equality/hash are
    /// `Arc` identity, matching every other cell-backed variant (none of
    /// these four JVM classes override `equals`/`hashCode` either).
    HostInst(Arc<crate::hostclass::HostInstVal>),
    /// S5 / M3: `IObj` metadata -- a *wrapper*, not a slot. See
    /// [`MetaObj`]'s doc for why this shape (and not a `meta` field on
    /// every collection payload) is the one that keeps
    /// `size_of::<Value>()` at its pinned width (72 when this was written,
    /// 32 since W-GEO stage 4's split-box) and costs a non-meta value
    /// exactly nothing.
    Meta(Arc<MetaObj>),
    /// S6: `java.util.UUID` -- `#uuid "..."` / `(java.util.UUID/randomUUID)`
    /// / `(java.util.UUID/fromString s)`. The 128-bit value a real `UUID`
    /// carries (two `long`s, packed big-endian into one `u128`), `Arc`-
    /// boxed for the SAME reason `BigInt`/`Ratio`/`BigDec` are (see those
    /// variants' own doc, and the `size_of_value_unchanged_by_bignum_
    /// variants` test this is pinned against): a bare `u128` is 16 bytes,
    /// twice the pointer-sized slot every other variant costs, and would
    /// have grown `size_of::<Value>()` from 72 to 80 -- measured by that
    /// same test going red the first time this was tried inline. `Eq`/
    /// `Hash` are `Arc<u128>`'s derived ones, which (like every other
    /// `Arc<T: Eq + Hash>`) compare/hash the POINTED-TO VALUE, not the
    /// pointer -- exactly value equality, matching real `UUID.equals`/
    /// `.hashCode` (measured: two `UUID/fromString` calls on the same
    /// text are `=`; a real Java `hashCode` bit-for-bit match is
    /// deliberately NOT attempted, only Clojure-visible `=`/`hash`-
    /// agrees-with-`=` behavior is in scope).
    Uuid(Arc<u128>),
    /// S6: `java.net.URI` -- ONLY constructed by `(java.net.URI. s)`
    /// (single-string-arg form; no parsing into scheme/host/etc, no other
    /// constructor arity). Kept as the normalized string the constructor
    /// was given, reusing [`Str`] rather than `Arc<str>` -- `Arc<str>` is
    /// a FAT pointer (data ptr + length, 16 bytes) since `str` is
    /// unsized, which hit the exact same `size_of::<Value>()` budget
    /// problem [`Value::Uuid`]'s doc describes; `Str` is already one of
    /// `Value`'s existing 8-byte-payload variants (`Value::Str`), so
    /// reusing it costs nothing new. Real `URI` does parse/normalize its
    /// argument, but nothing in this task's scope (predicates.clj's
    /// `uri?` truth table) observes that, only identity-free `=`/`str`/
    /// the `uri?` predicate. See `builtins::hostclass`'s `construct` arm
    /// for the one call site.
    Uri(Str),
    /// S7 (wave-C item 8): `clojure.lang.MapEntry` -- what map ITERATION
    /// yields. Invariant: the `PVec` always holds EXACTLY two elements,
    /// `[key, value]`, and is only ever built by [`PVec::pair`].
    ///
    /// # Why this exists at all
    ///
    /// Until S7 a map entry was a plain `Value::Vector`, byte-for-byte
    /// indistinguishable from a hand-typed `[:a 1]` literal, which forced
    /// `map-entry?` and `(instance? clojure.lang.IMapEntry x)` to be
    /// unconditionally `false` -- a documented permanent deviation,
    /// because the honest alternative (`true` for every 2-vector) is
    /// measurably WORSE against the oracle (`(map-entry? [:a 1])` =>
    /// `false`). One distinct discriminant retires both rows.
    ///
    /// # Why the payload is a `PVec` and not an `Arc<(Value, Value)>`
    ///
    /// A map entry IS a two-element vector on the JVM -- measured, not
    /// assumed: `clojure.lang.MapEntry extends AMapEntry extends
    /// APersistentVector`, so `(vector? (first {:a 1}))` is `true`,
    /// `(= (first {:a 1}) [:a 1])` is `true` in both directions,
    /// `(compare (first {:a 1}) [:a 1])` is `0`, `(hash ..)` agrees, and
    /// `nth`/`get`/`count`/`peek`/`pop`/`subvec`/`rseq`/`seq`/invoke/
    /// destructuring/print all behave exactly as the vector's do (see
    /// `compat/mapentry-oracle-transcript.txt`). Sharing `Vector`'s
    /// payload TYPE is what lets every one of those ~40 call sites become
    /// `Value::Vector(items) | Value::MapEntry(items)` -- one extra
    /// pattern, no second code path that could drift -- and it makes the
    /// PROMOTION rule (`conj`/`assoc`/`pop`/`update` return a plain
    /// vector, also measured) fall out for free, since those arms already
    /// rebuild a `Value::Vector` from `items`.
    ///
    /// An `Arc<(Value, Value)>` would have been 8 bytes narrower in the
    /// variant, which buys nothing (`Value` is sized by `PVec` either way
    /// -- `size_of_value_unchanged_by_bignum_variants` still pins 72) and
    /// would have cost a fresh `PVec` materialization at every one of
    /// those sites. `PVec::pair` recovers the allocation win instead: one
    /// `Arc<[Value; 2]>` alloc, vs `pvec![k, v]`'s two.
    ///
    /// # What is NOT shared with `Vector`
    ///
    /// Only where the oracle measurably differs: `class`/`type` say
    /// `clojure.lang.MapEntry`; `map-entry?` and `instance?` of
    /// `IMapEntry`/`java.util.Map$Entry` say `true`; `key`/`val` accept
    /// it and reject a plain vector (`(key [:a 1])` is a
    /// ClassCastException on the JVM); `empty` returns `nil` (not `[]`);
    /// and it is NOT `IObj`, so `with-meta` throws.
    MapEntry(PVec),
    /// C13 (sequences/transducers wave): `clojure.lang.Reduced` -- the
    /// early-termination signal a reducing function returns to tell its
    /// driver "stop, and this is the final value" (real Clojure:
    /// `(reduced x)` boxes `x`; `reduced?`/`deref`/`@` inspect it;
    /// `ensure-reduced`/`unreduced` are the idempotent wrap/unwrap pair).
    /// A genuine variant (not a sentinel map/tag on an existing type)
    /// because every stateful transducer this wave adds (`take`,
    /// `take-nth`, `halt-when`, `partition-by`'s completion arm, ...)
    /// needs to hand a value back through `interp.call`'s ordinary
    /// `Result<Value, _>` return path that its DRIVER (native `reduce`,
    /// mova's `sequence`/`transduce`) can recognize with one `matches!`
    /// and unwrap -- exactly the role `Value::Meta`'s wrapper plays for
    /// metadata, same one-more-discriminant-costs-nothing budget.
    /// `Arc`-boxed like every other boxed variant (`BigInt`, `Uuid`, ...)
    /// so this doesn't grow `size_of::<Value>()` past its pinned width
    /// (72 when this was written, 32 post-split-box). Equality/
    /// hash are `Arc` pointer identity (own tag 35): a `Reduced` has no
    /// meaningful structural equality on the JVM either (`Reduced` never
    /// overrides `equals`/`hashCode`), and every call site in this
    /// campaign compares/derefs an UNWRAPPED value, never a boxed one.
    Reduced(Arc<Value>),
    /// C10: `clojure.lang.PersistentQueue` -- a Rust-native FIFO, front at
    /// index 0 (`peek`/`pop`'s end), back at the last index (`conj`'s
    /// end). Follows `List`/`Vector`/`MapEntry`'s own precedent exactly:
    /// no new payload type, just another `PVec`-holding variant (costs
    /// nothing extra in the 72-byte `Value` budget -- `PVec` is already
    /// the largest field these sibling variants carry). `PVec::push_back`/
    /// `pop_front` give O(1)-amortized `conj`/`pop` without a bespoke
    /// deque type; nothing in the vendored suite exercises a queue large
    /// enough for `PVec`'s `Big`-representation asymptotics to matter.
    ///
    /// Deliberately narrower than real `PersistentQueue` (also
    /// `IPersistentList`/`IPersistentStack`/`Sequential`/`Counted` on the
    /// JVM, per the design directive: "java.* is a compat veneer, never
    /// JVM emulation deeper than the suite demands"): `=`/`hash` join the
    /// SAME sequence-equality class `List`/`Vector`/`MapEntry` already
    /// share (measured: `(= (conj EMPTY 1 2 3) '(1 2 3))` and `(hash (conj
    /// EMPTY 1 2 3))` == `(hash [1 2 3])`, both true on the oracle), `seq`/
    /// `first`/`next`/`rest` collapse to a plain `List` (measured, same as
    /// every other non-`List` seqable already does), `conj` appends to the
    /// BACK (measured: opposite end from `List`'s `push_front`), `pop`
    /// removes the FRONT and -- UNLIKE `List`/`Vector` -- never errors on
    /// an empty receiver (measured: `(pop EMPTY)` succeeds, returning
    /// another empty queue).
    Queue(PVec),
    /// `(timeout-put ms ch val)`'s handle — the claim cell its fire and
    /// its `cancel-timer!` race for (see [`TimerCancel`], and
    /// `docs/TIMER-CANCEL-DESIGN.md`). mova-native surface, same
    /// superset class as `flow/stop-proc`: `timeout` itself stays
    /// fire-and-forget because its chan-close contract is upstream's.
    /// `=`/`hash` are `Arc` identity, matching every other cell-backed
    /// variant (`Matcher`'s exact precedent); prints `#<timer>`;
    /// `class` is `"mova.async.Timer"` — a native name for a native
    /// type, where `Flow`'s upstream-shaped name is for upstream
    /// surface.
    Timer(Arc<TimerCancel>),
    /// SPEC-W6a: one `clojure.test.check.random/JavaUtilSplittableRandom`
    /// -- the immutable `(gamma, state)` pair upstream's `deftype`
    /// carries, and the return value of `make-random`/`split`/`split-n`.
    /// See [`crate::splitrandom`] for the arithmetic and for why the
    /// namespace is a native veneer at all.
    ///
    /// # The one variant in `Value` with an INLINE 16-byte payload
    ///
    /// Every other non-scalar variant here is `Arc`-boxed, and the doc
    /// comments above say why: an inline payload wider than the 8-byte
    /// pointer slot would grow `size_of::<Value>()` past its pinned
    /// width. This one does not, because `PVec`/`PMap` are already 16
    /// bytes each (`size_of_pvec_and_pmap_are_split_boxed`) and
    /// therefore already set that width --
    /// `size_of_value_unchanged_by_bignum_variants` still pins 32, which
    /// is the whole licence for this shape and is the test to watch if
    /// this variant is ever widened.
    ///
    /// What it buys: `split` -- called ONCE PER GENERATED ELEMENT by
    /// `clojure.test.check` -- allocates nothing at all. The two RNGs it
    /// produces are `Copy`ed into the result vector's slots. On a path
    /// whose whole reason for being native is throughput, an `Arc`
    /// allocation plus a refcount decrement per generated element would
    /// have been the only cost left worth measuring.
    ///
    /// # `=` is STRUCTURAL here, and identity on the JVM
    ///
    /// A deliberate, measured deviation. `JavaUtilSplittableRandom` is a
    /// bare `deftype` with no `equals`, so real test.check compares RNGs
    /// by object identity: `(= (make-random 42) (make-random 42))` is
    /// `false` on the JVM and `true` here (and `(= r r)` is `true` in
    /// both). An inline `Copy` value has no identity to compare -- that
    /// is exactly what makes it free -- so the choice was structural
    /// equality or nothing. Nothing in `clojure.test.check`,
    /// `clojure.spec.alpha` or the census suite ever compares two RNGs;
    /// they are threaded, split and consumed, never tested for equality.
    /// Pinned by `tc_random_equality_is_structural_not_identity` below,
    /// so the divergence is asserted rather than assumed.
    TcRandom(crate::splitrandom::SplitRandom),
}

/// [`Value::Timer`]'s cell: ONE one-shot claim flag, and that is the
/// entire state machine — `false` = armed, `true` = settled. The
/// timer's fire and `cancel-timer!` are the two contenders for one
/// `swap(true)`; whoever flips it first wins the entry's single
/// effect (fire's prize is the `chan_try_put`, cancel's prize is that
/// the put never happens). Exactly-once, honest returns, and
/// idempotent cancel all fall out of `swap` having exactly one
/// winner; there is deliberately no second flag to say WHICH side won
/// — `cancel-timer!`'s return value already tells the only caller who
/// can act on it.
///
/// `AcqRel` on the claim: a `cancel-timer!` that returns `true`
/// happens-before-anchors "the value will never be delivered"; the
/// delivery itself is ordered by the chan's own lock as usual.
pub struct TimerCancel {
    claimed: std::sync::atomic::AtomicBool,
}

impl TimerCancel {
    pub(crate) fn new() -> Self {
        TimerCancel { claimed: std::sync::atomic::AtomicBool::new(false) }
    }

    /// Contend for the cell's one effect. `true` iff THIS call won —
    /// at most one claim ever returns `true`.
    pub(crate) fn claim(&self) -> bool {
        !self.claimed.swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Still armed — neither fire nor cancel has claimed it yet. A
    /// snapshot, racy by nature (the fire can win a nanosecond later);
    /// under sim it is exact, because settling happens on the sim
    /// thread itself.
    pub(crate) fn armed(&self) -> bool {
        !self.claimed.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// [`Value::Atom`]'s cell.
///
/// `state` is `(version, value)`: swap!'s lock-free-*style* CAS-retry
/// loop (`builtins::atoms`) reads `version` outside the lock to detect
/// whether another thread mutated the atom while a (potentially
/// interpreter-reentrant) compute fn was running, without needing deep
/// equality on `Value`. See that module for the full algorithm.
///
/// S5/M3 turned this from a bare `Mutex<(u64, Value)>` into a struct to
/// make room for `meta`. An atom is `IReference` but NOT `IObj` -- the
/// SAME split vars have (see `crate::env::VarCell::meta`), and the one
/// the `metadata.corpus` calls "the atom trap": `alter-meta!`/
/// `reset-meta!` on an atom work, while `(with-meta (atom 1) {:a 1})`
/// throws (measured: ClassCastException, "class clojure.lang.Atom cannot
/// be cast to class clojure.lang.IObj"). Hence a slot HERE rather than a
/// `Value::Meta` wrapper around the atom: wrapping would have made
/// `with-meta` quietly succeed.
///
/// A SEPARATE lock from `state`, deliberately: metadata and value are
/// independent (`alter-meta!` must not contend with, or be serialized
/// behind, a long-running `swap!` compute fn, and vice versa), and
/// nothing ever needs to read both atomically.
pub struct AtomCell {
    pub state: Mutex<(u64, Value)>,
    /// `Value::Nil` when the atom has no metadata, else a map. Same
    /// representation choice as `VarCell::meta` -- see its doc.
    pub meta: RwLock<Value>,
    /// kondo-wave: real `add-watch`/`remove-watch` registrations --
    /// `(key, fn)` pairs, `IRef.addWatch`'s own shape. A SEPARATE lock
    /// from `state`/`meta`, same rationale: `builtins::atoms::notify_
    /// watches` clones this list and calls each `fn` with NO lock held
    /// at all (not even this one) -- a watch fn that re-enters (derefs
    /// or swaps! the same atom, or adds/removes a watch) must not
    /// deadlock. `add-watch` with an already-present `key` REPLACES
    /// that entry (measured real behavior), so lookups are by key
    /// equality (`Value`'s own `PartialEq`), not append-only.
    pub watches: Mutex<Vec<(Value, Value)>>,
    /// e2: true for a `(java.io.StringWriter.)` -- `str` gives its text, like the JVM's `toString`.
    pub string_writer: bool,
    /// True for an `(agent state)`: an atom-shaped cell that prints as `clojure.lang.Agent`.
    /// The actions, queue and error state live in Mova code (`core.mova`).
    pub agent: bool,
}

impl AtomCell {
    /// A fresh atom holding `v`, version 0, no metadata, no watches.
    pub fn new(v: Value) -> AtomCell {
        AtomCell {
            state: Mutex::new((0, v)),
            meta: RwLock::new(Value::Nil),
            watches: Mutex::new(Vec::new()),
            string_writer: false,
            agent: false,
        }
    }
}

/// S5 / M3: the `IObj` metadata wrapper's payload.
///
/// # Why a wrapper variant instead of a `meta` field
///
/// `Value` is cloned on nearly every eval step, so its size is a hot
/// budget (`size_of_value_unchanged_by_bignum_variants` pins it at 72
/// bytes). Putting an `Option<PMap>` beside every collection payload
/// would either grow `Value` or force a second `Arc` hop on the *fast*
/// path -- for a feature ~0% of runtime values use. A separate
/// `Arc<MetaObj>` variant costs a value with no metadata literally
/// nothing (one more discriminant in an enum that already has 30+), and
/// makes every dispatch site the compiler's problem rather than a
/// human's.
///
/// # Invariants
///
/// - `inner` is NEVER itself a `Value::Meta`. `Value::attach_meta` is the
///   only constructor and enforces this by unwrapping first.
/// - `meta` is a map-ish value (`Map`/`SortedMap`/`HostStruct`), never
///   `Nil`: `(with-meta x nil)` STRIPS the wrapper entirely rather than
///   storing `Nil` (measured: `(meta (with-meta (with-meta [1] {:a 1})
///   nil))` is `nil`). An EMPTY map is kept, though -- measured:
///   `(meta (with-meta [] {}))` is `{}`, not `nil`, so "empty meta" and
///   "no meta" are genuinely different states.
/// - `=`/`hash`/`compare` never observe `meta`: they unwrap first (see
///   `PartialEq`/`Hash` below). There is deliberately NO new hash tag for
///   `Meta` -- it hashes exactly as its inner value, which is what makes
///   `(= (hash (with-meta [1] {:a 1})) (hash [1]))` true, measured.
pub struct MetaObj {
    /// The metadata map. See the invariants above: never `Nil`.
    pub meta: Value,
    /// The value the metadata is attached to. Never a `Value::Meta`.
    pub inner: Value,
}

/// `Value::Matcher`'s cell. `pos` is the next byte offset `re-find` resumes
/// searching from (`regex::Regex::captures_at`'s `start` argument);
/// `last_match` mirrors Java's `Matcher` group-state contract: `None` until
/// a `re-find` call succeeds, and reset back to `None` by a call that finds
/// nothing -- `re-groups` reads it and throws (matching `IllegalStateException:
/// No match found`) exactly when it's `None`. `last_match` caches the
/// already-computed `captures_to_value` shape (bare string with no groups,
/// `[full g1 g2 ...]` with groups) rather than the borrowed `regex::Captures`
/// itself, which can't outlive the `find` call that produced it.
/// S4: a regex compiled on first use (image restore keeps only the source:
/// a compiled `fancy_regex` is ~25 KB).
pub struct LazyRegex {
    src: Box<str>,
    re: std::sync::OnceLock<fancy_regex::Regex>,
}

impl LazyRegex {
    /// Source not yet compiled; must be a pattern that compiled before.
    pub fn lazy(src: &str) -> Self {
        LazyRegex { src: src.into(), re: std::sync::OnceLock::new() }
    }
    /// The pattern text, without compiling.
    pub fn as_str(&self) -> &str {
        &self.src
    }
}

impl From<fancy_regex::Regex> for LazyRegex {
    fn from(r: fancy_regex::Regex) -> Self {
        LazyRegex { src: r.as_str().into(), re: std::sync::OnceLock::from(r) }
    }
}

impl std::ops::Deref for LazyRegex {
    type Target = fancy_regex::Regex;
    fn deref(&self) -> &fancy_regex::Regex {
        self.re.get_or_init(|| fancy_regex::Regex::new(&self.src).expect("lazy regex"))
    }
}

pub struct MatcherState {
    pub re: Arc<LazyRegex>,
    pub input: String,
    pub pos: usize,
    pub last_match: Option<Value>,
    /// D5: the last successful find's `[start, end)` as CHAR offsets --
    /// `java.util.regex.Matcher#start()`/`#end()`, which the vendored
    /// `clojure.pprint`'s format-directive parser reads on every
    /// parameter it extracts (`(subs s (.end m))`). CHAR offsets, not
    /// byte offsets like `pos`: `.end`'s only uses are as an index into
    /// `subs`/`nth`, both of which are char-indexed in mova exactly as
    /// they are on the JVM. `None` in exactly the states `last_match` is
    /// `None` (never found, or the most recent find failed), so `.start`/
    /// `.end` throw "No match found" precisely when `re-groups` does.
    pub last_span: Option<(usize, usize)>,
}

/// S4/1D: the component-type tag on a [`Value::Array`] -- drives `class`/
/// `instance?`/printing (`types::array_jvm_name`) and construction/`aset`
/// coercion (`builtins::arrays`). Elements are always stored as ordinary
/// `Value`s (`Int` for every integral primitive kind, `Float` for
/// `float`/`double`, `Bool`, `Char`, or an arbitrary `Value` for an
/// `Object`-kind array) -- this tag says how those `Value`s got there and
/// how they print/coerce, not a different storage representation. Kept
/// deliberately un-derived-`Default`/boring: every arm is a real, measured
/// JVM primitive array shape, or `Object(component)` for a reference-type
/// array (`component` is whatever `types::builtin_class_name` would say
/// for a representative element, e.g. "java.lang.Long", or
/// "java.lang.Object" for an explicit `Object`/empty/all-`nil` array).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayKind {
    Int,
    Long,
    Double,
    Float,
    Boolean,
    Byte,
    Char,
    Short,
    Object(&'static str),
}

/// Storage for a [`Value::Array`]: a fixed-length, mutable, `Arc`-shared
/// cell -- `aset`/`aset-*` mutate `data` in place (shared by every alias of
/// the same `Arc`, exactly the JVM's array reference semantics); `aclone`
/// makes a NEW `ArrayVal` (new `Arc`) with a copied `Vec`, never sharing
/// storage with its source (measured: mutating a clone doesn't touch the
/// original).
#[derive(Debug)]
pub struct ArrayVal {
    pub kind: ArrayKind,
    /// Total array dimensionality, `>= 1` (`2` for a `make-array`/
    /// `to-array-2d`-built "outer" array whose elements are themselves
    /// arrays -- measured: `(class (make-array String 2 3))` is
    /// `java.lang.String/2`, TWO brackets, not one). `kind` always
    /// describes the INNERMOST scalar element type regardless of `dims`
    /// -- see `builtins::arrays`' `mk_array_dims`/`build_make_array` for
    /// the one place this is set to anything but `1`.
    pub dims: u32,
    pub data: Mutex<Vec<Value>>,
}

/// A native (Rust) fast-path expander installed on an otherwise-ordinary
/// `Value::Macro` `Closure` -- see `Closure::native_macro`'s doc and
/// `crate::native_macros`. Plain fn pointer (not a boxed closure): every
/// expander is a top-level `fn`, so this stays `Copy`/`Send`/`Sync` for
/// free and needs no extra `Arc`.
pub type NativeMacroFn = fn(
    &mut crate::eval::Interp,
    &[crate::reader::Form],
    crate::reader::Span,
    &std::sync::Arc<Closure>,
) -> Result<Value, crate::error::RjError>;

pub struct Closure {
    pub name: Option<Str>, // for stack traces
    /// Multi-arity fns. Behind an `Arc` so the compiled tier's
    /// `Ir::MakeClosure` can hand every instance a nested `fn` form produces
    /// the SAME parsed arities (bodies are `Form` trees -- deep-cloning them
    /// per closure creation is exactly the churn cost `MakeClosure` exists
    /// to avoid); the tree-walker shares one `Arc` per `eval_fn_form` too.
    pub arities: Arc<Vec<Arity>>,
    pub env: crate::env::Env, // captured lexical env
    /// The namespace this fn was written in (v0.5 / R1). `apply_closure`
    /// makes it current for the duration of every call, which is what lets
    /// the tree-walker resolve the body's free names at ACCESS time and
    /// still agree with the compiled tier, which resolved them once at
    /// creation time -- in this same namespace. See `crate::ns`.
    pub ns: Str,
    /// v0.3 / S2, lazy tier-up (v0.6): the compiled-fn tier's resolved IR
    /// for this closure, computed AT MOST once, possibly deferred past
    /// creation time -- see [`CompileSlot`]. `arities`/`env` are ALWAYS
    /// populated and authoritative (arity selection and arity-error
    /// messages are still driven from `arities` in both tiers), so
    /// whether/when this settles can never change behavior -- see
    /// `crate::compile`'s module doc.
    pub compiled: CompileSlot,
    /// The `fn`/`defmacro` form's own span -- kept so a compile attempt
    /// deferred past creation time (`CompileSlot::on_call`) can still
    /// attribute a bail to the right place, exactly like the old
    /// always-at-creation-time call did.
    pub def_span: crate::reader::Span,
    /// The source buffer `def_span` is relative to -- `Interp::source_id`
    /// AT CREATION time. A deferred compile attempt runs from whatever
    /// call site triggered it (a later buffer, e.g. `(compile-explain f)`
    /// issued from a REPL line), so callers of `CompileSlot::on_call` swap
    /// `interp.source_id` to THIS before compiling and restore it after --
    /// otherwise a bail/explain report would misattribute the position to
    /// the CALLER's buffer instead of the fn's own.
    pub def_source_id: crate::source_registry::SrcRef,
    /// W4C: whether `*unchecked-math*` was truthy (`true` or
    /// `:warn-on-boxed`) at the moment THIS closure was created --
    /// `eval_fn_form`/`eval_defmacro`/`compile::exec::make_closure`'s one
    /// dynamic-var read, taken ONCE, at closure-creation time, never again.
    /// Mirrors the real JVM's compile-time `*unchecked-math*`: a `defn`
    /// compiled while the flag was set gets wrapping (rather than
    /// checked-throwing) `+`/`-`/`*`/`inc`/`dec` overflow behavior for its
    /// entire lifetime, independent of the flag's value at any later call
    /// site (`compat/w4-parse-unchecked-math-probe.clj`'s transcript). Read
    /// by `apply_closure`/`apply_closure_buf` (`eval::apply`), which copy it
    /// into `Interp::current_unchecked` for the duration of the call --
    /// consulted ONLY on the overflow path itself
    /// (`builtins::numbers::wrap_or_throw`); the non-overflow fast path is
    /// bit-identical either way.
    pub unchecked_math: bool,
    /// Fast-path native (Rust) macro expander, if one has been installed
    /// on this closure -- see `NativeMacroFn`'s doc. `None` for every
    /// ordinary `fn`/`defmacro` closure; `Some` only for the handful of
    /// hot `core.mova` macros `native_macros::install` swaps in after
    /// `load_core`. Dispatch (`eval::mod`'s `eval_list`,
    /// `eval::special_forms::macroexpand_1_value`) tries this first and
    /// falls back to the ordinary interpreted body otherwise.
    pub native_macro: Option<crate::value::NativeMacroFn>,
    // field4/W-LENS-1's `lens_site` (this fn's regret-ledger site id) is
    // folded into `CompileSlot` as of lazy tier-up: a lazily-compiled fn
    // has no site until its first compile ATTEMPT resolves one, so the
    // site and the compiled body settle together, atomically, in the same
    // `OnceLock`. Read via `Closure::lens_site()`.
}

impl Closure {
    /// See `CompileSlot::lens_site`.
    pub fn lens_site(&self) -> u32 {
        self.compiled.lens_site()
    }
}

/// Lazy tier-up (v0.6): a fn/macro's compiled-tier outcome, computed AT
/// MOST once and cached -- `None` (bailed, still just as `None` meant
/// before) equally as durably as `Some` (compiled). Two threads racing
/// `on_call` on the SAME closure resolve through `OnceLock::get_or_init`:
/// exactly one of them runs `compile`, the other blocks on the same call
/// and gets its result -- never two independent compiles, never a stale
/// half-written result.
///
/// Bundles the resolved regret-ledger site (`crate::lens`) alongside the
/// compiled body in the SAME cell: a lazily-compiled fn has no site to
/// report until the attempt itself resolves one (see
/// `Closure::lens_site`'s old doc, now here), so the two must settle
/// together or a reader could see a compiled body with the fn's old
/// (always-`NO_SITE`) default site, or vice versa.
pub struct CompileSlot {
    cell: std::sync::OnceLock<(Option<crate::compile::CompiledClosure>, u32)>,
    /// Calls seen BEFORE the slot settled, for the `N`-calls tier-up
    /// threshold (`MOVA_LAZY_TIER_N`, default 1). Stops mattering, and stops
    /// being touched, the instant `cell` settles -- `on_call`'s fast path
    /// checks `cell` first and never reaches this counter again.
    calls: std::sync::atomic::AtomicU32,
    /// S4: image-restored IR not yet decoded; settles `cell` on first use.
    lazy: std::sync::OnceLock<Box<crate::image::LazyIr>>,
}

impl CompileSlot {
    /// Not yet compiled, and not going to be until `on_call` decides it is
    /// time -- the DEFAULT for every `fn` created under lazy tier-up.
    pub fn pending() -> Self {
        Self {
            cell: std::sync::OnceLock::new(),
            calls: std::sync::atomic::AtomicU32::new(0),
            lazy: std::sync::OnceLock::new(),
        }
    }

    /// S4: attach lazily decoded image IR to a still-pending slot.
    pub fn img_lazy(&self, l: Box<crate::image::LazyIr>) {
        if self.cell.get().is_none() {
            let _ = self.lazy.set(l);
        }
    }

    /// S4: decode attached image IR into `cell` (once); None if none attached.
    #[cold]
    fn img_force(&self) -> Option<&(Option<crate::compile::CompiledClosure>, u32)> {
        let l = self.lazy.get()?;
        Some(self.cell.get_or_init(|| (Some(l.decode()), crate::lens::NO_SITE)))
    }

    /// Already resolved, as of closure-CREATION time -- eager mode
    /// (`MOVA_EAGER_COMPILE=1`), macros (always `(None, real_site)`), and
    /// every closure the compiled tier itself builds (`Ir::MakeClosure`,
    /// `RecGroup::materialize` -- compiled BY CONSTRUCTION, always
    /// `(Some(cc), NO_SITE)`).
    pub fn settled(compiled: Option<crate::compile::CompiledClosure>, site: u32) -> Self {
        // K7b: born settled (no `Once` state machine per fn literal).
        Self {
            cell: std::sync::OnceLock::from((compiled, site)),
            calls: std::sync::atomic::AtomicU32::new(0),
            lazy: std::sync::OnceLock::new(),
        }
    }

    /// S3: settle a still-pending slot on image-restored IR; false if already settled.
    pub fn img_settle(&self, compiled: crate::compile::CompiledClosure) -> bool {
        self.cell.set((Some(compiled), crate::lens::NO_SITE)).is_ok()
    }

    /// The compiled body, if this slot has settled on one -- `None` both
    /// for "not attempted yet" and for "attempted and bailed": callers that
    /// need to trigger the attempt use `on_call` instead.
    pub fn compiled(&self) -> Option<&crate::compile::CompiledClosure> {
        self.cell.get().or_else(|| self.img_force()).and_then(|(c, _)| c.as_ref())
    }

    /// This fn's regret-ledger site, or [`crate::lens::NO_SITE`] before the
    /// slot has settled (nothing to regret YET -- once it settles, on a
    /// bail or an escape, this starts reporting the real one).
    pub fn lens_site(&self) -> u32 {
        self.cell.get().map(|(_, s)| *s).unwrap_or(crate::lens::NO_SITE)
    }

    /// The one call-dispatch hook: bumps the pre-settle call counter and,
    /// once it reaches `threshold`, runs `compile` exactly once (via
    /// `OnceLock::get_or_init`, so a concurrent racer blocks and shares the
    /// result instead of compiling a second time) and caches whatever it
    /// returns -- `Bail` (`None`) included, so a fn that bails is never
    /// retried on a later call. Returns the compiled body if the slot is
    /// now settled on one.
    pub fn on_call<F>(&self, threshold: u32, compile: F) -> Option<&crate::compile::CompiledClosure>
    where
        F: FnOnce() -> (Option<crate::compile::CompiledClosure>, u32),
    {
        if let Some((c, _)) = self.cell.get().or_else(|| self.img_force()) {
            return c.as_ref();
        }
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n < threshold {
            return None;
        }
        let (c, _) = self.cell.get_or_init(compile);
        c.as_ref()
    }
}

/// fix/closure-env-cycles Step 1 (`tests/leak_cycle_probe.rs`): raw
/// create/drop counters, gated behind `leak-probe` so they never ship.
#[cfg(feature = "leak-probe")]
pub static CLOSURE_CREATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "leak-probe")]
pub static CLOSURE_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "leak-probe")]
impl Drop for Closure {
    fn drop(&mut self) {
        CLOSURE_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// D9: a `^long`/`^double` PARAMETER hint, as the call-boundary coercion it
/// actually is on the JVM.
///
/// Real Clojure compiles `(fn [^long x] ..)` to a class with a primitive
/// `invokePrim(long)` plus an `invoke(Object)` bridge, and the BRIDGE is
/// where the semantics live: `Compiler$HostExpr.emitUnboxArg` emits a
/// `checkcast java/lang/Number` followed by `RT.longCast(Object)` (or
/// `RT.doubleCast(Object)`). So the body of such a fn can never observe
/// anything but a real `long`/`double`, no matter what the caller passed --
/// which is exactly what `clojure.test.check.generators/shrink-long`'s
/// `(defn- shrink-long [^long x] ..)` relies on when `size-bounded-bignat`
/// hands it a `BigInt` that fits a long (see `docs/SPEC-PORT-PATCHES.md`
/// item 9).
///
/// Only `long` and `double` exist here because they are the only primitive
/// parameter hints the JVM compiler accepts at all (measured: `^int`,
/// `^float`, `^boolean` all fail with `IllegalArgumentException: Only long
/// and double primitives are supported`). Every OTHER tag -- `^String`,
/// `^Object`, `^longs`, `^Long` -- stays an inert reflection hint, measured
/// the same way (`((fn [^Long x] x) (bigint 5))` => `5N`, un-narrowed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrimCast {
    Long,
    Double,
}

pub struct Arity {
    pub params: Vec<Symbol>,
    pub rest: Option<Symbol>, // [a b & more]
    pub body: Body,
    /// D9: `Some(casts)` iff at least ONE of `params` carried a `^long`/
    /// `^double` hint; `casts` is then parallel to `params` (same length,
    /// `None` in every unhinted slot). `None` -- the overwhelming majority
    /// of arities in every program, `core.mova` included -- means "this
    /// call boundary has nothing to do", which is the whole point of the
    /// shape: `apply_closure`/`apply_closure_buf`/`apply_closure_lazy_rest`
    /// test exactly one `Option` discriminant per call and fall straight
    /// through. The hints are decoded ONCE here at parse time, never at
    /// call time.
    ///
    /// The rest (`& more`) parameter is deliberately NOT covered: real
    /// Clojure rejects a hint there outright (`RuntimeException: & arg
    /// cannot have type hint`), and a rest parameter is a seq, not a
    /// number.
    ///
    /// Costs 16 bytes on the `Arity` (a `Box<[T]>` fat pointer; the `Option`
    /// rides the pointer's null niche, so `Some`/`None` is free).
    pub coerce: Option<Box<[Option<PrimCast>]>>,
}

/// A fn arity's body forms. Normally built from a `Vec<Form>` and always
/// present. After a heap-image restore (`crate::image`) the body stays
/// ENCODED (a slice of the mmapped image) and is decoded on first use:
/// `Deref` is the only way to see the forms, so every inspector (tree-walk
/// apply, compile tier-up, explain) forces the decode. `OnceLock` makes a
/// racing first use decode once; the others wait and share the result.
pub struct Body {
    forms: OnceLock<Vec<crate::reader::Form>>,
    lazy: Option<Box<crate::image::LazyBody>>,
}

impl Body {
    pub fn lazy(l: crate::image::LazyBody) -> Body {
        Body { forms: OnceLock::new(), lazy: Some(Box::new(l)) }
    }
}

impl From<Vec<crate::reader::Form>> for Body {
    fn from(v: Vec<crate::reader::Form>) -> Body {
        Body { forms: OnceLock::from(v), lazy: None }
    }
}

impl std::ops::Deref for Body {
    type Target = [crate::reader::Form];
    fn deref(&self) -> &[crate::reader::Form] {
        self.forms.get_or_init(|| self.lazy.as_ref().expect("lazy body without source").decode())
    }
}

pub struct NativeFn {
    /// Owned (not `&'static str`) so `crate::embed::Engine::register_fn` can
    /// register a host-supplied name computed at runtime (including
    /// namespaced names like `"db/lookup"`) without leaking memory -- see
    /// that module's doc for the ripple this was measured against before
    /// picking owned-`Box<str>` over a leaked `&'static str`. Every
    /// existing call site passes a `&'static str` literal, which converts
    /// via the same blanket `From<&str> for Box<str>` at zero behavioral
    /// cost (registration-time only, never on a hot per-call path).
    pub name: Box<str>,
    #[allow(clippy::type_complexity)] // matches the exact signature mandated by ARCHITECTURE.md
    pub f: Box<
        dyn Fn(&mut crate::eval::Interp, &[Value]) -> Result<Value, crate::error::RjError> + Send + Sync,
    >,
    /// v0.3 / N1 (see NATIVE-STEP-DESIGN.md): `Some(_)` iff this native IS
    /// a flow step whose per-message transform can run with no interpreter
    /// entry at all. The engine (`builtins::flow`'s `run_proc`) checks this
    /// ONCE per proc at spawn -- `None` (the overwhelming majority of
    /// natives, including every N1 `map->step*`-built step-fn) always means
    /// "stay on the generic interpreted path", never an error. See
    /// `StepFactory`/`FastStep` below.
    pub step: Option<Arc<dyn StepFactory>>,
    /// v0.5 / Perceus-lite phase 1 (see `builtins::reuse`): `Some(_)` iff
    /// this native has a *consuming* entry point, which may MOVE its
    /// receiver out of the args buffer instead of cloning it out of a
    /// borrowed slice. `Interp::apply_value_owned`/`apply_value_slice` prefer
    /// it when present; `None` (every native but the small whitelist) always
    /// means "use `f`", never an error. The two entry points MUST agree
    /// observably -- `consuming` exists to change allocation traffic, not
    /// results.
    pub consuming: Option<ConsumingFn>,
    /// Heap-image gate-1: how to re-create this load-time native (None for
    /// startup builtins, which the image references by boot index).
    pub image_recipe: Option<Box<crate::image::Recipe>>,
}

/// The consuming counterpart of `NativeFn::f`. See `NativeFn::consuming`.
///
/// `&mut [Value]`, not `Vec<Value>` (phase 3 changed this): the callee's
/// power is "you may take my elements", which a mutable slice expresses
/// exactly, and taking the `Vec` instead forced every caller that did not
/// already have one to build one. That cost a malloc/free pair per flow
/// message, which measured as ~6% on `bench/flow-gen-sink.mova` -- a bench
/// that makes no consuming call at all and can only lose here. The elements
/// a callee takes must be left as `Value::Nil` (`take_arg`), never read back.
pub type ConsumingFn =
    Box<dyn Fn(&mut crate::eval::Interp, &mut [Value]) -> Result<Value, crate::error::RjError> + Send + Sync>;

impl NativeFn {
    /// Convenience constructor for the common case (`step: None`) so most
    /// call sites don't need to spell out the struct literal (and don't
    /// need to change at all when new `NativeFn` fields land in later
    /// native-step stages).
    pub fn new(
        name: impl Into<Box<str>>,
        f: impl Fn(&mut crate::eval::Interp, &[Value]) -> Result<Value, crate::error::RjError> + Send + Sync + 'static,
    ) -> Self {
        NativeFn {
            name: name.into(),
            f: Box::new(f),
            step: None,
            consuming: None,
            image_recipe: None,
        }
    }
}

/// A control-priority transition a running proc's lifecycle can apply to a
/// promoted `FastStep` instance -- the closed-world counterpart of the
/// generic shell calling `(step-fn state ::flow/resume|pause|stop)`
/// (`builtins::flow::call_transition`). See `FastStep::transition`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepTransition {
    Resume,
    Pause,
    Stop,
}

/// A `FastStep::transform`'s per-message output, replacing the generic
/// shell's `{out-id [msgs...]}` `Value::Map` parsing on the hot path (see
/// NATIVE-STEP-DESIGN.md's "Shape" section) -- a promoted step has AT MOST
/// ONE out port (that's one of the promotion preconditions `run_proc`
/// checks), so there's no out-id to carry here at all.
pub enum FastOut {
    None,
    One(Value),
    Many(Vec<Value>),
}

/// The hot-path capability a promoted native step runs through instead of
/// `Interp::call`ing the generic 4-arity step-fn shell per message. See
/// NATIVE-STEP-DESIGN.md's "Types" section for the exact contract; `Send`
/// (not `Sync`) because exactly one proc thread ever owns an instance.
pub trait FastStep: Send {
    /// HOT PATH. CONTRACT: no internal-state mutation until after the
    /// last fallible operation succeeds (keeps
    /// keep-previous-state-on-error structural).
    fn transform(&mut self, interp: &mut crate::eval::Interp, msg: &Value) -> Result<FastOut, crate::error::RjError>;
    /// RARE (ping/error/:state): the exact Value the generic shell
    /// would hold as state right now.
    fn snapshot(&self) -> Value;
    fn transition(&mut self, _t: StepTransition) {} // default no-op
}

/// Carried by a promoted `NativeFn`'s `step` field: config lives in the
/// factory (built once, at native-registration time), while any
/// per-message counters/state rehydrate from `init_state` (the value the
/// generic shell's own `init` arity would have produced) each time a proc
/// spawns. Returning `None` silently keeps the engine on the generic path
/// -- never an error -- e.g. when `init_state` doesn't have the shape this
/// factory knows how to fast-path.
pub trait StepFactory: Send + Sync {
    fn instantiate(&self, init_state: &Value) -> Option<Box<dyn FastStep>>;
}

pub struct LazySeq {
    pub thunk: Mutex<Option<Value>>, // a Fn/Native of 0 args, taken on force
    pub realized: Mutex<Option<Value>>, // forced result, must be seqable/nil
}

/// A `future*`-spawned computation's result cell: the spawned thread stores
/// exactly one of `Done`/`Failed` into `state` and notifies `cv`; every
/// `deref`/`@` (blocking, or with a timeout) waits on the same condvar. See
/// `builtins::conc`.
#[derive(Debug)]
pub enum FutureState {
    Pending,
    Done(Value),
    Failed(crate::error::RjError),
}

/// C3c: `Debug` (hand-written since L3/W2b, see below) so
/// `hostclass::HostState::RealThread` (a `(Thread. f)` construct's
/// `.start`/`.join` cell, which reuses this exact type rather than inventing
/// a parallel one) can sit inside `HostState`'s own derived `Debug` impl.
///
/// **L3/W2b: `task_wakers`.** A `deref` from a TASK cannot park on `cv` --
/// that burns the whole shard until the cell resolves (the L1-class gap
/// `builtins::flow`'s `native_inject` reported). So the cell carries a
/// second, task-side wait list beside the condvar, exactly like [`Doorbell`]
/// carries `task_wakers` beside its own `cv`, and `builtins::conc`'s
/// `future_deref`/`resolve_future` are the only things that touch it -- see
/// `future_deref`'s "Ordering proof" doc for why a waker can never be
/// pushed too late to be woken. Deliberately a SECOND mutex rather than
/// folding `{state, wakers}` into one inner struct: `state` is read as
/// `lock_mutex(&cell.state)` by `predicates.rs` (`realized?`),
/// `printer.rs`, `hostclass.rs` and `builtins::flow`, and the single-mutex
/// shape would have rewritten all of them for no correctness gain -- the
/// state mutex is the serialization point either way (that is what the
/// proof turns on), the waker mutex only ever protects a `Vec`.
pub struct FutureCell {
    pub state: Mutex<FutureState>,
    pub cv: Condvar,
    /// Task waiters, `(token, waker)`; drained by the resolver. The token
    /// is what lets a task that woke for an UNRELATED reason retract its
    /// own entry without disturbing anyone else's ([`Doorbell::
    /// wait_for_change_task`]'s `retain`, same reasoning).
    pub task_wakers: Mutex<Vec<(u64, crate::runtime::TaskWaker)>>,
    /// True for a Clojure `future`: `deref` of a failed one throws
    /// `java.util.concurrent.ExecutionException` with the error as its cause.
    /// Deferreds and flow cells rethrow the exact value.
    pub wrap_failures: bool,
}

impl FutureCell {
    /// A fresh unresolved cell. Every construction site goes through this
    /// (`builtins::conc`'s `future*`, `hostclass`'s `(Thread. f)`,
    /// `builtins::flow`'s `flow/inject`) so that growing the cell -- as
    /// L3/W2b just did -- stays a one-line change here.
    pub fn pending() -> Self {
        FutureCell {
            state: Mutex::new(FutureState::Pending),
            cv: Condvar::new(),
            task_wakers: Mutex::new(Vec::new()),
            wrap_failures: false,
        }
    }
}

/// Hand-written (L3/W2b) because `runtime::TaskWaker` is not `Debug` and has
/// no business becoming so for a printing convenience: the wait list is
/// summarized by its length, which is the only part of it that ever means
/// anything to a human reading a dump.
impl std::fmt::Debug for FutureCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FutureCell")
            .field("state", &self.state)
            .field("task_wakers", &lock_mutex(&self.task_wakers).len())
            .finish_non_exhaustive()
    }
}

/// A `promise`'s delivery cell: `deliver` fills it (at most once -- a second
/// `deliver` is a documented no-op, matching Clojure), `deref`/`@` blocks
/// until delivered.
pub enum PromiseState {
    Pending,
    Delivered(Value),
}

pub struct PromiseCell {
    pub state: Mutex<PromiseState>,
    pub cv: Condvar,
    /// L3/W2b, identical in every respect to [`FutureCell::task_wakers`] --
    /// see that field's doc. `deliver` is the resolver here.
    pub task_wakers: Mutex<Vec<(u64, crate::runtime::TaskWaker)>>,
}

impl PromiseCell {
    /// A fresh undelivered cell -- see [`FutureCell::pending`].
    pub fn pending() -> Self {
        PromiseCell {
            state: Mutex::new(PromiseState::Pending),
            cv: Condvar::new(),
            task_wakers: Mutex::new(Vec::new()),
        }
    }
}

/// A `delay`'s force-once cell. `f` holds the zero-arity thunk until it's
/// consumed by a `force` that reaches a terminal state; `result` is filled
/// exactly once, after which every later `force` takes the fast path and
/// never touches `f` again. Deliberately no `Condvar`: `force` is specified
/// to run "in the calling thread" (unlike `future*`), and
/// `builtins::conc::force_delay` serializes concurrent forcers of the *same*
/// delay by holding `f`'s lock for the whole computation instead (see that
/// fn's doc).
///
/// `result` caches `Err` too (S7 tail wave, measured against the oracle:
/// `(let [d (delay (throw (Exception. "x")))] (identical? (try @d (catch
/// Exception e e)) (try @d (catch Exception e e))))` is `true` on real
/// Clojure -- `clojure.lang.Delay` caches the thrown exception and
/// re-throws the SAME instance on every subsequent `deref`, it does NOT
/// retry the thunk. An earlier version of this cell only cached `Ok`,
/// which silently re-ran `f` on every failing `force` -- observably wrong
/// (a fresh exception instance each time, distinguishable via `identical?`,
/// and a delay with a side-effecting failing thunk re-running it) and is
/// what `delays.clj`'s `saves-exceptions` deftest catches.
pub struct DelayCell {
    pub f: Mutex<Option<Value>>,
    /// `OnceLock`, not `Mutex<Option<..>>` (C3c merge follow-up,
    /// measured): delays.clj's 100-thread x 10k-deref deftests convoyed
    /// on the old fast-path mutex (~5ms/deref under contention, 4900+
    /// CPU-seconds for the file). A forced delay is immutable forever
    /// after, which is exactly `OnceLock`'s contract -- the hot read is
    /// wait-free, and `force_delay`'s `f` mutex still serializes the
    /// one-time computation.
    pub result: std::sync::OnceLock<Result<Value, crate::error::RjError>>,
}

/// Buffer/backpressure policy for a `Channel` (v0.2 / A2, `core.async`
/// surface). `Unbuffered` is Clojure's default `(chan)`: `ChanState::buffer`
/// is (ab)used as a single-slot rendezvous cell rather than a queue for that
/// policy -- see `builtins::async`'s module doc for the exact protocol every
/// put/take against it follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferPolicy {
    Unbuffered,
    Fixed(usize),
    Dropping(usize),
    Sliding(usize),
}

/// Commit state of a parked task putter's cell ([`PutterWaiter::commit`]),
/// the L1/W3 sudog hand-off's whole vocabulary. A task that parked inside
/// `>!` re-reads THIS and nothing else when it resumes -- never the chan's
/// `buffer` -- because a task cannot hold the chan guard across its suspend,
/// so "my value was taken" can no longer be inferred from an empty slot (a
/// second putter refills it in the gap; docs/L1-LANDING-SPEC.md §W3, the
/// FORBIDDEN shape). The value rides in the waiter, so its fate has an
/// identity of its own.
pub const PUT_WAITING: u8 = 0;
/// The value was handed to a taker or promoted into the buffer: `>!` -> true.
pub const PUT_DONE: u8 = 1;
/// The channel closed with the value still un-taken: `>!` -> false, matching
/// Clojure's "puts blocked at close time return false".
pub const PUT_CLOSED: u8 = 2;
/// **The kill tombstone** (L4 W3, docs/L4-SUPERVISION-DESIGN.md §3.5): a
/// supervisor destroying this parked putter CLAIMED its commit cell before
/// force-unwinding it, so the value dies with the task and no deliverer may
/// report it as sent.
///
/// Written by exactly one party -- `runtime::TaskWaker::kill`, with
/// `CAS(PUT_WAITING -> PUT_KILLED)` -- and NAMED by no other: the deliverers
/// (`builtins::async`'s `take_from_task_putter_cold` /
/// `promote_task_putters_cold`) commit with `CAS(PUT_WAITING -> PUT_DONE)`
/// on this same word and treat *any* failure as "this waiter is a corpse:
/// cull it and serve the next one". That is what makes kill and delivery
/// atomic with respect to each other, on a word they both already write
/// (probe P5a-bis: killed-putter posthumous deliveries 52 174/100k -> 0).
pub const PUT_KILLED: u8 = 3;

/// One task parked inside a blocking put on this chan, carrying the value it
/// is trying to send (L1/W3).
///
/// Who writes what: the waiter is pushed onto [`ChanState::task_putters`] by
/// the parking task itself, UNDER the chan lock, before it drops the guard
/// and suspends. Every subsequent write is done by a deliverer (a taker, a
/// buffer promote, or `close!`) that has popped the waiter out of that queue
/// while holding the SAME chan lock: it takes `val`, stores a terminal state
/// into `commit`, drops the guard, and only then calls `waker.wake()` --
/// `builtins::async`'s clone-out-then-ring discipline, applied to the task
/// family. The parked task therefore never races anyone for its own value.
pub struct PutterWaiter {
    /// The value being sent. Moves out of the waiter when a deliverer pops
    /// it, so exactly one party can ever take it.
    pub val: Value,
    /// `PUT_WAITING` -> `PUT_DONE` | `PUT_CLOSED`, written once, under the
    /// chan lock, by whoever took `val`. An `Arc` because the parked task
    /// keeps its own handle on the stack while the waiter itself travels
    /// into the queue.
    pub commit: Arc<AtomicU8>,
    pub waker: crate::runtime::TaskWaker,
}

/// A parked task taker's one-shot delivery cell: the value it was handed, or
/// "the channel closed with nothing left to drain" (L1/W3).
///
/// `Closed` is only ever written by a deliverer that has checked, under the
/// chan lock, that the buffer is empty -- which is what preserves close!'s
/// drain-then-nil semantics for a taker that was already parked when the
/// close landed (`chan_close` hands buffered values to parked takers in FIFO
/// order first, and closed-markers only to whoever is left).
///
/// **`Closed` is ALSO the take-side kill tombstone** (L4 W3, §3.5) --
/// `runtime::TaskWaker::kill` writes it into the dying task's own cell,
/// under the same mutex a deliverer commits through, and the deliverers read
/// "not `Waiting`" as "this waiter is a corpse". Deliberately not a fifth
/// variant: `Closed` already means exactly "this cell will never yield a
/// value", every existing reader does the right thing with it, and no LIVE
/// task can observe the tombstone -- it is only ever written on a path that
/// ends in the shard destroying that task, and a task that is `PARKED` is by
/// construction present in its shard's slab, so that kill always completes.
pub enum TakeSlot {
    Waiting,
    Value(Value),
    Closed,
}

/// One task parked inside a blocking take on this chan (L1/W3). Mirror image
/// of [`PutterWaiter`]: pushed onto [`ChanState::task_takers`] by the parking
/// task under the chan lock, popped and filled in by a putter/closer under
/// that same lock, woken after the guard drops.
///
/// The cell is a `Mutex` rather than an atomic because a `Value` is not a
/// word. Lock ORDER is chan -> cell and never the reverse: a deliverer holds
/// the chan lock while it writes the cell, and the resumed taker locks only
/// the cell. A park boundary, not a hot path.
pub struct TakerWaiter {
    pub cell: Arc<Mutex<TakeSlot>>,
    pub waker: crate::runtime::TaskWaker,
}

/// Mutable state behind a `Channel`. `waiting_takers`/`waiting_putters`
/// count threads *currently parked* in a blocking `<!!`/`>!!` wait, not
/// merely "interested" -- `builtins::async`'s non-blocking `chan_try_put`
/// uses `waiting_takers > 0` to decide whether an `Unbuffered` channel's
/// rendezvous can complete immediately, matching core.async: a value can
/// only be *offered* onto an unbuffered channel when a taker is already
/// parked waiting for it (a blocking `>!!`, by contrast, is allowed to place
/// the value first and then block until a taker eventually arrives).
pub struct ChanState {
    pub buffer: VecDeque<Value>,
    pub policy: BufferPolicy,
    pub closed: bool,
    pub waiting_takers: usize,
    pub waiting_putters: usize,
    /// A `builtins::flow` proc's wake target, registered here while (and
    /// only while) that proc's read-set includes this chan -- `None` for
    /// every chan outside the flow engine (plain `chan`/`>!!`/`<!!`/
    /// `alts!!` usage never touches this field). See [`Doorbell`] and
    /// `builtins::flow`'s module doc ("Control-priority wait design") for
    /// why this exists: every `notify_all()` on `cv` (`builtins::async`'s 5
    /// call sites) also rings this, if set, so a proc parked on its OWN
    /// `Doorbell` instead of any one chan's condvar still wakes the instant
    /// this chan changes.
    pub doorbell: Option<Arc<Doorbell>>,
    /// A SECOND, deliberately separate doorbell slot family, owned by
    /// `builtins::async`'s blocking `alts!!` (never by the flow engine).
    /// `alts!!` can't reuse `doorbell` above: that slot is the flow
    /// engine's per-proc registration and a chan can be BOTH in a proc's
    /// read-set and an op in someone's `alts!!` at the same time, so
    /// sharing one slot would mean one registrant silently clobbering the
    /// other. And it's a `Vec`, not an `Option`, because N threads may
    /// `alts!!` over the same chan concurrently and every one of them has
    /// to be woken -- unlike the flow engine's slot, whose single writer
    /// is the one proc that owns the read-set.
    ///
    /// Empty for every chan nobody is currently `alts!!`-parked on, which
    /// is the overwhelmingly common case: the extra work at the 5
    /// `notify_all()` sites is an `is_empty()`-shaped walk of an empty
    /// `Vec`, and the `clone()` those sites do to ring outside the lock
    /// doesn't allocate when empty. Entries are pushed by `alts!!` before
    /// its first scan and removed by an RAII guard on EVERY exit path from
    /// that native (see `builtins::async`'s `AltsRegistration`) -- a
    /// leaked entry would be rung forever on a hot chan.
    pub alts_doorbells: Vec<Arc<Doorbell>>,
    /// L1/W3, always present: tasks parked in a blocking TAKE on this chan,
    /// oldest first. A `VecDeque` that has never been pushed to owns no
    /// allocation, so a chan no task ever touches pays two pointers and a
    /// pair of counters for the field and nothing else -- not `#[cfg]`-gated
    /// (unlike the now-deleted P2 probe's own waiter field) because `go` IS
    /// the task runtime from L1 on.
    ///
    /// **Invariant (relied on by every site below), restated in terms of
    /// LIVE waiters by L4 W3 (probe finding S1): a `task_takers` holding at
    /// least one LIVE waiter implies `buffer` empty, and `task_takers` and
    /// `task_putters` are never both non-empty IN THEIR LIVE MEMBERS.** A
    /// taker only parks after finding the buffer empty AND no putter
    /// waiting; a putter only parks after finding no taker waiting; and
    /// every value-producing site checks `task_takers` BEFORE it touches
    /// `buffer`, so nothing can be buffered behind a parked taker's back.
    ///
    /// **Corpses are exempt, and they are why the qualifier is not
    /// pedantry.** A killed task's waiter stays QUEUED until some deliverer
    /// walks past it (`TakeSlot::Closed` tombstone on the take side,
    /// `PUT_KILLED` on the put side -- see `runtime::TaskWaker::kill`), so a
    /// chan can genuinely hold 8 queued takers AND 8 buffered values at once
    /// (`tests/l4_kill_probe.rs`'s test D runs exactly that shape). Every
    /// reader is already safe -- they all consult `task_takers` first and
    /// cull what they find dead -- but the unqualified sentence this
    /// replaces was FALSE, and a load-bearing comment left false is how the
    /// next bug gets written. `builtins::async`'s
    /// `debug_assert_live_waiter_invariant` checks the live-waiter form
    /// directly, at the two park-registration sites, in debug builds.
    pub task_takers: VecDeque<TakerWaiter>,
    /// L1/W3, always present: tasks parked in a blocking PUT on this chan,
    /// oldest first, each carrying its own value (see [`PutterWaiter`] for
    /// why the value rides in the waiter and not in `buffer`). Read by every
    /// take-side site -- it is a fourth input to a readiness scan, beside
    /// `buffer`, `closed` and `waiting_takers`.
    pub task_putters: VecDeque<PutterWaiter>,
}

/// A `core.async`-style channel (v0.2 / A2). See `builtins::async`'s module
/// doc for the full put/take/close/alts protocol implemented against
/// `state`+`cv`.
pub struct Chan {
    pub state: Mutex<ChanState>,
    pub cv: Condvar,
}

impl Chan {
    pub fn new(policy: BufferPolicy) -> Self {
        Chan {
            state: Mutex::new(ChanState {
                buffer: VecDeque::new(),
                policy,
                closed: false,
                waiting_takers: 0,
                waiting_putters: 0,
                doorbell: None,
                alts_doorbells: Vec::new(),
                task_takers: VecDeque::new(),
                task_putters: VecDeque::new(),
            }),
            cv: Condvar::new(),
        }
    }
}

/// One per parked waiter that has to watch SEVERAL event sources at once: a
/// single wake target the waiter registers with every source its wait loop
/// currently cares about, so any of them ringing it wakes the waiter
/// immediately instead of the waiter polling on a short timeout. Two
/// independent users, each with its own registration slot on
/// [`ChanState`]:
///
/// - **`builtins::flow`, one per running proc** (`ChanState::doorbell`):
///   registered on its control chan, every chan in its read-set, its
///   inject chan, and -- for a fused run -- every member's control chan.
/// - **`builtins::async`'s blocking `alts!!`, one per parked call**
///   (`ChanState::alts_doorbells`): registered on every chan the call is
///   selecting over, for the duration of that one native call.
///
/// See `builtins::flow`'s module doc for the flow design and
/// `builtins::async`'s for the `alts!!` one;
/// this struct is data-only (mirroring `Chan`'s own `state`/`cv` split --
/// the logic that USES a `Doorbell` lives in `builtins::flow` and
/// `builtins::async`) and lives here rather than there so `ChanState`
/// (this file) can hold its two registration slots without `value.rs`
/// depending on `builtins`.
///
/// Deliberately a plain `Mutex<u64>` (a generation counter) + `Condvar`,
/// NOT the lock-free park/unpark protocol `transport.rs` uses for its 1:1
/// SPSC hot path: a `Doorbell` sits on the park/wake BOUNDARY (rare, by
/// construction, once the fix it enables is in place), never on a
/// per-message hot path, so there is nothing to optimize here and a plain,
/// easily-audited primitive is strictly better than a clever one.
///
/// **Missed-wakeup correctness.** Every caller that scans-then-parks MUST
/// snapshot [`Doorbell::current`] BEFORE its non-blocking scan of whatever
/// it's about to check, and only call [`Doorbell::wait_for_change`] with
/// that PRE-scan snapshot. If a [`Doorbell::ring`] happens between the
/// snapshot and the park call, `wait_for_change` sees the generation has
/// already moved and returns immediately without blocking (no missed
/// wakeup) -- the standard generation-counter pattern, and the reason no
/// CAS/spin-budget machinery is needed here: the generation check on entry
/// to `wait_for_change` IS the re-validation.
///
/// **The task arm (L1/W3).** A `Doorbell` can also be waited on by a TASK
/// (`alts!!` called inside a `go` block). A task is not an OS thread, so
/// neither `cv.notify_all()` nor `owner_thread.unpark()` can reach it: it
/// registers a `TaskWaker` in [`DoorbellState::task_wakers`] under the same
/// mutex that holds the generation, re-checks the generation while still
/// holding it, and suspends only if it has not moved. [`Doorbell::ring`]
/// DRAINS that list under the mutex and wakes what it drained after the
/// guard drops. The missed-wakeup argument is the identical
/// register-then-recheck one -- a ring landing between the drop and the
/// suspend finds the task still `RUNNING` and leaves a `NOTIFIED` note the
/// scheduler consumes at the park boundary (`runtime`'s module doc, step 3).
/// The task arm carries NO safety-net timeout (docs/L1-LANDING-SPEC.md,
/// "Landing stance"): the ring is the mechanism, not a backstop, and a net
/// here would hide exactly the missed ring W5 exists to hunt.
///
/// **The task arm has a SECOND caller as of L3.6/W1** (docs/
/// FLOW-HOP-RECOVERY.md §7). `transport.rs`'s SPSC lane grew a task park,
/// and a lane-parked task must notice control/inject events that never
/// touch its ring -- so it registers HERE too, through
/// [`Doorbell::register_task_waker`] / [`Doorbell::unregister_task_waker`],
/// which are literally the two halves [`Doorbell::wait_for_change_task`]
/// itself is now built from. That makes it a TWO-SOURCE park (this doorbell
/// plus the ring's own waiter slot), with the register-then-recheck dance
/// run independently on each; the interaction argument is in
/// `Ring::park_task_two_source`. Nothing about this type changed to support
/// it, which is the point: the lane borrows the protocol rather than
/// inventing one.
pub struct Doorbell {
    state: Mutex<DoorbellState>,
    cv: Condvar,
    /// The waiting thread's own OS thread, captured ONCE at construction
    /// and never mutated again -- a `Doorbell` is always created ON the
    /// thread that will park on it, before any waiting happens (
    /// `builtins::flow`'s `run_ready`/`run_fused` each call
    /// `Doorbell::new` as the first thing they do on their own proc
    /// thread; `builtins::async`'s `alts!!` calls it on the calling thread
    /// at the top of its parking path -- those are the three call sites),
    /// and that thread identity is fixed for the doorbell's whole life,
    /// unlike chan registration which tracks the read-set / the op set.
    /// [`Doorbell::ring`] unparks this thread IN ADDITION TO
    /// its `Condvar`-based wake, so a proc parked inside
    /// `transport.rs`'s lock-free `Ring` (which has no `Doorbell`
    /// awareness of its own -- see that module's doc) still receives a
    /// real wake for a control/inject event, without a single line of
    /// `Ring`'s own wait-loop logic changing: `thread::park`/
    /// `park_timeout` is a per-OS-thread token, not scoped to whichever
    /// call happened to park it, and the ring's own loop already never
    /// trusts why it woke (that module's invariant 7), so an extra,
    /// uncorrelated `unpark()` is exactly the kind of spurious wake it's
    /// already built to tolerate for free.
    ///
    /// L3.6/W1 footnote: a lane-parked TASK does NOT rely on this unpark at
    /// all (a task has no `park()` for a token to latch against). It is
    /// reached by the `TaskWaker` it registered in `task_wakers` below,
    /// which the loop just above drains and wakes -- see the struct doc's
    /// "task arm". The unpark stays for the THREAD case, which still uses
    /// it exactly as described.
    ///
    /// L1/W3 footnote: a `Doorbell` created inside a TASK captures its SHARD
    /// thread here. That unpark is then merely uncorrelated, never wrong --
    /// the shard loop's `thread::park()` re-checks its inbox and goes back
    /// to sleep, exactly as it does for a spurious token left by work it had
    /// already drained.
    owner_thread: std::thread::Thread,
}

/// [`Doorbell`]'s mutex-protected half: the generation counter, and the
/// L1/W3 task-waker registrations that share its lock (see the struct doc's
/// "task arm"). One mutex, not two, because the whole missed-wakeup argument
/// is "register and re-check the generation atomically".
struct DoorbellState {
    generation: u64,
    /// `(token, waker)` for every task currently parked in
    /// [`Doorbell::wait_for_change`]. The token is a process-unique id whose
    /// only job is letting a resumed task remove ITS OWN entry (a
    /// `TaskWaker` carries no public identity, and this file must not reach
    /// into `runtime`'s internals for one).
    ///
    /// Drained wholesale by [`Doorbell::ring`], and additionally removed by
    /// the waking task itself: a registration that outlived its park would
    /// be a handle on a task that has moved on to park somewhere else
    /// entirely, and the next ring would fire a wake at it there.
    task_wakers: Vec<(u64, crate::runtime::TaskWaker)>,
}

/// Source of [`DoorbellState::task_wakers`] tokens. Relaxed is enough: the
/// only property required is uniqueness, and the mutex orders every use.
static NEXT_DOORBELL_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Process-wide count of genuine safety-net fallbacks: a
/// [`Doorbell::wait_for_change`] call whose `cv_wait_timeout` reports
/// `timed_out` AND whose generation is STILL unchanged after reacquiring
/// the lock (i.e. nothing rang -- see the increment site for why both
/// conditions are checked, not just `timed_out`). This is the direct,
/// deterministic proxy FLOW-IDLE-CPU-BUG.md's acceptance criterion #1
/// ("< ~200 context switches/s and < 1% CPU... sustained") asks for,
/// expressed as something `cargo test` can assert without shelling out to
/// `top`/`sample`: before this fix, a genuinely idle proc hit its
/// equivalent (a `cv_wait_timeout` that always timed out, every
/// `PARK_TIMEOUT`/`MULTI_INPUT_BACKOFF`) thousands of times per second;
/// after it, an idle proc should hit this ~0 times over many seconds,
/// since every real event (data, control, close) rings instead. A process
/// global, not a per-`Doorbell` counter, so black-box tests (which only
/// ever see the `flow/*` API, never a specific proc's `Doorbell`) can
/// observe it -- see `tests/flow_wake_test.rs`'s top-of-file doc for the
/// resulting test-isolation discipline this requires (serialize tests that
/// read it, measure deltas over narrow windows).
///
/// Since `builtins::async`'s `alts!!` parks on a `Doorbell` too, a
/// long-blocked `alts!!` also ticks this once per `ALTS_PARK_TIMEOUT` it
/// waits out -- same meaning ("a doorbell-aware park fell back to its
/// deadline"), and harmless to the flow tests, which run in their own
/// process and drive no `alts!!`.
static SAFETY_NET_HITS: AtomicU64 = AtomicU64::new(0);

impl Doorbell {
    /// MUST be called from the thread that will park on this doorbell (see
    /// `owner_thread`'s doc) -- every current call site satisfies this by
    /// construction (`run_ready`/`run_fused` each call it as the very
    /// first thing they do, on their own thread, before spawning
    /// anything else; `alts!!` calls it on the thread that is about to
    /// park inside the native).
    pub fn new() -> Self {
        Doorbell {
            state: Mutex::new(DoorbellState { generation: 0, task_wakers: Vec::new() }),
            cv: Condvar::new(),
            owner_thread: std::thread::current(),
        }
    }

    /// Current value of [`SAFETY_NET_HITS`]. `pub(crate)` visibility would
    /// hide this from `tests/flow_wake_test.rs` (an external crate as far
    /// as the compiler's concerned); re-exported through
    /// `mova::internal::Doorbell` alongside the type itself, matching
    /// every other white-box test hook in `lib.rs`'s `internal` module.
    pub fn safety_net_hits() -> u64 {
        SAFETY_NET_HITS.load(Ordering::Relaxed)
    }

    /// Folds an external doorbell-aware wait's genuine safety-net fallback
    /// into the SAME counter [`Doorbell::wait_for_change`] increments --
    /// both measure the identical thing: "a doorbell-aware park fell back
    /// to its deadline instead of being woken by a ring". The only current
    /// caller is `transport.rs`'s `Ring::wait_for_data_until_or_doorbell`
    /// (its OWN deadline branch, not its `doorbell.current() != seen`
    /// branch -- that one IS a genuine ring, the opposite of a safety-net
    /// fallback). `pub(crate)`: a test only ever needs to READ this
    /// counter (`Doorbell::safety_net_hits`), never increment it directly.
    pub(crate) fn note_external_safety_net_hit() {
        SAFETY_NET_HITS.fetch_add(1, Ordering::Relaxed);
    }

    /// Called by any event source when something a proc registered on this
    /// doorbell might care about happened. Cheap, and safe to call even if
    /// no proc is currently parked on it (the common case -- the hot
    /// per-message path never parks at all).
    pub fn ring(&self) {
        let mut g = lock_mutex(&self.state);
        g.generation = g.generation.wrapping_add(1);
        // L1/W3: drain the task registrations under the same lock that just
        // moved the generation, and wake them AFTER the guard drops -- the
        // clone-out discipline `builtins::async`'s ring sites use, for the
        // same reason (a woken task's first move may be to lock this very
        // doorbell to unregister). Draining, not merely reading: a task
        // re-registers on its own next trip round its wait loop, and a
        // handle left behind here would chase a task that has already moved
        // on to a different park.
        let tasks = std::mem::take(&mut g.task_wakers);
        drop(g);
        self.cv.notify_all();
        // Additive, not a replacement for the `Condvar` path above: a
        // proc only ever has ONE current wait point per loop iteration
        // (either parked in `wait_for_change` below, via the `Chan`/
        // control/inject path, OR parked inside `transport.rs`'s
        // `thread::park_timeout`, via a transport lane -- never both at
        // once), so exactly one of these two calls finds a real waiter;
        // the other is a harmless no-op (a `notify_all` with nobody
        // waiting, or an `unpark()` that just latches a token for this
        // thread's next `park`/`park_timeout` -- see `owner_thread`'s
        // doc for why the ring's own wait loop already tolerates that).
        self.owner_thread.unpark();
        for (_, w) in &tasks {
            w.wake();
        }
    }

    /// Current generation. Callers snapshot this BEFORE their non-blocking
    /// scan -- see the struct doc's "missed-wakeup correctness" section.
    pub fn current(&self) -> u64 {
        lock_mutex(&self.state).generation
    }

    /// Parks until [`Doorbell::ring`] has fired since `seen`, or
    /// `safety_net` elapses (a defensive backstop only -- never relied on
    /// for correctness, see the struct doc). Returns the generation
    /// observed on return, so a caller looping on this can tell "something
    /// rang" (`returned != seen`) from "nothing rang, either a genuine
    /// safety-net timeout or a spurious OS-level wake" (`returned == seen`)
    /// -- both cases are safe for a caller to treat identically (loop back
    /// and re-check the real condition), which is why this never needs to
    /// distinguish them itself.
    pub fn wait_for_change(&self, seen: u64, safety_net: Duration) -> u64 {
        // L1/W3: a TASK cannot block its shard thread here, and no OS-level
        // wake can reach it anyway. Same protocol, task waker instead of a
        // condvar, and no safety net (landing stance).
        if crate::runtime::in_task() {
            return self.wait_for_change_task(seen);
        }
        let mut g = lock_mutex(&self.state);
        if g.generation == seen {
            let (guard, timed_out) = cv_wait_timeout(&self.cv, g, safety_net);
            g = guard;
            // A GENUINE safety-net fallback is `timed_out` (the OS-level
            // wait exhausted its duration) AND the generation is STILL
            // `seen` (nothing rang while we were parked -- `timed_out` can
            // be `true` even when a ring landed in the last instant before
            // the deadline, since `Condvar::wait_timeout` doesn't retract
            // that report just because a notify also happened to fire; the
            // generation check is what disambiguates "really fell back to
            // the timeout" from "woke for the right reason, coincidentally
            // near the deadline").
            if timed_out && g.generation == seen {
                SAFETY_NET_HITS.fetch_add(1, Ordering::Relaxed);
            }
        }
        g.generation
    }

    /// [`Doorbell::wait_for_change`]'s task arm (L1/W3). Register under the
    /// mutex, re-check the generation while still holding it, drop, suspend.
    ///
    /// Returns after ONE suspend, whatever woke it -- exactly like the OS
    /// arm, whose callers all loop and re-scan the real condition rather
    /// than trusting why they woke. Nothing here increments
    /// [`SAFETY_NET_HITS`]: there is no deadline to fall back to.
    fn wait_for_change_task(&self, seen: u64) -> u64 {
        // The check-register-recheck step, factored out so `transport.rs`'s
        // TWO-SOURCE lane park performs the byte-identical protocol against
        // this same mutex (see [`Doorbell::register_task_waker`]).
        let token = match self.register_task_waker(seen, crate::runtime::current_waker()) {
            Ok(token) => token,
            Err(gen) => return gen,
        };
        // A ring landing HERE has already drained our waker and called
        // `wake()`, which finds the task still `RUNNING` and leaves
        // `NOTIFIED`; the scheduler then re-queues instead of parking. The
        // wake cannot be lost.
        crate::runtime::park_current_yield();
        let mut g = lock_mutex(&self.state);
        // Ours is already gone if a ring woke us; this is for the other
        // case (a wake that reached this task for an unrelated reason --
        // see `runtime`'s `NOTIFIED` note). Leaving it would arm a stale
        // wake at whatever this task parks on next.
        g.task_wakers.retain(|(t, _)| *t != token);
        g.generation
    }

    /// **The check-register-recheck step of the task arm, as ONE atomic
    /// step** -- `Ok(token)` when `waker` is now registered and the
    /// generation was still `seen` at the instant of registration,
    /// `Err(generation)` when a [`Doorbell::ring`] had ALREADY moved it (in
    /// which case nothing was registered and the caller must not suspend).
    ///
    /// The whole missed-wakeup argument is that the comparison and the push
    /// happen under the SAME lock `ring` takes to bump the generation and
    /// drain the list, so the two are totally ordered:
    /// - ring first: we observe the moved generation and return `Err`;
    /// - register first: the ring drains our waker and wakes it.
    ///
    /// Factored out of [`Doorbell::wait_for_change_task`] for
    /// `transport.rs`'s SPSC lane task park, which must register on this
    /// doorbell AND in a `Ring`'s waiter slot before suspending -- a
    /// two-source park (docs/FLOW-HOP-RECOVERY.md §7). Being one shared
    /// function rather than two copies is what makes "the lane park uses the
    /// same protocol the doorbell has always used" a fact rather than a
    /// claim.
    pub(crate) fn register_task_waker(
        &self,
        seen: u64,
        waker: crate::runtime::TaskWaker,
    ) -> Result<u64, u64> {
        let mut g = lock_mutex(&self.state);
        if g.generation != seen {
            return Err(g.generation);
        }
        let token = NEXT_DOORBELL_TOKEN.fetch_add(1, Ordering::Relaxed);
        g.task_wakers.push((token, waker));
        Ok(token)
    }

    /// Retracts a [`Doorbell::register_task_waker`] registration by its
    /// token. Idempotent (a ring that woke us already drained it), and
    /// MANDATORY on every path out of a park: a registration that outlived
    /// its park is a handle on a task that has moved on to park somewhere
    /// else entirely, and the next ring would fire a wake at it there.
    pub(crate) fn unregister_task_waker(&self, token: u64) {
        lock_mutex(&self.state).task_wakers.retain(|(t, _)| *t != token);
    }
}

impl Default for Doorbell {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// `flow` (Phase F1: `core.async.flow` on a native Rust engine, see
// FLOW-DESIGN.md). Same split as `Chan`/`FutureCell` above: this module only
// owns the *data* a flow needs to exist as a `Value` (its immutable
// definition, plus whatever live/running state must be reachable from
// outside the engine -- `flow/inject`, `flow?`, printing); all the actual
// engine logic (validation, wiring, proc threads, mult fan-out, control
// priority) lives in `builtins::flow`, which manipulates these fields
// directly (they're `pub` for exactly that reason, mirroring `Chan`'s
// `state`/`cv` being `pub` for `builtins::async`).
// ---------------------------------------------------------------------------

/// A resolved, restart-eligible `:supervision` config (L4 W1). Only ever
/// constructed for a proc whose EFFECTIVE `:policy` is `:restart` --
/// `:policy :none` (explicit or by absence) is represented by
/// `ProcDef::supervision` being `None` instead, so every field here is
/// meaningful (there is no "policy is none, ignore the rest" state to trip
/// over). See `builtins::flow`'s `parse_supervision_map` for the validation
/// this is built from and docs/L4-LANDING-SPEC.md §W1.4 for the defaults
/// each field falls back to when the cfg map omits it.
#[derive(Clone, Debug, PartialEq)]
pub struct SupervisionCfg {
    /// Max restarts inside one `window_ms` before `on_give_up` fires.
    pub max_restarts: u32,
    pub window_ms: u64,
    pub backoff: BackoffCfg,
    /// Graceful-stop grace window before escalation (W3; plumbed, unused
    /// until then).
    pub grace_ms: u64,
    pub on_give_up: OnGiveUp,
    /// L4 W5 (owner ruling #5, docs/L4-SUPERVISION-DESIGN.md §8): whether a
    /// restarted incarnation should be handed a synthesized `::flow/resume`
    /// the instant it is swapped in, PROVIDED every member of the run last
    /// had `running` recorded as its desired state (`FlowRuntime::desired`).
    /// Default `true` -- "the supervisor mirrors user intent" reads, by
    /// default, as "a proc the user had running comes back running"; a proc
    /// the user had PAUSED stays paused across a crash-restart regardless of
    /// this flag, because there is no "resume" intent to mirror. See
    /// `builtins::flow::Supervisor::act_restart` for where this is consulted.
    pub auto_resume: bool,
}

/// `:backoff {:initial-ms :factor :max-ms}` -- exponential with a cap,
/// `initial_ms * factor^k` clamped to `max_ms`, `k` = consecutive crashes
/// of that pid inside the current window (design §3.4). W2 computes the
/// delay; this struct only carries the config.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackoffCfg {
    pub initial_ms: u64,
    pub factor: f64,
    pub max_ms: u64,
}

/// L4 W5: one pid's USER-EXPRESSED lifecycle intent, tracked in
/// `FlowRuntime::desired` and consulted (never guessed) by the restart
/// action. Deliberately not the same type as `builtins::flow`'s private
/// `RunStatus` -- that one is a RUN's actual, currently-observed status,
/// mutated by the proc's own task loop as it processes control; this one is
/// the FLOW-level record of what the user last ASKED for, mutated only by
/// the four lifecycle natives (`pause`/`resume`/`pause-proc`/`resume-proc`)
/// and by `stop-proc`, and read only by the supervisor's restart action. The
/// two can disagree by design -- that disagreement (crashed while the user's
/// intent was `Running`) is exactly the signal auto-resume acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesiredState {
    Running,
    Paused,
}

/// `:on-give-up` -- what the (future, W2) supervisor does once
/// `max_restarts` is exhausted inside `window_ms`. `Report` is the default
/// (owner ruling #3: supervision must never crash the flow either); a proc
/// left down under `Report` is otherwise inert, not respawned again unless
/// a later unrelated event resets its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnGiveUp {
    Report,
    StopFlow,
}

/// One entry of a validated `create-flow` cfg's `:procs` map: a pid's
/// launcher (see `core/flow.mova`'s `flow/process`/`flow/map->step` -- either
/// a bare 4-arity step-fn or a `{:mova.flow/step sf :mova.flow/workload w}`
/// launcher map, `builtins::flow` unwraps either shape), its `:args` map
/// (merged with `clojure.core.async.flow/pid` at init time), its
/// `:chan-opts` (io-id -> `:buf-or-n`), and the proc's own `describe()`
/// result -- captured ONCE at `create-flow` validation time (not
/// re-invoked at `start`) so eager validation and wiring see the exact same
/// port set.
pub struct ProcDef {
    pub launcher: Value,
    pub args: Value, // always a Value::Map (empty map if the cfg omitted :args)
    pub chan_opts: std::collections::HashMap<Value, usize>, // io-id -> buf-or-n
    pub ins: Vec<Value>,  // port ids from describe()'s :ins map, validation order
    pub outs: Vec<Value>, // port ids from describe()'s :outs map, validation order
    /// This proc's resolved `:supervision` policy (L4 W1,
    /// docs/L4-SUPERVISION-DESIGN.md §3.7/docs/L4-LANDING-SPEC.md §W1.4) --
    /// computed ONCE at `create-flow` by `builtins::flow`'s
    /// `resolve_supervision_opt`/`parse_supervision_map` (precedence:
    /// this proc's own `:supervision` > the flow-level default > none) and
    /// stored here directly, mirroring how a proc's resolved `:workload` is
    /// normalized into `launcher` rather than re-derived from the raw cfg on
    /// every later read (592fa55).
    ///
    /// `None` means "this proc is unsupervised" -- deliberately collapsing
    /// TWO distinct raw inputs (no `:supervision` key reached this proc at
    /// all, OR the resolved `:policy` is explicitly `:none`) into the one
    /// value every later reader actually cares about: neither
    /// `native_start`'s `sup_chan` decision nor W2's supervisor has
    /// anything to do for this pid either way, so there is no
    /// `SupervisionPolicy::None` variant here for them to have to keep
    /// re-checking. `MOVA_NO_SUPERVISION=1` also collapses to `None` here
    /// -- validation still ran, loudly, at `create-flow` (the kill switch is
    /// "parse-then-drop", not "skip parsing"), but the resolved answer never
    /// reaches a `ProcDef`.
    pub supervision: Option<SupervisionCfg>,
}

/// One `:conns` entry: `[[from-pid from-port] [to-pid to-port]]`.
#[derive(Clone)]
pub struct FlowConn {
    pub from_pid: Value,
    pub from_port: Value,
    pub to_pid: Value,
    pub to_port: Value,
}

/// A validated `create-flow` cfg -- immutable for the life of the flow (a
/// flow is start/stop, not re-configurable).
pub struct FlowDef {
    pub procs: std::collections::HashMap<Value, ProcDef>,
    /// `procs`' keys, sorted by `pr_str` once at validation time, so every
    /// engine operation that iterates "every proc" (start's wiring, ping,
    /// stop's broadcast) does so in a fixed, reproducible order --
    /// `imbl`/`std` `HashMap`s have no defined iteration order of their own
    /// (see `printer.rs`'s identical reasoning for map printing).
    pub proc_order: Vec<Value>,
    pub conns: Vec<FlowConn>,
}

/// What `flow/inject` puts into, for one wired in-port.
///
/// Always a real `Chan` -- a `flow/inject` is a THIRD writer arriving from
/// its own thread, which is exactly the shape the strictly-1:1 transport
/// cannot carry. When that in-port's engine traffic runs over a transport
/// instead, the `Chan` stays wired as the injection side channel and the
/// owning proc drains BOTH (see `builtins::flow`'s `InLane`); `gate` is the
/// cheap "is there anything in the side channel" flag that keeps the proc's
/// hot loop from taking that chan's mutex on every lap when nobody is
/// injecting. `None` means this in-port has no transport, so its `Chan` is
/// the only path and no gate is needed.
pub struct InjectPort {
    pub chan: Arc<Chan>,
    pub gate: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// A running proc's shutdown handles + its control channel (buf 10, fed
/// directly by `flow/stop`/`pause`/`resume`/`pause-proc`/`resume-proc`/
/// `ping`/`ping-proc` -- see `builtins::flow`'s module doc for why this is a
/// plain per-proc chan rather than a broadcast/mult: "the flow holds a
/// control chan per proc, commands pushed to each" is simpler than
/// upstream's mult+tap and has identical observable semantics).
pub struct ProcRuntime {
    pub control_chan: Arc<Chan>,
    /// This proc's DONE-CELL: an engine-created `Chan` that never carries a
    /// value and whose ONLY signal is `closed` -- closed by the proc's own
    /// spawn closure, on every exit path including a panic unwind (a `Drop`
    /// guard, see `builtins::flow`'s `DoneCells`). "Closed" means exactly
    /// "this proc's body has returned", which is what `flow/stop` waits for
    /// when there is no thread to join (L3 §3.5, and the hook L4 supervision
    /// is meant to consume).
    ///
    /// Every proc has one in BOTH worlds (task procs and `:workload :io`
    /// thread procs alike), so `stop` never has to ask which kind it is
    /// looking at; a thread proc's close costs one uncontended mutex at the
    /// end of its life. Deliberately NOT a member of
    /// `FlowRuntime::engine_owned_chans`: it is engine bookkeeping that no
    /// user value ever reaches, it is closed by its own proc rather than by
    /// `stop`'s close-everything phase, and it dies with this struct.
    pub done: Arc<Chan>,
    /// `Some` for a run's HEAD when the run was spawned as an OS thread;
    /// `None` for every task-spawned run and for a thread run's non-head
    /// fused members. `stop` joins the former and waits on `done` for the
    /// latter.
    pub thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Everything a *running* flow owns beyond its immutable `FlowDef`:
/// per-proc runtime handles, the report/error chans, and every chan the
/// engine itself created during wiring (shared 1:1 chans, mult sources,
/// mult-fed in-ports, control chans) -- bookkept here SEPARATELY from any
/// external `clojure.core.async.flow/in-ports`/`out-ports` a step-fn's
/// `init` may have supplied (those are discovered only inside the owning
/// proc thread and are never added here), which is what lets `stop` close
/// every engine-owned chan while strictly never touching a user-supplied
/// one.
pub struct FlowRuntime {
    pub procs: std::collections::HashMap<Value, ProcRuntime>,
    pub report_chan: Arc<Chan>,
    pub error_chan: Arc<Chan>,
    /// Every chan the engine created while wiring `start` (see the struct
    /// doc above): closed by `stop`, once, after every proc thread has been
    /// joined (or timed out and detached).
    pub engine_owned_chans: Vec<Arc<Chan>>,
    /// `(pid, io-id) -> injection target` as originally wired by `start`
    /// (BEFORE any proc's own `init` may have overridden that port via
    /// `clojure.core.async.flow/in-ports`) -- `flow/inject`'s target lookup.
    /// A proc that overrides its own in-port via `::flow/in-ports` is
    /// expected to be injected into directly (the caller already holds that
    /// channel, having created it) rather than through `flow/inject`.
    pub initial_ins: std::collections::HashMap<(Value, Value), InjectPort>,
    /// Every 1:1 transport link the engine created while wiring `start`
    /// (see `builtins::flow`'s "transport selection" section): closed by
    /// `stop`, once, alongside `engine_owned_chans` and for the same
    /// reason. Empty under `MOVA_NO_SPSC=1`, and empty for any flow whose
    /// topology has no eligible conn.
    pub engine_owned_links: Vec<crate::transport::SpscCloser>,
    /// Detached mult (fan-out/self-loop) threads, kept only so their count
    /// is inspectable in tests/debugging; not joined by `stop` (see that
    /// native's doc: they self-terminate once their source chan closes,
    /// which `stop` already guarantees via `engine_owned_chans`).
    ///
    /// ALWAYS EMPTY in the default world since L3.5 item 2: a mult is a
    /// runtime task now (`builtins::flow`'s `run_mult_task`), which has no
    /// `JoinHandle` to keep, and needs none -- fire-and-forget was already
    /// the contract this field documented. Only
    /// `MOVA_FLOW_THREAD_PROCS=1` still fills it.
    pub mult_threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// L4 W1: the flow's ONE supervision-event stream, `Some` iff at least
    /// one `ProcDef` in this flow resolved a `:policy :restart`
    /// `SupervisionCfg` (`builtins::flow::native_start` decides once, at
    /// spawn time). `Fixed(procs.len())` -- every member can die at once
    /// without `ExitGuard::drop`'s `try_put` ever finding the buffer full
    /// (wall W3: at most one in-flight event per pid, because a pid's next
    /// death requires its restart, and a restart is only issued after the
    /// supervisor has consumed the previous one -- see that fn's doc).
    ///
    /// Deliberately NOT a member of `engine_owned_chans`: `stop`'s
    /// close-everything must not be the thing that closes this. L4 W2
    /// closes it explicitly, FIRST -- `builtins::flow::stop_flow_cell`'s
    /// step 1b, design §3.6's stop-ordering -- so the supervisor observes a
    /// clean end-of-stream (and drains what is already buffered: a closed
    /// chan still delivers its buffer) rather than losing its last few death
    /// events to a race against `stop`'s broadcast. A death that lands AFTER
    /// that close is dropped on purpose (`ExitGuard::drop`'s `TryPut::Closed`
    /// arm): from step 1b onwards `stop`'s own done-cell waits own the
    /// endgame, and a restart decided out of such an event would be racing
    /// the teardown it belongs to.
    pub sup_chan: Option<Arc<Chan>>,
    /// L4 W2: the supervisor TASK's own done-cell -- `Some` exactly when
    /// `sup_chan` is (one supervisor per supervised flow,
    /// `builtins::flow::native_start` creates both together). Same shape and
    /// same contract as `ProcRuntime::done`: a `Fixed(1)` chan whose only
    /// signal is `closed`, closed by the supervisor itself as the last thing
    /// it does, and NOT in `engine_owned_chans` for the same reason a proc's
    /// done-cell is not (`stop` must never close the cell it is about to
    /// wait on).
    ///
    /// `stop_flow_cell` waits on it inside the SAME shared done-deadline as
    /// every proc's cell, after the per-proc waits: the supervisor is a
    /// supervised flow's k+1-th "proc" for shutdown purposes, and closing
    /// `sup_chan` (step 1b) is what starts it exiting.
    pub sup_done: Option<Arc<Chan>>,
    /// L4 W3: pids `flow/stop-proc` has already asked the supervisor to
    /// escalate on. Empty for every flow nobody calls that native on, which
    /// is all of them by default.
    ///
    /// **This set is what keeps `sup_chan`'s `Fixed(2n)` sizing honest** (see
    /// `sup_chan`'s doc): a stop request is put on that chan AT MOST ONCE per
    /// pid for the life of the flow, so a script looping on `stop-proc`
    /// cannot fill the buffer a death event needs. It is never cleared --
    /// a stop-proc'd run does not come back (`:proc-stopped` is terminal by
    /// user intent), so "already requested" is a permanent fact and not a
    /// piece of per-incarnation state.
    pub stop_requested: Mutex<std::collections::HashSet<Value>>,
    /// **L4 W5: per-pid user lifecycle INTENT** (owner ruling #5,
    /// docs/L4-SUPERVISION-DESIGN.md §8 -- "the supervisor mirrors user
    /// intent, never guesses"). One entry per pid the flow ever spawned,
    /// written by exactly five call sites: `native_start` seeds every pid
    /// `Paused` (upstream contract -- `FLOW-DESIGN.md`'s "procs start
    /// paused; resume begins reading"); `native_pause`/`native_resume`
    /// overwrite EVERY pid (a broadcast is a statement about the whole
    /// flow); `native_pause_proc`/`native_resume_proc` overwrite ONE;
    /// `native_stop_proc`'s rung 1 overwrites its target to `Paused` (a
    /// proc the user asked to stop has no desire to run). Read by exactly
    /// one site, `builtins::flow::Supervisor::act_restart`, under this same
    /// `runtime` lock, to decide whether a freshly swapped-in incarnation
    /// gets a synthesized `::flow/resume`.
    ///
    /// **Same nested-`Mutex`-behind-the-outer-lock shape as
    /// `stop_requested`, and for the same reason**: every reader/writer
    /// already holds (or, for the natives, immediately takes) the outer
    /// `runtime` lock to reach a `ProcRuntime` or to validate the flow is
    /// `Running`, so a second independent lock here would only add a lock
    /// order to reason about for no concurrency this file's single-writer
    /// natives + single-reader supervisor shape ever needs. It is NOT
    /// merged into `ProcRuntime` itself: a pid's desired state must survive
    /// every `ProcRuntime` swap a restart performs (the whole point), so it
    /// lives at the flow level, once, rather than being re-initialized -- or
    /// worse, silently reset -- each time `act_restart` inserts a fresh
    /// entry.
    pub desired: Mutex<std::collections::HashMap<Value, DesiredState>>,
}

/// A flow's lifecycle phase (`create-flow` -> `start` -> `stop`). No
/// `Paused`/`Running` distinction at this level -- that's a per-*proc*
/// status (`clojure.core.async.flow/status` in `ping`'s reply map), tracked
/// by each proc thread itself, not centrally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowPhase {
    Created,
    Running,
    Stopped,
}

/// `Value::Flow`'s cell. `def` is immutable; `phase`/`runtime` are the only
/// mutable state, guarded together by one lock so `start`/`stop` can never
/// race each other into an inconsistent phase/runtime pairing.
pub struct FlowCell {
    pub def: FlowDef,
    pub phase: Mutex<FlowPhase>,
    pub runtime: Mutex<Option<FlowRuntime>>,
}

/// C7 (vecveneer): the ONE canonical empty `Value::List` -- real
/// `clojure.lang.PersistentList.EMPTY` is a genuine JVM singleton, and
/// `identical?` on two independently-`PVec::new()`-built empty lists is
/// measured FALSE here (`PVec::Small`'s `Arc::from(Vec::new())` allocates
/// its own `Arc` per call -- unlike a zero-sized TYPE, an empty SLICE
/// allocation isn't canonicalized to one address), so every call site
/// that needs `identical?`-with-`PersistentList/EMPTY` to hold (currently
/// `builtins::statics`' own `PersistentList/EMPTY` registration and
/// `builtins::vecdot`'s SEQ `.empty` dot-method) must clone from THIS one
/// `OnceLock`, not call `Value::List(PVec::new())` fresh.
pub fn empty_list_singleton() -> Value {
    static EMPTY: OnceLock<Value> = OnceLock::new();
    EMPTY.get_or_init(|| Value::List(PVec::new())).clone()
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Sym(_) => "symbol",
            Value::Keyword(_) => "keyword",
            Value::Char(_) => "char",
            Value::List(_) => "list",
            Value::Vector(_) => "vector",
            // S7: its own name, so a type error says which one it got --
            // this feeds mova's diagnostics only (`key`'s "not a map
            // entry: vector" message), never `class`/`type`, which
            // `crate::types::builtin_class_name` owns.
            Value::MapEntry(_) => "map-entry",
            Value::Queue(_) => "queue",
            Value::Map(_) => "map",
            // W3: script-indistinguishable from a real map (conformance-by-
            // construction -- see `crate::host_struct`'s doc), so it
            // reports the same type name in error messages/`type`.
            Value::HostStruct(_) | Value::LazyMap(_) => "map",
            Value::Set(_) => "set",
            Value::Fn(_) => "function",
            Value::Native(_) => "native-function",
            Value::Macro(_) => "macro",
            Value::Atom(_) => "atom",
            Value::Volatile(_) => "volatile",
            Value::Reduced(_) => "reduced",
            Value::Lazy(_) | Value::LazyTail(_) => "lazy-seq",
            Value::Future(_) => "future",
            Value::Promise(_) => "promise",
            Value::Delay(_) => "delay",
            Value::Channel(_) => "channel",
            Value::Flow(_) => "flow",
            Value::Regex(_) => "regex",
            Value::Var(_) => "var",
            // SPEC-B-bignum-wiring.md: diagnostic-only names (this is not
            // Clojure's `class`/`type`, which mova doesn't fully model --
            // see the other `type_name` call sites, all error messages),
            // just enough for `numbers.rs`'s "expected a number, got a
            // {type_name}" style errors to name these honestly.
            Value::BigInt(_) => "bigint",
            Value::BigInteger(_) => "biginteger",
            Value::Ratio(_) => "ratio",
            Value::BigDec(_) => "bigdec",
            // S3: records read as maps in error messages (they ARE
            // associative); deftypes/classes name themselves.
            Value::Inst(i) => {
                if i.tdef.is_record {
                    "record"
                } else {
                    "type-instance"
                }
            }
            Value::Class(_) => "class",
            Value::Matcher(_) => "matcher",
            Value::Timer(_) => "timer",
            // SPEC-W6a: diagnostic-only, like `matcher`/`timer` above --
            // the exact (deftype) class name is
            // `types::builtin_class_name`'s job.
            Value::TcRandom(_) => "random",
            // S4/1D: diagnostic-only, like `bigint`/`ratio`/`bigdec` above
            // -- not Clojure's `class`/`type` (those name the exact
            // component-typed array class, see `types::array_jvm_name`).
            Value::Array(_) => "array",
            // S4: masquerade as their untyped counterparts in error
            // messages/`type`, same policy `HostStruct` uses above for
            // "map" -- a sorted map/set and a typed vector ARE maps/sets/
            // vectors to every generic op (see the choke-point wiring in
            // `builtins::collections`/`builtins::sorted`), just with extra
            // ordering/coercion behavior layered on top.
            Value::SortedMap(_) => "map",
            Value::SortedSet(_) => "set",
            Value::TypedVec(_) => "vector",
            // C2 (defstruct): same masquerade policy as `SortedMap` above
            // -- a struct-map IS a map to every generic op.
            Value::StructMap(_) => "map",
            Value::StructBasis(_) => "struct-basis",
            // C7 (vecveneer): diagnostic-only, like `matcher`/`array`
            // above -- the exact JVM class name is
            // `types::builtin_class_name`'s job.
            Value::VecSeq(_) => "seq",
            // S5: diagnostic-only, like `matcher`/`array` above -- the
            // exact JVM class name is `types::builtin_class_name`'s job.
            Value::HostInst(h) => h.kind.diagnostic_name(),
            // S6: diagnostic-only, like `matcher`/`array`/`HostInst` above.
            Value::Uuid(_) => "uuid",
            Value::Uri(_) => "uri",
            // S5/M3: metadata is invisible to `type`/`class`/error
            // messages -- a `(with-meta [1] {:a 1})` IS a vector.
            Value::Meta(m) => m.inner.type_name(),
        }
    }

    /// Clojure truthiness: everything except `nil` and `false` is truthy.
    /// A `Meta` wrapper is neither, and its inner value can never *be*
    /// `Nil`/`false` either (neither is `IObj`), so no unwrap is needed
    /// here -- keeping this a single `matches!` matters, it's on the hot
    /// path of every `if`/`and`/`or`/`when`.
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    // -----------------------------------------------------------------
    // S6: `java.util.UUID` text <-> `u128` (see `Value::Uuid`'s own doc).
    // Shared by `reader.rs` (`#uuid "..."`), `printer.rs` (`pr_str`/`str`),
    // and `builtins::statics` (`UUID/randomUUID`/`UUID/fromString`) --
    // kept ONE place so all three producers/consumers agree on the exact
    // text shape.
    // -----------------------------------------------------------------

    /// `java.util.UUID`'s canonical hyphenated lowercase text form,
    /// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (the 32 hex digits of `u`
    /// packed big-endian, hyphens at fixed positions 8/13/18/23) --
    /// matches `UUID.toString()`'s exact output (measured against
    /// 1.13.0-alpha6: `(str (java.util.UUID/randomUUID))` and
    /// `(pr-str ...)`'s inner text both this shape).
    pub fn format_uuid(u: u128) -> String {
        let hex = format!("{u:032x}");
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    }

    /// Strict inverse of [`Value::format_uuid`]: exactly 36 characters,
    /// ASCII hex digits (either case -- measured `UUID/fromString`
    /// accepts mixed case, e.g. `550E8400-...`, and normalizes to
    /// lowercase on print) with `-` at positions 8/13/18/23, anything
    /// else `None`. Deliberately narrower than the real `UUID.
    /// fromString`, which (via its `Long.parseLong` per-dash-group
    /// parse) tolerates a few malformed shapes real code never actually
    /// produces -- no vendored-suite site or this task's own oracle probe
    /// constructs anything but well-formed 8-4-4-4-12 text (`UUID/
    /// randomUUID`'s own output, and the fixed literal `550e8400-...`
    /// probed against the oracle), so this is a documented narrowing,
    /// not a literal port of the JVM ctor's parsing quirks.
    pub fn parse_uuid(s: &str) -> Option<u128> {
        let b = s.as_bytes();
        if b.len() != 36 {
            return None;
        }
        for &pos in &[8usize, 13, 18, 23] {
            if b[pos] != b'-' {
                return None;
            }
        }
        let mut hex = String::with_capacity(32);
        for (idx, &c) in b.iter().enumerate() {
            if idx == 8 || idx == 13 || idx == 18 || idx == 23 {
                continue;
            }
            if !c.is_ascii_hexdigit() {
                return None;
            }
            hex.push(c as char);
        }
        u128::from_str_radix(&hex, 16).ok()
    }

    /// `(java.util.UUID/randomUUID)`: 16 bytes of OS entropy
    /// (`libc::getentropy`, already a project dependency -- see
    /// `builtins::sys`'s precedent for bare `unsafe { libc::... }` calls
    /// in this codebase), then the RFC 4122 version-4/variant-1 bit-twiddle
    /// real `UUID.randomUUID()` applies (clear/set the 4 version bits in
    /// byte 6, clear/set the 2 variant bits in byte 8), then packed
    /// big-endian into a `u128` -- exactly `new UUID(randomBytes)`'s own
    /// byte order (`mostSigBits` from bytes 0..8, `leastSigBits` from
    /// bytes 8..16, both big-endian). Never oracle-comparable bit-for-bit
    /// (it's random on both sides) -- only the FORMAT (`format_uuid`) and
    /// the version/variant nibbles are measured/scored.
    ///
    /// **L5/W3 fence #8 (design §4): SEEDED in sim.** The 16 bytes come from
    /// two draws on `clock::user_next`'s user stream instead of the OS
    /// entropy pool, and then take the identical version/variant twiddle.
    /// Consequence, stated because it IS the point rather than a defect: sim
    /// UUIDs are deterministic per seed, and therefore NOT unique across two
    /// runs of the same program at the same seed. A simulation whose whole
    /// promise is "same seed, same execution" cannot contain a value that
    /// differs every time; a program needing cross-run-unique ids under sim
    /// must derive them from something the simulation does not own.
    pub fn random_uuid_bits() -> u128 {
        let mut bytes = [0u8; 16];
        if crate::clock::sim_enabled() {
            bytes[..8].copy_from_slice(&crate::clock::user_next().to_be_bytes());
            bytes[8..].copy_from_slice(&crate::clock::user_next().to_be_bytes());
            bytes[6] = (bytes[6] & 0x0f) | 0x40;
            bytes[8] = (bytes[8] & 0x3f) | 0x80;
            return u128::from_be_bytes(bytes);
        }
        // SAFETY: `getentropy` either fills exactly `bytes.len()` bytes
        // (<= 256, which 16 always satisfies) or returns -1; the `!= 0`
        // check below treats any failure as "use the fallback" rather
        // than trusting a partially-written buffer.
        let ok = unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) == 0 };
        if !ok {
            // Transient-failure fallback: nanosecond clock mixed with a
            // monotonic counter, same shape as `hostclass::time_seed`'s
            // unseeded-`Random` fallback -- fine because, as the doc
            // above says, no golden ever pins `randomUUID`'s actual bits
            // on either side.
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let hi = nanos ^ c.wrapping_mul(0x2545_F491_4F6C_DD1D);
            let lo = nanos.rotate_left(31) ^ c.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            bytes[..8].copy_from_slice(&hi.to_be_bytes());
            bytes[8..].copy_from_slice(&lo.to_be_bytes());
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        u128::from_be_bytes(bytes)
    }

    // -----------------------------------------------------------------
    // S5 / M3: metadata (see `MetaObj`'s doc for the design + invariants)
    // -----------------------------------------------------------------

    /// See through a `Value::Meta` wrapper. THE default for every READ
    /// operation: `(count (with-meta [1 2] {:a 1}))` is 2, `(get ...)`,
    /// `(nth ...)`, `seq`, arithmetic, printing -- everything that asks
    /// "what is this value" wants the inner value, not the wrapper.
    ///
    /// Cheap by construction: one discriminant test, and by the `inner`
    /// invariant it never needs to loop.
    #[inline]
    pub fn unmeta(&self) -> &Value {
        match self {
            Value::Meta(m) => &m.inner,
            other => other,
        }
    }

    /// Consuming [`unmeta`](Value::unmeta): moves the inner value out
    /// (cloning only when the wrapper `Arc` is shared).
    #[inline]
    pub fn into_unmeta(self) -> Value {
        match self {
            Value::Meta(m) => match Arc::try_unwrap(m) {
                Ok(obj) => obj.inner,
                Err(shared) => shared.inner.clone(),
            },
            other => other,
        }
    }

    /// `true` iff this value carries an `IObj` metadata wrapper.
    #[inline]
    pub fn has_meta(&self) -> bool {
        matches!(self, Value::Meta(_))
    }

    /// `clojure.core/meta`: the attached map, or `Nil`. IReference
    /// metadata (vars/atoms) is NOT handled here -- that lives in a
    /// separate mutable slot on the cell itself, see
    /// `builtins::meta::meta_of_value`.
    #[inline]
    pub fn obj_meta(&self) -> Value {
        match self {
            Value::Meta(m) => m.meta.clone(),
            _ => Value::Nil,
        }
    }

    /// The one `Value::Meta` constructor. Enforces both invariants:
    /// a `Nil`/empty-`Nil` meta strips the wrapper, and `inner` is
    /// flattened so a wrapper never nests inside a wrapper.
    ///
    /// Note the deliberate asymmetry (measured): an *empty map* is a
    /// real metadata value (`(meta (with-meta [] {}))` is `{}`), only
    /// `nil` strips.
    pub fn attach_meta(inner: Value, meta: Value) -> Value {
        let inner = inner.into_unmeta();
        match meta {
            Value::Nil => inner,
            meta => Value::Meta(Arc::new(MetaObj { meta, inner })),
        }
    }

    /// Copy `src`'s metadata (if any) onto `self`. The workhorse of the
    /// per-op preservation table: an UPDATE on a collection
    /// (`conj`/`assoc`/`dissoc`/`disj`/`pop`/`into`/`empty`) returns a
    /// value carrying the receiver's metadata, while a rebuild
    /// (`map`/`filter`/`vec`/`keys`/`seq`-on-a-vector) does not. Measured
    /// per op against 1.13.0-alpha6 -- see `tests/conformance/pending/
    /// metadata.corpus` and this module's `meta_preservation` tests.
    #[inline]
    pub fn with_meta_of(self, src: &Value) -> Value {
        match src {
            Value::Meta(m) => Value::attach_meta(self, m.meta.clone()),
            _ => self,
        }
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::printer::pr_str(self))
    }
}

/// Deviation (documented in ARCHITECTURE.md): floats compare/hash by
/// `f64::to_bits`, so `NaN == NaN` as a collection key. `Int(1) != Float(1.0)`
/// here (the blended numeric `=` lives in `builtins::numbers` instead). Fns,
/// natives, macros and atoms compare by `Arc::ptr_eq` (as do futures,
/// promises, and delays). `Lazy` is forced before comparison. Lists and
/// vectors are `=` to each other when their elements match (Clojure
/// sequence equality). `Regex` compares by pattern string (`.as_str()`),
/// not identity or compiled-automaton equivalence. `Var` compares by
/// `Arc::ptr_eq`, same as the other cell-backed variants -- two `Value::Var`
/// only `=` when they resolved to the identical `VarCell`.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        use Value::*;
        match (self, other) {
            (Nil, Nil) => true,
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (Float(a), Float(b)) => a.to_bits() == b.to_bits(),
            (Str(a), Str(b)) => a == b,
            (Sym(a), Sym(b)) => a == b,
            // W-GEO stage 1: unchanged in shape -- a one-line delegation,
            // now to `Keyword`'s `PartialEq` (bare `u32` compare when both
            // sides are interned, content compare otherwise) instead of
            // `Str`'s. Same answer for every input; see `keyword.rs`.
            (Keyword(a), Keyword(b)) => a == b,
            (Char(a), Char(b)) => a == b,
            // S7: `MapEntry` joins the sequence-equality class unchanged
            // -- measured both directions, `(= (first {:a 1}) [:a 1])`,
            // `(= [:a 1] (first {:a 1}))` and `(= (first {:a 1}) '(:a 1))`
            // are all `true`, and `(get {[:a 1] :hit} (first {:a 1}))`
            // finds the vector-keyed entry (which additionally REQUIRES
            // the identical hash stream -- see `Hash` below).
            // C10: `Queue` joins the same sequence-equality class --
            // measured, `(= (conj EMPTY 1 2 3) '(1 2 3))` and `(= [1 2 3]
            // (conj EMPTY 1 2 3))` are both `true`, both directions.
            (
                List(a) | Vector(a) | MapEntry(a) | Queue(a),
                List(b) | Vector(b) | MapEntry(b) | Queue(b),
            ) => a == b,
            (Map(a), Map(b)) => a == b,
            // W3: a `HostStruct` is `=` to a real `Map`/another `HostStruct`
            // iff their materialized `PMap`s (`host_struct::as_pmap`'s ONE
            // choke point) are `=` -- delegation, not a bespoke comparison,
            // so this can never drift from `PMap`'s own equality contract.
            (HostStruct(a), Map(b)) | (Map(b), HostStruct(a)) => crate::host_struct::as_pmap(a) == b,
            (HostStruct(a), HostStruct(b)) => crate::host_struct::as_pmap(a) == crate::host_struct::as_pmap(b),
            (LazyMap(a), Map(b)) | (Map(b), LazyMap(a)) => crate::lazy_map::as_pmap(a) == b,
            (LazyMap(a), LazyMap(b)) => Arc::ptr_eq(a, b) || crate::lazy_map::as_pmap(a) == crate::lazy_map::as_pmap(b),
            (LazyMap(a), HostStruct(b)) | (HostStruct(b), LazyMap(a)) => {
                crate::lazy_map::as_pmap(a) == crate::host_struct::as_pmap(b)
            }
            (SortedMap(a), LazyMap(b)) | (LazyMap(b), SortedMap(a)) => {
                let m = crate::lazy_map::as_pmap(b);
                a.entries.len() == m.len() && a.entries.iter().all(|(k, v)| m.get(k) == Some(v))
            }
            (StructMap(a), LazyMap(b)) | (LazyMap(b), StructMap(a)) => {
                let m = crate::lazy_map::as_pmap(b);
                a.entries.len() == m.len() && a.entries.iter().all(|(k, v)| m.get(k) == Some(v))
            }
            (Set(a), Set(b)) => a == b,
            (Fn(a), Fn(b)) => Arc::ptr_eq(a, b),
            (Native(a), Native(b)) => Arc::ptr_eq(a, b),
            (Macro(a), Macro(b)) => Arc::ptr_eq(a, b),
            (Atom(a), Atom(b)) => Arc::ptr_eq(a, b),
            (Volatile(a), Volatile(b)) => Arc::ptr_eq(a, b),
            // C10: identity, like every other cell-backed variant -- a
            // bare `Reduced` escaping to user code for comparison isn't a
            // shape any measured test relies on (it's meant to be
            // unwrapped by a reduce loop before anyone sees it).
            (Reduced(a), Reduced(b)) => Arc::ptr_eq(a, b),
            (Future(a), Future(b)) => Arc::ptr_eq(a, b),
            (Promise(a), Promise(b)) => Arc::ptr_eq(a, b),
            (Delay(a), Delay(b)) => Arc::ptr_eq(a, b),
            (Channel(a), Channel(b)) => Arc::ptr_eq(a, b),
            (Flow(a), Flow(b)) => Arc::ptr_eq(a, b),
            (Regex(a), Regex(b)) => a.as_str() == b.as_str(),
            (Var(a), Var(b)) => Arc::ptr_eq(a, b),
            // SPEC-B-bignum-wiring.md §4: `BigInt` cross-`=`s with `Int`
            // ONLY when it fits an `i64` exactly (measured: `(= 7N 7)` and
            // `(= 7 7N)` both true) -- this is the ONE cross-type numeric
            // bridge these new variants get; unlike the S1 Int/Float
            // blending (`builtins::numbers`), it does NOT extend to
            // `Float`/`Ratio`/`BigDec` (measured: `(= 7N 7.0)` is false).
            (BigInt(a), Int(b)) | (Int(b), BigInt(a)) => a.to_i64_exact() == Some(*b),
            (BigInt(a), BigInt(b)) => a == b,
            // S5: `BigInteger` is the third member of the INTEGER equality
            // category (`clojure.lang.Numbers`'s `Category.INTEGER` holds
            // Long/Integer/Short/Byte/BigInt/BigInteger) -- measured
            // `(= (biginteger 5) 5)` and `(= (biginteger 5) 5N)` are both
            // true. Kept adjacent to the `BigInt` arms above because they
            // are the same bridge, one type wider.
            (BigInteger(a), Int(b)) | (Int(b), BigInteger(a)) => a.to_i64_exact() == Some(*b),
            (BigInteger(a), BigInt(b)) | (BigInt(b), BigInteger(a)) => a == b,
            (BigInteger(a), BigInteger(b)) => a == b,
            // `Ratio` and `BigDec` are `=` ONLY to their own type (measured:
            // `(= 1/2 0.5)` false, `(= 7M 7)` false) -- no wildcard bridge,
            // so a stray `(Ratio, Int)`/`(BigDec, Float)` pair correctly
            // falls through to the `_ => false` catch-all below.
            (Ratio(a), Ratio(b)) => a == b,
            (BigDec(a), BigDec(b)) => a == b,
            // TODO(P2): once eval.rs exists, force both thunks through an
            // `&mut Interp` before comparing. For now we can only compare
            // identity or already-realized values.
            // C3e: `LazyTail` rides this arm verbatim -- see its doc for
            // why the internal continuation marker must compare exactly
            // as the `Lazy` it wraps.
            (Lazy(a) | LazyTail(a), Lazy(b) | LazyTail(b)) => {
                if Arc::ptr_eq(a, b) {
                    true
                } else {
                    match (lock_mutex(&a.realized).as_ref(), lock_mutex(&b.realized).as_ref()) {
                        (Some(x), Some(y)) => x == y,
                        _ => false,
                    }
                }
            }
            // S3 (measured): records are `=` iff SAME type (Arc identity --
            // re-defrecord makes a new class) and same full map view
            // (basis + ext), meta ignored; a record is NEVER `=` to a
            // plain map in either direction (falls through to the
            // catch-all). Deftypes are identity-equal only (`(= (T. 1)
            // (T. 1))` is false). Classes: builtins by name, user classes
            // by TypeDef identity.
            (Inst(a), Inst(b)) => {
                if !Arc::ptr_eq(&a.tdef, &b.tdef) {
                    false
                } else if a.tdef.is_record {
                    a.data == b.data
                } else {
                    Arc::ptr_eq(a, b)
                }
            }
            (Class(a), Class(b)) => match (a.as_ref(), b.as_ref()) {
                (
                    crate::types::ClassVal::Builtin { name: n1, .. },
                    crate::types::ClassVal::Builtin { name: n2, .. },
                ) => n1 == n2,
                (crate::types::ClassVal::User(t1), crate::types::ClassVal::User(t2)) => {
                    Arc::ptr_eq(t1, t2)
                }
                // S5: interfaces are `=` by NAME -- see
                // `ClassVal::Interface`'s doc for why identity is the
                // name and not the `Arc` (values are interned per name,
                // so this and `Arc::ptr_eq` agree; name is the honest
                // statement of the rule).
                (
                    crate::types::ClassVal::Interface { name: n1 },
                    crate::types::ClassVal::Interface { name: n2 },
                ) => n1 == n2,
                // W3a: a name that appears in BOTH `types::builtin_classes()`
                // and `types::builtin_interfaces()` -- `java.util.Collection`
                // is the live example, `clojure.lang.ISeq` and
                // `java.util.Map$Entry` are others -- yields a `Builtin`
                // value from `builtins::types::class_by_full_name` and an
                // `Interface` value from the global var (`install` binds the
                // interface table LAST, overwriting the class binding). On
                // the JVM there is exactly ONE `Class` object per name, so
                // those two must be `=`; and `Hash` above ALREADY hashes
                // just the name for both variants, so without this arm the
                // pair hashes alike but compares unequal -- a broken
                // `Hash`/`Eq` contract that silently loses hash-map lookups.
                // That is not theoretical: it is why
                // `(derive java.util.Collection ::collection)` followed by
                // `(isa? clojure.lang.PersistentVector ::collection)`
                // answered `false` (multimethods.clj's `isA-multimethod-test`
                // -- `multi::isa_values`' derive-bridging walk looks the
                // bridged superclass up in the hierarchy map, and the lookup
                // missed).
                //
                // (`Builtin::name` is `&'static str` and `Interface::name`
                // is `Str`; `Str`'s own `Hash` is documented to stream
                // exactly what `<str as Hash>::hash` does, which is what
                // makes the two variants' hashes agree in the first place.)
                (
                    crate::types::ClassVal::Builtin { name: n1, .. },
                    crate::types::ClassVal::Interface { name: n2 },
                ) => n2.as_ref() == *n1,
                (
                    crate::types::ClassVal::Interface { name: n1 },
                    crate::types::ClassVal::Builtin { name: n2, .. },
                ) => n1.as_ref() == *n2,
                _ => false,
            },
            (Matcher(a), Matcher(b)) => Arc::ptr_eq(a, b),
            (Timer(a), Timer(b)) => Arc::ptr_eq(a, b),
            // SPEC-W6a: STRUCTURAL, unlike every `Arc`-identity variant
            // around it -- an inline `Copy` payload has no identity to
            // compare. See `Value::TcRandom`'s doc for the measured JVM
            // divergence this accepts, and why nothing observes it.
            (TcRandom(a), TcRandom(b)) => a == b,
            // S4/1D (measured): two arrays are `=` iff the SAME array
            // (identity) -- real Java arrays never override
            // `Object.equals`, so even two same-kind, same-content arrays
            // built separately are NOT `=`. Same precedent as every other
            // `Arc`-identity variant above (`Fn`/`Atom`/`Var`/...).
            (Array(a), Array(b)) => Arc::ptr_eq(a, b),
            // S4: content-only equality, `cmp` never consulted -- see
            // `SortedMapVal`/`SortedSetVal`'s doc. Cross-`=` with plain
            // `Map`/`Set`/`HostStruct` (measured: `(= (sorted-map 1 :a) {1
            // :a})` true both directions) delegates to `PMap`'s own
            // unordered-pairs `PartialEq` by building a throwaway `PMap`
            // from `entries` -- entries are already deduped (comparator-
            // equal keys collapsed at insert time), so this is a faithful
            // one-shot conversion, not a re-derivation of map equality.
            (SortedMap(a), SortedMap(b)) => {
                a.entries.len() == b.entries.len()
                    && a.entries.iter().all(|(k, v)| {
                        b.entries.iter().any(|(k2, v2)| k == k2 && v == v2)
                    })
            }
            (SortedMap(a), Map(b)) | (Map(b), SortedMap(a)) => {
                a.entries.len() == b.len() && a.entries.iter().all(|(k, v)| b.get(k) == Some(v))
            }
            (SortedMap(a), HostStruct(b)) | (HostStruct(b), SortedMap(a)) => {
                let m = crate::host_struct::as_pmap(b);
                a.entries.len() == m.len() && a.entries.iter().all(|(k, v)| m.get(k) == Some(v))
            }
            // C2 (defstruct): content-only equality, `basis` never
            // consulted -- see `StructMapVal`'s doc. Measured: two structs
            // built from DIFFERENT `defstruct`s with the same keys/values
            // are `=` (`(= (struct s1 1 2) (struct s2 1 2))` true even
            // though `s1`/`s2` are separate `create-struct` calls), and a
            // struct is `=` to a plain map/sorted-map/`HostStruct` with the
            // same content in BOTH directions -- same cross-type-map-family
            // shape `SortedMap`'s own arms above already establish.
            (StructMap(a), StructMap(b)) => {
                a.entries.len() == b.entries.len()
                    && a.entries.iter().all(|(k, v)| b.entries.iter().any(|(k2, v2)| k == k2 && v == v2))
            }
            (StructMap(a), Map(b)) | (Map(b), StructMap(a)) => {
                a.entries.len() == b.len() && a.entries.iter().all(|(k, v)| b.get(k) == Some(v))
            }
            (StructMap(a), HostStruct(b)) | (HostStruct(b), StructMap(a)) => {
                let m = crate::host_struct::as_pmap(b);
                a.entries.len() == m.len() && a.entries.iter().all(|(k, v)| m.get(k) == Some(v))
            }
            (StructMap(a), SortedMap(b)) | (SortedMap(b), StructMap(a)) => {
                a.entries.len() == b.entries.len()
                    && a.entries.iter().all(|(k, v)| b.entries.iter().any(|(k2, v2)| k == k2 && v == v2))
            }
            // C2: identity only -- see `Value::StructBasis`'s own doc.
            (StructBasis(a), StructBasis(b)) => Arc::ptr_eq(a, b),
            (SortedSet(a), SortedSet(b)) => {
                a.entries.len() == b.entries.len() && a.entries.iter().all(|x| b.entries.contains(x))
            }
            (SortedSet(a), Set(b)) | (Set(b), SortedSet(a)) => {
                a.entries.len() == b.len() && a.entries.iter().all(|x| b.contains(x))
            }
            // S4: a `TypedVec` is sequence-`=` to a plain `List`/`Vector`
            // (and another `TypedVec`) with the same elements -- `kind` is
            // never consulted, matching Clojure's `IPersistentVector`
            // equality across `clojure.core.Vec` and `PersistentVector`
            // (measured: `(= (vector-of :int 1 2 3) [1 2 3])` true both
            // directions).
            (TypedVec(a), TypedVec(b)) => a.data == b.data,
            // C10: `Queue` joins this cross-comparison too (measured:
            // `(= (vector-of :long 1 2 3) (conj EMPTY 1 2 3))` is `true`).
            (TypedVec(a), List(b) | Vector(b) | MapEntry(b) | Queue(b))
            | (List(b) | Vector(b) | MapEntry(b) | Queue(b), TypedVec(a)) => a.data == *b,
            // C7 (vecveneer): a `VecSeq` (either `kind` -- concrete class
            // is never consulted here, same "sequence equality is
            // class-blind" precedent `TypedVec` above follows) is
            // sequence-`=` to a `List`/`Vector`/`TypedVec`/another
            // `VecSeq` with the same elements (measured: `(= (.rseq [0 1
            // 2]) '(2 1 0))` true).
            (VecSeq(a), VecSeq(b)) => a.items == b.items,
            (VecSeq(a), List(b) | Vector(b) | MapEntry(b))
            | (List(b) | Vector(b) | MapEntry(b), VecSeq(a)) => a.items == *b,
            (VecSeq(a), TypedVec(b)) | (TypedVec(b), VecSeq(a)) => a.items == b.data,
            // C7 (vecveneer): a `java.util.ArrayList` IS content-equal to
            // a `List`/`Vector`/`TypedVec` with the same elements
            // (measured: `(= [0 1 2] (new java.util.ArrayList [0 1 2]))`
            // true both directions -- real `ArrayList.equals` implements
            // `java.util.List`'s sequence-equality contract, same as
            // Clojure's own vectors) -- checked BEFORE the identity-equal
            // `(HostInst, HostInst)` arm below, which still covers every
            // other `HostKind` (`Random`/`Date`/`Thread`/`ThreadLocal`/
            // `StringBuilder`/`StringBuffer`, none of which override
            // `equals` on the real JVM).
            (HostInst(a), List(b) | Vector(b) | MapEntry(b))
            | (List(b) | Vector(b) | MapEntry(b), HostInst(a))
                if a.kind == crate::hostclass::HostKind::ArrayList =>
            {
                match &*lock_mutex(&a.state) {
                    crate::hostclass::HostState::ArrayList(items) => items == b,
                    _ => false,
                }
            }
            (HostInst(a), TypedVec(b)) | (TypedVec(b), HostInst(a))
                if a.kind == crate::hostclass::HostKind::ArrayList =>
            {
                match &*lock_mutex(&a.state) {
                    crate::hostclass::HostState::ArrayList(items) => *items == b.data,
                    _ => false,
                }
            }
            // SPEC-W6b: `java.util.Date` IS value-equal on the real JVM --
            // `Date.equals` is `getTime() == other.getTime()` -- so two
            // independently-read `#inst "1942"` literals are `=` there and
            // must be here (measured on 1.13.0-alpha6: `(= #inst "1942"
            // #inst "1942")`, `(= (java.util.Date. 5) (java.util.Date. 5))`
            // and `(= #{(java.util.Date. 5)} #{(java.util.Date. 5)})` are
            // all `true`). The blanket `Arc`-identity arm below used to
            // claim in its own comment that `Date` does not override
            // `equals`; that was simply wrong, and it cost
            // `tests/clojure-suite/vendor/spec.clj`'s `conform-explain`
            // three `s/inst-in` rows. Checked BEFORE that arm, and
            // `Value::hash` carries the matching case so the `a == b =>
            // hash(a) == hash(b)` contract still holds inside a set/map.
            (HostInst(a), HostInst(b))
                if a.kind == crate::hostclass::HostKind::Date
                    && b.kind == crate::hostclass::HostKind::Date =>
            {
                // `Arc::ptr_eq` first: comparing a cell with ITSELF is
                // both the common case and the one where locking the same
                // `Mutex` twice below would deadlock.
                Arc::ptr_eq(a, b)
                    || match (&*lock_mutex(&a.state), &*lock_mutex(&b.state)) {
                        (
                            crate::hostclass::HostState::Date(x),
                            crate::hostclass::HostState::Date(y),
                        ) => x == y,
                        _ => false,
                    }
            }
            // S5: `Arc` identity, matching every other cell-backed variant
            // above (none of `Random`/`Thread`/`ThreadLocal`/
            // `StringBuilder`/`StringBuffer` override `equals` on the real
            // JVM either; `Date`, which does, is the arm just above).
            (HostInst(a), HostInst(b)) => Arc::ptr_eq(a, b),
            // S6: value equality, matching real `UUID.equals`/`URI.equals`
            // (measured: two `UUID/fromString`/`java.net.URI.` calls on the
            // same text are `=`) -- unlike the `Arc`-identity host cells
            // just above, these carry no mutable state to distinguish two
            // independently-constructed-but-equal instances.
            (Uuid(a), Uuid(b)) => a == b,
            (Uri(a), Uri(b)) => a == b,
            // S5/M3: metadata is excluded from `=` -- measured, `(= (with-meta
            // [1 2] {:a 1}) (with-meta [1 2] {:b 2}))` is true, and so is
            // `(= {:x 1} (with-meta {:x 1} {:a 1}))`. Recursing on the inner
            // value rather than testing for `Meta` at the top of this fn is
            // deliberate: `=` is one of the hottest functions in the
            // interpreter, and the tuple match above already loads both
            // discriminants, so handling it as two more ARMS costs the
            // overwhelmingly common non-`Meta` comparison literally nothing,
            // where a leading `matches!` pair cost it two extra branches
            // (measured at ~1.5% on the 10M-iteration loop benchmark).
            // The `(Meta, Meta)` pairing lands on the first of these and
            // then on the second, terminating after two hops because
            // `MetaObj::inner` is never itself a `Meta`.
            (Meta(a), _) => a.inner == *other,
            (_, Meta(b)) => *self == b.inner,
            // C13: `Arc` identity, matching every other opaque cell-backed
            _ => false,
        }
    }
}

impl Eq for Value {}

fn hash_unordered_elems<'a, I: Iterator<Item = &'a Value>>(iter: I) -> u64 {
    let mut acc: u64 = 0;
    for v in iter {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        acc ^= h.finish();
    }
    acc
}

fn hash_unordered_pairs<'a, I: Iterator<Item = (&'a Value, &'a Value)>>(iter: I) -> u64 {
    let mut acc: u64 = 0;
    for (k, v) in iter {
        let mut h = DefaultHasher::new();
        k.hash(&mut h);
        v.hash(&mut h);
        acc ^= h.finish();
    }
    acc
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            // S5/M3: NO hash tag of its own -- a `Meta` wrapper hashes
            // EXACTLY as its inner value, which is Rust's `a == b =>
            // hash(a) == hash(b)` contract given `PartialEq` above
            // unwraps, and is Clojure's measured behavior: `(= (hash
            // (with-meta [1] {:a 1})) (hash [1]))` is true.
            Value::Meta(m) => m.inner.hash(state),
            Value::Nil => 0u8.hash(state),
            Value::Bool(b) => {
                1u8.hash(state);
                b.hash(state);
            }
            Value::Int(i) => {
                2u8.hash(state);
                i.hash(state);
            }
            Value::Float(f) => {
                3u8.hash(state);
                f.to_bits().hash(state);
            }
            Value::Str(s) => {
                4u8.hash(state);
                s.as_ref().hash(state);
            }
            Value::Sym(sym) => {
                5u8.hash(state);
                sym.hash(state);
            }
            // W-GEO stage 1: a one-line delegation to `Keyword`'s own
            // `Hash`, which is CONTENT-based unconditionally for both of
            // its arms (see that impl for why the interned id's hash
            // speedup is deliberately unavailable here). The byte stream
            // is unchanged from the pre-stage-1 `k.as_ref().hash(state)`:
            // `Keyword` -> `Str` -> `<str as Hash>::hash`.
            Value::Keyword(k) => {
                6u8.hash(state);
                k.hash(state);
            }
            Value::Char(c) => {
                7u8.hash(state);
                c.hash(state);
            }
            // List and Vector must hash identically when their elements
            // match, since `=` treats them as sequence-equal.
            //
            // S7: `MapEntry` writes the IDENTICAL tag+stream -- no tag of
            // its own, exactly the precedent `SortedMap` sets by reusing
            // `Map`'s 9 and `TypedVec` sets by reusing this 8. `=` above
            // makes an entry and a 2-vector equal, so Rust's `a == b =>
            // hash(a) == hash(b)` contract (and Clojure's measured
            // `(= (hash (first {:a 1})) (hash [:a 1]))` => `true`, plus
            // `(get {[:a 1] :hit} (first {:a 1}))` => `:hit`) leaves no
            // other option.
            // C10: `Queue` reuses tag 8 too -- measured, `(hash (conj
            // EMPTY 1 2 3))` == `(hash [1 2 3])` == `(hash (list 1 2 3))`.
            Value::List(v) | Value::Vector(v) | Value::MapEntry(v) | Value::Queue(v) => {
                8u8.hash(state);
                v.len().hash(state);
                if let PVec::Col(_) = v {
                    for item in v.clone() {
                        item.hash(state);
                    }
                } else {
                    for item in v.iter() {
                        item.hash(state);
                    }
                }
            }
            Value::Map(m) => {
                9u8.hash(state);
                m.len().hash(state);
                state.write_u64(hash_unordered_pairs(m.iter()));
            }
            // W3: MUST write the identical tag+stream `Map` does above --
            // this is what makes a `HostStruct` collide correctly with an
            // `=` real map when used as a `HashMap`/`HashSet` key (Rust's
            // own contract: `a == b` implies `hash(a) == hash(b)`), per
            // `PartialEq`'s `(HostStruct, Map)` delegation above.
            Value::HostStruct(hs) => {
                let m = crate::host_struct::as_pmap(hs);
                9u8.hash(state);
                m.len().hash(state);
                state.write_u64(hash_unordered_pairs(m.iter()));
            }
            Value::LazyMap(lm) => {
                let m = crate::lazy_map::as_pmap(lm);
                9u8.hash(state);
                m.len().hash(state);
                state.write_u64(hash_unordered_pairs(m.iter()));
            }
            Value::Set(s) => {
                10u8.hash(state);
                s.len().hash(state);
                state.write_u64(hash_unordered_elems(s.iter()));
            }
            Value::Fn(rc) => {
                11u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            Value::Native(rc) => {
                12u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            Value::Macro(rc) => {
                13u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            Value::Atom(rc) => {
                14u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            // Own tag (26 -- the next free slot after `BigDec`'s 25 above),
            // pointer identity like every other cell-backed variant.
            Value::Volatile(rc) => {
                26u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            // C10: own tag (35, the next free slot after 34) -- pointer
            // identity, matching `PartialEq`'s `(Reduced, Reduced)` arm
            // above.
            Value::Reduced(rc) => {
                35u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            // C10: a REALIZED lazy seq hashes EXACTLY as its content, no
            // tag of its own -- same "no tag, unwrap" shape as `Meta`
            // above, and for the same reason: `values_equal` (what `=`
            // calls) forces a `Lazy` before comparing, so `(= (lazy-seq
            // (list 1 2 3)) [1 2 3])` is `true` (measured) -- a `15u8` tag
            // here broke the `a == b => hash(a) == hash(b)` contract for
            // every such pair (`ordered-collection-equality-test`'s
            // `is-same-collection` helper hits exactly this: a lazy-seq,
            // a vector, and a queue, all holding the same elements, all
            // asserted `(= (hash a) (hash b))`). An UNREALIZED cell keeps
          // the old identity-based fallback (still a TODO(P2): forcing
            // here would need `&mut Interp`, not available from `Hash`'s
            // signature) -- two never-yet-forced `Lazy`s legitimately
            // hash differently, same as `Value::PartialEq`'s own
            // `(Lazy, Lazy)` arm, which only compares content when BOTH
            // sides are already realized.
            //
            // The ONE exception to "hash exactly as content": a realized-
            // to-`Nil` lazy (an EMPTY lazy seq) does NOT hash as bare
            // `Nil` (tag 0) -- it must hash as an EMPTY SEQUENCE (tag 8,
            // len 0), because `values_equal`'s own `(Nil, Nil) => a_was_
            // lazy == b_was_lazy` rule makes `(= nil (lazy-seq))` FALSE
            // (measured) while `(= [] (lazy-seq))`/`(= (list) (lazy-
            // seq))` are TRUE -- a realized-empty-lazy is `=`-equal to an
            // empty `List`/`Vector`, never to bare `nil`. Hashing it as
            // `Nil` would violate `a == b => hash(a) == hash(b)` for the
            // former pair, exactly the bug this arm exists to avoid for
            // non-empty content.
            // C3e: `LazyTail` hashes exactly as the `Lazy` it wraps --
            // required by `a == b => hash(a) == hash(b)`, since
            // `PartialEq` above puts the two in one equivalence class.
            Value::Lazy(rc) | Value::LazyTail(rc) => match lock_mutex(&rc.realized).as_ref() {
                Some(Value::Nil) => {
                    8u8.hash(state);
                    0usize.hash(state);
                }
                Some(realized) => realized.hash(state),
                None => {
                    15u8.hash(state);
                    (Arc::as_ptr(rc) as usize).hash(state);
                }
            },
            Value::Future(rc) => {
                16u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            Value::Promise(rc) => {
                17u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            Value::Delay(rc) => {
                18u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            Value::Channel(rc) => {
                19u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            Value::Flow(rc) => {
                20u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            // Must agree with `PartialEq`'s pattern-string comparison above.
            Value::Regex(re) => {
                21u8.hash(state);
                re.as_str().hash(state);
            }
            Value::Var(rc) => {
                22u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            // SPEC-B-bignum-wiring.md §4: a `BigInt` that fits an `i64`
            // MUST hash byte-for-byte like `Value::Int` (tag `2u8` + the
            // `i64`), not its own tag -- that is the only way `(hash 7N)
            // == (hash 7)` (measured) can hold given `PartialEq` above
            // already treats them as the same key. Only once it doesn't
            // fit does it get its own tag.
            Value::BigInt(b) => match b.to_i64_exact() {
                Some(i) => {
                    2u8.hash(state);
                    i.hash(state);
                }
                None => {
                    23u8.hash(state);
                    b.0.hash(state);
                }
            },
            // S5: a `BigInteger` is `=` to the same-valued `Int`/`BigInt`
            // (see `eq_inner`'s INTEGER-category arms), so it MUST hash
            // through the identical two-case bridge -- no tag of its own,
            // or a `BigInteger` key would never find its `BigInt` twin in
            // a map.
            Value::BigInteger(b) => match b.to_i64_exact() {
                Some(i) => {
                    2u8.hash(state);
                    i.hash(state);
                }
                None => {
                    23u8.hash(state);
                    b.0.hash(state);
                }
            },
            // Own tag + the reduced (num, den) pair -- `RatioVal` derives
            // `Hash` already (the invariant `den > 0`, `gcd == 1` from
            // `RatioVal::reduce` is exactly what makes two `=` ratios also
            // structurally identical, so no canonicalization needed here
            // the way `BigDec` needs below).
            Value::Ratio(r) => {
                24u8.hash(state);
                r.hash(state);
            }
            // Must hash the CANONICAL (trailing-zero-stripped) form so
            // `(hash 1.5M) == (hash 1.50M)` (measured) agrees with
            // `PartialEq`'s scale-insensitive `BigDecVal::eq` above --
            // `BigDecVal`'s own `Hash` impl already canonicalizes.
            Value::BigDec(d) => {
                25u8.hash(state);
                d.hash(state);
            }
            // S3: a record's hash mixes its TypeDef identity with the
            // `Map`-style unordered pair hash of its full map view --
            // equal records hash equal (measured), and `(hash record) !=
            // (hash same-content-map)` (also measured) falls out of the
            // extra type mix-in. Deftypes/classes follow their
            // `PartialEq`: identity (Arc ptr) and name/identity
            // respectively.
            Value::Inst(inst) => {
                27u8.hash(state);
                if inst.tdef.is_record {
                    (Arc::as_ptr(&inst.tdef) as usize).hash(state);
                    inst.data.len().hash(state);
                    state.write_u64(hash_unordered_pairs(inst.data.iter()));
                } else {
                    (Arc::as_ptr(inst) as usize).hash(state);
                }
            }
            Value::Class(c) => {
                28u8.hash(state);
                match c.as_ref() {
                    crate::types::ClassVal::Builtin { name, .. } => name.hash(state),
                    crate::types::ClassVal::User(t) => (Arc::as_ptr(t) as usize).hash(state),
                    // S5: hash the name, matching `PartialEq`'s
                    // name-based interface equality directly above.
                    crate::types::ClassVal::Interface { name } => name.hash(state),
                }
            }
            // Own tag, `Arc` identity like every other cell-backed variant
            // (see `PartialEq` above).
            Value::Matcher(rc) => {
                29u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            // TIMER-CANCEL: `Arc` identity, matching `PartialEq`'s
            // `(Timer, Timer)` arm -- same tag-then-pointer shape as
            // every other identity-equal cell. Tag 35 (34 was the
            // highest in use when this landed).
            Value::Timer(rc) => {
                35u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            // SPEC-W6a: hashes its two `long` FIELDS, because
            // `PartialEq`'s `(TcRandom, TcRandom)` arm compares them --
            // the `a == b => hash(a) == hash(b)` contract, kept the same
            // way every structural variant above keeps it. Tag 36 (35
            // was the highest in use when this landed).
            Value::TcRandom(r) => {
                36u8.hash(state);
                r.hash(state);
            }
            // S4/1D: `Arc` pointer identity, matching `PartialEq`'s
            // `(Array, Array)` arm above -- same tag-then-pointer shape as
            // every other identity-equal variant (`Atom`'s `14u8`, `Var`'s
            // `19u8`, ...). Tag 30 (29 is `Matcher`'s, merged the same day).
            Value::Array(rc) => {
                30u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            // S4: MUST write the identical tag+stream `Map` (9) writes above
            // -- required for `(hash (sorted-map 1 :a)) == (hash {1 :a})`
            // (measured), same cross-type-hash-agrees-with-cross-type-`=`
            // contract `HostStruct` follows for the same tag.
            Value::SortedMap(m) => {
                9u8.hash(state);
                m.entries.len().hash(state);
                state.write_u64(hash_unordered_pairs(m.entries.iter().map(|(k, v)| (k, v))));
            }
            // S4: same idea, `Set`'s tag (10) -- `(hash (sorted-set 1 2)) ==
            // (hash #{1 2})` (measured).
            Value::SortedSet(s) => {
                10u8.hash(state);
                s.entries.len().hash(state);
                state.write_u64(hash_unordered_elems(s.entries.iter()));
            }
            // C2 (defstruct): MUST write the identical tag+stream `Map` (9)
            // writes above -- `(= (hash (struct s 1 2)) (hash {:a 1 :b
            // 2}))` (measured), same cross-type-hash-agrees-with-cross-
            // type-`=` contract `SortedMap`/`HostStruct` follow for the
            // same tag.
            Value::StructMap(m) => {
                9u8.hash(state);
                m.entries.len().hash(state);
                state.write_u64(hash_unordered_pairs(m.entries.iter().map(|(k, v)| (k, v))));
            }
            // C2: own tag (34), `Arc` identity -- matching `PartialEq`'s
            // `(StructBasis, StructBasis)` arm above.
            Value::StructBasis(rc) => {
                34u8.hash(state);
                (Arc::as_ptr(rc) as usize).hash(state);
            }
            // S4: MUST write the identical tag+stream `List`/`Vector` (8)
            // write above -- `(hash (vector-of :int 1 2 3)) == (hash [1 2
            // 3])` (measured), same reasoning as `SortedMap`/`SortedSet`.
            Value::TypedVec(v) => {
                8u8.hash(state);
                v.data.len().hash(state);
                for item in v.data.iter() {
                    item.hash(state);
                }
            }
            // C7 (vecveneer): same tag-8 stream as `List`/`Vector`/
            // `TypedVec` above -- `(.rseq [0 1 2])` is `=` to `[2 1 0]`
            // (measured), so it must hash identically, same cross-type
            // contract those arms follow.
            Value::VecSeq(vs) => {
                8u8.hash(state);
                vs.items.len().hash(state);
                for item in vs.items.iter() {
                    item.hash(state);
                }
            }
            // S5: own tag, `Arc` identity -- matching `PartialEq`'s
            // `(HostInst, HostInst)` arm above (tag 31; 29 is `Matcher`'s,
            // 30 is `Array`'s -- see those arms' own doc comments).
            //
            // SPEC-W6b: with ONE exception, `java.util.Date`, which
            // `PartialEq` compares by epoch millis because the real JVM's
            // `Date.equals` does. Hashing it by pointer there would break
            // `a == b => hash(a) == hash(b)` and two equal `#inst`s would
            // land in different buckets of a set/map. Sub-tag 0 keeps
            // every other `HostKind` on the pointer path unchanged.
            Value::HostInst(rc) => {
                31u8.hash(state);
                if rc.kind == crate::hostclass::HostKind::Date {
                    1u8.hash(state);
                    match &*crate::sync::lock_mutex(&rc.state) {
                        crate::hostclass::HostState::Date(ms) => ms.hash(state),
                        // Unreachable: a Date cell always holds
                        // `HostState::Date`. Hash the pointer rather than
                        // panicking in a `Hash` impl.
                        _ => (Arc::as_ptr(rc) as usize).hash(state),
                    }
                } else {
                    0u8.hash(state);
                    (Arc::as_ptr(rc) as usize).hash(state);
                }
            }
            // S6: own tags, hash the VALUE (not a pointer) -- matching
            // `PartialEq`'s value-equality arms directly above, so `(=
            // a b) => (hash a) == (hash b)` holds for two independently
            // constructed but equal UUIDs/URIs.
            Value::Uuid(u) => {
                32u8.hash(state);
                u.hash(state);
            }
            Value::Uri(s) => {
                33u8.hash(state);
                s.hash(state);
            }
        }
    }
}

/// Compile-time property assertion (A1 contract): `Value` and `Env` must be
/// `Send + Sync` -- a `future*`-spawned thread shares the same globals `Env`
/// and passes `Value`s across the thread boundary (captured closures, atoms
/// delivered into promises, etc). This function is never called; it exists
/// purely so `f::<Value>()`/`f::<Env>()` fail to *compile* (not just fail a
/// test) the moment something non-`Send`/`Sync` sneaks back in.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<Value>();
    f::<crate::env::Env>();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc_str(s: &str) -> Str {
        Str::from(s)
    }

    /// W-GEO stage 1: `Value::Keyword`'s payload is a [`Keyword`], not a
    /// `Str`. `rc_str`'s sibling for the keyword-building tests below, so
    /// those keep reading the way they did.
    fn kw_of(s: &str) -> Keyword {
        Keyword::construct(s)
    }

    /// SPEC-B-bignum-wiring.md §6: `size_of::<Value>()` was 72 bytes on the
    /// worktree HEAD this spec started from (commit `5eff7b7`), BEFORE
    /// `BigInt`/`Ratio`/`BigDec` existed. All three new variants are
    /// `Arc<...>`-boxed specifically so they cost only one more pointer-
    /// sized discriminant slot, same as every other boxed variant
    /// (`Fn`/`Atom`/`Regex`/...) -- never the size of the boxed payload
    /// itself. This test pins that invariant so a future change that
    /// accidentally unboxes one of them (or adds a fourth variant without
    /// boxing it) fails loudly here instead of silently bloating every
    /// `Value` in the interpreter.
    ///
    /// S5/M3 re-verified this after adding `Value::Meta(Arc<MetaObj>)`:
    /// still 72, for the same reason (the payload is behind an `Arc`, so
    /// the variant costs a pointer-sized slot the enum already had). That
    /// invariant is the whole argument for the wrapper-variant design
    /// over a `meta` field on every collection payload -- see
    /// `MetaObj`'s doc.
    ///
    /// W-GEO stage 4 (split-box) MOVED this number, 72 -> 32, and the pin
    /// moves with it. What changed: `PVec::Big`/`PMap::Big` now hold their
    /// big collections behind an `Arc` (`docs/W-GEO-STAGE4-SPLITBOX.md`),
    /// so the two widest payloads in the enum -- an inline
    /// `imbl::Vector<Value>` (64B) and an inline `champ::
    /// PersistentHashMap` (24B) -- became 16B and 16B. `Value`'s widest
    /// remaining payload is `Symbol` (32B: `Option<Str>` + `Str`, both
    /// 16B fat pointers), and `Symbol`'s own niches absorb the
    /// discriminant, so `Value` lands at 32 -- BETTER than the ~40 Probe
    /// G's arithmetic predicted (that estimate budgeted a separate 8B
    /// discriminant word; the niche made it free). This is a MEASURED
    /// number, not a target: 72 -> 32 is what the split-box change alone
    /// bought, with `Value`'s own arms (`List`/`Vector`/`Map`/`MapEntry`/
    /// `Queue` still holding `PVec`/`PMap` INLINE) untouched.
    ///
    /// Why the change was scoped this narrowly: Probe E measured that
    /// boxing `Value`'s five collection arms themselves (`MapEntry`
    /// included) costs +34-40% on map iteration -- KILLED. Probe G
    /// measured this narrower box, which leaves every `Small` path
    /// (including `PVec::pair`, every `MapEntry`'s sole constructor) at
    /// exactly one `Arc` as before, at +0.2%/-3.0% on that same
    /// workload -- PROCEED. Both verdicts:
    /// `docs/W-GEO-PROBE-VERDICTS.md` (branch `f5/geoprobe`).
    #[test]
    fn size_of_value_unchanged_by_bignum_variants() {
        assert_eq!(std::mem::size_of::<Value>(), 32);
    }

    /// W-GEO stage 4 (split-box): the two payload types the wave actually
    /// shrank, pinned at their own level so a future change that unboxes
    /// either `Big` arm fails HERE -- naming the type that regressed --
    /// rather than only in `Value`'s aggregate width above.
    ///
    /// 16 is the width of the `Small` arms' fat `Arc<[Value]>` /
    /// thin-`Arc`-plus-tag: `PVec::Small(Arc<[Value]>)` is a 16-byte fat
    /// pointer (`[Value]` is unsized) and `PVec::Big(Arc<imbl::Vector>)`
    /// is now an 8-byte thin one, so `Small` sets the width and the
    /// discriminant rides in a niche. `PMap` (`Arc<Vec<..>>` +
    /// `Arc<PersistentHashMap>`, both thin) is 8 + tag = 16.
    /// Pre-split-box: `PVec` 64, `PMap` 24.
    #[test]
    fn size_of_pvec_and_pmap_are_split_boxed() {
        // M6: a third arm (`Col`) costs the fat-pointer niche: 16 -> 24; `Value` stays 32 (asserted above).
        assert_eq!(std::mem::size_of::<PVec>(), 24, "PVec::Big must stay Arc-boxed");
        assert_eq!(std::mem::size_of::<PMap>(), 16, "PMap::Big must stay Arc-boxed");
    }

    /// S5/M3: metadata is invisible to `=` and to `hash`, in both
    /// directions and at every nesting depth (measured: `(= (with-meta
    /// [1 2] {:a 1}) (with-meta [1 2] {:b 2}))` and `(= {:x 1} (with-meta
    /// {:x 1} {:a 1}))` are both `true`, and `(= (hash (with-meta [1]
    /// {:a 1})) (hash [1]))` is `true`).
    ///
    /// The hash half is not merely a conformance nicety: Rust's own
    /// contract requires `a == b` to imply `hash(a) == hash(b)`, so a
    /// `Meta` that hashed with its own tag would corrupt every `HashMap`/
    /// `HashSet` keyed on a metadata-carrying value.
    #[test]
    fn meta_is_invisible_to_eq_and_hash() {
        let plain = Value::Vector(crate::pvec![Value::Int(1), Value::Int(2)]);
        let mut m1 = PMap::new();
        m1.insert(Value::Keyword("a".into()), Value::Int(1));
        let mut m2 = PMap::new();
        m2.insert(Value::Keyword("b".into()), Value::Int(2));
        let with_a = Value::attach_meta(plain.clone(), Value::Map(m1));
        let with_b = Value::attach_meta(plain.clone(), Value::Map(m2));

        assert_eq!(with_a, with_b);
        assert_eq!(with_a, plain);
        assert_eq!(plain, with_a);
        assert_eq!(hash_value(&with_a), hash_value(&plain));
        assert_eq!(hash_value(&with_a), hash_value(&with_b));

        // ... and nested one level down, where `=` re-enters per element.
        let nested_meta = Value::Vector(crate::pvec![with_a.clone()]);
        let nested_plain = Value::Vector(crate::pvec![plain.clone()]);
        assert_eq!(nested_meta, nested_plain);
        assert_eq!(hash_value(&nested_meta), hash_value(&nested_plain));
    }

    /// S5/M3: the two `Value::Meta` construction invariants from
    /// `MetaObj`'s doc. Both are measured behaviors, not conveniences:
    /// `nil` metadata STRIPS the wrapper (`(meta (with-meta (with-meta
    /// [1] {:a 1}) nil))` is `nil`) while an EMPTY map does NOT (`(meta
    /// (with-meta [] {}))` is `{}`), and re-attaching never nests.
    #[test]
    fn attach_meta_enforces_its_invariants() {
        let v = Value::Vector(crate::pvec![Value::Int(1)]);
        let mut m = PMap::new();
        m.insert(Value::Keyword("a".into()), Value::Int(1));

        // nil strips, entirely -- not "stores Nil".
        let wrapped = Value::attach_meta(v.clone(), Value::Map(m.clone()));
        assert!(wrapped.has_meta());
        let stripped = Value::attach_meta(wrapped.clone(), Value::Nil);
        assert!(!stripped.has_meta());
        assert_eq!(stripped.obj_meta(), Value::Nil);

        // An empty map is real metadata, distinct from having none.
        let empty_meta = Value::attach_meta(v.clone(), Value::Map(PMap::new()));
        assert!(empty_meta.has_meta());
        assert_eq!(empty_meta.obj_meta(), Value::Map(PMap::new()));

        // Re-attaching REPLACES; `inner` is never itself a `Meta`.
        let mut m2 = PMap::new();
        m2.insert(Value::Keyword("b".into()), Value::Int(2));
        let rewrapped = Value::attach_meta(wrapped, Value::Map(m2.clone()));
        assert_eq!(rewrapped.obj_meta(), Value::Map(m2));
        let Value::Meta(obj) = &rewrapped else {
            panic!("expected a Meta wrapper");
        };
        assert!(!obj.inner.has_meta(), "inner must never be a Meta");
        assert_eq!(rewrapped.unmeta(), &v);
    }

    #[test]
    fn float_equality_and_hash_uses_bits() {
        let a = Value::Float(1.0);
        let b = Value::Float(1.0);
        assert_eq!(a, b);
        let nan_a = Value::Float(f64::NAN);
        let nan_b = Value::Float(f64::NAN);
        // Documented deviation: NaN == NaN here, unlike IEEE 754.
        assert_eq!(nan_a, nan_b);
    }

    #[test]
    fn int_and_float_are_distinct_for_eq_and_hash() {
        assert_ne!(Value::Int(1), Value::Float(1.0));
    }

    #[test]
    fn list_and_vector_are_sequence_equal() {
        let list = Value::List(crate::pvec![Value::Int(1), Value::Int(2)]);
        let vector = Value::Vector(crate::pvec![Value::Int(1), Value::Int(2)]);
        assert_eq!(list, vector);

        let mut h1 = DefaultHasher::new();
        list.hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        vector.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn map_with_mixed_keys_equality_ignores_order() {
        let mut a = PMap::new();
        a.insert(Value::Keyword(kw_of("a")), Value::Int(1));
        a.insert(Value::Str(rc_str("b")), Value::Int(2));
        let mut b = PMap::new();
        b.insert(Value::Str(rc_str("b")), Value::Int(2));
        b.insert(Value::Keyword(kw_of("a")), Value::Int(1));
        assert_eq!(Value::Map(a), Value::Map(b));
    }

    /// Stage-2 CRITICAL invariant: a `Small` and a `Big` holding the same
    /// elements/pairs must be `==` and hash identically, regardless of
    /// which representation either side is in.
    #[test]
    fn pvec_small_and_big_are_representation_blind() {
        let elems: Vec<Value> = (0..20).map(Value::Int).collect(); // > PVEC_SMALL_MAX -> Big
        let big: PVec = elems.iter().cloned().collect();
        assert!(matches!(big, PVec::Big(_)));
        let small: PVec = elems[..10].iter().cloned().collect(); // <= PVEC_SMALL_MAX -> Small
        let mut big_prefix = PVec::new();
        for e in &elems[..10] {
            big_prefix.push_back(e.clone());
        }
        assert!(matches!(small, PVec::Small(_)));

        assert_eq!(small, big_prefix.clone()); // both Small here, sanity check
        // Force `big_prefix` down the exact same elements via `Big` by
        // growing then trimming is awkward with the one-way ratchet, so
        // instead compare a genuinely `Big` 20-elem vector against a
        // `Small` vector built from the same 20 elements one push at a
        // time would itself promote past 16 -- so compare `small` (10
        // elems, `Small`) against the `Big` vector `.take(10)` of it,
        // which stays `Big` (no demotion).
        let big_first_ten = big.take(10);
        assert!(matches!(big_first_ten, PVec::Big(_)));
        assert_eq!(small, big_first_ten, "Small and Big with equal elements must be ==");

        let mut h1 = DefaultHasher::new();
        Value::List(small.clone()).hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        Value::List(big_first_ten.clone()).hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish(), "Small and Big with equal elements must hash identically");

        // ptr_eq is representation-aware: never true across variants, even
        // when contents are equal (documented deviation, see `PVec::ptr_eq`).
        assert!(!small.ptr_eq(&big_first_ten));
    }

    #[test]
    fn pmap_small_and_big_are_representation_blind() {
        let mut small = PMap::new();
        for i in 0..5 {
            small.insert(Value::Int(i), Value::Int(i * 10));
        }
        assert!(matches!(small, PMap::Small(_)));

        let mut big = PMap::new();
        for i in 0..5 {
            big.insert(Value::Int(i), Value::Int(i * 10));
        }
        for i in 5..12 {
            big.insert(Value::Int(i), Value::Int(i * 10));
        }
        for i in 5..12 {
            big.remove(&Value::Int(i)); // dissoc back down to 5 entries -- must NOT demote
        }
        assert!(matches!(big, PMap::Big(_)), "dissoc on Big must never demote to Small");
        assert_eq!(big.len(), 5);

        assert_eq!(small, big, "Small and Big with equal pairs must be ==");
        let mut h1 = DefaultHasher::new();
        Value::Map(small.clone()).hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        Value::Map(big.clone()).hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish(), "Small and Big with equal pairs must hash identically");

        assert!(!small.ptr_eq(&big));
    }

    #[test]
    fn fns_and_atoms_compare_by_identity() {
        let atom_a = Value::Atom(Arc::new(AtomCell::new(Value::Int(1))));
        let atom_b = Value::Atom(Arc::new(AtomCell::new(Value::Int(1))));
        assert_ne!(atom_a, atom_b);
        let atom_c = atom_a.clone();
        assert_eq!(atom_a, atom_c);
    }

    /// M4b: `Value::Volatile` follows `Atom`'s own identity-not-content
    /// `=`/hash contract exactly (see this variant's doc in the enum
    /// above) -- two volatiles holding the same content are still distinct
    /// objects, and cloning the `Value` (an `Arc` bump) shares identity.
    #[test]
    fn volatiles_compare_by_identity() {
        let vol_a = Value::Volatile(Arc::new(RwLock::new(Value::Int(1))));
        let vol_b = Value::Volatile(Arc::new(RwLock::new(Value::Int(1))));
        assert_ne!(vol_a, vol_b, "same content, different cells: not =");
        let vol_c = vol_a.clone();
        assert_eq!(vol_a, vol_c, "same Arc: =");
        assert_eq!(hash_value(&vol_a), hash_value(&vol_c));
        assert_eq!(vol_a.type_name(), "volatile");
        // Never cross-`=` with `Atom`, same as every other pair of distinct
        // cell-backed variants (PartialEq's catch-all).
        let an_atom = Value::Atom(Arc::new(AtomCell::new(Value::Int(1))));
        assert_ne!(vol_a, an_atom);
    }

    fn hash_value(v: &Value) -> u64 {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    // --- SPEC-B-bignum-wiring.md §4: bignum =/hash bridging -------------

    fn hash_of(v: &Value) -> u64 {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    fn bigint(n: i64) -> Value {
        Value::BigInt(Arc::new(crate::bignum::BigIntVal::from_i64(n)))
    }

    fn ratio(num: i64, den: i64) -> Value {
        match crate::bignum::RatioVal::reduce(num.into(), den.into()) {
            Ok(crate::bignum::Reduced::Ratio(r)) => Value::Ratio(Arc::new(r)),
            other => panic!("expected a genuine Ratio from {num}/{den}, got {other:?}"),
        }
    }

    fn bigdec(unscaled: i64, scale: i32) -> Value {
        Value::BigDec(Arc::new(crate::bignum::BigDecVal::new(unscaled.into(), scale)))
    }

    /// `(= 7N 7)`/`(= 7 7N)` true, `(hash 7N) == (hash 7)` -- a `BigInt`
    /// that fits an `i64` must cross-`=` AND cross-hash with `Value::Int`
    /// in both directions, which is exactly what makes the map-lookup
    /// tests below work.
    #[test]
    fn bigint_int_bridge_both_directions() {
        assert_eq!(bigint(7), Value::Int(7));
        assert_eq!(Value::Int(7), bigint(7));
        assert_eq!(hash_of(&bigint(7)), hash_of(&Value::Int(7)));

        // A BigInt too large for i64 must NOT bridge to any Int.
        let huge: num_bigint::BigInt = "170141183460469231731687303715884105728".parse().unwrap();
        let huge_val = Value::BigInt(Arc::new(crate::bignum::BigIntVal(huge)));
        assert_ne!(huge_val, Value::Int(7));
    }

    /// `(= 7M 7)` false -- `BigDec` equals ONLY `BigDec`, never blends
    /// with `Int`/`Float` the way `BigInt` does.
    #[test]
    fn bigdec_does_not_bridge_to_int_or_float() {
        assert_ne!(bigdec(7, 0), Value::Int(7));
        assert_ne!(bigdec(70, 1), Value::Float(7.0));
    }

    /// `(= 1.5M 1.50M)` true and hashes identically -- scale-insensitive,
    /// per `BigDecVal`'s own canonicalizing `PartialEq`/`Hash`. `pr_str`
    /// still tells them apart (printer.rs, not this layer): that's the
    /// point of the "scale preserved on print, ignored on `=`" split.
    #[test]
    fn bigdec_equality_and_hash_are_scale_insensitive() {
        let a = bigdec(15, 1); // 1.5M
        let b = bigdec(150, 2); // 1.50M
        assert_eq!(a, b);
        assert_eq!(hash_of(&a), hash_of(&b));
        assert_ne!(crate::printer::pr_str(&a), crate::printer::pr_str(&b));
    }

    /// `(= 1/2 0.5)` false, `(= 1/2 1/2)` true -- `Ratio` equals ONLY
    /// `Ratio`, no numeric blending with `Float`.
    #[test]
    fn ratio_equality_is_exact_and_does_not_bridge_to_float() {
        assert_eq!(ratio(1, 2), ratio(1, 2));
        assert_ne!(ratio(1, 2), Value::Float(0.5));
    }

    /// `(= 7N 7.0)` must be false: unlike the Int/BigInt bridge, the S1
    /// Int/Float blend (`builtins::numbers`/`Interp::values_equal`) is
    /// NOT extended to any of the three new variants at this `Value`
    /// layer.
    #[test]
    fn bigint_does_not_bridge_to_float() {
        assert_ne!(bigint(7), Value::Float(7.0));
    }

    /// `({7N :a} 7) :a` and `({7 :a} 7N) :a` -- direct consequence of the
    /// BigInt/Int PartialEq+Hash bridge, exercised through a real `PMap`
    /// lookup (not just the raw `==`/`hash` calls above) both directions.
    #[test]
    fn map_lookup_bridges_bigint_and_int_both_directions() {
        let mut m1 = PMap::new();
        m1.insert(bigint(7), Value::Keyword(kw_of("a")));
        assert_eq!(m1.get(&Value::Int(7)), Some(&Value::Keyword(kw_of("a"))));

        let mut m2 = PMap::new();
        m2.insert(Value::Int(7), Value::Keyword(kw_of("a")));
        assert_eq!(m2.get(&bigint(7)), Some(&Value::Keyword(kw_of("a"))));
    }

    /// `({1.5M :a} 1.50M) :a` -- scale-insensitive `BigDec` map lookup.
    #[test]
    fn map_lookup_bridges_bigdec_scales() {
        let mut m = PMap::new();
        m.insert(bigdec(15, 1), Value::Keyword(kw_of("a"))); // 1.5M
        assert_eq!(m.get(&bigdec(150, 2)), Some(&Value::Keyword(kw_of("a")))); // 1.50M
    }

    /// `(contains? #{1/2} 1/2)` true -- `Ratio` set membership.
    #[test]
    fn set_contains_ratio_by_value() {
        let s: champ::PersistentHashSet<Value> = std::iter::once(ratio(1, 2)).collect();
        assert!(s.contains(&ratio(1, 2)));
    }

    #[test]
    fn truthy_rules() {
        assert!(!Value::Nil.truthy());
        assert!(!Value::Bool(false).truthy());
        assert!(Value::Bool(true).truthy());
        assert!(Value::Int(0).truthy());
        assert!(Value::Str(rc_str("")).truthy());
    }

    #[test]
    fn type_names() {
        assert_eq!(Value::Nil.type_name(), "nil");
        assert_eq!(Value::Int(1).type_name(), "int");
        assert_eq!(Value::Keyword(kw_of("k")).type_name(), "keyword");
    }

    // -------------------- M8: Str Flat/Rope dual representation --------------------
    // SPEC-M8-TEXT-INTEGRATION.md's representation-blind contract: Eq/Hash/
    // print must agree across variants for equal content; ptr_eq must NOT
    // (a Flat and a Rope handle are never the same allocation).

    const M8_SAMPLES: &[&str] = &["", "a", "hello, world", "the quick brown fox\njumps over\tthe lazy dog", "日本語のテキスト🎉multi-byte"];

    #[test]
    fn cross_variant_eq_is_representation_blind() {
        for &s in M8_SAMPLES {
            let flat = Str::force_flat(s);
            let rope = Str::force_rope(s);
            assert_eq!(flat, rope, "Flat({s:?}) should equal Rope({s:?})");
            assert_eq!(rope, flat, "Eq must be symmetric");
            assert_eq!(flat, Str::force_flat(s));
            assert_eq!(rope, Str::force_rope(s));
        }
        assert_ne!(Str::force_flat("a"), Str::force_rope("b"));
    }

    #[test]
    fn cross_variant_hash_is_representation_blind() {
        fn hash_of(s: &Str) -> u64 {
            let mut h = DefaultHasher::new();
            s.hash(&mut h);
            h.finish()
        }
        for &s in M8_SAMPLES {
            let flat = Str::force_flat(s);
            let rope = Str::force_rope(s);
            assert_eq!(hash_of(&flat), hash_of(&rope), "hash must agree for equal Flat/Rope content ({s:?})");
        }
    }

    #[test]
    fn cross_variant_hash_matches_plain_str_hash() {
        // Required for `Borrow<str>`'s `HashMap<Str,_>::get(&str)` contract
        // to hold for a Rope-backed key too, not just Flat.
        fn hash_str(s: &str) -> u64 {
            let mut h = DefaultHasher::new();
            s.hash(&mut h);
            h.finish()
        }
        fn hash_of(s: &Str) -> u64 {
            let mut h = DefaultHasher::new();
            s.hash(&mut h);
            h.finish()
        }
        for &s in M8_SAMPLES {
            assert_eq!(hash_of(&Str::force_rope(s)), hash_str(s), "Rope Str hash must match plain str hash for {s:?}");
        }
    }

    #[test]
    fn cross_variant_pr_str_is_representation_blind() {
        for &s in M8_SAMPLES {
            let flat_pr = crate::printer::pr_str(&Value::Str(Str::force_flat(s)));
            let rope_pr = crate::printer::pr_str(&Value::Str(Str::force_rope(s)));
            assert_eq!(flat_pr, rope_pr, "pr-str must match across representations for {s:?}");
            let flat_disp = crate::printer::display_str(&Value::Str(Str::force_flat(s)));
            let rope_disp = crate::printer::display_str(&Value::Str(Str::force_rope(s)));
            assert_eq!(flat_disp, rope_disp, "display_str must match across representations for {s:?}");
        }
    }

    #[test]
    fn ptr_eq_is_representation_aware_never_true_across_variants() {
        let s = "shared content, same bytes";
        let flat = Str::force_flat(s);
        let rope = Str::force_rope(s);
        assert!(Str::ptr_eq(&flat, &flat.clone()));
        assert!(Str::ptr_eq(&rope, &rope.clone()));
        assert!(!Str::ptr_eq(&flat, &rope), "Flat and Rope must never ptr_eq, even with identical content");
        assert!(!Str::ptr_eq(&rope, &flat));
    }

    #[test]
    fn rope_char_count_ascii_and_deref_all_agree_with_flat() {
        for &s in M8_SAMPLES {
            let flat = Str::force_flat(s);
            let rope = Str::force_rope(s);
            assert_eq!(flat.char_count_cached(), rope.char_count_cached(), "{s:?}");
            assert_eq!(flat.is_ascii_cached(), rope.is_ascii_cached(), "{s:?}");
            assert_eq!(flat.byte_len(), rope.byte_len(), "{s:?}");
            assert_eq!(&*flat, &*rope, "Deref materialization must reproduce the same content for {s:?}");
            assert_eq!(flat.is_blank(), rope.is_blank(), "{s:?}");
        }
    }

    #[test]
    fn rope_char_slice_matches_flat_subs_semantics() {
        let s = "the quick brown fox jumps over the lazy dog";
        let flat = Str::force_flat(s);
        let rope = Str::force_rope(s);
        let n = s.chars().count();
        for (start, end) in [(0, 3), (4, 9), (0, n), (10, 10), (n - 4, n)] {
            let expected: String = s.chars().skip(start).take(end - start).collect();
            assert_eq!(&*flat.char_slice(start..end), expected, "flat subs({start},{end})");
            assert_eq!(&*rope.char_slice(start..end), expected, "rope subs({start},{end})");
        }
    }

    #[test]
    fn rope_char_slice_ratchet_stays_rope_even_when_result_is_tiny() {
        // SPEC-M8-TEXT-INTEGRATION.md's ratchet: an op on an already-Rope
        // value stays Rope even if the result shrinks well below
        // STR_ROPE_MIN -- mirrors PVec/PMap's Small/Big policy.
        let big = "x".repeat(STR_ROPE_MIN * 2);
        let rope = Str::wrap_rope(champ::PText::from(big.as_str()));
        assert!(rope.is_rope());
        let tiny = rope.char_slice(0..1);
        assert!(tiny.is_rope(), "subs of a Rope source must stay Rope even when the slice is tiny");
        assert_eq!(&*tiny, "x");
    }

    #[test]
    fn from_str_promotes_to_rope_only_at_the_threshold() {
        let just_under = "a".repeat(STR_ROPE_MIN - 1);
        let at_threshold = "a".repeat(STR_ROPE_MIN);
        assert!(!Str::from(just_under.as_str()).is_rope());
        assert!(Str::from(at_threshold.as_str()).is_rope());
        assert_eq!(Str::from(at_threshold.as_str()).char_count_cached(), STR_ROPE_MIN);
    }

    #[test]
    fn rope_char_at_matches_flat_nth_semantics() {
        let s = "abc日本語xyz";
        let flat = Str::force_flat(s);
        let rope = Str::force_rope(s);
        for idx in 0..s.chars().count() + 1 {
            assert_eq!(flat.char_at(idx), rope.char_at(idx), "idx={idx}");
        }
        let oob = s.chars().count() + 5;
        assert_eq!(flat.char_at(oob), None);
        assert_eq!(rope.char_at(oob), None);
    }

    #[test]
    fn rope_find_and_rfind_char_match_str_semantics() {
        let s = "line one\nline two\nline three\n";
        let rope = Str::force_rope(s);
        // Every '\n' position, forward from 0 and from just past each hit.
        let newline_positions: Vec<usize> = s.char_indices().filter(|&(_, c)| c == '\n').map(|(b, _)| s[..b].chars().count()).collect();
        let mut from = 0usize;
        for &pos in &newline_positions {
            assert_eq!(rope.find_char_from('\n', from), Some(pos));
            from = pos + 1;
        }
        assert_eq!(rope.find_char_from('\n', from), None);

        // Backward from the end.
        let total = s.chars().count();
        let mut from = total;
        for &pos in newline_positions.iter().rev() {
            assert_eq!(rope.rfind_char_from('\n', from), Some(pos), "from={from}");
            from = pos.saturating_sub(1);
        }
    }

    // -------------------- M8.1: rope-native literal replace --------------------

    #[test]
    fn replace_literal_matches_at_chunk_seams() {
        // A short pattern recurring at a prime stride (97) over a large
        // enough document (~20,000 bytes, several times champ's
        // LEAF_MAX) statistically guarantees some occurrences straddle a
        // real leaf boundary -- this is the actual "matches at chunk
        // seams" case, exercised black-box (no access to champ's
        // internal leaf-split points from here).
        let mut s = String::new();
        while s.len() < 20_000 {
            s.push_str("the quick brown fox XY jumps over the lazy dog. ");
        }
        let rope = Str::force_rope(&s);
        let expected = s.replace("XY", "<MATCH>");
        let got = rope.replace_literal("XY", "<MATCH>", false);
        assert!(got.is_rope(), "Rope source must stay Rope (ratchet)");
        assert_eq!(&*got, expected);
        // Sanity: the stride guarantees at least a couple hundred matches,
        // so this isn't accidentally testing zero occurrences.
        assert!(s.matches("XY").count() > 100);
    }

    #[test]
    fn replace_literal_overlapping_adjacent_matches() {
        // "aa" repeated: every "aa" pair is adjacent to the next, so a
        // naive scanner could double-count or skip -- must match
        // str::replace's non-overlapping, left-to-right semantics exactly.
        let s = "aa".repeat(20_000); // 40,000 bytes
        let rope = Str::force_rope(&s);
        let expected = s.replace("aa", "b"); // "b" * 20,000
        let got = rope.replace_literal("aa", "b", false);
        assert!(got.is_rope());
        assert_eq!(&*got, expected);
        assert_eq!(got.char_count_cached(), 20_000);
    }

    #[test]
    fn replace_literal_empty_replacement_deletes_matches() {
        let mut s = String::new();
        while s.len() < 15_000 {
            s.push_str("abcxdefxghixjklx");
        }
        let rope = Str::force_rope(&s);
        let expected = s.replace('x', "");
        let got = rope.replace_literal("x", "", false);
        assert!(got.is_rope());
        assert_eq!(&*got, expected);
        assert!(!expected.contains('x'));
    }

    #[test]
    fn replace_literal_pattern_longer_than_a_chunk() {
        // Pattern well over champ's LEAF_MAX (2048, not exported --
        // duplicated here deliberately, same precedent champ's own
        // tests use) so it cannot possibly fit inside a single leaf/chunk.
        let pattern = "Z".repeat(5_000);
        let mut s = "prefix text before the long marker. ".repeat(50); // padding, several chunks
        s.push_str(&pattern);
        s.push_str(&"suffix text after the long marker. ".repeat(50));
        let rope = Str::force_rope(&s);
        let expected = s.replace(pattern.as_str(), "<LONG>");
        let got = rope.replace_literal(&pattern, "<LONG>", false);
        assert!(got.is_rope());
        assert_eq!(&*got, expected);
    }

    #[test]
    fn replace_literal_multibyte_utf8_pattern() {
        let mut s = String::new();
        while s.len() < 20_000 {
            s.push_str("hello 日本語テスト world émigré café naïve ");
        }
        let rope = Str::force_rope(&s);
        let expected = s.replace("日本語", "CJK");
        let got = rope.replace_literal("日本語", "CJK", false);
        assert!(got.is_rope());
        assert_eq!(&*got, expected);
        // A multibyte replacement too, for good measure.
        let expected2 = s.replace("café", "☕");
        let got2 = rope.replace_literal("café", "☕", false);
        assert_eq!(&*got2, expected2);
    }

    #[test]
    fn replace_literal_first_only_on_rope() {
        let mut s = String::new();
        while s.len() < 20_000 {
            s.push_str("marker text marker more marker ");
        }
        let rope = Str::force_rope(&s);
        let expected = {
            let idx = s.find("marker").unwrap();
            format!("{}{}{}", &s[..idx], "FIRST", &s[idx + "marker".len()..])
        };
        let got = rope.replace_literal("marker", "FIRST", true);
        assert!(got.is_rope());
        assert_eq!(&*got, expected);
        // Only one replacement happened -- every other "marker" survives.
        assert_eq!(got.as_contiguous().matches("marker").count(), s.matches("marker").count() - 1);
    }

    #[test]
    fn replace_literal_zero_matches_short_circuits_to_self() {
        let s = "x".repeat(70_000); // no "\r" anywhere -- normalize-plain's real shape
        let rope = Str::force_rope(&s);
        let got = rope.replace_literal("\r", "\n", false);
        assert!(Str::ptr_eq(&rope, &got), "zero matches should return self, not rebuild an identical tree");
        assert_eq!(&*got, s);
    }

    #[test]
    fn replace_literal_random_fuzz_matches_str_replace() {
        // Small deterministic xorshift PRNG (no external crate) generating
        // many random (haystack, pattern, replacement) triples over a
        // tiny alphabet -- deliberately small so patterns recur often and
        // land at many different offsets relative to leaf boundaries
        // across repeated runs. Every case is checked against `str::
        // replace` directly, both `first_only` on and off.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet = ["a", "b", "ab", "aba"];
        for case in 0..300 {
            let haystack_len = 1 + (next() % 4000) as usize;
            let haystack: String = (0..haystack_len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
            let pattern = alphabet[(next() % alphabet.len() as u64) as usize];
            let replacement = alphabet[(next() % alphabet.len() as u64) as usize];
            let first_only = next() % 2 == 0;
            let rope = Str::force_rope(&haystack);
            let expected = if first_only {
                match haystack.find(pattern) {
                    Some(idx) => format!("{}{}{}", &haystack[..idx], replacement, &haystack[idx + pattern.len()..]),
                    None => haystack.clone(),
                }
            } else {
                haystack.replace(pattern, replacement)
            };
            let got = rope.replace_literal(pattern, replacement, first_only);
            assert_eq!(
                &*got, expected,
                "case={case} haystack_len={haystack_len} pattern={pattern:?} replacement={replacement:?} first_only={first_only}"
            );
        }
    }

    #[test]
    fn replace_literal_flat_and_rope_agree() {
        let s = "one two three two one two".to_string();
        let flat = Str::force_flat(&s);
        let rope = Str::force_rope(&s);
        for (pat, repl, first) in [("two", "TWO", false), ("two", "TWO", true), ("one", "", false), ("zzz", "nope", false)] {
            let f = flat.replace_literal(pat, repl, first);
            let r = rope.replace_literal(pat, repl, first);
            assert_eq!(&*f, &*r, "pat={pat:?} repl={repl:?} first={first}");
        }
    }
}

// ---- heap-image gate-1 accessor (src/image.rs) ----
impl KeywordRegistry {
    pub(crate) fn img_all(&self) -> Vec<Str> {
        let mut v: Vec<Str> = crate::sync::lock_read(&self.0).iter().cloned().collect();
        v.sort_by(|a, b| (&**a).cmp(&**b));
        v
    }
}
impl Str {
    pub(crate) fn ptr_addr(&self) -> usize {
        self.identity_addr()
    }
    pub(crate) fn strong_count(&self) -> usize {
        match &self.0 {
            StrRepr::Flat(inner) => Arc::strong_count(inner),
            StrRepr::Rope(inner) => Arc::strong_count(inner),
        }
    }
}
