//! `textDocument/formatting` and `rangeFormatting` on the native cljfmt (`crate::fmt`).
//! Config as clojure-lsp `feature/format.clj`: cljfmt defaults < `:style/indent` var metadata < user config
//! (`:cljfmt` of `~/.config/clojure-lsp/config.edn` and `<root>/.lsp/config.edn`, deep-merged with the
//! `:cljfmt-config-path` file, default `.cljfmt.edn`). Cached per project; reloaded when a config file's
//! (mtime, size) changes (checked per request: 3 stats) and the style-indent scan when the snapshot version changes.
use super::{json_str, Q};
use crate::cst::{Cst, Kind, NodeId};
use crate::fmt::{self, FmtConfig, FnArgIndent, Key, Part, Spec, TextEdit};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Minimal EDN value (numbers, booleans, regexes included; `io::edn` drops them).
#[derive(Clone, Debug)]
enum V {
    Map(Vec<(V, V)>),
    Seq(Vec<V>),
    Kw(String),
    Sym(String),
    Str(String),
    Re(String),
    Int(i64),
    Bool(bool),
    Nil,
}

fn conv(c: &Cst, id: NodeId) -> V {
    let id = c.unwrap_meta(id);
    let sig = |id: NodeId| -> Vec<NodeId> { c.sig_children(id).filter(|&x| c.kind(x) != Kind::Uneval).collect() };
    match c.kind(id) {
        Kind::Map => V::Map(sig(id).chunks(2).filter(|p| p.len() == 2).map(|p| (conv(c, p[0]), conv(c, p[1]))).collect()),
        Kind::Vector | Kind::List | Kind::Set => V::Seq(sig(id).into_iter().map(|x| conv(c, x)).collect()),
        Kind::Keyword => V::Kw(c.text(id).trim_start_matches(':').to_string()),
        Kind::Symbol => V::Sym(c.text(id).to_string()),
        Kind::String => V::Str(c.string_content(id).to_string()),
        Kind::Regex => V::Re(c.string_content(id).to_string()),
        Kind::Number => c.text(id).parse().map(V::Int).unwrap_or(V::Nil),
        Kind::True => V::Bool(true),
        Kind::False => V::Bool(false),
        Kind::Tagged => {
            let k = sig(id);
            // `#re "..."` (clojure-lsp reads the file with {'re re-pattern})
            match (k.first().map(|&t| c.text(t)), k.get(1)) {
                (Some("re"), Some(&v)) => match conv(c, v) {
                    V::Str(s) => V::Re(s),
                    o => o,
                },
                (_, Some(&v)) => conv(c, v),
                _ => V::Nil,
            }
        }
        _ => V::Nil,
    }
}

fn read_file(p: &Path) -> Option<V> {
    let src = std::fs::read_to_string(p).ok()?;
    let c = crate::reader::parse(&src);
    let f = c.sig_children(c.root()).find(|&x| c.kind(x) != Kind::Uneval)?;
    Some(conv(&c, f))
}

impl V {
    fn get(&self, key: &str) -> Option<&V> {
        match self {
            V::Map(kv) => kv.iter().find(|(k, _)| matches!(k, V::Kw(s) if s == key)).map(|(_, v)| v),
            _ => None,
        }
    }
    fn truthy(&self) -> Option<bool> {
        match self {
            V::Bool(b) => Some(*b),
            V::Nil => Some(false),
            _ => None,
        }
    }
    fn text(&self) -> Option<&str> {
        match self {
            V::Str(s) | V::Sym(s) | V::Kw(s) => Some(s),
            _ => None,
        }
    }
}

fn same_key(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::Kw(x), V::Kw(y)) | (V::Sym(x), V::Sym(y)) | (V::Str(x), V::Str(y)) | (V::Re(x), V::Re(y)) => x == y,
        _ => false,
    }
}

/// medley `deep-merge`: maps merge recursively, everything else is replaced by the right side.
fn deep_merge(a: V, b: V) -> V {
    match (a, b) {
        (V::Map(mut x), V::Map(y)) => {
            for (k, v) in y {
                match x.iter().position(|(k0, _)| same_key(k0, &k)) {
                    Some(i) => {
                        let old = std::mem::replace(&mut x[i].1, V::Nil);
                        x[i].1 = deep_merge(old, v);
                    }
                    None => x.push((k, v)),
                }
            }
            V::Map(x)
        }
        (_, b) => b,
    }
}

// ---- cljfmt config value -> FmtConfig ----

