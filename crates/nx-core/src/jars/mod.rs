//! Native jar analysis + global per-jar cache.
//!
//! `analyze_classpath` analyzes every jar's Clojure sources in external (shallow, definitions-only) mode, collects
//! `java-class-definitions` from `.class` headers and `.java` sources, and stores one compact file per jar
//! (`<cache_dir>/<jar-key>.nxc`, jar-key = `io::jar::stat_key`). Warm runs mmap the files and decode only the
//! definition summary; per-file `FileAnalysis` (positions, docs, arglists) is decoded lazily.
//!
//! Bump `ANALYSIS_REV` whenever analyzer output or this encoding changes (stale caches are then ignored).
//!
//! Per-jar cache records (`io::cache`): `s` string table, `d` def summary (28 B records), `f` file table + blobs,
//! `c` class table, `v` header (written last = commit marker).
pub mod codec;
pub mod java;

use crate::analyzer::defs::{self, F_DECLARED};
use crate::analyzer::{analyze_file_opts, finish_extras, Arities, BaseLang, Config, DefsIndex, FileAnalysis, FileKind, Options, Src, Val, VarInfo};
use crate::intern::{intern, SymId};
use crate::io::cache::Cache;
use crate::io::classpath::Classpath;
use crate::io::jar::{self, EntryKind, Jar};
use codec::{decode_fa, encode_fa, StrTab, Strs, W};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Bump when analyzer output or the encoding changes.
pub const ANALYSIS_REV: u32 = 4;
const SCHEMA: u32 = 0x4a00_0000 | ANALYSIS_REV;
const REC: usize = 28;
const R_NS_ONLY: u8 = 0x40;
const R_PRIMARY: u8 = 0x80;

/// kondo `java-class-definitions` element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassDef {
    /// Binary name with dots (`a.b.Outer$Inner`).
    pub class: String,
    /// Entry path inside the jar.
    pub entry: String,
    /// `java::FL_*` bits.
    pub flags: u8,
}

/// One row of the def summary (what `DefsIndex` needs to resolve calls).
#[derive(Clone, Copy, Debug)]
pub struct DefRec {
    pub src: Src,
    pub ns: SymId,
    /// `SymId::NONE` = namespace record only.
    pub name: SymId,
    pub info: VarInfo,
    pub primary: bool,
    /// Index of the defining file in the jar's file table.
    pub file: u32,
}

