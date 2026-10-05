//! clojuredocs hover data (`feature/clojuredocs.clj`): the export is fetched in a background thread (never blocks a request),
//! cached at `$XDG_CACHE_HOME/nx/clojuredocs.edn`, and indexed as `"ns/name" -> examples / see-alsos / notes / doc`.
//! Setting `[:hover :clojuredocs]` (default true) disables it. Offline: the cached file (if any) is used, fetch retried after 60 s.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::sync::RwLock;

const URL: &str = "https://github.com/clojure-emacs/clojuredocs-export-edn/raw/master/exports/export.compact.edn";
const REFRESH_SECS: u64 = 24 * 3600;
const RETRY_SECS: u64 = 60;

#[derive(Default, Debug)]
pub struct Entry {
    pub doc: Option<String>,
    pub examples: Vec<String>,
    /// `(namespace, name)` of each see-also keyword.
    pub see_alsos: Vec<(String, String)>,
    pub notes: Vec<String>,
}

type Fetcher = fn(&str) -> Result<Vec<u8>, String>;
static FETCHER: OnceLock<Fetcher> = OnceLock::new();
static INDEX: RwLock<Option<Arc<HashMap<String, Entry>>>> = RwLock::new(None);
static ENABLED: AtomicBool = AtomicBool::new(true);
static BUSY: AtomicBool = AtomicBool::new(false);
static LAST_TRY: AtomicU64 = AtomicU64::new(0);

/// The HTTP GET of the host (body bytes); set once by the Mova binding.
pub fn set_fetcher(f: Fetcher) {
    let _ = FETCHER.set(f);
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn cache_file() -> PathBuf {
    crate::io::cache_root().join("clojuredocs.edn")
}

/// Setting from the config files (global then project) and the `initializationOptions` value; false disables.
pub fn start(root: Option<&std::path::Path>, init_opt: Option<bool>) {
    let mut en = true;
    let cfg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    let mut files = Vec::new();
    if let Some(c) = cfg {
        files.push(c.join("clojure-lsp/config.edn"));
    }
    if let Some(r) = root {
        files.push(r.join(".lsp/config.edn"));
    }
    for f in files {
        if let Some(e) = crate::io::edn::read_file(&f) {
            if let Some(crate::io::edn::Edn::Bool(b)) = e.get("hover").and_then(|h| h.get("clojuredocs")) {
                en = *b;
            }
        }
    }
    if let Some(b) = init_opt {
        en = b;
    }
    ENABLED.store(en, Ordering::Relaxed);
    if en {
        spawn();
    }
}

fn spawn() {
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    LAST_TRY.store(now(), Ordering::Relaxed);
    let r = std::thread::Builder::new().name("nx-clojuredocs".into()).spawn(|| {
        refresh();
        BUSY.store(false, Ordering::Release);
    });
    if r.is_err() {
        BUSY.store(false, Ordering::Release);
    }
}

fn refresh() {
    let file = cache_file();
    let age = std::fs::metadata(&file).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|d| d.as_secs());
    if INDEX.read().unwrap().is_none() {
        if let Ok(b) = std::fs::read(&file) {
            if let Some(ix) = parse(&b) {
                *INDEX.write().unwrap() = Some(Arc::new(ix));
            }
        }
    }
    if age.map_or(false, |a| a < REFRESH_SECS) && INDEX.read().unwrap().is_some() {
        return;
    }
    let Some(f) = FETCHER.get() else { return };
    let Ok(bytes) = f(URL) else { return };
    let Some(ix) = parse(&bytes) else { return };
    if let Some(d) = file.parent() {
        let _ = std::fs::create_dir_all(d);
        let tmp = d.join(format!(".clojuredocs-{}.tmp", std::process::id()));
        if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    *INDEX.write().unwrap() = Some(Arc::new(ix));
}

/// `find-docs-for`: entry of `ns/name`. Without an index yet, triggers (rate-limited) a background refresh and answers None.
pub fn lookup() -> Option<Arc<HashMap<String, Entry>>> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let g = INDEX.read().unwrap().clone();
    if g.is_none() && now().saturating_sub(LAST_TRY.load(Ordering::Relaxed)) >= RETRY_SECS {
        spawn();
    }
    g
}

