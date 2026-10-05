//! Plain data: languages, findings, per-file entries.
use crate::analyzer::FileAnalysis;
use crate::jars::JarLayer;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use super::index::PosIdx;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Lang {
    Clj,
    Cljs,
    Cljc,
    Edn,
    Unknown,
}

impl Lang {
    /// By file extension (clojure-lsp `uri->file-type`): clj/bb/cljd/clj_kondo -> Clj, cljs, cljc, edn.
    pub fn from_path(p: &str) -> Lang {
        let ext = Path::new(p).extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "clj" | "bb" | "cljd" | "clj_kondo" | "mova" => Lang::Clj,
            "cljs" => Lang::Cljs,
            "cljc" => Lang::Cljc,
            "edn" => Lang::Edn,
            _ => Lang::Unknown,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Lang::Clj => "clj",
            Lang::Cljs => "cljs",
            Lang::Cljc => "cljc",
            Lang::Edn => "edn",
            Lang::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error,
    Warning,
    Info,
}

/// A kondo-shaped finding. Positions 1-based, cols UTF-16 (kondo). `end_*` == start when kondo gives none.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Finding {
    pub level: Level,
    /// kondo type name, possibly `ns/name` (`syntax`, `unused-binding`, `clojure-lsp/unused-public-var`).
    pub ty: String,
    pub row: u32,
    pub col: u32,
    pub end_row: u32,
    pub end_col: u32,
    pub message: String,
}

/// What the analyzer returns for one text.
pub struct FileResult {
    pub hash: u64,
    pub findings: Vec<Finding>,
    /// Pass-1 analysis (usage targets not yet filled: the store runs `finish_usages`).
    pub analysis: Option<FileAnalysis>,
    pub pos: Option<PosIdx>,
    /// Source text (internal files only).
    pub text: Option<Arc<str>>,
}

/// A dependency file whose analysis is decoded from the jar cache on first use.
pub struct LazyFile {
    pub layer: Arc<JarLayer>,
    pub jar: usize,
    pub file: usize,
    an: OnceLock<Option<Box<FileAnalysis>>>,
    pos: OnceLock<Box<PosIdx>>,
}

impl LazyFile {
    pub fn new(layer: Arc<JarLayer>, jar: usize, file: usize) -> LazyFile {
        LazyFile { layer, jar, file, an: OnceLock::new(), pos: OnceLock::new() }
    }
    pub fn analysis(&self) -> Option<&FileAnalysis> {
        self.an.get_or_init(|| self.layer.file(self.jar, self.file).map(Box::new)).as_deref()
    }
    pub fn pos(&self) -> Option<&PosIdx> {
        let fa = self.analysis()?;
        Some(self.pos.get_or_init(|| Box::new(super::index::build_pos(fa))))
    }
}

/// One file in the store. `version` = LSP doc version, -1 = read from disk.
#[derive(Clone)]
pub struct FileEntry {
    pub uri: Arc<str>,
    pub version: i64,
    pub lang: Lang,
    pub hash: u64,
    pub findings: Vec<Finding>,
    pub text: Option<Arc<str>>,
    pub analysis: Option<Arc<FileAnalysis>>,
    pub pos: Option<Arc<PosIdx>>,
    /// Built on first query for disk-read project files (open documents carry `pos` eagerly): ~2 MB less on lib/.
    pub lazy_pos: Arc<OnceLock<PosIdx>>,
    /// (to, name, usage idx) sorted; filled by the store after `finish_usages`.
    pub tgt: Arc<Vec<(u32, u32, u32)>>,
    /// Project source (true) or dependency/external (false).
    pub internal: bool,
    /// Dependency file decoded lazily from the jar cache (then `analysis`/`pos` are None).
    pub lazy: Option<Arc<LazyFile>>,
}

impl FileEntry {
    pub fn fa(&self) -> Option<&FileAnalysis> {
        match &self.analysis {
            Some(a) => Some(a),
            None => self.lazy.as_ref()?.analysis(),
        }
    }
    /// Source text: kept for open documents; closed project files are re-read from disk (verified by hash), never kept.
    pub fn text(&self) -> Option<Arc<str>> {
        if let Some(t) = &self.text {
            return Some(t.clone());
        }
        if !self.internal || self.version >= 0 || self.analysis.is_none() {
            return None;
        }
        let p = super::scan::uri_to_path(&self.uri)?;
        let s = super::pool::read_lossy(&p)?;
        (super::analyze::hash_text(&s) == self.hash).then(|| Arc::from(s))
    }
    pub fn pos_idx(&self) -> Option<&PosIdx> {
        match &self.pos {
            Some(p) => Some(p),
            None => match &self.lazy {
                Some(l) => l.pos(),
                None => {
                    let fa = self.analysis.as_deref()?;
                    Some(self.lazy_pos.get_or_init(|| super::index::build_pos(fa)))
                }
            },
        }
    }
}

/// True for jar/zip entries and JDK sources (never internal).
pub fn is_external_uri(uri: &str) -> bool {
    uri.starts_with("jar:") || uri.starts_with("zipfile:")
}
