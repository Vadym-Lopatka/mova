//! textDocument/completion (feature/completion.clj), as far as the analysis buckets go: refer / require / symbol
//! completion with local items, aliases, full-namespace vars, clojure.core + cljs.core tables.
//! Keyword, java and snippet branches plug in with their buckets.
use super::text::Doc;
use super::*;
use crate::cst::{Kind, NodeId};
use std::collections::HashSet;

const PRIORITY: [&str; 18] = [
    "snippet", "java-member-definitions", "java-class-definitions", "java-usages", "clojurescript-core", "clojure-core", "ns-definition",
    "unrequired-alias", "required-alias", "refer", "keyword", "keyword-same-ns", "alias-keyword", "simple-cursor", "locals", "kw-arg",
    "lib-version", "lib-name",
];

fn score(p: &str) -> usize {
    PRIORITY.iter().position(|x| *x == p).map_or(0, |i| i + 1)
}

#[derive(Default, Clone)]
struct Item {
    label: String,
    kind: u8,
    detail: Option<String>,
    priority: &'static str,
    deprecated: bool,
    /// Unrequired alias item: `(alias, ns)` to add to the ns form (additionalTextEdits / `alias` resolve data).
    alias_add: Option<(String, String)>,
    /// Element behind the item (documentation resolve data).
    doc: DocRef,
    /// Snippet payload (kind 15 items).
    snippet: Option<Snip>,
    /// `data.snippet-kind`: kind of the item this snippet replaced.
    snip_kind: Option<u8>,
}

#[derive(Default, Clone)]
enum DocRef {
    #[default]
    None,
    /// clojure.core / cljs.core table entry (namespace).
    Core(&'static str),
    Pos { name: String, uri: String, row: u32, col: u32 },
}

#[derive(Clone)]
struct Snip {
    text: String,
    /// `:function-call` key (present on some built-ins).
    fc: Option<bool>,
    /// Own `:text-edit` ($current-form additional snippets): (range json, new text).
    edit: Option<(String, String)>,
}

const K_FUNCTION: u8 = 3;
const K_VARIABLE: u8 = 6;
const K_MODULE: u8 = 9;
const K_PROPERTY: u8 = 10;
const K_REFERENCE: u8 = 18;

/// The token under the cursor as clojure-lsp sees it (`z/sexpr`).
enum CursorValue {
    Sym { ns: Option<String>, name: String },
    Kw { text: String },
    /// Any other sexpr-able node (string, number, collection): its printed form.
    Other(String),
    Empty,
}

impl CursorValue {
    fn from_node(doc: &Doc, n: Option<NodeId>) -> CursorValue {
        let Some(n) = n else { return CursorValue::Empty };
        let t = doc.cst.text(n);
        match doc.cst.kind(n) {
            Kind::Symbol => match t.find('/') {
                Some(i) if i > 0 && i + 1 < t.len() => CursorValue::Sym { ns: Some(t[..i].to_string()), name: t[i + 1..].to_string() },
                _ => CursorValue::Sym { ns: None, name: t.to_string() },
            },
            Kind::Keyword => CursorValue::Kw { text: t.to_string() },
            Kind::Vector | Kind::Root => CursorValue::Empty,
            Kind::Uneval => CursorValue::Empty,
            _ => CursorValue::Other(t.to_string()),
        }
    }
    /// `(str cursor-value)` blank?
    fn blank(&self) -> bool {
        match self {
            CursorValue::Empty => true,
            CursorValue::Other(s) => s.trim().is_empty(),
            CursorValue::Kw { text } => text.trim().is_empty(),
            CursorValue::Sym { name, ns } => name.trim().is_empty() && ns.is_none(),
        }
    }
    fn simple_ident(&self) -> bool {
        match self {
            CursorValue::Sym { ns, .. } => ns.is_none(),
            CursorValue::Kw { text } => !text.trim_start_matches(':').contains('/'),
            _ => false,
        }
    }
    /// String `matches-cursor?` compares against: `(name sym)` for symbols else `(str v)`.
    fn match_str(&self) -> String {
        match self {
            CursorValue::Sym { name, .. } => name.clone(),
            CursorValue::Kw { text } => text.clone(),
            CursorValue::Other(s) => s.clone(),
            CursorValue::Empty => String::new(),
        }
    }
    /// `cursor-value-or-ns`.
    fn value_or_ns(&self) -> String {
        match self {
            CursorValue::Sym { ns: Some(ns), .. } => ns.clone(),
            CursorValue::Sym { name, .. } => name.clone(),
            CursorValue::Kw { text } => {
                let t = text.trim_start_matches(':');
                match t.find('/') {
                    Some(i) => t[..i].to_string(),
                    None => t.to_string(),
                }
            }
            CursorValue::Other(s) => s.clone(),
            CursorValue::Empty => String::new(),
        }
    }
    /// `(str cursor-value)`.
    fn to_str(&self) -> String {
        match self {
            CursorValue::Sym { ns: Some(ns), name } => format!("{ns}/{name}"),
            CursorValue::Sym { ns: None, name } => name.clone(),
            CursorValue::Kw { text } => text.clone(),
            CursorValue::Other(s) => s.clone(),
            CursorValue::Empty => String::new(),
        }
    }
}

/// Is (row, col) inside a `;` comment of its line (strings and char literals respected)?
fn in_comment(text: &str, row: u32, col: u32) -> bool {
    let Some(line) = text.lines().nth(row as usize - 1) else { return false };
    let mut in_str = false;
    let mut esc = false;
    let mut prev_backslash = false;
    for (i, c) in line.chars().enumerate() {
        if i + 1 > col as usize {
            break;
        }
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if prev_backslash {
            prev_backslash = false;
            continue;
        }
        match c {
            '\\' => prev_backslash = true,
            '"' => in_str = true,
            ';' => return true,
            _ => {}
        }
    }
    false
}

impl<'a> Q<'a> {
    fn var_def_item(&self, f: FileId, i: usize, label_prefix: Option<&str>, priority: &'static str) -> Item {
        let d = &self.fa(f).var_definitions[i];
        let native = self.s.mova.as_ref().is_some_and(|m| m.rank.get(&f) == Some(&crate::mova::RANK_NATIVE));
        let kind = if d.has_arglists || d.macro_ || native { K_FUNCTION } else { K_VARIABLE };
        let label = match label_prefix {
            Some(p) => format!("{p}/{}", d.name.as_str()),
            None => d.name.as_str().to_string(),
        };
        Item { label, kind, detail: None, priority, deprecated: !d.deprecated.is_none(), doc: DocRef::Pos { name: d.name.as_str().to_string(), uri: self.entry(f).uri.to_string(), row: d.name_pos.row, col: d.name_pos.col }, ..Default::default() }
    }

