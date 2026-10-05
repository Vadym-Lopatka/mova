//! definition / declaration (queries.clj `find-definition`, `find-declaration`).
use super::*;

impl<'a> Q<'a> {
    /// Last element of `cands` (file, idx) preferring internal files, else external (`find-last-order-by-project-analysis`).
    fn last_by_project(&self, mut cands: Vec<(FileId, u32)>, b: B, ns: SymId) -> Option<El> {
        // JVM iterates `select-keys analysis (ns-uris ns)`, a hash set of the uris defining `ns`: the "last" candidate is the
        // last in that set's iteration order (real JVM: Clojure hash-set order; NX_SET_ORDER=champ for the Mova port)
        let mut all = self.s.ns_files_of(ns);
        for (f, _) in &cands {
            if !all.contains(f) {
                all.push(*f);
            }
        }
        all.sort();
        all.dedup();
        let uris: Vec<&str> = all.iter().map(|f| self.uri(*f)).collect();
        let rank: std::collections::HashMap<FileId, usize> = super::jorder::set_order(&uris).into_iter().enumerate().map(|(r, i)| (all[i], r)).collect();
        cands.sort_by_key(|(f, _)| rank.get(f).copied().unwrap_or(0));
        let pick = |internal: bool| cands.iter().rev().find(|(f, _)| self.internal(*f) == internal).map(|(f, i)| El { f: *f, b, i: *i });
        // Mova project: the Mova layer (stdlib source, then Rust native) is the runtime, ahead of any jar on the classpath
        let mova = || {
            let m = self.s.mova.as_ref()?;
            cands.iter().rev().filter_map(|(f, i)| m.rank.get(f).map(|r| (*r, *f, *i))).max_by_key(|x| x.0).map(|(_, f, i)| El { f, b, i })
        };
        pick(true).or_else(mova).or_else(|| pick(false))
    }

    /// Last namespace definition named `ns` whose lang intersects `langs`.
    pub fn last_ns_def(&self, ns: SymId, langs: u8) -> Option<El> {
        let mut c = Vec::new();
        for f in self.s.ns_files_of(ns) {
            for (i, n) in self.fa(f).namespace_definitions.iter().enumerate() {
                if n.name == ns && n.name_pos.row != 0 && self.langs(El { f, b: B::NsDef, i: i as u32 }) & langs != 0 {
                    c.push((f, i as u32));
                }
            }
        }
        self.last_by_project(c, B::NsDef, ns)
    }

    /// Last var definition `ns/name` with lang in `langs`; `skip_declare` ignores `declare` definitions.
    pub fn last_var_def(&self, ns: SymId, name: SymId, langs: u8, skip_declare: bool) -> Option<El> {
        let mut files = self.s.defs.get(&(ns.0, name.0)).cloned().unwrap_or_default();
        files.sort();
        if let Some(j) = self.s.jars.as_ref() {
            files.extend(j.locate_all(ns, name));
        }
        let mut c = Vec::new();
        for f in files {
            for xi in self.var_def_idx(f, ns, name) {
                let e = El { f, b: B::VarDef, i: xi };
                let d = &self.fa(f).var_definitions[xi as usize];
                if skip_declare && is_clj_declare(d) {
                    continue;
                }
                if self.langs(e) & langs != 0 {
                    c.push((f, xi));
                }
            }
        }
        self.last_by_project(c, B::VarDef, ns)
    }

    pub fn find_definition(&self, e: El) -> Option<El> {
        self.find_def(e, self.langs(e), false)
    }

    /// `find-last` namespace usage with `alias` in the element's own file.
    fn ns_usage_by_alias(&self, f: FileId, alias: SymId) -> Option<El> {
        let fa = self.fa(f);
        fa.namespace_usages.iter().rposition(|u| u.alias == alias).map(|i| El { f, b: B::NsUsage, i: i as u32 })
    }

    /// `find-definition` of a var usage given as (file, to, name, alias).
    fn find_def_var(&self, f: FileId, to0: SymId, name: SymId, alias: SymId, langs: u8, fb: bool) -> Option<El> {
        let mut to = to0;
        if to == syms().unknown_ns && !alias.is_none() && self.last_ns_def(alias, langs).is_some() {
            to = alias;
        }
        if let Some(d) = self.last_var_def(to, name, langs, false) {
            return Some(d);
        }
        if langs & CLJS != 0 {
            if let Some(d) = self.find_def_var(f, to0, name, alias, CLJ, fb) {
                if d.b == B::VarDef && self.fa(d.f).var_definitions[d.i as usize].macro_ {
                    return Some(d);
                }
            }
        }
        if !fb {
            if let Some(d) = self.find_def_var(f, to0, name, alias, CLJS, true) {
                return Some(d);
            }
        }
        if !alias.is_none() {
            if let Some(nu) = self.ns_usage_by_alias(f, alias) {
                return self.find_def(nu, self.langs(nu), fb);
            }
        }
        None
    }

