//! `mova --source-index`: schema, counts, and file:line accuracy.
use std::process::Command;

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

#[test]
fn source_index_is_accurate() {
    let out = Command::new(env!("CARGO_BIN_EXE_mova")).arg("--source-index").env_remove("MOVA_IMAGE").output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["v"], 2);
    let root = v["root"].as_str().unwrap().to_string();
    let nat = v["natives"].as_array().unwrap();
    assert!(nat.len() >= 300, "natives: {}", nat.len());
    let find = |ns: &str, name: &str| nat.iter().find(|n| n["ns"] == ns && n["name"] == name).unwrap_or_else(|| panic!("{ns}/{name} missing"));
    assert_eq!(find("clojure.core", "assoc")["file"], "src/builtins/collections.rs");
    assert!(find("clojure.core", "conj")["file"].as_str().unwrap().starts_with("src/builtins/"));
    assert_eq!(find("mova.metrics", "span!")["file"], "src/metrics.rs");
    let nss = v["namespaces"].as_array().unwrap();
    assert!(nss.iter().any(|n| n["ns"] == "clojure.core" && n["file"] == "core/core.mova"));
    // core/async.mova defs are bare; `clojure.core.async/<name>` are aliases of the bare vars
    assert!(nss.iter().any(|n| n["ns"] == "clojure.core" && n["file"] == "core/async.mova"));
    let al = v["aliases"].as_array().unwrap();
    for name in ["chan", "go-loop", "chan?"] {
        assert!(al.iter().any(|a| a["ns"] == "clojure.core.async" && a["name"] == name && a["to_ns"] == "clojure.core" && a["to"] == name), "alias {name}");
    }
    assert!(al.iter().any(|a| a["ns"] == "clojure.repl" && a["name"] == "doc"));
    let da = v["default_aliases"].as_array().unwrap();
    assert!(da.iter().any(|a| a["alias"] == "async" && a["ns"] == "clojure.core.async"));
    assert!(da.iter().any(|a| a["alias"] == "flow" && a["ns"] == "clojure.core.async.flow"));
    // sample 30 natives with a fixed-stride walk (deterministic pseudo-random)
    let (mut hit, mut total) = (0, 0);
    let mut idx = 7usize;
    for _ in 0..30 {
        idx = (idx * 7919 + 13) % nat.len();
        let n = &nat[idx];
        let text = std::fs::read_to_string(format!("{root}/{}", n["file"].as_str().unwrap())).unwrap();
        let line = text.lines().nth(n["line"].as_u64().unwrap() as usize - 1).unwrap_or("");
        total += 1;
        if norm(line).contains(&norm(n["name"].as_str().unwrap())) {
            hit += 1;
        }
    }
    eprintln!("srcindex hit rate {hit}/{total}");
    assert!(hit * 100 >= total * 95, "hit rate {hit}/{total}");
}
