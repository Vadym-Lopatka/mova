//! `#_:clj-kondo/ignore` / `#_{:clj-kondo/ignore [:linter ...]}` (kondo `findings/ignored?`, `utils/handle-ignore`)
//! and the `redundant-ignore` linter. Regions come from the CST; findings are filtered at the end of
//! `finish_usages` (when all findings exist).
use super::*;
use crate::cst::{Cst, Kind, NodeId};

#[derive(Clone, Debug)]
pub struct IgnoreRegion {
    /// Extent of the form following the marker.
    pub pos: Pos,
    /// `None` = all linters.
    pub linters: Option<Vec<FType>>,
    /// Position of the marker node (keyword or map).
    pub marker: Pos,
    /// Marker was the bare keyword (`:linters :all`) rather than a map.
    pub kw: bool,
}

/// Regions of all ignore markers of a parsed file.
pub fn collect(c: &Cst) -> Vec<IgnoreRegion> {
    let mut out = Vec::new();
    if !c.src().contains("clj-kondo/ignore") {
        return out;
    }
    for i in 0..c.len() {
        let n = NodeId(i as u32);
        if !Cst::is_container(c.kind(n)) {
            continue;
        }
        let ch = c.children(n);
        for (j, &k) in ch.iter().enumerate() {
            if c.kind(k) != Kind::Uneval {
                continue;
            }
            let Some(&inner) = c.children(k).first() else { continue };
            let Some((linters, kw)) = marker(c, inner) else { continue };
            // the next form after the marker (skipping further unevals)
            let Some(&next) = ch[j + 1..].iter().find(|&&x| c.kind(x) != Kind::Uneval) else { continue };
            out.push(IgnoreRegion { pos: c.pos(next), linters, marker: c.pos(inner), kw });
        }
    }
    out
}

fn is_ignore_kw(c: &Cst, n: NodeId) -> bool {
    c.kind(n) == Kind::Keyword && c.ns(n).as_str() == "clj-kondo" && c.name(n).as_str() == "ignore"
}

/// `(linters, kw)` of a marker form, `None` when not an ignore marker.
fn marker(c: &Cst, n: NodeId) -> Option<(Option<Vec<FType>>, bool)> {
    if is_ignore_kw(c, n) {
        return Some((None, true));
    }
    if c.kind(n) == Kind::Map {
        let kids: Vec<NodeId> = c.sig_children(n).collect();
        if kids.len() >= 2 && is_ignore_kw(c, kids[0]) {
            let v = kids[1];
            return Some(match c.kind(v) {
                Kind::True => (None, false),
                Kind::Keyword if c.name(v).as_str() == "all" => (None, false),
                Kind::Vector | Kind::Set | Kind::List => {
                    let mut ls = Vec::new();
                    for x in c.sig_children(v) {
                        if c.kind(x) == Kind::Keyword {
                            if let Some(t) = FType::from_name(c.name(x).as_str()) {
                                ls.push(t);
                            }
                        }
                    }
                    (Some(ls), false)
                }
                _ => (Some(Vec::new()), false),
            });
        }
    }
    None
}

/// Filter `fa.findings` by the ignore regions; append `redundant-ignore` findings for unused ones.
pub fn apply(fa: &mut super::super::types::FileAnalysis) {
    if fa.lint_ignores.is_empty() {
        return;
    }
    let regions = std::mem::take(&mut fa.lint_ignores);
    let mut used = vec![false; regions.len()];
    let fs = std::mem::take(&mut fa.findings);
    for f in fs {
        let mut ignored = false;
        if f.pos.end_row != 0 {
            for (i, r) in regions.iter().enumerate() {
                let starts = (f.pos.row, f.pos.col) >= (r.pos.row, r.pos.col);
                let ends = (f.pos.end_row, f.pos.end_col) <= (r.pos.end_row, r.pos.end_col);
                if starts && ends && r.linters.as_ref().map_or(true, |l| l.contains(&f.ty)) {
                    used[i] = true;
                    ignored = true;
                    break;
                }
            }
        }
        if !ignored {
            fa.findings.push(f);
        }
    }
    let level = fa.lint_levels.get(FType::RedundantIgnore as usize).copied().unwrap_or_else(|| FType::RedundantIgnore.default_level());
    if level != OFF {
        let langs: &[u8] = match fa.base_lang {
            Some(super::super::types::BaseLang::Cljc) => &[super::super::types::L_CLJ, super::super::types::L_CLJS],
            Some(super::super::types::BaseLang::Cljs) => &[super::super::types::L_CLJS],
            _ => &[super::super::types::L_CLJ],
        };
        for (i, r) in regions.iter().enumerate() {
            if used[i] {
                continue;
            }
            for &l in langs {
                let mut f = Finding::new(FType::RedundantIgnore, r.marker, "Redundant ignore");
                f.lang = l;
                f.explicit_lang = true;
                f.extra.push(("linters", "null".to_owned()));
                fa.findings.push(f);
            }
        }
    }
    fa.lint_ignores = regions;
}