    /// clojure-lsp's view of a project file: 2 = internal (under the source paths), 1 = opened outside them (analyzed
    /// `:external?`: no keyword usages, no internal dependents), 0 = not in its analysis at all. Jar entries: 2.
    fn jvm_state(&self, f: FileId) -> u8 {
        if f >= crate::engine::jarview::EXT_BASE || self.s.mova.as_ref().is_some_and(|m| m.rank.contains_key(&f)) {
            return 2; // jar entries; the Mova layer (stdlib sources, natives) is the runtime of a Mova project
        }
        let e = self.entry(f);
        let Some(p) = self.s.project.as_ref().filter(|p| !p.source_paths.is_empty()) else { return 2 };
        let under = crate::engine::scan::uri_to_path(&e.uri).map_or(true, |path| p.source_paths.iter().any(|sp| path.starts_with(sp)));
        if under {
            2
        } else if e.version >= 0 {
            1
        } else {
            0
        }
    }

    /// Public var definitions of namespace `ns` (files of the ns).
    fn public_defs(&self, ns: SymId) -> Vec<(FileId, usize)> {
        let mut out = Vec::new();
        let mut files = self.s.ns_files_of(ns);
        files.sort();
        files.dedup();
        for f in files {
            if self.jvm_state(f) == 0 {
                continue;
            }
            for (i, d) in self.fa(f).var_definitions.iter().enumerate() {
                if d.ns == ns && !d.private && !d.name.is_none() {
                    out.push((f, i));
                }
            }
        }
        out
    }
}

fn core_items(out: &mut Vec<Item>, cljs: bool, matches: &dyn Fn(&str) -> bool) {
    static TABLE: std::sync::OnceLock<Vec<(bool, &'static str, u8)>> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        include_str!("core_syms.txt")
            .lines()
            .filter_map(|l| {
                let mut p = l.split('\t');
                let (tag, name, kind) = (p.next()?, p.next()?, p.next()?);
                Some((tag == "cljs", name, match kind { "function" => K_FUNCTION, "variable" => K_VARIABLE, _ => K_REFERENCE }))
            })
            .collect()
    });
    let ns = if cljs { "cljs.core" } else { "clojure.core" };
    for (c, name, kind) in t {
        if *c == cljs && matches(name) {
            out.push(Item { label: name.to_string(), kind: *kind, detail: Some(format!("{ns}/{name}")), priority: if cljs { "clojurescript-core" } else { "clojure-core" }, deprecated: false, doc: DocRef::Core(ns), ..Default::default() });
        }
    }
}

