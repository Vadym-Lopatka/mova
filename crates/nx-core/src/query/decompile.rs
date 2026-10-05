//! Definition of a class from a classpath jar (clojure-lsp `java-interop/uri->translated-uri`): the `.class` is
//! decompiled with CFR into the project's `.lsp/.cache/java/decompiled` (or, when the jar has a `META-INF/**/pom.xml`,
//! into the global cache `java/decompiled/<group>/<artifact>/<artifact>-<version>/<src>`) and the `.java` file is the
//! definition. Decompiling runs the CFR jar found in `~/.m2` (or `NX_CFR_JAR`) with the `java` of `JAVA_HOME`/PATH;
//! when either is missing the location is still answered (the file just does not exist yet).
use crate::io::jar::Jar;
use std::path::{Path, PathBuf};

fn global_cache() -> PathBuf {
    match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("clojure-lsp"),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/clojure-lsp"),
    }
}

/// `jar->java-project-info`: (group, artifact, version, source dir) of the first `META-INF/**/pom.xml`.
fn pom_info(jar: &mut Jar) -> Option<(String, String, String, String)> {
    let name = jar.find_entry(|n| n.starts_with("META-INF") && n.ends_with("pom.xml"))?;
    let mut buf = Vec::new();
    jar.read(&name, &mut buf).ok()?;
    let pom = String::from_utf8_lossy(&buf);
    let tag = |t: &str| regex::Regex::new(&format!("<{t}>(.+)</{t}>")).ok()?.captures(&pom).map(|c| c[1].to_string());
    Some((tag("groupId")?, tag("artifactId")?, tag("version")?, tag("sourceDirectory").unwrap_or_else(|| "src".into())))
}

fn cfr_jar() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NX_CFR_JAR") {
        return Some(PathBuf::from(p));
    }
    let base = PathBuf::from(std::env::var_os("HOME")?).join(".m2/repository/org/benf/cfr");
    let mut vs: Vec<PathBuf> = std::fs::read_dir(base).ok()?.flatten().map(|e| e.path()).collect();
    vs.sort();
    vs.iter().rev().find_map(|d| std::fs::read_dir(d).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "jar")))
}

fn decompile(jar_path: &str, entry: &str, class_cache: &Path, out_dir: &Path) {
    let Some(cfr) = cfr_jar() else { return };
    let mut buf = Vec::new();
    let class_file = class_cache.join(entry);
    let ok = Jar::open(jar_path).and_then(|mut j| j.read(entry, &mut buf)).is_ok()
        && class_file.parent().is_some_and(|p| std::fs::create_dir_all(p).is_ok())
        && std::fs::write(&class_file, &buf).is_ok();
    if !ok {
        return;
    }
    let java = std::env::var_os("JAVA_HOME").map(|h| PathBuf::from(h).join("bin/java")).filter(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("java"));
    let _ = std::process::Command::new(java)
        .arg("-jar")
        .arg(cfr)
        .arg(&class_file)
        .arg("--outputdir")
        .arg(out_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// URI of the decompiled `.java` of `entry` (`a/b/C.class`) of `jar_path`, decompiling it when absent.
pub fn decompiled_uri(root: &Path, jar_path: &str, entry: &str, jar_scheme: bool) -> String {
    let _ = jar_scheme;
    let java_rel = format!("{}.java", entry.strip_suffix(".class").unwrap_or(entry));
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let local = root.join(".lsp/.cache");
    let pom = Jar::open(jar_path).ok().and_then(|mut j| pom_info(&mut j));
    let (dest, out_dir) = match &pom {
        Some((g, a, v, src)) => {
            let folder = global_cache().join("java/decompiled").join(g).join(a).join(format!("{a}-{v}")).join(src);
            (folder.join(&java_rel), folder)
        }
        None => (local.join("java/decompiled").join(&java_rel), local.join("java/decompiled")),
    };
    // inner classes live in their outer class' file (the JVM cuts the uri at the first `$`)
    let mut uri = crate::engine::scan::path_to_uri(&dest);
    if let Some(i) = uri.find('$') {
        uri = format!("{}.java", &uri[..i]);
    }
    let file = crate::engine::scan::uri_to_path(&uri).unwrap_or(dest);
    if !file.is_file() {
        decompile(jar_path, entry, &local.join("java/classes"), &out_dir);
    }
    uri
}
