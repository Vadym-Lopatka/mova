//! prepareRename / rename (feature/rename.clj): edits come from the references index, never from file rescans.
use super::hover::uri_filename;
use super::*;

pub const INVALID_PARAMS: i32 = -32602;

/// Marker frame understood by the `mova.nx/query` builtin: `\u{1}code\u{1}message`.
pub fn err_frame(code: i32, msg: &str) -> String {
    format!("\u{1}{code}\u{1}{msg}")
}

pub fn internal_error() -> String {
    err_frame(-32603, "Internal error")
}

fn invalid(msg: &str) -> String {
    err_frame(INVALID_PARAMS, msg)
}

fn no_element() -> String {
    invalid("Can't rename - no element found.")
}

fn utf16(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

struct Status {
    refs: Vec<El>,
    def: El,
    source_path: Option<String>,
}

/// One text edit (1-based positions).
struct Edit {
    f: FileId,
    p: Pos,
    text: String,
}

fn is_kw(b: B) -> bool {
    matches!(b, B::KwDef | B::KwUsage)
}

impl<'a> Q<'a> {
    fn def_ns_is_none(&self, d: El) -> bool {
        match d.b {
            B::VarDef | B::VarUsage => false,
            B::KwDef | B::KwUsage => self.fa(d.f).keywords[d.i as usize].ns.is_none(),
            _ => true,
        }
    }

    fn keys_destructuring(&self, e: El) -> bool {
        is_kw(e.b) && self.fa(e.f).keywords[e.i as usize].flags & KW_KEYS_DESTR != 0
    }

    fn source_path_of(&self, uri: &str) -> Option<String> {
        let p = self.s.project.as_ref()?;
        let name = uri_filename(uri);
        p.source_paths.iter().find(|s| name.starts_with(&if s.ends_with('/') { s.to_string() } else { format!("{s}/") })).cloned()
    }

    /// `find-all-project-namespace-definitions` count: distinct internal files defining `ns`.
    fn project_ns_def_count(&self, ns: SymId) -> usize {
        let mut n = 0;
        for f in self.s.ns_files_of(ns) {
            if self.internal(f) && self.fa(f).namespace_definitions.iter().any(|d| d.name == ns) {
                n += 1;
            }
        }
        n
    }

    fn rename_status(&self, e: El) -> Result<Status, String> {
        let mut def = self.find_definition(e);
        if def.is_none() && super::jdk::has_class_definition(self, e) {
            def = Some(e); // a JDK / jar class: clojure-lsp's definition is a java-class-definition element
        }
        let mut refs = self.find_references(e, true, None);
        if let Some(d) = def {
            if self.def_ns_is_none(d) {
                refs.retain(|r| !self.keys_destructuring(*r));
            }
        }
        let Some(def) = def else { return Err(invalid("Can't rename - no definition found.")) };
        if refs.is_empty() {
            return Err(invalid("Can't rename - no other references found."));
        }
        let source_path = self.source_path_of(self.uri(def.f));
        if def.b == B::NsDef && e.b != B::NsAlias {
            if source_path.is_none() {
                return Err(invalid("Can't rename - invalid source-paths. Are project :source-paths configured correctly?"));
            }
            if !self.s.opts.we_doc_changes {
                return Err(invalid("Can't rename - client does not support file renames."));
            }
            if self.project_ns_def_count(self.fa(def.f).namespace_definitions[def.i as usize].name) != 1 {
                return Err(invalid("Can't rename - namespace is defined in multiple files."));
            }
        }
        Ok(Status { refs, def, source_path })
    }

    fn edit(&self, r: El, p: Pos, text: String) -> Edit {
        Edit { f: r.f, p, text }
    }

    fn rename_other(&self, repl: &str, r: El) -> Result<Edit, ()> {
        let name = self.name(r);
        if name.is_none() {
            return Err(());
        }
        let p = self.name_pos(r);
        let start = p.end_col.saturating_sub(utf16(super::callh::simple_name(name.as_str())));
        Ok(self.edit(r, Pos { col: start, ..p }, repl.to_string()))
    }

    fn rename_alias_definition(&self, repl: &str, r: El) -> Result<Edit, ()> {
        let p = self.name_pos(r);
        if is_kw(r.b) {
            return Ok(self.edit(r, p, format!("::{}/{}", repl, self.name(r).as_str())));
        }
        if r.b == B::NsAlias {
            return Ok(self.edit(r, p, repl.to_string()));
        }
        let name = self.name(r);
        if name.is_none() {
            return Err(());
        }
        let (prefix, _, nm) = ident_split(name.as_str());
        Ok(self.edit(r, p, format!("{prefix}{repl}/{nm}")))
    }

    fn rename_local(&self, repl: &str, r: El) -> Result<Edit, ()> {
        let name = self.name(r);
        if name.is_none() {
            return Err(());
        }
        let p = self.name_pos(r);
        let start = p.end_col.saturating_sub(utf16(super::callh::simple_name(name.as_str())));
        let text = repl.strip_prefix(':').unwrap_or(repl);
        Ok(self.edit(r, Pos { col: start, ..p }, text.to_string()))
    }

    fn rename_defrecord(&self, repl: &str, r: El) -> Result<Edit, ()> {
        let name = self.name(r);
        if name.is_none() {
            return Err(());
        }
        let cur = name.as_str();
        let p = self.name_pos(r);
        let alias = if r.b == B::VarUsage { self.fa(r.f).var_usages[r.i as usize].alias } else { SymId::NONE };
        let col = if alias.is_none() { p.col } else { p.col + 1 + utf16(alias.as_str()) };
        let text = if cur.starts_with("map->") {
            format!("map->{repl}")
        } else if cur.starts_with("->") {
            format!("->{repl}")
        } else {
            repl.to_string()
        };
        Ok(self.edit(r, Pos { col, end_col: col + utf16(cur), ..p }, text))
    }

    fn rename_keyword(&self, repl: &str, raw: &str, r: El, out: &mut Vec<Edit>) {
        let k = &self.fa(r.f).keywords[r.i as usize];
        let p = self.name_pos(r);
        let name = k.name.as_str();
        let qualified_same_ns = p.end_col - p.col == 2 + utf16(name);
        let repl_name = strip_colon_prefixes(repl);
        let repl_ns = replacement_ns(raw);
        let ns_changed = k_has_ns_form(raw);
        let ns = if k.ns.is_none() { "" } else { k.ns.as_str() };
        let local = if k.flags & KW_KEYS_DESTR != 0 {
            let fa = self.fa(r.f);
            fa.locals.iter().position(|l| l.pos.row == p.row && l.pos.col == p.col && l.pos.end_row == p.end_row && l.pos.end_col == p.end_col)
        } else {
            None
        };
        let lstr = local.map(|i| self.fa(r.f).locals[i].str_.as_str());
        let text = if let Some(ls) = lstr.filter(|s| s.contains('/') && s.starts_with(':')) {
            let _ = ls;
            format!(":{}/{}", ns, repl_name)
        } else if lstr.is_some_and(|s| s.contains('/')) {
            format!("{}/{}", ns, repl_name)
        } else if local.is_some() {
            repl_name.clone()
        } else if !k.alias.is_none() {
            format!("::{}/{}", k.alias.as_str(), repl_name)
        } else if qualified_same_ns && raw.starts_with("::") {
            format!("::{}", repl_name)
        } else if qualified_same_ns && raw.starts_with(':') {
            raw.to_string()
        } else if k.flags & KW_PREFIX != 0 {
            format!(":{}", repl_name)
        } else if !k.ns.is_none() && ns_changed {
            format!(":{}/{}", repl_ns, repl_name)
        } else if !k.ns.is_none() {
            format!(":{}/{}", ns, repl_name)
        } else {
            repl.to_string()
        };
        out.push(self.edit(r, p, text));
        if let Some(li) = local {
            let l = El { f: r.f, b: B::Local, i: li as u32 };
            for x in self.find_references(l, false, None) {
                out.push(self.edit(x, self.name_pos(x), repl_name.clone()));
            }
        }
    }

    fn rename_changes(&self, e: El, st: &Status, repl: &str, raw: &str) -> Result<Vec<Edit>, ()> {
        let refs = &st.refs;
        let mut out = Vec::new();
        if e.b == B::NsAlias {
            for &r in refs {
                out.push(self.rename_alias_definition(repl, r)?);
            }
        } else if e.b == B::VarUsage && raw.contains('/') {
            let new_alias = raw.split('/').next().unwrap_or("");
            let old_alias = refs
                .iter()
                .find_map(|r| (r.b == B::VarUsage && r.f == e.f).then(|| self.fa(r.f).var_usages[r.i as usize].alias).filter(|a| !a.is_none()));
            let alias_def = old_alias.and_then(|a| {
                self.fa(e.f).namespace_usages.iter().rposition(|u| u.alias == a && u.alias_pos.row != 0).map(|i| El { f: e.f, b: B::NsAlias, i: i as u32 })
            });
            for &r in refs {
                if r == e || (r.f == e.f && r.b == B::VarUsage && !self.fa(r.f).var_usages[r.i as usize].alias.is_none()) {
                    out.push(self.edit(r, self.name_pos(r), raw.to_string()));
                } else {
                    out.push(self.rename_other(repl, r)?);
                }
            }
            let ad = alias_def.ok_or(())?;
            out.push(self.rename_alias_definition(new_alias, ad)?);
        } else if st.def.b == B::NsDef {
            for &r in refs {
                let text = if is_kw(r.b) { format!(":{}/{}", repl, self.name(r).as_str()) } else { repl.to_string() };
                out.push(self.edit(r, self.name_pos(r), text));
            }
        } else if is_kw(st.def.b) {
            for &r in refs {
                self.rename_keyword(repl, raw, r, &mut out);
            }
        } else if st.def.b == B::Local {
            for &r in refs {
                out.push(self.rename_local(repl, r)?);
            }
        } else if st.def.b == B::VarDef && {
            let d = &self.fa(st.def.f).var_definitions[st.def.i as usize];
            d.defined_by.1.as_str() == "defrecord" || d.defined_by_lint_as.1.as_str() == "defrecord"
        } {
            for &r in refs {
                if r.b == B::VarDef {
                    let n = self.name(r).as_str();
                    if n.starts_with("->") || n.starts_with("map->") {
                        continue;
                    }
                }
                out.push(self.rename_defrecord(repl, r)?);
            }
        } else {
            for &r in refs {
                out.push(self.rename_other(repl, r)?);
            }
        }
        Ok(out)
    }

    fn doc_version(&self, f: FileId) -> i64 {
        self.entry(f).version.max(0)
    }
}

/// `ident-split`: `[prefix ns name]` (ns empty when absent).
fn ident_split(s: &str) -> (&str, Option<&str>, &str) {
    let nc = if s.starts_with("::") { 2 } else if s.starts_with(':') { 1 } else { 0 };
    let (prefix, conformed) = s.split_at(nc);
    match conformed.find('/') {
        Some(i) if i + 1 != conformed.len() => (prefix, Some(&conformed[..i]), &conformed[i + 1..]),
        _ => (prefix, None, conformed),
    }
}

/// `(string/replace s #":+(.+/)?" "")`.
fn strip_colon_prefixes(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != ':' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        while i < b.len() && b[i] == ':' {
            i += 1;
        }
        // optional `.+/`: greedy, up to the last '/' after at least one char
        if let Some(j) = (i + 1..b.len()).rev().find(|&j| b[j] == '/') {
            i = j + 1;
        }
    }
    out
}

