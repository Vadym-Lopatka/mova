//! hover (feature/hover.clj): element choice, definition, `calling:` header, plaintext/markdown layouts.
use super::text::Doc;
use super::*;
use crate::engine::ctx::ClientOpts;

mod sf {
    pub use crate::query::special_forms::SPECIAL_FORMS;
}

/// Fields of the element hover-documentation reads.
pub struct HDef<'a> {
    pub ns: Option<SymId>,
    pub name: SymId,
    pub doc: Option<&'a str>,
    pub arglists: Vec<&'static str>,
    pub bucket: B,
    pub to: SymId,
    pub uri: &'a str,
    pub method: SymId,
    /// Java member return type / `(params)` (JDK definitions).
    pub ret: Option<&'a str>,
    pub params: Option<&'a str>,
    /// Mova native (Rust): (`file:line` relative to its checkout, registration source lines).
    pub native: Option<(String, String)>,
}

impl<'a> Q<'a> {
    pub fn hdef(&self, e: El) -> HDef<'a> {
        let fa = self.fa(e.f);
        let i = e.i as usize;
        let uri = self.uri(e.f);
        let mut h = HDef { ns: None, name: self.name(e), doc: None, arglists: Vec::new(), bucket: e.b, to: SymId::NONE, uri, method: SymId::NONE, ret: None, params: None, native: None };
        match e.b {
            B::VarDef => {
                let d = &fa.var_definitions[i];
                h.ns = Some(d.ns);
                if self.s.mova.as_ref().is_some_and(|m| m.rank.get(&e.f) == Some(&crate::mova::RANK_NATIVE)) {
                    h.native = self.native_info(uri, d.name_pos.row, d.name.as_str());
                }
                if !d.doc.is_none() {
                    h.doc = Some(d.doc.as_str());
                }
                if d.has_arglists {
                    for k in 0..d.arglists.1 {
                        h.arglists.push(fa.strs[(d.arglists.0 + k) as usize].as_str());
                    }
                }
            }
            B::VarUsage => h.to = fa.var_usages[i].to,
            B::KwDef | B::KwUsage => {
                let k = &fa.keywords[i];
                if !k.ns.is_none() {
                    h.ns = Some(k.ns);
                }
            }
            B::InstInv => h.method = fa.instance_invocations[i].method_name,
            B::NsDef => {
                let d = &fa.namespace_definitions[i];
                if !d.doc.is_none() {
                    h.doc = Some(d.doc.as_str());
                }
            }
            _ => {}
        }
        h
    }
}

impl<'a> Q<'a> {
    /// Where a Mova native is registered and the source of its registration (up to the line that names it).
    fn native_info(&self, uri: &str, row: u32, name: &str) -> Option<(String, String)> {
        let path = crate::engine::scan::uri_to_path(uri)?;
        let m = self.s.mova.as_ref()?;
        let proj = self.s.project.as_ref().map(|p| p.root.clone());
        let rel = path.strip_prefix(&m.root).ok().or_else(|| proj.as_ref().and_then(|r| path.strip_prefix(std::fs::canonicalize(r).ok()?).ok())).unwrap_or(&path);
        let loc = format!("{}:{}", rel.display(), row);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let at = (row as usize).saturating_sub(1);
        let lines: Vec<&str> = text.lines().skip(at).take(4).collect();
        let q = format!("\"{name}\"");
        let n = lines.iter().position(|l| l.contains(&q)).map_or(1, |i| i + 1).min(lines.len());
        let indent = lines[..n].iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
        let mut src: Vec<&str> = lines[..n].iter().map(|l| l.get(indent..).unwrap_or(l.trim_start())).collect();
        // the comment block right above the registration documents the native
        let all: Vec<&str> = text.lines().take(at).collect();
        let doc: Vec<&str> = all.iter().rev().take_while(|l| l.trim_start().starts_with("//")).take(12).map(|l| l.trim_start()).collect();
        src.splice(0..0, doc.into_iter().rev());
        Some((loc, src.join("\n")))
    }
}

/// `shared/uri->filename`.
pub fn uri_filename(uri: &str) -> String {
    let dec = |s: &str| {
        let b = s.as_bytes();
        let mut o = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() + 0 {
                if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    o.push(v);
                    i += 3;
                    continue;
                }
            }
            o.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&o).into_owned()
    };
    if let Some(r) = uri.strip_prefix("jar:file://") {
        if let Some((j, e)) = r.split_once("!/") {
            return format!("{}:{}", dec(j), e);
        }
    }
    if let Some(r) = uri.strip_prefix("zipfile://") {
        if let Some((j, e)) = r.split_once("::") {
            return format!("{}:{}", dec(j), e);
        }
    }
    dec(uri.strip_prefix("file://").unwrap_or(uri))
}

