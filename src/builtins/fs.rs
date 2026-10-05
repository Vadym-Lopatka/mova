//! `mova.fs` -- lsp/kondo (clojure-lsp-on-Mova campaign, mova/PLAN.md "reuse
//! Rust crates"): replaces the interpreted `babashka.fs` shim's per-call
//! recursive `.list`/`.isDirectory` walk (`mova/shims/babashka/fs.mova`'s old
//! `list-rel-paths`/`glob->regex`) with `walkdir` (traversal: depth/hidden/
//! follow-links) + `globset` (glob->matcher compilation), the same crate
//! family ripgrep uses. The interpreted walk made `cli_smoke` take minutes
//! (every directory/file stat went through the evaluator); this walks in
//! native code and only crosses back into Mova values for the final path
//! list.
//!
//! `**`-anywhere: babashka.fs (and clojure-lsp's own `clj-file-regex`,
//! `"**.{edn,clj,cljs,cljc,bb,cljd,clj_kondo}"`) allows a bare `**` as a
//! SEGMENT PREFIX (not only the whole-segment `**/`/`/**`/`/**/` forms
//! `globset::Glob` accepts -- see that crate's syntax doc, which calls any
//! other placement of `**` illegal). [`fixup_leading_doublestar`] rewrites
//! a pattern whose first segment starts with `**` followed by more
//! characters into `**/` + `*` + those characters, which IS a legal
//! `Glob` (starts with `**/`) with the same "cross directories, then match
//! this suffix within the last segment" meaning the old shim's regex
//! (`**` -> `.*`, unconditionally) gave that one case.

use std::path::Path;
use std::sync::Arc;

use globset::GlobBuilder;
use walkdir::WalkDir;

use crate::builtins::fileio::expect_path;
use crate::builtins::ArityHint;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{NativeFn, Symbol, Value};

/// Exact duplicate of `builtins::strings::reg_ns` -- private to its own
/// module, same reason every other `mova.*` native module keeps its own
/// copy (see that fn's doc).
#[track_caller]
fn reg_ns(
    i: &mut Interp,
    ns: &'static str,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let native = NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            return Err(RjError::arity(format!(
                "{name}: expected {}, got {}",
                arity.expected_desc(),
                args.len()
            ))
            .with_stack(interp.stack_snapshot(), interp.source_id));
        }
        f(interp, args)
    });
    i.globals.set_builtin(
        Symbol {
            ns: Some(ns.into()),
            name: name.into(),
        },
        Value::Native(Arc::new(native)),
    );
}

pub fn register(i: &mut Interp) {
    reg_ns(i, "mova.fs", "glob", ArityHint::Range(2, 3), glob_native);
    reg_ns(i, "mova.fs", "list-dir", ArityHint::Exact(1), list_dir_native);
    reg_ns(i, "mova.fs", "walk", ArityHint::Range(1, 2), walk_native);
    reg_ns(i, "mova.fs", "canonicalize", ArityHint::Exact(1), canonicalize_native);
    reg_ns(i, "mova.fs", "real-path", ArityHint::Exact(1), canonicalize_native);
    reg_ns(i, "mova.fs", "exists?", ArityHint::Exact(1), exists_native);
    reg_ns(i, "mova.fs", "directory?", ArityHint::Exact(1), directory_native);
    reg_ns(i, "mova.fs", "regular-file?", ArityHint::Exact(1), regular_file_native);
    reg_ns(
        i,
        "mova.fs",
        "last-modified-time",
        ArityHint::Exact(1),
        last_modified_native,
    );
    reg_ns(i, "mova.fs", "unzip", ArityHint::Exact(2), unzip_native);
}

/// lsp/host: `babashka.fs/unzip` (mova/shims/babashka/fs.mova) -- extracts
/// a zip archive (clojure-lsp's JDK `src.zip`) to a directory. Reuses the
/// `zip` crate already vendored for the JarFile/ZipFile veneer
/// (hostclass.rs) instead of a hand-rolled deflate reader.
fn unzip_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let zip_path = expect_path(&args[0], "mova.fs/unzip")?;
    let dest = expect_path(&args[1], "mova.fs/unzip")?;
    let file = std::fs::File::open(zip_path.as_ref())
        .map_err(|e| RjError::sys(format!("mova.fs/unzip {zip_path:?}"), e.raw_os_error().unwrap_or(2), "open"))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| RjError::type_err(format!("mova.fs/unzip {zip_path:?}: {e}")))?;
    archive
        .extract(Path::new(dest.as_ref()))
        .map_err(|e| RjError::type_err(format!("mova.fs/unzip {zip_path:?} -> {dest:?}: {e}")))?;
    Ok(Value::Nil)
}