    /// Last keyword definition with the same ns+name (internal files; dependency keyword definitions are not indexed).
    pub fn last_kw_def(&self, ns: SymId, name: SymId) -> Option<El> {
        let mut files = self.s.kws.get(&(ns.0, name.0)).cloned().unwrap_or_default();
        files.sort();
        for &f in files.iter().rev() {
            let mut best = None;
            for xi in self.kw_idx(f, ns, name) {
                if !self.fa(f).keywords[xi as usize].reg.is_none() {
                    best = Some(xi);
                }
            }
            if let Some(i) = best {
                return Some(El { f, b: B::KwDef, i });
            }
        }
        None
    }

    fn find_def(&self, e: El, langs: u8, fb: bool) -> Option<El> {
        let fa = self.fa(e.f);
        match e.b {
            B::NsAlias | B::NsUsage => self.last_ns_def(fa.namespace_usages[e.i as usize].to, langs),
            B::VarUsage => {
                let u = &fa.var_usages[e.i as usize];
                self.find_def_var(e.f, u.to, u.name, u.alias, langs, fb)
            }
            B::Symbols => {
                let s = &fa.symbols[e.i as usize];
                let to = if !s.to.is_none() {
                    s.to
                } else {
                    let t = s.symbol.as_str();
                    crate::intern::intern(t.split_once('/').map_or("", |x| x.0))
                };
                let langs = if s.lang == 3 { CLJ } else { langs };
                self.find_def_var(e.f, to, s.name, SymId::NONE, langs, fb)
            }
            B::KwUsage => {
                let k = &fa.keywords[e.i as usize];
                Some(self.last_kw_def(k.ns, k.name).unwrap_or(e))
            }
            B::ProtoImpl => {
                let p = &fa.protocol_impls[e.i as usize];
                self.last_var_def(p.protocol_ns, p.method_name, langs, false)
            }
            B::JavaClassUsage => {
                // member/class definitions come from the JDK/jar class layer; a defrecord class maps to its var
                let u = &fa.java_class_usages[e.i as usize];
                let c = u.class.as_str();
                let (pkg, cls) = c.rsplit_once('.')?;
                let ns = crate::intern::intern(&pkg.replace('_', "-"));
                self.find_def_var(e.f, ns, crate::intern::intern(cls), SymId::NONE, langs, fb)
            }
            B::LocalUsage => {
                let id = fa.local_usages[e.i as usize].id;
                let pi = self.pos_idx(e.f);
                let idx = pi.local_by_id.get(id as usize).copied().filter(|x| *x != u32::MAX)?;
                Some(El { f: e.f, b: B::Local, i: idx })
            }
            B::VarDef => {
                let d = &fa.var_definitions[e.i as usize];
                if is_clj_declare(d) {
                    if let Some(a) = self.last_var_def(d.ns, d.name, langs, true) {
                        return Some(a);
                    }
                }
                Some(e)
            }
            _ => Some(e),
        }
    }

    /// `find-declaration` (var usages only).
    pub fn find_declaration(&self, e: El) -> Option<El> {
        if e.b != B::VarUsage {
            return None;
        }
        let fa = self.fa(e.f);
        let u = &fa.var_usages[e.i as usize];
        if u.to == syms().unknown_ns {
            return None;
        }
        let langs = self.langs(e);
        let ok = |b: B, i: usize| self.langs(El { f: e.f, b, i: i as u32 }) & langs != 0;
        if !u.alias.is_none() {
            return fa.namespace_usages.iter().enumerate().rev().find(|(i, n)| n.to == u.to && n.alias == u.alias && n.alias_pos.row != 0 && ok(B::NsAlias, *i)).map(|(i, _)| El { f: e.f, b: B::NsAlias, i: i as u32 });
        }
        let by_refer = fa.var_usages.iter().enumerate().rev().find(|(i, v)| v.refer && v.to == u.to && v.name == u.name && ok(B::VarUsage, *i));
        if let Some((i, _)) = by_refer {
            return Some(El { f: e.f, b: B::VarUsage, i: i as u32 });
        }
        fa.namespace_usages.iter().enumerate().rev().find(|(i, n)| n.to == u.to && ok(B::NsUsage, *i)).map(|(i, _)| El { f: e.f, b: B::NsUsage, i: i as u32 })
    }
}

/// `(= 'clojure.core/declare defined-by)`: a cljs `declare` (`cljs.core/declare`) is never skipped (queries.clj:342,353).
fn is_clj_declare(d: &VarDef) -> bool {
    static S: std::sync::OnceLock<(SymId, SymId)> = std::sync::OnceLock::new();
    *S.get_or_init(|| (crate::intern::intern("clojure.core"), crate::intern::intern("declare"))) == d.defined_by
}

pub fn definition(q: &Q, at: At) -> String {
    let Some(e) = q.first_under_cursor(at.uri, at.row(), at.col()) else { return "null".into() };
    if let Some(h) = super::jdk::hit(q, e, false) {
        return h.location();
    }
    if let Some(l) = super::jdk::jar_class_location(q, e) {
        return l;
    }
    match q.find_definition(e) {
        Some(d) => q.location(d),
        None => "null".into(),
    }
}

pub fn declaration(q: &Q, at: At) -> String {
    let Some(e) = q.first_under_cursor(at.uri, at.row(), at.col()) else { return "null".into() };
    match q.find_declaration(e) {
        Some(d) => q.location(d),
        None => "null".into(),
    }
}
