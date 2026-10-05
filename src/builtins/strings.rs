//! `str pr-str println prn print name keyword symbol subs` plus
//! `clojure.string`-ish `split join upper-case lower-case trim starts-with?
//! ends-with? includes? replace`, registered both as plain globals and
//! under the `clojure.string/` and `string/` namespaces (cheap aliases of
//! the same native). `env::Env::get` already falls back from a namespaced
//! symbol to its bare name, so the explicit aliasing here is belt-and-
//! braces robustness (works even if that fallback behavior ever changes,
//! and survives the bare name being shadowed later).
//!
//! R3: `split`/`replace`/`replace-first` additionally accept a
//! `Value::Regex` pattern (`#"..."` / `re-pattern`), alongside their
//! original plain-string form -- see `split_on`/`replace_impl` below. The
//! string-pattern path is untouched literal-text behavior (still not real
//! Clojure semantics for a *string* `match` arg passed where Clojure
//! expects a char/Pattern, but that gap predates R3 and is unrelated to
//! regex support, so it's left alone here).
//!
//! R4 adds `pr` (readable, no trailing newline -- `prn`'s twin) and more
//! `clojure.string` ports: `blank? index-of last-index-of split-lines
//! triml trimr` (same bare-plus-namespaced-alias treatment).
//!
//! Also here (not a `clojure.string` fn, but a fellow "macro-support
//! native" alongside `gensym`): the two tiny primitives `declare`/
//! `defonce` (`core/core.mova`) are built from -- `--intern-unbound!` calls
//! `env::Env::intern` directly (interns the symbol's `VarCell` without
//! writing a value through it, leaving it genuinely unbound, exactly like
//! real Clojure's `declare`) and `--global-bound?` reports whether a
//! symbol currently resolves in globals (an interned-but-unbound cell
//! reads as "not bound", matching `Env::get`'s own unbound-is-absent
//! contract). Leading `--` marks these as internal, mirroring `defn-`'s own
//! `-`-for-private convention one dash further.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::builtins::collections::materialize;
use crate::builtins::{reg, reg_unmeta, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{ArrayKind, ArrayVal, Keyword, PVec, Str, Symbol, Value, STR_ROPE_MIN};

/// Backs the `gensym` native: monotonically increasing, so `(gensym)` /
/// `(gensym "prefix")` calls are unique for the life of the process.
/// Independent of `quasiquote.rs`'s own auto-gensym counter (see that
/// module's doc comment for why they aren't shared) -- each is
/// individually unique, which is all uniqueness requires.
pub(crate) static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(1);

fn expect_str<'a>(v: &'a Value, op: &str) -> Result<&'a Str, RjError> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(RjError::type_err(format!(
            "{op}: expected a string, got {}",
            other.type_name()
        ))
        .with_class(char_seq_reject_class(other))),
    }
}

/// W3a: which JVM class real Clojure raises when a `clojure.string` fn
/// (every one of whose oracle signatures is `^CharSequence`, see
/// `char_seq_str`'s doc) is handed something that is not one. Measured
/// against the 1.13.0-alpha6 oracle, and the split is on `nil`
/// specifically, not on "wrong type" generally:
///
/// - `nil` => `java.lang.NullPointerException`. The generated
///   `checkcast`/`.toString()`/`.length()` dereferences null:
///   `(s/reverse nil)` => `NullPointerException: Cannot invoke
///   "java.lang.CharSequence.length()" because "seq" is null`;
///   `(s/replace nil #"foo" "bar")`, `s/capitalize`, `s/upper-case`,
///   `s/lower-case`, `s/split`, `s/trim`, `s/triml`, `s/trimr`,
///   `s/trim-newline`, `s/re-quote-replacement`, `s/index-of`,
///   `s/starts-with?`, `s/includes?`, `s/escape` all likewise. This is
///   string.clj's whole `nil-handling` `are` block.
/// - anything else non-`CharSequence` => `java.lang.ClassCastException`
///   (`(s/reverse 5)` => `class java.lang.Long cannot be cast to class
///   java.lang.CharSequence`), which is already `ErrorKind::TypeErr`'s
///   default mapping -- named explicitly here so the two cases sit side
///   by side rather than one being an unstated fallthrough.
fn char_seq_reject_class(v: &Value) -> crate::error::JvmClass {
    match v {
        Value::Nil => crate::error::JvmClass::NullPointer,
        _ => crate::error::JvmClass::ClassCast,
    }
}

/// S7: `clojure.string`'s OWN oracle signatures are all `^CharSequence`
/// (`.oracle/clojure-src/src/clj/clojure/string.clj`'s `reverse`/`replace`/
/// `capitalize`/`trim`/... every one of them), not `^String` -- so on the
/// JVM, a `StringBuilder`/`StringBuffer` argument (BOTH implement
/// `CharSequence`) works everywhere a literal string does, coerced via
/// `.toString()` at entry. `char_seq_str` is that coercion for mova's
/// two host-side `CharSequence`s (`hostclass.rs`'s `HostKind::
/// StringBuilder`/`StringBuffer`, S6): measured via `for.clj`'s
/// `char-sequence-handling` deftest, which wraps a plain string in
/// `(StringBuffer. s)` before calling `s/reverse`/`s/replace`/`s/trim`/etc
/// -- `expect_str` alone (which only knows `Value::Str`) rejects every one
/// of those calls, "don't know how to create a seq from stringbuffer" or
/// "expected a string, got stringbuffer" depending on the callee.
/// `HostState::CharBuf`'s content is immutable once constructed (S6's
/// `HostKind` doc: "construction-only", still true -- this is a READ, not
/// a new mutation path), so cloning it out from under the mutex is exactly
/// what `.toString()` would snapshot on the JVM too. Returns an OWNED
/// `Str` (cheap: `Str` is `Arc`/rope-backed) since a `HostInst`'s content
/// lives behind a lock, unlike `expect_str`'s borrowed `&Str`.
fn char_seq_str(v: &Value, op: &str) -> Result<Str, RjError> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::HostInst(h)
            if matches!(h.kind, crate::hostclass::HostKind::StringBuilder | crate::hostclass::HostKind::StringBuffer) =>
        {
            match &*crate::sync::lock_mutex(&h.state) {
                crate::hostclass::HostState::CharBuf(s) => Ok(s.clone()),
                _ => unreachable!("StringBuilder/StringBuffer HostInst always holds HostState::CharBuf"),
            }
        }
        other => Err(RjError::type_err(format!(
            "{op}: expected a string, got {}",
            other.type_name()
        ))
        .with_class(char_seq_reject_class(other))),
    }
}

/// clojure-lsp campaign (mova/PLAN.md): `clojure.string/starts-with?`/
/// `ends-with?`/`includes?`'s real Clojure source calls `(.toString s)`
/// on its first arg before comparing (`^CharSequence s` is only a type
/// HINT, not a runtime check -- `.toString` reflectively dispatches on
/// ANY non-nil Object), so e.g. `(str/ends-with? 'foo.bar ".")` works on
/// the JVM for a bare SYMBOL. `char_seq_str`'s strict "string or
/// StringBuilder only" is right for `reverse`/etc, whose real source
/// really does require an actual `CharSequence`; this lenient sibling
/// is for the three functions that specifically stringify first. Falls
/// back to `str_concat`'s single-arg form (same `.toString()`-parity
/// path `str`/`(.toString x)` interop already share, honoring a
/// `deftype`'s own override) for anything that isn't already a
/// string/StringBuilder. `Value::Nil` is deliberately NOT covered here:
/// real `.toString()` on a null reference throws `NullPointerException`,
/// so `char_seq_str`'s plain type error stands in for that case too.
fn char_seq_str_lenient(interp: &mut Interp, v: &Value, op: &str) -> Result<Str, RjError> {
    match v {
        Value::Str(_) => char_seq_str(v, op),
        // clojure-lsp campaign: only StringBuilder/StringBuffer are real
        // CharSequences -- any OTHER HostInst (e.g. a `java.io.File`, which
        // real `.toString()`-based `starts-with?`/`ends-with?`/`includes?`
        // happily accepts on the JVM, see `clojure_lsp.source_paths`'s
        // `relativize-filepath` call on a raw `normalize-file` result) must
        // fall through to the lenient `str_concat` branch below, not the
        // strict `char_seq_str` (which only recognizes StringBuilder/Buffer
        // and would wrongly reject it).
        Value::HostInst(h)
            if matches!(h.kind, crate::hostclass::HostKind::StringBuilder | crate::hostclass::HostKind::StringBuffer) =>
        {
            char_seq_str(v, op)
        }
        Value::Nil => char_seq_str(v, op),
        other => str_concat(interp, std::slice::from_ref(other)),
    }
}

/// `index-of`/`last-index-of` accept either a string or a char as the
/// needle (matching JVM Clojure's overloaded `String/indexOf`); this
/// normalizes either into an owned `String` to search for at the BYTE
/// level (see `char_find`/`char_rfind` below -- this is the fix for the
/// V05-PERF hot loop: the old `pattern_chars` fed a `Vec<char>` haystack
/// AND needle into a hand-rolled char-by-char scan, which required
/// collecting the ENTIRE haystack into a `Vec<char>` first).
fn pattern_string(v: &Value, op: &str) -> Result<String, RjError> {
    match v {
        Value::Str(s) => Ok(s.to_string()),
        Value::Char(c) => Ok(c.to_string()),
        other => Err(RjError::type_err(format!(
            "{op}: expected a string or char pattern, got {}",
            other.type_name()
        ))),
    }
}

/// If `v` is (or names) exactly one char -- a `Value::Char`, or a
/// `Value::Str` whose content is a single codepoint -- returns it. Used by
/// `index-of`/`last-index-of` to take the M8 rope-native fast path
/// ([`Str::find_char_from`]/[`Str::rfind_char_from`]): a single-char
/// needle can never straddle a rope leaf boundary, so it's the one needle
/// shape that's cheap to search natively without materializing a `Rope`
/// haystack. This covers the actual host workload -- `oma.core.text/
/// line-start`/`line-end` always search for `'\n'` (via `clojure.string/
/// index-of`/`last-index-of`, i.e. as a one-char `Value::Str`, not a
/// `Value::Char`) over the whole document, once per line.
fn single_char_needle(v: &Value) -> Option<char> {
    match v {
        Value::Char(c) => Some(*c),
        Value::Str(s) if s.char_count_cached() == 1 => s.char_at(0),
        _ => None,
    }
}

/// Byte offset of char index `idx` in `s` (mova strings are char-indexed
/// throughout), found by walking `char_indices()` directly -- no
/// intermediate `Vec<char>` allocation of the whole string. `idx` equal to
/// `s`'s char count is a valid "one past the end" position (subs's `end`,
/// or `index-of`'s `from` sitting exactly at the string's length); `None`
/// only when `idx` is genuinely out of bounds.
///
/// This (and `char_find`/`char_rfind` below) is the fix for a profiled
/// O(n^2) hot loop: `oma.core.layout/layout-doc` (the omawrite host repo)
/// calls `(str/index-of text "\n" ls)` and `(subs text ls le)` once per
/// LOGICAL LINE of a document, with `text` the WHOLE document every time.
/// The old `subs`/`index-of` each collected the full haystack argument
/// into a fresh `Vec<char>` on every call regardless of `ls`, so opening
/// an N-line file paid an O(document length) allocating scan N times --
/// quadratic in document size, and the dominant cost of the file-open
/// scaling wall measured in docs/mova-vs-jank-bench.md (2KB: 2.5s, 5KB:
/// 8.3s, 10KB: 23.8s, 20KB: did not finish in 30s). Walking
/// `char_indices()` without collecting removed the allocation (and the
/// `imbl`/`Vec` bookkeeping) on every call, leaving it O(document length)
/// per call without a byte-offset cache -- the `Value::Str` representation
/// change that note flagged as out of scope landed in M8
/// (SPEC-M8-TEXT-INTEGRATION.md): `subs` itself no longer calls this fn at
/// all (see `Str::char_slice`, `O(log n)` for a `Rope` source). This fn
/// (via `char_find`/`char_rfind`) still backs `index-of`/`last-index-of`'s
/// multi-char-needle case, which stayed a "materialize with care" op --
/// `Deref` (`s.char_indices()`, `s.len()` below) transparently
/// materializes a `Rope` source's content first. See
/// `single_char_needle`'s doc comment for the rope-native fast path those
/// two builtins take instead for the actual host usage (searching for a
/// single char).
fn char_byte_offset(s: &Str, idx: usize) -> Option<usize> {
    // ASCII fast path: char index IS byte index. `is_ascii_cached` reads a
    // cache populated once per `Str` allocation (see value.rs) rather than
    // re-scanning the whole string on every call -- this editor-hosting
    // workload is overwhelmingly ASCII text, and the E5 typing-bench
    // regression (2.4x CPU, 66% of samples in this fn) was per-call
    // `is_ascii()` scans stacking up per keystroke; caching removes even
    // that scan's O(n) cost for all but the first touch of a given string.
    if s.is_ascii_cached() {
        return (idx <= s.len()).then_some(idx);
    }
    let mut n = 0usize;
    for (b, _) in s.char_indices() {
        if n == idx {
            return Some(b);
        }
        n += 1;
    }
    if n == idx {
        Some(s.len())
    } else {
        None
    }
}

/// Char count of the (typically short-lived, uncached) `needle`/pattern
/// argument -- an ASCII fast path without going through `Str`'s cache,
/// since needles here are freshly-built `String`s/literals, not cached
/// `Value::Str` haystacks. See [`Str::char_count_cached`] for the haystack
/// version, used everywhere below instead of this.
fn count_chars_uncached(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().count()
    }
}

