//! Heap image S3: threaded-tier native code written with the image (MOVA_JIT=1)
//! is bound on first call after restore (no Cranelift), and bound code still
//! sees var redefinitions made after restore.

use std::process::Command;

fn run(dir: &std::path::Path, image: bool, jit: bool) -> (String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mova"));
    c.current_dir(dir).env("MOVA_IMAGE_TRACE", "1").env("MOVA_AOT_STATS", "1").args(["--module-path", "src", "main.clj"]);
    c.env("MOVA_JIT", if jit { "1" } else { "0" }).env_remove("MOVA_JIT_HOT_N");
    if image {
        c.env("MOVA_IMAGE", dir.join("t.img")).env("MOVA_IMAGE_PRELOAD", "rp.core").env("MOVA_IMAGE_TRAIN", dir.join("train.clj"));
    }
    let out = c.output().expect("spawn mova");
    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn stat(err: &str, key: &str) -> u64 {
    let l = err.lines().find(|l| l.starts_with("aot-stats:")).unwrap_or("");
    l.split_whitespace().find_map(|w| w.strip_prefix(&format!("{key}="))).and_then(|v| v.parse().ok()).unwrap_or(0)
}

#[test]
fn image_native_code_binds_and_sees_redefinition() {
    if !cfg!(feature = "jit") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("mova-image-aot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/rp")).unwrap();
    std::fs::write(
        dir.join("src/rp/core.clj"),
        "(ns rp.core)\n(defn g [x] (* x 10))\n(def k 5)\n\
         (defn f [{:keys [a b] :or {b 1}} & more]\n  (let [h (fn [y] (+ (g y) k))]\n    \
         (try (mapv h (into [a b] more)) (catch Exception e (str \"E:\" (.getMessage e))))))\n\
         (defn loopy [n] (loop [i 0 acc []] (if (< i n) (recur (inc i) (conj acc (g i))) acc)))\n\
         (defn kw [m] (if (:a m) (+ (:a m) k 1.5) :none))\n",
    )
    .unwrap();
    std::fs::write(dir.join("train.clj"), "(dotimes [_ 3] (rp.core/f {:a 1} 2) (rp.core/loopy 3) (rp.core/kw {:a 2}))\n").unwrap();
    std::fs::write(
        dir.join("main.clj"),
        "(require 'rp.core)\n(prn (rp.core/f {:a 1} 2) (rp.core/loopy 3) (rp.core/kw {:a 2}))\n\
         (in-ns 'rp.core)\n(defn g [x] (- x))\n(def k 100)\n\
         (prn (f {:a 1 :b 2} 3) (loopy 3) (kw {}) (kw {:a 1}))\n(defn g [x] (throw (Exception. \"boom\")))\n(prn (f {:a 1}))\n",
    )
    .unwrap();
    let want = "[15 15 25] [0 10 20] 8.5\n[99 98 97] [0 -1 -2] :none 102.5\n\"E:boom\"\n";
    let plain = run(&dir, false, false);
    assert_eq!(plain.0, want, "plain run: {}", plain.1);
    let first = run(&dir, true, true);
    assert!(dir.join("t.img").exists(), "image not written: {}", first.1);
    assert!(first.1.contains("\"arity:native-code\""), "no native code written: {}", first.1);
    assert_eq!(first.0, want, "training run: {}", first.1);
    assert_eq!(stat(&first.1, "unclassified"), 0, "unclassified absolute refs: {}", first.1);
    let second = run(&dir, true, true);
    let off = run(&dir, true, false);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(second.1.contains("restored"), "not restored: {}", second.1);
    assert_eq!(second.0, want, "restored run: {}", second.1);
    assert!(stat(&second.1, "bound") >= 3, "nothing bound from the image: {}", second.1);
    assert_eq!(stat(&second.1, "lowered"), 0, "restored run still lowered: {}", second.1);
    assert_eq!(off.0, want, "restored run, JIT off: {}", off.1);
}

// K3 site caches (protocol IC, direct-call entry, destructuring let, armed intrinsic) in image code.
#[test]
fn image_native_code_k3_sites() {
    if !cfg!(feature = "jit") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("mova-image-aot-k3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/rp")).unwrap();
    std::fs::write(
        dir.join("src/rp/core.clj"),
        "(ns rp.core)\n(defprotocol Area (area [s]))\n(defrecord Sq [w])\n(extend-type Sq Area (area [s] (* (:w s) (:w s))))\n\
         (defrecord Rect [w h] Area (area [_] (* w h)))\n(defn g [x] (+ x 1))\n\
         (defn go [shapes m v]\n  (let [{:keys [a b]} m [x y] v]\n    [(mapv (fn [s] (area s)) shapes) (g a) (g b) (+ x y)]))\n",
    )
    .unwrap();
    std::fs::write(dir.join("train.clj"), "(dotimes [_ 3] (rp.core/go [(rp.core/->Sq 2) (rp.core/->Rect 2 3)] {:a 1 :b 2} [3 4]))\n").unwrap();
    std::fs::write(
        dir.join("main.clj"),
        "(require 'rp.core)\n(prn (rp.core/go [(rp.core/->Sq 2) (rp.core/->Rect 2 3)] {:a 1 :b 2} [3 4]))\n(in-ns 'rp.core)\n\
         (defn g [x] (* x 100))\n(extend-protocol Area Sq (area [s] (- (:w s))))\n\
         (prn (go [(->Sq 2) (->Rect 2 3)] {:a 1 :b 2} [3 4]))\n",
    )
    .unwrap();
    let want = "[[4 6] 2 3 7]\n[[-2 6] 100 200 7]\n";
    let plain = run(&dir, false, false);
    assert_eq!(plain.0, want, "plain run: {}", plain.1);
    let first = run(&dir, true, true);
    assert_eq!(first.0, want, "training run: {}", first.1);
    assert!(first.1.contains("\"arity:native-code\""), "no native code written: {}", first.1);
    assert_eq!(stat(&first.1, "unclassified"), 0, "unclassified absolute refs: {}", first.1);
    let second = run(&dir, true, true);
    let off = run(&dir, true, false);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(second.0, want, "restored run: {}", second.1);
    assert!(stat(&second.1, "bound") >= 2, "nothing bound from the image: {}", second.1);
    assert_eq!(stat(&second.1, "lowered"), 0, "restored run still lowered: {}", second.1);
    assert_eq!(off.0, want, "restored run, JIT off: {}", off.1);
}

