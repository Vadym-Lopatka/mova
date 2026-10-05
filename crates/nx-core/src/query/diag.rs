//! Diagnostics of an open file: kondo findings (syntax + linters in `FileEntry::findings`) followed by clojure-lsp's own
//! `clojure-lsp/unused-public-var` (built_in.clj `unused-public-vars`, default level :info, default excludes).
use super::symbols::doc_var_defs;
use super::text::Doc;
use super::*;
use crate::cst::Kind;
use crate::engine::index::equal_range;
use crate::engine::lsp::{to_diagnostic, Diagnostic};
use crate::engine::types::{Finding, Level};
use std::collections::HashSet;
use std::sync::Arc;

/// clojure-lsp `default-public-vars-defined-by-to-exclude`.
const EXCLUDED_DEFINED_BY: [&str; 4] = ["clojure.test/deftest", "cljs.test/deftest", "state-flow.cljtest/defflow", "potemkin/import-vars"];

impl<'a> Q<'a> {
    fn is_excluded_def(&self, fa: &FileAnalysis, d: &VarDef, e: &FileEntry) -> bool {
        // inside (comment ...)
        for k in 0..d.cs.1 {
            let (ns, name) = fa.callstacks[(d.cs.0 + k) as usize];
            if name.as_str() == "comment" && ns.as_str() == "clojure.core" {
                return true;
            }
        }
        let by = d.defined_by.1.as_str();
        let q = |p: (SymId, SymId)| if p.0.is_none() { p.1.as_str().to_string() } else { format!("{}/{}", p.0.as_str(), p.1.as_str()) };
        let bys = [q(d.defined_by), q(d.defined_by_lint_as)];
        let cfg = self.s.project.as_ref().map(|p| p.upv.clone()).unwrap_or_default();
        if bys.iter().any(|b| EXCLUDED_DEFINED_BY.contains(&b.as_str()) || cfg.exclude_when_defined_by.contains(b) || cfg.exclude_when_defined_by_regex.iter().any(|r| r.is_match(b))) {
            return true;
        }
        let (ns, name) = (d.ns.as_str(), d.name.as_str());
        let fq = format!("{ns}/{name}");
        if cfg.exclude_simple.iter().any(|s| s == name || s == ns) || cfg.exclude_fq.contains(&fq) || cfg.exclude_regex.iter().any(|r| r.is_match(&fq)) {
            return true;
        }
        // definterface methods
        if by == "definterface" && !d.protocol_name.is_none() {
            return true;
        }
        if d.name.as_str() == "-main" || !d.export.is_none() {
            return true;
        }
        if d.name.as_str().starts_with('-') && e.text().map_or(false, |t| t.contains(":gen-class")) {
            return true;
        }
        false
    }

    /// Is some usage of `ns/name` (or record/type constructor names) present in the ns + dependents files?
    fn var_used(&self, scope: &HashSet<FileId>, ns: SymId, names: &[SymId]) -> bool {
        for &n in names {
            let Some(files) = self.s.uses.get(&(ns.0, n.0)) else { continue };
            for &f in files {
                if !scope.contains(&f) || !self.s.in_source_paths(&self.entry(f).uri) {
                    continue;
                }
                let fa = self.fa(f);
                for &(_, _, i) in equal_range(&self.entry(f).tgt, ns, n) {
                    let u = &fa.var_usages[i as usize];
                    if !u.from_var.is_none() && u.from_var == u.name && u.from == u.to {
                        continue;
                    }
                    return true;
                }
            }
        }
        false
    }

