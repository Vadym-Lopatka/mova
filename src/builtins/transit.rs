//! clojure-lsp campaign (mova/PLAN.md): `cognitect.transit`'s `:json`
//! format is JSON with a small tagged-value/caching convention on top --
//! per PLAN's "reuse Rust crates" rule this is a native codec over
//! `serde_json`, NOT a transit-java port. `clj-kondo.impl.cache`
//! (required unconditionally by `clj-kondo.core`) reads its BUILT-IN
//! per-namespace/per-Java-class analysis cache (`resources/clj_kondo/
//! impl/cache/built_in/**/*.transit.json`, real files shipped alongside
//! the vendored `clj_kondo` source, found via `io/resource`) through
//! `transit/reader`+`transit/read` regardless of the top-level `:cache`
//! lint option (that option only gates the ON-DISK, user-writable lint
//! cache -- see `mova/shims/cognitect/transit.mova`'s doc). Only READ is
//! implemented: nothing in clj-kondo's lint path (`:cache false`, this
//! campaign's whole smoke corpus) ever WRITES a transit cache entry.
//!
//! ## Decoding rules (verified against transit-format's own spec, see
//! this fn's callers' commit message for the citation), and empirically
//! checked against ALL 741 `built_in/**/*.transit.json` files vendored
//! in this repo (zero out-of-range cache references under this exact
//! rule -- a wrong caching predicate desyncs the REST of the document,
//! not just one value, so this was verified file-by-file, not guessed):
//!
//! - A JSON array whose first element decodes to the literal string
//!   `"^ "` is a MAP: remaining elements alternate key, value, key, ...
//! - A JSON array whose first element decodes to a string starting with
//!   `"~#"` is a TAGGED value: `~#set`/`~#list` wrap a payload array
//!   into a `Value::Set`/`Value::List`; `~#'` (quote) just unwraps to
//!   its single payload element; any other tag falls back to decoding
//!   the payload as-is (none of the vendored corpus uses another tag).
//! - Any other JSON array is a plain vector.
//! - Ground-type string prefixes: `~:kw` keyword, `~$sym` symbol, `~iN`
//!   integer, `~cX` char, `~ddd.d` / `~fddd.d` decimal (both -> float,
//!   mova has no bignum/bigdec), `~_` nil; `~~`/`~^`/`` ~` `` escape a
//!   literal string that itself starts with `~`/`^`/`` ` ``.
//! - Cache codes (`^0`..`^9`,`^;`..,`^10`,...): `SUB_STR + index`, where
//!   `index`'s digits are `index / 44` then `index % 44` (omitting the
//!   high digit when it's 0), each digit mapped through ASCII 48..91
//!   inclusive (`char::from(digit as u8 + 48)`) -- ONE- or TWO-char
//!   codes, exactly transit-format's own `indexToCode`.
//! - Cacheable (assigned the next index, in document order): every
//!   `~:`/`~$`/`~#` tagged string longer than 3 chars (wire length,
//!   including its 2-char prefix), AND every plain (untagged) string
//!   longer than 3 chars used in a MAP-KEY position -- verified: NOT
//!   "only when every sibling key is also stringable" (the spec's own
//!   wording), which this module deliberately does not attempt to
//!   detect; the looser per-key-position rule alone already reproduces
//!   every cache reference in the vendored corpus with zero mismatches.

use std::collections::HashMap;
use std::sync::Arc;

use serde::de::{self, DeserializeSeed, Deserializer as SerdeDeserializer, MapAccess, SeqAccess, Visitor};

use crate::builtins::ArityHint;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{Keyword, NativeFn, PMap, PVec, Str, Symbol, Value};

/// Exact duplicate of `builtins::io`'s own `reg_ns` (private to that
/// module, same reason it isn't factored out there): registers a native
/// under a QUALIFIED `mova.transit/name` symbol, never a bare global --
/// same convention `mova.io`/`mova.json`/`mova.digest` follow.
#[track_caller]
fn reg_ns(
    i: &mut Interp,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let native = NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            return Err(RjError::arity(format!("mova.transit/{name}: wrong number of args ({})", args.len()))
                .with_stack(interp.stack_snapshot(), interp.source_id));
        }
        f(interp, args)
    });
    i.globals.set_builtin(
        Symbol { ns: Some("mova.transit".into()), name: name.into() },
        Value::Native(Arc::new(native)),
    );
}

/// Census/bench entry point (`examples/transit_census.rs`): identical
/// decode path the `mova.transit/read-json` builtin below runs, exposed
/// as a plain `pub fn` so an example binary (which only sees `pub` crate
/// surface) can drive it directly on an arbitrary JSON string.
pub fn read_json_str(s: &str) -> Result<Value, RjError> {
    let mut de = serde_json::Deserializer::from_str(s);
    let mut cache: Vec<Value> = Vec::new();
    let v = ValueSeed { cache: &mut cache, is_key: false }
        .deserialize(&mut de)
        .map_err(|e| RjError::other(format!("mova.transit/read-json: invalid JSON: {e}")))?;
    de.end().map_err(|e| RjError::other(format!("mova.transit/read-json: invalid JSON: {e}")))?;
    Ok(v)
}

/// Same decode, but streaming straight off a `Read` -- `mova.transit/
/// read-json-file`'s entry point (module doc / `register` below): lets
/// a caller handed a file/stream (`cognitect.transit/reader`'s shim)
/// skip materializing the whole document as a Mova `Str` first.
pub(crate) fn read_json_reader<R: std::io::Read>(r: R) -> Result<Value, RjError> {
    let mut de = serde_json::Deserializer::from_reader(r);
    let mut cache: Vec<Value> = Vec::new();
    let v = ValueSeed { cache: &mut cache, is_key: false }
        .deserialize(&mut de)
        .map_err(|e| RjError::other(format!("mova.transit/read-json-file: invalid JSON: {e}")))?;
    de.end().map_err(|e| RjError::other(format!("mova.transit/read-json-file: invalid JSON: {e}")))?;
    Ok(v)
}