/// First occurrence of `needle` in `s` at or after char index `from`,
/// char-indexed. Converts `from` to a byte offset (`char_byte_offset`,
/// no allocation), searches `s[byte_from..]` at the BYTE level
/// (`str::find`, no allocation -- memchr-optimized for a single-byte
/// needle like layout-doc's `"\n"`), then converts the match's byte
/// offset back to a char index by counting chars in the short span
/// between `byte_from` and the match (not the whole haystack). An empty
/// `needle` matches at `from` itself, mirroring Java's `String/indexOf("")`.
fn char_find(s: &Str, needle: &str, from: usize) -> Option<usize> {
    if needle.is_empty() {
        let total = s.char_count_cached();
        return (from <= total).then_some(from);
    }
    let byte_from = char_byte_offset(s, from)?;
    let byte_pos = byte_from + s[byte_from..].find(needle)?;
    if s.is_ascii_cached() {
        return Some(byte_pos);
    }
    Some(from + s[byte_from..byte_pos].chars().count())
}

/// Last occurrence of `needle` in `s` at or before char index `from`
/// (searching backward from `from`, Java `String/lastIndexOf`-style --
/// NOT "at or after" like [`char_find`]). Bounds the search to
/// `s[..byte_end]` and uses `str::rfind` (byte-level, no allocation)
/// rather than scanning a collected `Vec<char>` backward.
fn char_rfind(s: &Str, needle: &str, from: usize) -> Option<usize> {
    let total = s.char_count_cached();
    if needle.is_empty() {
        return Some(from.min(total));
    }
    let needle_len = count_chars_uncached(needle);
    if total < needle_len {
        return None;
    }
    let upper = from.min(total - needle_len);
    let byte_end = char_byte_offset(s, upper + needle_len).unwrap_or(s.len());
    let byte_pos = s[..byte_end].rfind(needle)?;
    if s.is_ascii_cached() {
        return Some(byte_pos);
    }
    Some(s[..byte_pos].chars().count())
}

fn require_index(v: &Value, op: &str) -> Result<usize, RjError> {
    match v {
        Value::Int(n) if *n >= 0 => Ok(*n as usize),
        other => Err(RjError::type_err(format!(
            "{op}: expected a non-negative int index, got {}",
            other.type_name()
        ))),
    }
}

/// `index-of`'s `from-index`, specifically -- unlike `subs`/`.substring`/
/// `.charAt` (which all THROW on a negative index via [`require_index`]
/// above), Java's `String.indexOf(str, fromIndex)` documents "if
/// fromIndex is negative, it has the same effect as if it were zero" --
/// clamps rather than errors. Measured: `(clojure.string/index-of "tacos"
/// "o" -100)` is `3`, not a thrown exception. Deliberately NOT folded into
/// [`require_index`] itself: that fn's OTHER callers (`subs`, `.charAt`,
/// `.substring`) must keep throwing on a negative index, matching their
/// own (different) JVM contracts.
fn require_index_clamped(v: &Value, op: &str) -> Result<usize, RjError> {
    match v {
        Value::Int(n) => Ok((*n).max(0) as usize),
        other => Err(RjError::type_err(format!("{op}: expected an int index, got {}", other.type_name()))),
    }
}

fn require_int(v: &Value, op: &str) -> Result<i64, RjError> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(RjError::type_err(format!(
            "{op}: expected an int, got {}",
            other.type_name()
        ))),
    }
}

/// Mirrors `reader::parse_symbol`'s `ns/name` splitting (that fn is private
/// to `reader.rs`), for `(symbol "ns/name")`.
pub(crate) fn symbol_from_str(s: &str) -> Symbol {
    if s == "/" {
        return Symbol::simple("/");
    }
    if let Some(idx) = s.find('/') {
        if idx > 0 && idx + 1 < s.len() {
            return Symbol {
                ns: Some(s[..idx].into()),
                name: s[idx + 1..].into(),
            };
        }
    }
    Symbol::simple(s)
}

/// `find-keyword`'s shared registry probe: `full` is the flat `"ns/name"`
/// or `"name"` string (`Value::Keyword`'s own repr) to look up.
fn find_keyword_lookup(interp: &Interp, full: &Str) -> Value {
    if interp.keywords.contains(full) {
        Value::Keyword(Keyword::from(full))
    } else {
        Value::Nil
    }
}

fn keyword_full_name(v: &Value, op: &str) -> Result<Str, RjError> {
    match v {
        Value::Keyword(k) => Ok(k.text()),
        Value::Sym(s) => Ok(match &s.ns {
            Some(ns) => format!("{ns}/{}", s.name).into(),
            None => s.name.clone(),
        }),
        Value::Str(s) => Ok(s.clone()),
        other => Err(RjError::type_err(format!(
            "{op}: expected a keyword, symbol, or string, got {}",
            other.type_name()
        ))),
    }
}

/// `clojure.string/split`'s `limit` semantics -- `java.util.regex.
/// Pattern#split(s, limit)`'s three cases, shared by the regex and literal
/// paths below (`split_regex`/`split_literal` each supply their own
/// "did anything match" probe and their own splitter):
///
/// - `limit == 0`: every piece, with ALL trailing empty pieces dropped
///   (this is mova's pre-R5 2-arity behavior, kept as the default).
/// - `limit > 0`: at most `limit` pieces -- the last one is whatever is
///   left AFTER `limit - 1` splits, unsplit. Delegated straight to the
///   underlying splitter's own `splitn`, so it never needs the trailing-
///   empty rule at all.
/// - `limit < 0`: every piece, trailing empties KEPT.
///
/// A string with no match at all is always returned whole regardless of
/// `limit`'s sign (mirrors the JVM: a pattern that never matches never
/// triggers the trailing-empty rule, so `(split "" #",")` is `[""]`, not
/// `[]`) -- callers check that themselves before reaching here, so this
/// only ever runs the `limit <= 0` trailing-empty rule on an actual split.
fn apply_split_limit(pieces: Vec<String>, limit: i64) -> Vec<String> {
    let mut pieces = pieces;
    if limit == 0 {
        while pieces.last().is_some_and(|p| p.is_empty()) {
            pieces.pop();
        }
    }
    pieces
}

/// The regex path: every piece between/around matches, `limit`-shaped per
/// `apply_split_limit`'s doc.
fn split_regex(re: &fancy_regex::Regex, s: &str, limit: i64) -> Vec<String> {
    // D5: `fancy_regex`'s split iterators yield `Result` (a backtracking
    // pattern can exhaust its budget mid-walk -- see `builtins::regex`'s
    // `match_err`). `split` has no error channel of its own, and a
    // partial split would be a silently wrong answer, so a failed step
    // ends the walk and the remainder of the input comes back as one
    // final piece -- the same shape a pattern that stops matching
    // produces.
    if limit > 0 {
        return re.splitn(s, limit as usize).map_while(|p| p.ok()).map(|p| p.to_string()).collect();
    }
    if !matches!(re.find(s), Ok(Some(_))) {
        return vec![s.to_string()];
    }
    apply_split_limit(re.split(s).map_while(|p| p.ok()).map(|p| p.to_string()).collect(), limit)
}

/// The literal-string path (mova's own deviation from real Clojure, which
/// requires a `Pattern`): an empty separator is a no-op regardless of
/// `limit` (pre-existing mova behavior, kept as-is); otherwise `limit`-
/// shaped identically to `split_regex`.
fn split_literal(s: &str, sep: &str, limit: i64) -> Vec<String> {
    if sep.is_empty() {
        return vec![s.to_string()];
    }
    if limit > 0 {
        return s.splitn(limit as usize, sep).map(|p| p.to_string()).collect();
    }
    if !s.contains(sep) {
        return vec![s.to_string()];
    }
    apply_split_limit(s.split(sep).map(|p| p.to_string()).collect(), limit)
}

/// Whether `v` is one of `replace`/`replace-first`'s CharSequence-shaped
/// replacement/pattern values ([`char_seq_str`]'s domain) -- used to tell
/// "literal CharSequence" apart from "function" in the regex-pattern arm
/// below, matching real Clojure's own `(instance? CharSequence
/// replacement)` dispatch in `clojure.string/replace`.
fn is_char_seq(v: &Value) -> bool {
    matches!(v, Value::Str(_))
        || matches!(v, Value::HostInst(h) if matches!(h.kind, crate::hostclass::HostKind::StringBuilder | crate::hostclass::HostKind::StringBuffer))
}

