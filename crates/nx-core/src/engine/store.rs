//! Immutable snapshots swapped atomically. One writer (`commit`), lock-free readers (`snapshot`).
//!
//! A snapshot holds `Arc<FileEntry>` per uri (analysis + position index) and global indexes kept
//! incrementally: defs by (ns,name), usage files by target (ns,name), ns -> files, ns -> dependents.
//! Re-analysis of one file patches only that file's index entries; when its definitions changed, the
//! project `DefsIndex` is rebuilt and only files whose usage targets may change are re-finished.
use super::ctx::{ClientOpts, Ctx, SharedCtx};
use super::index;
use super::jarview::{JarView, EXT_BASE};
use super::pool::Done;
use super::scan::ProjectInfo;
use super::shmap::ShardedMap;
use super::types::{FileEntry, Lang};
use crate::analyzer::{self, BaseLang, DefsIndex, FileAnalysis, Src, L_CLJS};
use crate::intern::SymId;
use arc_swap::ArcSwap;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

pub type FileId = u32;
type Key2 = (u32, u32);

/// Immutable view of all files. Cheap to hold; never mutated after publication.
#[derive(Clone)]
pub struct Snapshot {
    pub version: u64,
    pub files: Arc<Vec<Option<Arc<FileEntry>>>>,
    pub by_uri: ShardedMap<Arc<str>, FileId>,
    /// ns -> files defining it (dep-graph `:uris`).
    pub ns_files: ShardedMap<u32, Vec<FileId>>,
    /// to-ns -> (from-ns, file of the usage) (dep-graph `:dependents`).
    pub ns_deps: ShardedMap<u32, Vec<(u32, FileId)>>,
    /// (ns, name) -> files with that var-definition.
    pub defs: ShardedMap<Key2, Vec<FileId>>,
    /// (to, name) -> files with a var-usage of it.
    pub uses: ShardedMap<Key2, Vec<FileId>>,
    /// (ns, name) -> internal files with a keyword definition or usage (ns `u32::MAX` = unqualified).
    pub kws: ShardedMap<Key2, Vec<FileId>>,
    /// keyword ns -> internal files having keywords in it.
    pub kw_ns: ShardedMap<u32, Vec<FileId>>,
    pub project: Option<Arc<ProjectInfo>>,
    pub opts: Arc<ClientOpts>,
    pub file_count: usize,
    /// Dependency layer (native jar cache), lazily decoded.
    pub jars: Option<Arc<JarView>>,
    /// Jar files opened by a request (JVM `force-get-document-text`): fully analyzed, replace the lazy stubs.
    pub promoted: Arc<std::collections::HashMap<FileId, Arc<FileEntry>>>,
    /// Mova layer (stdlib sources + Rust natives) of a Mova project.
    pub mova: Option<Arc<MovaView>>,
}

/// Mova layer files among the external files (`crate::mova`).
#[derive(Default, Debug)]
pub struct MovaView {
    /// file -> `mova::RANK_*`.
    pub rank: std::collections::HashMap<FileId, u8>,
    /// Namespaces usable without a require: (written prefix, namespace).
    pub auto_ns: Vec<(crate::intern::SymId, crate::intern::SymId)>,
    /// Mova checkout the layer points into.
    pub root: std::path::PathBuf,
}

/// Mova part of `Store::set_dir_files`.
pub struct MovaMeta {
    pub rank: std::collections::HashMap<String, u8>,
    pub auto_ns: Vec<(crate::intern::SymId, crate::intern::SymId)>,
    pub root: std::path::PathBuf,
}

