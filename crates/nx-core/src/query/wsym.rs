//! workspace/symbol (feature/workspace_symbols.clj): internal ns / var definitions and defmethods, fuzzy filtered.
use super::callh::simple_name;
use super::*;
use std::collections::HashSet;

/// `anonimitoraf.clj-flx/score` (clj-flx 1.2.0) on UTF-16 units, both already lowercased: None = non-match.
/// A match needs every query char to have a distinct occurrence (`chars-all-present?`) and, step by step, an occurrence of
/// char k after some occurrence of char k-1 (`chars-in-order?`, weaker than a subsequence); the score is the longest common substring.
fn flx_score(q: &[u16], c: &[u16]) -> Option<u32> {
    if q.is_empty() || c.is_empty() {
        return None;
    }
    let occs: Vec<Vec<u32>> = q.iter().map(|&x| c.iter().enumerate().filter(|(_, &y)| y == x).map(|(i, _)| i as u32).collect()).collect();
    let mut visited: Vec<u32> = Vec::new();
    for o in &occs {
        let f = o.iter().copied().find(|i| !visited.contains(i))?;
        visited.push(f);
    }
    visited.clear();
    let mut prev: Vec<i64> = vec![-1];
    for o in &occs {
        let cur: Vec<u32> = o.iter().copied().filter(|i| !visited.contains(i)).collect();
        if cur.is_empty() || !cur.iter().any(|&x| prev.iter().any(|&p| p < x as i64)) {
            return None;
        }
        visited.push(cur[0]);
        prev = cur.iter().map(|&x| x as i64).collect();
    }
    let mut best = 0u32;
    let mut row = vec![0u32; q.len()];
    for &ch in c {
        let mut next = vec![0u32; q.len()];
        for (j, &s) in q.iter().enumerate() {
            if s == ch {
                next[j] = if j == 0 { 1 } else { row[j - 1] + 1 };
                best = best.max(next[j]);
            }
        }
        row = next;
    }
    Some(best)
}

fn lower16(s: &str) -> Vec<u16> {
    s.to_lowercase().encode_utf16().collect()
}

pub fn workspace_symbol(q: &Q, query: &str) -> String {
    let blank = query.trim().is_empty();
    let ql = lower16(query);
    let mut out: Vec<(FileId, String)> = Vec::new();
    let mut scores: Vec<u32> = Vec::new();
    let mut seen: HashSet<(u32, u32, u32, u32)> = HashSet::new();
    let mut push = |f: FileId, e: El, name: &str, pos: Pos, label: String| {
        let mut sc = 0;
        if !blank {
            match flx_score(&ql, &lower16(simple_name(name))) {
                Some(v) => sc = v,
                None => return,
            }
        }
        scores.push(sc);
        out.push((
            f,
            format!(
                "{{\"name\":{},\"kind\":{},\"location\":{{\"uri\":{},\"range\":{}}}}}",
                json_str(&label),
                q.symbol_kind(e),
                json_str(q.uri(f)),
                range_json(pos)
            ),
        ));
    };
    for (idx, ent) in q.s.files.iter().enumerate() {
        let f = idx as FileId;
        let Some(ent) = ent else { continue };
        if !ent.internal {
            continue;
        }
        let Some(fa) = ent.fa() else { continue };
        for (i, n) in fa.namespace_definitions.iter().enumerate() {
            if n.name_pos.row != 0 {
                push(f, El { f, b: B::NsDef, i: i as u32 }, n.name.as_str(), n.pos, n.name.as_str().to_string());
            }
        }
        for (i, d) in fa.var_definitions.iter().enumerate() {
            if d.name.is_none() || d.name_pos.row == 0 {
                continue;
            }
            push(f, El { f, b: B::VarDef, i: i as u32 }, d.name.as_str(), d.pos, d.name.as_str().to_string());
        }
    }
    // defmethods: one distinct state across all files
    for (idx, ent) in q.s.files.iter().enumerate() {
        let f = idx as FileId;
        let Some(ent) = ent else { continue };
        if !ent.internal {
            continue;
        }
        let Some(fa) = ent.fa() else { continue };
        for (i, u) in fa.var_usages.iter().enumerate() {
            if u.defmethod && !u.derived && !u.derived_name && u.name_pos.row != 0 && seen.insert((u.to.0, u.name.0, u.name_pos.row, u.name_pos.col)) {
                let mut label = u.name.as_str().to_string();
                if !u.dispatch_val_str.is_none() {
                    label.push(' ');
                    label.push_str(u.dispatch_val_str.as_str());
                }
                push(f, El { f, b: B::VarUsage, i: i as u32 }, u.name.as_str(), u.pos, label);
            }
        }
    }
    // `sort-by :score (comp - compare)` is stable; then grouped by uri in first-seen order
    if !blank {
        let mut idx: Vec<usize> = (0..out.len()).collect();
        idx.sort_by(|&a, &b| scores[b].cmp(&scores[a]));
        let mut old: Vec<Option<(FileId, String)>> = out.into_iter().map(Some).collect();
        out = idx.into_iter().map(|i| old[i].take().unwrap()).collect();
    }
    let mut order: Vec<FileId> = Vec::new();
    for (f, _) in &out {
        if !order.contains(f) {
            order.push(*f);
        }
    }
    let mut s = String::from("[");
    let mut first = true;
    for f in order {
        for (g, j) in &out {
            if *g == f {
                if !first {
                    s.push(',');
                }
                first = false;
                s.push_str(j);
            }
        }
    }
    s.push(']');
    s
}
