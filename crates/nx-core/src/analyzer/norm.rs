//! Tree normalization before analysis: drops `#_` forms and resolves reader conditionals for one language.
use crate::cst::*;
use crate::intern::intern;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Clj,
    Cljs,
}

/// True when the tree has nodes `normalize` must rewrite.
pub fn needs_norm(c: &Cst) -> bool {
    c.nodes.iter().any(|n| matches!(n.kind, Kind::Uneval | Kind::ReaderCond))
}

fn select(c: &Cst, rc: NodeId, lang: Lang, out: &mut Vec<NodeId>, br: &mut Vec<NodeId>) {
    let Some(list) = c.children(rc).iter().copied().find(|&x| c.kind(x) != Kind::Uneval) else { return };
    let want = intern(if lang == Lang::Clj { "clj" } else { "cljs" });
    let dflt = intern("default");
    let kids: Vec<NodeId> = c.children(list).iter().copied().filter(|&x| c.kind(x) != Kind::Uneval).collect();
    // kondo `process-reader-conditional`: the first exact feature match wins (and carries `:branch`);
    // otherwise the first `:default` value.
    let mut default: Option<NodeId> = None;
    let mut chosen: Option<(NodeId, bool)> = None;
    let mut i = 0;
    while i + 1 < kids.len() {
        let k = kids[i];
        if c.kind(k) == Kind::Keyword && c.ns(k).is_none() {
            if c.name(k) == want {
                chosen = Some((kids[i + 1], true));
                break;
            }
            if c.name(k) == dflt && default.is_none() {
                default = Some(kids[i + 1]);
            }
        }
        i += 2;
    }
    let Some((f, exact)) = chosen.or(default.map(|d| (d, false))) else { return };
    if c.flags(rc) & F_SPLICING != 0 {
        if matches!(c.kind(f), Kind::Vector | Kind::List) {
            if exact {
                br.push(f);
            }
            for &g in c.children(f) {
                if exact {
                    rec(c, g, br);
                }
                push(c, g, lang, out, br);
            }
        }
    } else {
        if exact {
            rec(c, f, br);
        }
        push(c, f, lang, out, br);
    }
}

/// Nodes picked by a reader conditional carry kondo's `:branch` metadata.
fn rec(c: &Cst, x: NodeId, br: &mut Vec<NodeId>) {
    if !matches!(c.kind(x), Kind::Uneval | Kind::ReaderCond) {
        br.push(x);
    }
}

fn push(c: &Cst, x: NodeId, lang: Lang, out: &mut Vec<NodeId>, br: &mut Vec<NodeId>) {
    match c.kind(x) {
        Kind::Uneval => {}
        Kind::ReaderCond => select(c, x, lang, out, br),
        _ => out.push(x),
    }
}

/// Rewrite child lists so they contain neither `Uneval` nor `ReaderCond` nodes.
pub fn normalize(c: &mut Cst, lang: Lang) -> Vec<NodeId> {
    let mut br: Vec<NodeId> = Vec::new();
    let n = c.nodes.len();
    let mut buf: Vec<NodeId> = Vec::new();
    for i in 0..n {
        let id = NodeId(i as u32);
        if !Cst::is_container(c.kind(id)) || c.kind(id) == Kind::ReaderCond {
            continue;
        }
        let kids = c.children(id);
        if !kids.iter().any(|&k| matches!(c.kind(k), Kind::Uneval | Kind::ReaderCond)) {
            continue;
        }
        buf.clear();
        for &k in c.children(id) {
            push(c, k, lang, &mut buf, &mut br);
        }
        let b = std::mem::take(&mut buf);
        c.set_children(id, &b);
        buf = b;
    }
    br
}
