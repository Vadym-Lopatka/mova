//! Heap image: a macro restored from an image must still expand via the
//! tree-walker. Regression (S0): restored `Value::Macro` closures got a
//! pending `CompileSlot`, so the compiled tier (which has no `&form`/`&env`)
//! ran the macro body and failed "Unable to resolve symbol: &form".

use std::process::Command;

fn run(dir: &std::path::Path) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mova"))
        .current_dir(dir)
        .env("MOVA_IMAGE", dir.join("t.img"))
        .env("MOVA_IMAGE_PRELOAD", "rp.core")
        .args(["--module-path", "src", "main.clj"])
        .output()
        .expect("spawn mova");
    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn restored_macro_with_form_expands() {
    let dir = std::env::temp_dir().join(format!("mova-image-macro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/rp")).unwrap();
    // `&form` outside syntax-quote, so the macro body is fully compilable.
    std::fs::write(
        dir.join("src/rp/log.clj"),
        "(ns rp.log)\n(defn -info [m & args] (apply println \"L\" (:ns-str m) args))\n\
         (defmacro info [& args]\n  (let [fmeta (assoc (meta &form) :ns-str (str *ns*))]\n    `(-info ~fmeta ~@args)))\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/rp/core.clj"),
        "(ns rp.core (:require [rp.log :as logger]))\n(defn g [] (logger/info \"a\"))\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.clj"),
        "(require 'rp.core)\n(rp.core/g)\n(in-ns 'rp.core)\n(logger/info \"b\")\n",
    )
    .unwrap();
    let first = run(&dir);
    assert!(dir.join("t.img").exists(), "image not written: {}", first.1);
    let second = run(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(first.0, "L rp.core a\nL rp.core b\n", "fresh run: {}", first.1);
    assert_eq!(second.0, first.0, "restored run: {}", second.1);
}
