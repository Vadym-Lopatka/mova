//! `clojure/workspace/projectTree/nodes`, `clojure/serverInfo`, `clojure/cursorInfo` (feature/project_tree.clj,
//! feature/development_info.clj) and the log text of the `server-info` / `cursor-info` commands.
use super::preds::file_text;
use crate::analyzer::json::{parse as jparse, Json};
use crate::query::symbols::doc_var_defs;
use crate::query::{json_str, range_json, Q};

fn id_enum(kind: u8) -> u8 {
    // LSP SymbolKind -> projectTree type enum (:class 6 :function 7 :variable 8 :interface 9)
    match kind {
        12 => 7,
        11 => 9,
        5 => 6,
        _ => 8,
    }
}

fn rel_name(root: &std::path::Path, p: &str) -> String {
    let rp = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let rs = rp.to_string_lossy();
    match p.strip_prefix(rs.as_ref()) {
        Some(r) => r.trim_start_matches('/').to_string(),
        None => p.to_string(),
    }
}

fn node_json(name: &str, uri: Option<&str>, fin: bool, ty: u8) -> String {
    let mut s = format!("{{\"name\":{}", json_str(name));
    if let Some(u) = uri {
        s.push_str(&format!(",\"uri\":{}", json_str(u)));
    }
    s.push_str(&format!(",\"final\":{fin},\"type\":{ty}}}"));
    s
}

fn jar_uris(q: &Q) -> Vec<String> {
    let mut v: Vec<String> = match q.s.jars.as_ref() {
        Some(jv) => {
            let scheme_jar = jv.files.first().map_or(false, |f| f.uri.starts_with("jar:"));
            if !scheme_jar {
                return vec![];
            }
            jv.layer.jars.iter().map(|j| format!("jar:file://{}", j.path)).collect()
        }
        None => vec![],
    };
    v.sort();
    v.dedup();
    v
}

/// JVM `:source-paths` setting: project source paths plus the classpath dirs inside the project (existing or not).
pub fn jvm_source_paths(proj: &crate::engine::scan::ProjectInfo) -> Vec<String> {
    let mut sps = proj.source_paths.clone();
    let st = crate::io::project::Settings::load(&proj.root);
    for d in crate::io::classpath::resolve(&proj.root, &st).classpath.dirs {
        let abs = if d.starts_with('/') { std::path::PathBuf::from(&d) } else { proj.root.join(&d) };
        let abs = abs.to_string_lossy().into_owned();
        if abs.starts_with(&*proj.root.to_string_lossy()) && !sps.iter().any(|x| *x == abs) {
            sps.push(abs);
        }
    }
    sps
}