fn docstring_formatted(doc: &str) -> String {
    // `string/split-lines`: split on `\r?\n`, trailing empty strings dropped
    let mut lines: Vec<&str> = doc.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return doc.to_string();
    }
    let others: Vec<&&str> = lines.iter().skip(1).filter(|l| !l.trim().is_empty()).collect();
    if others.is_empty() {
        return doc.to_string();
    }
    let ws = |s: &str| s.chars().count() - s.trim_start().chars().count();
    let indent = others.iter().map(|l| ws(l)).min().unwrap_or(0);
    let mut out = vec![lines[0].to_string()];
    for l in &lines[1..] {
        let n = l.chars().count();
        if indent > n {
            out.push(l.to_string());
        } else {
            let full = l.trim_start();
            let dropped: String = l.chars().skip(indent).collect();
            // `(last (sort-by count [fully-trimmed dropped]))`: the longer one, ties -> dropped
            out.push(if full.chars().count() > dropped.chars().count() { full.to_string() } else { dropped });
        }
    }
    out.join("\n")
}

fn calling_line(h: &HDef, markdown: bool) -> String {
    let caller = match h.bucket {
        // `to` is a keyword (`:clj-kondo/unknown-namespace`) for unresolved namespaces; `str` keeps its colon
        B::VarUsage if h.to.as_str().contains('/') => format!(":{}/{}", h.to.as_str(), h.name.as_str()),
        B::VarUsage => format!("{}/{}", h.to.as_str(), h.name.as_str()),
        B::LocalUsage => h.name.as_str().to_string(),
        B::KwUsage => format!(":{}", h.name.as_str()),
        B::InstInv => format!(".{}", h.method.as_str()),
        _ => format!("{}/{}", h.ns.map_or("", |n| n.as_str()), if h.name.is_none() { "" } else { h.name.as_str() }),
    };
    let args: String = h.arglists.iter().map(|a| format!(" {a}")).collect();
    let call = format!("({caller}{args})");
    if markdown {
        format!("```clojure\n#_calling: {call}\n```\n\n----\n\n")
    } else {
        format!("calling: {call}")
    }
}

/// `hover-documentation`: a JSON value (plaintext list, or MarkupContent).
pub fn hover_documentation(h: &HDef, o: &ClientOpts, calling: Option<&HDef>) -> String {
    let markdown = o.hover_markdown;
    let join = if o.arity_on_same_line { " " } else { "\n " };
    let special = if h.bucket == B::VarUsage && h.to.as_str() == "clojure.core" && h.ns.is_none() && file_langs(h.uri) & CLJ != 0 {
        sf::SPECIAL_FORMS.iter().find(|s| s.0 == h.name.as_str())
    } else {
        None
    };
    let special_sigs: Option<String> = special.map(|s| s.1.join(join)).filter(|s| !s.is_empty());
    let signatures: Option<String> = if !h.arglists.is_empty() {
        Some(h.arglists.join(join))
    } else if let Some(p) = h.params {
        Some(p.to_string())
    } else {
        special_sigs.clone()
    };
    let mut sym = String::new();
    if let Some(r) = h.ret {
        sym.push_str(r);
        sym.push(' ');
    }
    if let Some(ns) = h.ns {
        sym.push_str(ns.as_str());
        sym.push('/');
    }
    if !h.name.is_none() {
        sym.push_str(h.name.as_str());
    }
    let sym_line = if let Some(ss) = &special_sigs {
        ss.clone()
    } else if let Some(sg) = &signatures {
        format!("({sym}{join}{sg})")
    } else {
        sym.clone()
    };
    let doc_line: Option<String> = if let Some(s) = special {
        Some(format!(
            "Special Form\n\n{}\n\nPlease see http://clojure.org/{}",
            if markdown { docstring_formatted(s.2) } else { s.2.to_string() },
            if s.3.is_empty() { format!("special_forms#{}", h.name.as_str()) } else { s.3.to_string() }
        ))
    } else {
        h.doc.filter(|d| !d.is_empty()).map(|d| if markdown { docstring_formatted(d) } else { d.to_string() })
    };
    let filename = uri_filename(h.uri);
    if markdown {
        let fence = if matches!(h.bucket, B::JavaMemberDef | B::JavaClassDef) { "java" } else { "clojure" };
        let mut v = format!("```{fence}\n{sym_line}\n```");
        if let Some(c) = calling {
            v = format!("{}{}", calling_line(c, true), v);
        }
        let cd = h.ns.and_then(|ns| {
            let ix = crate::clojuredocs::lookup()?;
            let n = h.name.as_str();
            let f = |ns: &str| crate::clojuredocs::find(&ix, ns, n).map(|e| crate::clojuredocs::hover_docs(e, doc_line.as_deref()));
            f(ns.as_str()).or_else(|| if file_langs(h.uri) & CLJS != 0 { f(&ns.as_str().replace("cljs", "clojure")) } else { None })
        });
        if let Some((loc, src)) = &h.native {
            v.push_str(&format!("\n\nMova native (Rust): {loc}"));
            if !src.is_empty() {
                v.push_str(&format!("\n\n```rust\n{src}\n```"));
            }
        }
        if let Some(c) = cd {
            v.push_str("\n\n");
            v.push_str(&c);
        } else if let Some(d) = &doc_line {
            v.push_str("\n\n");
            v.push_str(d);
        }
        if !o.hide_file_location {
            v.push_str(&format!("\n\n----\n\n*[{}]({})*", filename.replace('\\', "\\\\"), h.uri));
        }
        format!("{{\"kind\":\"markdown\",\"value\":{}}}", json_str(&v))
    } else {
        let mut items: Vec<String> = Vec::new();
        let cl = |v: &str| format!("{{\"language\":\"clojure\",\"value\":{}}}", json_str(v));
        if let Some(c) = calling {
            items.push(json_str(&calling_line(c, false)));
        }
        if special_sigs.is_none() {
            items.push(cl(if o.arity_on_same_line { &sym_line } else { &sym }));
        }
        if let Some(sg) = &signatures {
            if !o.arity_on_same_line || special_sigs.is_some() {
                items.push(cl(sg));
            }
        }
        if let Some((loc, _)) = &h.native {
            items.push(json_str(&format!("Mova native (Rust): {loc}")));
        }
        if let Some(d) = &doc_line {
            items.push(json_str(d));
        }
        if !o.hide_file_location {
            items.push(json_str(&filename));
        }
        format!("[{}]", items.join(","))
    }
}

