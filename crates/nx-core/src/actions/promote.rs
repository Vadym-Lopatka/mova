//! promote-fn / demote-fn (`refactor/transform.clj`): `#()` <-> `fn` <-> `defn`.
use super::exec::{Ctx, Edit, Out};
use super::refactors::{prepend_preserving_comment, An};
use super::rz::*;
use super::tree::Tag;
use super::zops::*;
use std::collections::BTreeMap;

fn outer_fn_form(z: &Loc) -> bool {
    z.tag() == Tag::List && z.down().map_or(false, |d| d.node.is_sym() && d.node.text == "fn")
}

pub fn convert_fn_to_literal_params(z: &Loc) -> Option<(Loc, Vec<String>)> {
    let fn_z = if outer_fn_form(z) { z.clone() } else { find_ops_up(z, &["fn"])?.up()? };
    let v = fn_z.down()?.find_tag_right(Tag::Vector)?;
    let mut params = Vec::new();
    for k in v.node.kids.iter().filter(|k| !is_printable_only(k.tag)) {
        if !k.is_sym() {
            return None;
        }
        params.push(k.text.clone());
    }
    Some((fn_z, params))
}

/// `replace-sexprs`: prewalk replacing symbol nodes by name.
fn replace_sexprs(z: &Loc, repl: &BTreeMap<String, String>) -> Loc {
    z.subedit(|sub| {
        let mut loc = sub;
        loop {
            if loc.is_end() {
                return loc;
            }
            let p = !is_printable_only(loc.tag());
            if p && loc.node.is_sym() {
                if let Some(r) = repl.get(&loc.node.text) {
                    loc = loc.replace(token_sym(r));
                }
            }
            let n = loc.next();
            if n.is_end() {
                return n;
            }
            loc = n;
        }
    })
}

pub fn demote_fn(z: &Loc) -> Option<Vec<super::transform::ZE>> {
    let (fn_z, params) = convert_fn_to_literal_params(z)?;
    let amp = params.iter().position(|p| p == "&");
    let positioned: Vec<&String> = match amp {
        Some(i) => params[..i].iter().collect(),
        None => params.iter().collect(),
    };
    let vararg = amp.and_then(|i| params.get(i + 1));
    let mut repl: BTreeMap<String, String> = BTreeMap::new();
    if positioned.len() == 1 {
        repl.insert(positioned[0].clone(), "%".into());
    } else {
        for (i, p) in positioned.iter().enumerate() {
            repl.insert((*p).clone(), format!("%{}", i + 1));
        }
    }
    if let Some(v) = vararg {
        repl.insert(v.clone(), "%&".into());
    }
    let replaced = replace_sexprs(&fn_z, &repl);
    let vec_loc = replaced.down()?.find_tag_right(Tag::Vector)?;
    let mut interior: Vec<NR> = Vec::new();
    let mut c = vec_loc.right_raw();
    let mut dropping = true;
    while let Some(x) = c {
        if dropping && is_ws(x.tag()) {
            c = x.right_raw();
            continue;
        }
        dropping = false;
        interior.push(x.node.clone());
        c = x.right_raw();
    }
    let n_sexpr = interior.iter().filter(|n| !is_printable_only(n.tag)).count();
    let kids: Vec<NR> = if n_sexpr > 1 {
        let mut v = vec![token_sym("do"), spaces(1)];
        v.extend(interior);
        v
    } else {
        interior.iter().flat_map(|n| if is_inner(n.tag) { n.kids.clone() } else { vec![n.clone()] }).collect()
    };
    let lit = inner(Tag::Fn, kids);
    let loc = fn_z.replace(lit);
    Some(vec![super::transform::ZE { range: fn_z.meta(), text: loc.string() }])
}

struct Param {
    unnamed: Vec<String>,
    named: String,
}

