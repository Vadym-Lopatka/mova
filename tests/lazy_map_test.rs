//! Track M1: `Value::LazyMap` (`mova.edn/read-string-lazy`) must behave exactly like an eager `read-string` map.
use std::process::Command;

fn run(script: &str, name: &str) -> String {
    let path = std::env::temp_dir().join(format!("lazy_map_test_{name}.mova"));
    std::fs::write(&path, script).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mova")).arg(&path).output().unwrap();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

#[test]
fn small_map_equals_eager() {
    let script = r#"
(def src "{:a 1, :b/c [1 2 {:x \"s}\"}], :d {:e #{1 2}}, :f nil, :g \"a\\\"b\", :h (1 2), ;; c
 :i 1.5 :j \"x\"}")
(def l (mova.edn/read-string-lazy src))
(def e (read-string src))
(defn chk [n ok] (when-not ok (println "FAIL" n)))
(chk :type (= (type l) (type e)))
(chk :map? (map? l))
(chk :count (= (count l) (count e)))
(chk :eq (and (= l e) (= e l) (= l l)))
(chk :hash (= (hash l) (hash e)))
(chk :as-key (= :v (get {e :v} l)))
(chk :get (every? (fn [k] (= (get l k :nf) (get e k :nf))) (concat (keys e) [:zz :b])))
(chk :kw-call (every? (fn [k] (= (k l) (k e))) (keys e)))
(chk :nil-val (and (nil? (get l :f :nf)) (contains? l :f) (not (contains? l :zz)) (= :nf (get l :zz :nf))))
(chk :non-kw (nil? (get l "a")))
(chk :seq (= (set (seq l)) (set (seq e))))
(chk :keys (= (set (keys l)) (set (keys e))))
(chk :vals (= (set (vals l)) (set (vals e))))
(chk :assoc (= (assoc l :z 1) (assoc e :z 1)))
(chk :dissoc (= (dissoc l :a) (dissoc e :a)))
(chk :merge (= (merge l {:q 1}) (merge e {:q 1})))
(chk :into (= (into {} l) e))
(chk :conj (= (conj l [:k 1]) (conj e [:k 1])))
(chk :rkv (= (reduce-kv (fn [a k v] (conj a k)) #{} l) (set (keys e))))
(chk :select (= (select-keys l [:a :d]) (select-keys e [:a :d])))
(chk :get-in (= (get-in l [:d :e]) (get-in e [:d :e])))
(chk :pr (= (pr-str l) (pr-str e)))
(chk :destr (let [{:keys [a g]} l] (and (= a 1) (= g "a\"b"))))
(chk :empty (= (empty l) {}))
(chk :find (= (find l :a) (find e :a)))
(chk :dup-bails (= {:a 2} (try (mova.edn/read-string-lazy "{:a 1 :a 2}") (catch Exception x {:a 2}))))
(chk :non-map (= [1 2] (mova.edn/read-string-lazy "[1 2]")))
(chk :str-key (= {"a" 1} (mova.edn/read-string-lazy "{\"a\" 1}")))
(println "DONE")
"#;
    let out = run(script, "small");
    assert!(out.trim() == "DONE", "{out}");
}

#[test]
fn clojuredocs_every_key_equals_eager() {
    let Ok(dir) = std::env::var("MOVA_CLOJUREDOCS_DIR") else {
        println!("skipped: set MOVA_CLOJUREDOCS_DIR");
        return;
    };
    let p = format!("{dir}/cd.edn");
    if !std::path::Path::new(&p).exists() {
        eprintln!("skip: {p} missing");
        return;
    }
    let script = format!(
        r#"
(def s (slurp "{p}"))
(def l (mova.edn/read-string-lazy s))
(def e (read-string s))
(println (count l) (count e))
(println (every? (fn [k] (= (get l k) (get e k))) (keys e)))
(println (= l e) (= (hash l) (hash e)) (= (set (keys l)) (set (keys e))))
"#
    );
    let out = run(&script, "cd");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert_eq!(lines[0].split(' ').collect::<Vec<_>>()[0], lines[0].split(' ').collect::<Vec<_>>()[1], "{out}");
    assert_eq!(lines[1], "true", "{out}");
    assert_eq!(lines[2], "true true true", "{out}");
}

#[test]
fn json_generate_equals_eager() {
    let script = r#"
(def src "{:a 1, :b [1 2 {:x \"s\"}], :d {:e 2}, :f nil, :g \"q\"}")
(def l (mova.edn/read-string-lazy src))
(def e (read-string src))
(println (= (mova.json/generate-string l nil) (mova.json/generate-string e nil)))
(println (mova.json/generate-string l nil))
"#;
    let out = run(script, "json");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "true", "{out}");
    assert!(lines[1].starts_with('{') && lines[1].contains("\"a\":1"), "{out}");
}