pub fn completion(q: &Q, at: At) -> String {
    let (row, col) = (at.row(), at.col());
    let Some(entry) = q.s.get(at.uri) else { return "[]".into() };
    let text_arc = entry.text();
    let Some(text) = text_arc.as_deref() else { return "[]".into() };
    let doc = Doc::new(text);
    // `(dec col)`: complete what is behind the cursor; the node must start on the cursor row
    // JVM `parser/safe-zloc-of-file` is nil when rewrite-clj cannot parse the document (unclosed / unmatched brackets, EOF
    // in a string): no cursor loc, so the cursor is "not on a symbol" and ALL candidates come back (Emacs typing at EOF)
    let parse_failed = doc.cst.errors().iter().any(|e| !e.msg.starts_with("Invalid"));
    let cursor_node = if parse_failed { None } else { doc.find_at(row, col.saturating_sub(1)).filter(|n| doc.cst.pos(*n).row == row) };
    // a `;` inside a string / regex spanning lines is not a comment (rewrite-clj finds the string node, not a comment node)
    let in_str = !parse_failed && doc.find_at(row, col.saturating_sub(1)).map_or(false, |n| matches!(doc.cst.kind(n), Kind::String | Kind::Regex));
    if !parse_failed && !in_str && in_comment(text, row, col.saturating_sub(1)) {
        return "[]".into();
    }
    let value = CursorValue::from_node(&doc, cursor_node);
    let matches_str = value.match_str();
    let matches = |s: &str| s.starts_with(matches_str.as_str());
    let file_mask = file_langs(at.uri);
    let simple_cursor = value.simple_ident() || value.blank();
    let value_or_ns = value.value_or_ns();
    let cursor_el = q.first_under_cursor(at.uri, row, col);
    let f = q.s.id(at.uri);
    let fa = entry.fa();
    let inside_require = cursor_node.map_or(false, |n| doc.inside_require(n));
    let inside_refer = inside_require && cursor_node.map_or(false, |n| inside_refer(&doc, n));
    let snippets = q.s.opts.completion_snippets;
    let next_node = if parse_failed { None } else { doc.find_at(row, col) };
    let function_call = cursor_node.and_then(|n| doc.find_op(n)).map_or(false, |op| doc.cst.text(op) == value.to_str());
    let mut items: Vec<Item> = Vec::new();
    if inside_refer {
        // vars of the required namespace
        if let Some(ns) = refer_ns(&doc, cursor_node.unwrap()) {
            let ns = crate::intern::intern(&ns);
            for (f, i) in q.public_defs_all(ns) {
                let d = &q.fa(f).var_definitions[i];
                if matches(d.name.as_str()) {
                    items.push(q.var_def_item(f, i, None, "refer"));
                }
            }
        }
    } else if inside_require {
        let mut seen: HashSet<u32> = HashSet::new();
        let mut names: Vec<(SymId, DocRef)> = Vec::new();
        for uri in q.s.uris() {
            if &**uri == at.uri {
                continue;
            }
            if let Some(id) = q.s.id(uri) {
                let e = q.entry(id);
                if e.internal && q.jvm_state(id) != 0 {
                    if let Some(fa) = e.fa() {
                        let u = e.uri.to_string();
                        names.extend(fa.namespace_definitions.iter().map(|n| (n.name, DocRef::Pos { name: n.name.as_str().to_string(), uri: u.clone(), row: n.name_pos.row, col: n.name_pos.col })));
                    }
                }
            }
        }
        if let Some(j) = &q.s.jars {
            names.extend(j.all_ns().iter().map(|n| (*n, DocRef::None)));
        }
        for (n, doc) in names {
            if seen.insert(n.0) && matches(n.as_str()) {
                items.push(Item { label: n.as_str().to_string(), kind: K_MODULE, detail: None, priority: "ns-definition", deprecated: false, doc, ..Default::default() });
            }
        }
        if snippets && simple_cursor {
            merge_snippets(&mut items, q, &doc, cursor_node, next_node, function_call, &matches);
        }
    } else if let CursorValue::Kw { text } = &value {
        let aliased = text.starts_with("::") && text.trim_start_matches(':').contains('/');
        let kwargs = if aliased { None } else { cursor_node.and_then(|n| q.kw_arg_items(&doc, n, at, &matches)) };
        match kwargs {
            Some(v) => items.extend(v),
            None => q.keyword_items(text, at, &matches, cursor_el, &mut items),
        }
    } else {
        // full namespace: public vars labelled `ns/name`
        let full_ns = q.is_ns_name(&value_or_ns);
        if full_ns {
            let ns = crate::intern::intern(&value_or_ns);
            for (f, i) in q.public_defs(ns) {
                items.push(q.var_def_item(f, i, Some(&value_or_ns), "ns-definition"));
            }
        }
        // aliases
        if let (Some(f), Some(fa)) = (f, fa) {
            let local: Vec<(SymId, SymId)> = fa.namespace_usages.iter().filter(|u| !u.alias.is_none() && u.alias.as_str() == value_or_ns).map(|u| (u.alias, u.to)).collect();
            // Mova default alias (`async/`, `flow/`), unless the file gives the name another meaning
            let auto_ns: Option<SymId> = if local.is_empty() { q.s.mova.as_ref().and_then(|m| m.auto_ns.iter().find(|(a, ns)| a != ns && a.as_str() == value_or_ns).map(|x| x.1)) } else { None };
            let aliases: Vec<(SymId, SymId)> = if !local.is_empty() { local } else { q.project_aliases() };
            if value.simple_ident() {
                let mut seen: HashSet<(u32, u32)> = HashSet::new();
                for (alias, to) in &aliases {
                    if (matches(alias.as_str()) || matches(to.as_str())) && seen.insert((alias.0, to.0)) {
                        let m = matches(alias.as_str());
                        items.push(Item {
                            label: if m { alias.as_str().to_string() } else { to.as_str().to_string() },
                            kind: K_PROPERTY,
                            detail: Some(if m { format!("alias to: {}", to.as_str()) } else { format!(":as {}", alias.as_str()) }),
                            priority: "required-alias",
                            deprecated: false,
                            alias_add: Some((alias.as_str().to_string(), to.as_str().to_string())),
                            ..Default::default()
                        });
                    }
                }
            }
            let alias_nses: HashSet<u32> = match auto_ns {
                Some(ns) => [ns.0].into_iter().collect(),
                None => aliases.iter().filter(|(a, _)| a.as_str() == value_or_ns).map(|(_, t)| t.0).collect(),
            };
            let mut seen: HashSet<(u32, String, u8)> = HashSet::new();
            for ns in alias_nses {
                for (df, i) in q.public_defs(SymId(ns)) {
                    let d = &q.fa(df).var_definitions[i];
                    if value.simple_ident() || matches(d.name.as_str()) {
                        let mut it = q.var_def_item(df, i, Some(&value_or_ns), "unrequired-alias");
                        if auto_ns.is_none() {
                            it.alias_add = Some((value_or_ns.clone(), SymId(ns).as_str().to_string()));
                        }
                        if seen.insert((ns, it.label.clone(), it.kind)) {
                            items.push(it);
                        }
                    }
                }
            }
            let _ = f;
        }
        if simple_cursor {
            if let (Some(f), Some(fa)) = (f, fa) {
                q.local_items(f, fa, cursor_el, row, col, file_mask, &matches, &mut items);
            }
            core_items(&mut items, false, &matches);
            q.mova_core_items(&mut items, &matches);
            if file_mask & CLJS != 0 {
                core_items(&mut items, true, &matches);
            }
        }
        if file_mask & CLJ != 0 && !at.uri.ends_with(".cljc") {
            jdk_items(q, &value, simple_cursor, &matches_str, &mut items);
        }
        if snippets && simple_cursor {
            merge_snippets(&mut items, q, &doc, cursor_node, next_node, function_call, &matches);
        }
    }
    // sort: distinct by [label kind detail]; score desc, label asc, detail asc
    let mut seen: HashSet<(String, u8, Option<String>)> = HashSet::new();
    let mut scored: Vec<(usize, Item)> = Vec::new();
    for it in items {
        if seen.insert((it.label.clone(), it.kind, it.detail.clone())) {
            scored.push((score(it.priority), it));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)).then_with(|| a.1.detail.cmp(&b.1.detail)));
    scored.truncate(600);
    let range = cursor_node.map(|n| range_json(doc.cst.pos(n)));
    let o = q.s.opts.clone();
    let mut edits: std::collections::HashMap<(String, String), Option<String>> = std::collections::HashMap::new();
    let mut out = String::from("[");
    for (i, (sc, it)) in scored.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"label\":{},\"kind\":{}", json_str(&it.label), it.kind);
        if let Some(d) = &it.detail {
            let _ = write!(out, ",\"detail\":{}", json_str(d));
        }
        if it.deprecated {
            out.push_str(",\"tags\":[1]");
        }
        let _ = write!(out, ",\"score\":{sc}");
        let mut unresolved: Vec<String> = Vec::new();
        if o.resolve_documentation {
            match &it.doc {
                DocRef::None => {}
                DocRef::Core(ns) => {
                    let uri = if *ns == "cljs.core" { "file:///cljs.core.cljs" } else { "file:///clojure.core.clj" };
                    unresolved.push(format!("[\"documentation\",{{\"name\":{},\"ns\":{},\"uri\":{}}}]", json_str(&it.label), json_str(ns), json_str(uri)));
                }
                DocRef::Pos { name, uri, row, col } => {
                    unresolved.push(format!("[\"documentation\",{{\"name\":{},\"uri\":{},\"name-row\":{row},\"name-col\":{col}}}]", json_str(name), json_str(uri)));
                }
            }
        }
        if let Some((alias, ns)) = &it.alias_add {
            if o.resolve_alias_edit {
                unresolved.push(format!("[\"alias\",{{\"ns-to-add\":{},\"alias-to-add\":{},\"rcf-pos\":null,\"uri\":{}}}]", json_str(ns), json_str(alias), json_str(at.uri)));
            } else {
                let e = edits.entry((alias.clone(), ns.clone())).or_insert_with(|| super::alias_edit::add_alias_edit(&doc, ns, alias).map(|e| e.json()));
                if let Some(e) = e {
                    let _ = write!(out, ",\"additionalTextEdits\":[{e}]");
                }
            }
        }
        let snip_kind = it.snip_kind.map(|k| format!("\"snippet-kind\":{k}"));
        if !unresolved.is_empty() || snip_kind.is_some() {
            let mut parts: Vec<String> = Vec::new();
            parts.extend(snip_kind);
            if !unresolved.is_empty() {
                parts.push(format!("\"unresolved\":[{}]", unresolved.join(",")));
            }
            let _ = write!(out, ",\"data\":{{{}}}", parts.join(","));
        }
        if let Some(sn) = &it.snippet {
            if let Some(fc) = sn.fc {
                let _ = write!(out, ",\"functionCall\":{fc}");
            }
            if sn.edit.is_none() {
                let _ = write!(out, ",\"insertText\":{}", json_str(&sn.text));
            }
            out.push_str(",\"insertTextFormat\":2");
        }
        match (&it.snippet, &range) {
            (Some(Snip { edit: Some((r, t)), .. }), _) => {
                let _ = write!(out, ",\"textEdit\":{{\"newText\":{},\"range\":{}}}", json_str(t), r);
            }
            (Some(sn), Some(r)) => {
                let _ = write!(out, ",\"textEdit\":{{\"newText\":{},\"range\":{}}}", json_str(&sn.text), r);
            }
            (None, Some(r)) => {
                let _ = write!(out, ",\"textEdit\":{{\"newText\":{},\"range\":{}}}", json_str(&it.label), r);
            }
            _ => {}
        }
        out.push('}');
    }
    out.push(']');
    out
}