/// Java `Matcher.appendReplacement`-style replacement-template
/// interpreter: `\` escapes the NEXT char literally (the backslash itself
/// is dropped -- exactly [`Matcher/quoteReplacement`'s][re-quote] own
/// contract, which is the whole reason `re-quote-replacement` exists);
/// `$` followed by one or more digits substitutes that capture group
/// (`$0` the whole match, greedy digit run, same as `Matcher`'s own
/// group-number parsing; an out-of-range or non-participating group
/// contributes nothing, matching a null `Matcher.group(n)`); any other
/// char is literal.
///
/// [re-quote]: https://docs.oracle.com/javase/8/docs/api/java/util/regex/Matcher.html#quoteReplacement-java.lang.String-
///
/// Implemented by hand rather than handed to the `regex` crate's OWN
/// replacement-string parser (`Regex::replace_all(s, "$1")`'s built-in
/// syntax): that parser's escape convention for a literal `$` is `$$`,
/// NOT Java's `\`-escape, so a `re-quote-replacement`-produced string
/// (which MUST be `\`-escaped -- measured, its own direct-output test in
/// `for.clj`'s `char-sequence-handling` deftest requires it) came out
/// wrong fed through it: `(replace "food" #"o" (re-quote-replacement
/// "$"))` measures as `"f$$d"`, but `Regex::replace_all(s, "\$")` left
/// the backslash in, giving `"f\$\$d"`.
fn append_replacement(caps: &fancy_regex::Captures<str>, template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '$' => {
                let mut digits = String::new();
                while let Some(d) = chars.peek() {
                    if d.is_ascii_digit() {
                        digits.push(*d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if digits.is_empty() {
                    out.push('$');
                } else if let Ok(n) = digits.parse::<usize>() {
                    if let Some(m) = caps.get(n) {
                        out.push_str(m.as_str());
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `clojure.string/replace` and `replace-first`'s shared body: `first_only`
/// picks `replacen(.., 1, ..)` vs `replace_all`/one-shot splice. Four
/// pattern shapes, all measured against `.oracle/clojure-src`'s own
/// `clojure.string/replace`/`replace-first` (`string.clj`'s `cond` over
/// `(instance? Character match)` / `CharSequence` / `Pattern`):
///
/// - `Value::Char`: `replace` requires a `Char` replacement too (real
///   Clojure reflectively calls `String.replace(char, char)`, which has no
///   overload for anything else); `replace-first` stringifies WHATEVER the
///   replacement is via `display_str` (real `replace-first-char` builds
///   its result with plain `str`, no type restriction) and splices at the
///   first occurrence only.
/// - `Value::Regex` + a CharSequence replacement: hands the replacement
///   string straight to the `regex` crate's own `$1`/`${1}` group-
///   reference syntax (R3 spec: close enough to `Matcher.replaceAll`'s
///   `$1` for this corpus).
/// - `Value::Regex` + anything else (a function): calls it once per match
///   with `re-groups`' own shape -- the whole-match STRING if the pattern
///   has no explicit capture groups, else a `[whole g1 g2 ...]` vector
///   (unmatched groups are `nil`, mirroring a null `Matcher.group(n)`) --
///   and splices the (literal, unexpanded) string it returns back in,
///   mirroring `replace-by`'s `Matcher/quoteReplacement` + `appendReplacement`
///   pairing (the fn's return value is NEVER re-interpreted for `$1`-style
///   backreferences, unlike the CharSequence-replacement arm above).
/// - A CharSequence pattern (`Value::Str` or a `StringBuilder`/
///   `StringBuffer` [`char_seq_str`]): `Str::replace_literal` (M8.1,
///   rope-native for a `Rope` haystack, never materializes).
fn replace_impl(interp: &mut Interp, args: &[Value], op: &'static str, first_only: bool) -> Result<Value, RjError> {
    // MOVA-PATCH: real `replace`/`replace-first` call `.toString` on `s` up front -- lenient.
    let s = char_seq_str_lenient(interp, &args[0], op)?;
    match &args[1] {
        Value::Char(pat_c) => {
            if first_only {
                let repl = crate::printer::display_str(&args[2]);
                let hay = s.as_ref();
                match hay.find(*pat_c) {
                    None => Ok(Value::Str(s.clone())),
                    Some(byte_idx) => {
                        let mut out = String::with_capacity(hay.len() + repl.len());
                        out.push_str(&hay[..byte_idx]);
                        out.push_str(&repl);
                        out.push_str(&hay[byte_idx + pat_c.len_utf8()..]);
                        Ok(Value::Str(out.into()))
                    }
                }
            } else {
                let repl_c = match &args[2] {
                    Value::Char(c) => *c,
                    other => {
                        return Err(RjError::type_err(format!(
                            "{op}: expected a char replacement for a char pattern, got {}",
                            other.type_name()
                        )))
                    }
                };
                let out: String = s.as_ref().chars().map(|c| if c == *pat_c { repl_c } else { c }).collect();
                Ok(Value::Str(out.into()))
            }
        }
        Value::Regex(re) => {
            if is_char_seq(&args[2]) {
                let replacement = char_seq_str(&args[2], op)?;
                let template = replacement.as_ref().to_string();
                let do_one = |caps: &fancy_regex::Captures<str>| -> String { append_replacement(caps, &template) };
                let out = if first_only {
                    re.replacen(s.as_ref(), 1, do_one)
                } else {
                    re.replace_all(s.as_ref(), do_one)
                };
                Ok(Value::Str(out.into_owned().into()))
            } else {
                let f = args[2].clone();
                let mut call_err: Option<RjError> = None;
                let do_one = |caps: &fancy_regex::Captures<str>| -> String {
                    if call_err.is_some() {
                        return String::new();
                    }
                    let call_arg = if caps.len() <= 1 {
                        Value::Str(caps.get(0).map(|m| m.as_str()).unwrap_or("").into())
                    } else {
                        let groups: PVec = (0..caps.len())
                            .map(|idx| match caps.get(idx) {
                                Some(m) => Value::Str(m.as_str().into()),
                                None => Value::Nil,
                            })
                            .collect();
                        Value::Vector(groups)
                    };
                    match interp.call(&f, std::slice::from_ref(&call_arg)) {
                        Ok(Value::Str(out)) => out.to_string(),
                        Ok(other) => {
                            call_err = Some(RjError::type_err(format!(
                                "{op}: replacement fn must return a string, got {}",
                                other.type_name()
                            )));
                            String::new()
                        }
                        Err(e) => {
                            call_err = Some(e);
                            String::new()
                        }
                    }
                };
                let out = if first_only {
                    re.replacen(s.as_ref(), 1, do_one)
                } else {
                    re.replace_all(s.as_ref(), do_one)
                };
                if let Some(e) = call_err {
                    return Err(e);
                }
                Ok(Value::Str(out.into_owned().into()))
            }
        }
        other if is_char_seq(other) => {
            let pat = char_seq_str(other, op)?;
            let replacement = char_seq_str(&args[2], op)?;
            Ok(Value::Str(s.replace_literal(pat.as_ref(), replacement.as_ref(), first_only)))
        }
        other => Err(RjError::type_err(format!(
            "{op}: expected a string, char, or regex pattern, got {}",
            other.type_name()
        ))),
    }
}

/// `.indexOf`/`.lastIndexOf`'s needle arg. `String.indexOf` is
/// JVM-overloaded exactly two ways: `indexOf(String)` and `indexOf(int)`
/// (an `int` CODE POINT -- `(.indexOf s (int \a))` is idiomatic
/// Java-interop Clojure). Measured (`clojure -M -e`): a bare `Value::Char`
/// needle -- `(.indexOf "hello" \l)` -- THROWS
/// `IllegalArgumentException: No matching method indexOf found taking 1
/// args for class java.lang.String`, because Clojure's reflective dispatch
/// doesn't auto-unbox `Character` to `int` and `Character` isn't a
/// `CharSequence` either -- it matches NEITHER overload, unlike
/// [`pattern_string`] above (which backs the bare `index-of`/
/// `last-index-of` builtins and deliberately accepts a char as a
/// same-as-a-string convenience -- that leniency is specifically NOT part
/// of the JVM method's own overload set, so it's deliberately not
/// inherited here). Only `Value::Str` and `Value::Int` are accepted; a
/// `Value::Char` (or anything else) is a `bad_arg` error, matching the
/// real throw.
fn dot_needle(v: &Value, op: &str) -> Result<String, RjError> {
    match v {
        Value::Str(s) => Ok(s.to_string()),
        Value::Int(n) => match u32::try_from(*n).ok().and_then(char::from_u32) {
            Some(c) => Ok(c.to_string()),
            None => Err(RjError::type_err(format!("{op}: not a valid char code point: {n}"))),
        },
        other => Err(bad_arg(op, other)),
    }
}

/// Uniform "wrong argument type" error for [`str_dot_method`]'s arms below.
fn bad_arg(op: &str, got: &Value) -> RjError {
    RjError::type_err(format!("{op}: unexpected argument type {}", got.type_name()))
}

/// Uniform "wrong argument count" error for [`str_dot_method`]'s arms below.
fn bad_arity(op: &str, got: usize) -> RjError {
    RjError::arity(format!("{op}: wrong number of args ({got})"))
}

/// S6 (strdot): backs `eval::types_forms::eval_dot_form`'s `Value::Str`
/// receiver arm -- `(.startsWith s "a")`, `(.split s ",")`, etc. `s` is the
/// already-evaluated receiver; `args` is everything after the receiver in
/// `(.method receiver args...)`, already evaluated too (mirrors
/// `builtins::numbers::numeric_dot_method`'s convention one level up, minus
/// the "receiver only" restriction -- string methods need real args).
///
/// Delegates to the bare `clojure.string`-ish builtins above wherever their
/// semantics already match `java.lang.String`'s instance method of the same
/// name (`starts-with?`/`ends-with?`/`includes?`/`upper-case`/`lower-case`/
/// `trim`); reimplements the rest locally because the JVM shape genuinely
/// differs from the bare builtin's Clojure-idiomatic one. Every deviation
/// below is a MEASURED JVM behavior (real Clojure 1.13.0-alpha6 via
/// `clojure -M -e`, transcript in `compat/strdot-probe-transcript.txt`),
/// not a guess:
///
/// - `.indexOf`/`.lastIndexOf` return `-1` on no match, never `nil` --
///   the bare `index-of`/`last-index-of` builtins return `nil`.
/// - `.split`'s pattern arg is ALWAYS compiled as a regex, even a plain
///   string like `","` -- `String.split(String regex)` takes its arg AS a
///   regex source, unlike the bare `split` builtin's deliberate
///   literal-string arm for a `Value::Str` pattern (see this module's
///   header doc). Also returns a `Value::Array` (`String[]`, measured
///   `(class (.split "a,b" ","))` => `java.lang.String/1`), not a
///   `Value::Vector`.
/// - `.replace` is ALWAYS literal (`String.replace(CharSequence,
///   CharSequence)` -- Java's regex-flavored `.replaceAll`/`.replaceFirst`
///   are separate methods, not implemented here, no vendored call site).
///   Matches the bare `replace` builtin's own `Value::Str` arm exactly, so
///   this just forces that arm regardless of what the pattern arg looks
///   like.
/// - `.length`/`.isEmpty` count chars via `char_count_cached` (mova's
///   char-indexed `Str`), not UTF-16 code units -- identical to Java's
///   `.length()` for every BMP codepoint; a supplementary-plane (astral)
///   codepoint would count 1 here vs Java's 2. No vendored call site or
///   corpus form exercises that gap; flagged here rather than silently
///   assumed equivalent.
/// - `.charAt` returns a `Value::Char` (measured: `(class (.charAt s i))`
///   is `java.lang.Character`); out-of-range throws (message text
///   unchecked -- exception KIND is out of the conformance corpus's scope
///   per `CONFORMANCE-GUARANTEE.md` rule 6, and mova has one untyped
///   `catch` regardless, per `SHIM-LIMITS.md`).
/// - `.toString` returns the SAME string (measured:
///   `(identical? s (.toString s))` is `true` on the JVM; mova's `Str`
///   clone is a cheap ref-count bump either way, so `s.clone()` here is
///   the faithful analog without needing pointer-identity plumbing).
///
/// Returns `None` for a method name this doesn't recognize at all (the
/// caller turns that into an "unresolved method" error, matching real
/// Clojure's reflective-failure shape for a truly missing method);
/// `Some(Err(_))` for a recognized method called with the wrong arg
/// shape/count/type.
pub(crate) fn str_dot_method(field: &str, s: &Str, args: &[Value]) -> Option<Result<Value, RjError>> {
    let op = field;
    Some(match field {
        "startsWith" => {
            if args.len() != 1 {
                return Some(Err(bad_arity(op, args.len())));
            }
            match &args[0] {
                Value::Str(prefix) => Ok(Value::Bool(s.starts_with(prefix.as_ref()))),
                other => Err(bad_arg(op, other)),
            }
        }
        "endsWith" => {
            if args.len() != 1 {
                return Some(Err(bad_arity(op, args.len())));
            }
            match &args[0] {
                Value::Str(suffix) => Ok(Value::Bool(s.ends_with(suffix.as_ref()))),
                other => Err(bad_arg(op, other)),
            }
        }
        "contains" => {
            if args.len() != 1 {
                return Some(Err(bad_arity(op, args.len())));
            }
            match &args[0] {
                Value::Str(needle) => Ok(Value::Bool(s.contains(needle.as_ref()))),
                other => Err(bad_arg(op, other)),
            }
        }
        // S8 (mova campaign task 3): `.getBytes()` 0-arg overload -- JVM
        // default-charset overload, but every JDK's default charset is
        // UTF-8 in practice (and clojure-lsp/clj-kondo call sites --
        // `classpath.mova`'s md5, `config.mova`'s settings-hash, vendored
        // clj-kondo `core.clj:765` `(.getBytes (str cfg))` -- never pass a
        // Charset arg), so UTF-8 is the faithful byte source here. Bytes
        // come back as signed `byte` (`-128..=127`, matching real
        // `String.getBytes()`'s `byte[]` element type and this codebase's
        // own `vector-of :byte` convention in `builtins::sorted`).
        "getBytes" if args.is_empty() => Ok(Value::Array(Arc::new(ArrayVal {
            kind: ArrayKind::Byte,
            dims: 1,
            data: Mutex::new(s.as_bytes().iter().map(|b| Value::Int(*b as i8 as i64)).collect()),
        }))),
        "trim" if args.is_empty() => Ok(Value::Str(s.trim().into())),
        "toUpperCase" if args.is_empty() => Ok(Value::Str(s.to_uppercase().into())),
        "toLowerCase" if args.is_empty() => Ok(Value::Str(s.to_lowercase().into())),
        "isEmpty" if args.is_empty() => Ok(Value::Bool(s.char_count_cached() == 0)),
        "length" if args.is_empty() => Ok(Value::Int(s.char_count_cached() as i64)),
        "toString" if args.is_empty() => Ok(Value::Str(s.clone())),
        "trim" | "toUpperCase" | "toLowerCase" | "isEmpty" | "length" | "toString" => {
            Err(bad_arity(op, args.len()))
        }
        "concat" => {
            if args.len() != 1 {
                return Some(Err(bad_arity(op, args.len())));
            }
            match &args[0] {
                Value::Str(other) => {
                    let mut out = s.to_string();
                    out.push_str(other.as_ref());
                    Ok(Value::Str(out.into()))
                }
                other => Err(bad_arg(op, other)),
            }
        }
        "charAt" => {
            if args.len() != 1 {
                return Some(Err(bad_arity(op, args.len())));
            }
            match require_index(&args[0], op) {
                Ok(idx) => match s.char_at(idx) {
                    Some(c) => Ok(Value::Char(c)),
                    None => Err(RjError::other(format!("String index out of range: {idx}"))),
                },
                Err(e) => Err(e),
            }
        }
        "substring" => {
            if args.is_empty() || args.len() > 2 {
                return Some(Err(bad_arity(op, args.len())));
            }
            let total = s.char_count_cached();
            match require_index(&args[0], op) {
                Ok(start) => {
                    let end = match args.get(1) {
                        Some(v) => require_index(v, op),
                        None => Ok(total),
                    };
                    match end {
                        Ok(end) if start <= end && end <= total => {
                            Ok(Value::Str(s.char_slice(start..end)))
                        }
                        Ok(end) => Err(RjError::other(format!(
                            "String index out of range (start={start}, end={end}, len={total})"
                        ))),
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            }
        }
        "indexOf" => {
            if args.is_empty() || args.len() > 2 {
                return Some(Err(bad_arity(op, args.len())));
            }
            match dot_needle(&args[0], op) {
                Ok(needle) => {
                    let from = match args.get(1) {
                        Some(Value::Int(n)) => (*n).max(0) as usize,
                        Some(other) => return Some(Err(bad_arg(op, other))),
                        None => 0,
                    };
                    Ok(Value::Int(char_find(s, &needle, from).map(|i| i as i64).unwrap_or(-1)))
                }
                Err(e) => Err(e),
            }
        }
        "lastIndexOf" => {
            if args.is_empty() || args.len() > 2 {
                return Some(Err(bad_arity(op, args.len())));
            }
            match dot_needle(&args[0], op) {
                Ok(needle) => {
                    let from = match args.get(1) {
                        Some(Value::Int(n)) => (*n).max(0) as usize,
                        Some(other) => return Some(Err(bad_arg(op, other))),
                        None => usize::MAX,
                    };
                    Ok(Value::Int(char_rfind(s, &needle, from).map(|i| i as i64).unwrap_or(-1)))
                }
                Err(e) => Err(e),
            }
        }
        "replace" => {
            if args.len() != 2 {
                return Some(Err(bad_arity(op, args.len())));
            }
            match (&args[0], &args[1]) {
                (Value::Str(pat), Value::Str(replacement)) => {
                    Ok(Value::Str(s.replace_literal(pat.as_ref(), replacement.as_ref(), false)))
                }
                (other, _) => Err(bad_arg(op, other)),
            }
        }
        // D5: `.replaceFirst`/`.replaceAll` -- unlike `.replace` directly
        // above, these two take a REGEX pattern (as a String -- that is
        // `java.lang.String`'s own signature, `replaceFirst(String regex,
        // String replacement)`, not a `Pattern`). The vendored
        // `clojure.pprint` uses `(.replaceFirst s "\\s+$" "")` to trim
        // trailing whitespace off a line before emitting it, in both
        // `pretty_writer.clj` and `dispatch.clj`.
        //
        // The replacement string keeps Java's `$1` group-reference
        // syntax, which is `regex::Regex::replace`'s syntax too, so it is
        // passed through rather than escaped. Deliberately does NOT
        // accept a compiled `Value::Regex`: neither does the JVM method
        // (see the measured note on `.split` just below).
        "replaceFirst" | "replaceAll" => {
            if args.len() != 2 {
                return Some(Err(bad_arity(op, args.len())));
            }
            let (Value::Str(pat), Value::Str(replacement)) = (&args[0], &args[1]) else {
                return Some(Err(bad_arg(op, &args[0])));
            };
            let re = match regex::Regex::new(pat.as_ref()) {
                Ok(re) => re,
                Err(e) => {
                    return Some(Err(RjError::type_err(format!(
                        "{op}: invalid regex pattern: {e}"
                    ))))
                }
            };
            let text = s.as_ref();
            let out = if field == "replaceFirst" {
                re.replacen(text, 1, replacement.as_ref())
            } else {
                re.replace_all(text, replacement.as_ref())
            };
            Ok(Value::Str(out.into_owned().into()))
        }
        "split" => {
            if args.is_empty() || args.len() > 2 {
                return Some(Err(bad_arity(op, args.len())));
            }
            let limit = match args.get(1) {
                Some(v) => match require_int(v, op) {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                },
                None => 0,
            };
            // Measured (real Clojure 1.13.0-alpha6): `String.split`'s
            // parameter type is `String`, NOT `Pattern` -- passing a
            // compiled `#"..."` regex literal here reflects into the
            // `split(String)` overload and throws `ClassCastException`
            // ("class java.util.regex.Pattern cannot be cast to class
            // java.lang.String") rather than being accepted as a pattern
            // object. Only a `Value::Str` is a valid arg; it is compiled
            // as a regex SOURCE at call time (unlike the bare `split`
            // builtin's literal-string arm -- see this fn's doc comment).
            let pat = match &args[0] {
                Value::Str(pat) => pat,
                other => return Some(Err(bad_arg(op, other))),
            };
            // D5: `fancy_regex`, not `regex` -- `.split`'s argument is a
            // USER pattern in `java.util.regex.Pattern` syntax, so it gets
            // the same engine `#"..."` literals do (see Cargo.toml's
            // `fancy-regex` note). The internal patterns elsewhere in this
            // module stay on `regex`.
            let re = match fancy_regex::Regex::new(pat.as_ref()) {
                Ok(r) => r,
                Err(e) => return Some(Err(RjError::type_err(format!("{op}: invalid regex pattern: {e}")))),
            };
            let parts: Vec<Value> = split_regex(&re, s, limit).into_iter().map(|p| Value::Str(p.into())).collect();
            Ok(Value::Array(Arc::new(ArrayVal {
                kind: ArrayKind::Object("java.lang.String"),
                dims: 1,
                data: Mutex::new(parts),
            })))
        }
        _ => return None,
    })
}

/// Re-binds an already-registered bare `name` under `ns/name` too. Goes
/// through `set_builtin` (not `set`): this runs during `register_all`,
/// aliasing an untouched Rust native under a second name, so the new cell
/// should start pristine exactly like the bare-name cell it mirrors.
pub(crate) fn alias(i: &mut Interp, ns: &str, name: &'static str) {
    if let Some(v) = i.globals.get(&Symbol::simple(name)) {
        i.globals.set_builtin_alias(
            Symbol {
                ns: Some(ns.into()),
                name: name.into(),
            },
            v,
            name,
        );
    }
}

/// [`reg`], but registered ONLY under `ns/name` -- never as a bare global.
/// For the rare `clojure.string` fn whose name COLLIDES with an unrelated
/// bare builtin that must keep its own meaning (`reverse`: the bare one is
/// the general sequence-reversing fn every non-string caller needs;
/// `clojure.string/reverse` is a different, string-only fn with a
/// different return type -- see that registration's own doc comment).
/// `reg`'s own arity-checking wrapper is duplicated here rather than
/// factored out through it, since `reg` always ALSO binds the bare name.
#[track_caller]
fn reg_ns(
    i: &mut Interp,
    ns: &'static str,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let native = crate::value::NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            return Err(RjError::arity(format!(
                "{name}: expected {}, got {}",
                arity.expected_desc(),
                args.len()
            ))
            .with_stack(interp.stack_snapshot(), interp.source_id));
        }
        f(interp, args)
    });
    i.globals.set_builtin(
        Symbol {
            ns: Some(ns.into()),
            name: name.into(),
        },
        Value::Native(Arc::new(native)),
    );
}

/// M4b: `*out*`-aware output sink shared by `print`/`println`/`pr`/`prn`.
/// Design (SPEC, decided): read `*out*`'s CURRENT value -- `Env::get` on a
/// `Binding::Var` cell already goes through `VarCell::get`, so this
/// automatically picks up the innermost `binding` frame on THIS thread,
/// exactly like every other dynamic-var read in mova, with no special-
/// casing here. If that value is a `Value::Atom`, APPEND `s` to its string
/// content (this is what `with-out-str`, a `core.mova` macro binding
/// `*out*` to a fresh `(atom "")`, captures); otherwise (nil, unbound, or
/// any non-atom value someone `binding`s `*out*` to) fall through to real
/// stdout, unchanged from pre-M4b behavior. The atom write bumps its
/// version counter (`value.rs`'s `Value::Atom` doc) even though nothing
/// here can race with a reentrant compute fn -- this is a plain string
/// concat under one held lock, not `swap!`'s CAS loop -- purely to keep
/// the "every mutation of `.1` bumps `.0`" invariant that loop relies on.
///
/// D5 adds ONE more sink shape, and deliberately only one: a
/// `Value::Inst` whose type declares a `write` method. That is the
/// narrowest possible bridge for a WRITER OBJECT bound to `*out*`, which
/// is how the vendored `clojure.pprint` works -- its `pprint`/`cl-format`
/// entry points `binding` `*out*` to a `(proxy [Writer IDeref ...] ...)`
/// they built (a column writer wrapping a pretty writer wrapping the
/// caller's original `*out*`), and then call ordinary `print`/`pr`/`prn`
/// and expect the characters to arrive at the proxy's `write` override
/// rather than at stdout. Without this branch, everything the pretty
/// printer emits would bypass its own buffering machinery entirely and
/// the whole library would be inert.
///
/// The bridge is exactly `(.write out s)` with the WHOLE string in one
/// call -- never a char at a time. That is what real `clojure.core/pr`
/// does on the JVM too (`(.write ^Writer *out* (str x))`), and it is the
/// arity every vendored `write` override implements as its String case,
/// so no chunking policy has to be invented here. Anything else bound to
/// `*out*` (nil, unbound, a non-writer value) keeps falling through to
/// real stdout, unchanged.
///
/// Now fallible, because calling into interpreted code can throw and
/// swallowing a writer's error would be a silent correctness hole: an
/// `is`-assertion comparing captured output must see the throw, not a
/// truncated string.
pub(crate) fn out_write(interp: &mut Interp, s: &str) -> Result<(), RjError> {
    out_write_impl(interp, s, false)
}

thread_local! {
    /// nREPL: true only while `out_write` makes its own implicit flush of a
    /// stream (see [`in_auto_flush`]).
    static AUTO_FLUSH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// True while `out_write` flushes a stream on its own, after a plain
/// `print`/`pr`. A stream writer that must tell that apart from a real
/// `(flush)` / `.flush` call (the nREPL `out` sink: `print` output waits for
/// the end of the form, `println` and `(flush)` send it at once) asks this.
pub fn in_auto_flush() -> bool {
    AUTO_FLUSH.with(|c| c.get())
}

/// `out_write`, plus `explicit_flush`: `println`/`prn`/`newline` end with a
/// real flush of `*out*` (Clojure: `*flush-on-newline*`); `print`/`pr` do not.
/// Only a stream sink (`HostKind::OutputStream`) can tell the two apart.
pub(crate) fn out_write_flushing(interp: &mut Interp, s: &str) -> Result<(), RjError> {
    out_write_impl(interp, s, true)
}

/// A text sink that `print`/`println` may write to directly when `*out*` is
/// the very stream that sink made (the nREPL `out` sink). See [`set_fast_out`].
pub(crate) trait FastOut: Send + Sync {
    /// Same as writing `s` to the `*out*` stream, then flushing it when `flush`.
    fn write_flush(&self, s: &str, flush: bool);
}

type FastOutEntry = (usize, std::sync::Arc<dyn FastOut>);

/// What `print`/`println` cache per thread: the cells of `*out*` and
/// `*print-dup*` (K7b style, keyed on the globals env and its root map), and
/// the session sink to write to directly.
struct PrintFast {
    out_sink: Option<FastOutEntry>,
    cells: Option<(crate::env::Env, usize, std::sync::Arc<crate::env::VarCell>, Option<std::sync::Arc<crate::env::VarCell>>)>,
    scratch: String,
}

thread_local! {
    static PRINT_FAST: std::cell::RefCell<PrintFast> = const {
        std::cell::RefCell::new(PrintFast { out_sink: None, cells: None, scratch: String::new() })
    };
}

/// nREPL: the calling thread's `*out*` stream is `stream` (the address of its
/// `HostInstVal`) and `sink` is what it writes to. While set, `print` /
/// `println` of plain values skip the generic stream path (two stream locks,
/// a `Vec<String>`, a `format!`, five var lookups) and append to the sink
/// directly. Any other `*out*` (a `binding`, `with-out-str`) takes the
/// generic path, so behavior is the same.
pub(crate) fn set_fast_out(entry: Option<FastOutEntry>) {
    PRINT_FAST.with(|f| f.borrow_mut().out_sink = entry);
}

/// `print` (`newline` false) or `println` (true) of plain values straight into
/// the nREPL sink. `false` = not applicable, use the generic path.
fn fast_print(interp: &Interp, args: &[Value], newline: bool) -> bool {
    use std::fmt::Write as _;
    // Only values whose text never depends on `*print-length*`, `*print-level*`,
    // `*print-meta*`, namespace maps ...; `*print-dup*` is checked below.
    if !args.iter().all(|a| matches!(a, Value::Str(_) | Value::Int(_) | Value::Nil | Value::Bool(_) | Value::Keyword(_) | Value::Char(_))) {
        return false;
    }
    PRINT_FAST.with(|f| {
        let Ok(mut f) = f.try_borrow_mut() else { return false };
        if f.out_sink.is_none() {
            return false;
        }
        let Some(id) = interp.globals.root_map_id() else { return false };
        let hit = matches!(&f.cells, Some((e, k, _, _)) if *k == id && crate::env::Env::ptr_eq(e, &interp.globals));
        if !hit {
            let Some(out) = interp.globals.root_cell(&Symbol::simple("*out*")) else { return false };
            let dup = interp.globals.root_cell(&Symbol::simple("*print-dup*"));
            f.cells = Some((interp.globals.clone(), id, out, dup));
        }
        let f = &mut *f;
        let Some((_, _, out_cell, dup_cell)) = &f.cells else { return false };
        if dup_cell.as_ref().and_then(|c| c.get()).is_some_and(|v| v.truthy()) {
            return false;
        }
        let Some(Value::HostInst(h)) = out_cell.get() else { return false };
        let Some((ptr, sink)) = &f.out_sink else { return false };
        if std::sync::Arc::as_ptr(&h) as usize != *ptr {
            return false;
        }
        f.scratch.clear();
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                f.scratch.push(' ');
            }
            match a {
                Value::Str(s) => s.write_into(&mut f.scratch),
                Value::Int(n) => {
                    let _ = write!(f.scratch, "{n}");
                }
                Value::Nil => f.scratch.push_str("nil"),
                Value::Bool(b) => f.scratch.push_str(if *b { "true" } else { "false" }),
                Value::Keyword(k) => {
                    f.scratch.push(':');
                    f.scratch.push_str(k);
                }
                Value::Char(c) => f.scratch.push(*c),
                _ => unreachable!(),
            }
        }
        if newline {
            f.scratch.push('\n');
        }
        sink.write_flush(&f.scratch, newline);
        if f.scratch.capacity() > 64 << 10 {
            f.scratch = String::new();
        }
        true
    })
}

fn out_write_impl(interp: &mut Interp, s: &str, explicit_flush: bool) -> Result<(), RjError> {
    match interp.globals.get(&Symbol::simple("*out*")) {
        Some(Value::Atom(cell)) => {
            let mut guard = crate::sync::lock_mutex(&cell.state);
            let mut appended = match &guard.1 {
                Value::Str(existing) => existing.to_string(),
                _ => String::new(),
            };
            appended.push_str(s);
            guard.0 = guard.0.wrapping_add(1);
            guard.1 = Value::Str(Str::from(appended));
        }
        Some(out @ Value::Inst(_)) => {
            let Value::Inst(inst) = &out else { unreachable!() };
            match crate::builtins::types::lookup_interface_method(&interp.interfaces, inst, "write")
            {
                Some(f) => {
                    let arg = Value::Str(Str::from(s));
                    interp.apply_value(&f, &[out.clone(), arg], crate::reader::Span { start: 0, end: 0 })?;
                }
                // An `Inst` with no `write` method is not a writer; treat
                // it like any other non-writer `*out*` value.
                None => print!("{s}"),
            }
        }
        // W-EMBED follow-up: `*out*`/`*err*` bound to a REAL native
        // stream (`System/out`/`System/err`, or any `HostKind::
        // OutputStream` -- a real file, a `java.io.FileWriter`, ...).
        // Without this arm the write fell through to the final
        // `_ => print!` stdout fallback below regardless -- the
        // reported bug: `(binding [*out* *err*] (println "x"))` kept
        // printing to stdout because `*err*`'s value (a `HostInst`, not
        // an `Atom` or an interpreted `Inst`) matched nothing above.
        // Flushed immediately (these are `BufWriter`s) so redirected
        // output is visible right away.
        Some(Value::HostInst(h)) if h.kind == crate::hostclass::HostKind::OutputStream => {
            crate::hostclass::stream_write_str(&h, s)?;
            if explicit_flush {
                let _ = crate::hostclass::stream_flush(&h);
            } else {
                AUTO_FLUSH.with(|c| c.set(true));
                let _ = crate::hostclass::stream_flush(&h);
                AUTO_FLUSH.with(|c| c.set(false));
            }
        }
        _ => print!("{s}"),
    }
    Ok(())
}

/// Deep-realizes every arg (see `Interp::realize_deep`) before `pr_str`ing
/// it, so lazy seqs print their realized elements (`(2 3 4)`) instead of
/// `#<lazy-seq>`.
pub(crate) fn realize_all_pr_str(interp: &mut Interp, args: &[Value]) -> Result<Vec<String>, RjError> {
    // S5/M3: THE boundary where `*print-meta*` is read. `pr_str` itself
    // is a free `&Value -> String` fn with no `Interp` in scope (it's
    // called from `Debug for Value`, from error messages, from a dozen
    // other builtins), so the var is looked up once here -- exactly like
    // `out_write` looks up `*out*` -- and parked in a thread-local for
    // the duration. See `printer::PRINT_META`'s doc for why that's the
    // shape rather than an extra parameter. The guard restores the
    // previous value on drop, including on the `?` early return below.
    let _guard = crate::printer::print_meta_scope(print_meta_on(interp));
    let _dup_guard = crate::printer::print_dup_scope(
        dynamic_var_on(interp, "*print-dup*"),
        dynamic_var_on(interp, "*verbose-defrecords*"),
    );
    // D5: `*print-namespace-maps*`, read at the same one boundary and
    // scoped the same way -- see `printer::PRINT_NAMESPACE_MAPS`.
    let _ns_maps_guard =
        crate::printer::print_ns_maps_scope(dynamic_var_on(interp, "*print-namespace-maps*"));
    // W3b: `*print-length*`/`*print-level*`, same one boundary -- see
    // `printer::PRINT_LENGTH`/`PRINT_LEVEL`.
    let _limits_guard = crate::printer::print_limits_scope(
        dynamic_var_opt_int(interp, "*print-length*"),
        dynamic_var_opt_int(interp, "*print-level*"),
    );
    // `*print-readably*` false: `pr` prints like `print`.
    let readably = interp
        .globals
        .get(&Symbol::simple("*print-readably*"))
        .is_none_or(|v| v.truthy());
    args.iter()
        .map(|a| {
            let v = interp.realize_deep(a)?;
            Ok(if readably { crate::printer::pr_str(&v) } else { crate::printer::print_family_str(&v) })
        })
        .collect()
}

/// Reads `*print-meta*`'s CURRENT value (innermost `binding` frame on
/// this thread, via the same `globals.get` -> `VarCell::get` path
/// `out_write` uses for `*out*`). Unbound or `nil`/`false` -> `false`,
/// which is Clojure's own default.
fn print_meta_on(interp: &mut Interp) -> bool {
    dynamic_var_on(interp, "*print-meta*")
}

/// Reads any bare-boolean-ish dynamic var's CURRENT value by name, same
/// "innermost `binding` frame on this thread" path `print_meta_on` uses.
/// Unbound/`nil`/`false` -> `false`.
fn dynamic_var_on(interp: &mut Interp, name: &str) -> bool {
    interp
        .globals
        .get(&Symbol::simple(name))
        .is_some_and(|v| v.truthy())
}

/// W3b: reads a dynamic var expected to hold either `nil` (Clojure's own
/// default for `*print-length*`/`*print-level*`, meaning "unlimited") or a
/// plain integer, same "innermost `binding` frame on this thread" path
/// `dynamic_var_on` uses. Unbound, `nil`, or anything that isn't a
/// `Value::Int` -> `None` (unlimited) -- `printer.clj`'s deftests only
/// ever bind these two vars to a small non-negative `Value::Int`, and
/// mova has no separate bignum-vs-fixnum split here worth tracking.
fn dynamic_var_opt_int(interp: &mut Interp, name: &str) -> Option<i64> {
    match interp.globals.get(&Symbol::simple(name)) {
        Some(Value::Int(n)) => Some(n),
        _ => None,
    }
}

/// Wave-C small sweep item 4: `print`/`println`'s own form, parallel to
/// [`realize_all_pr_str`] above but routing through `printer::
/// print_family_str` instead of `pr_str` -- unlike `str` (`str_concat`,
/// below, which calls `printer::display_str` directly per-piece), nested
/// collection elements stay non-readable too, all the way down (measured:
/// `(println ["a" "b"])` -> `[a b]`, not `["a" "b"]`). See `printer::
/// DISPLAY_PROMOTES_ELEMENTS`'s doc for exactly how the two forms differ
/// on a collection argument.
fn realize_all_print_family_str(interp: &mut Interp, args: &[Value]) -> Result<Vec<String>, RjError> {
    // W3b: `*print-length*`/`*print-level*` apply to the whole `pr`/
    // `print` family, not just `pr-str` -- `print-str` (which
    // `printer.clj`'s deftests actually call) is `core.mova`'s
    // `(with-out-str (apply print xs))`, routing through THIS function.
    let _limits_guard = crate::printer::print_limits_scope(
        dynamic_var_opt_int(interp, "*print-length*"),
        dynamic_var_opt_int(interp, "*print-level*"),
    );
    // W3b (item 2): `*print-dup*`/`*verbose-defrecords*`, same one
    // boundary -- `printer.clj`'s `print-dup-expected`/`print-dup-
    // readable` deftests call `print-str` (this function), not `pr-str`,
    // and `printer::write_value`'s own `*print-dup*` check (forcing
    // readable output) needs this thread-local actually set for that
    // family too, not just `realize_all_pr_str`'s.
    let _dup_guard = crate::printer::print_dup_scope(
        dynamic_var_on(interp, "*print-dup*"),
        dynamic_var_on(interp, "*verbose-defrecords*"),
    );
    args.iter()
        .map(|a| Ok(crate::printer::print_family_str(&interp.realize_deep(a)?)))
        .collect()
}

/// One `str` argument, realized: either a genuine `Value::Str` (kept as
/// such, so a big `Rope` piece never gets flattened just to be re-joined)
/// or anything else's `display_str` (already a plain owned `String` --
/// nothing further to preserve representation-wise).
enum StrPiece {
    S(Str),
    Owned(String),
}

/// `str`'s N-ary concatenation. M8: this is THE editor splice path --
/// omawritejank's `oma.core.edit/insert` (host-source-unchanged, not
/// touched by this integration) builds every keystroke's new
/// `:editor/text` as `(str (subs text 0 from) s (subs text to))`, so this
/// function is exactly as hot as `subs` itself.
///
/// Below `STR_ROPE_MIN` total bytes (and no piece already `Rope`, per the
/// ratchet -- ANY `Rope` piece forces the rope-native path below
/// regardless of the total, same "stays big once big" policy as
/// `PVec`/`PMap`), this is the pre-M8 flat-`String`-builder path
/// unchanged: cheapest for the overwhelmingly common case of small `str`
/// calls (symbol building, error messages, etc.).
///
/// At/above `STR_ROPE_MIN` (or with any `Rope` piece present), building
/// the result by materializing every piece into one flat buffer would be
/// exactly the O(document size) per-keystroke cost this whole milestone
/// exists to remove -- instead, each piece contributes its own `PText`
/// (an O(1) `PText::clone` for an already-`Rope` piece, e.g. `subs`'s
/// result; a cheap small-leaf build via `PText::from` for a `Flat` piece,
/// e.g. the one freshly-typed char) and the pieces are joined with
/// `PText::concat`, `O(log n)` per join. For the `(str (subs text 0
/// from) s (subs text to))` shape specifically, this composes into
/// exactly the `O(log n)` cost a direct `PText::splice` would have paid --
/// `subs`'s two `Str::char_slice` calls are themselves `O(log n)`
/// (`PText::slice`), and the two `PText::concat` joins here are each
/// `O(log n)` too, with no full-document copy anywhere in the chain.
fn str_concat(interp: &mut Interp, args: &[Value]) -> Result<Str, RjError> {
    let mut pieces: Vec<StrPiece> = Vec::with_capacity(args.len());
    let mut total_bytes = 0usize;
    let mut any_rope = false;
    for a in args {
        if matches!(a, Value::Nil) {
            continue;
        }
        let realized = interp.realize_deep(a)?;
        if let Value::Str(s) = realized {
            total_bytes += s.byte_len();
            any_rope |= s.is_rope();
            pieces.push(StrPiece::S(s));
        } else if let Some(overridden) = crate::builtins::types::inst_to_string_override(interp, &realized) {
            // clojure-lsp campaign (mova/PLAN.md): `str` is `.toString()`
            // on the JVM -- a `defrecord`/`deftype` that overrides it
            // must be honored here, ahead of mova's generic record
            // printer (`display_str`'s `Value::Inst` arm), same as real
            // Clojure. See `inst_to_string_override`'s doc.
            let d = match overridden? {
                Value::Str(s) => s.to_string(),
                other => crate::printer::display_str(&other),
            };
            total_bytes += d.len();
            pieces.push(StrPiece::Owned(d));
        } else {
            let d = crate::printer::display_str(&realized);
            total_bytes += d.len();
            pieces.push(StrPiece::Owned(d));
        }
    }
    if pieces.is_empty() {
        return Ok(Str::from(""));
    }
    if !any_rope && total_bytes < STR_ROPE_MIN {
        let mut s = String::with_capacity(total_bytes);
        for p in &pieces {
            match p {
                StrPiece::S(sv) => s.push_str(sv),
                StrPiece::Owned(d) => s.push_str(d),
            }
        }
        return Ok(Str::from(s));
    }
    let mut acc: Option<champ::PText> = None;
    for p in pieces {
        let piece_rope = match p {
            StrPiece::S(sv) => match sv.as_rope() {
                Some(r) => r.clone(),
                None => champ::PText::from(&*sv),
            },
            StrPiece::Owned(d) => champ::PText::from(d.as_str()),
        };
        acc = Some(match acc {
            None => piece_rope,
            Some(a) => champ::PText::concat(a, piece_rope),
        });
    }
    Ok(Str::wrap_rope(acc.expect("pieces checked non-empty above")))
}

/// S8 (mova campaign task 2): a real `java.util.Formatter` implementation
/// -- argument index (`%N$`), flags `-#+ 0,(`, width, precision, and
/// conversions `d o x X e E f g G s S c C b B h H % n`. Grew out of the
/// earlier "measured subset" (`%s`/`%d`/plain width/`-`) that blocked
/// `clj-kondo.impl.core/config-hash` (`%032x` on a `BigInteger` digest)
/// and `clojure-lsp.shared/format-time-delta-ms` (`%.0f`) -- both now go
/// through this directly, no overlay workaround. Scoped to what real
/// `Value`s this codebase has (`Int`=`long`, `BigInt`/`BigInteger`,
/// `Float`=`double`, `Str`, `Char`, `Bool`, `Nil`), not full JVM `Object`
/// dispatch (no user-`Formattable`, no `Locale` variance -- `,`
/// grouping is always `,`, decimal point always `.`, matching the ROOT
/// locale Java defaults to when none is passed, which is every call site
/// in this corpus).
///
/// `%s`/`%S` are `String.valueOf`, NOT `clojure.core/str` -- measured,
/// `(format "%s" nil)` is `"null"` where `(str nil)` is `""`.
///
/// `%f`/`%e`/`%g` round HALF_UP (`format_fixed`'s doc), matching real
/// `Formatter` (which formats via `BigDecimal` with `RoundingMode.
/// HALF_UP`), NOT Rust's own default binary-tie-to-even rounding -- this
/// is exactly the discrepancy that made `(format "%.0f" 2.5)` a silent
/// correctness bug rather than a missing-directive error if implemented
/// naively on top of `f64::round`/`{:.N}` alone.
fn format_impl(interp: &mut Interp, fmt: &str, args: &[Value]) -> Result<String, RjError> {
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars().peekable();
    let mut arg_idx = 0usize; // next IMPLICIT arg index (0-based)
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // `%N$...` explicit argument index -- lookahead: digits followed
        // by `$` (not just any digits, which would be width).
        let mut explicit_index: Option<usize> = None;
        {
            let mut lookahead = chars.clone();
            let mut digits = String::new();
            while let Some(d) = lookahead.peek().and_then(|c| c.to_digit(10)) {
                digits.push(char::from_digit(d, 10).unwrap());
                lookahead.next();
            }
            if !digits.is_empty() && lookahead.peek() == Some(&'$') {
                lookahead.next();
                explicit_index = digits.parse::<usize>().ok();
                chars = lookahead;
            }
        }
        // Flags: any combination of `- # + <space> 0 , (`.
        let mut left_justify = false;
        let mut flag_hash = false;
        let mut flag_plus = false;
        let mut flag_space = false;
        let mut flag_zero = false;
        let mut flag_comma = false;
        let mut flag_paren = false;
        loop {
            match chars.peek() {
                Some('-') => {
                    left_justify = true;
                    chars.next();
                }
                Some('#') => {
                    flag_hash = true;
                    chars.next();
                }
                Some('+') => {
                    flag_plus = true;
                    chars.next();
                }
                Some(' ') => {
                    flag_space = true;
                    chars.next();
                }
                Some('0') => {
                    flag_zero = true;
                    chars.next();
                }
                Some(',') => {
                    flag_comma = true;
                    chars.next();
                }
                Some('(') => {
                    flag_paren = true;
                    chars.next();
                }
                _ => break,
            }
        }
        let mut width: Option<usize> = None;
        {
            let mut w = 0usize;
            let mut any = false;
            while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                chars.next();
                w = w * 10 + d as usize;
                any = true;
            }
            if any {
                width = Some(w);
            }
        }
        let mut precision: Option<usize> = None;
        if chars.peek() == Some(&'.') {
            chars.next();
            let mut p = 0usize;
            while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                chars.next();
                p = p * 10 + d as usize;
            }
            precision = Some(p);
        }
        let Some(&directive) = chars.peek() else {
            return Err(RjError::other("format: dangling % at end of format string".to_string()));
        };
        chars.next();

        // `%%`/`%n` never consume an argument.
        if directive == '%' {
            out.push('%');
            continue;
        }
        if directive == 'n' {
            out.push('\n');
            continue;
        }

        let v: Value = if let Some(idx1) = explicit_index {
            match args.get(idx1.wrapping_sub(1)) {
                Some(v) if idx1 >= 1 => v.clone(),
                _ => {
                    return Err(RjError::other(format!(
                        "format: missing argument for index {idx1}$ (directive %{directive})"
                    )))
                }
            }
        } else {
            let Some(v) = args.get(arg_idx) else {
                return Err(RjError::other(format!(
                    "format: missing argument for %{directive} (directive #{})",
                    arg_idx + 1
                )));
            };
            arg_idx += 1;
            v.clone()
        };

        let piece = format_one(interp, directive, &v, flag_hash, flag_plus, flag_space, flag_zero, flag_comma, flag_paren, precision)?;
        pad_into(&mut out, &piece, width, left_justify, flag_zero && matches!(directive, 'd'|'o'|'x'|'X'|'e'|'E'|'f'|'g'|'G'));
    }
    Ok(out)
}

