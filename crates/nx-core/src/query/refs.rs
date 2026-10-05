//! references / documentHighlight (queries.clj `find-references*`).
use super::*;
use crate::engine::index::equal_range;
use std::collections::HashSet;

/// Dedup key `[uri name row col]` (declaration `row/col`, FEATURES T7).
/// The flag marks keyword elements: kondo gives them a string `:name`, never equal to a symbol name at the same place.
type Key = (FileId, u32, u32, u32, bool);

fn defrecord_like(d: &VarDef) -> (bool, bool) {
    let s = |n: &str| d.defined_by.1.as_str() == n;
    (s("defrecord"), s("deftype"))
}

impl<'a> Q<'a> {
    /// `var-definition-names`: names a definition can be referenced by.
    fn var_names(&self, ns_name: SymId, def: Option<&VarDef>) -> Vec<SymId> {
        let mut v = vec![ns_name];
        if let Some(d) = def {
            let (rec, ty) = defrecord_like(d);
            if rec || ty {
                v.push(crate::intern::intern(&format!("->{}", ns_name.as_str())));
            }
            if rec {
                v.push(crate::intern::intern(&format!("map->{}", ns_name.as_str())));
            }
        }
        v
    }

    fn key(&self, e: El) -> Key {
        let p = self.form_pos(e);
        (e.f, self.name(e).0, p.row, p.col, matches!(e.b, B::KwDef | B::KwUsage))
    }

    /// Hits for `ns/names`: definitions (when `include`) and usages over ns + dependents.
    fn var_refs(&self, ns: SymId, names: &[SymId], include: bool, only: Option<FileId>, out: &mut Vec<El>) {
        // JVM highlight: local analysis intersected with the dep-graph ns + dependents (`js/x` -> empty)
        let mut files: Vec<FileId> = self.s.ns_and_dependents(ns);
        if let Some(f) = only {
            // keep `f` when the dep-graph relates it to `ns`; script files without `ns` form (implicit `user`) are kept
            // (measured: JVM keeps them), others through their own definitions / namespace usages
            let fa = self.fa(f);
            let related = files.contains(&f)
                || fa.namespace_definitions.is_empty()
                || fa.namespace_definitions.iter().any(|n| n.name == ns)
                || fa.namespace_usages.iter().any(|u| u.to == ns)
                || fa.var_definitions.iter().any(|d| d.ns == ns);
            files = if related { vec![f] } else { Vec::new() };
        }
        if include && only.is_none() {
            // dependency definitions can live in a secondary file of the namespace (in-ns continuations)
            for &n in names {
                if let Some(jv) = self.s.jars.as_ref() {
                    for j in jv.locate_all(ns, n) {
                        if !files.contains(&j) {
                            files.push(j);
                        }
                    }
                }
            }
        }
        for f in files {
            let fa = self.fa(f);
            if include {
                for &n in names {
                    for xi in self.var_def_idx(f, ns, n) {
                        out.push(El { f, b: B::VarDef, i: xi });
                    }
                }
            }
            let tgt = &self.entry(f).tgt;
            for &n in names {
                for &(_, _, i) in equal_range(tgt, ns, n) {
                    let u = &fa.var_usages[i as usize];
                    if u.name_pos.row == 0 {
                        continue; // `valid-element?`: no name location
                    }
                    if !include && !u.from_var.is_none() && u.from_var == u.name && u.from == u.to {
                        continue;
                    }
                    out.push(El { f, b: B::VarUsage, i });
                }
            }
            for (i, sy) in fa.symbols.iter().enumerate() {
                if names.contains(&sy.name) && self.symbol_ns(sy) == ns {
                    out.push(El { f, b: B::Symbols, i: i as u32 });
                }
            }
        }
    }

    /// `(or :to (symbol (namespace symbol)))` of a quoted symbol.
    fn symbol_ns(&self, s: &SymbolUse) -> SymId {
        if !s.to.is_none() {
            s.to
        } else {
            crate::intern::intern(s.symbol.as_str().split_once('/').map_or("", |x| x.0))
        }
    }

    /// Internal files holding keywords `(ns, name)`.
    fn kw_files(&self, ns: SymId, name: SymId) -> Vec<FileId> {
        let mut v = self.s.kws.get(&(ns.0, name.0)).cloned().unwrap_or_default();
        v.sort();
        v
    }

    /// `:keywords` references: definitions + usages (include) or usages, same ns+name, internal analysis.
    fn kw_refs(&self, ns: SymId, name: SymId, include: bool, only: Option<FileId>, out: &mut Vec<El>) {
        let files = match only {
            Some(f) => vec![f],
            None => self.kw_files(ns, name),
        };
        for f in files {
            let fa = self.fa(f);
            for xi in self.kw_idx(f, ns, name) {
                let is_def = !fa.keywords[xi as usize].reg.is_none();
                if is_def && !include {
                    continue;
                }
                out.push(El { f, b: if is_def { B::KwDef } else { B::KwUsage }, i: xi });
            }
        }
    }

