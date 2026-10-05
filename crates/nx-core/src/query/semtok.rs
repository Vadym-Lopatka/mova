//! textDocument/semanticTokens/full + /range (feature/semantic_tokens.clj). Legend (source order):
//! types `[namespace type function macro keyword class variable method event interface comment]`,
//! modifiers `[definition defaultLibrary implementation]` (bits 1, 2, 4).
use super::text::Doc;
use super::*;
use crate::cst::{Cst, Kind, NodeId};
use crate::engine::index::Ent;

const T_NAMESPACE: i64 = 0;
const T_TYPE: i64 = 1;
const T_FUNCTION: i64 = 2;
const T_MACRO: i64 = 3;
const T_KEYWORD: i64 = 4;
const T_CLASS: i64 = 5;
const T_VARIABLE: i64 = 6;
const T_METHOD: i64 = 7;
const T_EVENT: i64 = 8;
const T_INTERFACE: i64 = 9;
const T_COMMENT: i64 = 10;
const M_DEFINITION: i64 = 1;
const M_DEFAULT_LIB: i64 = 2;
const M_IMPLEMENTATION: i64 = 4;

/// Absolute token: (row0, col0, length, type, modifier bits).
type Tok = (i64, i64, i64, i64, i64);

fn u16len(s: &str) -> i64 {
    s.encode_utf16().count() as i64
}

/// One element's name range with the `assoc` edits of the JVM token builders.
struct NamePos {
    row: i64,
    col: i64,
    end_col: i64,
}

impl NamePos {
    fn tok(&self, ty: i64, m: i64) -> Tok {
        (self.row - 1, self.col - 1, self.end_col - self.col, ty, m)
    }
    fn with(&self, col: i64, end_col: i64) -> NamePos {
        NamePos { row: self.row, col, end_col }
    }
}

fn is_def_by(pair: (crate::intern::SymId, crate::intern::SymId), ns: &[&str], names: &[&str]) -> bool {
    !pair.1.is_none() && ns.contains(&pair.0.as_str()) && names.contains(&pair.1.as_str())
}

impl<'a> Q<'a> {
    fn element_tokens(&self, e: El, np: &NamePos, out: &mut Vec<Tok>) {
        let fa = self.fa(e.f);
        let i = e.i as usize;
        match e.b {
            B::VarUsage => {
                let u = &fa.var_usages[i];
                let name = u.name.as_str();
                let has_alias = !u.alias.is_none();
                let trio = |out: &mut Vec<Tok>, pre: i64, last: i64| {
                    let slash = np.col + pre;
                    out.push(np.with(np.col, slash).tok(T_TYPE, 0));
                    out.push(np.with(slash, slash + 1).tok(T_EVENT, 0));
                    out.push(np.with(slash + 1, np.end_col).tok(last, 0));
                };
                if u.macro_ && !has_alias {
                    out.push(np.tok(T_MACRO, 0));
                } else if u.macro_ && has_alias {
                    trio(out, u16len(u.alias.as_str()), T_MACRO);
                } else if has_alias || (u16len(u.to.as_str()) + 1 + u16len(name) == np.end_col - np.col) {
                    let pre = if has_alias { u.alias.as_str() } else { u.to.as_str() };
                    trio(out, u16len(pre), T_FUNCTION);
                } else if u.to == crate::analyzer::syms().unknown_ns {
                } else if name.starts_with('*') && name.ends_with('*') && u16len(name) > 2 {
                    out.push(np.tok(T_VARIABLE, M_DEFAULT_LIB));
                } else {
                    out.push(np.tok(T_FUNCTION, 0));
                }
            }
            B::VarDef => {
                let d = &fa.var_definitions[i];
                let by = [d.defined_by, d.defined_by_lint_as];
                let proto = by.iter().any(|&p| is_def_by(p, &["clojure.core"], &["defprotocol", "definterface"]));
                if d.protocol_ns.is_none() && proto {
                    out.push(np.tok(T_INTERFACE, 0));
                } else if by.iter().any(|p| !p.1.is_none()) {
                    out.push(np.tok(T_FUNCTION, M_DEFINITION));
                }
            }
            B::Local | B::LocalUsage => out.push(np.tok(T_VARIABLE, 0)),
            B::NsDef => out.push(np.tok(T_NAMESPACE, 0)),
            B::KwDef | B::KwUsage => {
                let k = &fa.keywords[i];
                if k.flags & KW_KEYS_DESTR != 0 {
                    return;
                }
                let has_ns = !k.ns.is_none();
                let prefix = k.flags & KW_PREFIX != 0;
                let auto = k.flags & KW_AUTO != 0;
                let has_alias = !k.alias.is_none();
                if has_ns && (!auto || has_alias) && !prefix {
                    let lead = if has_alias { 2 } else { 1 };
                    let q = if has_alias { k.alias } else { k.ns };
                    let slash = lead + np.col + u16len(q.as_str());
                    out.push(np.with(np.col + lead, slash).tok(T_TYPE, 0));
                    out.push(np.with(slash, slash + 1).tok(T_EVENT, 0));
                    out.push(np.with(slash + 1, np.end_col).tok(T_KEYWORD, 0));
                } else {
                    let off = if has_ns && !prefix { 2 } else { 1 };
                    out.push(np.with(np.col + off, np.end_col).tok(T_KEYWORD, 0));
                }
            }
            B::JavaClassUsage | B::JavaClassDef => out.push(np.tok(T_CLASS, 0)),
            B::InstInv => out.push(np.tok(T_METHOD, 0)),
            B::ProtoImpl => out.push(np.tok(T_METHOD, M_IMPLEMENTATION)),
            _ => {}
        }
    }