enum Form {
    Sym(String),
    Kw(String),
    Seq(Vec<Form>),
    Other,
}

/// Minimal EDN reader for arglist strings (`edn/read-string`); None = unreadable.
fn read_form(b: &[u8], i: &mut usize, depth: u32) -> Option<Form> {
    if depth > 64 {
        return None;
    }
    loop {
        while *i < b.len() && (b[*i].is_ascii_whitespace() || b[*i] == b',') {
            *i += 1;
        }
        if *i < b.len() && b[*i] == b';' {
            while *i < b.len() && b[*i] != b'\n' {
                *i += 1;
            }
            continue;
        }
        break;
    }
    let c = *b.get(*i)?;
    let close = |o: u8| match o {
        b'[' => b']',
        b'{' => b'}',
        _ => b')',
    };
    match c {
        b'[' | b'{' | b'(' => {
            *i += 1;
            let mut v = Vec::new();
            loop {
                while *i < b.len() && (b[*i].is_ascii_whitespace() || b[*i] == b',') {
                    *i += 1;
                }
                if *b.get(*i)? == close(c) {
                    *i += 1;
                    return Some(Form::Seq(v));
                }
                v.push(read_form(b, i, depth + 1)?);
            }
        }
        b'#' if b.get(*i + 1) == Some(&b'{') => {
            *i += 1;
            read_form(b, i, depth + 1)
        }
        b'#' | b'@' | b'`' | b'~' | b']' | b'}' | b')' => None,
        b'^' | b'\'' => {
            *i += 1;
            if c == b'^' {
                read_form(b, i, depth + 1)?;
            }
            read_form(b, i, depth + 1)
        }
        b'"' => {
            *i += 1;
            while *i < b.len() && b[*i] != b'"' {
                *i += if b[*i] == b'\\' { 2 } else { 1 };
            }
            *i += 1;
            Some(Form::Other)
        }
        b'\\' => {
            *i += 2;
            Some(Form::Other)
        }
        _ => {
            let st = *i;
            while *i < b.len() && !(b[*i].is_ascii_whitespace() || b",[]{}()\";".contains(&b[*i])) {
                *i += 1;
            }
            let t = std::str::from_utf8(&b[st..*i]).ok()?;
            Some(if t.starts_with(':') { Form::Kw(t.to_string()) } else if t.as_bytes()[0].is_ascii_digit() { Form::Other } else { Form::Sym(t.to_string()) })
        }
    }
}

/// `(:keys (first (edn/read-string arglist-str)))` as strings.
fn first_param_keys(arglist: &str) -> Option<Vec<String>> {
    let b = arglist.as_bytes();
    let mut i = 0;
    let Form::Seq(params) = read_form(b, &mut i, 0)? else { return None };
    let Some(Form::Seq(m)) = params.into_iter().next() else { return None };
    let mut it = m.into_iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        if let (Form::Kw(k), Form::Seq(ks)) = (k, v) {
            if k == ":keys" {
                return Some(ks.into_iter().filter_map(|f| match f {
                    Form::Sym(s) => Some(s),
                    Form::Kw(s) => Some(s),
                    _ => None,
                }).collect());
            }
        }
    }
    None
}

