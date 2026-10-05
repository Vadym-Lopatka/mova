//! Heap image S2: IR compiled during a training run (MOVA_IMAGE_TRAIN) is
//! persisted; restored fns come back compiled and must still see var
//! redefinitions made after restore (GlobalChain cells keep identity).

use std::process::Command;

fn run(dir: &std::path::Path, image: bool) -> (String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mova"));
    c.current_dir(dir).env("MOVA_IMAGE_TRACE", "1").args(["--module-path", "src", "main.clj"]);
    if image {
        c.env("MOVA_IMAGE", dir.join("t.img")).env("MOVA_IMAGE_PRELOAD", "rp.core").env("MOVA_IMAGE_TRAIN", dir.join("train.clj"));
    }
    let out = c.output().expect("spawn mova");
    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn restored_ir_sees_redefinition() {
    let dir = std::env::temp_dir().join(format!("mova-image-ir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/rp")).unwrap();
    std::fs::write(
        dir.join("src/rp/core.clj"),
        "(ns rp.core)\n(defn g [x] (* x 10))\n(def k 5)\n\
         (defn f [{:keys [a b] :or {b 1}} & more]\n  (let [h (fn [y] (+ (g y) k))]\n    \
         (try (mapv h (into [a b] more)) (catch Exception e (str \"E:\" (.getMessage e))))))\n\
         (defn loopy [n] (loop [i 0 acc []] (if (< i n) (recur (inc i) (conj acc (g i))) acc)))\n",
    )
    .unwrap();
    std::fs::write(dir.join("train.clj"), "(dotimes [_ 3] (rp.core/f {:a 1} 2) (rp.core/loopy 3))\n").unwrap();
    std::fs::write(
        dir.join("main.clj"),
        "(require 'rp.core)\n(prn (rp.core/f {:a 1} 2) (rp.core/loopy 3))\n\
         (in-ns 'rp.core)\n(defn g [x] (- x))\n(def k 100)\n\
         (prn (f {:a 1 :b 2} 3) (loopy 3))\n(defn g [x] (throw (Exception. \"boom\")))\n(prn (f {:a 1}))\n",
    )
    .unwrap();
    let plain = run(&dir, false);
    let first = run(&dir, true);
    assert!(dir.join("t.img").exists(), "image not written: {}", first.1);
    let second = run(&dir, true);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(plain.0, "[15 15 25] [0 10 20]\n[99 98 97] [0 -1 -2]\n\"E:boom\"\n", "plain run: {}", plain.1);
    assert_eq!(first.0, plain.0, "training run: {}", first.1);
    assert!(first.1.contains("\"closure:ir-persisted\""), "no IR persisted: {}", first.1);
    assert!(second.1.contains("restored"), "not restored: {}", second.1);
    assert_eq!(second.0, plain.0, "restored run: {}", second.1);
}