/// Space- or zero-pads `piece` to `width` (char count) and appends it to
/// `out`. Zero-padding a NUMERIC directive inserts the zeros AFTER any
/// leading sign/prefix (`format_one` already put the sign/prefix at the
/// front of `piece`, so this scans for the first digit-or-letter-after-
/// sign boundary -- in practice: skip a leading `-`/`+`/` `/`(` and an
/// optional `0x`/`0X` right after it).
fn pad_into(out: &mut String, piece: &str, width: Option<usize>, left_justify: bool, zero_numeric: bool) {
    let Some(width) = width else {
        out.push_str(piece);
        return;
    };
    let len = piece.chars().count();
    if len >= width {
        out.push_str(piece);
        return;
    }
    let pad = width - len;
    if left_justify {
        out.push_str(piece);
        out.extend(std::iter::repeat_n(' ', pad));
    } else if zero_numeric {
        let mut it = piece.chars().peekable();
        let mut consumed = String::new();
        if matches!(it.peek(), Some('-') | Some('+') | Some(' ') | Some('(')) {
            consumed.push(it.next().unwrap());
        }
        // `0x`/`0X` hex prefix right after an optional sign.
        let rest: String = it.clone().collect();
        if rest.starts_with("0x") || rest.starts_with("0X") {
            consumed.push_str(&rest[..2]);
            for _ in 0..2 {
                it.next();
            }
        }
        let remainder: String = it.collect();
        out.push_str(&consumed);
        out.extend(std::iter::repeat_n('0', pad));
        out.push_str(&remainder);
    } else {
        out.extend(std::iter::repeat_n(' ', pad));
        out.push_str(piece);
    }
}