/// Snippets of completion.clj `merging-snippets`: built-ins + `:additional-snippets` matching the cursor replace same-label items.
#[allow(clippy::too_many_arguments)]
fn merge_snippets(items: &mut Vec<Item>, q: &Q, doc: &Doc, cursor: Option<NodeId>, next: Option<NodeId>, function_call: bool, matches: &dyn Fn(&str) -> bool) {
    let o = &q.s.opts;
    let mut snips: Vec<Item> = Vec::new();
    for (label, detail, text, fc) in super::snippets::SNIPPETS {
        if !matches(label) {
            continue;
        }
        let mut t = text.to_string();
        if *label == "defn-" && o.use_metadata_privacy {
            t = t.replacen("(defn- ", "(defn ^:private ", 1);
        }
        if *fc && function_call {
            t = t.chars().take(t.chars().count() - 1).skip(1).collect();
        }
        snips.push(Item { label: label.to_string(), kind: 15, detail: Some(detail.to_string()), priority: "snippet", snippet: Some(Snip { text: t, fc: fc.then_some(function_call), edit: None }), ..Default::default() });
    }
    // build-additional-snippets
    if let Some(range_node) = next.or(cursor) {
        let has_meta = doc.cst.has_pos(range_node);
        for (name, detail, snippet) in &o.additional_snippets {
            if !matches(name) {
                continue;
            }
            let cur = snippet.contains("$current-form");
            if cur && !(cursor.is_some() && next.is_some() && has_meta) {
                continue;
            }
            let sn = if cur {
                let nx = next.unwrap();
                let mut p = doc.cst.pos(nx);
                let tok = cursor.map_or(false, |c| !crate::cst::Cst::is_container(doc.cst.kind(c)) && doc.cst.kind(c) != Kind::Uneval);
                if tok {
                    p.col = p.col.saturating_sub(doc.cst.text(cursor.unwrap()).chars().count() as u32);
                }
                let text = snippet.replace("$current-form", doc.cst.text(nx));
                Snip { text: text.clone(), fc: None, edit: Some((range_json(p), text)) }
            } else {
                Snip { text: snippet.clone(), fc: None, edit: None }
            };
            snips.push(Item { label: name.clone(), kind: 15, detail: detail.clone(), priority: "snippet", snippet: Some(sn), ..Default::default() });
        }
    }
    let mut by_label: std::collections::HashMap<String, Item> = std::collections::HashMap::new();
    for s in snips {
        by_label.insert(s.label.clone(), s);
    }
    let taken: HashSet<String> = items.iter().map(|i| i.label.clone()).collect();
    for it in items.iter_mut() {
        if let Some(sn) = by_label.get(&it.label) {
            let mut n = sn.clone();
            n.snip_kind = Some(it.kind);
            n.doc = std::mem::take(&mut it.doc);
            n.alias_add = it.alias_add.take();
            *it = n;
        }
    }
    let mut rest: Vec<Item> = by_label.into_values().filter(|s| !taken.contains(&s.label)).collect();
    rest.sort_by(|a, b| a.label.cmp(&b.label));
    items.extend(rest);
}

/// `java-class-for-static-member`: `^([A-Z]\w*)/|\.([A-Z]\w*)/` first match in the cursor symbol.
fn static_member_class(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let word = |from: usize| -> usize {
        let mut e = from;
        while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'_') {
            e += 1;
        }
        e
    };
    // alternative 1 at position 0, alternative 2 at each `.`; leftmost wins
    if !b.is_empty() && b[0].is_ascii_uppercase() {
        let e = word(0);
        if b.get(e) == Some(&b'/') {
            return Some(&s[..e]);
        }
    }
    for i in 0..b.len() {
        if b[i] == b'.' && b.get(i + 1).map_or(false, |c| c.is_ascii_uppercase()) {
            let e = word(i + 1);
            if b.get(e) == Some(&b'/') {
                return Some(&s[i + 1..e]);
            }
        }
    }
    None
}

/// JDK class definitions (simple cursor) and static members (`Class/`) of completion.
fn jdk_items(q: &Q, value: &CursorValue, simple_cursor: bool, matches_str: &str, out: &mut Vec<Item>) {
    let full = match value {
        CursorValue::Sym { ns: Some(ns), name } => format!("{ns}/{name}"),
        CursorValue::Sym { ns: None, name } => name.clone(),
        _ => return,
    };
    let class_items = simple_cursor && !full.is_empty();
    let member_class = static_member_class(&full);
    if !class_items && member_class.is_none() {
        return;
    }
    if class_items {
        if let Some(jars) = &q.s.jars {
            for c in jars.classes_with_prefix(matches_str) {
                out.push(Item { label: c.to_string(), kind: 7, priority: "java-class-definitions", ..Default::default() });
            }
        }
    }
    let Some(j) = crate::jdk::wait(std::time::Duration::from_secs(5)) else { return };
    if class_items {
        for c in j.classes_with_prefix(matches_str) {
            out.push(Item { label: j.class_name(c).to_string(), kind: 7, detail: None, priority: "java-class-definitions", deprecated: false, ..Default::default() });
        }
        if !matches_str.is_empty() {
            for c in j.classes_with_prefix("java.lang") {
                let class = j.class_name(c);
                let short = class.rsplit('.').next().unwrap_or("");
                if short.starts_with(matches_str) {
                    out.push(Item { label: short.to_string(), kind: 7, detail: Some(class.to_string()), priority: "java-class-definitions", deprecated: false, ..Default::default() });
                }
            }
        }
    }
    if let Some(cls) = member_class {
        let full_package = full.contains('.');
        for c in j.classes_simple(cls) {
            let class = j.class_name(c);
            for m in j.members(c) {
                let name = j.member_name(m);
                if name == "<init>" {
                    continue;
                }
                let ok = if full_package { format!("{class}/{name}").starts_with(full.as_str()) } else { name.starts_with(matches_str) };
                if !ok {
                    continue;
                }
                let kind = if j.is_final(m) && j.is_field(m) && !j.is_method(m) {
                    21
                } else if j.is_method(m) {
                    2
                } else if j.is_field(m) {
                    5
                } else {
                    2
                };
                let detail = j.member_type(m).unwrap_or("").to_string();
                let label = if full_package { format!("{class}/{name}") } else { format!("{cls}/{name}") };
                out.push(Item { label, kind, detail: Some(detail), priority: "java-member-definitions", deprecated: false, ..Default::default() });
            }
        }
    }
}

