//! Post-processing of findings: cljc collapse (kondo `core-impl/filter-findings`).
use super::*;
use crate::analyzer::types::*;
use std::collections::BTreeMap;

/// Collapse findings for output. For cljc files kondo groups by (row, col, type), drops the
/// "redundant-*" kinds unless both dialects agree, and merges equal messages into one finding with `langs`.
/// Returns (finding, langs) pairs.
pub fn collapse(base: Option<BaseLang>, fs: &[Finding]) -> Vec<(Finding, Vec<u8>)> {
    if base != Some(BaseLang::Cljc) {
        return fs.iter().map(|f| (f.clone(), Vec::new())).collect();
    }
    let mut groups: BTreeMap<(u32, u32, u8), Vec<&Finding>> = BTreeMap::new();
    for f in fs {
        groups.entry((f.pos.row, f.pos.col, f.ty as u8)).or_default().push(f);
    }
    let mut out = Vec::new();
    for (_, g) in groups {
        let ty = g[0].ty;
        let interest = !matches!(ty, FType::RedundantDo | FType::RedundantLet | FType::RedundantLetBinding | FType::RedundantCall | FType::SingleLogicalOperand | FType::RedundantNestedCall | FType::RedundantFnWrapper | FType::RedundantIgnore | FType::UnusedExcludedVar);
        if !interest && g.len() != 2 {
            continue;
        }
        let mut by_msg: Vec<(&str, &Finding, Vec<u8>)> = Vec::new();
        for f in g {
            match by_msg.iter_mut().find(|e| e.0 == f.msg) {
                Some(e) => e.2.push(f.lang),
                None => by_msg.push((&f.msg, f, vec![f.lang])),
            }
        }
        for (_, f, langs) in by_msg {
            out.push((f.clone(), langs));
        }
    }
    out
}

/// Final findings of a file as kondo reports them: cljc collapse, then `(sort-by (juxt :filename :row :col :message))`
/// and `dedupe` of consecutive equal findings.
pub fn finalize(base: Option<BaseLang>, fs: &[Finding]) -> Vec<(Finding, Vec<u8>)> {
    let mut v = collapse(base, fs);
    v.sort_by(|a, b| (a.0.pos.row, a.0.pos.col, &a.0.msg).cmp(&(b.0.pos.row, b.0.pos.col, &b.0.msg)));
    v.dedup_by(|b, a| a.0.ty == b.0.ty && a.0.pos == b.0.pos && a.0.msg == b.0.msg && a.0.extra == b.0.extra && a.1 == b.1 && a.0.lang == b.0.lang);
    v
}

/// kondo `reg-refer-alls!`: `:refer :all` / `:use` findings listing the vars actually used through them.
/// `used` = (caller ns, defining ns, var name, lang) of resolved simple-symbol usages.
pub fn refer_alls(fa: &mut FileAnalysis, used: &[(crate::intern::SymId, crate::intern::SymId, crate::intern::SymId, u8)]) {
    use crate::analyzer::expr::json_str;
    let recs = std::mem::take(&mut fa.lint_ralls);
    for r in &recs {
        let mut names: Vec<&str> = used.iter().filter(|u| u.0 == r.from && u.1 == r.ns && u.3 == r.lang).map(|u| u.2.as_str()).collect();
        names.sort();
        names.dedup();
        let mut msg = format!("use {}alias or :refer", if r.is_use { if r.kw { ":require with " } else { "require with " } } else { "" });
        if !names.is_empty() {
            msg.push_str(&format!(" [{}]", names.join(" ")));
        }
        let refers = format!("[{}]", names.iter().map(|s| json_str(s)).collect::<Vec<_>>().join(","));
        let mut f = Finding::new(if r.is_use { FType::Use } else { FType::ReferAll }, r.pos, msg);
        f.lang = r.lang;
        f.extra.push(("refers", refers));
        fa.findings.push(f);
    }
    fa.lint_ralls = recs;
}
