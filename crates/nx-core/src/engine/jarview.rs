//! The dependency layer inside a snapshot: a native `JarLayer` plus one lazily-decoded `FileEntry` stub per jar source.
use super::types::{FileEntry, Lang, LazyFile};
use crate::intern::SymId;
use crate::jars::JarLayer;
use std::sync::Arc;

/// File ids at or above this are jar files (`JarView::files[id - EXT_BASE]`).
pub const EXT_BASE: u32 = 0x4000_0000;

pub struct JarView {
    pub layer: Arc<JarLayer>,
    pub files: Vec<Arc<FileEntry>>,
    base: Vec<u32>,
    ns_memo: std::sync::Mutex<std::collections::HashMap<SymId, Arc<Vec<u32>>>>,
    ns_all: std::sync::OnceLock<Vec<SymId>>,
    alias_all: std::sync::OnceLock<Vec<(SymId, SymId)>>,
    class_all: std::sync::OnceLock<Vec<Box<str>>>,
    ns_users: std::sync::OnceLock<std::collections::HashMap<SymId, Vec<SymId>>>,
}

fn ns_stem(ns: SymId) -> String {
    ns.as_str().replace('.', "/").replace('-', "_")
}

/// Client-visible uri of a jar entry: `jar:file:///x/a.jar!/e` (dependency-scheme "jar") or `zipfile:///x/a.jar::e`.
pub fn jar_entry_uri(jar_path: &str, entry: &str, jar_scheme: bool) -> String {
    if jar_scheme {
        format!("jar:file://{jar_path}!/{entry}")
    } else {
        format!("zipfile://{jar_path}::{entry}")
    }
}

impl JarView {
    pub fn new(layer: JarLayer, jar_scheme: bool) -> JarView {
        let layer = Arc::new(layer);
        let mut files = Vec::new();
        let mut base = Vec::new();
        for (ji, j) in layer.jars.iter().enumerate() {
            base.push(files.len() as u32);
            let mut fi = 0;
            while let Some(name) = j.file_name(fi) {
                let uri = jar_entry_uri(&j.path, name, jar_scheme);
                files.push(Arc::new(FileEntry {
                    uri: uri.into(),
                    version: -1,
                    lang: Lang::from_path(name),
                    hash: 0,
                    findings: Vec::new(),
                    text: None,
                    analysis: None,
                    pos: None,
                    lazy_pos: Default::default(),
                    tgt: Arc::new(Vec::new()),
                    internal: false,
                    lazy: Some(Arc::new(LazyFile::new(layer.clone(), ji, fi))),
                }));
                fi += 1;
            }
        }
        JarView { layer, files, base, ns_memo: Default::default(), ns_all: Default::default(), class_all: Default::default(), alias_all: Default::default(), ns_users: Default::default() }
    }

