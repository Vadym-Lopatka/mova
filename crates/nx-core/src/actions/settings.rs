//! clojure-lsp settings for the refactorings: client `initializationOptions` < global config < project `.lsp/config.edn`.
use super::tree::{Tag, Tk, Tree};
use crate::analyzer::json::Json;
use std::path::Path;

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t[1..t.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        t.to_string()
    }
}

fn conv(t: &Tree, id: u32) -> Json {
    let z = t.z(id);
    match z.tag() {
        Tag::Map => {
            let items = elems(t, id);
            let mut out: Vec<(String, Json)> = Vec::new();
            for p in items.chunks(2) {
                if p.len() < 2 {
                    break;
                }
                let k = conv(t, p[0]);
                let key = match k {
                    Json::Str(s) => s.trim_start_matches(':').to_string(),
                    _ => continue,
                };
                out.push((key, conv(t, p[1])));
            }
            Json::Obj(out)
        }
        Tag::Vector | Tag::List | Tag::Set => Json::Arr(elems(t, id).into_iter().map(|c| conv(t, c)).collect()),
        Tag::Meta => elems(t, id).last().map_or(Json::Null, |&c| conv(t, c)),
        Tag::Token | Tag::MultiLine => match z.tk() {
            Tk::Str => Json::Str(unquote(z.text())),
            Tk::Kw | Tk::KwAuto => Json::Str(z.text().to_string()),
            Tk::Num => Json::Num(z.text().parse::<f64>().unwrap_or(0.0)),
            Tk::Const => match z.text() {
                "true" => Json::Bool(true),
                "false" => Json::Bool(false),
                _ => Json::Null,
            },
            _ => Json::Str(z.text().to_string()),
        },
        _ => Json::Null,
    }
}

fn elems(t: &Tree, id: u32) -> Vec<u32> {
    t.z(id).kids().iter().copied().filter(|&k| !super::clauses::skippable(t.nodes[k as usize].tag)).collect()
}

pub fn read_edn_file(p: &Path) -> Option<Json> {
    let text = std::fs::read_to_string(p).ok()?;
    let t = Tree::parse(&text);
    if t.err {
        return None;
    }
    let first = t.root().kids().iter().copied().find(|&k| !super::clauses::skippable(t.nodes[k as usize].tag))?;
    Some(conv(&t, first))
}

/// `shared/deep-merge`: maps merge recursively, everything else is replaced by `b`.
pub fn deep_merge(a: Json, b: Json) -> Json {
    match (a, b) {
        (Json::Obj(mut av), Json::Obj(bv)) => {
            for (k, v) in bv {
                match av.iter().position(|(ak, _)| *ak == k) {
                    Some(i) => {
                        let old = std::mem::replace(&mut av[i].1, Json::Null);
                        av[i].1 = deep_merge(old, v);
                    }
                    None => av.push((k, v)),
                }
            }
            Json::Obj(av)
        }
        (_, b) => b,
    }
}

/// Effective settings: init options, then the global config file, then the project config file.
pub fn effective(root: Option<&Path>, init: Option<Json>) -> Option<Json> {
    let mut s = init.unwrap_or(Json::Obj(vec![]));
    let cfg = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    if let Some(c) = cfg {
        if let Some(g) = read_edn_file(&c.join("clojure-lsp/config.edn")) {
            s = deep_merge(s, g);
        }
    }
    if let Some(r) = root {
        if let Some(p) = read_edn_file(&r.join(".lsp/config.edn")) {
            s = deep_merge(s, p);
        }
    }
    Some(s)
}