    /// `element->token-type`: (token type name, modifier names JSON) of one element.
    pub fn element_token_types(&self, e: El) -> Vec<(&'static str, String)> {
        let p = self.name_pos(e);
        let np = NamePos { row: p.row as i64, col: p.col as i64, end_col: p.end_col as i64 };
        let mut v = Vec::new();
        self.element_tokens(e, &np, &mut v);
        const TYPES: [&str; 11] = ["namespace", "type", "function", "macro", "keyword", "class", "variable", "method", "event", "interface", "comment"];
        const MODS: [&str; 3] = ["definition", "defaultLibrary", "implementation"];
        v.into_iter()
            .map(|(_, _, _, t, m)| {
                let names: Vec<String> = (0..3).filter(|b| m & (1 << b) != 0).map(|b| format!("\"{}\"", MODS[b])).collect();
                (TYPES.get(t as usize).copied().unwrap_or("namespace"), format!("[{}]", names.join(",")))
            })
            .collect()
    }

    /// Elements of the file in JVM token order: `(row, col)` of the name, ties by bucket-iteration order.
    fn sorted_elements(&self, f: FileId) -> Vec<(Ent, El)> {
        let pi = self.pos_idx(f);
        let mut v: Vec<Ent> = pi.ents.iter().map(|e| e.ent()).chain(pi.multi.iter().copied()).collect();
        v.sort_by_key(|e| (e.row, e.col, pi.rank[e.b as usize], e.i));
        v.into_iter().map(|e| (e, El { f, b: e.b, i: e.i })).collect()
    }

    fn tokens(&self, f: FileId, rows: Option<(u32, u32)>) -> Vec<Tok> {
        let mut out = Vec::new();
        for (e, el) in self.sorted_elements(f) {
            if let Some((r1, r2)) = rows {
                if e.row < r1 || e.end_row > r2 {
                    continue;
                }
            }
            let np = NamePos { row: e.row as i64, col: e.col as i64, end_col: e.end_col as i64 };
            self.element_tokens(el, &np, &mut out);
        }
        out
    }
}

/// `#_` nodes as rewrite-clj `find-tag z/next :uneval` + `z/right` walks them: after a hit the search resumes at the
/// hit's right sibling; a hit without right sibling ends the search.
fn uneval_tokens(text: &str) -> Vec<Tok> {
    let doc = Doc::new(text);
    let cst = &doc.cst;
    let mut out = Vec::new();
    let mut cur = first_in_preorder(cst);
    while let Some(n) = cur {
        // find next uneval at or after n in preorder
        let Some(u) = find_uneval(&doc, n) else { break };
        let p = cst.pos(u);
        out.push((p.row as i64 - 1, p.col as i64 - 1, u16len(cst.text(u)), T_COMMENT, 0));
        cur = right_sibling(&doc, u);
    }
    out
}

fn first_in_preorder(cst: &Cst) -> Option<NodeId> {
    cst.children(cst.root()).first().copied()
}

fn right_sibling(doc: &Doc, n: NodeId) -> Option<NodeId> {
    let p = doc.parent(n)?;
    let ch = doc.cst.children(p);
    let k = ch.iter().position(|&c| c == n)?;
    ch.get(k + 1).copied()
}

/// Next node in document preorder (`z/next`), None at the end.
fn next_preorder(doc: &Doc, n: NodeId) -> Option<NodeId> {
    if let Some(&c) = doc.cst.children(n).first() {
        return Some(c);
    }
    let mut x = n;
    loop {
        if let Some(r) = right_sibling(doc, x) {
            return Some(r);
        }
        x = doc.parent(x)?;
    }
}

fn find_uneval(doc: &Doc, from: NodeId) -> Option<NodeId> {
    let mut n = Some(from);
    while let Some(x) = n {
        if doc.cst.kind(x) == Kind::Uneval {
            return Some(x);
        }
        n = next_preorder(doc, x);
    }
    None
}

fn relative(toks: &[Tok]) -> String {
    let mut s = String::from("{\"data\":[");
    let (mut pr, mut pc) = (0i64, 0i64);
    for (k, &(r, c, l, t, m)) in toks.iter().enumerate() {
        let (dr, dc) = if k == 0 { (r, c) } else if r == pr { (0, c - pc) } else { (r - pr, c) };
        if k > 0 {
            s.push(',');
        }
        let _ = write!(s, "{dr},{dc},{l},{t},{m}");
        pr = r;
        pc = c;
    }
    s.push_str("]}");
    s
}

pub fn full(q: &Q, uri: &str) -> String {
    let Some(f) = q.s.id(uri) else { return "null".into() };
    let mut toks = q.tokens(f, None);
    if let Some(t) = q.entry(f).text().as_deref() {
        if t.contains("#_") {
            toks.extend(uneval_tokens(t));
        }
    }
    toks.sort_by_key(|t| (t.0, t.1));
    relative(&toks)
}

/// `range` = (start line, end line), 0-based LSP lines; only rows are compared (JVM `element-inside-range?`).
pub fn range(q: &Q, uri: &str, start_line: u32, end_line: u32) -> String {
    let Some(f) = q.s.id(uri) else { return "null".into() };
    relative(&q.tokens(f, Some((start_line + 1, end_line + 1))))
}