/// `(re-matches #"^:(.+)/.+" s)`.
fn k_has_ns_form(s: &str) -> bool {
    let Some(r) = s.strip_prefix(':') else { return false };
    let c: Vec<char> = r.chars().collect();
    c.len() >= 3 && (1..c.len() - 1).any(|j| c[j] == '/')
}

/// `(string/replace s #":+(.+)/.+" "$1")` for strings that match `^:(.+)/.+`; unchanged otherwise.
fn replacement_ns(s: &str) -> String {
    let lead = s.chars().take_while(|c| *c == ':').count();
    if lead == 0 {
        return s.to_string();
    }
    let c: Vec<char> = s.chars().collect();
    for k in (1..=lead).rev() {
        let rest = &c[k..];
        if let Some(j) = (1..rest.len().saturating_sub(1)).rev().find(|&j| rest[j] == '/') {
            return rest[..j].iter().collect();
        }
    }
    s.to_string()
}

fn range_of(p: Pos) -> String {
    range_json(p)
}

fn edit_json(e: &Edit, annot: bool) -> String {
    let mut s = format!("{{\"range\":{},\"newText\":{}", range_of(e.p), json_str(&e.text));
    if annot {
        s.push_str(",\"annotationId\":\"confirmClojureLspRefactor\"");
    }
    s.push('}');
    s
}