fn convert_literal_to_fn(z: &Loc, provided_name: Option<&str>) -> Option<Vec<super::transform::ZE>> {
    // literal params: depth-first walk over the subtree
    let mut literal: Vec<(String, Param)> = Vec::new(); // key: "0", "n", "&"
    let sub = z.subzip();
    let mut loc = sub.down();
    while let Some(l) = loc {
        if l.is_end() {
            break;
        }
        if l.tag() == Tag::Token {
            let t = l.string();
            if let Some(rest) = t.strip_prefix('%') {
                let ok = rest.is_empty() || rest == "&" || (rest.chars().all(|c| c.is_ascii_digit()) && !rest.starts_with('0'));
                if ok {
                    let (key, p) = if rest == "&" {
                        ("&".to_string(), Param { unnamed: vec![t.clone()], named: "args".into() })
                    } else if rest.is_empty() {
                        ("0".to_string(), Param { unnamed: vec![t.clone()], named: "element".into() })
                    } else {
                        (rest.to_string(), Param { unnamed: vec![t.clone()], named: format!("element{}", rest) })
                    };
                    match literal.iter_mut().find(|(k, _)| *k == key) {
                        Some(e) => e.1 = p,
                        None => literal.push((key, p)),
                    }
                }
            }
        }
        let n = l.next();
        loc = if n.is_end() { None } else { Some(n) };
    }
    let get = |k: &str| literal.iter().find(|(kk, _)| kk == k).map(|(_, p)| p);
    let param0 = get("0");
    let param1 = get("1");
    let vararg = get("&");
    let mut positioned: BTreeMap<u64, (Vec<String>, String)> = BTreeMap::new();
    for (k, p) in &literal {
        if k != "0" && k != "&" {
            positioned.insert(k.parse().ok()?, (p.unnamed.clone(), p.named.clone()));
        }
    }
    if let Some(p0) = param0 {
        let mut unnamed = p0.unnamed.clone();
        if let Some(p1) = param1 {
            for u in &p1.unnamed {
                if !unnamed.contains(u) {
                    unnamed.push(u.clone());
                }
            }
        }
        positioned.insert(1, (unnamed, p0.named.clone()));
    }
    let mut fn_params: Vec<String> = Vec::new();
    if let Some(&max) = positioned.keys().max() {
        for pos in 1..=max {
            fn_params.push(positioned.get(&pos).map_or("_".to_string(), |(_, n)| n.clone()));
        }
    }
    if let Some(v) = vararg {
        fn_params.push("&".into());
        fn_params.push(v.named.clone());
    }
    let mut repl: BTreeMap<String, String> = BTreeMap::new();
    if let Some(v) = vararg {
        for u in &v.unnamed {
            repl.insert(u.clone(), v.named.clone());
        }
    }
    for (_, (unnamed, named)) in &positioned {
        for u in unnamed {
            repl.insert(u.clone(), named.clone());
        }
    }
    let replaced = replace_sexprs(z, &repl);
    let interior: Vec<NR> = replaced.node.kids.clone();
    let mut kids: Vec<NR> = vec![token_sym("fn"), spaces(1)];
    if let Some(n) = provided_name {
        kids.push(token_sym(n));
        kids.push(spaces(1));
    }
    kids.push(leaf(Tag::Token, super::tree::Tk::Sym, &format!("[{}]", fn_params.join(" "))));
    let first_form = interior.iter().find(|n| !is_printable_only(n.tag));
    match first_form {
        None => kids.extend(interior.iter().cloned()),
        Some(f) if f.is_sym() && f.text == "do" => {
            let i = interior.iter().position(|n| !is_printable_only(n.tag) && n.is_sym() && n.text == "do").unwrap();
            kids.extend(interior[..i].iter().cloned());
            kids.extend(interior[i + 1..].iter().cloned());
        }
        Some(_) => {
            let i = interior.iter().position(|n| !is_printable_only(n.tag)).unwrap();
            kids.push(spaces(1));
            kids.extend(interior[..i].iter().cloned());
            kids.push(list(interior[i..].to_vec()));
        }
    }
    let fn_node = list(kids);
    let loc = z.replace(fn_node);
    Some(vec![super::transform::ZE { range: z.meta(), text: loc.string() }])
}