    /// Unused public vars defined in file `f` (internal files only).
    pub fn unused_public_vars(&self, f: FileId) -> Vec<Finding> {
        let e = self.entry(f);
        let Some(fa) = e.fa() else { return Vec::new() };
        if !e.internal || !self.s.in_source_paths(&e.uri) {
            return Vec::new();
        }
        if fa.mova && fa.namespace_definitions.is_empty() {
            return Vec::new(); // a Mova script (no `ns`): its top-level defs are not an API
        }
        let level = match self.s.project.as_ref().and_then(|p| p.upv.level) {
            Some(0) => return Vec::new(),
            Some(2) => Level::Warning,
            Some(3) => Level::Error,
            _ => Level::Info,
        };
        let mut scopes: std::collections::HashMap<u32, HashSet<FileId>> = std::collections::HashMap::new();
        let mut symsets: std::collections::HashMap<u32, HashSet<(u32, u32)>> = std::collections::HashMap::new();
        let open = e.text.is_some();
        let var_nses: HashSet<u32> = fa.var_definitions.iter().map(|d| d.ns.0).collect();
        let mut out = Vec::new();
        for i in doc_var_defs(fa, false) {
            let d = &fa.var_definitions[i];
            if self.is_excluded_def(fa, d, e) {
                continue;
            }
            // JVM: an open file is re-analysed alone (scope = ns + dependents); closed files get the startup batch (union ~ every internal file).
            let scope = scopes.entry(if open { d.ns.0 } else { u32::MAX }).or_insert_with(|| {
                if open {
                    let mut sc: HashSet<FileId> = self.s.ns_and_dependents(d.ns).into_iter().collect();
                    sc.insert(f); // ns-less script files (implicit `user`) have no ns-definition: the file itself still counts
                    sc
                } else {
                    self.s.uris().filter_map(|u| self.s.id(u)).filter(|&i| self.entry(i).internal).collect()
                }
            });
            let mut names = vec![d.name];
            let by = d.defined_by.1.as_str();
            if by == "defrecord" || by == "deftype" {
                names.push(crate::intern::intern(&format!("->{}", d.name.as_str())));
            }
            if by == "defrecord" {
                names.push(crate::intern::intern(&format!("map->{}", d.name.as_str())));
            }
            if self.var_used(scope, d.ns, &names) {
                continue;
            }
            // quoted qualified symbols (`requiring-resolve 'ns/var`): kondo `:symbols` bucket, signature [ns-of-symbol name].
            let syms = symsets.entry(if open { d.ns.0 } else { u32::MAX }).or_insert_with(|| {
                let mut set = HashSet::new();
                for &sf in scope.iter().filter(|&&i| self.s.in_source_paths(&self.entry(i).uri)) {
                    let Some(sfa) = self.entry(sf).fa() else { continue };
                    for u in &sfa.symbols {
                        let sym = u.symbol.as_str();
                        let Some((nsp, _)) = sym.split_once('/') else { continue };
                        let nsid = crate::intern::intern(nsp);
                        let target = if u.to.is_none() { nsid } else { u.to };
                        if var_nses.contains(&target.0) {
                            set.insert((nsid.0, u.name.0));
                        }
                    }
                }
                set
            });
            if names.iter().any(|n| syms.contains(&(d.ns.0, n.0))) {
                continue;
            }
            out.push(Finding {
                level,
                ty: "clojure-lsp/unused-public-var".into(),
                row: d.name_pos.row,
                col: d.name_pos.col,
                end_row: d.name_pos.end_row,
                end_col: d.name_pos.end_col,
                message: format!("Unused public var '{}/{}'", d.ns.as_str(), d.name.as_str()),
            });
        }
        // keyword definitions (spec / re-frame registrations) without any usage in the project
        let mut seen: HashSet<(u32, u32, u32, u32)> = HashSet::new();
        for k in &fa.keywords {
            if k.reg.is_none() || !seen.insert((k.ns.0, k.name.0, k.pos.row, k.pos.col)) {
                continue;
            }
            if self.kw_used(k.ns, k.name) {
                continue;
            }
            let message = if k.ns.is_none() { format!("Unused public keyword ':{}'", k.name.as_str()) } else { format!("Unused public keyword ':{}/{}'", k.ns.as_str(), k.name.as_str()) };
            out.push(Finding { level, ty: "clojure-lsp/unused-public-var".into(), row: k.pos.row, col: k.pos.col, end_row: k.pos.end_row, end_col: k.pos.end_col, message });
        }
        out
    }

