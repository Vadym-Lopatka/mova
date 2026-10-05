//! edn/fast: a byte-level reader that parses EDN *data* source text
//! straight into a [`Value`], skipping `reader.rs`'s `Form` tree entirely.
//!
//! # Why
//!
//! `benches/edn_split_probe.rs` (competitor review 2026-08-23) measured
//! that for the edn.c fast-edn corpus, `read-string`'s cost is ~80-95%
//! `source -> Form` (phase A) and only a small remainder is `Form ->
//! Value` (phase B). `Form` exists to support the FULL Clojure surface
//! syntax (reader macros, metadata, spans for error messages, `#?`
//! reader conditionals) -- machinery genuine EDN *data* never needs. This
//! module is the fast path for the common case: plain EDN with none of
//! that.
//!
//! # The golden rule: bail, never diverge
//!
//! [`try_read_edn`] returns `Some(value)` ONLY when it fully handled the
//! input, and `None` on ANY construct outside its supported subset, ANY
//! parse error, or ANY doubt whatsoever. The caller (`builtins::reflect::
//! read_string`) then falls back to the general reader (`reader::
//! read_one_with_ns_ctors`), which either succeeds or raises the
//! canonical error. This module NEVER constructs an `RjError` and never
//! partially commits to an interpretation it isn't certain of -- which is
//! what makes semantic divergence on unsupported input structurally
//! impossible. For input it DOES accept, the output must be byte-identical
//! (by `pr_str` and `Value` equality) to what the general reader would
//! have produced; `tests/edn_fast_test.rs` is the differential battery
//! that pins this down.
//!
//! To guarantee identical construction semantics rather than merely
//! *similar* ones, this module calls the general reader's OWN helpers on
//! bounded token slices wherever one exists -- `reader::parse_number`
//! (bigint/ratio/BigDecimal productions, `Err` -> bail),
//! `reader::parse_symbol` (ns/name split), `reader::looks_like_number_start`
//! (the same "does this even look like a number" gate `read_atom` uses),
//! and `reader::DELIM_TABLE` (the exact ASCII delimiter set token scanning
//! must stop at). See each call site below for the few places no reusable
//! helper existed and this module had to replicate reader.rs's logic by
//! hand (keyword/string/collection construction) -- each is cross-checked
//! against the exact `reader.rs` function it mirrors in its own doc
//! comment.
//!
//! # Supported subset (anything else bails)
//!
//! - Trivia: ASCII whitespace, `,`, `;` line comments. (`#_` discards are
//!   NOT trivia here -- they're a bail trigger, see [`parse_hash`].)
//! - Any byte `>= 0x80` OUTSIDE a string literal bails immediately (both
//!   as a leading dispatch byte and mid-token) -- Unicode symbols/
//!   keywords/whitespace go to the general reader. Inside a string
//!   literal, UTF-8 bytes are copied through verbatim.
//! - `nil`/`true`/`false`, numbers, ASCII-token symbols/keywords, strings
//!   -- including the common escapes (`\n \t \r \\ \" \b \f`, `\uXXXX`,
//!   octal `\0`-`\377`) -- and `(` `)` `[` `]` `{` `}` `#{` `}`
//!   collections.
//! - Depth cap of 200 nested collections.
//!
//! Everything else (metadata `^`, quote/quasiquote/unquote/deref
//! `' ` ~ @`, character literals `\`, `::` auto-resolve, any `#` dispatch
//! other than `#{`, a `\uXXXX` escape landing in the UTF-16 surrogate
//! range `0xD800..=0xDFFF`, or any escape `read_string` itself treats as
//! an error) bails.

use crate::reader::{self, DELIM_TABLE};
use crate::value::{PMap, PVec, Value};

/// Mirrors real Clojure's/this crate's own reader having no depth limit in
/// principle, but recursing without one in a hand-rolled byte scanner risks
/// a stack overflow on adversarial input long before the general reader
/// (which shares the same risk, but is not this module's problem to fix)
/// would notice. 200 is comfortably past any real EDN document's nesting
/// and comfortably short of blowing the stack; exceeding it bails to the
/// general reader rather than growing the risk surface.
const MAX_DEPTH: u32 = 200;