/// Renders ONE directive's argument (no width/left-justify padding --
/// that's `pad_into`'s job, applied uniformly to every conversion's
/// output). `precision` means "decimal places" for `f`, "significant
/// digits" for `e`/`g`, "max chars" for `s`, and is invalid (ignored) for
/// everything else, matching `java.util.Formatter`'s own per-conversion
/// precision meaning.
#[allow(clippy::too_many_arguments)]
fn format_one(
    interp: &mut Interp,
    directive: char,
    v: &Value,
    flag_hash: bool,
    flag_plus: bool,
    flag_space: bool,
    flag_zero: bool,
    flag_comma: bool,
    flag_paren: bool,
    precision: Option<usize>,
) -> Result<String, RjError> {
    let _ = flag_zero; // zero-padding is applied by `pad_into`, not here
    match directive {
        's' | 'S' => {
            let mut s = if matches!(v, Value::Nil) {
                "null".to_string()
            } else {
                let realized = interp.realize_deep(v)?;
                crate::printer::display_str(&realized)
            };
            if let Some(p) = precision {
                if s.chars().count() > p {
                    s = s.chars().take(p).collect();
                }
            }
            if directive == 'S' {
                s = s.to_uppercase();
            }
            Ok(s)
        }
        'c' | 'C' => {
            let ch = match v {
                Value::Char(c) => *c,
                Value::Int(n) if (0..=0x10FFFF).contains(n) => {
                    char::from_u32(*n as u32).unwrap_or('\u{FFFD}')
                }
                other => {
                    return Err(RjError::type_err(format!(
                        "format: %c requires a char or codepoint int, got {}",
                        other.type_name()
                    )))
                }
            };
            let mut s = ch.to_string();
            if directive == 'C' {
                s = s.to_uppercase();
            }
            Ok(s)
        }
        'b' | 'B' => {
            let b = match v {
                Value::Nil => false,
                Value::Bool(b) => *b,
                _ => true,
            };
            let mut s = b.to_string();
            if directive == 'B' {
                s = s.to_uppercase();
            }
            Ok(s)
        }
        'h' | 'H' => {
            let s = if matches!(v, Value::Nil) {
                "null".to_string()
            } else {
                let realized = interp.realize_deep(v)?;
                let text = crate::printer::display_str(&realized);
                let mut hash: i32 = 0;
                // Java `String.hashCode`: s[0]*31^(n-1) + ... + s[n-1].
                for ch in text.chars() {
                    hash = hash.wrapping_mul(31).wrapping_add(ch as i32);
                }
                format!("{hash:x}")
            };
            Ok(if directive == 'H' { s.to_uppercase() } else { s })
        }
        'd' => format_integer(v, 10, flag_plus, flag_space, flag_comma, flag_paren, false, precision),
        'o' => format_integer(v, 8, flag_plus, flag_space, false, false, flag_hash, precision),
        'x' => format_integer(v, 16, flag_plus, flag_space, false, false, flag_hash, precision),
        'X' => format_integer(v, 16, flag_plus, flag_space, false, false, flag_hash, precision).map(|s| s.to_uppercase()),
        'f' => format_float_fixed(v, precision.unwrap_or(6), flag_plus, flag_space, flag_comma, flag_paren),
        'e' | 'E' => {
            let s = format_float_sci(v, precision.unwrap_or(6), flag_plus, flag_space, flag_paren)?;
            Ok(if directive == 'E' { s.to_uppercase() } else { s })
        }
        'g' | 'G' => {
            let s = format_float_general(v, precision.unwrap_or(6), flag_plus, flag_space, flag_comma, flag_paren)?;
            Ok(if directive == 'G' { s.to_uppercase() } else { s })
        }
        other => Err(RjError::other(format!(
            "format: unsupported directive %{other} (see format_impl's doc)"
        ))),
    }
}

