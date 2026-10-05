//! mova.reader (clojure-lsp-on-Mova campaign, mova/PLAN.md "Where does a
//! fix go?" rule #3): hand-written Rust CST parser mirroring clj-kondo's
//! FORKED rewrite-clj parser (clj_kondo.impl.rewrite_clj.parser.core) --
//! NOT upstream rewrite-clj. Discovery: this fork's `:whitespace`/`:comment`
//! dispatch methods return the reader itself, which `read-with-meta`
//! treats as "produced nothing, skip" -- so whitespace/newline/comma/
//! comment are never built as nodes at all.
//!
//! Round 2 discovery: `#_` (uneval) is ALSO transparent in the common
//! case -- `parser/core.clj`'s `read-with-ignore-hint` reads the `#_`
//! target purely to check `ignore-meta` (a clj-kondo `:clj-kondo/ignore`
//! lint-directive hint); when that's nil (the ordinary case) it discards
//! the target and returns whatever `parse-next` reads AFTER it, exactly
//! as if `#_form` were whitespace -- it never becomes an `UnevalNode`.
//! Only the ignore-hint-truthy case and the rare `#_#?...` disguise case
//! fall back to the interpreted path (both need machinery this native
//! path doesn't have: emitting the hint as real object metadata, and
//! reader-conditional-feature filtering).
//!
//! `^`/`#^` reader metadata is likewise NOT a `MetaNode`: `parse-meta`
//! reads the meta form, reads the value, and calls
//! `(update value-node :meta lconj meta-node)` -- a record-field update,
//! via the `:attach-reader-meta` ctors entry (a plain Clojure closure
//! supplied by the overlay). Per `read-with-meta`'s `(conj {:row ...}
//! (meta entry))` merge (the second map's keys win), the RESULT keeps
//! the value's own (inner) position, not the `^`-prefixed span -- so
//! this bypasses the generic end-of-`parse_next` position wrap entirely.
//!
//! `(mova.reader/parse-string s ctors)` / `parse-string-all`: `ctors` is
//! a map of tag keyword -> the LIBRARY'S OWN node constructor fn (or, for
//! `:attach-reader-meta`/`:ignore-meta`, a small helper closure), looked
//! up by the overlay adapter. Rust calls each bottom-up with exactly the
//! args it needs, then attaches `{:row :col :end-row :end-col}` via
//! `Value::attach_meta` (1-based, col reset to 1 after `\n`), matching
//! `reader/read-with-meta`.
//!
//! Coverage: tokens (symbol/keyword/string incl. multi-line/number incl.
//! hex/octal/radix/ratio/bigint-N/bigdecimal-M/char incl. named+\u+\o/
//! nil/true/false), collections (list/vector/map/set/`#()` fn),
//! quote/syntax-quote/unquote/unquote-splicing, deref, `#'` var, `#=`
//! eval, `#:ns{}`/`#::{}`/`#::alias{}` namespaced maps, `#?`/`#?@`
//! reader-conditional, any other `#tag form` as a generic reader-macro,
//! `^`/`#^` reader metadata, `#_` (transparent skip). Anything else --
//! `##Inf`/`##NaN`/`##-Inf` symbolic values, `#!` shebang, malformed
//! `#?` forms, the `#_`-ignore-hint and `#_#?` disguise cases, ratios/
//! radixes with an invalid digit, and any genuine lex/parse error --
//! returns the sentinel keyword `:mova.reader/fallback`; the overlay
//! adapter re-parses the WHOLE top-level form (or whole string, for
//! `-all`) with the original interpreted parser, so output stays
//! byte-identical.

use std::sync::Arc;

use num_bigint::BigInt;

use crate::bignum::{self, BigDecVal, BigIntVal, RatioVal, Reduced};
use crate::builtins::ArityHint;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{Keyword, PMap, PVec, Str, Symbol, Value};

/// [`crate::builtins::reg`], but registered ONLY under `ns/name`.
/// Duplicated from `builtins::io`'s own copy (private to that module)
/// for the same reason its doc comment gives.
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
            return Err(RjError::arity(format!("{ns}/{name}: wrong number of args ({})", args.len()))
                .with_stack(interp.stack_snapshot(), interp.source_id));
        }
        f(interp, args)
    });
    i.globals.set_builtin(
        Symbol { ns: Some(ns.into()), name: name.into() },
        Value::Native(Arc::new(native)),
    );
}

/// The fallback sentinel: a plain (never namespaced-node-shaped) keyword.
/// A real parse result is always a node record, never a bare `Value`, so
/// this can never collide with genuine output.
fn fallback() -> Value {
    Value::Keyword(Keyword::construct("mova.reader/fallback"))
}

fn is_boundary(c: Option<char>) -> bool {
    match c {
        None => true,
        Some(c) => "\":;'@^`~()[]{}\\".contains(c),
    }
}

/// True at EOF or a closing delimiter -- used by `parse_uneval` to detect
/// "nothing left to transparently return" without misreading a genuine
/// mismatched-bracket error as one.
fn is_boundary_close(c: Option<char>) -> bool {
    matches!(c, None | Some(')') | Some(']') | Some('}'))
}

fn is_ws_or_boundary(c: Option<char>) -> bool {
    match c {
        None => true,
        Some(c) => c.is_whitespace() || c == ',' || is_boundary(Some(c)),
    }
}

fn is_truthy(v: &Value) -> bool {
    !matches!(v, Value::Nil | Value::Bool(false))
}

struct P {
    chars: Vec<char>,
    pos: usize,
    row: i64,
    col: i64,
    /// Round 3 (real rewrite-clj, mova/PLAN.md): when true, trivia
    /// (whitespace/newline/comma/comment) become real, position-wrapped
    /// nodes instead of being silently skipped, and `^`/`#^`/`#_` become
    /// ordinary ctor'd nodes (no clj-kondo-fork bypass/record-update
    /// hack -- vanilla rewrite-clj never had that patch). Set once, from
    /// whether `ctors` has a `:whitespace` entry (see `str_and_ctors`'s
    /// callers): the fork's ctors map never has one, rewrite-clj's
    /// always does.
    trivia: bool,
}

type R<T> = Result<T, ()>;