/// Byte-level classification for the top-level dispatch switch inside
/// [`Scanner::parse_value`] -- a 256-entry table (unlike `reader::
/// DELIM_TABLE`, which only needs 128 entries because it's consulted only
/// on bytes already known to be ASCII) so every byte, including every
/// non-ASCII lead byte, gets an O(1) classification with no range check.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ByteClass {
    /// Whitespace/`,`/`;` -- consumed by `skip_trivia`, never actually
    /// seen by `parse_value` (kept as a distinct arm purely so an
    /// unexpected hit is a defensive bail rather than a silent
    /// misclassification).
    Trivia,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    /// `"` -- opens a string literal.
    Quote,
    /// `:` -- opens a keyword.
    Colon,
    /// `#` -- dispatch; only `#{` is supported (see [`Scanner::parse_hash`]).
    Hash,
    /// Every leading byte this module refuses to interpret at all: `'`
    /// `` ` `` `~` `@` `^` `\` (the general reader's own reader-macro
    /// dispatch characters, per `Reader::parse_one_form`'s match, other
    /// than the ones handled above) and every byte `>= 0x80` (Unicode
    /// outside a string literal, per this module's own bail rule).
    Bail,
    /// Everything else: the start of a number, `nil`/`true`/`false`, a
    /// symbol, or (post-`:`) a keyword body -- scanned as one ASCII token
    /// via `reader::DELIM_TABLE` and handed to the same helpers `read_atom`
    /// itself calls.
    AtomStart,
}

const fn build_class_table() -> [ByteClass; 256] {
    let mut t = [ByteClass::AtomStart; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = if i >= 0x80 {
            ByteClass::Bail
        } else {
            match i as u8 {
                0x09..=0x0D | 0x20 | b',' | b';' => ByteClass::Trivia,
                b'(' => ByteClass::LParen,
                b')' => ByteClass::RParen,
                b'[' => ByteClass::LBracket,
                b']' => ByteClass::RBracket,
                b'{' => ByteClass::LBrace,
                b'}' => ByteClass::RBrace,
                b'"' => ByteClass::Quote,
                b':' => ByteClass::Colon,
                b'#' => ByteClass::Hash,
                b'\'' | b'`' | b'~' | b'@' | b'^' | b'\\' => ByteClass::Bail,
                _ => ByteClass::AtomStart,
            }
        };
        i += 1;
    }
    t
}

static CLASS_TABLE: [ByteClass; 256] = build_class_table();