enum Change {
    Doc(FileId, Vec<Edit>),
    Rename(String, String),
}

fn client_changes(q: &Q, changes: Vec<Change>) -> String {
    let o = &q.s.opts;
    let dc = o.we_doc_changes || o.we_resource_ops;
    let annot = dc && o.we_annotations && changes.len() > 1;
    let mut s = String::new();
    if dc {
        s.push_str("{\"documentChanges\":[");
        for (n, c) in changes.iter().enumerate() {
            if n > 0 {
                s.push(',');
            }
            match c {
                Change::Doc(f, edits) => {
                    let es: Vec<String> = edits.iter().map(|e| edit_json(e, annot)).collect();
                    let _ = write!(s, "{{\"textDocument\":{{\"uri\":{},\"version\":{}}},\"edits\":[{}]}}", json_str(q.uri(*f)), q.doc_version(*f), es.join(","));
                }
                Change::Rename(old, new) => {
                    let _ = write!(s, "{{\"kind\":\"rename\",\"oldUri\":{},\"newUri\":{}", json_str(old), json_str(new));
                    if annot {
                        s.push_str(",\"annotationId\":\"confirmClojureLspRefactor\"");
                    }
                    s.push('}');
                }
            }
        }
        s.push(']');
        if annot {
            s.push_str(",\"changeAnnotations\":{\"confirmClojureLspRefactor\":{\"label\":\"Confirm clojure-lsp refactor\",\"needsConfirmation\":true}}");
        }
        s.push('}');
    } else {
        s.push_str("{\"changes\":{");
        let mut first = true;
        for c in &changes {
            if let Change::Doc(f, edits) = c {
                if !first {
                    s.push(',');
                }
                first = false;
                let es: Vec<String> = edits.iter().map(|e| edit_json(e, false)).collect();
                let _ = write!(s, "{}:[{}]", json_str(q.uri(*f)), es.join(","));
            }
        }
        s.push_str("}}");
    }
    s
}