/// edit/inside-refer?
fn inside_refer(doc: &Doc, n: NodeId) -> bool {
    let is_refer_kw = |x: Option<NodeId>| x.map_or(false, |x| doc.cst.kind(x) == Kind::Keyword && doc.cst.text(x) == ":refer");
    let left = |x: NodeId| -> Option<NodeId> {
        let p = doc.parent(x)?;
        let sig: Vec<NodeId> = doc.cst.sig_children(p).collect();
        let i = sig.iter().position(|c| *c == x)?;
        if i == 0 { None } else { Some(sig[i - 1]) }
    };
    match doc.cst.kind(n) {
        Kind::Vector => is_refer_kw(left(n)),
        Kind::Symbol | Kind::Keyword => doc.parent(n).map_or(false, |p| is_refer_kw(left(p))),
        _ => false,
    }
}

/// edit/find-refer-ns: first element of the libspec vector.
fn refer_ns(doc: &Doc, n: NodeId) -> Option<String> {
    let vec = if doc.cst.kind(n) == Kind::Vector { doc.parent(n)? } else { doc.parent(doc.parent(n)?)? };
    let first = doc.cst.sig_children(vec).next()?;
    Some(doc.cst.text(first).to_string())
}

impl<'a> Q<'a> {
    /// Does any namespace with this name exist (dep-graph key)?
    fn is_ns_name(&self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        let ns = crate::intern::intern(name);
        self.s.ns_deps.get(&ns.0).is_some() || self.s.ns_files_of(ns).iter().any(|f| self.jvm_state(*f) != 0)
    }

    /// Mova project: public `clojure.core` vars of the Mova layer (natives, host natives, stdlib sources).
    fn mova_core_items(&self, out: &mut Vec<Item>, matches: &dyn Fn(&str) -> bool) {
        let Some(m) = &self.s.mova else { return };
        let core = crate::analyzer::syms().clojure_core;
        let mut files: Vec<FileId> = m.rank.keys().copied().collect();
        files.sort();
        // names the Clojure core table already offered (same var, one item)
        let mut have: HashSet<String> = out.iter().filter(|x| x.priority == "clojure-core").map(|x| x.label.clone()).collect();
        for f in files {
            for (i, d) in self.fa(f).var_definitions.iter().enumerate() {
                if d.ns == core && !d.private && matches(d.name.as_str()) && have.insert(d.name.as_str().to_string()) {
                    let mut it = self.var_def_item(f, i, None, "clojure-core");
                    it.detail = Some(format!("clojure.core/{}", d.name.as_str()));
                    out.push(it);
                }
            }
        }
    }

    /// Var definitions of the namespace in non-local analysis (all files of the ns).
    fn public_defs_all(&self, ns: SymId) -> Vec<(FileId, usize)> {
        let mut out = Vec::new();
        for f in self.s.ns_files_of(ns) {
            if self.jvm_state(f) == 0 {
                continue;
            }
            for (i, d) in self.fa(f).var_definitions.iter().enumerate() {
                if d.ns == ns {
                    out.push((f, i));
                }
            }
        }
        out
    }