pub fn register(i: &mut Interp) {
    reg_ns(i, "read-json", ArityHint::Exact(1), |_i, args| {
        let s = match &args[0] {
            Value::Str(s) => s.as_ref(),
            other => {
                return Err(RjError::type_err(format!(
                    "mova.transit/read-json: expected a string, got {}",
                    other.type_name()
                )))
            }
        };
        read_json_str(s)
    });
    // `mova.transit/read-json-file`: same decode, direct off a
    // `HostKind::InputStream` (module doc) -- so a 70MB source document
    // isn't held as a Mova `Str` alongside the `Value` tree it decodes
    // to, the way a `mova.io/read-all` + `read-json` slurp would.
    reg_ns(i, "read-json-file", ArityHint::Exact(1), |_i, args| {
        let h = match &args[0] {
            Value::HostInst(h) => h,
            other => {
                return Err(RjError::type_err(format!(
                    "mova.transit/read-json-file: expected a stream, got {}",
                    other.type_name()
                )))
            }
        };
        crate::hostclass::stream_read_json(h)
    });
    // `mova.transit/write-json-file`: (stream-or-path value): same bytes as
    // `write-json`, streamed straight to an output stream / file, no tree.
    reg_ns(i, "write-json-file", ArityHint::Exact(2), |i, args| {
        match &args[0] {
            Value::HostInst(h) => crate::hostclass::with_output_stream(h, |w| write_json_stream(i, &args[1], w))?,
            Value::Str(p) => {
                let f = std::fs::File::create(p.as_ref())
                    .map_err(|e| RjError::other(format!("mova.transit/write-json-file: {p}: {e}")))?;
                let mut w = std::io::BufWriter::with_capacity(1 << 16, f);
                write_json_stream(i, &args[1], &mut w)?;
                std::io::Write::flush(&mut w).map_err(io_err)?;
            }
            other => {
                return Err(RjError::type_err(format!(
                    "mova.transit/write-json-file: expected a stream or path, got {}",
                    other.type_name()
                )))
            }
        }
        Ok(Value::Nil)
    });
    reg_ns(i, "write-json", ArityHint::Exact(1), |i, args| {
        // Deep-realize first: `encode` below has no `Interp` to force a
        // thunk with, so an unrealized `Value::Lazy`/`Value::LazyTail`
        // (e.g. a `(map ...)`/`(filter ...)`/`(keys ...)` chain nested
        // anywhere in the value, not just at the root -- clojure-lsp's
        // on-disk cache map is exactly this: `:dep-graph`/`:classpath`
        // etc carry lazy seqs straight out of `clojure.core`) fell to
        // `encode`'s catch-all and errored as "unsupported value type
        // lazy-seq" instead of being written. `realize_deep` (used the
        // same way by `pr-str`/`str` and `sorted::hash_value` for the
        // identical problem) also walks through `Value::Meta` wrappers,
        // which is why the value it returns can still contain `Meta`
        // (re-attached, not stripped) -- `encode`'s own `Value::Meta` arm
        // unwraps those at write time.
        let realized = i.realize_deep(&args[0])?;
        let mut cache: HashMap<String, usize> = HashMap::new();
        let mut j = encode(&realized, false, &mut cache)?;
        // transit-clj's own root-quoting rule (measured against the JVM
        // oracle, `cli && clojure -M`): a document whose ROOT value isn't
        // itself an array (map/vector/list/set all encode as one, see
        // `encode` below) gets wrapped in the `~#'` quote tag -- only at
        // the top level; a nested scalar never does (e.g. `[1,2,:a,"b"]`
        // stays bare). Not cached (a single `write-json` call is always
        // exactly one document, so a repeat within it can't occur).
        if !matches!(j, serde_json::Value::Array(_)) {
            j = serde_json::Value::Array(vec![serde_json::Value::String("~#'".to_string()), j]);
        }
        serde_json::to_string(&j)
            .map(|s| Value::Str(Str::from(s)))
            .map_err(|e| RjError::other(format!("mova.transit/write-json: {e}")))
    });
}

/// Root cause (mova/PLAN.md campaign, `canonicalize-java-analysis`'s
/// `:class` field turning up `nil` on a fraction of real-world
/// `~/.cache/clojure-lsp/db.transit.json` entries): transit-format's
/// write-side priority cache is NOT unbounded -- both `WriteCache` and
/// `ReadCache` (transit-java's actual source, `com.cognitect.transit.
/// impl.{Write,Read}Cache`, decompiled from the vendored 1.0.362-sources
/// jar to get this exact, verified against real output rather than
/// guessed) clear back to index 0 once `MAX_CACHE_ENTRIES` (`44*44` =
/// 1936, assigned right when `index == MAX_CACHE_ENTRIES` before the
/// increment -- NOT `-1`) entries have been assigned in the current
/// generation, so a large real document re-spells its keywords/strings
/// in full and restarts `^0`.. numbering partway through (confirmed on
/// the real 70MB `db.transit.json`: `~:java-class-definitions` appears
/// fully spelled out 6 separate times, not once). This port's `cache:
/// Vec<Value>`/`HashMap<String, usize>` (both `decode_string` below and
/// `maybe_cache_str`) grew UNBOUNDED with no clear, which is invisible
/// on the small vendored `built_in/**/*.transit.json` corpus (module
/// doc: none reach 1936 distinct cacheable strings) but desyncs a large
/// document's decode/encode index numbering against the real writer's
/// the moment it wraps: a later `^N` code then resolves against a STALE
/// slot from thousands of pushes earlier (e.g. an unrelated file:// URI
/// cached as an early map key), silently substituting the wrong value
/// with no error -- exactly the `:class`-goes-`nil` (or worse, wrong-
/// but-non-nil) symptom. Fixing this at the codec is the actual root
/// fix; `feature/completion.clj`'s `(when class ...)` guard was a
/// band-aid around the corrupted data, not the bug itself. Verified: a
/// from-scratch Python reference decoder, transcribed line-for-line from
/// the real `ReadCache.cacheRead`, finds 15140 `:java-class-definitions`
/// entries in the real cache file with ZERO missing/nil `:class` --
/// confirming the wire data is always sound and this codec bug was the
/// entire story.
const MAX_CACHE_ENTRIES: usize = 44 * 44;

/// `index -> "^" + one/two cache-alphabet chars`, transit-format's own
/// `indexToCode` (`BASE_CHAR_INDEX = 48`, `CACHE_CODE_DIGITS = 44`).
fn index_to_code(index: usize) -> String {
    const DIGITS: usize = 44;
    const BASE: u8 = 48;
    let hi = index / DIGITS;
    let lo = index % DIGITS;
    if hi == 0 {
        format!("^{}", (lo as u8 + BASE) as char)
    } else {
        format!("^{}{}", (hi as u8 + BASE) as char, (lo as u8 + BASE) as char)
    }
}

