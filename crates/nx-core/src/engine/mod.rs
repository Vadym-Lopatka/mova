//! Analysis engine: store + worker pool + project scan + LSP diagnostic mapping (no Mova deps).
//!
//! # The seam (what the real analyzer replaces)
//! `analyze::analyze_file(uri, text, lang) -> FileResult`: today parse + syntax findings; tomorrow the full
//! analyzer fills `FileResult::analysis` (opaque `Arc<dyn Any>`) and more findings. Nothing else changes.
//!
//! # Flow of data
//! `Engine::analyze_text` (open/changed, high prio) and `Engine::analyze_project` (bg, low prio) queue jobs on
//! the long-lived `Pool`; workers send `Done` results; ONE writer calls `Engine::commit` which swaps a new
//! immutable `Snapshot` into the `Store`; readers take `Store::snapshot()` (lock-free `ArcSwap` load).
pub mod analyze;
pub mod ctx;
pub mod external;
pub mod index;
pub mod jarview;
pub mod lsp;
pub mod pool;
pub mod scan;
pub mod shmap;
pub mod store;
pub mod types;

pub use analyze::analyze_file;
pub use ctx::ClientOpts;
pub use pool::{Done, Pool};
pub use store::{Commit, Snapshot, Store};
pub use types::{FileEntry, FileResult, Finding, Lang, Level};

use std::path::Path;
use std::sync::Arc;

/// Allocator trim (mimalloc `mi_collect(true)` in the Mova binding): returns freed pages of the calling thread's heap
/// to the OS. Called by idle workers and after a background batch completes.
static TRIM: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

pub fn set_trim_hook(f: fn()) {
    let _ = TRIM.set(f);
}

pub(crate) fn trim() {
    if let Some(f) = TRIM.get() {
        f();
    }
}

/// Store + pool. One per process (the Mova binding keeps a singleton).
pub struct Engine {
    pub store: Store,
    pub pool: Pool,
}

impl Engine {
    /// `workers` = 0 means default (`NX_WORKERS` env, else cores-2, min 1).
    pub fn new(workers: usize) -> Arc<Engine> {
        let store = Store::new();
        let pool = Pool::new(workers, store.clone());
        Arc::new(Engine { store, pool })
    }

    /// Queue an open/changed document (high priority, coalesced by uri). Never blocks.
    pub fn analyze_text(&self, uri: &str, version: i64, text: String) {
        self.pool.submit_text(uri, version, text); // an open doc keeps the client's uri (JVM: documents are keyed by it)
    }

    /// A `.mova` document was opened or changed. When the project pass did not see it as Mova code (a Mova module
    /// inside a Clojure repository, a module dir below the top level), turn the Mova layer on and add the file's
    /// module root (from its `ns` form) to the source paths. Work runs on its own thread; never blocks.
    pub fn mova_open(self: &Arc<Engine>, uri: &str, text: &str) {
        if !uri.ends_with(".mova") {
            return;
        }
        let snap = self.store.snapshot();
        let Some(proj) = snap.project.clone() else { return };
        let Some(path) = scan::uri_to_path(uri) else { return };
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let under = proj.source_paths.iter().any(|sp| path.starts_with(sp));
        if under && proj.mova {
            return;
        }
        let root = std::fs::canonicalize(&proj.root).unwrap_or_else(|_| proj.root.clone());
        let new_root = if under { None } else { crate::mova::module_root(&path, text).filter(|r| *r != root && r.starts_with(&root)) };
        if proj.mova && new_root.is_none() {
            return;
        }
        let e = self.clone();
        std::thread::spawn(move || e.enable_mova(new_root));
    }