fn spec_of(v: &V) -> Option<Spec> {
    let V::Seq(s) = v else { return None };
    let idx = |i: usize| match s.get(i) {
        Some(V::Int(n)) if *n >= 0 => Some(*n as usize),
        _ => None,
    };
    match s.first()? {
        V::Kw(k) if k == "inner" => Some(Spec::Inner(idx(1)?, idx(2))),
        V::Kw(k) if k == "block" => Some(Spec::Block(idx(1)?)),
        V::Kw(k) if k == "default" => Some(Spec::Default),
        _ => None,
    }
}

fn part_of(v: &V) -> Option<Part> {
    match v {
        V::Sym(s) | V::Str(s) => Some(Part::Name(s.clone())),
        V::Re(s) => Some(Part::Re(s.clone())),
        _ => None,
    }
}

fn key_of(v: &V) -> Option<Key> {
    match v {
        V::Sym(s) => Some(match s.split_once('/') {
            Some((n, m)) if !n.is_empty() && !m.is_empty() => Key::Qual(n.into(), m.into()),
            _ => Key::Sym(s.clone()),
        }),
        V::Re(s) => Some(Key::Re(s.clone())),
        V::Seq(p) if p.len() == 2 => Some(Key::Vec(part_of(&p[0])?, part_of(&p[1])?)),
        _ => None,
    }
}

fn put_indents(cfg: &mut FmtConfig, v: Option<&V>) {
    let Some(V::Map(kv)) = v else { return };
    for (k, specs) in kv {
        let (Some(key), V::Seq(ss)) = (key_of(k), specs) else { continue };
        cfg.set_indent(key, ss.iter().filter_map(spec_of).collect());
    }
}