    /// dep-graph `ns-aliases`: (alias, ns) over all namespace usages (project + dependency sources) of namespaces that have an internal dependent.
    fn project_aliases(&self) -> Vec<(SymId, SymId)> {
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        let mut used: HashSet<u32> = HashSet::new();
        let mut out = Vec::new();
        used.insert(crate::intern::intern("clojure.core").0);
        used.insert(crate::intern::intern("cljs.core").0);
        for uri in self.s.uris() {
            let Some(id) = self.s.id(uri) else { continue };
            let e = self.entry(id);
            if !e.internal {
                continue;
            }
            let st = self.jvm_state(id);
            if st == 0 {
                continue;
            }
            if let Some(fa) = e.fa() {
                for u in &fa.namespace_usages {
                    if st == 2 {
                        used.insert(u.to.0);
                    }
                    if !u.alias.is_none() && seen.insert((u.alias.0, u.to.0)) {
                        out.push((u.alias, u.to));
                    }
                }
            }
        }
        if let Some(j) = &self.s.jars {
            for (alias, to) in j.aliases() {
                if seen.insert((alias.0, to.0)) {
                    out.push((*alias, *to));
                }
            }
        }
        out.retain(|(_, to)| used.contains(&to.0));
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn local_items(&self, f: FileId, fa: &FileAnalysis, cursor_el: Option<El>, row: u32, col: u32, mask: u8, matches: &dyn Fn(&str) -> bool, out: &mut Vec<Item>) {
        let lang_ok = |l: u8| l == 0 || (l == L_CLJ && mask & CLJ != 0) || (l == L_CLJS && mask & CLJS != 0);
        let (crow, ccol) = match cursor_el {
            Some(e) => {
                let p = self.name_pos(e);
                (p.row, p.col)
            }
            None => (row, col),
        };
        let furi = self.entry(f).uri.to_string();
        let pd = |name: &str, p: Pos| DocRef::Pos { name: name.to_string(), uri: furi.clone(), row: p.row, col: p.col };
        let cursor_bucket_var_usage = cursor_el.map_or(false, |e| e.b == B::VarUsage);
        let cursor_from = cursor_el.filter(|e| e.b == B::VarUsage).map(|e| self.fa(e.f).var_usages[e.i as usize].from);
        for n in &fa.namespace_definitions {
            if lang_ok(n.lang) && matches(n.name.as_str()) {
                out.push(Item { label: n.name.as_str().to_string(), kind: K_MODULE, detail: None, priority: "simple-cursor", deprecated: false, doc: pd(n.name.as_str(), n.name_pos), ..Default::default() });
            }
        }
        for n in &fa.namespace_usages {
            if lang_ok(n.lang) && matches(n.to.as_str()) {
                out.push(Item { label: n.to.as_str().to_string(), kind: K_MODULE, detail: Some(String::new()), priority: "simple-cursor", deprecated: false, doc: pd(n.to.as_str(), n.name_pos), ..Default::default() });
            }
        }
        for (i, d) in fa.var_definitions.iter().enumerate() {
            if d.name.is_none() || !lang_ok(d.lang) || !matches(d.name.as_str()) {
                continue;
            }
            if cursor_bucket_var_usage && Some(d.ns) != cursor_from {
                continue;
            }
            out.push(self.var_def_item(f, i, None, "simple-cursor"));
        }
        for u in &fa.var_usages {
            if u.refer && lang_ok(u.lang) && matches(u.name.as_str()) {
                out.push(Item { label: u.name.as_str().to_string(), kind: K_REFERENCE, detail: Some(format!("refer to: {}", u.to.as_str())), priority: "simple-cursor", deprecated: false, doc: pd(u.name.as_str(), u.name_pos), ..Default::default() });
            }
        }
        for l in &fa.locals {
            if !lang_ok(l.lang) || !matches(l.name.as_str()) {
                continue;
            }
            let (er, ec) = if l.scope_end_row == u32::MAX { (l.pos.end_row, l.pos.end_col) } else { (l.scope_end_row, l.scope_end_col) };
            let inside = (l.pos.row < crow || (l.pos.row == crow && l.pos.col <= ccol)) && (crow < er || (crow == er && ccol <= ec));
            if inside {
                out.push(Item { label: l.name.as_str().to_string(), kind: K_VARIABLE, detail: Some(String::new()), priority: "locals", deprecated: false, doc: pd(l.name.as_str(), l.pos), ..Default::default() });
            }
        }
        for u in &fa.java_class_usages {
            if u.flags & JU_IMPORT == 0 || u.class.is_none() || (u.pos.row == 0 && !(u.flags & JU_HAS_NAME != 0 && u.name_pos.row != 0)) {
                continue;
            }
            let short = u.class.as_str().rsplit('.').next().unwrap_or("");
            let l = if u.flags & JU_CLJS != 0 { L_CLJS } else { L_CLJ };
            if lang_ok(l) && matches(short) {
                out.push(Item { label: short.to_string(), kind: 7, detail: Some(u.class.as_str().to_string()), priority: "java-usages", deprecated: false, doc: pd("", if u.flags & JU_HAS_NAME != 0 && u.name_pos.row != 0 { u.name_pos } else { u.pos }), ..Default::default() });
            }
        }
    }

    /// `with-definition-kws-args-element-items`: the `:keys` of the called function's first param (None = no caller kws).
    fn kw_arg_items(&self, doc: &Doc, cursor: NodeId, at: At, matches: &dyn Fn(&str) -> bool) -> Option<Vec<Item>> {
        let op = doc.find_op(cursor)?;
        let p = doc.cst.pos(op);
        let el = self.first_under_cursor(at.uri, p.row, p.col)?;
        let def = self.find_definition(el)?;
        if def.b != B::VarDef {
            return None;
        }
        let fa = self.fa(def.f);
        let d = &fa.var_definitions[def.i as usize];
        if !d.has_arglists {
            return None;
        }
        let mut kws: Vec<String> = Vec::new();
        for k in 0..d.arglists.1 {
            if let Some(v) = first_param_keys(fa.strs[(d.arglists.0 + k) as usize].as_str()) {
                kws.extend(v);
            }
        }
        if kws.is_empty() {
            return None;
        }
        // keys already present in the enclosing map
        let mut existing: Vec<String> = Vec::new();
        if let Some(up) = doc.parent(cursor) {
            let kids: Vec<NodeId> = doc.cst.sig_children(up).collect();
            if kids.len() != 1 && doc.cst.kind(up) == Kind::Map {
                let n = if kids.len() % 2 == 0 { kids.len() } else { kids.len() - 1 };
                existing = kids[..n].iter().step_by(2).map(|k| if doc.cst.kind(*k) == Kind::String { doc.cst.string_content(*k).to_string() } else { doc.cst.text(*k).to_string() }).collect();
            }
        }
        let uri = self.entry(def.f).uri.to_string();
        let mut out = Vec::new();
        for k in kws {
            let label = format!(":{k}");
            if matches(&label) && !existing.contains(&label) {
                out.push(Item { label, kind: 14, detail: Some(String::new()), priority: "kw-arg", doc: DocRef::Pos { name: k, uri: uri.clone(), row: d.name_pos.row, col: d.name_pos.col }, ..Default::default() });
            }
        }
        Some(out)
    }

    /// Keyword completion: `::alias/x` keyword definitions, or all known keywords.
    fn keyword_items(&self, text: &str, at: At, matches: &dyn Fn(&str) -> bool, cursor_el: Option<El>, out: &mut Vec<Item>) {
        let auto = text.starts_with("::");
        let body = text.trim_start_matches(':');
        let cur_file = self.s.id(at.uri);
        let kw_label = |alias: Option<&str>, ns: Option<&str>, name: &str| {
            let mut l = String::from(":");
            if alias.is_some() {
                l.push(':');
            }
            if let Some(x) = alias.or(ns) {
                l.push_str(x);
                l.push('/');
            }
            l.push_str(name);
            l
        };
        if auto && body.contains('/') {
            // ::alias/name: keyword definitions of the aliased namespace in other files
            let (alias, name) = body.split_once('/').unwrap();
            let Some(fa) = cur_file.and_then(|f| self.entry(f).fa()) else { return };
            let ns = cursor_el.and_then(|e| (e.b == B::KwUsage || e.b == B::KwDef).then(|| self.fa(e.f).keywords[e.i as usize].ns)).filter(|n| !n.is_none()).or_else(|| fa.namespace_usages.iter().find(|u| u.alias.as_str() == alias).map(|u| u.to));
            let Some(ns) = ns else { return };
            for uri in self.s.uris() {
                let Some(f) = self.s.id(uri) else { continue };
                if Some(f) == cur_file || !self.internal(f) {
                    continue;
                }
                let Some(fa) = self.entry(f).fa() else { continue };
                for k in &fa.keywords {
                    if !k.reg.is_none() && k.ns == ns && k.name.as_str().starts_with(name) && self.jvm_state(f) != 0 {
                        let doc = DocRef::Pos { name: k.name.as_str().to_string(), uri: self.entry(f).uri.to_string(), row: k.pos.row, col: k.pos.col };
                        out.push(Item { label: kw_label(Some(alias), None, k.name.as_str()), kind: 14, detail: Some(String::new()), priority: "alias-keyword", deprecated: false, doc, ..Default::default() });
                    }
                }
            }
            return;
        }
        let cursor_pos = cursor_el.map(|e| self.name_pos(e));
        let cursor_from = cursor_el.and_then(|e| match e.b {
            B::KwUsage | B::KwDef => Some(self.fa(e.f).keywords[e.i as usize].from),
            _ => None,
        });
        for uri in self.s.uris() {
            let Some(f) = self.s.id(uri) else { continue };
            if !self.internal(f) {
                continue;
            }
            let Some(fa) = self.entry(f).fa() else { continue };
            if !fa.has_callstack {
                continue;
            }
            let st = self.jvm_state(f);
            if st == 0 {
                continue;
            }
            for k in &fa.keywords {
                if st == 1 && k.reg.is_none() {
                    continue; // external file: keyword usages are not analyzed
                }
                if let Some(p) = cursor_pos {
                    if Some(f) == cur_file && k.pos.row == p.row && k.pos.col == p.col && k.pos.end_row == p.end_row && k.pos.end_col == p.end_col {
                        continue;
                    }
                }
                let (ns, alias) = (if k.ns.is_none() { None } else { Some(k.ns.as_str()) }, if k.alias.is_none() { None } else { Some(k.alias.as_str()) });
                let label = kw_label(alias, ns, k.name.as_str());
                if matches(&label) || ns.map_or(false, |n| matches(n)) || alias.map_or(false, |a| matches(a)) || matches(k.name.as_str()) {
                    let same = cursor_from == Some(k.from);
                    let doc = DocRef::Pos { name: k.name.as_str().to_string(), uri: self.entry(f).uri.to_string(), row: k.pos.row, col: k.pos.col };
                    out.push(Item { label, kind: 14, detail: Some(String::new()), priority: if same { "keyword-same-ns" } else { "keyword" }, deprecated: false, doc, ..Default::default() });
                }
            }
        }
    }
}

/// `completionItem/resolve` (completion.clj `resolve-item`): `data.unresolved` entries resolved (`documentation`,
/// `alias` -> additionalTextEdits), `data` dropped. `item` is the client's item JSON; returns the resolved item JSON.
pub fn resolve_item(s: &Snapshot, item: &str) -> String {
    use crate::analyzer::json::{self, Json};
    let Some(Json::Obj(mut fields)) = json::parse(item) else { return item.to_string() };
    let data = fields.iter().position(|(k, _)| k == "data").map(|i| fields.remove(i).1);
    let q = Q::new(s);
    let unresolved: Vec<Json> = data.as_ref().and_then(|d| d.get("unresolved")).and_then(|u| u.as_arr()).map(|a| a.to_vec()).unwrap_or_default();
    for u in &unresolved {
        let (Some(ty), Some(args)) = (u.as_arr().and_then(|a| a.first()).and_then(|t| t.as_str()), u.as_arr().and_then(|a| a.get(1))) else { continue };
        let arg = |k: &str| args.get(k).and_then(|v| v.as_str());
        let num = |k: &str| args.get(k).and_then(|v| v.as_f64()).map(|n| n as u32);
        match ty {
            "documentation" => {
                let (Some(uri), Some(name), Some(row), Some(col)) = (arg("uri"), arg("name"), num("name-row"), num("name-col")) else { continue };
                let Some(f) = s.id(uri) else { continue };
                let Some(fa) = s.get(uri).and_then(|e| e.fa()) else { continue };
                let Some(i) = fa.var_definitions.iter().position(|d| d.name_pos.row == row && d.name_pos.col == col && d.name.as_str() == name) else { continue };
                let h = q.hdef(El { f, b: B::VarDef, i: i as u32 });
                let mut o = (*s.opts).clone();
                o.hover_markdown = o.completion_markdown;
                let doc = super::hover::hover_documentation(&h, &o, None);
                let value = match json::parse(&doc) {
                    Some(Json::Arr(v)) => Json::Str(
                        v.iter()
                            .map(|x| match x {
                                Json::Str(t) => t.clone(),
                                other => other.get("value").and_then(|v| v.as_str()).map(String::from).unwrap_or_default(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Some(other) => other,
                    None => continue,
                };
                fields.retain(|(k, _)| k != "documentation");
                fields.push(("documentation".into(), value));
            }
            "alias" => {
                let (Some(uri), Some(ns), Some(alias)) = (arg("uri"), arg("ns-to-add"), arg("alias-to-add")) else { continue };
                let Some(text) = s.get(uri).and_then(|e| e.text()) else { continue };
                if let Some(e) = super::alias_edit::add_alias_edit(&Doc::new(&text), ns, alias) {
                    if let Some(edit) = json::parse(&e.json()) {
                        fields.retain(|(k, _)| k != "additionalTextEdits");
                        fields.push(("additionalTextEdits".into(), Json::Arr(vec![edit])));
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    Json::Obj(fields).write(&mut out);
    out
}