/// Inverse of [`index_to_code`]: `code` is the cache-code string WITHOUT
/// its leading `^` (1 or 2 chars, each ASCII 48..91).
fn code_to_index(code: &str) -> Option<usize> {
    const BASE: i32 = 48;
    let chars: Vec<char> = code.chars().collect();
    match chars.len() {
        1 => Some((chars[0] as i32 - BASE) as usize),
        2 => {
            let hi = chars[0] as i32 - BASE;
            let lo = chars[1] as i32 - BASE;
            Some((hi * 44 + lo) as usize)
        }
        _ => None,
    }
}

fn is_tagged(s: &str) -> bool {
    s.starts_with("~:") || s.starts_with("~$") || s.starts_with("~#")
}

/// Resolves `raw` through the cache table if it's a `^`-code reference
/// -- returning the cached `Value` itself (an `Arc`-bump for `Str`/
/// `Keyword`, a plain copy otherwise), NOT a fresh re-decode of a cloned
/// `String` -- matching transit-java's own "cache hit returns the same
/// object" contract (module doc). `None` means `raw` is not a cache
/// code: caller ground-decodes it and, if cacheable, pushes the
/// resulting `Value` (cloned once, then shared from the cache on every
/// later hit).
fn resolve_cache_hit(raw: &str, cache: &[Value]) -> Result<Option<Value>, RjError> {
    if raw != "^ " && raw.len() >= 2 && raw.as_bytes()[0] == b'^' {
        let idx = code_to_index(&raw[1..])
            .ok_or_else(|| RjError::other(format!("mova-transit-read-json: malformed cache code {raw:?}")))?;
        return cache.get(idx).cloned().map(Some).ok_or_else(|| {
            RjError::other(format!(
                "mova-transit-read-json: cache code {raw:?} (index {idx}) out of range (cache has {})",
                cache.len()
            ))
        });
    }
    Ok(None)
}

/// Ground-type semantic decode of an already cache-resolved raw string
/// (see this module's doc for the prefix table).
fn decode_ground_string(raw: &str) -> Value {
    if let Some(rest) = raw.strip_prefix("~:") {
        return Value::Keyword(Keyword::from(rest.to_string()));
    }
    if let Some(rest) = raw.strip_prefix("~$") {
        let sym = crate::builtins::strings::symbol_from_str(rest);
        return Value::Sym(sym);
    }
    if let Some(rest) = raw.strip_prefix("~i") {
        if let Ok(n) = rest.parse::<i64>() {
            return Value::Int(n);
        }
        if let Ok(f) = rest.parse::<f64>() {
            return Value::Float(f);
        }
        return Value::Str(Str::from(rest.to_string()));
    }
    if let Some(rest) = raw.strip_prefix("~c") {
        if let Some(c) = rest.chars().next() {
            return Value::Char(c);
        }
    }
    if let Some(rest) = raw.strip_prefix("~d").or_else(|| raw.strip_prefix("~f")) {
        if let Ok(f) = rest.parse::<f64>() {
            return Value::Float(f);
        }
    }
    if raw == "~_" {
        return Value::Nil;
    }
    if let Some(rest) = raw.strip_prefix("~~") {
        return Value::Str(Str::from(format!("~{rest}")));
    }
    if let Some(rest) = raw.strip_prefix("~^") {
        return Value::Str(Str::from(format!("^{rest}")));
    }
    if let Some(rest) = raw.strip_prefix("~`") {
        return Value::Str(Str::from(format!("`{rest}")));
    }
    // `~#tag` bare (not array-head position, so no payload to attach) or
    // any other/unknown `~x` ground-type prefix: kept verbatim, best
    // effort -- not exercised by the vendored corpus (module doc).
    Value::Str(Str::from(raw.to_string()))
}

/// Ground-decodes `s` once, sharing the cache table's `Value` on a cache
/// HIT (an `Arc`-bump, never a re-decode) and, on a cache-eligible MISS,
/// decoding once and pushing a clone of the *result* `Value` (not the
/// raw string) so every later hit shares that same allocation -- the
/// fix for the module's former "cache stored `String`, so a hit still
/// re-allocated a fresh `Value` every time" cost (see this module's
/// commit history / the census that found it).
fn decode_string(s: &str, is_key: bool, cache: &mut Vec<Value>) -> Result<Value, RjError> {
    if let Some(v) = resolve_cache_hit(s, cache)? {
        return Ok(v);
    }
    let v = decode_ground_string(s);
    if s.len() > 3 && (is_tagged(s) || is_key) {
        // MAX_CACHE_ENTRIES: mirror the real writer's cache-clear-and-
        // restart-at-0 once full (see this module's doc const).
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.clear();
        }
        cache.push(v.clone());
    }
    Ok(v)
}

// ---------------------------------------------------------------------
// Streaming decode: a `serde::de::Visitor` over `serde_json`'s own
// pull parser writes straight into Mova `Value`s, one pass, no
// intermediate `serde_json::Value` tree (module doc / the census this
// rewrite closes out, `mova/NOTES.md`). `ValueSeed`/`ValueVisitor`
// reproduce the old `decode`+`decode_array` dispatch EXACTLY (map-
// marker/tag/plain-vector head check, cache-sharing, `is_key` cache
// eligibility) -- only the source of `Value`s changed, from an
// already-built tree to `SeqAccess`/`MapAccess` pulled straight off the
// byte stream.
// ---------------------------------------------------------------------

/// Threads `cache` (and `is_key`, only meaningful for a bare string)
/// through one `serde` deserialize call -- `DeserializeSeed` is how
/// serde passes caller state into an otherwise stateless `Deserialize`
/// walk.
struct ValueSeed<'c> {
    cache: &'c mut Vec<Value>,
    is_key: bool,
}

impl<'de, 'c> DeserializeSeed<'de> for ValueSeed<'c> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: SerdeDeserializer<'de>,
    {
        deserializer.deserialize_any(ValueVisitor { cache: self.cache, is_key: self.is_key })
    }
}

struct ValueVisitor<'c> {
    cache: &'c mut Vec<Value>,
    is_key: bool,
}