pub fn find<'a>(ix: &'a HashMap<String, Entry>, ns: &str, name: &str) -> Option<&'a Entry> {
    let mut k = String::with_capacity(ns.len() + name.len() + 1);
    k.push_str(ns);
    k.push('/');
    k.push_str(name);
    ix.get(&k)
}

// ---- EDN subset reader (export.compact.edn: maps, vectors, strings, keywords, numbers, nil, booleans) ----

enum V {
    Nil,
    Str(String),
    Kw(String),
    Vec(Vec<V>),
    Map(Vec<(V, V)>),
    Other,
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && (self.b[self.i].is_ascii_whitespace() || self.b[self.i] == b',') {
            self.i += 1;
        }
    }
    fn val(&mut self, depth: u32) -> Option<V> {
        if depth > 64 {
            return None;
        }
        self.ws();
        let c = *self.b.get(self.i)?;
        match c {
            b'{' => {
                self.i += 1;
                let mut kv = Vec::new();
                loop {
                    self.ws();
                    if *self.b.get(self.i)? == b'}' {
                        self.i += 1;
                        return Some(V::Map(kv));
                    }
                    let k = self.val(depth + 1)?;
                    let v = self.val(depth + 1)?;
                    kv.push((k, v));
                }
            }
            b'[' | b'(' | b'#' => {
                let close = if c == b'(' { b')' } else { b']' };
                self.i += 1;
                if c == b'#' {
                    // `#{...}` set
                    if *self.b.get(self.i)? != b'{' {
                        return None;
                    }
                    self.i += 1;
                }
                let close = if c == b'#' { b'}' } else { close };
                let mut v = Vec::new();
                loop {
                    self.ws();
                    if *self.b.get(self.i)? == close {
                        self.i += 1;
                        return Some(V::Vec(v));
                    }
                    v.push(self.val(depth + 1)?);
                }
            }
            b'"' => {
                self.i += 1;
                let mut o: Vec<u8> = Vec::new();
                loop {
                    let c = *self.b.get(self.i)?;
                    self.i += 1;
                    match c {
                        b'"' => return Some(V::Str(String::from_utf8_lossy(&o).into_owned())),
                        b'\\' => {
                            let e = *self.b.get(self.i)?;
                            self.i += 1;
                            match e {
                                b'n' => o.push(b'\n'),
                                b't' => o.push(b'\t'),
                                b'r' => o.push(b'\r'),
                                b'f' => o.push(0x0c),
                                b'b' => o.push(0x08),
                                b'u' => {
                                    let h = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
                                    self.i += 4;
                                    let mut cp = u32::from_str_radix(h, 16).ok()?;
                                    if (0xD800..0xDC00).contains(&cp) && self.b.get(self.i..self.i + 2) == Some(b"\\u") {
                                        if let Some(lo) = std::str::from_utf8(self.b.get(self.i + 2..self.i + 6)?).ok().and_then(|h| u32::from_str_radix(h, 16).ok()) {
                                            if (0xDC00..0xE000).contains(&lo) {
                                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                                self.i += 6;
                                            }
                                        }
                                    }
                                    let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                                    let mut buf = [0u8; 4];
                                    o.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                }
                                other => o.push(other),
                            }
                        }
                        _ => o.push(c),
                    }
                }
            }
            _ => {
                let s = self.i;
                while self.i < self.b.len() && !matches!(self.b[self.i], b' ' | b'\n' | b'\t' | b'\r' | b',' | b'}' | b']' | b')' | b'{' | b'[' | b'(' | b'"') {
                    self.i += 1;
                }
                if self.i == s {
                    return None;
                }
                let t = std::str::from_utf8(&self.b[s..self.i]).ok()?;
                Some(if t == "nil" {
                    V::Nil
                } else if let Some(k) = t.strip_prefix(':') {
                    V::Kw(k.to_string())
                } else {
                    V::Other
                })
            }
        }
    }
}