impl Snapshot {
    fn empty() -> Snapshot {
        Snapshot {
            version: 0,
            files: Arc::new(Vec::new()),
            by_uri: ShardedMap::default(),
            ns_files: ShardedMap::default(),
            ns_deps: ShardedMap::default(),
            defs: ShardedMap::default(),
            uses: ShardedMap::default(),
            kws: ShardedMap::default(),
            kw_ns: ShardedMap::default(),
            project: None,
            opts: Arc::new(ClientOpts::default()),
            file_count: 0,
            jars: None,
            promoted: Arc::new(Default::default()),
            mova: None,
        }
    }
    pub fn id(&self, uri: &str) -> Option<FileId> {
        if let Some(i) = self.by_uri.get(uri) {
            return Some(*i);
        }
        // client uri through a symlink (macOS /var -> /private/var): analysis stores canonical paths
        let p = super::scan::uri_to_path(uri)?;
        let c = std::fs::canonicalize(p).ok()?;
        self.by_uri.get(super::scan::path_to_uri(&c).as_str()).copied()
    }
    /// Is `uri` a jar file already opened (promoted)?
    pub fn promoted_uri(&self, uri: &str) -> bool {
        self.by_uri.get(uri).map_or(false, |id| self.promoted.contains_key(id))
    }
    pub fn file(&self, id: FileId) -> Option<&Arc<FileEntry>> {
        if id >= EXT_BASE {
            if let Some(p) = self.promoted.get(&id) {
                return Some(p);
            }
            return self.jars.as_ref()?.files.get((id - EXT_BASE) as usize);
        }
        self.files.get(id as usize).and_then(|f| f.as_ref())
    }
    /// Entry stored under exactly `uri` (no symlink canonicalization).
    pub fn get_exact(&self, uri: &str) -> Option<&Arc<FileEntry>> {
        self.file(*self.by_uri.get(uri)?)
    }
    pub fn get(&self, uri: &str) -> Option<&Arc<FileEntry>> {
        self.file(self.id(uri)?)
    }
    /// clojure-lsp `external-filename?` (negated): false for a file outside every known source path (an open doc there
    /// is not internal: its usages and vars stay out of the project-wide unused-public-var scope).
    pub fn in_source_paths(&self, uri: &str) -> bool {
        let Some(pr) = self.project.as_ref() else { return true };
        if pr.source_paths.is_empty() || uri.ends_with(".lsp/config.edn") {
            return true;
        }
        let Some(path) = super::scan::uri_to_path(uri) else { return true };
        let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let (pc, rc) = (canon(&path), canon(&pr.root));
        pr.source_paths.iter().any(|sp| {
            let spp = std::path::Path::new(sp);
            let abs = if spp.is_absolute() { spp.to_path_buf() } else { pr.root.join(spp) };
            let absc = if spp.is_absolute() { canon(spp) } else { rc.join(spp) };
            path.starts_with(&abs) || pc.starts_with(&absc) || pc.starts_with(canon(&abs))
        })
    }
    pub fn uris(&self) -> impl Iterator<Item = &Arc<str>> {
        self.by_uri.keys()
    }
    /// Files defining namespace `ns` (project files first, then the dependency file).
    pub fn ns_files_of(&self, ns: SymId) -> Vec<FileId> {
        let mut v = self.ns_files.get(&ns.0).cloned().unwrap_or_default();
        if let Some(j) = &self.jars {
            v.extend(j.ns_files(ns).iter().copied());
        }
        v
    }
    /// clojure-lsp `reference-uris`: project files that use namespaces defined in `uri` (dependents) or that define
    /// namespaces `uri` uses (dependencies), excluding `uri` itself.
    pub fn reference_uris(&self, uri: &str) -> Vec<String> {
        let Some(id) = self.id(uri) else { return Vec::new() };
        let Some(fa) = self.file(id).and_then(|e| e.fa()) else { return Vec::new() };
        let mut ids: Vec<FileId> = Vec::new();
        for n in &fa.namespace_definitions {
            if let Some(ds) = self.ns_deps.get(&n.name.0) {
                ids.extend(ds.iter().map(|(_, f)| *f));
            }
        }
        for u in &fa.namespace_usages {
            ids.extend(self.ns_files.get(&u.to.0).cloned().unwrap_or_default());
        }
        let mut out: Vec<String> = Vec::new();
        for f in ids {
            if f == id || f >= EXT_BASE {
                continue;
            }
            if let Some(e) = self.file(f).filter(|e| e.internal) {
                if !out.iter().any(|u| **u == *e.uri) {
                    out.push(e.uri.to_string());
                }
            }
        }
        out
    }
    /// dep-graph `ns-and-dependents-uris` as file ids (deduped, ns files first).
    pub fn ns_and_dependents(&self, ns: SymId) -> Vec<FileId> {
        let mut out: Vec<FileId> = self.ns_files_of(ns);
        let mut seen: HashSet<u32> = HashSet::new();
        if let Some(ds) = self.ns_deps.get(&ns.0) {
            for (from, fid) in ds {
                // the dependent file itself (ns-less script files define no ns, so `ns_files_of(from)` misses them)
                if !out.contains(fid) {
                    out.push(*fid);
                }
                if seen.insert(*from) {
                    for f in self.ns_files_of(SymId(*from)) {
                        if !out.contains(&f) {
                            out.push(f);
                        }
                    }
                }
            }
        }
        out
    }
}