/// `clojure/workspace/projectTree/nodes`: `params` is the clicked node (JSON) or null for the root.
pub fn project_tree(q: &Q, params: &str) -> String {
    let node = jparse(params).unwrap_or(Json::Null);
    let Some(proj) = q.s.project.as_ref() else { return "null".into() };
    let get_s = |k: &str| node.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let ty = node.get("type").and_then(|v| v.as_f64()).map(|n| n as i64);
    match ty {
        None => {
            let name = proj.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut sps = jvm_source_paths(proj);
            sps.sort();
            let mut nodes: Vec<String> = sps.iter().map(|p| node_json(&rel_name(&proj.root, p), None, false, 2)).collect();
            nodes.push("{\"name\":\"External dependencies\",\"id\":\"external-dependencies\",\"final\":false,\"type\":3}".into());
            format!("{{\"name\":{},\"type\":1,\"nodes\":[{}]}}", json_str(&name), nodes.join(","))
        }
        Some(2) => {
            let name = get_s("name").unwrap_or_default();
            let abs = proj.root.join(&name);
            let uri = crate::engine::scan::path_to_uri(&abs);
            let mut ns: Vec<(String, String)> = Vec::new();
            for u in q.s.uris() {
                let Some(f) = q.s.id(u) else { continue };
                if !q.internal(f) || !u.starts_with(&uri) {
                    continue;
                }
                if let Some(fa) = q.entry(f).fa() {
                    for n in &fa.namespace_definitions {
                        ns.push((n.name.as_str().to_string(), u.to_string()));
                    }
                }
            }
            ns.sort();
            let nodes: Vec<String> = ns.iter().map(|(n, u)| node_json(n, Some(u), false, 5)).collect();
            format!("{{\"name\":{},\"type\":2,\"nodes\":[{}]}}", json_str(&name), nodes.join(","))
        }
        Some(3) => {
            let jars = jar_uris(q);
            let nodes: Vec<String> = jars
                .iter()
                .map(|j| {
                    let nm = j.rsplit('/').next().unwrap_or(j);
                    format!("{{\"name\":{},\"detail\":{},\"uri\":{},\"final\":false,\"type\":4}}", json_str(nm), json_str(j), json_str(j))
                })
                .collect();
            format!(
                "{{\"name\":{},\"type\":3,\"id\":\"external-dependencies\",\"nodes\":[{}]}}",
                json_str(&get_s("name").unwrap_or_default()),
                nodes.join(",")
            )
        }
        Some(4) => {
            let uri = get_s("uri").unwrap_or_default();
            let mut ns: Vec<(String, String)> = Vec::new();
            if let Some(jv) = q.s.jars.as_ref() {
                for f in jv.files.iter().filter(|f| f.uri.starts_with(&uri) && f.uri[uri.len()..].starts_with("!/")) {
                    if let Some(fa) = f.fa() {
                        for n in &fa.namespace_definitions {
                            ns.push((n.name.as_str().to_string(), f.uri.to_string()));
                        }
                    }
                }
            }
            let nodes: Vec<String> = ns.iter().map(|(n, u)| node_json(n, Some(u), false, 5)).collect();
            format!(
                "{{\"name\":{},\"type\":4,\"detail\":{},\"uri\":{},\"nodes\":[{}]}}",
                json_str(&get_s("name").unwrap_or_default()),
                json_str(&get_s("detail").unwrap_or_default()),
                json_str(&uri),
                nodes.join(",")
            )
        }
        Some(5) => {
            let uri = get_s("uri").unwrap_or_default();
            let mut nodes: Vec<String> = Vec::new();
            if let Some(f) = q.s.id(&uri) {
                if let Some(fa) = q.entry(f).fa() {
                    for i in doc_var_defs(fa, true) {
                        let d = &fa.var_definitions[i];
                        let el = crate::query::El { f, b: crate::engine::index::B::VarDef, i: i as u32 };
                        let kind = id_enum(q.symbol_kind(el));
                        let mut s = format!(
                            "{{\"name\":{},\"uri\":{},\"range\":{},\"final\":true,\"type\":{kind}",
                            json_str(d.name.as_str()),
                            json_str(&uri),
                            range_json(d.name_pos)
                        );
                        if d.private {
                            s.push_str(",\"detail\":\"private\"");
                        }
                        s.push('}');
                        nodes.push(s);
                    }
                    let mut seen = std::collections::HashSet::new();
                    for k in &fa.keywords {
                        if k.reg.is_none() || !seen.insert((k.ns.0, k.name.0, k.pos.row, k.pos.col)) {
                            continue;
                        }
                        let reg = k.reg.as_str();
                        let detail = reg.rsplit('/').next().unwrap_or(reg);
                        nodes.push(format!(
                            "{{\"name\":{},\"uri\":{},\"range\":{},\"final\":true,\"type\":7,\"detail\":{}}}",
                            json_str(k.name.as_str()),
                            json_str(&uri),
                            range_json(k.pos),
                            json_str(detail)
                        ));
                    }
                }
            }
            format!(
                "{{\"name\":{},\"type\":5,\"uri\":{},\"nodes\":[{}]}}",
                json_str(&get_s("name").unwrap_or_default()),
                json_str(&uri),
                nodes.join(",")
            )
        }
        _ => "null".into(),
    }
}