    /// Any keyword usage (non-definition) with the same ns + name in the project.
    fn kw_used(&self, ns: SymId, name: SymId) -> bool {
        let Some(files) = self.s.kws.get(&(ns.0, name.0)) else { return false };
        for &f in files {
            let fa = self.fa(f);
            for xi in self.kw_idx(f, ns, name) {
                if fa.keywords[xi as usize].reg.is_none() {
                    return true;
                }
            }
        }
        false
    }
}

/// `#_:clj-kondo/ignore` / `#_:clojure-lsp/ignore` (+ `#_{:clj-kondo/ignore [:code]}`): ranges of the ignored next form.
fn ignore_ranges(text: &str) -> Vec<(crate::cst::Pos, Option<Vec<String>>)> {
    if !text.contains(":clj-kondo/ignore") && !text.contains(":clojure-lsp/ignore") {
        return Vec::new();
    }
    let doc = Doc::new(text);
    let cst = &doc.cst;
    let mut out = Vec::new();
    for i in 0..cst.len() {
        let n = crate::cst::NodeId(i as u32);
        if cst.kind(n) != Kind::Uneval {
            continue;
        }
        let Some(&inner) = cst.children(n).first() else { continue };
        let Some(p) = doc.parent(n) else { continue };
        let sibs: Vec<_> = cst.children(p).to_vec();
        let Some(pos) = sibs.iter().position(|c| *c == n) else { continue };
        let Some(&next) = sibs[pos + 1..].iter().find(|c| cst.kind(**c) != Kind::Uneval) else { continue };
        let is_kw = |x| cst.kind(x) == Kind::Keyword && matches!(cst.text(x), ":clj-kondo/ignore" | ":clojure-lsp/ignore");
        if is_kw(inner) {
            out.push((cst.pos(next), None));
        } else if cst.kind(inner) == Kind::Map {
            let kids: Vec<_> = cst.sig_children(inner).collect();
            if kids.len() == 2 && is_kw(kids[0]) {
                let codes: Vec<String> = if cst.kind(kids[1]) == Kind::Vector { cst.sig_children(kids[1]).map(|c| cst.text(c).trim_start_matches(':').to_string()).collect() } else { Vec::new() };
                out.push((cst.pos(next), if codes.is_empty() { None } else { Some(codes) }));
            }
        }
    }
    out
}

/// All diagnostics of `uri` (kondo findings, then built-in), LSP shape.
pub fn diagnostics(s: &Snapshot, uri: &str) -> Vec<Diagnostic> {
    let q = Q::new(s);
    let Some(f) = s.id(uri) else { return Vec::new() };
    let e = q.entry(f);
    let mut out: Vec<Diagnostic> = e.findings.iter().map(to_diagnostic).collect();
    let builtin = q.unused_public_vars(f);
    if !builtin.is_empty() {
        let ignores = e.text().as_deref().map(ignore_ranges).unwrap_or_default();
        for b in &builtin {
            let inside = |p: &crate::cst::Pos| {
                // `shared/inside?`: diagnostic name range starts inside the ignored form
                (p.row < b.row || (p.row == b.row && p.col <= b.col)) && (b.row < p.end_row || (b.row == p.end_row && b.col <= p.end_col))
            };
            let ignored = ignores.iter().any(|(p, codes)| codes.as_ref().map_or(true, |c| c.iter().any(|x| x == "clojure-lsp/unused-public-var")) && inside(p));
            if !ignored {
                out.push(to_diagnostic(b));
            }
        }
    }
    out
}

/// Startup lint (JVM `publish-all-diagnostics-directly!` with `publish-empty? false`): diagnostics of every internal
/// file that has any; empty unless `:lint-project-files-after-startup?` (default true). Sorted by uri.
pub fn project_diagnostics(s: &Snapshot) -> Vec<(String, Vec<Diagnostic>)> {
    if !s.project.as_ref().map_or(false, |p| p.lint_after_startup) {
        return Vec::new();
    }
    let mut uris: Vec<&Arc<str>> = s.uris().filter(|u| s.get(u).map_or(false, |e| e.internal && e.lang != crate::engine::types::Lang::Edn)).collect();
    uris.sort();
    uris.into_iter()
        .filter_map(|u| {
            let d = diagnostics(s, u);
            (!d.is_empty()).then(|| (u.to_string(), d))
        })
        .collect()
}
