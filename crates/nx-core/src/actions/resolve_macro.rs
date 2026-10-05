//! resolve-macro-as: asks how to resolve the macro and where to save the clj-kondo `:lint-as` setting.
use super::exec::{ask, Ctx, Out};
use super::rz::*;
use super::tree::{Tag, Tk, Tree};

const KNOWN: [&str; 10] = [
    "clojure.core/def",
    "clojure.core/defn",
    "clojure.core/let",
    "clojure.core/fn",
    "clojure.core/for",
    "clojure.core/->",
    "clojure.core/->>",
    "clojure.core/as->",
    "clojure.test/deftest",
    "clj-kondo.lint-as/def-catch-all",
];

fn skip_right_ws_or_comment(l: &Loc) -> Loc {
    // z/skip z/right (not rightmost and (ws-or-comment or uneval))
    let mut c = l.clone();
    loop {
        if c.rightmost_p() || !(is_wsc(c.tag()) || c.tag() == Tag::Uneval) {
            return c;
        }
        match c.right() {
            Some(n) => c = n,
            None => return c,
        }
    }
}

fn count_uncommented(n: &NR) -> usize {
    n.kids.iter().filter(|k| !is_wsc(k.tag) && k.tag != Tag::Uneval).count()
}

fn indent_or_space(z: Loc, key_count: usize, align: Option<super::tree::Meta>) -> Loc {
    let cur = z.meta();
    let align_ok = align.map_or(false, |a| key_count == 1 || z.tag() == Tag::Comment || z.tag() == Tag::Uneval || Some(a.row) != cur.map(|c| c.row));
    if align_ok {
        let spaces_n = align.unwrap().col as i64 - 1;
        let mut r = z;
        if spaces_n > 0 {
            r = r.insert_space_right(spaces_n as usize);
        }
        if r.tag() != Tag::Comment {
            r = r.insert_newline_right(1);
        }
        r
    } else {
        z.insert_space_right(1)
    }
}

fn nil_node() -> NR {
    leaf(Tag::Token, Tk::Const, "nil")
}

/// `update*` on a forms/map node: `f(existing value node or nil node) -> new value`.
fn update_star(forms: NR, key: &str, f: &dyn Fn(NR) -> NR) -> Option<NR> {
    let root = Loc::of_node(if forms.tag == Tag::Forms { forms.clone() } else { super::rz::forms(vec![forms.clone()]) });
    let mut zloc = root.down().or_else(|| Some(root.clone()))?;
    // skip until token / map / vector
    while !matches!(zloc.tag(), Tag::Token | Tag::Map | Tag::Vector) {
        zloc = zloc.right()?;
    }
    let is_nil = zloc.tag() == Tag::Token && zloc.node.text == "nil";
    let length = count_uncommented(&zloc.node);
    if is_nil {
        zloc = zloc.replace(inner(Tag::Map, vec![]));
    }
    let length = if is_nil { 0 } else { length };
    let comment_child = if length == 0 {
        zloc.node.kids.iter().position(|k| k.tag == Tag::Comment || k.tag == Tag::Uneval)
    } else {
        None
    };
    let empty = (is_nil || length == 0) && comment_child.is_none();
    let key_node = keyword(key);
    if empty {
        let l = zloc.append_child(key_node).append_child(nil_node());
        let new_root = l.root();
        return update_star(new_root, key, f);
    }
    if zloc.tag() != Tag::Map {
        return None;
    }
    let (start, align): (Loc, Option<super::tree::Meta>) = if let Some(ci) = comment_child {
        // (-> zloc z/down* skip-right-to-last-non-ws) aligned to the comment
        let d = zloc.down_raw()?;
        let mut last = d.rightmost_raw();
        while is_ws(last.tag()) {
            last = last.left_raw()?;
        }
        (last, zloc.node.kids[ci].meta)
    } else {
        let first = skip_right_ws_or_comment(&zloc.down()?);
        let m = first.meta();
        (first, m)
    };
    let mut key_count = 0usize;
    let mut z = start;
    loop {
        if z.rightmost_p() {
            let l = z.insert_right_raw(key_node.clone());
            let l = indent_or_space(l, key_count, align);
            let l = l.right()?;
            let l = l.insert_right(f(nil_node()));
            return Some(l.root());
        }
        if z.tag() == Tag::Token && z.node.text == key {
            let v = skip_right_ws_or_comment(&z.right()?);
            let nv = f(v.node.clone());
            return Some(v.replace(nv).root());
        }
        key_count += 1;
        z = skip_right_ws_or_comment(&z);
        z = z.right()?;
        z = skip_right_ws_or_comment(&z);
    }
}