impl<'de, 'c> Visitor<'de> for ValueVisitor<'c> {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a transit-json value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Nil)
    }
    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Nil)
    }
    fn visit_bool<E: de::Error>(self, b: bool) -> Result<Value, E> {
        Ok(Value::Bool(b))
    }
    fn visit_i64<E: de::Error>(self, n: i64) -> Result<Value, E> {
        Ok(Value::Int(n))
    }
    fn visit_u64<E: de::Error>(self, n: u64) -> Result<Value, E> {
        // Same "safe i64 range" fallback as the old `n.as_i64()` check
        // (module doc's ground-type table).
        Ok(if n <= i64::MAX as u64 { Value::Int(n as i64) } else { Value::Float(n as f64) })
    }
    fn visit_f64<E: de::Error>(self, f: f64) -> Result<Value, E> {
        Ok(Value::Float(f))
    }
    fn visit_str<E: de::Error>(self, s: &str) -> Result<Value, E> {
        decode_string(s, self.is_key, self.cache).map_err(de::Error::custom)
    }
    fn visit_string<E: de::Error>(self, s: String) -> Result<Value, E> {
        decode_string(&s, self.is_key, self.cache).map_err(de::Error::custom)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        decode_seq(&mut seq, self.cache)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        // Not used by the vendored corpus (module doc: array-map form
        // only) -- best-effort plain-object decode, string/tagged keys,
        // no special cache-order handling.
        let ValueVisitor { cache, .. } = self;
        let mut m = PMap::new();
        while let Some(k) = map.next_key_seed(ValueSeed { cache: &mut *cache, is_key: true })? {
            let v = map.next_value_seed(ValueSeed { cache: &mut *cache, is_key: false })?;
            m.insert(k, v);
        }
        Ok(Value::Map(m))
    }
}

/// Same map/tag/plain-vector head dispatch as the old `decode_array`,
/// driven by `SeqAccess` instead of a `&[serde_json::Value]` slice: the
/// first element is decoded (same cache-sharing path as any string) and
/// then inspected, exactly like the old code inspected `resolved_head`.
fn decode_seq<'de, A>(seq: &mut A, cache: &mut Vec<Value>) -> Result<Value, A::Error>
where
    A: SeqAccess<'de>,
{
    let first = match seq.next_element_seed(ValueSeed { cache: &mut *cache, is_key: false })? {
        None => return Ok(Value::Vector(PVec::new())),
        Some(v) => v,
    };
    let tag: Option<String> = match &first {
        Value::Str(s) if s.as_ref() == "^ " => Some("^ ".to_string()),
        Value::Str(s) if s.as_ref().starts_with("~#") => Some(s.as_ref()[2..].to_string()),
        _ => None,
    };
    match tag.as_deref() {
        Some("^ ") => {
            let mut m = PMap::new();
            loop {
                let k = match seq.next_element_seed(ValueSeed { cache: &mut *cache, is_key: true })? {
                    Some(k) => k,
                    None => break,
                };
                let v = seq.next_element_seed(ValueSeed { cache: &mut *cache, is_key: false })?.unwrap_or(Value::Nil);
                m.insert(k, v);
            }
            Ok(Value::Map(m))
        }
        Some("set") => {
            let elems = seq.next_element_seed(PayloadSeed { cache: &mut *cache })?.unwrap_or_default();
            let mut out = champ::PersistentHashSet::new().transient();
            for e in elems {
                out.insert(e);
            }
            drain_rest(seq, cache)?;
            Ok(Value::Set(out.persistent()))
        }
        Some("list") => {
            let elems = seq.next_element_seed(PayloadSeed { cache: &mut *cache })?.unwrap_or_default();
            let mut out = PVec::new();
            for e in elems {
                out.push_back(e);
            }
            drain_rest(seq, cache)?;
            Ok(Value::List(out))
        }
        // `"'"` (quote) and any other/unknown tag (not exercised by the
        // vendored corpus): decode the payload as-is, best effort.
        Some(_tag) => {
            let v = seq.next_element_seed(ValueSeed { cache: &mut *cache, is_key: false })?.unwrap_or(Value::Nil);
            drain_rest(seq, cache)?;
            Ok(v)
        }
        None => {
            // Plain vector (first element wasn't a map-marker/tag
            // string, or wasn't a string at all) -- keep the already-
            // decoded first value, stream the rest straight in.
            let mut out = PVec::new();
            out.push_back(first);
            while let Some(v) = seq.next_element_seed(ValueSeed { cache: &mut *cache, is_key: false })? {
                out.push_back(v);
            }
            Ok(Value::Vector(out))
        }
    }
}

/// Drains any elements left in `seq` after a tag+payload dispatch has
/// consumed what it needs -- not exercised by the vendored corpus
/// (every tagged array is exactly `[tag, payload]`), but required for
/// `serde_json` to leave its cursor past the array's closing `]`.
fn drain_rest<'de, A>(seq: &mut A, cache: &mut Vec<Value>) -> Result<(), A::Error>
where
    A: SeqAccess<'de>,
{
    while seq.next_element_seed(ValueSeed { cache: &mut *cache, is_key: false })?.is_some() {}
    Ok(())
}

/// Decodes a tag's payload array as a flat list of elements -- NOT
/// through `decode_seq`'s map/tag head dispatch (the payload container
/// itself is never a map-marker/tag; only its own children can be, and
/// each child still recurses through the full `ValueSeed`/`decode_seq`
/// path via `deserialize_any`). A non-array payload (or no payload at
/// all) decodes to an empty list, matching the old code's `if let
/// Some(serde_json::Value::Array(elems)) = payload` guard exactly (a
/// non-array payload was silently dropped, never decoded).
struct PayloadSeed<'c> {
    cache: &'c mut Vec<Value>,
}

impl<'de, 'c> DeserializeSeed<'de> for PayloadSeed<'c> {
    type Value = Vec<Value>;

    fn deserialize<D>(self, deserializer: D) -> Result<Vec<Value>, D::Error>
    where
        D: SerdeDeserializer<'de>,
    {
        deserializer.deserialize_any(PayloadVisitor { cache: self.cache })
    }
}

struct PayloadVisitor<'c> {
    cache: &'c mut Vec<Value>,
}