pub fn prepare_rename(q: &Q, at: At) -> String {
    let Some(e) = q.first_under_cursor(at.uri, at.row(), at.col()) else { return no_element() };
    match q.rename_status(e) {
        Err(m) => m,
        Ok(_) => range_json(q.name_pos(e)),
    }
}

pub fn rename(q: &Q, at: At, new_name: &str) -> String {
    let Some(e) = q.first_under_cursor(at.uri, at.row(), at.col()) else { return no_element() };
    let st = match q.rename_status(e) {
        Err(m) => return m,
        Ok(s) => s,
    };
    let repl = match new_name.rfind('/') {
        Some(i) => &new_name[i + 1..],
        None => new_name,
    };
    let Ok(edits) = q.rename_changes(e, &st, repl, new_name) else { return internal_error() };
    let mut groups: Vec<(FileId, Vec<Edit>)> = Vec::new();
    for ed in edits {
        match groups.iter_mut().find(|g| g.0 == ed.f) {
            Some(g) => g.1.push(ed),
            None => groups.push((ed.f, vec![ed])),
        }
    }
    if st.def.b == B::NsDef && e.b != B::NsAlias {
        let old = q.uri(st.def.f).to_string();
        let ext = if old.ends_with(".cljs") {
            "cljs"
        } else if old.ends_with(".cljc") {
            "cljc"
        } else if old.ends_with(".edn") {
            "edn"
        } else if old.ends_with(".mova") {
            "mova"
        } else {
            "clj"
        };
        let sp = st.source_path.clone().unwrap_or_default();
        let rel = format!("{}.{}", repl.replace('.', "/").replace('-', "_"), ext);
        let path = std::path::Path::new(&sp).join(rel);
        let new = crate::engine::scan::path_to_uri(&path);
        return client_changes(q, vec![Change::Rename(old, new)]);
    }
    client_changes(q, groups.into_iter().map(|(f, es)| Change::Doc(f, es)).collect())
}