    pub fn id(&self, jar: usize, file: usize) -> u32 {
        EXT_BASE + self.base[jar] + file as u32
    }
    /// Is file `f` of jar `j` the namespace's own file (not an `in-ns` continuation such as core_deftype.clj)?
    fn primary(&self, j: usize, f: usize, ns: SymId) -> bool {
        match self.layer.jars[j].file_name(f) {
            Some(n) => n.rsplit_once('.').map_or(n, |x| x.0).ends_with(&ns_stem(ns)),
            None => false,
        }
    }
    /// Build the lazy indexes now (call from a background thread so the first request never pays for them).
    pub fn warm(&self) {
        let _ = self.locate(SymId(0), SymId(0));
        let _ = self.layer.locate_multi(SymId(0), SymId(0));
        let _ = self.all_ns();
    }
    /// File id defining `ns/name` in the namespace's own file (first jar on the classpath wins).
    pub fn locate(&self, ns: SymId, name: SymId) -> Option<u32> {
        let (j, f) = self.layer.locate(ns, name)?;
        self.primary(j, f, ns).then(|| self.id(j, f))
    }
    /// Every file id defining `ns/name` (namespace's own file first) when several files define it.
    pub fn locate_all(&self, ns: SymId, name: SymId) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        if let Some(p) = self.locate(ns, name) {
            out.push(p);
        }
        for &(j, f) in self.layer.locate_multi(ns, name) {
            let id = self.id(j as usize, f as usize);
            // JVM: `in-ns` continuation files (core_deftype.clj) are not in the namespace's uri set
            if !out.contains(&id) && self.primary(j as usize, f as usize, ns) {
                out.push(id);
            }
        }
        out
    }
    /// Dependency files whose namespace uses `ns` (dep-graph dependents): the files DEFINING the using namespaces
    /// (an `in-ns` continuation file counts for the namespace it continues, not as a file of its own).
    pub fn ns_users(&self, ns: SymId) -> Vec<u32> {
        let m = self.ns_users.get_or_init(|| {
            let mut m: std::collections::HashMap<SymId, Vec<SymId>> = std::collections::HashMap::new();
            for j in &self.layer.jars {
                let mut fi = 0;
                while j.file_name(fi).is_some() {
                    if let Some(fa) = j.file(fi) {
                        for u in &fa.namespace_usages {
                            let v = m.entry(u.to).or_default();
                            if !v.contains(&u.from) {
                                v.push(u.from);
                            }
                        }
                    }
                    fi += 1;
                }
            }
            m
        });
        let mut out: Vec<u32> = Vec::new();
        for from in m.get(&ns).map(|v| v.as_slice()).unwrap_or(&[]) {
            for f in self.ns_files(*from).iter() {
                if !out.contains(f) {
                    out.push(*f);
                }
            }
        }
        out
    }
    /// `(alias, ns)` pairs used by dependency sources (deduped).
    pub fn aliases(&self) -> &[(SymId, SymId)] {
        self.alias_all.get_or_init(|| {
            let mut v: Vec<(SymId, SymId)> = self.layer.jars.iter().flat_map(|j| j.aliases()).collect();
            v.sort_by_key(|(a, t)| (a.as_str(), t.as_str()));
            v.dedup();
            v
        })
    }
    /// Java class names (`a.b.Outer$Inner`) of the dependency layer starting with `prefix` (sorted, deduped).
    pub fn classes_with_prefix(&self, prefix: &str) -> &[Box<str>] {
        let all = self.class_all.get_or_init(|| {
            let mut v: Vec<Box<str>> = self.layer.jars.iter().flat_map(|j| j.classes().into_iter().map(|c| c.class.into_boxed_str())).collect();
            v.sort();
            v.dedup();
            v
        });
        let lo = all.partition_point(|c| &**c < prefix);
        let hi = lo + all[lo..].partition_point(|c| c.starts_with(prefix));
        &all[lo..hi]
    }
    /// All namespace names defined in the dependency layer (sorted, deduped).
    pub fn all_ns(&self) -> &[SymId] {
        self.ns_all.get_or_init(|| {
            let mut v: Vec<SymId> = Vec::new();
            for j in &self.layer.jars {
                j.for_each_def(|r| {
                    if r.name.is_none() {
                        v.push(r.ns);
                    }
                });
            }
            v.sort_by_key(|s| s.as_str());
            v.dedup();
            v
        })
    }
    /// Files (clj / cljc / cljs variants of every jar having them) whose own namespace is `ns`.
    pub fn ns_files(&self, ns: SymId) -> Arc<Vec<u32>> {
        if let Some(v) = self.ns_memo.lock().unwrap().get(&ns) {
            return v.clone();
        }
        let mut out = Vec::new();
        let stem = ns_stem(ns);
        for ext in ["clj", "cljc", "cljs"] {
            let entry = format!("{stem}.{ext}");
            // every jar on the classpath (the JVM analyzes all of them: both versions of a ns are in its dep-graph `:uris`)
            for (ji, j) in self.layer.jars.iter().enumerate() {
                if let Some(fi) = j.find_file(&entry) {
                    out.push(self.id(ji, fi));
                }
            }
        }
        let v = Arc::new(out);
        self.ns_memo.lock().unwrap().insert(ns, v.clone());
        v
    }
}