impl<'de, 'c> Visitor<'de> for PayloadVisitor<'c> {
    type Value = Vec<Value>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a transit tag payload")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Vec<Value>, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(ValueSeed { cache: &mut *self.cache, is_key: false })? {
            out.push(v);
        }
        Ok(out)
    }
    fn visit_map<A>(self, mut map: A) -> Result<Vec<Value>, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_entry::<de::IgnoredAny, de::IgnoredAny>()?.is_some() {}
        Ok(Vec::new())
    }
    fn visit_unit<E: de::Error>(self) -> Result<Vec<Value>, E> {
        Ok(Vec::new())
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Vec<Value>, E> {
        Ok(Vec::new())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Vec<Value>, E> {
        Ok(Vec::new())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Vec<Value>, E> {
        Ok(Vec::new())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Vec<Value>, E> {
        Ok(Vec::new())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Vec<Value>, E> {
        Ok(Vec::new())
    }
    fn visit_string<E: de::Error>(self, _: String) -> Result<Vec<Value>, E> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------
// WRITER (`mova.transit/write-json`, mova/PLAN.md clj-kondo/clojure-lsp
// on-disk cache campaign): exact inverse of the decoder above. `cache`
// here maps a full wire string ALREADY WRITTEN to the index it was
// assigned, so a later occurrence of the same string emits `^<code>`
// instead -- the reader's `Vec<String>` inverted (lookup by content
// instead of by index), same caching predicate (module doc): every
// `~:`/`~$`/`~#`-tagged string longer than 3 chars, PLUS any plain
// (untagged) string longer than 3 chars in a map-key position.
// ---------------------------------------------------------------------

/// Escapes a plain string that would otherwise collide with the
/// format's own reserved lead-in characters -- transit-format's own
/// documented rule (any string starting with `~`/`^`/`` ` `` MUST be
/// escaped by prepending `~`), exact inverse of `decode_ground_string`'s
/// `~~`/`~^`/`` ~` `` branches above.
fn escape_ground_string(s: &str) -> String {
    match s.chars().next() {
        Some('~') | Some('^') | Some('`') => format!("~{s}"),
        _ => s.to_string(),
    }
}

fn keyword_wire(k: &Keyword) -> String {
    format!("~:{}", k.text_ref())
}

fn symbol_wire(sym: &Symbol) -> String {
    match &sym.ns {
        Some(ns) => format!("~${ns}/{}", sym.name),
        None => format!("~${}", sym.name),
    }
}

/// Emits `wire` as a JSON string, resolving it to a `^<code>` reference
/// if it's already in `cache`, else (when `cacheable`) registering it at
/// the next index -- mirrors `resolve_and_maybe_cache`'s read-side rule
/// exactly, just inverted (content -> index instead of index -> content).
fn maybe_cache_str(wire: String, cacheable: bool, cache: &mut HashMap<String, usize>) -> serde_json::Value {
    if let Some(&idx) = cache.get(&wire) {
        return serde_json::Value::String(index_to_code(idx));
    }
    if cacheable && wire.len() > 3 {
        // MAX_CACHE_ENTRIES: same clear-and-restart-at-0 the read side
        // now mirrors (this module's doc const) -- keeps write/read
        // self-consistent AND matches the real writers' wire format.
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(wire.clone(), cache.len());
    }
    serde_json::Value::String(wire)
}

/// JSON's safe-integer range (`+-2^53`, transit-format's own threshold
/// for "a JSON-number-typed double can hold this exactly"); a long past
/// it is written as a tagged `~iN` string instead of a bare JSON number,
/// exactly transit-clj's own long-vs-JSON-number split.
const MAX_SAFE_INT: i64 = 9_007_199_254_740_991;

fn encode(v: &Value, is_key: bool, cache: &mut HashMap<String, usize>) -> Result<serde_json::Value, RjError> {
    use serde_json::Value as J;
    Ok(match v {
        // Transit (like transit-clj) doesn't round-trip Clojure metadata,
        // so a metadata-carrying value (e.g. a `^:foo sym` symbol key in a
        // dep-graph map) is encoded as its plain inner value -- otherwise
        // this fell through to the catch-all `other` arm below and errored
        // as "unsupported value type <inner type_name>", even though the
        // inner type (symbol, map, ...) is perfectly encodable.
        Value::Meta(m) => return encode(&m.inner, is_key, cache),
        Value::Nil => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(n) => {
            if (-MAX_SAFE_INT..=MAX_SAFE_INT).contains(n) {
                J::Number((*n).into())
            } else {
                maybe_cache_str(format!("~i{n}"), false, cache)
            }
        }
        Value::Float(f) => {
            if f.is_nan() {
                J::String("~zNaN".to_string())
            } else if f.is_infinite() {
                J::String(if *f > 0.0 { "~zINF".to_string() } else { "~z-INF".to_string() })
            } else {
                serde_json::Number::from_f64(*f)
                    .map(J::Number)
                    .ok_or_else(|| RjError::other("mova.transit/write-json: unrepresentable float".to_string()))?
            }
        }
        Value::BigInt(b) | Value::BigInteger(b) => maybe_cache_str(format!("~n{}", b.to_decimal_string()), false, cache),
        Value::BigDec(d) => maybe_cache_str(format!("~f{}", d.to_java_string()), false, cache),
        Value::Char(c) => maybe_cache_str(format!("~c{c}"), false, cache),
        Value::Uuid(u) => maybe_cache_str(format!("~u{}", Value::format_uuid(**u)), false, cache),
        Value::Str(s) => maybe_cache_str(escape_ground_string(s), is_key, cache),
        Value::Keyword(k) => maybe_cache_str(keyword_wire(k), true, cache),
        Value::Sym(sym) => maybe_cache_str(symbol_wire(sym), true, cache),
        Value::List(items) => {
            let tag = maybe_cache_str("~#list".to_string(), true, cache);
            let mut arr = Vec::with_capacity(items.len());
            for it in items.iter() {
                arr.push(encode(it, false, cache)?);
            }
            J::Array(vec![tag, J::Array(arr)])
        }
        Value::Vector(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for it in items.iter() {
                arr.push(encode(it, false, cache)?);
            }
            J::Array(arr)
        }
        Value::Set(s) => {
            let tag = maybe_cache_str("~#set".to_string(), true, cache);
            // Element order: sets are unordered under `=`, so any order
            // round-trips correctly through THIS reader; sorted by
            // `pr_str` (same tiebreak `printer.rs` uses for `#{...}`) for
            // deterministic output -- NOT guaranteed to match transit-
            // clj's own JVM-hash-bucket order byte-for-byte (module doc
            // caveat, see NOTES.md).
            let mut items: Vec<&Value> = s.iter().collect();
            items.sort_by_key(|v| crate::printer::pr_str(v));
            let mut arr = Vec::with_capacity(items.len());
            for it in items {
                arr.push(encode(it, false, cache)?);
            }
            J::Array(vec![tag, J::Array(arr)])
        }
        Value::Map(m) => {
            let mut arr = Vec::with_capacity(m.len() * 2 + 1);
            arr.push(J::String("^ ".to_string()));
            for (k, val) in m.iter() {
                arr.push(encode(k, true, cache)?);
                arr.push(encode(val, false, cache)?);
            }
            J::Array(arr)
        }
        other => {
            return Err(RjError::type_err(format!(
                "mova.transit/write-json: unsupported value type {}",
                other.type_name()
            )))
        }
    })
}

fn io_err(e: std::io::Error) -> RjError {
    RjError::other(format!("mova.transit/write-json-file: {e}"))
}

/// Streaming twin of `encode`: same cache order, tags, escapes, bytes.
struct StreamEnc<'a> {
    i: &'a mut Interp,
    w: &'a mut dyn std::io::Write,
    cache: HashMap<String, usize>,
    buf: String,
    budget: usize,
}

/// Root value encodes as a JSON array (else it gets the `~#'` quote).
fn root_is_array(v: &Value) -> bool {
    match v {
        Value::Meta(m) => root_is_array(&m.inner),
        Value::Vector(_) | Value::Set(_) | Value::Map(_) | Value::List(_) | Value::Lazy(_) => true,
        _ => false,
    }
}

pub(crate) fn write_json_stream(i: &mut Interp, v: &Value, w: &mut dyn std::io::Write) -> Result<(), RjError> {
    let mut e = StreamEnc { i, w, cache: HashMap::new(), buf: String::new(), budget: 0 };
    let quote = !root_is_array(v);
    if quote {
        e.w.write_all(b"[\"~#'\",").map_err(io_err)?;
    }
    e.enc(v, false)?;
    if quote {
        e.w.write_all(b"]").map_err(io_err)?;
    }
    Ok(())
}

impl<'a> StreamEnc<'a> {
    fn raw(&mut self, b: &[u8]) -> Result<(), RjError> {
        self.w.write_all(b).map_err(io_err)
    }

    fn json_str(&mut self, s: &str) -> Result<(), RjError> {
        serde_json::to_writer(&mut *self.w, s).map_err(|e| io_err(e.into()))
    }

    // Writes `self.buf` as a JSON string, or its `^code` if cached (see `maybe_cache_str`).
    fn emit_buf(&mut self, cacheable: bool) -> Result<(), RjError> {
        if let Some(&idx) = self.cache.get(self.buf.as_str()) {
            let code = index_to_code(idx);
            return self.json_str(&code);
        }
        if cacheable && self.buf.len() > 3 {
            if self.cache.len() >= MAX_CACHE_ENTRIES {
                self.cache.clear();
            }
            let n = self.cache.len();
            self.cache.insert(self.buf.clone(), n);
        }
        let b = std::mem::take(&mut self.buf);
        let r = self.json_str(&b);
        self.buf = b;
        r
    }

    fn wire(&mut self, cacheable: bool, f: impl FnOnce(&mut String)) -> Result<(), RjError> {
        self.buf.clear();
        f(&mut self.buf);
        self.emit_buf(cacheable)
    }

    fn tag(&mut self, t: &str) -> Result<(), RjError> {
        self.wire(true, |b| b.push_str(t))
    }

    // Elements of a lazy/improper seq, realized one at a time (same 100k cap as `realize_deep`).
    fn enc_lazy(&mut self, v: &Value) -> Result<(), RjError> {
        self.raw(b"[")?;
        self.tag("~#list")?;
        self.raw(b",[")?;
        let mut cur = v.clone();
        let mut first = true;
        while let Some((h, t)) = crate::builtins::uncons(self.i, &cur)? {
            self.budget += 1;
            if self.budget > 100_000 {
                return Err(RjError::other("realizing infinite/huge lazy seq for printing (capped at 100000 elements)"));
            }
            if !first {
                self.raw(b",")?;
            }
            first = false;
            self.enc(&h, false)?;
            cur = t;
        }
        self.raw(b"]]")
    }

    fn enc(&mut self, v: &Value, is_key: bool) -> Result<(), RjError> {
        use std::fmt::Write as _;
        match v {
            Value::Meta(m) => self.enc(&m.inner, is_key),
            Value::Nil => self.raw(b"null"),
            Value::Bool(b) => self.raw(if *b { b"true" } else { b"false" }),
            Value::Int(n) => {
                if (-MAX_SAFE_INT..=MAX_SAFE_INT).contains(n) {
                    serde_json::to_writer(&mut *self.w, n).map_err(|e| io_err(e.into()))
                } else {
                    self.wire(false, |b| {
                        let _ = write!(b, "~i{n}");
                    })
                }
            }
            Value::Float(f) => {
                if f.is_nan() {
                    self.json_str("~zNaN")
                } else if f.is_infinite() {
                    self.json_str(if *f > 0.0 { "~zINF" } else { "~z-INF" })
                } else {
                    serde_json::to_writer(&mut *self.w, f).map_err(|e| io_err(e.into()))
                }
            }
            Value::BigInt(b) | Value::BigInteger(b) => {
                let d = b.to_decimal_string();
                self.wire(false, |o| {
                    let _ = write!(o, "~n{d}");
                })
            }
            Value::BigDec(d) => {
                let d = d.to_java_string();
                self.wire(false, |o| {
                    let _ = write!(o, "~f{d}");
                })
            }
            Value::Char(c) => self.wire(false, |o| {
                let _ = write!(o, "~c{c}");
            }),
            Value::Uuid(u) => {
                let s = Value::format_uuid(**u);
                self.wire(false, |o| {
                    let _ = write!(o, "~u{s}");
                })
            }
            Value::Str(s) => self.wire(is_key, |o| {
                if matches!(s.chars().next(), Some('~') | Some('^') | Some('`')) {
                    o.push('~');
                }
                o.push_str(s);
            }),
            Value::Keyword(k) => self.wire(true, |o| {
                o.push_str("~:");
                o.push_str(k.text_ref());
            }),
            Value::Sym(sym) => self.wire(true, |o| {
                o.push_str("~$");
                if let Some(ns) = &sym.ns {
                    let _ = write!(o, "{ns}/");
                }
                let _ = write!(o, "{}", sym.name);
            }),
            Value::List(items) => {
                if crate::builtins::lazy_tail_split(items).is_some() {
                    return self.enc_lazy(v);
                }
                self.raw(b"[")?;
                self.tag("~#list")?;
                self.raw(b",[")?;
                for (n, it) in items.iter().enumerate() {
                    if n > 0 {
                        self.raw(b",")?;
                    }
                    self.enc(it, false)?;
                }
                self.raw(b"]]")
            }
            Value::Lazy(_) => self.enc_lazy(v),
            Value::Vector(items) => {
                self.raw(b"[")?;
                for (n, it) in items.iter().enumerate() {
                    if n > 0 {
                        self.raw(b",")?;
                    }
                    self.enc(it, false)?;
                }
                self.raw(b"]")
            }
            Value::Set(s) => {
                self.raw(b"[")?;
                self.tag("~#set")?;
                self.raw(b",[")?;
                let mut items: Vec<Value> = Vec::with_capacity(s.len());
                for it in s.iter() {
                    items.push(self.i.realize_deep(it)?);
                }
                items.sort_by_cached_key(crate::printer::pr_str);
                for (n, it) in items.iter().enumerate() {
                    if n > 0 {
                        self.raw(b",")?;
                    }
                    self.enc(it, false)?;
                }
                self.raw(b"]]")
            }
            Value::Map(m) => {
                self.raw(b"[\"^ \"")?;
                for (k, val) in m.iter() {
                    self.raw(b",")?;
                    self.enc(k, true)?;
                    self.raw(b",")?;
                    self.enc(val, false)?;
                }
                self.raw(b"]")
            }
            other => Err(RjError::type_err(format!(
                "mova.transit/write-json: unsupported value type {}",
                other.type_name()
            ))),
        }
    }
}

#[cfg(test)]
mod write_json_tests {
    use super::*;

    fn write_json(v: &Value) -> String {
        let mut cache = HashMap::new();
        serde_json::to_string(&encode(v, false, &mut cache).unwrap()).unwrap()
    }

    fn read_json(s: &str) -> Value {
        read_json_str(s).unwrap()
    }

    fn roundtrip(v: Value) {
        let s = write_json(&v);
        let back = read_json_str(&s).unwrap();
        assert_eq!(back, v, "roundtrip mismatch for {s:?}");
    }

    #[test]
    fn scalars_roundtrip() {
        roundtrip(Value::Nil);
        roundtrip(Value::Bool(true));
        roundtrip(Value::Int(42));
        roundtrip(Value::Int(-42));
        roundtrip(Value::Int(i64::MAX));
        roundtrip(Value::Float(1.5));
        roundtrip(Value::Char('x'));
        roundtrip(Value::Str(Str::from("hello".to_string())));
        roundtrip(Value::Str(Str::from("~escape-me".to_string())));
        roundtrip(Value::Str(Str::from("^escape-me".to_string())));
        roundtrip(Value::Str(Str::from("`escape-me".to_string())));
        roundtrip(Value::Keyword(Keyword::from("foo".to_string())));
        roundtrip(Value::Keyword(Keyword::from("ns/foo".to_string())));
        roundtrip(Value::Sym(Symbol::simple("bar")));
        roundtrip(Value::Sym(crate::builtins::strings::symbol_from_str("ns/bar")));
    }

    #[test]
    fn map_key_caching_reuses_repeated_keyword() {
        // Two map entries sharing the same (long-enough) keyword key: the
        // SECOND occurrence must be written as a `^<code>` back-reference,
        // exactly like the vendored built-in caches do.
        let mut m1 = PMap::new();
        m1.insert(Value::Keyword(Keyword::from("filename".to_string())), Value::Int(1));
        let mut m2 = PMap::new();
        m2.insert(Value::Keyword(Keyword::from("filename".to_string())), Value::Int(2));
        let mut outer = PMap::new();
        outer.insert(Value::Str(Str::from("a".to_string())), Value::Map(m1));
        outer.insert(Value::Str(Str::from("b".to_string())), Value::Map(m2));
        let v = Value::Map(outer);
        let s = write_json(&v);
        assert!(s.contains("~:filename"), "first occurrence must be spelled out: {s}");
        assert_eq!(s.matches("~:filename").count(), 1, "second occurrence must be cache-coded: {s}");
        assert_eq!(read_json(&s), v);
    }

    #[test]
    fn list_and_set_and_vector_roundtrip() {
        let mut v = PVec::new();
        v.push_back(Value::Int(1));
        v.push_back(Value::Int(2));
        roundtrip(Value::List(v.clone()));
        roundtrip(Value::Vector(v));

        let mut s = champ::PersistentHashSet::new().transient();
        s.insert(Value::Keyword(Keyword::from("a".to_string())));
        s.insert(Value::Keyword(Keyword::from("b".to_string())));
        roundtrip(Value::Set(s.persistent()));
    }

    #[test]
    fn nested_map_roundtrip() {
        let mut inner = PMap::new();
        inner.insert(Value::Keyword(Keyword::from("row".to_string())), Value::Int(3));
        inner.insert(Value::Keyword(Keyword::from("col".to_string())), Value::Int(7));
        let mut outer = PMap::new();
        outer.insert(Value::Keyword(Keyword::from("loc".to_string())), Value::Map(inner));
        outer.insert(Value::Keyword(Keyword::from("name".to_string())), Value::Str(Str::from("x".to_string())));
        roundtrip(Value::Map(outer));
    }

    /// Independent reference encoder for the wrap test below -- NOT the
    /// production `maybe_cache_str`/`encode` (deliberately: a self-
    /// encode/self-decode round trip through this crate's own (formerly
    /// both-unbounded, so self-consistent either way) cache would never
    /// have caught the real bug -- see that test's doc). Transcribed
    /// straight from transit-java's real `WriteCache.cacheWrite`
    /// (module doc), independently of this file's production code, so
    /// it produces wire text a REAL writer would produce, wrap included.
    fn reference_write_map(pairs: &[(&str, i64)]) -> String {
        let mut cache: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut index = 0usize;
        let mut out = String::from("[\"^ \"");
        for (k, v) in pairs {
            let wire_key = if k.len() >= 4 {
                if let Some(code) = cache.get(*k) {
                    code.clone()
                } else {
                    if index == MAX_CACHE_ENTRIES {
                        cache.clear();
                        index = 0;
                    }
                    cache.insert(k.to_string(), index_to_code(index));
                    index += 1;
                    k.to_string()
                }
            } else {
                k.to_string()
            };
            out.push_str(&format!(",{},{v}", serde_json::to_string(&wire_key).unwrap()));
        }
        out.push(']');
        out
    }

    /// Regression for the root cause behind `canonicalize-java-analysis`'s
    /// `:class` turning up `nil` on real (large) `db.transit.json`
    /// documents: the priority cache must clear back to index 0 once
    /// `MAX_CACHE_ENTRIES` (1936) entries have been assigned in the
    /// current generation -- matching transit-java's real `WriteCache`/
    /// `ReadCache` exactly (module doc). Feeds `read_json_str` wire text
    /// built by an INDEPENDENT reference encoder (above) that forces a
    /// real wrap partway through -- a repeated "marker" key is written
    /// once before the wrap (cached at a low index), referenced by that
    /// same low `^code` again before the wrap, then, after >1936 filler
    /// keys force a wrap, re-spelled in full and re-cached at a NEW low
    /// index (exactly what a real writer does) -- the pre-fix decoder,
    /// whose cache never cleared, would resolve the marker's pre-wrap
    /// `^code` reference correctly but then get the WRONG value (some
    /// unrelated filler key from thousands of pushes earlier) the first
    /// time it saw the marker's second, post-wrap `^code` reference,
    /// since its un-cleared cache still had 1936+ live filler entries at
    /// that same low index.
    #[test]
    fn cache_wraps_past_max_entries_without_corrupting_values() {
        let mut pairs: Vec<(String, i64)> = Vec::new();
        pairs.push(("marker-keyword-key".to_string(), -1));
        pairs.push(("marker-keyword-key".to_string(), -2)); // pre-wrap `^code` hit
        for i in 0..2000i64 {
            pairs.push((format!("filler-cacheable-key-{i:04}"), i));
        }
        pairs.push(("marker-keyword-key".to_string(), -3)); // post-wrap: re-spelled + re-cached
        pairs.push(("marker-keyword-key".to_string(), -4)); // post-wrap `^code` hit, new index

        let ref_pairs: Vec<(&str, i64)> = pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let wire = reference_write_map(&ref_pairs);
        // Sanity: the wrap really happened on the wire (marker re-spelled).
        assert_eq!(wire.matches("marker-keyword-key").count(), 2, "expected exactly one re-spell: {wire}");

        let decoded = read_json(&wire);
        let Value::Map(m) = decoded else { panic!("expected a map") };
        // Last-write-wins per key (map semantics): the marker's final
        // value is -4, and every filler key must map to its own index,
        // not some other entry's.
        assert_eq!(m.get(&Value::Str(Str::from("marker-keyword-key".to_string()))), Some(&Value::Int(-4)));
        for i in 0..2000i64 {
            let key = Value::Str(Str::from(format!("filler-cacheable-key-{i:04}")));
            assert_eq!(m.get(&key), Some(&Value::Int(i)), "filler entry {i} corrupted after cache wrap");
        }
    }

    // Streaming writer must be byte-identical to the tree writer.
    fn both(src: &str) -> (String, String) {
        let mut interp = Interp::new();
        let v = interp.eval_str("t", src).unwrap();
        let realized = interp.realize_deep(&v).unwrap();
        let mut cache = HashMap::new();
        let mut j = encode(&realized, false, &mut cache).unwrap();
        if !matches!(j, serde_json::Value::Array(_)) {
            j = serde_json::Value::Array(vec![serde_json::Value::String("~#'".to_string()), j]);
        }
        let old = serde_json::to_string(&j).unwrap();
        let mut out: Vec<u8> = Vec::new();
        write_json_stream(&mut interp, &v, &mut out).unwrap();
        let new = String::from_utf8(out).unwrap();
        if !new.contains("~z") && !new.contains("~n") && !new.contains("~f") && !new.contains("~u") {
            assert_eq!(read_json_str(&new).unwrap(), realized);
        }
        (old, new)
    }

    #[test]
    fn stream_writer_bytes_equal_tree_writer() {
        let srcs = [
            "nil",
            "true",
            "42",
            "-9007199254740993",
            "9007199254740993",
            "1.5",
            "-0.0",
            "1e300",
            "##NaN",
            "##Inf",
            "\"plain\"",
            "\"~tilde\"",
            "\"^caret\"",
            "\"`tick\"",
            "\"quote \\\" nl \\n tab \\t uni \\u00e9 \\u0001\"",
            "\\x",
            "42N",
            "1.5M",
            ":foo",
            ":ns/foo-long",
            "'sym",
            "'ns/sym-long",
            "[1 2 [3 :abcd] :abcd]",
            "'(1 2 :abcd :abcd)",
            "#{:aaaa :bbbb :cccc 1 \"x\" [1 2]}",
            "{:abcd 1 :efgh {:abcd 2 \"longkey\" 3 \"longkey2\" #{:abcd}}}",
            "(map inc [1 2 3])",
            "[1 (map inc [1 2]) {:abcd (filter odd? (range 9))}]",
            "{:a (keys {:bbbb 1 :cccc 2})}",
            "(cons 1 (range 3))",
            "(with-meta [1 2] {:m 1})",
            "{(with-meta 'ksym {:x 1}) 1}",
            "(java.util.UUID/fromString \"550e8400-e29b-41d4-a716-446655440000\")",
            "(into {} (map (fn [i] [(keyword (str \"kkkk-\" i)) i]) (range 5000)))",
            "(vec (mapcat (fn [i] [{(keyword (str \"kkkk-\" (mod i 2500))) i :abcd \"~x\"}]) (range 6000)))",
        ];
        for s in srcs {
            let (old, new) = both(s);
            assert_eq!(old, new, "mismatch for {s}");
        }
    }
}