fn get<'a>(m: &'a [(V, V)], key: &str) -> Option<&'a V> {
    m.iter().find(|(k, _)| matches!(k, V::Kw(s) if s == key)).map(|(_, v)| v)
}

fn strs(v: Option<&V>) -> Vec<String> {
    match v {
        Some(V::Vec(x)) => x.iter().filter_map(|e| if let V::Str(s) = e { Some(s.clone()) } else { None }).collect(),
        _ => Vec::new(),
    }
}

/// Parse the export into the index; None when it is not the expected shape.
pub fn parse(bytes: &[u8]) -> Option<HashMap<String, Entry>> {
    let mut p = P { b: bytes, i: 0 };
    let V::Map(top) = p.val(0)? else { return None };
    let mut ix = HashMap::with_capacity(top.len());
    for (k, v) in top {
        let (V::Kw(k), V::Map(m)) = (k, v) else { continue };
        let see_alsos = match get(&m, "see-alsos") {
            Some(V::Vec(x)) => x
                .iter()
                .filter_map(|e| if let V::Kw(s) = e { Some(s.rsplit_once('/').filter(|(n, _)| !n.is_empty()).map_or((String::new(), s.clone()), |(n, a)| (n.to_string(), a.to_string()))) } else { None })
                .collect(),
            _ => Vec::new(),
        };
        let doc = match get(&m, "doc") {
            Some(V::Str(s)) => Some(s.clone()),
            _ => None,
        };
        ix.insert(k, Entry { doc, examples: strs(get(&m, "examples")), see_alsos, notes: strs(get(&m, "notes")) });
    }
    Some(ix)
}

/// `clojuredocs->hover-docs`: sections joined by a blank line.
pub fn hover_docs(e: &Entry, doc_line: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = doc_line {
        parts.push(d.to_string());
    } else if let Some(d) = &e.doc {
        parts.push(d.clone());
    }
    if !e.examples.is_empty() {
        parts.push("__Examples:__".into());
        parts.push(e.examples.iter().map(|x| format!("```clojure\n{x}\n```")).collect::<Vec<_>>().join("\n---\n"));
    }
    if !e.see_alsos.is_empty() {
        parts.push("__See also:__".into());
        parts.push(e.see_alsos.iter().map(|(ns, n)| format!("[{ns}/{n}](https://clojuredocs.org/{ns}/{n})")).collect::<Vec<_>>().join("\n\n"));
    }
    if !e.notes.is_empty() {
        parts.push("__Notes:__".into());
        parts.push(e.notes.join("\n---\n"));
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses() {
        let ix = parse(r#"{:a/b {:doc "d\né", :examples ["x" "y"], :see-alsos [:a/c], :notes nil, :line 3}}"#.as_bytes()).unwrap();
        let e = find(&ix, "a", "b").unwrap();
        assert_eq!(e.doc.as_deref(), Some("d\n\u{e9}"));
        assert_eq!(hover_docs(e, Some("L")), "L\n\n__Examples:__\n\n```clojure\nx\n```\n---\n```clojure\ny\n```\n\n__See also:__\n\n[a/c](https://clojuredocs.org/a/c)");
    }
}

#[cfg(test)]
mod bench {
    #[test]
    #[ignore]
    fn parse_time() {
        let b = std::fs::read(std::env::var("CD_FILE").unwrap()).unwrap();
        let t = std::time::Instant::now();
        let ix = super::parse(&b).unwrap();
        eprintln!("parse {:?} entries {}", t.elapsed(), ix.len());
    }
}