fn opt_bool(opts: Option<&Value>, key: &str, default: bool) -> bool {
    match opts {
        Some(Value::Map(m)) => match m.get(&Value::Keyword(key.into())) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Nil) | None => default,
            Some(_) => default,
        },
        _ => default,
    }
}

fn opt_usize(opts: Option<&Value>, key: &str, default: usize) -> usize {
    match opts {
        Some(Value::Map(m)) => match m.get(&Value::Keyword(key.into())) {
            Some(Value::Int(n)) if *n >= 0 => *n as usize,
            _ => default,
        },
        _ => default,
    }
}

/// Rewrite a segment-prefix `**` (e.g. `"**.{clj,cljc}"`) into a
/// `globset`-legal `**/`-prefixed pattern with the same "cross directories,
/// then match this suffix" meaning. A no-op for every already-legal form
/// (`**`, `**/...`, `.../**`, `.../**/...`) and for patterns with no `**`
/// at all.
fn fixup_leading_doublestar(pattern: &str) -> String {
    if let Some(rest) = pattern.strip_prefix("**") {
        if rest.is_empty() || rest.starts_with('/') {
            return pattern.to_string();
        }
        return format!("**/*{rest}");
    }
    pattern.to_string()
}

fn compile_matcher(pattern: &str) -> Result<globset::GlobMatcher, RjError> {
    let fixed = fixup_leading_doublestar(pattern);
    let glob = GlobBuilder::new(&fixed)
        .literal_separator(true)
        .build()
        .map_err(|e| RjError::type_err(format!("mova.fs/glob: bad pattern {pattern:?}: {e}")))?;
    Ok(glob.compile_matcher())
}

fn join(root: &str, rel: &str) -> String {
    if root.is_empty() {
        rel.to_string()
    } else if root.ends_with('/') {
        format!("{root}{rel}")
    } else {
        format!("{root}/{rel}")
    }
}

/// True if any path component (other than `root` itself) starts with `.`.
fn has_hidden_component(rel: &Path) -> bool {
    rel.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
    })
}

fn glob_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let root = expect_path(&args[0], "mova.fs/glob")?;
    let pattern = match &args[1] {
        Value::Str(s) => s.as_ref().to_string(),
        other => {
            return Err(RjError::type_err(format!(
                "mova.fs/glob: expected a string pattern, got {}",
                other.type_name()
            )))
        }
    };
    let opts = args.get(2);
    let hidden = opt_bool(opts, "hidden", pattern.starts_with('.'));
    let max_depth = opt_usize(opts, "max-depth", 10);
    let follow_links = opt_bool(opts, "follow-links", false);

    let matcher = compile_matcher(&pattern)?;
    let root_str = root.as_ref().to_string();
    let root_path = Path::new(&root_str);

    let mut out: Vec<Value> = Vec::new();
    let walker = WalkDir::new(root_path)
        .max_depth(max_depth)
        .follow_links(follow_links)
        .into_iter()
        .filter_entry(|e| {
            hidden
                || e.depth() == 0
                || !e
                    .file_name()
                    .to_str()
                    .map(|s| s.starts_with('.'))
                    .unwrap_or(false)
        });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(root_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !hidden && has_hidden_component(rel) {
            continue;
        }
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if matcher.is_match(&rel_str) {
            out.push(Value::Str(join(&root_str, &rel_str).into()));
        }
    }
    Ok(Value::Vector(out.into_iter().collect()))
}

fn list_dir_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let dir = expect_path(&args[0], "mova.fs/list-dir")?;
    let mut names: Vec<String> = std::fs::read_dir(dir.as_ref())
        .map_err(|e| RjError::sys(format!("mova.fs/list-dir {dir:?}"), e.raw_os_error().unwrap_or(2), "opendir"))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(Value::Vector(
        names
            .into_iter()
            .map(|n| Value::Str(join(dir.as_ref(), &n).into()))
            .collect(),
    ))
}

fn walk_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let root = expect_path(&args[0], "mova.fs/walk")?;
    let opts = args.get(1);
    let max_depth = opt_usize(opts, "max-depth", usize::MAX);
    let hidden = opt_bool(opts, "hidden", false);
    let root_str = root.as_ref().to_string();
    let root_path = Path::new(&root_str);

    let mut out: Vec<Value> = Vec::new();
    let walker = WalkDir::new(root_path)
        .max_depth(if max_depth == usize::MAX { usize::MAX } else { max_depth })
        .into_iter()
        .filter_entry(|e| {
            hidden
                || e.depth() == 0
                || !e
                    .file_name()
                    .to_str()
                    .map(|s| s.starts_with('.'))
                    .unwrap_or(false)
        });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        out.push(Value::Str(entry.path().to_string_lossy().into_owned().into()));
    }
    Ok(Value::Vector(out.into_iter().collect()))
}