fn src_from(b: u8) -> Src {
    match b {
        0 => Src::Clj,
        1 => Src::Cljs,
        2 => Src::CljcClj,
        _ => Src::CljcCljs,
    }
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// One analyzed jar backed by its cache file.
pub struct JarData {
    pub path: String,
    pub key: String,
    /// Loaded from an existing cache file (no analysis).
    pub warm: bool,
    // The slices point into `cache`'s mmap (heap-stable); declared before it so they drop first.
    strs_blob: &'static [u8],
    defs_blob: &'static [u8],
    files_blob: &'static [u8],
    classes_blob: &'static [u8],
    aliases_blob: &'static [u8],
    strs: OnceLock<Strs<'static>>,
    pub file_count: usize,
    pub def_count: usize,
    pub class_count: usize,
    /// Size of the cache file in bytes.
    pub cache_bytes: usize,
    _cache: Cache,
}

impl JarData {
    fn strs(&self) -> &Strs<'static> {
        self.strs.get_or_init(|| Strs::parse(self.strs_blob).expect("validated at open"))
    }

    /// Entry path of file `i`.
    pub fn file_name(&self, i: usize) -> Option<&str> {
        if i >= self.file_count {
            return None;
        }
        let ent = self.files_blob.get(4 + i * 12..4 + i * 12 + 12)?;
        self.strs().str_at(le32(ent, 0) as usize)
    }

    /// Index of the file with this entry path.
    pub fn find_file(&self, entry: &str) -> Option<usize> {
        (0..self.file_count).find(|&i| self.file_name(i) == Some(entry))
    }

    /// Decode the full analysis of file `i` (var-definitions with positions, docs, arglists, ns definitions...).
    pub fn file(&self, i: usize) -> Option<FileAnalysis> {
        let ent = self.files_blob.get(4 + i * 12..4 + i * 12 + 12)?;
        let (off, len) = (le32(ent, 4) as usize, le32(ent, 8) as usize);
        decode_fa(self.files_blob.get(off..off + len)?, self.strs())
    }

    /// `jar:file:<path>!/<entry>` uri of a jar entry.
    pub fn entry_uri(&self, entry: &str) -> String {
        format!("jar:file:{}!/{}", self.path, entry)
    }

    /// Def summary rows.
    pub fn for_each_def(&self, mut f: impl FnMut(&DefRec)) {
        let t = self.strs();
        for r in self.defs_blob.chunks_exact(REC) {
            let flags = r[1];
            let ns = t.sym(le32(r, 12) as u64).unwrap_or(SymId::NONE);
            let name = if flags & R_NS_ONLY != 0 { SymId::NONE } else { t.sym(le32(r, 16) as u64).unwrap_or(SymId::NONE) };
            let info = VarInfo { flags: flags & 0x1f, varargs_min: u16::from_le_bytes([r[2], r[3]]), fixed: Arities(u64::from_le_bytes(r[4..12].try_into().unwrap())), deprecated: Val(t.sym(le32(r, 20) as u64).unwrap_or(SymId::NONE)) };
            f(&DefRec { src: src_from(r[0]), ns, name, info, primary: flags & R_PRIMARY != 0, file: le32(r, 24) });
        }
    }

    /// `(alias, ns)` pairs of the namespace usages in this jar's sources (clojure-lsp dep-graph aliases, deduped).
    pub fn aliases(&self) -> Vec<(SymId, SymId)> {
        let t = self.strs();
        let b = self.aliases_blob;
        if b.len() < 4 {
            return Vec::new();
        }
        b[4..].chunks_exact(8).filter_map(|r| Some((t.sym(le32(r, 4) as u64)?, t.sym(le32(r, 0) as u64)?))).collect()
    }

    /// Entry path of class `name` (dots) in this jar's `java-class-definitions`, scanned without allocating.
    pub fn find_class_entry(&self, name: &str) -> Option<&str> {
        let t = self.strs();
        self.classes_blob[4..].chunks_exact(12).find(|r| t.str_at(le32(r, 0) as usize) == Some(name)).and_then(|r| t.str_at(le32(r, 4) as usize))
    }

    /// `java-class-definitions` of this jar.
    pub fn classes(&self) -> Vec<ClassDef> {
        let t = self.strs();
        self.classes_blob[4..]
            .chunks_exact(12)
            .filter_map(|r| Some(ClassDef { class: t.str_at(le32(r, 0) as usize)?.to_string(), entry: t.str_at(le32(r, 4) as usize)?.to_string(), flags: r[8] }))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub jars: usize,
    pub warm: usize,
    pub cold: usize,
    pub failed: usize,
    pub files: usize,
    pub defs: usize,
    pub classes: usize,
    pub cache_bytes: usize,
}

/// The analyzed dependency layer of a classpath (jars in classpath order).
pub struct JarLayer {
    pub jars: Vec<JarData>,
    pub stats: Stats,
    loc: OnceLock<HashMap<(SymId, SymId), (u32, u32)>>,
    /// `(ns, name)` defined in more than one file (macro file + runtime file, in-ns continuations): every `(jar, file)`.
    multi: OnceLock<HashMap<(SymId, SymId), Vec<(u32, u32)>>>,
}

impl JarLayer {
    /// `(jar path, entry)` of the first jar on the classpath defining class `name`.
    pub fn find_class(&self, name: &str) -> Option<(&str, &str)> {
        self.jars.iter().find_map(|j| j.find_class_entry(name).map(|e| (j.path.as_str(), e)))
    }

    /// Feed the jar layer of `defs`. Jars are applied last-to-first so the first jar on the classpath wins.
    pub fn feed(&self, defs: &mut DefsIndex) {
        for j in self.jars.iter().rev() {
            j.for_each_def(|r| {
                if r.name.is_none() {
                    defs.add_jar_ns(r.src, r.ns);
                } else {
                    defs.add_jar_def(r.src, r.ns, r.name, r.info, r.primary, r.info.flags & F_DECLARED != 0);
                }
            });
        }
    }

    /// Decode file `file` of jar `jar`.
    pub fn file(&self, jar: usize, file: usize) -> Option<FileAnalysis> {
        self.jars.get(jar)?.file(file)
    }

    /// Where is var `ns/name` defined: `(jar index, file index)` (first jar on the classpath wins). Builds an index on first use.
    pub fn locate(&self, ns: SymId, name: SymId) -> Option<(usize, usize)> {
        let m = self.loc.get_or_init(|| {
            let mut m = HashMap::new();
            for (ji, j) in self.jars.iter().enumerate().rev() {
                // Several rows per var (clj/cljs/cljc tables, sibling files): row order is arbitrary, so pick
                // deterministically within the jar: primary file first, then the lexically first file name
                // (`core.cljc` before `core.cljs`, as the JVM's sorted file walk).
                let mut own: HashMap<(SymId, SymId), (u32, bool)> = HashMap::new();
                j.for_each_def(|r| {
                    let k = (r.ns, r.name);
                    let better = match own.get(&k) {
                        None => true,
                        Some(&(pf, pp)) if pf != r.file || pp != r.primary => {
                            (r.primary, std::cmp::Reverse(j.file_name(r.file as usize))) > (pp, std::cmp::Reverse(j.file_name(pf as usize)))
                        }
                        _ => false,
                    };
                    if better {
                        own.insert(k, (r.file, r.primary));
                        m.insert(k, (ji as u32, r.file));
                    }
                });
            }
            m
        });
        m.get(&(ns, name)).map(|&(j, f)| (j as usize, f as usize))
    }

    /// Every `(jar, file)` defining `ns/name` when it is defined in several files (empty otherwise).
    pub fn locate_multi(&self, ns: SymId, name: SymId) -> &[(u32, u32)] {
        let m = self.multi.get_or_init(|| {
            let _ = self.locate(SymId(0), SymId(0)); // builds `loc`
            let loc = self.loc.get().expect("loc built");
            let mut all: HashMap<(SymId, SymId), Vec<(u32, u32)>> = HashMap::new();
            for (ji, j) in self.jars.iter().enumerate() {
                j.for_each_def(|r| {
                    let here = (ji as u32, r.file);
                    if let Some(&f0) = loc.get(&(r.ns, r.name)) {
                        if f0 != here {
                            let v = all.entry((r.ns, r.name)).or_insert_with(|| vec![f0]);
                            if !v.contains(&here) {
                                v.push(here);
                            }
                        }
                    }
                });
            }
            all
        });
        m.get(&(ns, name)).map_or(&[][..], |v| v.as_slice())
    }

    /// Where is namespace `ns` defined.
    pub fn locate_ns(&self, ns: SymId) -> Option<(usize, usize)> {
        self.locate(ns, SymId::NONE)
    }

    /// All `java-class-definitions` (classpath order).
    pub fn classes(&self) -> Vec<(usize, ClassDef)> {
        self.jars.iter().enumerate().flat_map(|(i, j)| j.classes().into_iter().map(move |c| (i, c))).collect()
    }
}

fn worker_count(n: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    cores.saturating_sub(2).max(1).min(n.max(1))
}

/// Analyze (or load from `cache_dir`) every jar of `cp`; parallel over jars (cores - 2 threads).
pub fn analyze_classpath(cp: &Classpath, cache_dir: &Path) -> JarLayer {
    analyze_jars(&cp.jars, cache_dir, &Config::new())
}

/// Like `analyze_classpath` with the project's config (`:lint-as` / hooks change which defs a jar yields, e.g.
/// `schema.core/defn`). Jars analyzed under a non-default config get their own cache file (config fingerprint in the name).
pub fn analyze_classpath_with(cp: &Classpath, cache_dir: &Path, cfg: &Config) -> JarLayer {
    analyze_jars(&cp.jars, cache_dir, cfg)
}

/// Stable fingerprint of the config parts that affect analysis; empty for the default config.
fn config_fp(cfg: &Config) -> String {
    let dflt = Config::new();
    let ents = |c: &Config| -> Vec<String> {
        let mut v: Vec<String> = c.lint_as.iter().map(|(k, t)| format!("L{}/{}>{}/{}", k.0.as_str(), k.1.as_str(), t.0.as_str(), t.1.as_str())).collect();
        v.extend(c.hooks.iter().map(|(k, t)| format!("H{}/{}>{}/{}", k.0.as_str(), k.1.as_str(), t.0.as_str(), t.1.as_str())));
        v.sort();
        v
    };
    let (a, b) = (ents(cfg), ents(&dflt));
    if a == b {
        return String::new();
    }
    let h = crate::io::hash_parts(a.iter().map(|s| s.as_bytes()));
    format!(".{}", &h[..12])
}

pub fn analyze_jars(jars: &[String], cache_dir: &Path, cfg: &Config) -> JarLayer {
    let _ = std::fs::create_dir_all(cache_dir);
    let fp = config_fp(cfg);
    let next = AtomicUsize::new(0);
    let mut slots: Vec<Option<JarData>> = (0..jars.len()).map(|_| None).collect();
    let workers = worker_count(jars.len());
    let results = std::sync::Mutex::new(&mut slots);
    std::thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| {
                let defs = DefsIndex::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= jars.len() {
                        break;
                    }
                    let r = load_or_analyze(Path::new(&jars[i]), cache_dir, cfg, &fp, &defs);
                    results.lock().unwrap()[i] = r;
                }
            });
        }
    });
    let mut st = Stats { jars: jars.len(), ..Default::default() };
    let jars: Vec<JarData> = slots.into_iter().flatten().collect();
    st.failed = st.jars - jars.len();
    for j in &jars {
        if j.warm {
            st.warm += 1
        } else {
            st.cold += 1
        }
        st.files += j.file_count;
        st.defs += j.def_count;
        st.classes += j.class_count;
        st.cache_bytes += j.cache_bytes;
    }
    JarLayer { jars, stats: st, loc: OnceLock::new(), multi: OnceLock::new() }
}