/// One accepted entry of a commit.
#[derive(Clone, Debug)]
pub struct Changed {
    pub uri: Arc<str>,
    pub version: i64,
    pub hash: u64,
    /// Disk read replacing the entry (watched file, revert after didClose), not a background pass or an open doc.
    pub disk: bool,
}

pub struct Commit {
    pub snapshot: Arc<Snapshot>,
    pub changed: Vec<Changed>,
    /// Results that were dropped as stale.
    pub dropped: usize,
}

/// Writer-only state.
struct Writer {
    /// jar layer + project layer (used by `finish_usages`).
    defs: DefsIndex,
    /// Files whose usage targets are not filled yet (bulk project pass).
    unfinished: HashSet<FileId>,
    /// Files with `:refer :all` usages (their targets depend on other namespaces' definitions).
    refer_all: HashSet<FileId>,
    /// Classpath-directory files (external sources outside jars): ids, for replacement.
    dir_files: Vec<FileId>,
    /// Background-batch results held (unpublished) until their batch completes: the settle then finishes them in place.
    pending: Vec<Done>,
}

#[derive(Clone)]
pub struct Store {
    cur: Arc<ArcSwap<Snapshot>>,
    ctx: SharedCtx,
    w: Arc<Mutex<Writer>>,
}

impl Default for Store {
    fn default() -> Self {
        Store::new()
    }
}

fn src_of(base: BaseLang, lang: u8) -> Src {
    match (base, lang) {
        (BaseLang::Clj, _) => Src::Clj,
        (BaseLang::Cljs, _) => Src::Cljs,
        (BaseLang::Cljc, L_CLJS) => Src::CljcCljs,
        (BaseLang::Cljc, _) => Src::CljcClj,
    }
}

fn lang_bits(uri: &str) -> (bool, bool) {
    match Lang::from_path(uri) {
        Lang::Cljs => (false, true),
        Lang::Cljc => (true, true),
        _ => (true, false),
    }
}

/// Hash of everything other files see of this file's definitions.
fn def_sig(fa: Option<&Arc<FileAnalysis>>) -> u64 {
    let Some(fa) = fa else { return 0 };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for n in &fa.namespace_definitions {
        (n.name.0, n.lang).hash(&mut h);
    }
    for d in &fa.var_definitions {
        (d.ns.0, d.name.0, d.lang, d.macro_, d.private, d.has_fixed, d.declared, d.fixed.0, d.varargs_min).hash(&mut h);
    }
    h.finish()
}

fn def_nses(fa: Option<&Arc<FileAnalysis>>, out: &mut HashSet<SymId>) {
    if let Some(fa) = fa {
        out.extend(fa.namespace_definitions.iter().map(|n| n.name));
        out.extend(fa.var_definitions.iter().map(|d| d.ns));
    }
}