/// Extracts `(negative, magnitude-as-i128-or-bigint-string)` for `%d`/
/// `%o`/`%x`/`%X`. `Value::Int` (mova's `long`) uses the JVM's own
/// unsigned-bit-pattern convention for `o`/`x` on a negative value (NOT
/// a `-` sign -- measured: `Long.toHexString(-1)` is
/// `"ffffffffffffffff"`, sixteen `f`s); `Value::BigInt`/`BigInteger` has
/// no fixed bit width, so a negative one keeps a real `-` sign with an
/// unsigned magnitude, for every radix including `o`/`x`.
#[allow(clippy::too_many_arguments)]
fn format_integer(
    v: &Value,
    radix: u32,
    flag_plus: bool,
    flag_space: bool,
    flag_comma: bool,
    flag_paren: bool,
    flag_hash: bool,
    precision: Option<usize>,
) -> Result<String, RjError> {
    if precision.is_some() {
        return Err(RjError::other("format: precision is not allowed for d/o/x/X".to_string()));
    }
    let (negative, magnitude): (bool, String) = match v {
        Value::Int(n) => {
            if radix == 10 {
                (*n < 0, (*n as i128).unsigned_abs().to_string())
            } else {
                let bits = *n as u64;
                (false, if radix == 16 { format!("{bits:x}") } else { format!("{bits:o}") })
            }
        }
        Value::BigInt(b) | Value::BigInteger(b) => {
            use num_bigint::Sign;
            let neg = b.0.sign() == Sign::Minus;
            let mag = b.0.magnitude().to_str_radix(radix);
            (neg, mag)
        }
        other => {
            return Err(RjError::type_err(format!(
                "format: %{} requires an integer, got {}",
                if radix == 10 { 'd' } else if radix == 8 { 'o' } else { 'x' },
                other.type_name()
            )))
        }
    };
    let digits = if radix == 10 && flag_comma { group_thousands(&magnitude) } else { magnitude };
    let prefix = if flag_hash && radix == 16 {
        "0x"
    } else if flag_hash && radix == 8 && digits != "0" {
        "0"
    } else {
        ""
    };
    let sign = if negative {
        if flag_paren { "(" } else { "-" }
    } else if flag_plus {
        "+"
    } else if flag_space {
        " "
    } else {
        ""
    };
    let suffix = if negative && flag_paren { ")" } else { "" };
    Ok(format!("{sign}{prefix}{digits}{suffix}"))
}

/// Inserts `,` every 3 digits from the right (grouping separator, `%,d`)
/// -- ASCII-decimal-only input (already-formatted magnitude digits), so
/// byte indexing is safe.
fn group_thousands(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        let from_end = bytes.len() - i;
        if i > 0 && from_end % 3 == 0 {
            out.push(b',' as u8);
        }
        out.push(*b);
    }
    String::from_utf8(out).unwrap()
}

fn as_f64(v: &Value) -> Result<f64, RjError> {
    match v {
        Value::Float(f) => Ok(*f),
        Value::Int(n) => Ok(*n as f64),
        Value::BigInt(b) | Value::BigInteger(b) => {
            use num_traits::ToPrimitive;
            b.0.to_f64().ok_or_else(|| RjError::type_err("format: BigInteger too large for float directive".to_string()))
        }
        other => Err(RjError::type_err(format!(
            "format: expected a floating-point argument, got {}",
            other.type_name()
        ))),
    }
}

/// HALF_UP rounding of `abs_value` to `precision` decimal places,
/// returning the unsigned digit string split at the decimal point (no
/// sign, no grouping -- callers add those). Rust's own `{:.N}` float
/// formatting rounds ties to EVEN (nearest-representable-value
/// rounding), where real `java.util.Formatter` explicitly documents
/// HALF_UP (`BigDecimal`-based) -- this is the exact discrepancy that
/// made a naive `%f` port silently diverge from the JVM oracle on exact
/// `.5` ties (`(format "%.0f" 2.5)` must be `"3"`, matching real
/// Clojure/Java, not `"2"`). Strategy: format with ONE extra digit
/// (Rust's rounding at that extra position is not a tie for any value
/// that was itself an exact decimal tie one digit up), then apply
/// HALF_UP by hand on that resolved last digit.
fn round_half_up_digits(abs_value: f64, precision: usize) -> (String, String) {
    let extra = format!("{:.*}", precision + 1, abs_value);
    let (int_part, frac_part) = extra.split_once('.').unwrap_or((extra.as_str(), ""));
    let mut digits: Vec<u8> = int_part.bytes().chain(frac_part.bytes()).map(|b| b - b'0').collect();
    let round_digit = digits.pop().unwrap_or(0);
    if round_digit >= 5 {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, 1);
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let frac_len = precision;
    let int_len = digits.len() - frac_len;
    let int_str: String = digits[..int_len].iter().map(|d| (d + b'0') as char).collect();
    let frac_str: String = digits[int_len..].iter().map(|d| (d + b'0') as char).collect();
    (int_str, frac_str)
}

fn sign_str(negative: bool, flag_plus: bool, flag_space: bool, flag_paren: bool) -> (&'static str, &'static str) {
    if negative {
        (if flag_paren { "(" } else { "-" }, if flag_paren { ")" } else { "" })
    } else if flag_plus {
        ("+", "")
    } else if flag_space {
        (" ", "")
    } else {
        ("", "")
    }
}

fn format_float_fixed(
    v: &Value,
    precision: usize,
    flag_plus: bool,
    flag_space: bool,
    flag_comma: bool,
    flag_paren: bool,
) -> Result<String, RjError> {
    let f = as_f64(v)?;
    if f.is_nan() {
        return Ok("NaN".to_string());
    }
    let negative = f.is_sign_negative() && f != 0.0;
    if f.is_infinite() {
        let (s, suf) = sign_str(negative, flag_plus, flag_space, flag_paren);
        return Ok(format!("{s}Infinity{suf}"));
    }
    let (int_str, frac_str) = round_half_up_digits(f.abs(), precision);
    let int_str = if flag_comma { group_thousands(&int_str) } else { int_str };
    let (s, suf) = sign_str(negative, flag_plus, flag_space, flag_paren);
    if precision > 0 {
        Ok(format!("{s}{int_str}.{frac_str}{suf}"))
    } else {
        Ok(format!("{s}{int_str}{suf}"))
    }
}

fn format_float_sci(
    v: &Value,
    precision: usize,
    flag_plus: bool,
    flag_space: bool,
    flag_paren: bool,
) -> Result<String, RjError> {
    let f = as_f64(v)?;
    if f.is_nan() {
        return Ok("NaN".to_string());
    }
    let negative = f.is_sign_negative() && f != 0.0;
    if f.is_infinite() {
        let (s, suf) = sign_str(negative, flag_plus, flag_space, flag_paren);
        return Ok(format!("{s}Infinity{suf}"));
    }
    let abs = f.abs();
    let (mantissa, exp) = if abs == 0.0 {
        (0.0, 0i32)
    } else {
        let exp = abs.log10().floor() as i32;
        let m = abs / 10f64.powi(exp);
        // Guard against log10 rounding putting mantissa outside [1,10).
        if m >= 10.0 {
            (m / 10.0, exp + 1)
        } else if m < 1.0 {
            (m * 10.0, exp - 1)
        } else {
            (m, exp)
        }
    };
    let (int_str, frac_str) = round_half_up_digits(mantissa, precision);
    // A HALF_UP round of the mantissa can carry into "10.xxx" -- bump
    // the exponent and re-split.
    let (int_str, exp) = if int_str == "10" { ("1".to_string(), exp + 1) } else { (int_str, exp) };
    let (s, suf) = sign_str(negative, flag_plus, flag_space, flag_paren);
    let exp_sign = if exp < 0 { "-" } else { "+" };
    let body = if precision > 0 {
        format!("{int_str}.{frac_str}")
    } else {
        int_str
    };
    Ok(format!("{s}{body}e{exp_sign}{:02}{suf}", exp.abs()))
}

