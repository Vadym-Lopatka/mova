//! R3: `re-pattern re-find re-matches re-seq` (+ a convenience `regex?`
//! predicate) against the `regex` crate. `#"pattern"` literals are compiled
//! at read time (`reader.rs`'s `read_regex`); `re-pattern` is the runtime
//! entry point for building one from a string. Patterns arrive in Java
//! (`java.util.regex.Pattern`) syntax -- see `reader.rs`'s `read_regex` doc
//! comment for why that's accepted as-is (no translation layer) for this
//! corpus, and where an unsupported construct (lookaround, backrefs) is
//! rejected.
//!
//! ## Match shape
//!
//! `re-find`/`re-matches`/`re-seq` all report a match the same way: no
//! capture groups -> the matched string (or `nil` for no match); one or
//! more groups -> `[full g1 g2 ...]` where a group that didn't participate
//! in the match (e.g. the untaken side of an alternation) is `nil`, not an
//! empty string -- matching Clojure's `re-groups`.

use std::sync::{Arc, Mutex};

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{MatcherState, PVec, Value};

fn expect_regex(v: &Value, op: &str) -> Result<Arc<crate::value::LazyRegex>, RjError> {
    match v {
        Value::Regex(re) => Ok(re.clone()),
        other => Err(RjError::type_err(format!(
            "{op}: expected a regex (#\"...\" or re-pattern), got {}",
            other.type_name()
        ))),
    }
}

fn expect_str<'a>(v: &'a Value, op: &str) -> Result<&'a str, RjError> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(RjError::type_err(format!(
            "{op}: expected a string, got {}",
            other.type_name()
        ))),
    }
}

fn expect_matcher(v: &Value, op: &str) -> Result<Arc<Mutex<MatcherState>>, RjError> {
    match v {
        Value::Matcher(m) => Ok(m.clone()),
        other => Err(RjError::type_err(format!(
            "{op}: expected a matcher (re-matcher), got {}",
            other.type_name()
        ))),
    }
}