impl P {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.row += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    /// Skip whitespace, commas and `;` line comments (through and
    /// including the terminating linebreak, matching
    /// `reader/read-include-linebreak`) -- never produces a node.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() || c == ',' => {
                    self.advance();
                }
                Some(';') => {
                    while let Some(c) = self.peek() {
                        self.advance();
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    fn read_raw_token(&mut self) -> String {
        let mut s = String::new();
        while !is_ws_or_boundary(self.peek()) {
            s.push(self.advance().unwrap());
        }
        s
    }

    /// Extends an already-read SYMBOL token past `'`/`:`, matching
    /// clj-kondo's own rewrite-clj fork (`parser/token.clj`'s
    /// `symbol-node`/`not-boundary-allow-extra?`): a bare read stops at
    /// `:` (keywords start there), but once a token is already known to
    /// be a symbol, `'`/`:` mid-token don't end it (`can-move-to-:let?`
    /// is one symbol, not `can-move-to-` + `:let?`). Only called for the
    /// symbol fallthrough in `parse_token` -- nil/true/false/numbers are
    /// decided on the first (strict) read, before this ever runs.
    fn read_symbol_extra(&mut self, buf: &mut String) {
        loop {
            match self.peek() {
                Some(c) if c == '\'' || c == ':' || !is_ws_or_boundary(Some(c)) => {
                    buf.push(self.advance().unwrap());
                }
                _ => break,
            }
        }
    }

    fn ctor(&self, ctors: &PMap, tag: &str) -> R<Value> {
        ctors.get(&Value::Keyword(Keyword::construct(tag))).cloned().ok_or(())
    }

    fn call_ctor(&self, interp: &mut Interp, ctors: &PMap, tag: &str, args: &[Value]) -> R<Value> {
        let f = self.ctor(ctors, tag)?;
        interp.call(&f, args).map_err(|_| ())
    }

    /// Entry point for one node: skip trivia, dispatch, wrap with
    /// `{:row :col :end-row :end-col}` (matches `read-with-meta`). Bare
    /// `^`/`#^`/`#_` bypass this wrap (see module doc). `Ok(None)` at EOF.
    fn parse_next(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Option<Value>> {
        if self.trivia {
            return self.dispatch_rc(interp, ctors).map(|opt| opt.map(|(v, _)| v));
        }
        self.skip_trivia();
        let c = match self.peek() {
            None => return Ok(None),
            Some(c) => c,
        };
        if c == '^' {
            self.advance();
            return self.parse_meta_apply(interp, ctors).map(Some);
        }
        if c == '#' && self.peek_at(1) == Some('^') {
            self.advance();
            self.advance();
            return self.parse_meta_apply(interp, ctors).map(Some);
        }
        if c == '#' && self.peek_at(1) == Some('_') {
            self.advance();
            self.advance();
            return self.parse_uneval(interp, ctors);
        }

        let start_row = self.row;
        let start_col = self.col;
        let node = match c {
            '(' => self.parse_seq(interp, ctors, '(', ')', "list")?,
            '[' => self.parse_seq(interp, ctors, '[', ']', "vector")?,
            '{' => self.parse_seq(interp, ctors, '{', '}', "map")?,
            ')' | ']' | '}' => return Err(()),
            '\'' => {
                self.advance();
                self.parse_one_child(interp, ctors, "quote")?
            }
            '`' => {
                self.advance();
                self.parse_one_child(interp, ctors, "syntax-quote")?
            }
            '@' => {
                self.advance();
                self.parse_one_child(interp, ctors, "deref")?
            }
            '~' => {
                self.advance();
                if self.peek() == Some('@') {
                    self.advance();
                    self.parse_one_child(interp, ctors, "unquote-splicing")?
                } else {
                    self.parse_one_child(interp, ctors, "unquote")?
                }
            }
            '#' => self.parse_sharp(interp, ctors)?,
            ':' => self.parse_keyword(interp, ctors)?,
            '"' => self.parse_string_tok(interp, ctors)?,
            '\\' => self.parse_char_token(interp, ctors)?,
            _ => self.parse_token(interp, ctors)?,
        };
        let end_row = self.row;
        let end_col = self.col;
        let meta = PMap::from_iter(vec![
            (Value::Keyword(Keyword::construct("row")), Value::Int(start_row)),
            (Value::Keyword(Keyword::construct("col")), Value::Int(start_col)),
            (Value::Keyword(Keyword::construct("end-row")), Value::Int(end_row)),
            (Value::Keyword(Keyword::construct("end-col")), Value::Int(end_col)),
        ]);
        Ok(Some(Value::attach_meta(node, Value::Map(meta))))
    }

    fn parse_seq(&mut self, interp: &mut Interp, ctors: &PMap, _open: char, close: char, tag: &str) -> R<Value> {
        self.advance(); // opening delim
        let mut children: Vec<Value> = Vec::new();
        loop {
            // Trivia mode: don't pre-skip -- the `_` branch below calls
            // `parse_next`, which (via `dispatch_rc`) turns whitespace/
            // comments into real child nodes instead.
            if !self.trivia {
                self.skip_trivia();
            }
            match self.peek() {
                None => return Err(()), // unmatched, EOF
                Some(c) if c == close => {
                    self.advance();
                    break;
                }
                Some(')') | Some(']') | Some('}') => return Err(()), // mismatched bracket
                _ => {
                    // `Ok(None)` here (fork/non-trivia mode only) is a
                    // trailing `#_form` with nothing left before our
                    // `close` -- transparent skip, matching bare
                    // whitespace: produces no child, loop around to see
                    // `close` on the next iteration (regression:
                    // `#_()))`-shaped trailing discards, e.g.
                    // paredit_test.clj).
                    if let Some(child) = self.parse_next(interp, ctors)? {
                        children.push(child);
                    }
                }
            }
        }
        self.call_ctor(interp, ctors, tag, &[Value::Vector(PVec::from_slice(&children))])
    }

    /// Sigil already consumed by the caller: read exactly one child.
    fn parse_one_child(&mut self, interp: &mut Interp, ctors: &PMap, tag: &str) -> R<Value> {
        let child = self.parse_next(interp, ctors)?.ok_or(())?;
        self.call_ctor(interp, ctors, tag, &[Value::Vector(PVec::from_slice(&[child]))])
    }

    /// `^`/`#^`: meta-node, then value-node (both position-wrapped via
    /// the normal path), then `(update value-node :meta lconj meta-node)`
    /// via the adapter-supplied `:attach-reader-meta` closure. Result
    /// keeps value-node's OWN position, per module doc.
    fn parse_meta_apply(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Value> {
        let meta_node = self.parse_next(interp, ctors)?.ok_or(())?;
        let value_node = self.parse_next(interp, ctors)?.ok_or(())?;
        self.call_ctor(interp, ctors, "attach-reader-meta", &[value_node, meta_node])
    }

    /// `#_`: transparent skip in the common case (see module doc).
    fn parse_uneval(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Option<Value>> {
        if self.peek() == Some('#') && self.peek_at(1) == Some('?') {
            return Err(()); // #_#?...: rare disguise case, not attempted
        }
        let discarded = self.parse_next(interp, ctors)?.ok_or(())?;
        let im = self.call_ctor(interp, ctors, "ignore-meta", &[Value::Vector(PVec::from_slice(&[discarded]))])?;
        if is_truthy(&im) {
            // clj-kondo `:clj-kondo/ignore` lint-directive hint (`#_:clj-
            // kondo/ignore form` / `#_{:clj-kondo/ignore [...]} form`,
            // e.g. jar `sci/impl/fns.cljc`, `aaaa_this_has_to_be_first/
            // pprint.clj`): real semantics is
            // `(vary-meta (parse-next reader context) into im)` --
            // parse the NEXT real node as usual, then merge `im`'s
            // entries into ITS meta (`im`'s keys win on conflict, same
            // as `into`/`conj`). `:clj-kondo/ignore-id` is a fresh
            // gensym per call (even in the interpreted parser, run to
            // run) -- never oracle-compared for equality by design.
            let Value::Map(im_map) = im else { return Err(()) };
            let next_node = self.parse_next(interp, ctors)?.ok_or(())?;
            let (inner, mut next_meta) = match &next_node {
                Value::Meta(m) => {
                    let meta = match &m.meta {
                        Value::Map(pm) => pm.clone(),
                        _ => PMap::new(),
                    };
                    (m.inner.clone(), meta)
                }
                _ => (next_node.clone(), PMap::new()),
            };
            for (k, v) in im_map.iter() {
                next_meta.insert(k.clone(), v.clone());
            }
            return Ok(Some(Value::attach_meta(inner, Value::Map(next_meta))));
        }
        // Nothing left before our enclosing `close`/EOF (`#_{})`-shaped
        // trailing discard, e.g. paredit_test.clj's `#_()))`): produces
        // no node, same as bare whitespace -- don't delegate into
        // `parse_next`, whose top dispatch treats a bare close-delimiter
        // as a hard mismatched-bracket error (correct for real top-level
        // input, wrong for "what follows the discard" here).
        if is_boundary_close(self.peek()) {
            return Ok(None);
        }
        self.parse_next(interp, ctors)
    }

    fn parse_sharp(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Value> {
        self.advance(); // '#'
        match self.peek() {
            None => Err(()),
            Some('{') => self.parse_seq(interp, ctors, '{', '}', "set"),
            Some('(') => self.parse_seq(interp, ctors, '(', ')', "fn"),
            Some('\'') => {
                self.advance();
                self.parse_one_child(interp, ctors, "var")
            }
            Some('=') => {
                self.advance();
                self.parse_one_child(interp, ctors, "eval")
            }
            Some('"') => {
                let lines = self.read_string_data()?;
                self.call_ctor(interp, ctors, "regex", &[Value::Str(Str::from(lines.join("\n").as_str()))])
            }
            Some(':') => self.parse_namespaced_map(interp, ctors),
            Some('?') => {
                self.advance();
                let tag = match self.peek() {
                    Some('(') => "?",
                    Some('@') => {
                        self.advance();
                        "?@"
                    }
                    _ => return Err(()), // malformed reader-conditional shape
                };
                let tag_node = self.bare_token_symbol(interp, ctors, tag)?;
                let form_node = self.parse_next(interp, ctors)?.ok_or(())?;
                self.call_ctor(interp, ctors, "reader-macro", &[Value::Vector(PVec::from_slice(&[tag_node, form_node]))])
            }
            Some('!') => Err(()), // shebang comment: rare, not attempted
            Some('#') => Err(()), // ##Inf/##NaN/##-Inf: rare, not attempted
            _ => {
                // generic reader macro / tagged literal: #js{...}, #inst "...", ...
                let tag = self.parse_next(interp, ctors)?.ok_or(())?;
                let form = self.parse_next(interp, ctors)?.ok_or(())?;
                self.call_ctor(interp, ctors, "reader-macro", &[Value::Vector(PVec::from_slice(&[tag, form]))])
            }
        }
    }

    /// A synthetic token-node for a literal like `?`/`?@` used by the
    /// `#?`/`#?@` shorthand -- built bare, with NO position wrap, exactly
    /// matching `(node/token-node (symbol "?"))` in the fork.
    fn bare_token_symbol(&self, interp: &mut Interp, ctors: &PMap, s: &str) -> R<Value> {
        self.call_ctor(
            interp,
            ctors,
            "token",
            &[Value::Sym(Symbol { ns: None, name: Str::from(s) }), Value::Str(Str::from(s))],
        )
    }

    /// `#:ns{}` / `#::{}` / `#::alias{}`.
    fn parse_namespaced_map(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Value> {
        self.advance(); // the ':' already peeked
        let aliased = if self.peek() == Some(':') {
            self.advance();
            true
        } else {
            false
        };
        let mut name = String::new();
        loop {
            match self.peek() {
                None => return Err(()),
                Some('{') => break,
                Some(_) => name.push(self.advance().unwrap()),
            }
        }
        let name = name.trim();
        let k = if name.is_empty() {
            if !aliased {
                return Err(()); // bare "#:{...}" (no namespace, not aliased) is invalid
            }
            Value::Keyword(Keyword::construct("__current-ns__"))
        } else {
            Value::Keyword(Keyword::construct(name))
        };
        // `namespaced-map-node`'s `ns` arg is a bare KEYWORD-NODE record
        // (it reads `(:k ns)`/`(:namespaced? ns)`), built directly via
        // the "keyword" ctor with NO position wrap -- matches the fork's
        // raw `(node/keyword-node k aliased?)` call in `parse-map-ns`.
        let map_ns_node = self.call_ctor(interp, ctors, "keyword", &[k, Value::Bool(aliased)])?;
        let the_map = self.parse_next(interp, ctors)?.ok_or(())?;
        self.call_ctor(
            interp,
            ctors,
            "namespaced-map",
            &[map_ns_node, Value::Bool(aliased), Value::Vector(PVec::from_slice(&[the_map]))],
        )
    }

    fn parse_keyword(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Value> {
        self.advance(); // ':'
        let namespaced = if self.peek() == Some(':') {
            self.advance();
            true
        } else {
            false
        };
        let name = self.read_raw_token();
        if name.is_empty() {
            return Err(());
        }
        let k = Value::Keyword(Keyword::construct(&name));
        // `parser/keyword.clj`'s `parse-keyword` calls the 1-arg ctor
        // (namespaced? left as `nil`, not `false`) for a plain `:kw`,
        // and only passes an explicit `true` for `::kw` -- some
        // downstream code (clj-kondo's usage analysis) distinguishes
        // "absent" from "false", so this arity difference is load-
        // bearing, not cosmetic.
        if namespaced {
            self.call_ctor(interp, ctors, "keyword", &[k, Value::Bool(true)])
        } else {
            self.call_ctor(interp, ctors, "keyword", &[k])
        }
    }

    /// Raw (still-escaped) lines between the quotes -- multi-line
    /// capable, matching `parser/utils.clj`'s `read-string-data`: a real
    /// embedded newline splits `lines` (the newline itself is dropped,
    /// not stored); `\`-escape parity (not a fixed 2-char skip) decides
    /// whether a `"` closes the string.
    fn read_string_data(&mut self) -> R<Vec<String>> {
        self.advance(); // opening quote
        let mut lines = Vec::new();
        let mut buf = String::new();
        let mut escape = false;
        loop {
            match self.advance() {
                None => return Err(()), // unterminated
                Some('"') if !escape => {
                    lines.push(buf);
                    return Ok(lines);
                }
                Some('\n') => {
                    lines.push(std::mem::take(&mut buf));
                    escape = false;
                }
                Some(c) => {
                    buf.push(c);
                    escape = !escape && c == '\\';
                }
            }
        }
    }

    fn parse_string_tok(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Value> {
        let lines = self.read_string_data()?;
        let vec: Vec<Value> = lines.iter().map(|s| Value::Str(Str::from(s.as_str()))).collect();
        self.call_ctor(interp, ctors, "string", &[Value::Vector(PVec::from_slice(&vec))])
    }

    /// `\c`, `\newline`, `\uXXXX`, `\oNNN`, `\\` -- matches
    /// `read-to-char-boundary`: the char right after `\` is taken
    /// unconditionally (even if it's itself a boundary char, e.g. `\(`),
    /// then extended only if that first char wasn't itself `\`.
    fn parse_char_token(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Value> {
        let start = self.pos;
        self.advance(); // backslash
        let c1 = self.advance().ok_or(())?;
        let mut raw = String::new();
        raw.push(c1);
        if c1 != '\\' {
            while !is_ws_or_boundary(self.peek()) {
                raw.push(self.advance().unwrap());
            }
        }
        let ch = decode_char_literal(&raw).ok_or(())?;
        let orig: String = self.chars[start..self.pos].iter().collect();
        self.call_ctor(interp, ctors, "token", &[Value::Char(ch), Value::Str(Str::from(orig.as_str()))])
    }

    fn parse_token(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Value> {
        let tok = self.read_raw_token();
        if tok.is_empty() {
            return Err(());
        }
        match tok.as_str() {
            "nil" => return self.call_ctor(interp, ctors, "token", &[Value::Nil, Value::Str(Str::from("nil"))]),
            "true" => {
                return self.call_ctor(interp, ctors, "token", &[Value::Bool(true), Value::Str(Str::from("true"))])
            }
            "false" => {
                return self.call_ctor(interp, ctors, "token", &[Value::Bool(false), Value::Str(Str::from("false"))])
            }
            _ => {}
        }
        let c0 = tok.chars().next().unwrap();
        let looks_numeric = c0.is_ascii_digit()
            || ((c0 == '+' || c0 == '-') && tok.chars().nth(1).is_some_and(|c| c.is_ascii_digit()));
        if looks_numeric {
            let v = parse_number(&tok).ok_or(())?;
            return self.call_ctor(interp, ctors, "token", &[v, Value::Str(Str::from(tok.as_str()))]);
        }
        let mut tok = tok;
        self.read_symbol_extra(&mut tok);
        let sym = parse_symbol(&tok).ok_or(())?;
        self.call_ctor(interp, ctors, "token", &[Value::Sym(sym), Value::Str(Str::from(tok.as_str()))])
    }

    // ---- Round 3 (real rewrite-clj): trivia-preserving dispatch ----

    /// Reads through and including the next linebreak (or to EOF),
    /// matching `reader/read-include-linebreak` -- used for `;`/`#!`
    /// comment content.
    fn read_include_linebreak(&mut self) -> String {
        let mut s = String::new();
        loop {
            match self.peek() {
                None => break,
                Some(c) => {
                    self.advance();
                    s.push(c);
                    if c == '\n' || c == '\r' {
                        break;
                    }
                }
            }
        }
        s
    }

    /// One homogeneous run, matching `node/whitespace.cljc`'s
    /// `whitespace-nodes` (`partition-by classify-whitespace` -- a fresh
    /// node starts at EVERY class change, comma/newline/plain-space
    /// alike): linebreaks group into a `newline` node, commas into a
    /// `comma` node, anything else whitespace-shaped (`space?`:
    /// whitespace minus linebreak AND minus comma -- `whitespace-node`'s
    /// own `:pre` asserts every char is `space?`, which rejects an
    /// embedded comma) into a `whitespace` node.
    ///
    /// Regression (W1 corpus census: `lib/src/clojure_lsp/queries.clj`'s
    /// `cond`-clause-separator style, ` , (filter ...)`): the previous
    /// version let a plain-whitespace run swallow a MID-run comma too
    /// (misreading rewrite-clj's real behavior), producing e.g. `" , "`
    /// as ONE whitespace node -- the ctor's own `:pre` check then threw,
    /// which is exactly the "genuine parse error" case `parse_next`
    /// converts to a hard fallback.
    fn lex_whitespace_run(&mut self) -> (String, &'static str) {
        let c = self.peek().unwrap();
        if c == '\n' || c == '\r' {
            let mut s = String::new();
            while matches!(self.peek(), Some(c2) if c2 == '\n' || c2 == '\r') {
                s.push(self.advance().unwrap());
            }
            (s, "newline")
        } else if c == ',' {
            let mut s = String::new();
            while self.peek() == Some(',') {
                s.push(self.advance().unwrap());
            }
            (s, "comma")
        } else {
            let mut s = String::new();
            while matches!(self.peek(), Some(c2) if c2.is_whitespace() && c2 != ',' && c2 != '\n' && c2 != '\r') {
                s.push(self.advance().unwrap());
            }
            (s, "whitespace")
        }
    }

    /// Trivia-mode entry point: dispatch, wrap with position meta like
    /// every other node (no bypass -- unlike the clj-kondo fork,
    /// vanilla rewrite-clj's `^`/`#^`/`#_` are ordinary ctor'd nodes
    /// that go through the SAME `read-with-meta` wrap as everything
    /// else). Returns the tag alongside the value so `parse_printables`
    /// can tell printable-only (trivia/comment/uneval) nodes apart from
    /// real ones without a round-trip back into Clojure.
    fn dispatch_rc(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Option<(Value, &'static str)>> {
        let c = match self.peek() {
            None => return Ok(None),
            Some(c) => c,
        };
        let start_row = self.row;
        let start_col = self.col;
        let (node, tag): (Value, &'static str) = match c {
            c if c.is_whitespace() || c == ',' => {
                let (s, tag) = self.lex_whitespace_run();
                (self.call_ctor(interp, ctors, tag, &[Value::Str(Str::from(s.as_str()))])?, tag)
            }
            ';' => {
                self.advance();
                let content = self.read_include_linebreak();
                (
                    self.call_ctor(
                        interp,
                        ctors,
                        "comment",
                        &[Value::Str(Str::from(";")), Value::Str(Str::from(content.as_str()))],
                    )?,
                    "comment",
                )
            }
            '(' => (self.parse_seq(interp, ctors, '(', ')', "list")?, "list"),
            '[' => (self.parse_seq(interp, ctors, '[', ']', "vector")?, "vector"),
            '{' => (self.parse_seq(interp, ctors, '{', '}', "map")?, "map"),
            ')' | ']' | '}' => return Err(()),
            '^' => {
                self.advance();
                let children = self.parse_printables(interp, ctors, 2, false)?;
                (self.call_ctor(interp, ctors, "meta", &[Value::Vector(PVec::from_slice(&children))])?, "meta")
            }
            '\'' => {
                let children = self.parse_printables(interp, ctors, 1, true)?;
                (self.call_ctor(interp, ctors, "quote", &[Value::Vector(PVec::from_slice(&children))])?, "quote")
            }
            '`' => {
                let children = self.parse_printables(interp, ctors, 1, true)?;
                (
                    self.call_ctor(interp, ctors, "syntax-quote", &[Value::Vector(PVec::from_slice(&children))])?,
                    "syntax-quote",
                )
            }
            '@' => {
                let children = self.parse_printables(interp, ctors, 1, true)?;
                (self.call_ctor(interp, ctors, "deref", &[Value::Vector(PVec::from_slice(&children))])?, "deref")
            }
            '~' => {
                self.advance();
                if self.peek() == Some('@') {
                    let children = self.parse_printables(interp, ctors, 1, true)?;
                    (
                        self.call_ctor(
                            interp,
                            ctors,
                            "unquote-splicing",
                            &[Value::Vector(PVec::from_slice(&children))],
                        )?,
                        "unquote-splicing",
                    )
                } else {
                    let children = self.parse_printables(interp, ctors, 1, false)?;
                    (self.call_ctor(interp, ctors, "unquote", &[Value::Vector(PVec::from_slice(&children))])?, "unquote")
                }
            }
            '#' => self.parse_sharp_rc(interp, ctors)?,
            ':' => (self.parse_keyword(interp, ctors)?, "keyword"),
            '"' => (self.parse_string_tok(interp, ctors)?, "string"),
            '\\' => (self.parse_char_token(interp, ctors)?, "token"),
            _ => (self.parse_token(interp, ctors)?, "token"),
        };
        let end_row = self.row;
        let end_col = self.col;
        let meta = PMap::from_iter(vec![
            (Value::Keyword(Keyword::construct("row")), Value::Int(start_row)),
            (Value::Keyword(Keyword::construct("col")), Value::Int(start_col)),
            (Value::Keyword(Keyword::construct("end-row")), Value::Int(end_row)),
            (Value::Keyword(Keyword::construct("end-col")), Value::Int(end_col)),
        ]);
        Ok(Some((Value::attach_meta(node, Value::Map(meta)), tag)))
    }

    /// `parser/core.cljc`'s `parse-printables`: read nodes (trivia
    /// included, via `dispatch_rc`) until `n` non-printable-only ones
    /// have been seen; `ignore_first` consumes one char first (the
    /// prefix sigil, when the caller hasn't already).
    fn parse_printables(&mut self, interp: &mut Interp, ctors: &PMap, n: usize, ignore_first: bool) -> R<Vec<Value>> {
        if ignore_first {
            self.advance();
        }
        let mut children = Vec::new();
        let mut real = 0usize;
        while real < n {
            let (v, tag) = self.dispatch_rc(interp, ctors)?.ok_or(())?;
            if !matches!(tag, "whitespace" | "newline" | "comma" | "comment" | "uneval") {
                real += 1;
            }
            children.push(v);
        }
        Ok(children)
    }

    fn parse_sharp_rc(&mut self, interp: &mut Interp, ctors: &PMap) -> R<(Value, &'static str)> {
        self.advance(); // '#'
        match self.peek() {
            None => Err(()),
            Some('#') => Err(()), // ##Inf/##NaN/##-Inf: rare, not attempted
            Some('!') => {
                self.advance();
                let content = self.read_include_linebreak();
                Ok((
                    self.call_ctor(
                        interp,
                        ctors,
                        "comment",
                        &[Value::Str(Str::from("#!")), Value::Str(Str::from(content.as_str()))],
                    )?,
                    "comment",
                ))
            }
            Some('{') => Ok((self.parse_seq(interp, ctors, '{', '}', "set")?, "set")),
            Some('(') => Ok((self.parse_seq(interp, ctors, '(', ')', "fn")?, "fn")),
            Some('"') => {
                let lines = self.read_string_data()?;
                Ok((self.call_ctor(interp, ctors, "regex", &[Value::Str(Str::from(lines.join("\n").as_str()))])?, "regex"))
            }
            Some('^') => {
                let children = self.parse_printables(interp, ctors, 2, true)?;
                Ok((
                    self.call_ctor(interp, ctors, "raw-meta", &[Value::Vector(PVec::from_slice(&children))])?,
                    "raw-meta",
                ))
            }
            Some('\'') => {
                let children = self.parse_printables(interp, ctors, 1, true)?;
                Ok((self.call_ctor(interp, ctors, "var", &[Value::Vector(PVec::from_slice(&children))])?, "var"))
            }
            Some('=') => {
                let children = self.parse_printables(interp, ctors, 1, true)?;
                Ok((self.call_ctor(interp, ctors, "eval", &[Value::Vector(PVec::from_slice(&children))])?, "eval"))
            }
            Some('_') => {
                let children = self.parse_printables(interp, ctors, 1, true)?;
                Ok((self.call_ctor(interp, ctors, "uneval", &[Value::Vector(PVec::from_slice(&children))])?, "uneval"))
            }
            Some(':') => Ok((self.parse_namespaced_map_rc(interp, ctors)?, "namespaced-map")),
            Some('?') => {
                self.advance();
                let tag = match self.peek() {
                    Some('(') => "?",
                    Some('@') => {
                        self.advance();
                        "?@"
                    }
                    _ => return Err(()), // malformed reader-conditional shape
                };
                let tag_node = self.bare_token_symbol(interp, ctors, tag)?;
                let mut children = vec![tag_node];
                children.extend(self.parse_printables(interp, ctors, 1, false)?);
                Ok((
                    self.call_ctor(interp, ctors, "reader-macro", &[Value::Vector(PVec::from_slice(&children))])?,
                    "reader-macro",
                ))
            }
            _ => {
                let children = self.parse_printables(interp, ctors, 2, false)?;
                Ok((
                    self.call_ctor(interp, ctors, "reader-macro", &[Value::Vector(PVec::from_slice(&children))])?,
                    "reader-macro",
                ))
            }
        }
    }

    /// `#:ns{}`/`#::{}`/`#::alias{}`: unlike the clj-kondo fork, real
    /// rewrite-clj's qualifier is its OWN node type (`map-qualifier`,
    /// fields `auto-resolved?` + a raw string `prefix`, built bare with
    /// NO position wrap, matching `parse-qualifier` calling
    /// `map-qualifier-node` directly rather than via `parse-next`), and
    /// the whole thing is one flat `children` vector
    /// `[qualifier, trivia..., map]`.
    fn parse_namespaced_map_rc(&mut self, interp: &mut Interp, ctors: &PMap) -> R<Value> {
        self.advance(); // the ':' the sharp-dispatch peeked
        let auto_resolved = if self.peek() == Some(':') {
            self.advance();
            true
        } else {
            false
        };
        let mut prefix = String::new();
        while !is_ws_or_boundary(self.peek()) {
            prefix.push(self.advance().unwrap());
        }
        if prefix.is_empty() && !auto_resolved {
            return Err(()); // "namespaced map expects a namespace"
        }
        let prefix_val = if prefix.is_empty() { Value::Nil } else { Value::Str(Str::from(prefix.as_str())) };
        let qualifier = self.call_ctor(interp, ctors, "map-qualifier", &[Value::Bool(auto_resolved), prefix_val])?;
        let mut children = vec![qualifier];
        loop {
            let (v, tag) = self.dispatch_rc(interp, ctors)?.ok_or(())?;
            children.push(v);
            if matches!(tag, "whitespace" | "newline" | "comma") {
                continue;
            }
            if tag != "map" {
                return Err(()); // "namespaced map expects a map"
            }
            break;
        }
        self.call_ctor(interp, ctors, "namespaced-map", &[Value::Vector(PVec::from_slice(&children))])
    }
}

fn decode_char_literal(raw: &str) -> Option<char> {
    if raw.chars().count() == 1 {
        return raw.chars().next();
    }
    match raw {
        "newline" => return Some('\n'),
        "space" => return Some(' '),
        "tab" => return Some('\t'),
        "backspace" => return Some('\u{8}'),
        "formfeed" => return Some('\u{c}'),
        "return" => return Some('\r'),
        _ => {}
    }
    if let Some(hex) = raw.strip_prefix('u') {
        if hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return char::from_u32(u32::from_str_radix(hex, 16).ok()?);
        }
        return None;
    }
    if let Some(oct) = raw.strip_prefix('o') {
        if (1..=3).contains(&oct.len()) && oct.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            return char::from_u32(u32::from_str_radix(oct, 8).ok()?);
        }
        return None;
    }
    None
}

fn int_value(b: BigIntVal, force_big: bool) -> Value {
    if force_big {
        return Value::BigInt(Arc::new(b));
    }
    match b.to_i64_exact() {
        Some(i) => Value::Int(i),
        None => Value::BigInt(Arc::new(b)),
    }
}

/// Full integer/float/ratio/radix/bigint/bigdecimal grammar, modeled on
/// `clojure.lang.LispReader`'s number regexes -- see module doc for what
/// still falls back (`##Inf`-style symbolic values aren't reached here
/// at all; those are intercepted earlier as `##`).
fn parse_number(tok: &str) -> Option<Value> {
    let (neg, mut body) = match tok.as_bytes().first()? {
        b'-' => (true, &tok[1..]),
        b'+' => (false, &tok[1..]),
        _ => (false, tok),
    };
    if body.is_empty() {
        return None;
    }

    if let Some(slash) = body.find('/') {
        let (a, b) = (&body[..slash], &body[slash + 1..]);
        if a.is_empty()
            || b.is_empty()
            || !a.bytes().all(|c| c.is_ascii_digit())
            || !b.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let abig = BigInt::parse_bytes(a.as_bytes(), 10)?;
        let abig = if neg { -abig } else { abig };
        let bbig = BigInt::parse_bytes(b.as_bytes(), 10)?;
        return Some(match RatioVal::reduce(abig, bbig).ok()? {
            Reduced::Int(n) => match n.to_i64_exact() {
                Some(i) => Value::Int(i),
                None => Value::BigInt(Arc::new(n)),
            },
            Reduced::Ratio(r) => Value::Ratio(Arc::new(r)),
        });
    }

    let force_big = body.ends_with('N');
    let force_dec = body.ends_with('M');
    if force_big || force_dec {
        body = &body[..body.len() - 1];
    }
    if body.is_empty() {
        return None;
    }

    if force_dec {
        let signed = format!("{}{}", if neg { "-" } else { "" }, body);
        return BigDecVal::parse(&signed).map(|d| Value::BigDec(Arc::new(d)));
    }

    if let Some(hexdigits) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return bignum::parse_bigint_radix(hexdigits, 16, neg).map(|b| int_value(b, force_big));
    }
    if let Some(rpos) = body.find(|c: char| c == 'r' || c == 'R') {
        let (radix_str, digits) = (&body[..rpos], &body[rpos + 1..]);
        let radix: u32 = radix_str.parse().ok()?;
        if !(2..=36).contains(&radix) {
            return None;
        }
        return bignum::parse_bigint_radix(digits, radix, neg).map(|b| int_value(b, force_big));
    }
    if body.len() > 1 && body.as_bytes()[0] == b'0' && !body.contains(['.', 'e', 'E']) {
        return bignum::parse_bigint_radix(&body[1..], 8, neg).map(|b| int_value(b, force_big));
    }
    if body.bytes().all(|c| c.is_ascii_digit()) {
        return bignum::parse_bigint_radix(body, 10, neg).map(|b| int_value(b, force_big));
    }
    if force_big {
        return None; // N only valid on an integer literal
    }
    let is_float_shape = body.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c))
        && body.chars().any(|c| c == '.' || c == 'e' || c == 'E');
    if is_float_shape {
        let signed = format!("{}{}", if neg { "-" } else { "" }, body);
        return signed.parse::<f64>().ok().map(Value::Float);
    }
    None
}

/// The namespace/name split is on the FIRST `/` only (matching
/// `reader.clj`'s `parse-symbol`/tools.reader's `str/index-of`), not
/// every `/` -- `parts.len()==2` (Rust `str::split`) was wrong for a
/// qualified symbol whose NAME is itself `/` (regression: W1 corpus
/// census, jar `clj_kondo/impl/var_info_gen.clj`'s `clojure.core//`,
/// the qualified division fn). No further validation beyond that split
/// (digit-leading array-class names etc. are left to the differential
/// oracle, mova/smoke/reader_diff.clj, as the backstop).
fn parse_symbol(tok: &str) -> Option<Symbol> {
    if tok == "/" {
        return Some(Symbol { ns: None, name: Str::from("/") });
    }
    match tok.find('/') {
        None => Some(Symbol { ns: None, name: Str::from(tok) }),
        Some(idx) => {
            let (ns, name) = (&tok[..idx], &tok[idx + 1..]);
            if ns.is_empty() || name.is_empty() || (name != "/" && name.contains('/')) {
                None
            } else {
                Some(Symbol { ns: Some(Str::from(ns)), name: Str::from(name) })
            }
        }
    }
}

fn str_and_ctors(args: &[Value]) -> Option<(String, PMap)> {
    let s = match &args[0] {
        Value::Str(s) => s.as_ref().to_string(),
        _ => return None,
    };
    let ctors = match &args[1] {
        Value::Map(m) => m.clone(),
        _ => return None,
    };
    Some((s, ctors))
}

fn is_trivia_mode(ctors: &PMap) -> bool {
    ctors.contains_key(&Value::Keyword(Keyword::construct("whitespace")))
}

fn parse_string_native(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let Some((s, ctors)) = str_and_ctors(args) else { return Ok(fallback()) };
    let trivia = is_trivia_mode(&ctors);
    let mut p = P { chars: s.chars().collect(), pos: 0, row: 1, col: 1, trivia };
    match p.parse_next(interp, &ctors) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Ok(Value::Nil),
        Err(()) => Ok(fallback()),
    }
}

fn parse_string_all_native(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let Some((s, ctors)) = str_and_ctors(args) else { return Ok(fallback()) };
    let trivia = is_trivia_mode(&ctors);
    let mut p = P { chars: s.chars().collect(), pos: 0, row: 1, col: 1, trivia };
    let mut forms: Vec<Value> = Vec::new();
    loop {
        match p.parse_next(interp, &ctors) {
            Ok(Some(v)) => forms.push(v),
            Ok(None) => break,
            Err(()) => return Ok(fallback()),
        }
    }
    let node_meta = |v: &Value| -> Option<PMap> {
        match v {
            Value::Meta(m) => match &m.meta {
                Value::Map(pm) => Some(pm.clone()),
                _ => None,
            },
            _ => None,
        }
    };
    // Fork mode (clj-kondo): meta of the FIRST node only, matching
    // `parse-all`'s `(with-meta ... (meta (first nodes)))`. Trivia mode
    // (real rewrite-clj): `rewrite-clj.parser/parse-all` instead merges
    // the first node's meta with the LAST node's `:end-row`/`:end-col`
    // (since with trivia, `first`/`last` may themselves be whitespace,
    // not the first/last REAL form) -- replicate that merge exactly.
    let combined_meta = if trivia {
        let first_m = forms.first().and_then(node_meta);
        let last_m = forms.last().and_then(node_meta);
        match (first_m, last_m) {
            (Some(mut fm), Some(lm)) => {
                for k in ["end-row", "end-col"] {
                    if let Some(v) = lm.get(&Value::Keyword(Keyword::construct(k))).cloned() {
                        fm.insert(Value::Keyword(Keyword::construct(k)), v);
                    }
                }
                Value::Map(fm)
            }
            (Some(fm), None) => Value::Map(fm),
            _ => Value::Nil,
        }
    } else {
        match forms.first().and_then(node_meta) {
            Some(pm) => Value::Map(pm),
            None => Value::Nil,
        }
    };
    let node = match p.call_ctor(interp, &ctors, "forms", &[Value::Vector(PVec::from_slice(&forms))]) {
        Ok(v) => v,
        Err(()) => return Ok(fallback()),
    };
    Ok(Value::attach_meta(node, combined_meta))
}

pub fn register(i: &mut Interp) {
    reg_ns(i, "mova.reader", "parse-string", ArityHint::Exact(2), parse_string_native);
    reg_ns(i, "mova.reader", "parse-string-all", ArityHint::Exact(2), parse_string_all_native);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_fn(
        name: &'static str,
        f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
    ) -> Value {
        Value::Native(Arc::new(crate::value::NativeFn::new(name, f)))
    }

    fn test_ctors() -> Value {
        macro_rules! tagctor {
            ($tag:expr) => {
                native_fn($tag, |_i, args| {
                    Ok(Value::Vector(PVec::from_slice(&[&[Value::Keyword(Keyword::construct($tag))], args].concat())))
                })
            };
        }
        let pairs = vec![
            (Value::Keyword(Keyword::construct("token")), tagctor!("token")),
            (Value::Keyword(Keyword::construct("keyword")), tagctor!("keyword")),
            (Value::Keyword(Keyword::construct("string")), tagctor!("string")),
            (Value::Keyword(Keyword::construct("list")), tagctor!("list")),
            (Value::Keyword(Keyword::construct("vector")), tagctor!("vector")),
            (Value::Keyword(Keyword::construct("map")), tagctor!("map")),
            (Value::Keyword(Keyword::construct("set")), tagctor!("set")),
            (Value::Keyword(Keyword::construct("fn")), tagctor!("fn")),
            (Value::Keyword(Keyword::construct("quote")), tagctor!("quote")),
            (Value::Keyword(Keyword::construct("syntax-quote")), tagctor!("syntax-quote")),
            (Value::Keyword(Keyword::construct("unquote")), tagctor!("unquote")),
            (Value::Keyword(Keyword::construct("unquote-splicing")), tagctor!("unquote-splicing")),
            (Value::Keyword(Keyword::construct("deref")), tagctor!("deref")),
            (Value::Keyword(Keyword::construct("var")), tagctor!("var")),
            (Value::Keyword(Keyword::construct("eval")), tagctor!("eval")),
            (Value::Keyword(Keyword::construct("regex")), tagctor!("regex")),
            (Value::Keyword(Keyword::construct("namespaced-map")), tagctor!("namespaced-map")),
            (Value::Keyword(Keyword::construct("reader-macro")), tagctor!("reader-macro")),
            (Value::Keyword(Keyword::construct("forms")), tagctor!("forms")),
            (
                Value::Keyword(Keyword::construct("attach-reader-meta")),
                native_fn("attach-reader-meta", |_i, args| {
                    Ok(Value::Vector(PVec::from_slice(&[
                        Value::Keyword(Keyword::construct("attach-reader-meta")),
                        args[0].clone(),
                        args[1].clone(),
                    ])))
                }),
            ),
            (
                Value::Keyword(Keyword::construct("ignore-meta")),
                native_fn("ignore-meta", |_i, _args| Ok(Value::Nil)),
            ),
        ];
        Value::Map(PMap::from_iter(pairs))
    }

    fn parse(src: &str) -> Value {
        let mut interp = Interp::new();
        let ctors = test_ctors();
        parse_string_native(&mut interp, &[Value::Str(Str::from(src)), ctors]).unwrap()
    }

    fn is_fallback(v: &Value) -> bool {
        v == &fallback()
    }

    fn kw(s: &str) -> Value {
        Value::Keyword(Keyword::construct(s))
    }

    fn super_meta(v: &Value) -> PMap {
        match v {
            Value::Meta(m) => match &m.meta {
                Value::Map(pm) => pm.clone(),
                _ => panic!("expected map meta"),
            },
            other => panic!("expected metadata attached, got {other:?}"),
        }
    }

    #[test]
    fn symbol_token_positions() {
        let v = parse("  foo");
        let meta_val = super_meta(&v);
        assert_eq!(meta_val.get(&kw("row")), Some(&Value::Int(1)));
        assert_eq!(meta_val.get(&kw("col")), Some(&Value::Int(3)));
        assert_eq!(meta_val.get(&kw("end-row")), Some(&Value::Int(1)));
        assert_eq!(meta_val.get(&kw("end-col")), Some(&Value::Int(6)));
    }

    #[test]
    fn number_forms() {
        assert_eq!(parse_number("42"), Some(Value::Int(42)));
        assert_eq!(parse_number("-3.5"), Some(Value::Float(-3.5)));
        assert_eq!(parse_number("0x1F"), Some(Value::Int(31)));
        assert_eq!(parse_number("010"), Some(Value::Int(8)));
        assert_eq!(parse_number("2r101"), Some(Value::Int(5)));
        assert_eq!(parse_number("4/2"), Some(Value::Int(2))); // reduces & collapses
        assert!(matches!(parse_number("1/3"), Some(Value::Ratio(_))));
        assert!(matches!(parse_number("5N"), Some(Value::BigInt(_))));
        assert!(matches!(parse_number("1.5M"), Some(Value::BigDec(_))));
        assert_eq!(parse_number("1e10"), Some(Value::Float(1e10)));
        assert!(matches!(parse_number("99999999999999999999"), Some(Value::BigInt(_))));
    }

    #[test]
    fn char_literals() {
        assert_eq!(decode_char_literal("a"), Some('a'));
        assert_eq!(decode_char_literal("newline"), Some('\n'));
        assert_eq!(decode_char_literal("space"), Some(' '));
        assert_eq!(decode_char_literal("u0041"), Some('A'));
        assert_eq!(decode_char_literal("o101"), Some('A'));
        assert_eq!(decode_char_literal("\\"), Some('\\'));
        assert_eq!(decode_char_literal("bogus"), None);
    }

    #[test]
    fn nil_true_false_and_keyword_and_string() {
        assert!(!is_fallback(&parse("nil")));
        assert!(!is_fallback(&parse("true")));
        assert!(!is_fallback(&parse(":foo/bar")));
        assert!(!is_fallback(&parse("::bar")));
        assert!(!is_fallback(&parse("\"hello\\nworld\"")));
        assert!(!is_fallback(&parse("\"multi\nline\""))); // real embedded newline: now native
        assert!(!is_fallback(&parse("\\a")));
        assert!(!is_fallback(&parse("\\newline")));
    }

    #[test]
    fn collections_and_prefixes_and_reader_macros() {
        for src in [
            "(a b c)", "[1 2 3]", "{:a 1}", "#{1 2}", "#(+ % 1)", "'x", "`x", "~x", "~@x", "@x", "#'x", "#=(+ 1 2)",
            "#:ns{:a 1}", "#::{:a 1}", "#::alias{:a 1}", "#?(:clj 1 :cljs 2)", "#?@(:clj [1])", "#js{:a 1}",
            "#inst \"2020\"", "#\"a.*b\"", "#_ignored real",
        ] {
            assert!(!is_fallback(&parse(src)), "expected native for {src}");
        }
    }

    #[test]
    fn meta_bypasses_generic_wrap() {
        let v = parse("^:private foo");
        assert!(!is_fallback(&v));
    }

    #[test]
    fn unsupported_constructs_fall_back() {
        for src in ["##Inf", "##NaN", "#_#?(:clj 1)"] {
            assert!(is_fallback(&parse(src)), "expected fallback for {src}");
        }
    }

    #[test]
    fn mismatched_and_unterminated_fall_back() {
        for src in ["(a b", "\"unterminated", "(a ]", "#:{:a 1}"] {
            assert!(is_fallback(&parse(src)), "expected fallback for {src}");
        }
    }

    #[test]
    fn uneval_is_transparent() {
        let mut interp = Interp::new();
        let ctors = test_ctors();
        let v = parse_string_native(&mut interp, &[Value::Str(Str::from("#_discarded kept")), ctors]).unwrap();
        let meta_val = super_meta(&v);
        assert_eq!(meta_val.get(&kw("col")), Some(&Value::Int(13)));
    }

    /// Trivia-mode (real rewrite-clj) ctors: same shape as `test_ctors`
    /// plus the `:whitespace`/`:newline`/`:comma`/`:meta`/`:raw-meta`/
    /// `:uneval`/`:comment`/`:map-qualifier` entries `is_trivia_mode`
    /// keys off of and `dispatch_rc` needs.
    fn trivia_test_ctors() -> Value {
        macro_rules! tagctor {
            ($tag:expr) => {
                native_fn($tag, |_i, args| {
                    Ok(Value::Vector(PVec::from_slice(&[&[Value::Keyword(Keyword::construct($tag))], args].concat())))
                })
            };
        }
        let Value::Map(mut pairs) = test_ctors() else { unreachable!() };
        for tag in ["whitespace", "newline", "comma", "meta", "raw-meta", "uneval", "comment", "map-qualifier"] {
            pairs.insert(Value::Keyword(Keyword::construct(tag)), tagctor!(tag));
        }
        Value::Map(pairs)
    }

    // Regression (W1 corpus census: lib/src/clojure_lsp/queries.clj's
    // `cond`-clause-separator style, `test\n  , (action)`): a plain-
    // whitespace run used to swallow a comma embedded mid-run (e.g. the
    // " , " between two forms), producing ONE whitespace node whose
    // string isn't all `space?` chars -- real rewrite-clj's
    // `whitespace-node` ctor asserts that and throws, which surfaced as
    // a hard fallback. Each class (whitespace/comma/newline) must be
    // its own run per `node/whitespace.cljc`'s `partition-by`.
    #[test]
    fn comma_mid_run_splits_from_whitespace() {
        let mut interp = Interp::new();
        let ctors = trivia_test_ctors();
        for src in ["(a , b)", "(a ,)", "[a , b]", "(cond\n  a\n  , (b))"] {
            let v = parse_string_native(&mut interp, &[Value::Str(Str::from(src)), ctors.clone()]).unwrap();
            assert!(!is_fallback(&v), "expected native for {src}, got fallback");
        }
    }

    #[test]
    fn pure_symbol_parser() {
        assert!(parse_symbol("foo").is_some());
        assert!(parse_symbol("foo/bar").is_some());
        assert!(parse_symbol("foo/bar/baz").is_none());
        assert!(parse_symbol("/").is_some());
    }

    // Regression (W1 corpus census: jar `clj_kondo/impl/var_info_gen.clj`'s
    // `clojure.core//`, the namespace-qualified division symbol): the
    // ns/name split is on the FIRST `/` only, so a name that is itself
    // `/` (one MORE slash after the separator) must still parse, not
    // fall back.
    #[test]
    fn symbol_ns_qualified_slash_name() {
        let sym = parse_symbol("clojure.core//").expect("clojure.core// should parse");
        assert_eq!(sym.ns.as_ref().map(|s| s.as_ref()), Some("clojure.core"));
        assert_eq!(sym.name.as_ref(), "/");
        assert!(!is_fallback(&parse("clojure.core//")));
    }

    // Regression (mova-lsp-io / clj-kondo diagnostics parity): a plain
    // symbol may contain a non-leading `:` (`can-move-to-:let?` in
    // clojure-lsp's own transform.clj) -- clj-kondo's rewrite-clj fork
    // reads it as ONE symbol token (see `parser/token.clj`'s
    // `symbol-node`), not a truncated symbol plus a separate keyword.
    #[test]
    fn symbol_with_embedded_colon() {
        let v = parse("can-move-to-:let?");
        let inner = match &v {
            Value::Meta(m) => &m.inner,
            other => other,
        };
        let Value::Vector(items) = inner else { panic!("expected token vector, got {inner:?}") };
        assert_eq!(items[1], Value::Sym(Symbol { ns: None, name: Str::from("can-move-to-:let?") }));
        assert_eq!(items[2], Value::Str(Str::from("can-move-to-:let?")));
    }

    // Regression (W1 corpus census: lib/test/clojure_lsp/feature/
    // paredit_test.clj's `#_()))`, lib/src/clojure_lsp/handlers.clj's
    // `(comment ... #_{})`): a `#_form` as the LAST child before the
    // enclosing collection's close delimiter must be a transparent skip
    // (like whitespace), not a hard fallback -- `parse_uneval`'s "read
    // what follows the discard" used to delegate straight into
    // `parse_next`, which treats a bare close-delimiter as a mismatched-
    // bracket error.
    // Regression (W1 corpus census: jar `sci/impl/fns.cljc`'s
    // `#_{:clj-kondo/ignore [:unused-binding]} (defn fun ...)`, jar
    // `aaaa_this_has_to_be_first/pprint.clj`'s `#_:clj-kondo/ignore
    // (if ...)`): the clj-kondo ignore-hint case used to hard-fallback
    // (`Err`), which -- because `parse-string-all` is whole-string
    // all-or-nothing -- forced the ENTIRE FILE back to the interpreted
    // parser for a single directive comment anywhere in it. Real
    // semantics (`parser/core.clj`'s `read-with-ignore-hint`):
    // `(vary-meta (parse-next reader context) into im)` -- parse the
    // NEXT real node normally, merge `im`'s keys into ITS meta.
    #[test]
    fn ignore_hint_merges_into_next_node_meta() {
        fn ctors_with_ignore_hint() -> Value {
            let Value::Map(mut pairs) = test_ctors() else { unreachable!() };
            pairs.insert(
                Value::Keyword(Keyword::construct("ignore-meta")),
                native_fn("ignore-meta", |_i, args| {
                    let Value::Vector(v) = &args[0] else { return Ok(Value::Nil) };
                    let discarded = match &v[0] {
                        Value::Meta(m) => &m.inner,
                        other => other,
                    };
                    let Value::Vector(items) = discarded else { return Ok(Value::Nil) };
                    if items.len() >= 2 && format!("{:?}", items[1]).contains("clj-kondo/ignore") {
                        return Ok(Value::Map(PMap::from_iter(vec![(
                            Value::Keyword(Keyword::construct("clj-kondo/ignore")),
                            Value::Bool(true),
                        )])));
                    }
                    Ok(Value::Nil)
                }),
            );
            Value::Map(pairs)
        }
        let mut interp = Interp::new();
        let ctors = ctors_with_ignore_hint();
        for src in ["#_:clj-kondo/ignore 42", "#_{:x 1} #_:clj-kondo/ignore [1 2]"] {
            let v = parse_string_native(&mut interp, &[Value::Str(Str::from(src)), ctors.clone()]).unwrap();
            assert!(!is_fallback(&v), "expected native for {src}, got fallback");
            let m = super_meta(&v);
            assert_eq!(m.get(&kw("clj-kondo/ignore")), Some(&Value::Bool(true)), "{src}: meta={m:?}");
            // Position meta from the NEXT (real) node is still present.
            assert!(m.get(&kw("row")).is_some(), "{src}: missing :row in merged meta");
        }
    }

    #[test]
    fn trailing_uneval_before_close() {
        for (src, expected_children) in [("(a #_b)", 1usize), ("[#_a #_b]", 0), ("(#_a #_b #_c)", 0)] {
            let v = parse(src);
            assert!(!is_fallback(&v), "expected native for {src}, got fallback");
            let inner = match &v {
                Value::Meta(m) => &m.inner,
                other => other,
            };
            let Value::Vector(items) = inner else { panic!("expected tag vector, got {inner:?}") };
            let Value::Vector(children) = &items[1] else { panic!("expected children vector") };
            assert_eq!(children.len(), expected_children, "{src}: children={children:?}");
        }
    }

    #[test]
    fn symbol_with_embedded_colon_in_call() {
        let mut interp = Interp::new();
        let ctors = test_ctors();
        let v =
            parse_string_native(&mut interp, &[Value::Str(Str::from("(can-move-to-:let? 1)")), ctors]).unwrap();
        let inner = match &v {
            Value::Meta(m) => &m.inner,
            other => other,
        };
        let Value::Vector(items) = inner else { panic!("expected list vector, got {inner:?}") };
        // items = [:list [<token1> <token2>]]
        let Value::Vector(children) = &items[1] else { panic!("expected children vector") };
        assert_eq!(children.len(), 2, "expected exactly 2 forms in the call, got {children:?}");
    }
}