fn jstr(v: &Json) -> String {
    let mut s = String::new();
    v.write(&mut s);
    s
}

/// `clojure/serverInfo/raw` (feature/development_info.clj `server-info`): nx values for version / log-path / port.
pub fn server_info_json(q: &Q, init: Option<&Json>) -> String {
    let Some(proj) = q.s.project.as_ref() else { return "null".into() };
    let root = &proj.root;
    let st = crate::io::project::Settings::load(root);
    let cp = crate::io::classpath::resolve(root, &st).classpath;
    let project_settings = super::settings::read_edn_file(&root.join(".lsp/config.edn")).unwrap_or(Json::Obj(vec![]));
    // client-settings: `clean-client-settings` defaults over the initializationOptions
    let mut client: Vec<(String, Json)> = match init {
        Some(Json::Obj(o)) => o.clone(),
        _ => vec![],
    };
    let mut put = |k: &str, v: Json, keep: bool| match client.iter().position(|(a, _)| a == k) {
        Some(i) => {
            if !keep {
                client[i].1 = v
            }
        }
        None => client.push((k.to_string(), v)),
    };
    put("dependency-scheme", Json::Str("zipfile".into()), true);
    put("text-document-sync-kind", Json::Null, true);
    put("source-paths", Json::Null, true);
    put("source-aliases", Json::Null, true);
    put("cljfmt-config-path", Json::Str(".cljfmt.edn".into()), true);
    put("document-formatting?", Json::Bool(true), true);
    put("document-range-formatting?", Json::Bool(true), true);
    let client = Json::Obj(client);
    let sps: Vec<Json> = jvm_source_paths(proj).into_iter().map(Json::Str).collect();
    let dep_scheme = client.get("dependency-scheme").cloned().unwrap_or(Json::Null);
    let final_settings = Json::Obj(vec![
        ("document-formatting?".into(), Json::Bool(true)),
        ("dependency-scheme".into(), dep_scheme),
        ("source-paths".into(), Json::Arr(sps)),
        ("text-document-sync-kind".into(), Json::Null),
        ("source-aliases".into(), Json::Arr(vec![Json::Str("test".into()), Json::Str("dev".into())])),
        ("uri-format".into(), Json::Obj(vec![("upper-case-drive-letter?".into(), Json::Bool(false)), ("encode-colons-in-path?".into(), Json::Bool(false))])),
        ("document-range-formatting?".into(), Json::Bool(true)),
        ("cljfmt-config-path".into(), Json::Str(".cljfmt.edn".into())),
    ]);
    let classpath: Vec<Json> = cp.jars.iter().chain(cp.dirs.iter()).cloned().map(Json::Str).collect();
    // analysis-summary: internal buckets counted over the project files
    let mut c = [0u64; 11];
    for u in q.s.uris() {
        let Some(f) = q.s.id(u) else { continue };
        if !q.internal(f) {
            continue;
        }
        let Some(fa) = q.entry(f).fa() else { continue };
        c[0] += fa.keywords.iter().filter(|k| k.reg.is_none()).count() as u64;
        c[1] += fa.protocol_impls.len() as u64;
        c[2] += fa.local_usages.len() as u64;
        c[3] += fa.namespace_usages.iter().filter(|n| n.alias_pos.row != 0).count() as u64;
        c[4] += fa.namespace_usages.len() as u64;
        c[5] += fa.var_usages.iter().filter(|u| u.to != crate::analyzer::syms().unknown_ns).count() as u64;
        c[6] += fa.namespace_definitions.len() as u64;
        c[7] += fa.java_class_usages.len() as u64;
        c[8] += fa.locals.len() as u64;
        c[9] += fa.var_definitions.len() as u64;
        c[10] += fa.instance_invocations.len() as u64;
    }
    let names = ["keyword-usages", "protocol-impls", "local-usages", "namespace-alias", "namespace-usages", "var-usages", "namespace-definitions", "java-class-usages", "locals", "var-definitions", "instance-invocations"];
    let internal = Json::Obj(names.iter().zip(c.iter()).filter(|(_, v)| **v > 0).map(|(n, v)| (n.to_string(), Json::Num(*v as f64))).collect());
    let summary = Json::Obj(vec![("internal".into(), internal), ("external".into(), Json::Obj(["java-class-definitions", "java-member-definitions", "keyword-definitions", "namespace-alias", "namespace-definitions", "namespace-usages", "protocol-impls", "var-definitions"].iter().map(|k| (k.to_string(), Json::Num(0.0))).collect()))]);
    let out = Json::Obj(vec![
        ("log-path".into(), Json::Str(String::new())),
        ("project-settings".into(), project_settings),
        ("classpath".into(), Json::Arr(classpath)),
        ("project-root-uri".into(), Json::Str(crate::engine::scan::path_to_uri(root))),
        ("analysis-summary".into(), summary),
        ("client-settings".into(), client),
        ("clj-kondo-version".into(), Json::Str("nx".into())),
        ("server-version".into(), Json::Str("nx".into())),
        ("port".into(), Json::Str("NREPL only available on :debug profile (`bb debug-cli`)".into())),
        ("final-settings".into(), final_settings),
        ("classpath-settings".into(), Json::Null),
        ("cljfmt-raw".into(), Json::Str("{}".into())),
    ]);
    jstr(&out)
}