/// Approximation of `%g`/`%G`: real `Formatter` picks fixed vs
/// scientific based on the value's decimal exponent vs `precision`
/// (`10^-4 <= |x| < 10^precision` -> fixed, else scientific), with
/// `precision` meaning TOTAL significant digits either way. No known
/// clojure-lsp/clj-kondo call site uses `%g` (only `%d`/`%x`/`%f` do,
/// per this campaign's census) -- implemented for completeness/no-crash
/// rather than measured against the oracle digit-for-digit.
fn format_float_general(
    v: &Value,
    precision: usize,
    flag_plus: bool,
    flag_space: bool,
    flag_comma: bool,
    flag_paren: bool,
) -> Result<String, RjError> {
    let f = as_f64(v)?;
    let precision = precision.max(1);
    if f == 0.0 || f.is_nan() || f.is_infinite() {
        return format_float_fixed(v, precision.saturating_sub(1), flag_plus, flag_space, flag_comma, flag_paren);
    }
    let exp = f.abs().log10().floor() as i32;
    if exp >= -4 && exp < precision as i32 {
        let decimals = (precision as i32 - 1 - exp).max(0) as usize;
        format_float_fixed(v, decimals, flag_plus, flag_space, flag_comma, flag_paren)
    } else {
        format_float_sci(v, precision.saturating_sub(1), flag_plus, flag_space, flag_paren)
    }
}