    /// `find-references` for one element.
    pub fn find_references(&self, e: El, include: bool, only: Option<FileId>) -> Vec<El> {
        let fa = self.fa(e.f);
        let mut out = Vec::new();
        match e.b {
            B::VarUsage => {
                let u = &fa.var_usages[e.i as usize];
                if u.to == syms().unknown_ns {
                    return vec![e];
                }
                self.var_refs(u.to, &[u.name], include, only, &mut out);
            }
            B::VarDef => {
                let d = &fa.var_definitions[e.i as usize];
                let names = self.var_names(d.name, Some(d));
                self.var_refs(d.ns, &names, include, only, &mut out);
            }
            B::NsDef | B::NsUsage => {
                let name = self.name(e);
                if include {
                    out.push(e);
                }
                let mut files: Vec<FileId> = match only {
                    Some(f) => vec![f],
                    None => self.s.ns_and_dependents(name),
                };
                if only.is_none() {
                    if let Some(j) = &self.s.jars {
                        let extra: Vec<FileId> = j.ns_users(name).into_iter().filter(|f| !files.contains(f)).collect();
                        files.extend(extra);
                    }
                }
                let mut seen: HashSet<Key> = HashSet::new();
                for f in files {
                    for (i, u) in self.fa(f).namespace_usages.iter().enumerate() {
                        if u.to == name && u.name_pos.row != 0 {
                            let el = El { f, b: B::NsUsage, i: i as u32 };
                            if seen.insert(self.key(el)) {
                                out.push(el);
                            }
                        }
                    }
                }
                // all keywords whose ns is this namespace (whole project)
                let mut kf = match only {
                    Some(f) => vec![f],
                    None => self.s.kw_ns.get(&name.0).cloned().unwrap_or_default(),
                };
                kf.sort();
                for f in kf {
                    for (i, k) in self.fa(f).keywords.iter().enumerate() {
                        if k.ns == name && k.flags & (KW_AUTO | KW_PREFIX) == 0 {
                            let b = if !k.reg.is_none() { B::KwDef } else { B::KwUsage };
                            let el = El { f, b, i: i as u32 };
                            if seen.insert(self.key(el)) {
                                out.push(el);
                            }
                        }
                    }
                }
            }
            B::NsAlias => {
                let alias = fa.namespace_usages[e.i as usize].alias;
                if include {
                    out.push(e);
                }
                let mut seen: HashSet<(FileId, u32, u32, u32)> = HashSet::new();
                let mut push = |el: El, me: &Self, out: &mut Vec<El>| {
                    let p = me.name_pos(el);
                    if seen.insert((el.f, me.name(el).0, p.row, p.col)) {
                        out.push(el);
                    }
                };
                let (mut kd, mut ku) = (Vec::new(), Vec::new());
                for (i, k) in fa.keywords.iter().enumerate() {
                    if k.alias == alias {
                        if !k.reg.is_none() {
                            kd.push(El { f: e.f, b: B::KwDef, i: i as u32 });
                        } else if fa.has_callstack {
                            ku.push(El { f: e.f, b: B::KwUsage, i: i as u32 });
                        }
                    }
                }
                for el in kd.into_iter().chain(ku) {
                    push(el, self, &mut out);
                }
                for (i, u) in fa.var_usages.iter().enumerate() {
                    if u.alias == alias && !u.derived && !u.derived_name {
                        push(El { f: e.f, b: B::VarUsage, i: i as u32 }, self, &mut out);
                    }
                }
            }
            B::KwDef | B::KwUsage => {
                let k = &fa.keywords[e.i as usize];
                self.kw_refs(k.ns, k.name, include, only, &mut out);
            }
            B::Symbols => {
                let sy = &fa.symbols[e.i as usize];
                if include {
                    out.push(e);
                }
                let to = self.symbol_ns(sy);
                for f in self.s.ns_and_dependents(to) {
                    if !self.internal(f) {
                        continue;
                    }
                    for (i, o) in self.fa(f).symbols.iter().enumerate() {
                        if o.name == sy.name && o.to == sy.to {
                            out.push(El { f, b: B::Symbols, i: i as u32 });
                        }
                    }
                }
                self.var_refs(to, &[sy.name], include, only, &mut out);
            }
            B::ProtoImpl => {
                let p = &fa.protocol_impls[e.i as usize];
                if include {
                    out.push(e);
                }
                self.var_refs(p.protocol_ns, &[p.method_name], false, only, &mut out);
                out.retain(|h| h.b != B::VarDef);
            }
            B::Local | B::LocalUsage => {
                let id = match e.b {
                    B::Local => fa.locals[e.i as usize].id,
                    _ => fa.local_usages[e.i as usize].id,
                };
                if include {
                    for (i, l) in fa.locals.iter().enumerate() {
                        if l.id == id {
                            out.push(El { f: e.f, b: B::Local, i: i as u32 });
                        }
                    }
                }
                for (i, l) in fa.local_usages.iter().enumerate() {
                    if l.id == id {
                        out.push(El { f: e.f, b: B::LocalUsage, i: i as u32 });
                    }
                }
            }
            _ => out.push(e),
        }
        out
    }

    pub fn references_from_cursor(&self, at: At, include: bool, only_file: bool) -> Option<Vec<El>> {
        let els = self.under_cursor(at.uri, at.row(), at.col());
        if els.is_empty() {
            return None;
        }
        let only = if only_file { self.s.id(at.uri) } else { None };
        let mut seen: HashSet<Key> = HashSet::new();
        let mut out = Vec::new();
        for e in els {
            for h in self.find_references(e, include, only) {
                if seen.insert(self.key(h)) {
                    out.push(h);
                }
            }
        }
        Some(out)
    }
}

pub fn references(q: &Q, at: At, include: bool) -> String {
    let mut s = String::from("[");
    if let Some(hits) = q.references_from_cursor(at, include, false) {
        for (n, h) in hits.iter().enumerate() {
            if n > 0 {
                s.push(',');
            }
            s.push_str(&q.location(*h));
        }
    }
    s.push(']');
    s
}

/// documentHighlight: references with declaration, restricted to the file; `[{range}]`.
pub fn highlight(q: &Q, at: At) -> String {
    let mut s = String::from("[");
    if let Some(hits) = q.references_from_cursor(at, true, true) {
        for (n, h) in hits.iter().enumerate() {
            if n > 0 {
                s.push(',');
            }
            let _ = write!(s, "{{\"range\":{}}}", range_json(q.name_pos(*h)));
        }
    }
    s.push(']');
    s
}