/// Text of the `server-info` command message (JVM pprints the same map).
pub fn server_info_text(q: &Q, init: Option<&Json>) -> String {
    server_info_json(q, init)
}

/// Text of the `cursor-info` command message.
pub fn cursor_info_text(q: &Q, uri: &str, row: u32, col: u32) -> String {
    cursor_info_json(q, uri, row, col)
}

/// rewrite-clj node record of the token at the cursor (JVM `(dissoc node :children)`); None for nodes with children
/// (the JVM cannot encode those and never answers).
fn node_record(q: &Q, uri: &str, row: u32, col: u32) -> Option<String> {
    use super::tree::{find_at_pos, Tag, Tk, Tree};
    let f = q.s.id(uri)?;
    let text = file_text(q, f)?;
    let tree = Tree::parse(&text);
    if tree.err {
        return None;
    }
    let z = find_at_pos(&tree, row, col)?;
    match z.tag() {
        Tag::Token => {
            let t = z.text();
            match z.tk() {
                Tk::Sym => Some(format!("{{\"map-qualifier\":null,\"string-value\":{},\"value\":{}}}", json_str(t), json_str(t))),
                Tk::Kw | Tk::KwAuto => {
                    let auto = t.starts_with("::");
                    let k = t.trim_start_matches(':');
                    Some(format!("{{\"auto-resolved?\":{auto},\"k\":{},\"map-qualifier\":null}}", json_str(k)))
                }
                Tk::Num => Some(format!("{{\"string-value\":{},\"value\":{}}}", json_str(t), t)),
                Tk::Const => {
                    let v = if t == "nil" { "null" } else { t };
                    Some(format!("{{\"string-value\":{},\"value\":{}}}", json_str(t), v))
                }
                _ => Some(format!("{{\"string-value\":{},\"value\":{}}}", json_str(t), json_str(t))),
            }
        }
        Tag::Whitespace => Some(format!("{{\"whitespace\":{}}}", json_str(z.text()))),
        Tag::Newline => Some(format!("{{\"newlines\":{}}}", json_str(z.text()))),
        _ => None,
    }
}