/// Byte-position cursor over `src`. No decoding: every position this
/// struct ever stops at is a byte offset the caller already knows is a
/// UTF-8 char boundary (either the start, right after a single-byte ASCII
/// delimiter, or right after a whole verbatim-copied string body scanned
/// via `memchr`), so slicing `src` with it is always safe.
struct Scanner<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    /// Skips ASCII whitespace, `,`, and `;` line comments -- the exact
    /// trivia set `reader::Reader::skip_trivia` handles MINUS `#_` discards
    /// (deliberately not replicated: a `#_` here is a bail trigger, not
    /// trivia -- see [`Self::parse_hash`]'s doc). Never fails: running off
    /// the end of `src` just stops the loop, same as reaching EOF mid-
    /// comment does in the general reader.
    fn skip_trivia(&mut self) {
        loop {
            match self.bytes.get(self.pos) {
                Some(&b) if matches!(b, 0x09..=0x0D | 0x20 | b',') => self.pos += 1,
                Some(&b';') => {
                    while let Some(&b) = self.bytes.get(self.pos) {
                        if b == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Scans a delimiter-bounded ASCII token, mirroring `reader::Reader::
    /// read_token` exactly for the bytes it accepts (same `DELIM_TABLE`),
    /// but returning `None` -- a bail, not a truncated token -- the instant
    /// a byte `>= 0x80` is seen before any delimiter. The general reader's
    /// `is_delim` treats most non-ASCII characters as ORDINARY token
    /// constituents (only Unicode whitespace stops a token there), so a
    /// caller here that just stopped at the `0x80` byte would silently
    /// under-read a symbol/keyword the general reader reads as one longer
    /// token -- exactly the divergence this module's golden rule forbids.
    fn scan_token(&mut self) -> Option<&'a str> {
        let start = self.pos;
        loop {
            match self.bytes.get(self.pos) {
                None => break,
                Some(&b) => {
                    if b >= 0x80 {
                        return None;
                    }
                    if DELIM_TABLE[b as usize] {
                        break;
                    }
                    self.pos += 1;
                }
            }
        }
        Some(&self.src[start..self.pos])
    }

    /// `"..."`: mirrors `reader::Reader::read_string` exactly (see
    /// `decode_escape`'s doc for the escape-by-escape cross-check). The
    /// escape-free case is one slice-copy (no `String` builder at all);
    /// the instant a `\` is seen, this switches to
    /// [`Self::parse_string_escaped`], which builds an owned `String`
    /// seeded with the escape-free prefix already scanned. An unterminated
    /// string (no closing `"` before EOF) bails, matching "never construct
    /// an error" -- the general reader raises the canonical "unclosed
    /// string".
    ///
    /// `memchr2` finds the FIRST of `"`/`\` in one SIMD-accelerated pass.
    fn parse_string(&mut self) -> Option<Value> {
        debug_assert_eq!(self.bytes.get(self.pos), Some(&b'"'));
        self.pos += 1; // consume opening quote
        let start = self.pos;
        match memchr::memchr2(b'"', b'\\', &self.bytes[self.pos..]) {
            None => None, // unclosed string -> bail
            Some(off) => {
                let idx = self.pos + off;
                if self.bytes[idx] == b'"' {
                    let s = &self.src[start..idx];
                    self.pos = idx + 1; // consume closing quote
                    Some(Value::Str(s.into()))
                } else {
                    // '\\' -- switch to the owned-String decode path,
                    // seeded with the escape-free prefix already scanned.
                    let mut out = String::with_capacity((idx - start) + 16);
                    out.push_str(&self.src[start..idx]);
                    self.pos = idx; // positioned AT the '\'
                    self.parse_string_escaped(out)
                }
            }
        }
    }

    /// The escaped continuation of [`Self::parse_string`]: `self.pos` is
    /// always positioned at a pending `\` at the top of the loop. Each
    /// iteration decodes one escape via [`Self::decode_escape`] (`None` ->
    /// bail, an unrecognized/invalid escape per `read_string`), then
    /// `memchr2`s ahead for the next `"`/`\`, pushing the escape-free run
    /// between them verbatim -- escapes are typically sparse, so this
    /// keeps the common case (a long unescaped run after the first escape)
    /// at `memchr` speed instead of a per-byte loop.
    fn parse_string_escaped(&mut self, mut out: String) -> Option<Value> {
        loop {
            debug_assert_eq!(self.bytes.get(self.pos), Some(&b'\\'));
            self.pos += 1; // consume '\'
            self.decode_escape(&mut out)?;
            match memchr::memchr2(b'"', b'\\', &self.bytes[self.pos..]) {
                None => return None, // unclosed string -> bail
                Some(off) => {
                    let idx = self.pos + off;
                    out.push_str(&self.src[self.pos..idx]);
                    self.pos = idx;
                    if self.bytes[idx] == b'"' {
                        self.pos += 1; // consume closing quote
                        return Some(Value::Str(out.into()));
                    }
                    // else '\\' -- loop back and decode it
                }
            }
        }
    }

    /// Decodes exactly one string escape into `out`, `self.pos` positioned
    /// right AFTER the `\` on entry and advanced past the whole escape on
    /// success. Mirrors `reader::Reader::read_string`'s escape `match`
    /// arm-for-arm:
    /// - `\n \t \r \\ \" \b \f` -- the fixed single-character escapes.
    /// - `\uXXXX` -- exactly 4 hex digits (`Self::read_hex4`, itself
    ///   mirroring `read_string_hex4`'s "not exactly 4 hex digits ->
    ///   error"). UNLIKE `read_string`'s `push_unicode_string_escape`, this
    ///   does NOT attempt high/low surrogate PAIRING for supplementary-
    ///   plane characters (`😀` etc) -- a deliberate
    ///   simplification: ANY code point landing in `0xD800..=0xDFFF`
    ///   (lone OR half of a valid pair) bails to the general reader, which
    ///   already has that pairing logic. This only gives up the fast path
    ///   on the rare supplementary-plane-via-surrogate-pair case, never on
    ///   an ordinary `\uXXXX` BMP escape.
    /// - `\0`-`\377` -- 1-3 octal digits, greedy, range-checked against
    ///   `0o377` exactly like `read_string`'s octal arm (`> 0o377` bails,
    ///   matching that method's "Octal escape sequence must be in range
    ///   [0, 377]" error).
    /// - Any other character after `\`, or EOF right after `\` -- bails
    ///   (`read_string`'s "invalid escape" / "unclosed string" errors,
    ///   respectively).
    fn decode_escape(&mut self, out: &mut String) -> Option<()> {
        let b = *self.bytes.get(self.pos)?;
        match b {
            b'n' => {
                out.push('\n');
                self.pos += 1;
            }
            b't' => {
                out.push('\t');
                self.pos += 1;
            }
            b'r' => {
                out.push('\r');
                self.pos += 1;
            }
            b'\\' => {
                out.push('\\');
                self.pos += 1;
            }
            b'"' => {
                out.push('"');
                self.pos += 1;
            }
            b'b' => {
                out.push('\u{8}');
                self.pos += 1;
            }
            b'f' => {
                out.push('\u{c}');
                self.pos += 1;
            }
            b'u' => {
                self.pos += 1;
                let code = self.read_hex4()?;
                if (0xD800..=0xDFFF).contains(&code) {
                    return None; // surrogate half -> bail, see doc above
                }
                // Safe: `code` is <= 0xFFFF (4 hex digits) and outside the
                // surrogate range, so it is always a valid scalar value.
                out.push(char::from_u32(code)?);
            }
            b'0'..=b'7' => {
                let mut val = u32::from(b - b'0');
                self.pos += 1;
                let mut count = 1;
                while count < 3 {
                    match self.bytes.get(self.pos) {
                        Some(&d) if (b'0'..=b'7').contains(&d) => {
                            val = val * 8 + u32::from(d - b'0');
                            self.pos += 1;
                            count += 1;
                        }
                        _ => break,
                    }
                }
                if val > 0o377 {
                    return None; // out of range -> bail
                }
                // Safe: val <= 0o377 = 255, always a valid Latin-1 scalar.
                out.push(val as u8 as char);
            }
            _ => return None, // invalid escape -> bail
        }
        Some(())
    }

    /// Exactly 4 ASCII hex-digit bytes, mirroring `reader::Reader::
    /// read_string_hex4`'s fixed-width requirement: fewer than 4 (EOF or a
    /// non-hex-digit byte before the 4th) bails rather than reading a
    /// short/partial escape.
    fn read_hex4(&mut self) -> Option<u32> {
        let digits = self.bytes.get(self.pos..self.pos + 4)?;
        if !digits.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        // Safe: just verified all 4 bytes are ASCII hex digits.
        let text = std::str::from_utf8(digits).ok()?;
        let v = u32::from_str_radix(text, 16).ok()?;
        self.pos += 4;
        Some(v)
    }

    /// `:name` / `:ns/name`: mirrors `reader::Reader::read_keyword`'s
    /// plain-single-colon case exactly -- the constructed keyword TEXT is
    /// the raw token after the colon, verbatim, with no ns/name splitting
    /// (unlike symbols, a keyword's `ns/name` split happens lazily, not at
    /// construction -- confirmed by reading `read_keyword`: the
    /// non-auto-resolve arm is just `token.to_string()`). `::` (auto-
    /// resolve against `*ns*`) and a bare `:` with no name both bail --
    /// this module has no `NsContext` and never will (auto-resolution is
    /// read-time namespace state, structurally out of scope for a
    /// stateless byte scanner).
    fn parse_keyword(&mut self) -> Option<Value> {
        debug_assert_eq!(self.bytes.get(self.pos), Some(&b':'));
        self.pos += 1; // consume ':'
        if self.bytes.get(self.pos) == Some(&b':') {
            return None; // `::`/`::alias/kw` auto-resolve -> bail
        }
        let token = self.scan_token()?;
        if token.is_empty() {
            return None; // bare ':' -- "expected a name after ':'"
        }
        Some(Value::Keyword(token.into()))
    }

    /// `nil`/`true`/`false`/number/symbol: mirrors `reader::Reader::
    /// read_atom` exactly, in the same order -- `looks_like_number_start`
    /// gates `parse_number` first (an `Err` bails, never becomes a made-up
    /// error), then the three literal keywords, then `parse_symbol` for
    /// everything else. Both `looks_like_number_start`/`parse_number` and
    /// `parse_symbol` are the general reader's OWN functions, reused
    /// verbatim (not reimplemented), so this arm cannot diverge on any
    /// input it accepts.
    fn parse_atom(&mut self) -> Option<Value> {
        let token = self.scan_token()?;
        if reader::looks_like_number_start(token) {
            return reader::parse_number(token).ok();
        }
        Some(match token {
            "nil" => Value::Nil,
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::Sym(reader::parse_symbol(token)),
        })
    }

    /// `#...`: mirrors `reader::Reader::read_hash`'s dispatch, but ONLY the
    /// `#{` arm is supported -- every other dispatch character (`(` fn
    /// literals, `"` regex, `'` var, `^` archaic-meta, `#` symbolic value,
    /// `_` discard, any tag-symbol lead including `uuid`) bails outright,
    /// per the owner spec's supported subset.
    fn parse_hash(&mut self, depth: u32) -> Option<Value> {
        debug_assert_eq!(self.bytes.get(self.pos), Some(&b'#'));
        self.pos += 1; // consume '#'
        match self.bytes.get(self.pos) {
            Some(&b'{') => {
                self.pos += 1; // consume '{'
                let items = self.read_delimited(b'}', depth + 1)?;
                // C10 (mirrors `reader::Reader::read_set`'s duplicate-
                // element check): build the persistent set, then compare
                // its `len()` against how many elements went in. Since
                // `PersistentHashSet::from_iter` inserts one at a time
                // with the same `Hash + Eq` `Value` this module's own
                // atoms/collections were just built with, a mismatch here
                // means a genuine duplicate under `Value`'s own equality
                // -- exactly what the general reader's pairwise
                // `form_to_value` scan detects, just cheaper.
                let n = items.len();
                let set: champ::PersistentHashSet<Value> = items.into_iter().collect();
                if set.len() != n {
                    return None; // duplicate element -> bail
                }
                Some(Value::Set(set))
            }
            _ => None, // #(, #", #', #^, ##, #_, #uuid, any tag -> bail
        }
    }

    /// Reads forms up to (and consuming) `close`, mirroring `reader::
    /// Reader::read_delimited`'s loop shape (skip trivia, check for the
    /// closer, else read one more form) but flat -- no `#?@` splicing to
    /// account for, since a `#` this module ever accepts is only `#{`.
    /// `depth` is the depth of the elements being read (i.e. already one
    /// past the collection's own opening bracket); an unclosed collection
    /// (EOF before `close`) bails rather than raising the general reader's
    /// "unclosed list/vector/map/set literal" error itself.
    fn read_delimited(&mut self, close: u8, depth: u32) -> Option<Vec<Value>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut items = Vec::with_capacity(4);
        loop {
            self.skip_trivia();
            match self.bytes.get(self.pos) {
                None => return None, // unclosed -> bail
                Some(&b) if b == close => {
                    self.pos += 1;
                    return Some(items);
                }
                Some(_) => items.push(self.parse_value(depth)?),
            }
        }
    }

    /// `{...}` -> `Value::Map`, mirroring `reader::Reader::read_map`'s two
    /// checks exactly: an odd item count bails (general reader's "map
    /// literal must contain an even number of forms"), and a duplicate key
    /// -- detected the same way as `parse_hash`'s set case, by comparing
    /// `PMap::from_iter`'s resulting `len()` against the pair count -- also
    /// bails (general reader's "Duplicate key: ...").
    fn build_map(items: Vec<Value>) -> Option<Value> {
        if items.len() % 2 != 0 {
            return None; // odd map literal -> bail
        }
        let n = items.len() / 2;
        let mut pairs = Vec::with_capacity(n);
        let mut it = items.into_iter();
        while let (Some(k), Some(v)) = (it.next(), it.next()) {
            pairs.push((k, v));
        }
        let map: PMap = pairs.into_iter().collect();
        if map.len() != n {
            return None; // duplicate key -> bail
        }
        Some(Value::Map(map))
    }

    /// Reads exactly one form at the current position (trivia already
    /// skipped by the caller -- same contract as `reader::Reader::
    /// parse_one_form`). `depth` is THIS form's own nesting depth (0 at
    /// the top level); collections check `depth + 1` against
    /// [`MAX_DEPTH`] before recursing into their elements via
    /// [`Self::read_delimited`].
    fn parse_value(&mut self, depth: u32) -> Option<Value> {
        let b = *self.bytes.get(self.pos)?;
        match CLASS_TABLE[b as usize] {
            ByteClass::LParen => {
                self.pos += 1;
                let items = self.read_delimited(b')', depth + 1)?;
                Some(Value::List(PVec::from(items)))
            }
            ByteClass::LBracket => {
                self.pos += 1;
                let items = self.read_delimited(b']', depth + 1)?;
                Some(Value::Vector(PVec::from(items)))
            }
            ByteClass::LBrace => {
                self.pos += 1;
                let items = self.read_delimited(b'}', depth + 1)?;
                Self::build_map(items)
            }
            // A closing delimiter where a form was expected is a genuine
            // syntax error on the general reader ("unexpected ')'" etc) --
            // bail rather than construct that error ourselves.
            ByteClass::RParen | ByteClass::RBracket | ByteClass::RBrace => None,
            ByteClass::Quote => self.parse_string(),
            ByteClass::Colon => self.parse_keyword(),
            ByteClass::Hash => self.parse_hash(depth),
            ByteClass::Bail => None,
            ByteClass::AtomStart => self.parse_atom(),
            // Trivia is always consumed by `skip_trivia` before
            // `parse_value` is ever called -- an unreachable defensive
            // bail, not a real code path.
            ByteClass::Trivia => None,
        }
    }
}

/// Reads exactly the first EDN form out of `src`, or `None` ("bail") the
/// instant anything outside this module's supported subset is seen -- see
/// the module doc for the full contract and supported-subset list. The ONE
/// caller is `builtins::reflect::read_string`'s 1-arity path (before it
/// builds an `NsContext` at all -- this module has no namespace state and
/// never resolves `::kw`), which falls back to `reader::
/// read_one_with_ns_ctors` on `None`.
///
/// EOF after skipping trivia (an empty string, all-whitespace, or a
/// comment with nothing after it) reads as `Value::Nil` -- matching
/// `(read-string "")` => `nil` (`read_string_reads_one_form_and_stops`,
/// `src/builtins/reflect.rs`), even though the general reader's own
/// `read_one` represents that case as `Ok(None)` rather than a `Nil` form;
/// only `read-string`'s final OUTPUT needs to match, and it does.
///
/// Trailing content after the first form (`"1 2"`) is never inspected --
/// same as the general reader's `read_one`, which reads exactly one
/// top-level form and ignores the rest.
pub(crate) fn try_read_edn(src: &str) -> Option<Value> {
    let mut s = Scanner { src, bytes: src.as_bytes(), pos: 0 };
    s.skip_trivia();
    if s.pos >= s.bytes.len() {
        return Some(Value::Nil);
    }
    s.parse_value(0)
}