fn convert_fn_to_defn(c: &Ctx, z: &Loc, provided_name: Option<&str>) -> Option<Vec<super::transform::ZE>> {
    let fn_meta = z.meta()?;
    let use_meta = c.settings.use_metadata_for_privacy;
    let isolated = Loc::of_node(forms(vec![z.node.clone()])).down()?;
    let on_defn = isolated.down()?.replace(token_sym(if use_meta { "defn" } else { "defn-" }));
    // fn name: first symbol node before a list/vector among the siblings
    let mut fn_name_loc: Option<Loc> = None;
    let mut cur = on_defn.right();
    while let Some(x) = cur {
        if matches!(x.tag(), Tag::List | Tag::Vector) {
            break;
        }
        if x.node.is_sym() {
            fn_name_loc = Some(x);
            break;
        }
        cur = x.right();
    }
    let defn_name = provided_name.map(|s| s.to_string()).or_else(|| fn_name_loc.as_ref().map(|l| l.node.text.clone())).unwrap_or_else(|| "new-function".into());
    let name_node = if use_meta { inner(Tag::Meta, vec![keyword(":private"), spaces(1), token_sym(&defn_name)]) } else { token_sym(&defn_name) };
    let on_name = match &fn_name_loc {
        Some(f) => f.replace(name_node.clone()),
        None => on_defn.insert_right(name_node.clone()).right()?,
    };
    let f = c.q.s.id(&c.uri)?;
    let an = An { q: c.q, f };
    let mut used: Vec<String> = Vec::new();
    for u in an.local_usages_outside(fn_meta) {
        used.push(c.q.name(u).as_str().to_string());
    }
    let add_locals = |l: Loc| -> Option<Loc> {
        let mut params = l.find_tag_right(Tag::Vector)?;
        for u in used.iter().rev() {
            params = params.insert_child(token_sym(u));
        }
        Some(params)
    };
    let single_arity = on_name.right().and_then(|r| r.find_tag_right(Tag::Vector)).is_some();
    let defn_zloc = if single_arity {
        add_locals(on_name.clone())?.up()?
    } else {
        let mut l = on_name.clone();
        loop {
            match l.right().and_then(|r| r.find_tag_right(Tag::List)) {
                Some(next) => l = add_locals(next.down()?)?.up()?,
                None => break,
            }
        }
        l.up()?
    };
    let space = spaces(1);
    let replacement: NR = if used.is_empty() {
        token_sym(&defn_name)
    } else if !single_arity || z.find_up(&|l| l.tag() == Tag::Fn).is_some() {
        let mut kids = vec![token_sym("partial"), space.clone(), token_sym(&defn_name), space.clone()];
        for (i, u) in used.iter().enumerate() {
            if i > 0 {
                kids.push(space.clone());
            }
            kids.push(token_sym(u));
        }
        list(kids)
    } else {
        let orig_params = z.down()?.find_tag_right(Tag::Vector)?;
        let exprs: Vec<&NR> = orig_params.node.kids.iter().filter(|k| !is_printable_only(k.tag)).collect();
        let amp = exprs.iter().position(|n| n.is_sym() && n.text == "&");
        let before = amp.unwrap_or(exprs.len());
        let mut lit: Vec<String> = (0..before).map(|i| format!("%{}", i + 1)).collect();
        if amp.is_some() {
            lit.push("%&".into());
        }
        let mut kids = vec![token_sym(&defn_name), space.clone()];
        let all: Vec<String> = used.iter().cloned().chain(lit).collect();
        for (i, a) in all.iter().enumerate() {
            if i > 0 {
                kids.push(space.clone());
            }
            kids.push(token_sym(a));
        }
        inner(Tag::Fn, kids)
    };
    let e1 = prepend_preserving_comment(&to_top(z)?, &defn_zloc)?;
    Some(vec![e1, super::transform::ZE { range: Some(fn_meta), text: replacement.string() }])
}

pub fn promote_fn(c: &Ctx, z: &Loc, name: Option<&str>) -> Out {
    let Some(f) = z.find_up(&|l| l.tag() == Tag::Fn || outer_fn_form(l)) else { return Out::Nil };
    let r = if f.tag() == Tag::Fn { convert_literal_to_fn(&f, name) } else { convert_fn_to_defn(c, &f, name) };
    match r {
        Some(v) => Out::Seq(v.into_iter().map(|z| Edit { range: z.range, text: z.text }).collect()),
        None => Out::Nil,
    }
}