/// Static (finish-independent) index contributions of one file.
fn index_static(s: &mut Snapshot, id: FileId, e: &FileEntry, add: bool) {
    let Some(fa) = &e.analysis else { return };
    let (clj, cljs) = lang_bits(&e.uri);
    let mut seen: HashSet<u32> = HashSet::new();
    let mut ns_defs: Vec<u32> = Vec::new();
    for n in &fa.namespace_definitions {
        if seen.insert(n.name.0) {
            ns_defs.push(n.name.0);
        }
    }
    // clojure-lsp dep-graph: a file defining vars in `user` without an ns form implicitly defines ns `user`
    let user = analyzer::syms().user;
    if !fa.var_definitions.is_empty() && fa.var_definitions.iter().any(|d| d.ns == user) && seen.insert(user.0) {
        ns_defs.push(user.0);
    }
    let ext = id >= EXT_BASE; // promoted jar file: the jar layer already indexes its ns / defs
    for ns in ns_defs {
        if ext {
            // fall through to the dependency registration only
        } else if add {
            s.ns_files.add(ns, id);
        } else {
            s.ns_files.del(ns, &id);
        }
        // implicit usage of clojure.core / cljs.core by every namespace
        for (on, core) in [(clj, analyzer::syms().clojure_core), (cljs, analyzer::syms().cljs_core)] {
            if on {
                if add {
                    s.ns_deps.add(core.0, (ns, id));
                } else {
                    s.ns_deps.del(core.0, &(ns, id));
                }
            }
        }
    }
    let mut pairs: HashSet<(u32, u32)> = HashSet::new();
    for u in &fa.namespace_usages {
        if pairs.insert((u.from.0, u.to.0)) {
            if add {
                s.ns_deps.add(u.to.0, (u.from.0, id));
            } else {
                s.ns_deps.del(u.to.0, &(u.from.0, id));
            }
        }
    }
    if e.internal {
        let mut kk: HashSet<Key2> = HashSet::new();
        let mut kn: HashSet<u32> = HashSet::new();
        for k in &fa.keywords {
            if k.reg.is_none() && !fa.has_callstack {
                continue;
            }
            if kk.insert((k.ns.0, k.name.0)) {
                if add {
                    s.kws.add((k.ns.0, k.name.0), id);
                } else {
                    s.kws.del((k.ns.0, k.name.0), &id);
                }
            }
            if kn.insert(k.ns.0) {
                if add {
                    s.kw_ns.add(k.ns.0, id);
                } else {
                    s.kw_ns.del(k.ns.0, &id);
                }
            }
        }
    }
    let mut dk: HashSet<Key2> = HashSet::new();
    for d in &fa.var_definitions {
        if !ext && !d.name.is_none() && dk.insert((d.ns.0, d.name.0)) {
            if add {
                s.defs.add((d.ns.0, d.name.0), id);
            } else {
                s.defs.del((d.ns.0, d.name.0), &id);
            }
        }
    }
}

/// Usage-target contributions (after `finish_usages`).
fn index_uses(s: &mut Snapshot, id: FileId, e: &FileEntry, add: bool) {
    let mut last: Option<(u32, u32)> = None;
    for &(to, name, _) in e.tgt.iter() {
        if last == Some((to, name)) {
            continue;
        }
        last = Some((to, name));
        if add {
            s.uses.add((to, name), id);
        } else {
            s.uses.del((to, name), &id);
        }
    }
}

impl Store {
    pub fn new() -> Store {
        Store {
            cur: Arc::new(ArcSwap::from_pointee(Snapshot::empty())),
            ctx: Arc::new(Ctx::new()),
            w: Arc::new(Mutex::new(Writer { defs: DefsIndex::new(), unfinished: HashSet::new(), refer_all: HashSet::new(), dir_files: Vec::new(), pending: Vec::new() })),
        }
    }

    pub fn ctx(&self) -> &SharedCtx {
        &self.ctx
    }