    fn enable_mova(&self, new_root: Option<std::path::PathBuf>) {
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = ONE.lock().unwrap_or_else(|p| p.into_inner());
        let Some(proj) = self.store.snapshot().project.clone() else { return };
        let mut info = (*proj).clone();
        let mut files = Vec::new();
        if let Some(r) = new_root {
            let s = r.to_string_lossy().into_owned();
            if !info.source_paths.contains(&s) {
                scan::walk(&r, &mut files, 0);
                info.source_paths.push(s);
                let mut all = (**self.store.ctx().source_paths.load()).clone();
                if !all.contains(&r) {
                    all.push(r);
                    self.store.ctx().source_paths.store(Arc::new(all));
                }
            }
        }
        let first = !info.mova;
        if !first && files.is_empty() {
            return; // an earlier open already did this
        }
        info.mova = true;
        let root = info.root.clone();
        self.store.set_project(Arc::new(info));
        if first {
            self.load_classpath_jars(&root); // installs the layer, re-analyzes open docs
        } else {
            self.reanalyze_open();
        }
        let open: std::collections::HashSet<String> = self.pool.open_docs().into_iter().map(|d| d.0).collect();
        files.retain(|f| !open.contains(&scan::path_to_uri(f)));
        if !files.is_empty() {
            self.pool.submit_paths(files, false, pool::Prio::Low);
        }
    }

    /// Queue a disk re-read of `uri` that replaces any open-doc entry (after didClose).
    pub fn analyze_disk_override(&self, uri: &str) {
        self.pool.forget_open(uri);
        if let Some(p) = scan::uri_to_path(uri) {
            self.pool.submit_paths(vec![p], true, pool::Prio::High);
        }
    }

    /// Discover project (source paths, no classpath) and queue every source file at low priority.
    /// Returns (batch id, total files, source paths). Does filesystem IO: call from an IO thread.
    pub fn analyze_project(&self, root: &Path) -> (u64, usize, Vec<String>) {
        use crate::met;
        // metrics (off: no clock read): the spans of this pass go into one native record (`met::Pass`)
        let (mut ps, mut lap) = (met::Pass { start: met::now(), ..Default::default() }, met::Lap::start());
        let mut info = scan::discover(root);
        ps.us[met::P_DISCOVER] = lap.lap();
        crate::jdk::set_root(root);
        self.store.ctx().cfg.store(Arc::new(crate::analyzer::Config::load(root)));
        ps.us[met::P_CONFIG] = lap.lap();
        self.reanalyze_open(); // docs opened before the config was known
        self.load_classpath_jars_met(root, &mut ps, info.mova, &info.source_paths);
        let files = std::mem::take(&mut info.files);
        let total = files.len();
        let source_paths = info.source_paths.clone();
        {
            // union of the resolved classpath dirs (stored by `load_classpath_jars`) and the static estimate
            let mut all: Vec<std::path::PathBuf> = (**self.store.ctx().source_paths.load()).clone();
            for sp in &source_paths {
                let c = std::fs::canonicalize(sp).unwrap_or_else(|_| std::path::PathBuf::from(sp));
                if !all.contains(&c) {
                    all.push(c);
                }
            }
            self.store.ctx().source_paths.store(Arc::new(all));
        }
        self.store.set_project(Arc::new(info));
        ps.queued = met::now();
        let batch = self.pool.submit_batch(files);
        lap.lap();
        if let Some(j) = &self.store.snapshot().jars {
            j.warm(); // first definition / ns-references / require completion must not build indexes on a compute shard
        }
        if ps.start.is_some() {
            ps.us[met::P_INDEX] += lap.lap();
            (ps.batch, ps.n[met::N_FILES]) = (batch, total as u64);
            met::pass_open(ps);
        }
        (batch, total, source_paths)
    }

    /// Classpath jars -> dependency layer (native jar cache, `src/jars`). Blocks (subprocess on a cold classpath cache).
    pub fn load_classpath_jars(&self, root: &Path) {
        let (mova, sps) = self.store.snapshot().project.as_ref().map(|p| (p.mova, p.source_paths.clone())).unwrap_or_default();
        self.load_classpath_jars_met(root, &mut crate::met::Pass::default(), mova, &sps);
    }