fn load_or_analyze(path: &Path, cache_dir: &Path, cfg: &Config, fp: &str, defs: &DefsIndex) -> Option<JarData> {
    let key = jar::stat_key(path).ok()?;
    let cp = cache_dir.join(format!("{key}{fp}.nxc"));
    if let Some(j) = open_cached(path, &key, &cp, true) {
        return Some(j);
    }
    let tmp = cache_dir.join(format!("{key}.nxc.{}.{:?}.tmp", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_file(&tmp);
    let ok = build(path, cfg, defs, &tmp).is_some();
    if ok && std::fs::rename(&tmp, &cp).is_ok() {
        return open_cached(path, &key, &cp, false);
    }
    let _ = std::fs::remove_file(&tmp);
    None
}

fn open_cached(jar_path: &Path, key: &str, cache_path: &Path, warm: bool) -> Option<JarData> {
    let cache = Cache::open(cache_path, SCHEMA);
    let v = cache.get(b"v")?;
    if v.len() != 16 || le32(v, 0) != ANALYSIS_REV {
        return None;
    }
    let (nf, nc, nd) = (le32(v, 4) as usize, le32(v, 8) as usize, le32(v, 12) as usize);
    let s = cache.get(b"s")?;
    let d = cache.get(b"d")?;
    let f = cache.get(b"f")?;
    let c = cache.get(b"c")?;
    if d.len() != nd * REC || f.len() < 4 + nf * 12 || le32(f, 0) as usize != nf || c.len() != 4 + nc * 12 || le32(c, 0) as usize != nc {
        return None;
    }
    Strs::parse(s)?;
    let cache_bytes = std::fs::metadata(cache_path).map(|m| m.len() as usize).unwrap_or(0);
    // SAFETY: slices point into `cache`'s mmap, which is heap-stable and owned by the returned struct (never remapped).
    let st = |b: &[u8]| unsafe { std::mem::transmute::<&[u8], &'static [u8]>(b) };
    Some(JarData {
        path: jar_path.to_string_lossy().into_owned(),
        key: key.to_string(),
        warm,
        strs_blob: st(s),
        defs_blob: st(d),
        files_blob: st(f),
        classes_blob: st(c),
        aliases_blob: st(cache.get(b"a")?),
        strs: OnceLock::new(),
        file_count: nf,
        def_count: nd,
        class_count: nc,
        cache_bytes,
        _cache: cache,
    })
}

fn build(path: &Path, cfg: &Config, defs: &DefsIndex, out: &PathBuf) -> Option<()> {
    let mut jr = Jar::open(path).ok()?;
    let mut buf = Vec::new();
    let mut tab = StrTab::default();
    let mut blobs = W::default();
    let mut file_tab: Vec<(u32, u32, u32)> = Vec::new(); // (name idx, off, len)
    let mut tables: HashMap<(u8, SymId), defs::NsTable> = HashMap::new();
    let mut owner: HashMap<(u8, SymId, SymId), (u32, bool)> = HashMap::new();
    let mut classes: Vec<(u32, u32, u8)> = Vec::new();
    let mut ns_only: Vec<(u8, SymId, u32)> = Vec::new();
    let mut analyzed: Vec<(String, FileAnalysis)> = Vec::new();
    let mut aliases: std::collections::BTreeSet<(u32, u32)> = Default::default();
    jr.for_each(&[EntryKind::Source, EntryKind::Java], &mut buf, |name, kind, bytes| {
        if kind == EntryKind::Java {
            let txt = String::from_utf8_lossy(bytes);
            for (cls, fl) in java::source_classes(&txt) {
                classes.push((tab.str_idx(&cls) as u32 - 1, tab.str_idx(name) as u32 - 1, fl));
            }
            return;
        }
        let Some(fk) = FileKind::from_path(name) else { return };
        if fk == FileKind::Edn && name.contains("clj-kondo.exports") {
            return; // kondo config exports are not analyzed
        }
        let txt = String::from_utf8_lossy(bytes);
        let mut o = Options::external();
        if name.contains('/') && name.rsplit('/').next() == Some("project.clj") {
            o.init_ns = intern("leiningen.core.project");
        }
        analyzed.push((name.to_string(), analyze_file_opts(&txt, fk, cfg, defs, o)));
    })
    .ok()?;
    // second pass: cross-file protocol resolution within the jar (`in-ns` continuations)
    let mut local = DefsIndex::new();
    for (name, fa) in &analyzed {
        local.add_file_at(fa, Some(name));
    }
    for (name, mut fa) in analyzed {
        finish_extras(&mut fa, &local);
        let name: &str = &name;
        let fi = file_tab.len() as u32;
        for u in &fa.namespace_usages {
            if !u.alias.is_none() {
                aliases.insert((tab.idx(u.to) as u32, tab.idx(u.alias) as u32));
            }
        }
        let base = fa.base_lang.unwrap_or(BaseLang::Clj);
        for d in &fa.var_definitions {
            let src = defs::src_of(base, d.lang) as u8;
            let t = tables.entry((src, d.ns)).or_default();
            let primary = defs::is_primary(Some(name), d.ns);
            if defs::merge_def(t, d.name, defs::var_info(d), primary, d.declared) {
                owner.insert((src, d.ns, d.name), (fi, primary));
            }
        }
        for n in &fa.namespace_definitions {
            let src = defs::src_of(base, n.lang) as u8;
            tables.entry((src, n.name)).or_default();
            ns_only.push((src, n.name, fi));
        }
        let off = blobs.b.len() as u32;
        encode_fa(&fa, &mut blobs, &mut tab);
        file_tab.push((tab.str_idx(name) as u32 - 1, off, blobs.b.len() as u32 - off));
    }
    let mut infos: Vec<(String, java::ClassInfo)> = Vec::new();
    jr.for_each(&[EntryKind::Class], &mut buf, |name, _, b| {
        if let Some(ci) = java::class_info(b) {
            infos.push((name.to_string(), ci));
        }
    })
    .ok()?;
    for (entry, ci) in &infos {
        if java::keep_class(ci) {
            classes.push((tab.str_idx(&ci.name) as u32 - 1, tab.str_idx(entry) as u32 - 1, ci.flags));
        }
    }

    // def summary: merged vars first, then namespace records (first defining file of each table)
    let mut d = Vec::with_capacity((owner.len() + ns_only.len()) * REC);
    let mut n_defs = 0u32;
    let mut rec = |d: &mut Vec<u8>, src: u8, flags: u8, va: u16, fixed: u64, ns: SymId, name: u64, dep: u64, file: u32, tab: &mut StrTab| {
        d.push(src);
        d.push(flags);
        d.extend_from_slice(&va.to_le_bytes());
        d.extend_from_slice(&fixed.to_le_bytes());
        d.extend_from_slice(&(tab.idx(ns) as u32).to_le_bytes());
        d.extend_from_slice(&(name as u32).to_le_bytes());
        d.extend_from_slice(&(dep as u32).to_le_bytes());
        d.extend_from_slice(&file.to_le_bytes());
        n_defs += 1;
    };
    for (&(src, ns), t) in &tables {
        for (&name, info) in t {
            let (fi, primary) = owner.get(&(src, ns, name)).copied().unwrap_or((0, true));
            let nm = tab.idx(name);
            let dp = tab.idx(info.deprecated.0);
            rec(&mut d, src, info.flags | if primary { R_PRIMARY } else { 0 }, info.varargs_min, info.fixed.0, ns, nm, dp, fi, &mut tab);
        }
    }
    for (src, ns, fi) in ns_only {
        rec(&mut d, src, R_NS_ONLY, 0, 0, ns, 0, 0, fi, &mut tab);
    }

    let mut f = Vec::new();
    f.extend_from_slice(&(file_tab.len() as u32).to_le_bytes());
    for (n, o, l) in &file_tab {
        f.extend_from_slice(&n.to_le_bytes());
        f.extend_from_slice(&(o + 4 + file_tab.len() as u32 * 12).to_le_bytes());
        f.extend_from_slice(&l.to_le_bytes());
    }
    f.extend_from_slice(&blobs.b);
    let mut c = Vec::new();
    c.extend_from_slice(&(classes.len() as u32).to_le_bytes());
    for (cl, en, fl) in &classes {
        c.extend_from_slice(&cl.to_le_bytes());
        c.extend_from_slice(&en.to_le_bytes());
        c.extend_from_slice(&[*fl, 0, 0, 0]);
    }
    let mut a = Vec::with_capacity(4 + aliases.len() * 8);
    a.extend_from_slice(&(aliases.len() as u32).to_le_bytes());
    for (to, al) in &aliases {
        a.extend_from_slice(&to.to_le_bytes());
        a.extend_from_slice(&al.to_le_bytes());
    }
    let mut s = Vec::new();
    tab.write(&mut s);
    let mut v = Vec::with_capacity(16);
    for x in [ANALYSIS_REV, file_tab.len() as u32, classes.len() as u32, n_defs] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    let mut cache = Cache::open(out, SCHEMA);
    cache.put(b"s", &s).ok()?;
    cache.put(b"d", &d).ok()?;
    cache.put(b"f", &f).ok()?;
    cache.put(b"c", &c).ok()?;
    cache.put(b"a", &a).ok()?;
    cache.put(b"v", &v).ok()?;
    Some(())
}