    /// Lock-free read.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.cur.load_full()
    }

    pub fn set_project(&self, p: Arc<ProjectInfo>) {
        let _w = self.w.lock().unwrap(); // read-modify-write of the snapshot: serialize with commits (B12 lost update)
        let mut s = (*self.snapshot()).clone();
        s.version += 1;
        s.project = Some(p);
        self.cur.store(Arc::new(s));
    }

    /// Open a jar entry like the JVM `force-get-document-text` does: full single-file analysis (usages, locals) that
    /// replaces the shallow stub. Returns false when `uri` is not an unopened jar file.
    pub fn promote_external(&self, uri: &str, text: String) -> bool {
        let w = self.w.lock().unwrap();
        let old = self.snapshot();
        let Some(id) = old.id(uri) else { return false };
        if id < EXT_BASE || old.promoted.contains_key(&id) {
            return false;
        }
        let lang = Lang::from_path(uri);
        let r = super::analyze::analyze_file_ctx(uri, text, lang, true, &self.ctx);
        let Some(mut fa) = r.analysis else { return false };
        analyzer::finish_usages(&mut fa, &w.defs);
        let tgt = index::build_targets(&fa);
        let e = Arc::new(FileEntry {
            uri: uri.into(),
            version: -1,
            lang,
            hash: r.hash,
            findings: Vec::new(),
            text: r.text,
            analysis: Some(Arc::new(fa)),
            pos: r.pos.map(Arc::new),
            tgt: Arc::new(tgt),
            internal: false,
            lazy: None,
            lazy_pos: Default::default(),
        });
        let mut s = (*old).clone();
        let mut m = (*s.promoted).clone();
        m.insert(id, e.clone());
        s.promoted = Arc::new(m);
        index_static(&mut s, id, &e, true);
        s.version += 1;
        self.cur.store(Arc::new(s));
        true
    }

    pub fn set_opts(&self, o: ClientOpts) {
        self.ctx.opts.store(Arc::new(o.clone()));
        let _w = self.w.lock().unwrap();
        let mut s = (*self.snapshot()).clone();
        s.version += 1;
        s.opts = Arc::new(o);
        self.cur.store(Arc::new(s));
    }

    /// JarLayer seam: install the dependency layer (native jar cache). Feeds the pass-1 defs for workers and the
    /// writer's `DefsIndex`, registers one lazy file stub per jar source, re-finishes project usages.
    pub fn set_jars(&self, layer: crate::jars::JarLayer, jar_scheme: bool) {
        let mut w = self.w.lock().unwrap();
        let old = self.snapshot();
        let mut s = (*old).clone();
        if let Some(prev) = &s.jars {
            for f in &prev.files {
                s.by_uri.remove(&f.uri);
            }
            s.file_count -= prev.files.len();
        }
        let view = Arc::new(JarView::new(layer, jar_scheme));
        for (i, f) in view.files.iter().enumerate() {
            s.by_uri.insert(f.uri.clone(), EXT_BASE + i as u32);
        }
        s.file_count += view.files.len();
        s.jars = Some(view);
        let a = Self::external_defs(&s, &w);
        let b = a.share_external();
        self.ctx.defs.store(Arc::new(a));
        w.defs = b;
        Self::rebuild_project_defs(&mut w, &s);
        let unfinished: Vec<FileId> = (0..s.files.len() as FileId).filter(|id| s.file(*id).map_or(false, |e| e.internal && e.analysis.is_some())).collect();
        Self::finish_files(&mut w, &mut s, &unfinished);
        s.version += 1;
        self.cur.store(Arc::new(s));
    }

    /// Fresh dependency-layer defs: jar layer + classpath-dir files.
    fn external_defs(s: &Snapshot, w: &Writer) -> DefsIndex {
        let mut d = DefsIndex::new();
        if let Some(j) = &s.jars {
            j.layer.feed(&mut d);
        }
        if let Some(m) = &s.mova {
            d.set_auto_ns(&m.auto_ns);
        }
        for &id in &w.dir_files {
            if let Some(fa) = s.file(id).and_then(|e| e.fa()) {
                let base = fa.base_lang.unwrap_or(analyzer::BaseLang::Clj);
                let uri = &s.file(id).unwrap().uri;
                let stem = uri.rsplit_once('.').map(|x| x.0).unwrap_or(uri);
                for n in &fa.namespace_definitions {
                    d.add_jar_ns(src_of(base, n.lang), n.name);
                }
                for v in &fa.var_definitions {
                    let mut f = 0u8;
                    if v.macro_ {
                        f |= analyzer::defs::F_MACRO;
                    }
                    if v.private {
                        f |= analyzer::defs::F_PRIVATE;
                    }
                    if v.has_fixed {
                        f |= analyzer::defs::F_FIXED;
                    }
                    if v.declared {
                        f |= analyzer::defs::F_DECLARED;
                    }
                    let info = analyzer::VarInfo { flags: f, varargs_min: v.varargs_min, fixed: v.fixed, deprecated: v.deprecated };
                    let primary = stem.ends_with(&v.ns.as_str().replace('.', "/").replace('-', "_"));
                    d.add_jar_def(src_of(base, v.lang), v.ns, v.name, info, primary, v.declared);
                }
            }
        }
        d
    }

    /// Install classpath-directory sources (git deps, local roots) as external files; replaces the previous set.
    pub fn set_dir_files(&self, files: Vec<(String, Lang, FileAnalysis)>, mova: Option<MovaMeta>) {
        let mut w = self.w.lock().unwrap();
        let old = self.snapshot();
        let mut s = (*old).clone();
        for id in std::mem::take(&mut w.dir_files) {
            if let Some(e) = s.file(id).cloned() {
                index_static(&mut s, id, &e, false);
                s.by_uri.remove(&e.uri);
                Arc::make_mut(&mut s.files)[id as usize] = None;
                s.file_count -= 1;
            }
        }
        for (uri, lang, fa) in files {
            if s.by_uri.get(uri.as_str()).is_some() {
                continue; // a project file (or open doc) of the same path wins
            }
            let pos = index::build_pos(&fa);
            let e = Arc::new(FileEntry {
                uri: uri.as_str().into(),
                version: -1,
                lang,
                hash: 0,
                findings: Vec::new(),
                text: None,
                analysis: Some(Arc::new(fa)),
                pos: Some(Arc::new(pos)),
                lazy_pos: Default::default(),
                tgt: Arc::new(Vec::new()),
                internal: false,
                lazy: None,
            });
            Arc::make_mut(&mut s.files).push(None);
            s.file_count += 1;
            let id = (s.files.len() - 1) as FileId;
            s.by_uri.insert(e.uri.clone(), id);
            Arc::make_mut(&mut s.files)[id as usize] = Some(e.clone());
            index_static(&mut s, id, &e, true);
            w.dir_files.push(id);
        }
        s.mova = mova.map(|m| {
            let rank = m.rank.iter().filter_map(|(u, r)| s.by_uri.get(u.as_str()).map(|id| (*id, *r))).filter(|(id, _)| w.dir_files.contains(id)).collect();
            Arc::new(MovaView { rank, auto_ns: m.auto_ns, root: m.root })
        });
        let a = Self::external_defs(&s, &w);
        let b = a.share_external();
        self.ctx.defs.store(Arc::new(a));
        w.defs = b;
        Self::rebuild_project_defs(&mut w, &s);
        let unfinished: Vec<FileId> = (0..s.files.len() as FileId).filter(|id| s.file(*id).map_or(false, |e| e.internal && e.analysis.is_some())).collect();
        Self::finish_files(&mut w, &mut s, &unfinished);
        s.version += 1;
        self.cur.store(Arc::new(s));
    }

    fn rebuild_project_defs(w: &mut Writer, s: &Snapshot) {
        w.defs.clear_project();
        let mut ids: Vec<(&Arc<str>, FileId)> = s.by_uri.iter().map(|(u, id)| (u, *id)).collect();
        ids.sort();
        for (_, id) in ids {
            if let Some(e) = s.file(id) {
                if e.internal {
                    if let Some(fa) = &e.analysis {
                        w.defs.add_file_at(fa, Some(&e.uri));
                    }
                }
            }
        }
    }

    /// Fill usage targets of `ids`, rebuild their target tables and `uses` index. In place when the entry is unshared
    /// (fresh from a worker: no pass-1 copy, no old-analysis garbage); clone-on-write otherwise (readers hold the old one).
    fn finish_files(w: &mut Writer, s: &mut Snapshot, ids: &[FileId]) {
        for &id in ids {
            let Some(mut slot) = Arc::make_mut(&mut s.files).get_mut(id as usize).and_then(|x| x.take()) else { continue };
            if slot.analysis.is_none() {
                Arc::make_mut(&mut s.files)[id as usize] = Some(slot);
                continue;
            }
            index_uses(s, id, &slot, false);
            let unique = Arc::get_mut(&mut slot).map_or(false, |e| e.analysis.as_mut().map_or(false, |a| Arc::get_mut(a).is_some()));
            if !unique {
                let mut e = (*slot).clone();
                let fa: FileAnalysis = (**e.analysis.as_ref().unwrap()).clone();
                e.analysis = Some(Arc::new(fa));
                slot = Arc::new(e);
            }
            let e = Arc::get_mut(&mut slot).unwrap();
            let fa = Arc::get_mut(e.analysis.as_mut().unwrap()).unwrap();
            fa.keep();
            analyzer::finish_usages(fa, &w.defs);
            analyzer::shrink_vec(&mut fa.findings);
            let mut tgt = index::build_targets(fa);
            crate::analyzer::shrink_vec(&mut tgt);
            if fa.refer_alls.is_empty() {
                w.refer_all.remove(&id);
            } else {
                w.refer_all.insert(id);
            }
            if e.internal && crate::analyzer::lint::lint_enabled(true) {
                e.findings = super::analyze::kondo_findings(fa);
            }
            e.tgt = Arc::new(tgt);
            if let Some(p) = e.pos.as_mut().and_then(Arc::get_mut) {
                p.keep();
            }
            if let Some(t) = e.text.as_ref().filter(|t| Arc::strong_count(t) == 1) {
                e.text = Some(Arc::from(&**t)); // writer-owned copy, like the buckets
            }
            index_uses(s, id, &slot, true);
            Arc::make_mut(&mut s.files)[id as usize] = Some(slot);
            w.unfinished.remove(&id);
        }
    }

    /// Files whose usage targets may change when definitions of `touched` namespaces change.
    fn dependents_of(w: &Writer, s: &Snapshot, touched: &HashSet<SymId>) -> Vec<FileId> {
        let mut out: HashSet<FileId> = HashSet::new();
        for (k, files) in s.uses.iter() {
            if touched.contains(&SymId(k.0)) {
                out.extend(files.iter().copied());
            }
        }
        for &id in &w.refer_all {
            out.insert(id);
        }
        let mut v: Vec<FileId> = out.into_iter().filter(|id| s.file(*id).map_or(false, |e| e.internal)).collect();
        v.sort();
        v
    }

    /// A project file was deleted on disk: drop its entry, re-finish the files that used its definitions.
    /// Returns false when `uri` is unknown or an open document.
    pub fn remove_file(&self, uri: &str) -> bool {
        let mut w = self.w.lock().unwrap();
        let old = self.snapshot();
        let Some(id) = old.id(uri) else { return false };
        let Some(cur) = old.file(id).cloned() else { return false };
        if id >= EXT_BASE || cur.version >= 0 || !cur.internal {
            return false;
        }
        let mut s = (*old).clone();
        index_static(&mut s, id, &cur, false);
        index_uses(&mut s, id, &cur, false);
        let mut touched: HashSet<SymId> = HashSet::new();
        def_nses(cur.analysis.as_ref(), &mut touched);
        s.by_uri.remove(&cur.uri);
        Arc::make_mut(&mut s.files)[id as usize] = None;
        s.file_count -= 1;
        w.unfinished.remove(&id);
        w.refer_all.remove(&id);
        Self::rebuild_project_defs(&mut w, &s);
        let todo = Self::dependents_of(&w, &s, &touched);
        Self::finish_files(&mut w, &mut s, &todo);
        s.version += 1;
        self.cur.store(Arc::new(s));
        true
    }

    /// Single writer. Drops stale: older version than stored; disk read (-1) over an open doc (unless override).
    /// `complete`: background batch ids that finished with these results (their deferred files get settled now).
    pub fn commit_batches(&self, done: &[Done], complete: &[u64]) -> Commit {
        let v: Vec<Done> = done.iter().map(|d| Done { entry: d.entry.clone(), batch: d.batch, disk_override: d.disk_override, idx: d.idx }).collect();
        self.commit_owned(v, complete)
    }

    /// Like `commit_batches`, but consumes the results: fresh entries stay unshared so `finish_files` mutates them in place.
    pub fn commit_owned(&self, done: Vec<Done>, complete: &[u64]) -> Commit {
        let mut w = self.w.lock().unwrap();
        let old = self.snapshot();
        let mut now: Vec<Done> = Vec::with_capacity(done.len());
        for d in done {
            if d.batch != 0 && !complete.contains(&d.batch) {
                w.pending.push(d);
            } else {
                now.push(d);
            }
        }
        if !complete.is_empty() && !w.pending.is_empty() {
            let (mut fl, keep): (Vec<Done>, Vec<Done>) = std::mem::take(&mut w.pending).into_iter().partition(|d| complete.contains(&d.batch));
            w.pending = keep;
            fl.extend(now);
            now = fl;
        }
        let done = now;
        if done.is_empty() {
            return Commit { snapshot: old, changed: Vec::new(), dropped: 0 };
        }
        let mut s = (*old).clone();
        let (mut changed, mut dropped) = (Vec::new(), 0);
        let mut touched: HashSet<SymId> = HashSet::new();
        let mut rebuild = false;
        let mut grown_any = false;
        let mut fresh: Vec<FileId> = Vec::new();
        let mut settle = false;
        for d in done {
            if d.batch != 0 && complete.contains(&d.batch) {
                settle = true;
            }
            let Some(e) = d.entry else { continue };
            let (batch, disk_override) = (d.batch, d.disk_override);
            let cur_id = s.id(&e.uri);
            let cur = cur_id.and_then(|i| s.file(i)).cloned();
            let stale = match &cur {
                None => false,
                Some(_) if disk_override => false,
                Some(c) => e.version < c.version,
            };
            if stale {
                dropped += 1;
                continue;
            }
            let id = match cur_id {
                Some(i) => i,
                None => {
                    Arc::make_mut(&mut s.files).push(None);
                    s.file_count += 1;
                    let i = (s.files.len() - 1) as FileId;
                    s.by_uri.insert(e.uri.clone(), i);
                    i
                }
            };
            if let Some(c) = &cur {
                index_static(&mut s, id, c, false);
                index_uses(&mut s, id, c, false);
            }
            let (so, sn) = (def_sig(cur.as_ref().and_then(|c| c.analysis.as_ref())), def_sig(e.analysis.as_ref()));
            if so != sn {
                rebuild = true;
                def_nses(cur.as_ref().and_then(|c| c.analysis.as_ref()), &mut touched);
                def_nses(e.analysis.as_ref(), &mut touched);
            }
            // the used-namespace sets decide which jar namespaces resolve: a growth (set_jars swaps them for empty ones
            // while no project file is committed yet) must re-finish the files finished against the smaller sets
            if e.internal {
                if let Some(fa) = e.analysis.as_ref() {
                    let grown = w.defs.mark_file_used(fa);
                    if !grown.is_empty() {
                        rebuild = true;
                        grown_any = true;
                        touched.extend(grown);
                    }
                }
            }
            index_static(&mut s, id, &e, true);
            // a file being replaced has no valid targets until finished
            w.unfinished.insert(id);
            if batch == 0 || complete.contains(&batch) {
                fresh.push(id);
            }
            changed.push(Changed { uri: e.uri.clone(), version: e.version, hash: e.hash, disk: disk_override });
            Arc::make_mut(&mut s.files)[id as usize] = Some(e);
        }
        if changed.is_empty() && !settle {
            return Commit { snapshot: old, changed, dropped };
        }
        {
            if rebuild || settle {
                Self::rebuild_project_defs(&mut w, &s);
            }
            let mut todo: HashSet<FileId> = fresh.iter().copied().collect();
            if settle {
                todo.extend(w.unfinished.iter().copied());
            }
            if (rebuild && !settle) || grown_any {
                todo.extend(Self::dependents_of(&w, &s, &touched));
            }
            let mut v: Vec<FileId> = todo.into_iter().collect();
            v.sort();
            Self::finish_files(&mut w, &mut s, &v);
        }
        s.version += 1;
        let snap = Arc::new(s);
        self.cur.store(snap.clone());
        Commit { snapshot: snap, changed, dropped }
    }

    /// Commit with no batch completion info (single results, tests).
    pub fn commit(&self, done: &[Done]) -> Commit {
        let all: Vec<u64> = done.iter().map(|d| d.batch).filter(|b| *b != 0).collect();
        self.commit_batches(done, &all)
    }
}