// K5: `Ir::New` (compiled `(new C ..)`, gate + make helpers, Escape fallback) and `:ns/keys` in image code.
#[test]
fn image_native_code_k5_new() {
    if !cfg!(feature = "jit") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("mova-image-aot-k5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/rp")).unwrap();
    std::fs::write(
        dir.join("src/rp/core.clj"),
        "(ns rp.core)\n(defrecord P [x y])\n(defn g [v] (inc v))\n\
         (defn mk [{:a/keys [x y] :or {y 7}}]\n  [(new P (g x) y) (new rp.core.P x (g y))\n   (try (new P x) (catch Exception e :arity))\n   (let [P 3] (try (new P 1 2) (catch Exception e :not-class)))])\n",
    )
    .unwrap();
    std::fs::write(dir.join("train.clj"), "(dotimes [_ 3] (rp.core/mk {:a/x 1 :a/y 2}))\n").unwrap();
    std::fs::write(
        dir.join("main.clj"),
        "(require 'rp.core)\n(prn (rp.core/mk {:a/x 1 :a/y 2}) (rp.core/mk {:a/x 1 :x 5}))\n(in-ns 'rp.core)\n\
         (defn g [v] (* v 100))\n(prn (map :x (mk {:a/x 1})))\n",
    )
    .unwrap();
    let want = "[#rp.core.P{:x 2, :y 2} #rp.core.P{:x 1, :y 3} :arity :not-class] [#rp.core.P{:x 2, :y 7} #rp.core.P{:x 1, :y 8} :arity :not-class]\n(100 1 nil nil)\n";
    let plain = run(&dir, false, false);
    assert_eq!(plain.0, want, "plain run: {}", plain.1);
    let first = run(&dir, true, true);
    assert_eq!(first.0, want, "training run: {}", first.1);
    assert_eq!(stat(&first.1, "unclassified"), 0, "unclassified absolute refs: {}", first.1);
    let second = run(&dir, true, true);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(second.0, want, "restored run: {}", second.1);
    assert!(stat(&second.1, "bound") >= 1, "nothing bound from the image: {}", second.1);
}

// K7b: fn literal (`jit_t_mk_fn`), native `try` (`jit_t_catch`), destructuring `loop` in image code.
#[test]
fn image_native_code_k7b() {
    if !cfg!(feature = "jit") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("mova-image-aot-k7b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/rp")).unwrap();
    std::fs::write(
        dir.join("src/rp/core.clj"),
        "(ns rp.core)\n(defn g [v] (inc v))\n\
         (defn mk [xs n]\n  [(mapv (fn [x] (+ x n)) xs)\n   (try (g (nth xs n)) (catch IndexOutOfBoundsException e :oob) (catch Exception e :other))\n   (loop [[a & more] xs acc 0] (if a (recur more (+ acc (g a))) acc))])\n",
    )
    .unwrap();
    std::fs::write(dir.join("train.clj"), "(dotimes [_ 3] (rp.core/mk [1 2 3] 1) (rp.core/mk [1] 5))\n").unwrap();
    std::fs::write(
        dir.join("main.clj"),
        "(require 'rp.core)\n(prn (rp.core/mk [1 2 3] 1) (rp.core/mk [1] 5))\n(in-ns 'rp.core)\n\
         (defn g [v] (* v 100))\n(prn (mk [1 2] 0))\n",
    )
    .unwrap();
    let want = "[[2 3 4] 3 9] [[6] :oob 2]\n[[1 2] 100 300]\n";
    let plain = run(&dir, false, false);
    assert_eq!(plain.0, want, "plain run: {}", plain.1);
    let first = run(&dir, true, true);
    assert_eq!(first.0, want, "training run: {}", first.1);
    assert_eq!(stat(&first.1, "unclassified"), 0, "unclassified absolute refs: {}", first.1);
    let second = run(&dir, true, true);
    let off = run(&dir, true, false);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(second.0, want, "restored run: {}", second.1);
    assert!(stat(&second.1, "bound") >= 1, "nothing bound from the image: {}", second.1);
    assert_eq!(off.0, want, "restored run, JIT off: {}", off.1);
}
