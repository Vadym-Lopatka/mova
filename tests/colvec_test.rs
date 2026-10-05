//! Track M6: a packed vector (`mova.mem/pack`, `PVec::Col`) must behave exactly like the unpacked vector.
use std::process::Command;

fn run(script: &str, name: &str) -> String {
    let path = std::env::temp_dir().join(format!("colvec_test_{name}.mova"));
    std::fs::write(&path, script).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mova")).arg(&path).output().unwrap();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

const PRELUDE: &str = r#"
(defn chk [n ok] (when-not ok (println "FAIL" n)))
(def big (vec (for [i (range 60)]
  (cond-> {:bucket :var-usages :name (with-meta (symbol (str "f" (mod i 7))) {:row i :col 3 :end-row i :end-col 9})
           :row i :col (* 2 i) :end-row (inc i) :end-col -4 :uri "file:///a.clj" :external? false
           :from 'my.ns :to 'clojure.core :context {} :arity (when (odd? i) 2) :alias nil
           :fixed-arities #{1 (mod i 3)} :str (str "s" i) :ratio (/ i 7) :f (* 1.5 i)}
    (zero? (mod i 5)) (assoc :extra [i "x" :k])
    (zero? (mod i 9)) (dissoc :alias)))))
(def small (vec (for [i (range 40)] (if (even? i) {:a i :b "x"} {:b nil :a i :c #{i}}))))
"#;

fn body(v: &str) -> String {
    format!(
        r#"
(def e {v})
(def p (mova.mem/pack e))
(chk :packed (mova.mem/packed? p))
(chk :type (= (type p) (type e)))
(chk :vector? (and (vector? p) (sequential? p) (coll? p) (counted? p) (associative? p)))
(chk :count (= (count p) (count e)))
(chk :eq (and (= p e) (= e p) (= p p) (= p (seq e)) (= (seq p) e)))
(chk :hash (= (hash p) (hash e)))
(chk :as-key (= :v (get {{e :v}} p)))
(chk :nth (every? #(= (nth p %) (nth e %)) (range (count e))))
(chk :nth-default (= (nth p 1000 :d) :d))
(chk :get (and (= (get p 3) (get e 3)) (nil? (get p 1000)) (= (p 2) (e 2))))
(chk :elem-class (every? #(= (class (nth p %)) (class (nth e %))) (range (count e))))
(chk :elem-meta (every? #(= (meta (:name (nth p %))) (meta (:name (nth e %)))) (range (count e))))
(chk :first-last (and (= (first p) (first e)) (= (last p) (last e)) (= (peek p) (peek e))))
(chk :seq (= (vec (seq p)) e))
(chk :rest (and (= (rest p) (rest e)) (= (next p) (next e)) (= (nthrest p 5) (nthrest e 5)) (= (drop 30 p) (drop 30 e))))
(chk :reduce (= (reduce (fn [a m] (conj a (:row m))) [] p) (reduce (fn [a m] (conj a (:row m))) [] e)))
(chk :reduce-kv (= (reduce-kv (fn [a i m] (+ a i)) 0 p) (reduce-kv (fn [a i m] (+ a i)) 0 e)))
(chk :transduce (= (into [] (comp (filter :a) (map :a)) p) (into [] (comp (filter :a) (map :a)) e)))
(chk :map (= (map identity p) (map identity e)))
(chk :mapv (= (mapv :row p) (mapv :row e)))
(chk :conj (= (conj p {{:z 1}}) (conj e {{:z 1}})))
(chk :assoc (and (= (assoc p 0 :x) (assoc e 0 :x)) (= (assoc p (count p) :y) (assoc e (count e) :y))))
(chk :update (= (update p 1 assoc :q 1) (update e 1 assoc :q 1)))
(chk :pop (= (pop p) (pop e)))
(chk :subvec (and (= (subvec p 1 30) (subvec e 1 30)) (= (subvec p 2 5) (subvec e 2 5))))
(chk :into (and (= (into [] p) e) (= (into p [1 2]) (into e [1 2])) (= (into #{{}} p) (into #{{}} e))))
(chk :vec (= (vec p) e))
(chk :set (= (set p) (set e)))
(chk :concat (= (concat p p) (concat e e)))
(chk :sort (= (sort-by :a p) (sort-by :a e)))
(chk :group (= (group-by :a p) (group-by :a e)))
(chk :rseq (= (rseq p) (rseq e)))
(chk :print (and (= (pr-str p) (pr-str e)) (= (str p) (str e)) (= (with-out-str (print p)) (with-out-str (print e)))))
(chk :meta (and (= (meta (with-meta p {{:m 1}})) {{:m 1}}) (= (with-meta p {{:m 1}}) e) (nil? (meta p))))
(chk :empty (and (= (empty p) []) (= (empty p) (empty e)) (not (empty? p))))
(chk :transient (= (persistent! (conj! (transient p) 1)) (conj e 1)))
(chk :apply (= (apply list p) (apply list e)))
(chk :identical (and (identical? p p) (not (identical? p e))))
(chk :contains (and (contains? p 0) (not (contains? p 1000))))
(chk :index-of (= (.indexOf p (nth e 3)) 3))
(chk :compare (= 0 (compare (mova.mem/pack (vec (range 20))) (vec (range 20)))))
(chk :destructure (let [[a b & r] p [x y & z] e] (and (= a x) (= b y) (= r z))))
(chk :keys-in (= (map (juxt :name :row :f :ratio) p) (map (juxt :name :row :f :ratio) e)))
(println "done")
"#
    )
}

#[test]
fn packed_vector_of_shaped_maps_equals_unpacked() {
    let out = run(&format!("{PRELUDE}{}", body("big")), "big");
    assert_eq!(out.trim(), "done", "{out}");
}

#[test]
fn packed_vector_of_small_maps_equals_unpacked() {
    let out = run(&format!("{PRELUDE}{}", body("small")), "small");
    assert_eq!(out.trim(), "done", "{out}");
}

#[test]
fn pack_is_identity_when_ineligible() {
    let out = run(
        r#"
(defn chk [n ok] (when-not ok (println "FAIL" n)))
(chk :short (not (mova.mem/packed? (mova.mem/pack [{:a 1}]))))
(chk :non-map (not (mova.mem/packed? (mova.mem/pack (vec (range 100))))))
(chk :str-keys (not (mova.mem/packed? (mova.mem/pack (vec (for [i (range 50)] {"a" i}))))))
(chk :meta-elem (not (mova.mem/packed? (mova.mem/pack (vec (for [i (range 50)] (with-meta {:a i} {:m 1})))))))
(chk :non-vec (= '(1 2) (mova.mem/pack '(1 2))))
(chk :min-arg (mova.mem/packed? (mova.mem/pack [{:a 1} {:a 2}] 2)))
(chk :no-mat (let [p (mova.mem/pack (vec (for [i (range 50)] {:a i}))) s0 (second (mova.mem/colvec-stats))]
               (reduce + (map :a p)) (nth p 3) (count p) (into [] (filter :a) p) (= p p) (hash p)
               (= s0 (second (mova.mem/colvec-stats)))))
(println "done")
"#,
        "ineligible",
    );
    assert_eq!(out.trim(), "done", "{out}");
}

#[test]
fn packed_vector_survives_image_round_trip() {
    let dir = std::env::temp_dir().join(format!("mova-colvec-img-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/cv")).unwrap();
    std::fs::write(
        dir.join("src/cv/core.clj"),
        "(ns cv.core)\n(def e (vec (for [i (range 50)] {:a i :b (with-meta 'x {:row i}) :c \"s\" :d #{i} :e 1 :f 2 :g 3 :h 4 :i 5})))\n\
         (def p (mova.mem/pack e))\n(def p2 p)\n",
    )
    .unwrap();
    std::fs::write(dir.join("train.clj"), "(require 'cv.core)\n").unwrap();
    std::fs::write(
        dir.join("main.clj"),
        "(require 'cv.core)\n(prn (= cv.core/p cv.core/e) (= (map (comp meta :b) cv.core/p) (map (comp meta :b) cv.core/e)) (identical? cv.core/p cv.core/p2) (count cv.core/p))\n",
    )
    .unwrap();
    let run = |image: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mova"));
        c.current_dir(&dir).args(["--module-path", "src", "main.clj"]);
        if image {
            c.env("MOVA_IMAGE", dir.join("t.img")).env("MOVA_IMAGE_PRELOAD", "cv.core").env("MOVA_IMAGE_TRAIN", dir.join("train.clj"));
        }
        let o = c.output().unwrap();
        (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let plain = run(false);
    let first = run(true);
    let second = run(true);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(plain.0, "true true true 50\n", "{}", plain.1);
    assert_eq!(first.0, plain.0, "{}", first.1);
    assert_eq!(second.0, plain.0, "{}", second.1);
}
