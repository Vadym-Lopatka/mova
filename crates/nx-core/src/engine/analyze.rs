//! THE SEAM: text -> `FileResult` (native reader + the real analyzer, pass 1). Usage targets are filled by the store.
use super::ctx::Ctx;
use super::index;
use super::types::{FileResult, Finding, Lang, Level};
use crate::analyzer::{analyze_cst_at, FileKind, Options};
use std::hash::Hasher;
use std::sync::Arc;

/// 64-bit text hash (process-local use only: skip/dedupe).
pub fn hash_text(t: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(t.as_bytes());
    h.finish()
}

pub fn file_kind(lang: Lang) -> Option<FileKind> {
    match lang {
        Lang::Clj => Some(FileKind::Clj),
        Lang::Cljs => Some(FileKind::Cljs),
        Lang::Cljc => Some(FileKind::Cljc),
        Lang::Edn => Some(FileKind::Edn), // keyword usages / quoted symbols (references from other files)
        Lang::Unknown => None,
    }
}

/// kondo findings of an analyzed file (syntax + linters) as engine findings; level 0 (off) dropped.
pub fn kondo_findings(fa: &crate::analyzer::FileAnalysis) -> Vec<Finding> {
    let mut v: Vec<Finding> = crate::analyzer::lint::final_findings(fa)
        .into_iter()
        .filter(|(_, lv, _)| *lv != 0)
        .map(|(f, lv, _)| {
            let p = f.pos;
            let (er, ec) = if p.end_row == 0 { (p.row, p.col) } else { (p.end_row, p.end_col) };
            let level = match lv {
                3 => Level::Error,
                2 => Level::Warning,
                _ => Level::Info,
            };
            Finding { level, ty: f.ty.name().to_string(), row: p.row, col: p.col, end_row: er, end_col: ec, message: f.msg }
        })
        .collect();
    v.sort_by_key(|f| (f.row, f.col)); // kondo orders findings by position (stable: message order kept)
    v
}

/// Analyze one file text with the shared context. Pure; runs on pool workers. `internal` = project source.
pub fn analyze_file_ctx(uri: &str, text: String, lang: Lang, internal: bool, ctx: &Ctx) -> FileResult {
    analyze_file_ctx_pos(uri, text, lang, internal, true, ctx)
}

/// `with_pos`: build the position index now (open documents); disk files build it on first query.
pub fn analyze_file_ctx_pos(uri: &str, text: String, lang: Lang, internal: bool, with_pos: bool, ctx: &Ctx) -> FileResult {
    let hash = hash_text(&text);
    let Some(kind) = file_kind(lang) else {
        return FileResult { hash, findings: Vec::new(), analysis: None, pos: None, text: None };
    };
    let tp = crate::met::parse_start(); // Some only inside a timed worker job (metrics on)
    let cst = crate::reader::parse_owned(text);
    crate::met::parse_end(tp);
    let lint = crate::analyzer::lint::lint_enabled(internal);
    let mut findings: Vec<Finding> = if internal && !lint {
        cst.errors()
            .iter()
            .map(|e| Finding { level: Level::Error, ty: "syntax".to_string(), row: e.row, col: e.col, end_row: e.row, end_col: e.col, message: e.msg.clone() })
            .collect()
    } else {
        Vec::new()
    };
    let keep: Option<Arc<str>> = if internal && with_pos { Some(Arc::from(cst.src())) } else { None };
    let mut opts = if internal { Options::internal() } else { Options::external() };
    if internal {
        opts.apply_path(uri);
    }
    opts.mova = uri.ends_with(".mova");
    if let Some(ns) = ctx.init_ns.load().get(uri) {
        opts.init_ns = *ns;
    }
    let (cfg, defs) = (ctx.cfg.load(), ctx.defs.load());
    let fpath = if internal { Some(super::scan::uri_to_path(uri).map_or_else(|| uri.to_string(), |p| p.to_string_lossy().into_owned())) } else { None };
    let mut fa = analyze_cst_at(cst, kind, &cfg, &defs, opts, fpath.as_deref());
    if internal {
        drop_external_keyword_usages(uri, &mut fa, ctx);
    }
    fa.shrink();
    if lint {
        findings = kondo_findings(&fa); // provisional (pass 1); the store refreshes them after `finish_usages`
    }
    findings.sort_by_key(|f| (f.row, f.col)); // kondo orders findings by position
    let pos = with_pos.then(|| {
        let mut p = index::build_pos(&fa);
        p.shrink();
        p
    });
    FileResult { hash, findings, analysis: Some(fa), pos, text: keep }
}

/// JVM `kondo/element->normalized-elements`: keyword usages of a file outside the source paths (`external-filename?`)
/// are dropped; registered keyword definitions stay.
fn drop_external_keyword_usages(uri: &str, fa: &mut crate::analyzer::FileAnalysis, ctx: &Ctx) {
    let sps = ctx.source_paths.load();
    if sps.is_empty() || std::env::var_os("NX_KW_EXT_OFF").is_some() {
        return;
    }
    let Some(p) = super::scan::uri_to_path(uri) else { return };
    let p = std::fs::canonicalize(&p).unwrap_or(p);
    if !sps.iter().any(|s| p.starts_with(s)) {
        fa.keywords.retain(|k| !k.reg.is_none());
    }
}

/// Analysis with an empty context (tests, tools).
pub fn analyze_file(uri: &str, text: String, lang: Lang) -> FileResult {
    analyze_file_ctx(uri, text, lang, true, &Ctx::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn syntax_error_found() {
        let r = analyze_file("file:///a.clj", "(defn f [x]\n  (+ x 1)".to_string(), Lang::Clj);
        assert!(!r.findings.is_empty());
        assert_eq!(r.findings[0].ty, "syntax");
        let ok = analyze_file("file:///a.clj", "(ns a)\n(def x 1)\n".to_string(), Lang::Clj);
        assert!(ok.findings.is_empty());
        assert_eq!(ok.analysis.unwrap().var_definitions.len(), 1);
    }
}