/// HDef of a JDK hit (`calling:` header and docs); the definition's own `:name` is its member name.
fn jdk_hdef(j: &super::jdk::JdkHit) -> HDef<'_> {
    HDef {
        ns: None,
        name: j.name.as_deref().map_or(SymId::NONE, crate::intern::intern),
        doc: j.doc.as_deref(),
        arglists: Vec::new(),
        bucket: j.bucket(),
        to: SymId::NONE,
        uri: &j.uri,
        method: SymId::NONE,
        ret: j.ret.as_deref(),
        params: j.params.as_deref(),
        native: None,
    }
}

pub fn hover(q: &Q, at: At) -> String {
    let (row, col) = (at.row(), at.col());
    let opts = &q.s.opts;
    let cursor_el = q.first_under_cursor(at.uri, row, col);
    let entry = q.s.get(at.uri);
    let text_arc: Option<std::sync::Arc<str>> = entry.and_then(|e| e.text());
    let text: Option<&str> = text_arc.as_deref();
    let doc = text.map(Doc::new);
    let cursor_node = doc.as_ref().and_then(|d| d.find_at(row, col));
    let func_el = match (&doc, cursor_node) {
        (Some(d), Some(n)) => d.func_name_node(n).and_then(|f| {
            let p = d.cst.pos(f);
            q.first_under_cursor(at.uri, p.row, p.col)
        }),
        _ => None,
    };
    let func_hit = func_el.and_then(|f| super::jdk::hover_hit(q, f));
    let func_def = if func_hit.is_some() { None } else { func_el.and_then(|f| q.find_definition(f)) };
    let inside_ns = match (&doc, cursor_node) {
        (Some(d), Some(n)) => d.inside_require(n),
        _ => false,
    };
    let element = if matches!(cursor_el.map(|e| e.b), Some(B::VarUsage) | Some(B::VarDef)) || inside_ns {
        cursor_el
    } else {
        let mut found = None;
        let mut c = col;
        loop {
            if let Some(e) = q.first_under_cursor(at.uri, row, c) {
                found = Some(e);
                break;
            }
            if c == 0 {
                break;
            }
            c -= 1;
        }
        found
    };
    let el_hit = element.and_then(|e| super::jdk::hover_hit(q, e));
    let definition = if el_hit.is_some() { None } else { element.and_then(|e| q.find_definition(e)) };
    // HDef of a JDK hit (`calling:` header and docs); the definition's own `:name` is its member name
    let call_ok = !opts.hide_signature_call && element.is_some() && func_el.is_some() && element != func_el;
    let calling: Option<HDef> = if !call_ok {
        None
    } else if let Some(fh) = &func_hit {
        Some(jdk_hdef(fh))
    } else {
        Some(q.hdef(func_def.or(func_el).unwrap()))
    };
    let out = |range_el: El, h: &HDef, calling: Option<&HDef>| format!("{{\"range\":{},\"contents\":{}}}", range_json(q.name_pos(range_el)), hover_documentation(h, opts, calling));
    if let (Some(jh), Some(e)) = (&el_hit, element) {
        return out(e, &jdk_hdef(jh), calling.as_ref());
    }
    if let (Some(d), Some(e)) = (definition, element) {
        return out(e, &q.hdef(d), calling.as_ref());
    }
    if let Some(e) = element {
        return out(e, &q.hdef(e), calling.as_ref());
    }
    if let (Some(fh), Some(fe)) = (&func_hit, func_el) {
        return out(fe, &jdk_hdef(fh), None);
    }
    if let (Some(fd), Some(fe)) = (func_def, func_el) {
        return out(fe, &q.hdef(fd), None);
    }
    if let Some(fe) = func_el {
        return out(fe, &q.hdef(fe), None);
    }
    "{\"contents\":[]}".to_string()
}