fn apply_user(cfg: &mut FmtConfig, u: &V) {
    let flag = |k: &str, f: &mut bool| {
        if let Some(b) = u.get(k).and_then(V::truthy) {
            *f = b;
        }
    };
    flag("indentation?", &mut cfg.indentation);
    flag("remove-consecutive-blank-lines?", &mut cfg.remove_consecutive_blank_lines);
    flag("remove-surrounding-whitespace?", &mut cfg.remove_surrounding_whitespace);
    flag("insert-missing-whitespace?", &mut cfg.insert_missing_whitespace);
    flag("remove-trailing-whitespace?", &mut cfg.remove_trailing_whitespace);
    flag("remove-multiple-non-indenting-spaces?", &mut cfg.remove_multiple_non_indenting_spaces);
    flag("indent-line-comments?", &mut cfg.indent_line_comments);
    flag("normalize-newlines-at-file-end?", &mut cfg.normalize_newlines_at_file_end);
    if let Some(V::Kw(k)) = u.get("function-arguments-indentation") {
        cfg.function_arguments_indentation = match k.as_str() {
            "cursive" => FnArgIndent::Cursive,
            "zprint" => FnArgIndent::Zprint,
            _ => FnArgIndent::Community,
        };
    }
    put_indents(cfg, u.get("indents"));
    put_indents(cfg, u.get("extra-indents"));
    for (field, key) in [(0, "alias-map"), (1, "refer-map")] {
        if let Some(V::Map(kv)) = u.get(key) {
            for (k, v) in kv {
                if let (Some(k), Some(v)) = (k.text(), v.text()) {
                    let m = if field == 0 { &mut cfg.alias_map } else { &mut cfg.refer_map };
                    m.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
}

// ---- `:style/indent` metadata (Cider) -> cljfmt spec, port of `style-indent->cljfmt-spec` ----

#[derive(Clone, Debug)]
enum J {
    Num(i64),
    Defn,
    Form,
    Vec(Vec<J>),
    Other,
}

fn parse_j(s: &str) -> J {
    let c = crate::reader::parse(s);
    let Some(f) = c.sig_children(c.root()).next() else { return J::Other };
    fn go(c: &Cst, id: NodeId) -> J {
        match c.kind(id) {
            Kind::Number => c.text(id).parse().map(J::Num).unwrap_or(J::Other),
            Kind::Keyword | Kind::String | Kind::Symbol => match c.text(id).trim_start_matches(':').trim_matches('"') {
                "defn" => J::Defn,
                "form" => J::Form,
                _ => J::Other,
            },
            Kind::Vector | Kind::List => J::Vec(c.sig_children(id).map(|x| go(c, x)).collect()),
            _ => J::Other,
        }
    }
    go(&c, f)
}

fn arg_spec(index: usize, a: &J) -> Option<Spec> {
    fn depth(x: &J) -> i64 {
        match x {
            J::Vec(v) => 1 + v.first().map_or(-1000, depth),
            J::Num(_) | J::Defn => 0,
            _ => -1000,
        }
    }
    let d = depth(a);
    (d >= 0).then_some(Spec::Inner(d as usize, Some(index)))
}

fn style_indent_spec(j: &J) -> Option<Vec<Spec>> {
    let (sym, args): (&J, &[J]) = match j {
        J::Vec(v) if !v.is_empty() => (&v[0], &v[1..]),
        J::Vec(_) => (&J::Other, &[]),
        o => (o, &[]),
    };
    let sym_spec = match sym {
        J::Num(n) if *n >= 0 => Some(Spec::Block(*n as usize)),
        J::Defn => Some(Spec::Inner(0, None)),
        _ => None,
    };
    let arg_specs: Vec<Spec> = args.iter().enumerate().filter_map(|(i, a)| arg_spec(i, a)).collect();
    let mut out: Vec<Spec> = sym_spec.into_iter().collect();
    if let Some((last, init)) = arg_specs.split_last() {
        out.extend(init.iter().cloned());
        match last {
            Spec::Inner(d, Some(i)) if *i == args.len() - 1 => out.push(Spec::Inner(*d, None)),
            t => out.push(t.clone()),
        }
    }
    (!out.is_empty()).then_some(out)
}

fn style_indents(q: &Q) -> Vec<(Key, Vec<Spec>)> {
    let mut out = Vec::new();
    for f in q.s.files.iter().flatten() {
        if !f.internal {
            continue;
        }
        let Some(fa) = f.analysis.as_ref() else { continue };
        for vd in &fa.var_definitions {
            if vd.meta.is_none() {
                continue;
            }
            let m = vd.meta.0.as_str();
            let Some(p) = m.find("\"style/indent\":") else { continue };
            let rest = &m[p + 15..];
            let Some(spec) = style_indent_spec(&parse_j(rest.strip_suffix('}').unwrap_or(rest))) else { continue };
            out.push((Key::Qual(vd.ns.as_str().to_string(), vd.name.as_str().to_string()), spec));
        }
    }
    out
}

// ---- per-project cache ----

type Stamp = Option<(SystemTime, u64)>;

struct Cached {
    root: Option<PathBuf>,
    stamps: Vec<Stamp>,
    base: Arc<FmtConfig>,
    /// snapshot version the style-indent scan ran for, and the resulting config.
    ver: u64,
    full: Arc<FmtConfig>,
}

static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

fn stamp(p: &Path) -> Stamp {
    std::fs::metadata(p).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
}

fn global_config() -> Option<PathBuf> {
    let c = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(c.join("clojure-lsp/config.edn"))
}

/// (settings files, cljfmt config file) for the project.
fn config_files(root: Option<&Path>) -> (Vec<PathBuf>, Option<PathBuf>) {
    let mut settings: Vec<PathBuf> = global_config().into_iter().collect();
    let Some(root) = root else { return (settings, None) };
    let proj = root.join(".lsp/config.edn");
    settings.push(proj.clone());
    let path = read_settings(&settings).get("cljfmt-config-path").and_then(|v| v.text().map(String::from)).unwrap_or_else(|| ".cljfmt.edn".into());
    let f = if path.starts_with('/') { PathBuf::from(path) } else { root.join(path) };
    (settings, Some(f))
}

fn read_settings(files: &[PathBuf]) -> V {
    let mut acc = V::Map(vec![]);
    for f in files {
        if let Some(v @ V::Map(_)) = read_file(f) {
            acc = deep_merge(acc, v);
        }
    }
    acc
}

fn build_base(settings: &[PathBuf], cljfmt_file: Option<&Path>, style: Vec<(Key, Vec<Spec>)>) -> FmtConfig {
    let s = read_settings(settings);
    let mut user = s.get("cljfmt").cloned().unwrap_or(V::Map(vec![]));
    if let Some(v @ V::Map(_)) = cljfmt_file.and_then(read_file) {
        user = deep_merge(user, v);
    }
    let mut cfg = FmtConfig::default();
    // style-indent rules sit below the user config
    for (k, sp) in style {
        cfg.set_indent(k, sp);
    }
    apply_user(&mut cfg, &user);
    cfg
}

/// Effective config for the snapshot (cached; see module doc).
fn config(q: &Q) -> Arc<FmtConfig> {
    let root = q.s.project.as_ref().map(|p| p.root.clone());
    let (settings, cf) = config_files(root.as_deref());
    let stamps: Vec<Stamp> = settings.iter().chain(cf.iter()).map(|p| stamp(p)).collect();
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = g.as_ref().is_some_and(|c| c.root == root && c.stamps == stamps);
    if !fresh {
        let base = Arc::new(build_base(&settings, cf.as_deref(), Vec::new()));
        *g = Some(Cached { root, stamps, full: base.clone(), base, ver: u64::MAX });
    }
    let c = g.as_mut().unwrap();
    if c.ver != q.s.version {
        let st = style_indents(q);
        c.ver = q.s.version;
        if st.is_empty() {
            c.full = c.base.clone();
        } else {
            let (settings, cf) = config_files(c.root.as_deref());
            c.full = Arc::new(build_base(&settings, cf.as_deref(), st));
        }
    }
    c.full.clone()
}

// ---- answers ----

fn text_of(q: &Q, uri: &str) -> Option<Arc<str>> {
    if let Some(t) = q.s.get(uri).and_then(|e| e.text()) {
        return Some(t);
    }
    let p = crate::engine::scan::uri_to_path(uri)?;
    std::fs::read_to_string(p).ok().map(Arc::from)
}

fn edits_json(es: &[TextEdit]) -> String {
    let mut o = String::from("[");
    for (i, e) in es.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str(&format!(
            "{{\"range\":{{\"start\":{{\"line\":{},\"character\":{}}},\"end\":{{\"line\":{},\"character\":{}}}}},\"newText\":{}}}",
            e.start_line, e.start_col, e.end_line, e.end_col, json_str(&e.new_text)
        ));
    }
    o.push(']');
    o
}

pub fn formatting(q: &Q, uri: &str) -> String {
    let Some(t) = text_of(q, uri) else { return "[]".into() };
    let cfg = config(q);
    edits_json(&fmt::format_edits(&t, &cfg))
}

/// Aliases of the file's namespace (`q/namespace-aliases`): alias -> namespace over all files defining it.
fn ns_aliases(q: &Q, uri: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let Some(f) = q.s.id(uri) else { return m };
    let Some(fa) = q.s.file(f).and_then(|e| e.fa()) else { return m };
    let Some(nd) = fa.namespace_definitions.first() else { return m };
    for fid in q.s.ns_files_of(nd.name) {
        let Some(fa) = q.s.file(fid).and_then(|e| e.fa()) else { continue };
        for u in &fa.namespace_usages {
            if u.from == nd.name && !u.alias.is_none() {
                m.insert(u.alias.as_str().to_string(), u.to.as_str().to_string());
            }
        }
    }
    m
}

/// `nums` = [start line, start char, end line, end char] (0-based LSP).
pub fn range_formatting(q: &Q, uri: &str, nums: &[i64]) -> String {
    let Some(t) = text_of(q, uri) else { return "[]".into() };
    let n = |i: usize| nums.get(i).copied().unwrap_or(0).max(0) as u32 + 1;
    let base = config(q);
    let aliases = ns_aliases(q, uri);
    let es = if aliases.is_empty() {
        fmt::format_range_pos(&t, n(0), n(1), n(2), n(3), &base)
    } else {
        let mut cfg = (*base).clone();
        cfg.alias_map.extend(aliases);
        fmt::format_range_pos(&t, n(0), n(1), n(2), n(3), &cfg)
    };
    edits_json(&es)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_of(edn: &str) -> FmtConfig {
        let c = crate::reader::parse(edn);
        let f = c.sig_children(c.root()).next().unwrap();
        let mut cfg = FmtConfig::default();
        apply_user(&mut cfg, &conv(&c, f));
        cfg
    }

    #[test]
    fn user_indents_and_flags() {
        let cfg = cfg_of("{:extra-indents {foo [[:block 1]] my.ns/bar [[:inner 0]] #\"^baz\" [[:inner 0]]} :remove-trailing-whitespace? false}");
        assert!(!cfg.remove_trailing_whitespace);
        assert_eq!(fmt::format("(foo a\nb)", &cfg), "(foo a\n  b)");
        assert_eq!(fmt::format("(bazz a\nb)", &cfg), "(bazz a\n  b)");
        assert_eq!(fmt::format("(foo a\nb)", &FmtConfig::default()), "(foo a\n     b)");
    }

    #[test]
    fn style_indent_specs() {
        let s = |x: &str| style_indent_spec(&parse_j(x));
        assert_eq!(s("1"), Some(vec![Spec::Block(1)]));
        assert_eq!(s(":defn"), Some(vec![Spec::Inner(0, None)]));
        assert_eq!(s("[1 :form]"), Some(vec![Spec::Block(1)]));
        assert_eq!(s("[1 [1]]"), Some(vec![Spec::Block(1), Spec::Inner(1, None)]));
    }
}