fn bucket_of(q: &Q, e: crate::query::El) -> Option<(&'static str, usize)> {
    use crate::engine::index::B;
    let i = e.i as usize;
    Some(match e.b {
        B::NsDef => ("namespace-definitions", i),
        B::NsUsage => ("namespace-usages", i),
        B::NsAlias => ("namespace-usages", i),
        B::VarDef => ("var-definitions", i),
        B::VarUsage => ("var-usages", i),
        B::Local => ("locals", i),
        B::LocalUsage => ("local-usages", i),
        B::KwDef | B::KwUsage => ("keywords", i),
        B::Symbols => ("symbols", i),
        B::ProtoImpl => ("protocol-impls", i),
        B::JavaClassUsage => ("java-class-usages", i),
        B::JavaClassDef => ("java-class-definitions", i),
        B::InstInv => ("instance-invocations", i),
        B::JavaMemberDef => return None,
    })
    .filter(|_| q.entry(e.f).fa().is_some())
}

/// Clojure `hash` of a long (Murmur3.hashLong).
fn hash_long(n: i64) -> u32 {
    fn mix_k1(k: u32) -> u32 {
        k.wrapping_mul(0xcc9e2d51).rotate_left(15).wrapping_mul(0x1b873593)
    }
    fn mix_h1(h: u32, k: u32) -> u32 {
        (h ^ k).rotate_left(13).wrapping_mul(5).wrapping_add(0xe6546b64)
    }
    if n == 0 {
        return 0;
    }
    let (low, high) = (n as u32, ((n as u64) >> 32) as u32);
    let mut h = mix_h1(0, mix_k1(low));
    h = mix_h1(h, mix_k1(high));
    h ^= 8;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85ebca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2ae35);
    h ^ (h >> 16)
}

/// Iteration order of a Clojure hash-set of small ints (HAMT: 5-bit chunks of the hash, low chunk first).
fn hamt_order(v: &mut [i64]) {
    v.sort_by_key(|&n| {
        let h = hash_long(n);
        (0..7).map(|i| (h >> (5 * i)) & 31).collect::<Vec<u32>>()
    });
}

/// The kondo-style element map clojure-lsp holds for `e` (oracle emitter + `:bucket`/`:uri`/`:external?`).
fn element_json(q: &Q, e: crate::query::El, cache: &mut std::collections::HashMap<u32, Json>) -> Option<Json> {
    use crate::engine::index::B;
    let (bname, idx) = bucket_of(q, e)?;
    let doc = match cache.get(&e.f) {
        Some(d) => d,
        None => {
            let fa = q.fa(e.f);
            let uri = q.uri(e.f);
            let lang = if uri.ends_with(".cljs") { "cljs" } else if uri.ends_with(".cljc") { "cljc" } else { "clj" };
            let d = jparse(&crate::analyzer::emit::to_json(uri, lang, fa))?;
            cache.insert(e.f, d);
            cache.get(&e.f)?
        }
    };
    let item = doc.get("analysis")?.get(bname)?.as_arr()?.get(idx)?;
    let Json::Obj(fields) = item else { return None };
    let mut out: Vec<(String, Json)> = fields.iter().filter(|(k, v)| !matches!(v, Json::Null) || k == "derived-location" || k == "call" || k.starts_with("alias-")).cloned().collect();
    if matches!(e.b, B::NsUsage | B::NsAlias) {
        // the usage keeps `:name`; kondo's `:to` is renamed by clojure-lsp
        for (k, _) in out.iter_mut() {
            if k == "to" {
                *k = "name".into();
            }
        }
    }
    for (k, v) in out.iter_mut() {
        if k == "fixed-arities" {
            if let Json::Arr(a) = v {
                let mut ns: Vec<i64> = a.iter().filter_map(|x| x.as_f64()).map(|x| x as i64).collect();
                hamt_order(&mut ns);
                *a = ns.into_iter().map(|n| Json::Num(n as f64)).collect();
            }
        }
    }
    let has = |o: &Vec<(String, Json)>, k: &str| o.iter().any(|(a, _)| a == k);
    let bucket = match e.b {
        B::KwDef | B::KwUsage => {
            if q.fa(e.f).keywords[idx].reg.is_none() { "keyword-usages" } else { "keyword-definitions" }
        }
        B::NsAlias => "namespace-alias",
        _ => bname,
    };
    if e.b == B::NsAlias {
        // alias element: alias position as name, `:to` instead of `:name`
        let get = |o: &Vec<(String, Json)>, k: &str| o.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        let mut al: Vec<(String, Json)> = vec![];
        for (k, src) in [("alias", "alias"), ("from", "from"), ("to", "name")] {
            if let Some(v) = get(&out, src) {
                al.push((k.into(), v));
            }
        }
        for (k, src) in [("name-row", "alias-row"), ("name-col", "alias-col"), ("name-end-row", "alias-end-row"), ("name-end-col", "alias-end-col"), ("row", "row"), ("col", "col")] {
            if let Some(v) = get(&out, src) {
                al.push((k.into(), v));
            }
        }
        out = al;
    } else if !has(&out, "name-row") {
        for (k, src) in [("name-row", "row"), ("name-col", "col"), ("name-end-row", "end-row"), ("name-end-col", "end-col")] {
            if let Some(v) = out.iter().find(|(a, _)| a == src).map(|(_, v)| v.clone()) {
                out.push((k.into(), v));
            }
        }
    }
    out.push(("bucket".into(), Json::Str(bucket.into())));
    out.push(("uri".into(), Json::Str(q.uri(e.f).to_string())));
    out.push(("external?".into(), Json::Bool(!q.internal(e.f))));
    Some(Json::Obj(out))
}