/// `(r/assoc-in node [k1 k2] v)` for symbol key / symbol value.
fn assoc_in(forms: NR, k1: &str, k2: &str, v: &str) -> Option<NR> {
    let vn = token_sym(v);
    update_star(forms, k1, &|old: NR| {
        // (assoc-in old [k2] v) == assoc old k2 v
        let assoc = (|| -> Option<NR> {
            let root = Loc::of_node(super::rz::forms(vec![old.clone()]));
            let mut zloc = root.down()?;
            while !matches!(zloc.tag(), Tag::Token | Tag::Map | Tag::Vector) {
                zloc = zloc.right()?;
            }
            let is_nil = zloc.tag() == Tag::Token && zloc.node.text == "nil";
            if is_nil {
                zloc = zloc.replace(inner(Tag::Map, vec![]));
            }
            let length = if is_nil { 0 } else { count_uncommented(&zloc.node) };
            if length == 0 {
                let l = zloc.append_child(token_sym(k2)).append_child(vn.clone());
                return Some(l.root().kids.first().cloned()?).map(|n| if n.tag == Tag::Map { n } else { n });
            }
            let first = skip_right_ws_or_comment(&zloc.down()?);
            let align = first.meta();
            let mut key_count = 0usize;
            let mut z = first;
            loop {
                if z.rightmost_p() {
                    let l = z.insert_right_raw(token_sym(k2));
                    let l = indent_or_space(l, key_count, align);
                    let l = l.right()?;
                    let l = l.insert_right(vn.clone());
                    return l.root().kids.first().cloned();
                }
                if z.tag() == Tag::Token && z.node.text == k2 {
                    let v = skip_right_ws_or_comment(&z.right()?);
                    return v.replace(vn.clone()).root().kids.first().cloned();
                }
                key_count += 1;
                z = skip_right_ws_or_comment(&z);
                z = z.right()?;
                z = skip_right_ws_or_comment(&z);
            }
        })();
        assoc.unwrap_or(old)
    })
}

pub fn resolve_macro_as(c: &Ctx, macro_sym: Option<String>) -> Out {
    // JVM asks both questions first, then looks for the macro at the cursor
    let resolved = match ask(c, 0, "Select how LSP should resolve this macro:", &KNOWN) {
        Err(o) => return o,
        Ok(Some(r)) => r,
        Ok(None) => return Out::NoOp,
    };
    let root = c.q.s.project.as_ref().map(|p| p.root.clone());
    let project_cfg = root.as_ref().map(|r| r.join(".clj-kondo")).filter(|d| d.is_dir()).map(|d| d.join("config.edn")).unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join("config.edn"));
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))).unwrap_or_default();
    let home_cfg = xdg.join("clj-kondo").join("config.edn");
    let opts = [project_cfg.to_string_lossy().to_string(), home_cfg.to_string_lossy().to_string()];
    let opt_refs: Vec<&str> = opts.iter().map(|s| s.as_str()).collect();
    let path = match ask(c, 1, "Select where LSP should save this setting:", &opt_refs) {
        Err(o) => return o,
        Ok(Some(p)) => p,
        Ok(None) => return Out::NoOp,
    };
    let Some(full) = macro_sym else { return Out::NoOp };
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
    let tree = Tree::parse(&text);
    let forms_node = if tree.err { from_tree(&Tree::parse("{}")) } else { from_tree(&tree) };
    let Some(new) = assoc_in(forms_node, ":lint-as", &full, &resolved) else { return Out::NoOp };
    Out::Write { path, content: format!("{}\n", new.string()) }
}