    /// `load_classpath_jars`, its spans and counters noted in `ps` (metrics on only).
    fn load_classpath_jars_met(&self, root: &Path, ps: &mut crate::met::Pass, mova: bool, mova_sps: &[String]) {
        use crate::met;
        let mut lap = met::Lap::start();
        let settings = crate::io::project::Settings::load(root);
        let r = crate::io::classpath::resolve(root, &settings);
        ps.us[met::P_CLASSPATH] = lap.lap();
        (ps.n[met::N_CP_CACHED], ps.n[met::N_CP_ERRORS]) = (r.from_cache as u64, r.errors.len() as u64);
        let cache = crate::io::cache_root().join("jars");
        let layer = crate::jars::analyze_classpath_with(&r.classpath, &cache, &self.store.ctx().cfg.load());
        ps.us[met::P_JARS] = lap.lap();
        let st = layer.stats;
        for (i, v) in [(met::N_JARS, st.jars), (met::N_JARS_WARM, st.warm), (met::N_JARS_COLD, st.cold), (met::N_JARS_FAILED, st.failed), (met::N_JAR_FILES, st.files), (met::N_JAR_DEFS, st.defs), (met::N_JAR_CACHE_B, st.cache_bytes)] {
            ps.n[i] = v as u64;
        }
        // the resolved classpath dirs under the root are the JVM's source paths (static estimate until now)
        if settings.source_paths.is_none() && r.errors.is_empty() {
            let sp = crate::io::project::source_paths(root, &settings, &r.classpath.dirs);
            if !sp.is_empty() {
                self.store.ctx().source_paths.store(Arc::new(sp.iter().map(std::path::PathBuf::from).collect()));
            }
        }
        let source_paths = self.store.snapshot().project.as_ref().map(|p| p.source_paths.clone()).unwrap_or_default();
        let dirs = external::external_dirs(root, &r.classpath.dirs, &source_paths);
        let mut dir_files = if dirs.is_empty() { Vec::new() } else { external::analyze_dirs(&dirs, &self.store.ctx().cfg.load()) };
        // Mova project: stdlib sources + Rust natives as external files
        let mut mova_meta = None;
        if mova {
            let skip: Vec<std::path::PathBuf> = mova_sps.iter().map(std::path::PathBuf::from).collect();
            if let Some(l) = crate::mova::build_layer(root, &skip, &self.store.ctx().cfg.load()) {
                self.store.ctx().init_ns.store(Arc::new(l.init_ns.iter().cloned().collect()));
                let mut rank = std::collections::HashMap::new();
                for f in l.files {
                    rank.insert(f.uri.clone(), f.rank);
                    dir_files.push((f.uri, f.lang, f.fa));
                }
                mova_meta = Some(store::MovaMeta { rank, auto_ns: l.auto_ns, root: l.root });
            }
        }
        ps.us[met::P_EXT_DIRS] = lap.lap();
        (ps.n[met::N_EXT_DIRS], ps.n[met::N_EXT_FILES]) = (dirs.len() as u64, dir_files.len() as u64);
        self.store.set_jars(layer, self.store.snapshot().opts.jar_scheme);
        if !dir_files.is_empty() {
            self.store.set_dir_files(dir_files, mova_meta);
        }
        ps.us[met::P_INDEX] = lap.lap();
        self.reanalyze_open();
    }

    /// `workspace/didChangeWatchedFiles` created/changed: queue a disk read of the source file(s) at `uri` (a directory
    /// is expanded). Files open in the editor, unknown file types and ignored paths are skipped. Returns the queued uris.
    /// Does filesystem IO.
    pub fn watch_changed(&self, uri: &str) -> Vec<String> {
        let Some(p) = scan::uri_to_path(uri) else { return Vec::new() };
        let mut files = Vec::new();
        if p.is_dir() {
            scan::walk(&p, &mut files, 0);
        } else if p.is_file() && scan::is_source(&p) {
            files.push(p);
        }
        let snap = self.store.snapshot();
        let ignore = snap.project.as_ref().map(|i| scan::ignored_regexes(&i.root)).unwrap_or_default();
        let root = snap.project.as_ref().map(|i| i.root.clone());
        let mut out = Vec::new();
        for f in files {
            let f = std::fs::canonicalize(&f).unwrap_or(f);
            if let Some(r) = &root {
                if let Ok(rel) = f.strip_prefix(r) {
                    if ignore.iter().any(|re| re.is_match(&rel.to_string_lossy())) {
                        continue;
                    }
                }
            }
            let u = scan::path_to_uri(&f);
            if snap.get(&u).map_or(false, |e| e.version >= 0) {
                continue; // open in the editor: the editor text wins (clojure-lsp)
            }
            self.pool.submit_paths(vec![f], true, pool::Prio::High);
            out.push(u);
        }
        out
    }