/// `clojure/cursorInfo/raw`.
pub fn cursor_info_json(q: &Q, uri: &str, row: u32, col: u32) -> String {
    let Some(node) = node_record(q, uri, row, col) else { return "null".into() };
    let mut cache = std::collections::HashMap::new();
    let mut els: Vec<String> = Vec::new();
    for e in q.under_cursor(uri, row, col) {
        let Some(el) = element_json(q, e, &mut cache) else { continue };
        let mut s = format!("{{\"element\":{}", jstr(&el));
        if let Some(h) = crate::query::jdk::hit(q, e, false) {
            if h.name.is_none() {
                let cls = q.fa(e.f).java_class_usages[e.i as usize].class.as_str();
                s.push_str(&format!(",\"definition\":{{\"bucket\":\"java-class-definitions\",\"class\":{},\"external?\":true,\"flags\":[\"public\"],\"uri\":{}}}", json_str(cls), json_str(&h.uri)));
            }
        } else if let Some(d) = q.find_definition(e) {
            if let Some(dj) = element_json(q, d, &mut cache) {
                s.push_str(&format!(",\"definition\":{}", jstr(&dj)));
            }
        }
        let toks: Vec<String> = q.element_token_types(e).into_iter().map(|(t, m)| format!("{{\"token-type\":{},\"token-modifier\":{}}}", json_str(t), m)).collect();
        s.push_str(&format!(",\"semantic-tokens\":[{}]}}", toks.join(",")));
        els.push(s);
    }
    format!("{{\"node\":{node},\"elements\":[{}]}}", els.join(","))
}

#[allow(dead_code)]
fn _u(q: &Q, f: crate::engine::store::FileId) -> Option<std::sync::Arc<str>> {
    file_text(q, f)
}

// ---- clojure/clojuredocs/raw ---------------------------------------------------------------------------------

fn edn_skip(b: &[u8], i: &mut usize) {
    while *i < b.len() && (b[*i].is_ascii_whitespace() || b[*i] == b',') {
        *i += 1;
    }
}