pub fn register(i: &mut Interp) {
    reg(i, "str", ArityHint::Any, |interp, args| Ok(Value::Str(str_concat(interp, args)?)));

    reg(i, "format", ArityHint::Min(1), |interp, args| {
        let Value::Str(fmt) = &args[0] else {
            return Err(RjError::type_err(format!(
                "format: expected a format string, got {}",
                args[0].type_name()
            )));
        };
        let fmt = fmt.to_string();
        let s = format_impl(interp, &fmt, &args[1..])?;
        Ok(Value::Str(s.into()))
    });

    reg(i, "pr-str", ArityHint::Any, |interp, args| {
        let s = realize_all_pr_str(interp, args)?.join(" ");
        Ok(Value::Str(s.into()))
    });

    reg(i, "println", ArityHint::Any, |interp, args| {
        if fast_print(interp, args, true) {
            return Ok(Value::Nil);
        }
        let s = realize_all_print_family_str(interp, args)?.join(" ");
        out_write_flushing(interp, &format!("{s}\n"))?;
        Ok(Value::Nil)
    });

    reg(i, "print", ArityHint::Any, |interp, args| {
        if fast_print(interp, args, false) {
            return Ok(Value::Nil);
        }
        let s = realize_all_print_family_str(interp, args)?.join(" ");
        out_write(interp, &s)?;
        Ok(Value::Nil)
    });

    reg(i, "prn", ArityHint::Any, |interp, args| {
        let s = realize_all_pr_str(interp, args)?.join(" ");
        out_write_flushing(interp, &format!("{s}\n"))?;
        Ok(Value::Nil)
    });

    // `pr`: exactly `prn`, minus the trailing newline.
    reg(i, "pr", ArityHint::Any, |interp, args| {
        let s = realize_all_pr_str(interp, args)?.join(" ");
        out_write(interp, &s)?;
        Ok(Value::Nil)
    });

    // SPEC-W3 (defect ledger D5): `clojure.core/newline`, which mova
    // simply did not have. Upstream is `(defn newline [] (. *out* (append
    // \newline)) nil)` -- one `\n` to `*out*`, `nil` back (measured on
    // 1.13.0-alpha6: `(prn (newline))` prints a blank line, then `nil`).
    // Routed through `out_write` like every other printer here, so
    // `with-out-str` captures it and a rebound `*out*` receives it.
    //
    // Beyond completeness: `clojure.spec.alpha`'s `explain-printer` calls
    // `(newline)` after every problem it prints, and the W2 port had to
    // patch that call site to `(print "\n")` (P7, reverted now that this
    // exists). A `clojure.core` var that spec references BY NAME is not a
    // place to accept a paraphrase.
    reg(i, "newline", ArityHint::Exact(0), |interp, _args| {
        out_write_flushing(interp, "\n")?;
        Ok(Value::Nil)
    });

    reg_unmeta(i, "name", ArityHint::Exact(1), |_i, args| match &args[0] {
        // Unlike `Symbol`, `Value::Keyword` has no separate `ns`/`name`
        // fields -- a namespaced keyword's reader token (`"ns/foo"`) is
        // stored verbatim as one flat string (value.rs), so `name` must
        // split it the same way the reader splits symbols (`symbol_from_str`
        // mirrors `reader::parse_symbol`'s rule) to strip the namespace.
        Value::Keyword(k) => Ok(Value::Str(symbol_from_str(k).name)),
        Value::Sym(s) => Ok(Value::Str(s.name.clone())),
        Value::Str(s) => Ok(Value::Str(s.clone())),
        other => Err(RjError::type_err(format!(
            "name: expected a keyword, symbol, or string, got {}",
            other.type_name()
        ))),
    });

    reg(i, "keyword", ArityHint::Range(1, 2), |interp, args| {
        let k = if args.len() == 1 {
            keyword_full_name(&args[0], "keyword")?
        } else if matches!(args[0], Value::Nil) {
            // clojure-lsp campaign (mova/PLAN.md): measured -- `(keyword
            // nil "foo")` is `:foo`, same as the 1-arg form, real
            // Clojure's 2-arity going through `Keyword/intern(String ns,
            // String name)`, which treats a `null` ns as "no namespace"
            // rather than casting it. `rewrite-clj.reader/read-keyword`
            // (transcribed from clj-kondo, a transitive dependency of
            // clojure-lsp's own `clojure-lsp.parser`) calls `(keyword ns
            // name)` with `ns` genuinely `nil` for every UN-namespaced
            // keyword it reads (`:foo`, as opposed to `:ns/foo`) -- the
            // overwhelming majority of keywords in any real source file.
            let name = expect_str(&args[1], "keyword")?;
            name.to_string().into()
        } else {
            let ns = expect_str(&args[0], "keyword")?;
            let name = expect_str(&args[1], "keyword")?;
            format!("{ns}/{name}").into()
        };
        // S5: `keyword` is one of the two `find-keyword`-registration
        // chokepoints (`KeywordRegistry`'s doc) -- every keyword this
        // fn ever mints becomes `find-keyword`-able from here on.
        interp.keywords.intern(&k);
        Ok(Value::Keyword(Keyword::from(k)))
    });

    // S5 (keywords.clj): `Keyword/find`'s mova counterpart -- looks up
    // `interp.keywords` (`KeywordRegistry`), NOT a JVM-wide intern table
    // (mova keywords are plain values, `value.rs`'s `Value::Keyword`).
    // Measured oracle semantics (`clojure -M -e`, 1.13.0-alpha6):
    //   (find-keyword "no-such-kw-ever")                       => nil
    //   (do (keyword "made-kw") (find-keyword "made-kw"))      => :made-kw
    //   (find-keyword :already-a-keyword)                      => itself,
    //     trivially -- a `Value::Keyword` argument is definitionally
    //     already "found" (mirrors real Clojure: the object existing IS
    //     being interned), so this arm never touches the registry.
    //   (find-keyword "user" "x") / (find-keyword ns name)     => nsname
    //     lookup, same registry, formatted "ns/name" like `Value::
    //     Keyword`'s own flat repr.
    reg(i, "find-keyword", ArityHint::Range(1, 2), |interp, args| {
        if args.len() == 1 {
            match &args[0] {
                Value::Keyword(k) => Ok(Value::Keyword(k.clone())),
                Value::Sym(s) => {
                    let full: Str = match &s.ns {
                        Some(ns) => format!("{ns}/{}", s.name).into(),
                        None => s.name.clone(),
                    };
                    Ok(find_keyword_lookup(interp, &full))
                }
                Value::Str(s) => Ok(find_keyword_lookup(interp, s)),
                other => Err(RjError::type_err(format!(
                    "find-keyword: expected a keyword, symbol, or string, got {}",
                    other.type_name()
                ))),
            }
        } else {
            let ns = expect_str(&args[0], "find-keyword")?;
            let name = expect_str(&args[1], "find-keyword")?;
            let full: Str = format!("{ns}/{name}").into();
            Ok(find_keyword_lookup(interp, &full))
        }
    });

    reg(i, "symbol", ArityHint::Range(1, 2), |_i, args| {
        if args.len() == 1 {
            // clojure-lsp campaign (mova/PLAN.md): sees through
            // `Value::Meta` like `.field`/`.method` interop already does
            // (eval/mod.rs's `eval_dot_form`) -- `(symbol (with-meta 'x
            // {...}))` is a measured no-op on the JVM (`Symbol` args are
            // read, never re-wrapped), but fell to the error arm here
            // since `&args[0]` only matched a BARE `Value::Sym`.
            // `clj-kondo.impl.utils/symbol-from-string`-adjacent call
            // sites feed a reader-produced (metadata-carrying) symbol
            // straight into `symbol` expecting it back unchanged.
            match args[0].unmeta() {
                Value::Sym(s) => Ok(Value::Sym(s.clone())),
                Value::Str(s) => Ok(Value::Sym(symbol_from_str(s))),
                other => Err(RjError::type_err(format!(
                    "symbol: expected a string or symbol, got {}",
                    other.type_name()
                ))),
            }
        } else {
            // clojure-lsp campaign (mova/PLAN.md): a nil `ns` is how real
            // Clojure's `(symbol ns name)` spells "no namespace" --
            // measured, `(symbol nil "foo")` is the unqualified symbol
            // `foo`, not an error. rewrite-clj's own `symbol-sexpr`
            // relies on exactly this: `(symbol (some-> ... str) (name
            // value))` passes a bare `nil` for every unqualified token.
            //
            // Real `Symbol/intern(ns, name)` also accepts a SYMBOL for
            // either arg (measured: `(symbol 'ns-sym 'name-sym)` works
            // on the JVM -- `Symbol.getNamespace`/`.getName` string args
            // just get `.toString()`'d through, same `.toString()`-
            // parity `char_seq_str_lenient` above documents) -- `name`
            // (a keyword/symbol) is a genuinely measured clj-kondo call
            // shape (`namespace.clj`'s own munging helpers).
            fn as_str_ish(v: &Value, op: &str) -> Result<Str, RjError> {
                match v.unmeta() {
                    Value::Str(s) => Ok(s.clone()),
                    Value::Sym(s) => Ok(s.name.clone()),
                    Value::Keyword(k) => Ok(k.text()),
                    other => Err(RjError::type_err(format!(
                        "{op}: expected a string or symbol, got {}",
                        other.type_name()
                    ))),
                }
            }
            let ns = match args[0].unmeta() {
                Value::Nil => None,
                other => Some(as_str_ish(other, "symbol")?),
            };
            let name = as_str_ish(&args[1], "symbol")?;
            Ok(Value::Sym(Symbol { ns, name }))
        }
    });

    reg(i, "subs", ArityHint::Range(2, 3), |_i, args| {
        // M8: `Str::char_slice` (rope-native for a `Rope` source --
        // `PText::slice` is `O(log n)`, shares every untouched subtree,
        // never materializes; identical byte-offset-conversion logic to
        // pre-M8 for `Flat`). This is the editor's hottest string call --
        // `oma.core.layout/layout-doc` calls it once per document LINE
        // with `s` the whole document (see the module doc's O(n^2)
        // history), and `oma.core.edit/insert` calls it (paired with
        // `str`, below) on EVERY keystroke.
        let s = expect_str(&args[0], "subs")?;
        let start = require_index(&args[1], "subs")?;
        let total = s.char_count_cached();
        let end = match args.get(2) {
            Some(v) => require_index(v, "subs")?,
            None => total,
        };
        if start > end || end > total {
            return Err(RjError::other(format!("subs: index out of bounds (start={start}, end={end}, len={total})")));
        }
        Ok(Value::Str(s.char_slice(start..end)))
    });

    reg(i, "split", ArityHint::Range(2, 3), |_i, args| {
        let s = char_seq_str(&args[0], "split")?;
        let s = s.as_ref();
        let limit = match args.get(2) {
            Some(v) => require_int(v, "split")?,
            None => 0,
        };
        let parts: PVec = match &args[1] {
            Value::Regex(re) => split_regex(re, s, limit).into_iter().map(|p| Value::Str(p.into())).collect(),
            Value::Str(sep) => split_literal(s, sep.as_ref(), limit).into_iter().map(|p| Value::Str(p.into())).collect(),
            other => {
                return Err(RjError::type_err(format!(
                    "split: expected a string or regex pattern, got {}",
                    other.type_name()
                )))
            }
        };
        Ok(Value::Vector(parts))
    });

    reg(i, "join", ArityHint::Range(1, 2), |interp, args| {
        let (sep, coll) = if args.len() == 2 {
            (crate::printer::display_str(&args[0]), &args[1])
        } else {
            (String::new(), &args[0])
        };
        let items = materialize(interp, coll)?;
        let strs: Vec<String> = items.iter().map(crate::printer::display_str).collect();
        Ok(Value::Str(strs.join(&sep).into()))
    });

    // MOVA-PATCH: real `upper-case`/`lower-case`/`trim` are `(.. s toString ...)` -- lenient.
    reg(i, "upper-case", ArityHint::Exact(1), |interp, args| {
        Ok(Value::Str(char_seq_str_lenient(interp, &args[0], "upper-case")?.to_uppercase().into()))
    });
    reg(i, "lower-case", ArityHint::Exact(1), |interp, args| {
        Ok(Value::Str(char_seq_str_lenient(interp, &args[0], "lower-case")?.to_lowercase().into()))
    });
    reg(i, "trim", ArityHint::Exact(1), |interp, args| {
        Ok(Value::Str(char_seq_str_lenient(interp, &args[0], "trim")?.trim().into()))
    });
    // S7: `capitalize` -- oracle: first char upper-cased, the REST
    // lower-cased (not left alone) -- measured `(capitalize "FOOBAR")` =>
    // `"Foobar"`, not `"FOOBAR"` with just the leading `F` re-cased. A
    // 0/1-char `s` upper-cases its (at most one) char and stops, matching
    // `string.clj`'s own `(< (count s) 2)` short-circuit (no "rest" to
    // lower-case, and empty-string `subs` would be an edge case otherwise).
    reg(i, "capitalize", ArityHint::Exact(1), |_i, args| {
        let s = char_seq_str(&args[0], "capitalize")?;
        let mut chars = s.as_ref().chars();
        let out = match chars.next() {
            None => String::new(),
            Some(first) => {
                let mut out: String = first.to_uppercase().collect();
                out.push_str(&chars.as_str().to_lowercase());
                out
            }
        };
        Ok(Value::Str(out.into()))
    });
    // S7: `reverse` -- registered ONLY under `clojure.string`/`string`
    // (never as a bare global): a bare `reverse` already exists
    // (`seq.rs`, the general sequence-reversing builtin every other
    // collection type and `(reverse coll)` call in the suite relies on),
    // and `clojure.string/reverse` is a DIFFERENT function with a
    // DIFFERENT return type (a string, not a seq of chars) -- measured,
    // `(s/reverse "bat")` must be `"tab"`, not `(\t \a \b)`, which is what
    // it was getting via `env::Env::get`'s bare-name fallback before this
    // (nothing had ever registered `clojure.string/reverse` explicitly).
    // char-by-char reversal (not grapheme-cluster-aware) matches the JVM's
    // own `StringBuilder.reverse()`, which is UTF-16-code-unit-based, not
    // grapheme-aware either.
    for ns in ["clojure.string", "string"] {
        reg_ns(i, ns, "reverse", ArityHint::Exact(1), |_i, args| {
            let s = char_seq_str(&args[0], "reverse")?;
            Ok(Value::Str(s.as_ref().chars().rev().collect::<String>().into()))
        });
    }
    reg(i, "starts-with?", ArityHint::Exact(2), |interp, args| {
        Ok(Value::Bool(
            char_seq_str_lenient(interp, &args[0], "starts-with?")?.starts_with(expect_str(&args[1], "starts-with?")?.as_ref()),
        ))
    });
    reg(i, "ends-with?", ArityHint::Exact(2), |interp, args| {
        Ok(Value::Bool(
            char_seq_str_lenient(interp, &args[0], "ends-with?")?.ends_with(expect_str(&args[1], "ends-with?")?.as_ref()),
        ))
    });
    reg(i, "includes?", ArityHint::Exact(2), |interp, args| {
        Ok(Value::Bool(
            char_seq_str_lenient(interp, &args[0], "includes?")?.contains(char_seq_str_lenient(interp, &args[1], "includes?")?.as_ref()),
        ))
    });
    reg(i, "replace", ArityHint::Exact(3), |interp, args| replace_impl(interp, args, "replace", false));
    reg(i, "replace-first", ArityHint::Exact(3), |interp, args| replace_impl(interp, args, "replace-first", true));
    // S7: `trim-newline` -- strips ALL trailing `\n`/`\r` chars (any mix,
    // any count -- Perl `chomp`-like), not just one. Measured:
    // `"the end\r\n\r\r\n"` => `"the end"` (every trailing `\r`/`\n` gone,
    // regardless of order), `"foo"` (no trailing newline) => `"foo"`
    // unchanged, `""` => `""`.
    reg(i, "trim-newline", ArityHint::Exact(1), |_i, args| {
        let s = char_seq_str(&args[0], "trim-newline")?;
        Ok(Value::Str(s.as_ref().trim_end_matches(['\n', '\r']).into()))
    });
    // S7: `re-quote-replacement` -- escapes `\` and `$` (the two chars
    // `java.util.regex.Matcher/quoteReplacement` treats specially in a
    // replacement string) by prefixing each with `\`, so the result is
    // safe to hand to `replace`/`replace-first`'s regex-pattern +
    // CharSequence-replacement arm without its own `$1`-style groups
    // being (mis)interpreted. Measured: `(re-quote-replacement "\\ $")`
    // => `"\\\\ \\$"`.
    reg(i, "re-quote-replacement", ArityHint::Exact(1), |_i, args| {
        let s = char_seq_str(&args[0], "re-quote-replacement")?;
        let mut out = String::with_capacity(s.as_ref().len());
        for c in s.as_ref().chars() {
            if c == '\\' || c == '$' {
                out.push('\\');
            }
            out.push(c);
        }
        Ok(Value::Str(out.into()))
    });
    // S7: `escape` -- `cmap` is CALLED as a fn per char (`(cmap ch)`), not
    // just map-looked-up: real Clojure's own `string.clj` does exactly
    // this (`(if-let [replacement (cmap ch)] ...)`), which is why a plain
    // Clojure MAP works here too -- maps are callable in mova (`eval::
    // apply::apply_value`'s `Value::Map` arm) exactly like on the JVM
    // (`IFn` on `IPersistentMap`). A truthy replacement is stringified via
    // `display_str` (real: `(str (cmap ch))`, so a `Char` OR `Str`
    // replacement value both work, matching every row of `t-escape`/
    // `char-sequence-handling`'s `escape` cases); a falsy (`nil`/`false`)
    // one appends the original char unchanged.
    reg(i, "escape", ArityHint::Exact(2), |interp, args| {
        let s = char_seq_str(&args[0], "escape")?;
        let cmap = args[1].clone();
        let mut out = String::with_capacity(s.as_ref().len());
        for ch in s.as_ref().chars() {
            let replacement = interp.call(&cmap, &[Value::Char(ch)])?;
            match replacement {
                Value::Nil | Value::Bool(false) => out.push(ch),
                other => out.push_str(&crate::printer::display_str(&other)),
            }
        }
        Ok(Value::Str(out.into()))
    });

    reg(i, "gensym", ArityHint::Range(0, 1), |_i, args| {
        let n = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed);
        let prefix = match args.first() {
            None => "G__".to_string(),
            Some(Value::Str(s)) => s.to_string(),
            Some(other) => {
                return Err(RjError::type_err(format!(
                    "gensym: expected a string prefix, got {}",
                    other.type_name()
                )))
            }
        };
        Ok(Value::Sym(Symbol::simple(format!("{prefix}{n}"))))
    });

    // `random-uuid` (clojure.core, 1.11+): same entropy source as
    // `(java.util.UUID/randomUUID)` (see `builtins::statics::uuid_random_uuid`).
    reg(i, "random-uuid", ArityHint::Exact(0), |_i, _args| {
        Ok(Value::Uuid(Arc::new(Value::random_uuid_bits())))
    });

    reg(i, "blank?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(match &args[0] {
            Value::Nil => true,
            // M8: `Str::is_blank` is rope-native (chunk-wise scan,
            // short-circuits on the first non-whitespace char) instead of
            // `Deref`-materializing the whole document just to `.trim()`
            // it -- one of the spec's named rope-native ops.
            Value::Str(s) => s.is_blank(),
            // S7: `char_seq_str`'s domain (`StringBuilder`/`StringBuffer`)
            // -- `blank?`'s own oracle signature is `^CharSequence` too.
            other if is_char_seq(other) => char_seq_str(other, "blank?")?.is_blank(),
            other => {
                return Err(RjError::type_err(format!(
                    "blank?: expected a string or nil, got {}",
                    other.type_name()
                )))
            }
        }))
    });

    reg(i, "index-of", ArityHint::Range(2, 3), |_i, args| {
        // No `Vec<char>` collect of the whole haystack (see
        // `char_byte_offset`'s doc comment) -- `layout-doc` calls this
        // once per document LINE with the whole document as the
        // haystack, the dominant term in the profiled O(n^2) file-open
        // hot loop.
        let haystack = char_seq_str(&args[0], "index-of")?;
        let from = match args.get(2) {
            Some(v) => require_index_clamped(v, "index-of")?,
            None => 0,
        };
        // M8: rope-native fast path for the actual host usage (searching
        // for a single char, e.g. `"\n"`) -- see `single_char_needle`'s
        // doc comment. A `Rope` haystack with a genuinely multi-char
        // needle falls through to `char_find` below, which materializes
        // via `Deref` ("materialize with care": `index-of`/`last-index-of`
        // aren't in the spec's named rope-native op list, and a multi-char
        // literal search isn't what the editor's own hot path does).
        if haystack.is_rope() {
            if let Some(c) = single_char_needle(&args[1]) {
                return Ok(haystack.find_char_from(c, from).map(|i| Value::Int(i as i64)).unwrap_or(Value::Nil));
            }
        }
        let needle = pattern_string(&args[1], "index-of")?;
        Ok(char_find(&haystack, &needle, from).map(|i| Value::Int(i as i64)).unwrap_or(Value::Nil))
    });

    reg(i, "last-index-of", ArityHint::Range(2, 3), |_i, args| {
        let haystack = char_seq_str(&args[0], "last-index-of")?;
        // W3a (string.clj's `test-last-index-of`, measured): Java's
        // `String.lastIndexOf(str, fromIndex)` documents "if fromIndex is
        // negative, -1 is returned" -- the MIRROR of `indexOf`'s clamp-to-
        // zero -- so `clojure.string/last-index-of` answers `nil`, it does
        // NOT throw. Oracle: `(clojure.string/last-index-of "abcz" "z"
        // -10)` => `nil` (and the same over a `StringBuffer` haystack).
        // `require_index` alone rejected every negative from-index here.
        if let Some(Value::Int(n)) = args.get(2) {
            if *n < 0 {
                return Ok(Value::Nil);
            }
        }
        let from = match args.get(2) {
            Some(v) => require_index(v, "last-index-of")?,
            // char_rfind/rfind_char_from both clamp `from` to the
            // haystack's own char count internally, so MAX stands in for
            // "unspecified" without an eager count just to compute this
            // default.
            None => usize::MAX,
        };
        // M8: same rope-native single-char fast path as `index-of` above.
        if haystack.is_rope() {
            if let Some(c) = single_char_needle(&args[1]) {
                return Ok(haystack.rfind_char_from(c, from).map(|i| Value::Int(i as i64)).unwrap_or(Value::Nil));
            }
        }
        let needle = pattern_string(&args[1], "last-index-of")?;
        Ok(char_rfind(&haystack, &needle, from).map(|i| Value::Int(i as i64)).unwrap_or(Value::Nil))
    });

    // Splits on the same line-boundary set as Java's `Pattern.compile
    // ("\r\n|\r|\n")` (which is what real Clojure's `split-lines` uses):
    // Rust's `str::lines` already treats a trailing `\r` before `\n` as
    // part of the boundary rather than the line's content, and treats a
    // bare `\r` as ending a line too, so no separate regex is needed.
    reg(i, "split-lines", ArityHint::Exact(1), |_i, args| {
        let s = char_seq_str(&args[0], "split-lines")?;
        Ok(Value::Vector(s.as_ref().lines().map(|l| Value::Str(l.into())).collect()))
    });

    reg(i, "triml", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Str(char_seq_str(&args[0], "triml")?.trim_start().into()))
    });
    reg(i, "trimr", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Str(char_seq_str(&args[0], "trimr")?.trim_end().into()))
    });

    // `replace-first` itself is R3's `replace_impl` registration above --
    // it already dispatches on the pattern arg (string literal vs regex),
    // which subsumes R4's string-only version that lived here pre-merge.

    // `--intern-unbound!`/`--global-bound?`: internal primitives backing
    // `core/core.mova`'s `declare`/`defonce` macros -- see this module's
    // header doc for why they live here. Neither is part of the public R4
    // surface (no `clojure.*` alias, deliberately obscure name).
    // Both primitives address exactly the var a `def` of the symbol would
    // write (`qualify_def` + exact probe), NOT the resolution order: a
    // `defonce` shadowing a core/referred name must still assign on first
    // eval, and `declare` must intern into the current namespace.
    reg_unmeta(i, "--intern-unbound!", ArityHint::Exact(1), |interp, args| {
        let sym = match &args[0] {
            Value::Sym(s) => s.clone(),
            other => return Err(RjError::type_err(format!("declare: expected a symbol, got {}", other.type_name()))),
        };
        let qualified = interp.qualify_def(&sym);
        interp.globals.intern(&qualified);
        Ok(Value::Nil)
    });
    reg_unmeta(i, "--global-bound?", ArityHint::Exact(1), |interp, args| {
        let sym = match &args[0] {
            Value::Sym(s) => s.clone(),
            other => return Err(RjError::type_err(format!("defonce: expected a symbol, got {}", other.type_name()))),
        };
        let qualified = interp.qualify_def(&sym);
        Ok(Value::Bool(interp.globals.get_exact(&qualified).is_some()))
    });

    for name in [
        "split",
        "join",
        "upper-case",
        "lower-case",
        "trim",
        "starts-with?",
        "ends-with?",
        "includes?",
        "replace",
        "blank?",
        "index-of",
        "last-index-of",
        "split-lines",
        "triml",
        "trimr",
        "replace-first",
        // S7: `capitalize`/`trim-newline`/`escape`/`re-quote-replacement`
        // -- new this session, none collide with an existing bare global
        // (unlike `reverse`, registered separately above via `reg_ns`
        // ONLY under `clojure.string`/`string` -- see that registration's
        // doc comment for why it can't go through this bare-then-alias
        // path).
        "capitalize",
        "trim-newline",
        "escape",
        "re-quote-replacement",
    ] {
        alias(i, "clojure.string", name);
        alias(i, "string", name);
    }
}