    /// `workspace/didChangeWatchedFiles` deleted: drop the file(s) from the store. Returns (removed uris, reference uris
    /// of the removed files computed before removal, minus the removed ones).
    pub fn watch_deleted(&self, uri: &str) -> (Vec<String>, Vec<String>) {
        let Some(p) = scan::uri_to_path(uri) else { return (Vec::new(), Vec::new()) };
        let p = std::fs::canonicalize(&p).unwrap_or(p); // a deleted file: canonical parent + name
        let mut base = scan::path_to_uri(&p);
        let snap = self.store.snapshot();
        if snap.get(&base).is_none() {
            if let (Some(par), Some(name)) = (p.parent(), p.file_name()) {
                if let Ok(cp) = std::fs::canonicalize(par) {
                    base = scan::path_to_uri(&cp.join(name));
                }
            }
        }
        let dir_prefix = format!("{base}/");
        let victims: Vec<String> = snap.uris().filter(|u| ***u == *base || u.starts_with(&dir_prefix)).map(|u| u.to_string()).collect();
        let mut refs: Vec<String> = Vec::new();
        for u in &victims {
            for r in snap.reference_uris(u) {
                if !refs.contains(&r) {
                    refs.push(r);
                }
            }
        }
        let removed: Vec<String> = victims.into_iter().filter(|u| self.store.remove_file(u)).collect();
        refs.retain(|r| !removed.contains(r));
        (removed, refs)
    }

    /// Re-queue open documents (their analysis may predate the external layer).
    pub fn reanalyze_open(&self) {
        for (uri, version, text) in self.pool.open_docs() {
            self.pool.submit_text(&uri, version, text.to_string());
        }
    }

    /// Promote an unopened jar entry to a fully analyzed file (hover / call hierarchy on it). Blocking; true when done now.
    pub fn open_external(&self, uri: &str) -> bool {
        if !types::is_external_uri(uri) || self.store.snapshot().promoted_uri(uri) {
            return false;
        }
        match crate::query::callh::uri_text(uri) {
            Some(t) => self.store.promote_external(uri, t),
            None => false,
        }
    }

    pub fn set_client_opts(&self, o: ClientOpts) {
        self.store.set_opts(o);
    }

    /// Blocking batch API: analyze `paths` in parallel on the pool, read natively. Results in input order.
    pub fn analyze_paths(&self, paths: Vec<std::path::PathBuf>) -> Vec<Option<Arc<FileEntry>>> {
        self.pool.analyze_paths(paths)
    }

    /// Next results (blocks for the first, then drains what is ready, up to `max`). None = engine closed.
    pub fn await_results(&self, max: usize) -> Option<Vec<Done>> {
        self.pool.recv_batch(max)
    }

    /// Single-writer commit of worker results into a new snapshot.
    pub fn commit(&self, done: &[Done]) -> Commit {
        self.commit_with(done.iter().map(|d| Done { entry: d.entry.clone(), batch: d.batch, disk_override: d.disk_override, idx: d.idx }).collect())
    }

    /// Consuming commit: worker entries stay unshared, so the store finishes them in place (no copy).
    pub fn commit_with(&self, done: Vec<Done>) -> Commit {
        let mut complete = Vec::new();
        for d in &done {
            if d.batch != 0 && !complete.contains(&d.batch) {
                if let Some((n, t)) = self.pool.batch_progress(d.batch) {
                    if n >= t {
                        complete.push(d.batch);
                    }
                }
            }
        }
        let c = self.store.commit_owned(done, &complete);
        if !complete.is_empty() {
            if crate::met::on() {
                complete.iter().for_each(|b| crate::met::pass_done(*b)); // the pass ends here: one stamp
            }
            trim(); // the project pass just finished: its transient CST / scratch memory is free now
            crate::jdk::start(); // background JDK index after the project pass
        }
        c
    }
}