fn canonicalize_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let path = expect_path(&args[0], "mova.fs/canonicalize")?;
    let canon = std::fs::canonicalize(path.as_ref())
        .map_err(|e| RjError::sys(format!("mova.fs/canonicalize {path:?}"), e.raw_os_error().unwrap_or(2), "realpath"))?;
    Ok(Value::Str(canon.to_string_lossy().into_owned().into()))
}

fn exists_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let path = expect_path(&args[0], "mova.fs/exists?")?;
    Ok(Value::Bool(Path::new(path.as_ref()).exists()))
}

fn directory_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let path = expect_path(&args[0], "mova.fs/directory?")?;
    Ok(Value::Bool(Path::new(path.as_ref()).is_dir()))
}

fn regular_file_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let path = expect_path(&args[0], "mova.fs/regular-file?")?;
    Ok(Value::Bool(Path::new(path.as_ref()).is_file()))
}

fn last_modified_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let path = expect_path(&args[0], "mova.fs/last-modified-time")?;
    let meta = std::fs::metadata(path.as_ref())
        .map_err(|e| RjError::sys(format!("mova.fs/last-modified-time {path:?}"), e.raw_os_error().unwrap_or(2), "stat"))?;
    let millis = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(Value::Int(millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, rel: &str) -> bool {
        compile_matcher(pattern).unwrap().is_match(rel)
    }

    #[test]
    fn leading_doublestar_prefix_crosses_dirs() {
        // clojure-lsp's own clj-file-regex pattern.
        let pat = "**.{edn,clj,cljs,cljc,bb,cljd,clj_kondo}";
        assert!(matches(pat, "core.clj"));
        assert!(matches(pat, "a/b/core.cljc"));
        assert!(matches(pat, "deps.edn"));
        assert!(matches(pat, "x/y/z.clj_kondo"));
        assert!(!matches(pat, "core.rs"));
    }

    #[test]
    fn star_does_not_cross_separator() {
        assert!(matches("*.clj", "core.clj"));
        assert!(!matches("*.clj", "a/core.clj"));
    }

    #[test]
    fn question_mark_single_char() {
        assert!(matches("a?c.clj", "abc.clj"));
        assert!(!matches("a?c.clj", "ac.clj"));
        assert!(!matches("a?c.clj", "a/c.clj"));
    }

    #[test]
    fn brace_alternation() {
        assert!(matches("src/{a,b}.clj", "src/a.clj"));
        assert!(matches("src/{a,b}.clj", "src/b.clj"));
        assert!(!matches("src/{a,b}.clj", "src/c.clj"));
    }

    #[test]
    fn char_class() {
        assert!(matches("[abc].clj", "a.clj"));
        assert!(!matches("[abc].clj", "d.clj"));
    }

    #[test]
    fn already_legal_doublestar_forms_pass_through() {
        assert!(matches("**/*.clj", "a/b/c.clj"));
        assert!(matches("**/*.clj", "c.clj"));
        assert!(matches("src/**", "src/a/b.clj"));
        assert_eq!(fixup_leading_doublestar("**"), "**");
        assert_eq!(fixup_leading_doublestar("**/foo"), "**/foo");
    }

    #[test]
    fn glob_walks_and_skips_hidden(){
        let dir = std::env::temp_dir().join(format!("mova-fs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/.git")).unwrap();
        std::fs::write(dir.join("root.clj"), "").unwrap();
        std::fs::write(dir.join("a/nested.clj"), "").unwrap();
        std::fs::write(dir.join("a/.git/hidden.clj"), "").unwrap();
        std::fs::write(dir.join(".dotfile.clj"), "").unwrap();

        let root = Value::Str(dir.to_string_lossy().into_owned().into());
        let pattern = Value::Str("**.clj".into());
        let mut interp = Interp::new();
        let res = glob_native(&mut interp, &[root, pattern]).unwrap();
        let paths: Vec<String> = match res {
            Value::Vector(v) => v
                .into_iter()
                .map(|x| match x {
                    Value::Str(s) => s.as_ref().to_string(),
                    _ => panic!("expected string"),
                })
                .collect(),
            _ => panic!("expected vector"),
        };
        assert!(paths.iter().any(|p| p.ends_with("root.clj")));
        assert!(paths.iter().any(|p| p.ends_with("a/nested.clj")));
        assert!(!paths.iter().any(|p| p.contains(".git")));
        assert!(!paths.iter().any(|p| p.ends_with(".dotfile.clj")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