fn camel(k: &str) -> String {
    let mut out = String::new();
    let mut up = false;
    for c in k.chars() {
        if c == '-' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// EDN value at `*i` as JSON text (keywords become strings; map keys camelCased as lsp4clj does).
fn edn_json(b: &[u8], i: &mut usize, depth: u32) -> Option<String> {
    if depth > 64 {
        return None;
    }
    edn_skip(b, i);
    let c = *b.get(*i)?;
    match c {
        b'{' => {
            *i += 1;
            let mut parts = vec![];
            loop {
                edn_skip(b, i);
                if *b.get(*i)? == b'}' {
                    *i += 1;
                    return Some(format!("{{{}}}", parts.join(",")));
                }
                let k = edn_json(b, i, depth + 1)?;
                let v = edn_json(b, i, depth + 1)?;
                // lsp4clj camelCases the keys of the response
                let k = json_str(&camel(k.trim_matches('"')));
                parts.push(format!("{}:{}", k, v));
            }
        }
        b'[' | b'(' => {
            let close = if c == b'[' { b']' } else { b')' };
            *i += 1;
            let mut parts = vec![];
            loop {
                edn_skip(b, i);
                if *b.get(*i)? == close {
                    *i += 1;
                    return Some(format!("[{}]", parts.join(",")));
                }
                parts.push(edn_json(b, i, depth + 1)?);
            }
        }
        b'"' => {
            *i += 1;
            let mut o: Vec<u8> = vec![];
            loop {
                let ch = *b.get(*i)?;
                *i += 1;
                match ch {
                    b'"' => return Some(json_str(&String::from_utf8_lossy(&o))),
                    b'\\' => {
                        let e = *b.get(*i)?;
                        *i += 1;
                        match e {
                            b'n' => o.push(b'\n'),
                            b't' => o.push(b'\t'),
                            b'r' => o.push(b'\r'),
                            b'f' => o.push(0x0c),
                            b'b' => o.push(0x08),
                            b'u' => {
                                let h = std::str::from_utf8(b.get(*i..*i + 4)?).ok()?;
                                *i += 4;
                                let ch = char::from_u32(u32::from_str_radix(h, 16).ok()?).unwrap_or('\u{FFFD}');
                                let mut buf = [0u8; 4];
                                o.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                            other => o.push(other),
                        }
                    }
                    _ => o.push(ch),
                }
            }
        }
        _ => {
            let s = *i;
            while *i < b.len() && !matches!(b[*i], b' ' | b'\n' | b'\t' | b'\r' | b',' | b'}' | b']' | b')' | b'{' | b'[' | b'(' | b'"') {
                *i += 1;
            }
            let t = std::str::from_utf8(&b[s..*i]).ok()?;
            Some(match t {
                "nil" => "null".into(),
                "true" | "false" => t.into(),
                _ => match t.strip_prefix(':') {
                    Some(k) => json_str(k),
                    None => t.to_string(),
                },
            })
        }
    }
}

/// `clojure/clojuredocs/raw`: the export entry of `ns/name` (the JVM answers nil until its cache is filled).
pub fn clojuredocs_raw(params: &str) -> String {
    let Some(p) = jparse(params) else { return "null".into() };
    let (Some(name), Some(ns)) = (p.get("sym-name").and_then(|v| v.as_str()), p.get("sym-ns").and_then(|v| v.as_str())) else { return "null".into() };
    let Ok(bytes) = std::fs::read(crate::io::cache_root().join("clojuredocs.edn")) else { return "null".into() };
    let key = format!(":{ns}/{name}");
    let b = &bytes[..];
    let mut i = 0;
    edn_skip(b, &mut i);
    if b.get(i) != Some(&b'{') {
        return "null".into();
    }
    i += 1;
    loop {
        edn_skip(b, &mut i);
        if b.get(i) != Some(&b':') {
            return "null".into();
        }
        let s = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b',' {
            i += 1;
        }
        let k = &b[s..i];
        if k == key.as_bytes() {
            return edn_json(b, &mut i, 0).unwrap_or_else(|| "null".into());
        }
        // skip the value
        let mut j = i;
        if edn_json(b, &mut j, 0).is_none() {
            return "null".into();
        }
        i = j;
    }
}