/// C4: translates Java-flavored `\Q...\E` literal-quoting spans into
/// content the `regex` crate accepts -- every character inside a span is
/// escaped (via `regex::escape`, which is also what makes this correct
/// *inside* a character class: it escapes only the characters that need
/// it, e.g. `]` -> `\]`, so the result still parses as class members
/// rather than one atomic literal run) and everything outside spans is
/// passed through untouched, byte-for-byte.
///
/// Measured against the oracle (`java.util.regex.Pattern` via real
/// Clojure 1.13.0-alpha6; see `compat/regexq-oracle-transcript.txt`):
///
/// - Unterminated `\Q` (no matching `\E`) quotes to the end of the
///   pattern.
/// - `\Q\E` (empty span) contributes nothing.
/// - A bare `\E` with no preceding `\Q` is a *compile-time error* in
///   Java (`PatternSyntaxException: Illegal/unsupported escape
///   sequence`). We deliberately do NOT special-case it: it passes
///   through untouched here, so the `regex` crate rejects it on its own
///   terms ("unrecognized escape sequence") -- same error-vs-success
///   shape as Java, different message, which is the best this crate
///   allows short of hand-rolling a Java-style syntax error.
/// - `\Q` is detected by scanning for the literal two-char sequence
///   `\Q` (backslash immediately followed by `Q`); a second `\Q`
///   *inside* an already-open span is NOT special -- it's just two
///   literal characters (there's no such thing as nesting) -- so once
///   inside a span we only ever scan for the next `\E`.
/// - The `\E` terminator is found the same way: by scanning for the
///   literal two-char sequence `\E` one position at a time, NOT by
///   pairwise-consuming backslash-escapes while inside the span --
///   inside `\Q...\E`, backslash is ordinary data, not an escape
///   introducer. Measured: `#"\Qa\\Eb"` (content `a`, `\`, `\`, `E`,
///   `b`) quotes just `a\` (the first backslash is span data) and
///   terminates at the *second* backslash + `E`, then matches literal
///   `a\b` -- if we instead consumed backslash-escapes in pairs while
///   inside the span, the first `\\` pair would be swallowed and the
///   real `\E` missed entirely.
/// - Outside a span, ordinary backslash-escapes (`\\`, `\d`, ...) DO
///   consume in pairs before we look for the next `\Q`, so an escaped
///   backslash immediately followed by `Q` (e.g. `\\\Q`, three
///   backslashes then `Q`) is not misdetected as a quote-start
///   mid-escape-pair -- this mirrors Java's own token-by-token
///   tokenizing, which only treats `\Q` specially when it's read as a
///   fresh escape, not when `Q` falls out of an unrelated pair.
pub fn translate_java_regex_quoting(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(pattern.len());
    let mut i = 0;
    while i < n {
        if chars[i] == '\\' && i + 1 < n && chars[i + 1] == 'Q' {
            // Found `\Q` -- scan for the next raw `\E` one position at a
            // time (backslash is plain data inside the span, so this is
            // NOT paired escape-consumption).
            let span_start = i + 2;
            let mut j = span_start;
            let mut end = None;
            while j + 1 < n {
                if chars[j] == '\\' && chars[j + 1] == 'E' {
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            match end {
                Some(e) => {
                    let span: String = chars[span_start..e].iter().collect();
                    out.push_str(&regex::escape(&span));
                    i = e + 2;
                }
                None => {
                    // Unterminated `\Q`: quotes to the end of the pattern.
                    let span: String = chars[span_start..n].iter().collect();
                    out.push_str(&regex::escape(&span));
                    i = n;
                }
            }
        } else if chars[i] == '\\' && i + 1 < n {
            // Ordinary escape pair outside any span -- copy both chars
            // unchanged (leave interpretation to the `regex` crate /
            // caller) and advance by 2 so a `\Q` that's actually the
            // tail of an unrelated escape run isn't misdetected on the
            // next iteration.
            out.push(chars[i]);
            out.push(chars[i + 1]);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Compiles a fresh `#(pattern)` for `re-pattern`'s runtime (non-literal)
/// path. Shares the corpus caveat documented on `reader::Reader::read_regex`
/// (Java-flavored syntax, no lookaround/backrefs) but the error here is a
/// runtime `TypeErr`, not a reader error -- there's no source span to blame
/// once we're past the reader.
///
/// C4: `\Q...\E` (Java literal-quoting) is translated to `regex`-crate
/// syntax before compiling -- see `translate_java_regex_quoting`.
pub(crate) fn compile_pattern(pattern: &str, op: &str) -> Result<Arc<crate::value::LazyRegex>, RjError> {
    // S4: compiled-program cache (a compile is ~25 KB and ~50 us; `re-pattern`
    // in a loop, e.g. babashka.cli's str-width, paid it per call). A clone
    // shares the program; each Pattern stays a distinct object.
    static CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, fancy_regex::Regex>>> = std::sync::Mutex::new(None);
    let translated = translate_java_regex_quoting(pattern);
    if let Some(r) = crate::sync::lock_mutex(&CACHE).as_ref().and_then(|m| m.get(&*translated)) {
        return Ok(Arc::new(r.clone().into()));
    }
    let r = fancy_regex::Regex::new(&translated).map_err(|e| RjError::type_err(format!("{op}: invalid regex pattern: {e}")))?;
    let mut g = crate::sync::lock_mutex(&CACHE);
    let m = g.get_or_insert_with(Default::default);
    if m.len() >= 64 {
        m.clear()
    }
    m.insert(translated.to_string(), r.clone());
    Ok(Arc::new(r.into()))
}

/// D5: a `fancy_regex` match attempt's error -- only reachable for a
/// pattern that actually uses a backtracking construct (lookaround,
/// backreference), and then only when the match exceeds the engine's
/// backtrack limit. A `regex`-delegated pattern can never produce one.
/// Surfaced rather than swallowed: an exhausted backtrack budget is a
/// wrong answer, not a "no match".
fn match_err(op: &str, e: fancy_regex::Error) -> RjError {
    RjError::other(format!("{op}: regex match failed: {e}"))
}

/// Shared by `re-find`/`re-matches`/`re-seq`: turns one `regex::Captures`
/// into the `[full g1 g2 ...]` (or bare full-match string) shape described
/// in this module's doc comment.
fn captures_to_value(re: &fancy_regex::Regex, caps: &fancy_regex::Captures<str>) -> Value {
    if re.captures_len() == 1 {
        Value::Str(caps.get(0).unwrap().as_str().into())
    } else {
        let groups: PVec = (0..re.captures_len())
            .map(|i| match caps.get(i) {
                Some(m) => Value::Str(m.as_str().into()),
                None => Value::Nil,
            })
            .collect();
        Value::Vector(groups)
    }
}

// clojure-lsp campaign (mova/PLAN.md): `.matcher`/`.matches`/`.group` --
// the three `java.util.regex.Pattern`/`Matcher` methods `clojure.tools.
// reader.impl.commons`'s number parser calls directly (a transitive
// dependency of `rewrite-clj.reader`, itself required by clojure-lsp's
// own `clojure-lsp.parser`), rather than going through `re-find`/`re-
// matches`/`re-groups`. Real Java semantics, not a hack: `.matcher` is
// `Pattern.matcher(CharSequence)` (== `re-matcher`, same constructor,
// different spelling); `.matches` is `Matcher.matches()` -- unlike
// `find()`/`re-find`, it anchors to the WHOLE input, same test `re-
// matches` already makes, and on success leaves the matcher's group
// state populated for `.group` to read back; `.group` is `Matcher.
// group(int)`, index 0 the whole match, indexing into exactly the `[full
// g1 g2 ...]` (or bare string) shape `captures_to_value` already builds
// for `re-find`/`re-groups`.

/// `(.matcher pattern s)` -- identical to `(re-matcher pattern s)`.
pub(crate) fn dot_matcher(pattern: &Value, s: &Value) -> Result<Value, RjError> {
    let re = expect_regex(pattern, ".matcher")?;
    let s = expect_str(s, ".matcher")?;
    Ok(Value::Matcher(Arc::new(Mutex::new(MatcherState {
        re,
        input: s.to_string(),
        pos: 0,
        last_match: None,
        last_span: None,
    }))))
}

/// `(.matches m)` -- `Matcher.matches()`: the WHOLE input must match
/// (same anchor test `re-matches` makes), leaving `last_match`/
/// `last_span` populated on success so a following `.group` call reads
/// this find, exactly like `re-find`'s stateful 1-arity already does.
pub(crate) fn dot_matches(m: &Value) -> Result<Value, RjError> {
    let m = expect_matcher(m, ".matches")?;
    let mut guard = crate::sync::lock_mutex(&m);
    let re = guard.re.clone();
    let input = guard.input.clone();
    let full = re.captures(input.as_str()).map_err(|e| match_err(".matches", e))?.filter(|caps| {
        let whole = caps.get(0).expect("group 0 always participates");
        whole.start() == 0 && whole.end() == input.len()
    });
    match full {
        Some(caps) => {
            guard.last_match = Some(captures_to_value(&re, &caps));
            guard.last_span = Some((0, input.chars().count()));
            Ok(Value::Bool(true))
        }
        None => {
            guard.last_match = None;
            guard.last_span = None;
            Ok(Value::Bool(false))
        }
    }
}

/// `(.group m i)` -- `Matcher.group(int)`, indexing into the SAME `[full
/// g1 g2 ...]` (or bare full-match string) shape `last_match` already
/// holds. Throws "No match found" with no successful find yet, same as
/// `re-groups`; an out-of-range index (a group number the pattern does
/// not have) is `nil`, matching a non-participating group's own `nil`
/// (real Java throws `IndexOutOfBoundsException` instead, but nothing in
/// this campaign's corpus asks for an out-of-range group, so the more
/// permissive answer costs nothing and matches `re-groups`' own
/// "missing/non-participating group is `nil`" contract).
pub(crate) fn dot_group(m: &Value, idx: i64) -> Result<Value, RjError> {
    let m = expect_matcher(m, ".group")?;
    let guard = crate::sync::lock_mutex(&m);
    let idx = usize::try_from(idx).unwrap_or(usize::MAX);
    match &guard.last_match {
        Some(Value::Vector(v)) => Ok(v.get_owned(idx).unwrap_or(Value::Nil)),
        Some(whole) => Ok(if idx == 0 { whole.clone() } else { Value::Nil }),
        None => Err(RjError::other(".group: No match found")),
    }
}

pub fn register(i: &mut Interp) {
    reg(i, "regex?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Regex(_))))
    });

    reg(i, "re-pattern", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::Regex(re) => Ok(Value::Regex(re.clone())),
        Value::Str(s) => Ok(Value::Regex(compile_pattern(s, "re-pattern")?)),
        other => Err(RjError::type_err(format!(
            "re-pattern: expected a string or regex, got {}",
            other.type_name()
        ))),
    });

    // S4 (everyday3): `re-find` grew a 1-arity, stateful form -- given a
    // `re-matcher` value, it advances the search from the matcher's
    // current cursor (`Regex::captures_at`, the `regex` crate's direct
    // equivalent of `java.util.regex.Matcher#find()`'s "resume from where
    // the last find left off") and caches the result for `re-groups`.
    // Measured: after a zero-width match the cursor steps forward by one
    // CHAR (not byte -- `input[end..].chars().next()`, so the next
    // `captures_at` call always starts on a valid UTF-8 boundary), which
    // is what keeps a pattern like `#""` from looping on the same
    // position forever; after the LAST possible match the cursor is left
    // wherever it lands and every subsequent `re-find` on the same
    // matcher keeps returning `nil` (measured, not just "probably").
    reg(i, "re-find", ArityHint::Range(1, 2), |_i, args| {
        if let Some(second) = args.get(1) {
            let re = expect_regex(&args[0], "re-find")?;
            let s = expect_str(second, "re-find")?;
            return Ok(match re.captures(s).map_err(|e| match_err("re-find", e))? {
                Some(caps) => captures_to_value(&re, &caps),
                None => Value::Nil,
            });
        }
        let m = expect_matcher(&args[0], "re-find")?;
        let mut guard = crate::sync::lock_mutex(&m);
        let re = guard.re.clone();
        let found = re
            .captures_from_pos(guard.input.as_str(), guard.pos)
            .map_err(|e| match_err("re-find", e))?
            .map(|caps| {
                let whole = caps.get(0).expect("group 0 always participates");
                (whole.start(), whole.end(), captures_to_value(&re, &caps))
            });
        match found {
            Some((start, end, result)) => {
                // D5: record the match span in CHAR offsets for
                // `.start`/`.end` -- see `MatcherState::last_span`.
                let start_chars = guard.input[..start].chars().count();
                let end_chars = start_chars + guard.input[start..end].chars().count();
                guard.last_span = Some((start_chars, end_chars));
                guard.pos = if end > start {
                    end
                } else {
                    match guard.input[end..].chars().next() {
                        Some(c) => end + c.len_utf8(),
                        None => end + 1,
                    }
                };
                guard.last_match = Some(result.clone());
                Ok(result)
            }
            None => {
                guard.last_match = None;
                guard.last_span = None;
                Ok(Value::Nil)
            }
        }
    });

    // S4 (everyday3): `(re-matcher re s)` -- a fresh stateful matcher,
    // cursor at 0, no last match yet.
    reg(i, "re-matcher", ArityHint::Exact(2), |_i, args| {
        let re = expect_regex(&args[0], "re-matcher")?;
        let s = expect_str(&args[1], "re-matcher")?;
        Ok(Value::Matcher(Arc::new(Mutex::new(MatcherState {
            re,
            input: s.to_string(),
            pos: 0,
            last_match: None,
            last_span: None,
        }))))
    });

    // S4 (everyday3), measured: `(re-groups m)` returns the SAME shape
    // `re-find`/`re-matches` already return (bare string, no groups; `[full
    // g1 g2 ...]` with groups) for the matcher's last successful find, and
    // throws if the matcher has never found a match yet OR its most recent
    // find attempt failed (`java.lang.IllegalStateException: No match
    // found` on the real JVM; mova's own error type here, main corpus
    // only ever compares OK-vs-ERR, never the message).
    reg(i, "re-groups", ArityHint::Exact(1), |_i, args| {
        let m = expect_matcher(&args[0], "re-groups")?;
        let guard = crate::sync::lock_mutex(&m);
        match &guard.last_match {
            Some(v) => Ok(v.clone()),
            None => Err(RjError::other("re-groups: No match found")),
        }
    });

    reg(i, "re-matches", ArityHint::Exact(2), |_i, args| {
        let re = expect_regex(&args[0], "re-matches")?;
        let s = expect_str(&args[1], "re-matches")?;
        // Anchor semantics: a match that merely starts the leftmost search
        // isn't enough -- it must span the WHOLE input, matching Java's
        // `Matcher.matches()` (as opposed to `find()`). Wrapping the source
        // pattern in `^(?:...)$` was deliberately rejected (see R3 spec):
        // an inner `(?m)`/`(?s)` flag would make `^`/`$` mean line, not
        // string, boundaries, silently breaking the anchor. Checking the
        // found match's span instead is flag-agnostic.
        Ok(match re.captures(s).map_err(|e| match_err("re-matches", e))? {
            Some(caps) if caps.get(0).unwrap().start() == 0 && caps.get(0).unwrap().end() == s.len() => {
                captures_to_value(&re, &caps)
            }
            _ => Value::Nil,
        })
    });

    reg(i, "re-seq", ArityHint::Exact(2), |_i, args| {
        let re = expect_regex(&args[0], "re-seq")?;
        let s = expect_str(&args[1], "re-seq")?;
        // Eager (spec allows it): `captures_iter` walks successive
        // non-overlapping matches left to right, same as Clojure's
        // `re-seq`.
        let mut items = PVec::new();
        for caps in re.captures_iter(s) {
            let caps = caps.map_err(|e| match_err("re-seq", e))?;
            items.push_back(captures_to_value(&re, &caps));
        }
        Ok(Value::List(items))
    });
}
