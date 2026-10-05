//! K2 spike (docs/K2-SPIKE.md): per-call floor of two hot clj-kondo fns,
//! current compiled tier vs hand-written Rust over the REAL runtime Values
//! (borrowed args, keyword constants hoisted, type-identity inline cache).
//! Run: `cargo test --release -q --test k2_floor_spike -- --ignored --nocapture`
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use mova::internal::{Interp, Keyword, PMap, Value};

const SRC: &str = r#"
(defprotocol Node (tag [n]))
(defrecord TokenNode [value string-value] Node (tag [_] :token))
(defrecord SeqNode [tag format-string wrap-length seq-fn children] Node (tag [this] tag))
(def tok (->TokenNode 'foo "foo"))
(def lst (->SeqNode :list "(%s)" 2 seq [tok]))
;; clj-kondo.impl.utils/tag
(defn utag [expr] (when expr (tag expr)))
;; clj-kondo.impl.types/ret-tag-from-call, common path (no :ret, no :arg-types)
(defn ret-tag [ctx call _expr]
  (or (:ret call)
      (when (not (:unresolved? call))
        (or (when-let [ret (:ret call)] {:tag ret})
            (when-let [arg-types (:arg-types call)] {:call arg-types})))))
(def call (zipmap [:filename :type :lang :base-lang :resolved-ns :ns :name :arity :row :col
                   :end-row :end-col :expr :callstack :config :top-ns]
                  (range)))
(defn d-empty [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) x) a)))
(defn d-utag [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (utag x)) a)))
(defn d-ret [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (ret-tag nil x nil)) a)))
"#;

const N: i64 = 5_000_000;

fn kw(s: &str) -> Value {
    Value::Keyword(Keyword::construct(s))
}

fn time_min<F: FnMut()>(mut f: F) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed().as_nanos() as f64);
    }
    best / N as f64
}

fn clj_loop(interp: &mut Interp, driver: &str, arg: &str) -> f64 {
    let f = interp.eval_str("k2", driver).unwrap();
    let x = interp.eval_str("k2", arg).unwrap();
    interp.call(&f, &[Value::Int(1000), x.clone()]).unwrap();
    time_min(|| {
        black_box(interp.call(&f, &[Value::Int(N), x.clone()]).unwrap());
    })
}

/// Floor for `utag`: nil test, record-type IC, constant or field read.
#[inline(never)]
fn utag_floor(expr: &Value, tok_w: &Value, lst_w: &Value, k_tag: &Value, k_token: &Value) -> Value {
    match expr {
        Value::Nil => Value::Nil,
        Value::Inst(i) => match (tok_w, lst_w) {
            (Value::Inst(t), _) if Arc::ptr_eq(&i.tdef, &t.tdef) => k_token.clone(),
            (_, Value::Inst(s)) if Arc::ptr_eq(&i.tdef, &s.tdef) => {
                i.data.get(k_tag).cloned().unwrap_or(Value::Nil)
            }
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

/// Floor for `ret-tag`: four borrowed map reads, no clones on the nil path.
#[inline(never)]
fn ret_floor(call: &Value, ks: &[Value; 3]) -> Value {
    let Value::Map(m) = call else { unreachable!() };
    if let Some(v) = m.get(&ks[0]).filter(|v| v.truthy()) {
        return v.clone();
    }
    if m.get(&ks[1]).is_some_and(|v| v.truthy()) {
        return Value::Nil;
    }
    if let Some(r) = m.get(&ks[0]).filter(|v| v.truthy()) {
        let mut out = PMap::new();
        out.insert(kw("tag"), r.clone());
        return Value::Map(out);
    }
    if let Some(a) = m.get(&ks[2]).filter(|v| v.truthy()) {
        let mut out = PMap::new();
        out.insert(kw("call"), a.clone());
        return Value::Map(out);
    }
    Value::Nil
}

#[test]
#[ignore]
fn k2_floor() {
    let mut interp = Interp::new();
    interp.eval_str("k2", SRC).expect("defs");
    let empty_t = clj_loop(&mut interp, "d-empty", "tok");
    let utag_tok = clj_loop(&mut interp, "d-utag", "tok") - empty_t;
    let utag_lst = clj_loop(&mut interp, "d-utag", "lst") - empty_t;
    let ret_t = clj_loop(&mut interp, "d-ret", "call") - empty_t;

    let tok = interp.eval_str("k2", "tok").unwrap();
    let lst = interp.eval_str("k2", "lst").unwrap();
    let call = interp.eval_str("k2", "call").unwrap();
    let (k_tag, k_token) = (kw("tag"), kw("token"));
    let ks = [kw("ret"), kw("unresolved?"), kw("arg-types")];
    let f_tok = time_min(|| {
        for _ in 0..N {
            black_box(utag_floor(black_box(&tok), &tok, &lst, &k_tag, &k_token));
        }
    });
    let f_lst = time_min(|| {
        for _ in 0..N {
            black_box(utag_floor(black_box(&lst), &tok, &lst, &k_tag, &k_token));
        }
    });
    let f_ret = time_min(|| {
        for _ in 0..N {
            black_box(ret_floor(black_box(&call), &ks));
        }
    });
    println!("K2 empty-loop iter (current)      {empty_t:7.1} ns");
    println!("K2 utag tok   current {utag_tok:7.1}  floor {f_tok:6.1} ns/call  x{:.0}", utag_tok / f_tok);
    println!("K2 utag seq   current {utag_lst:7.1}  floor {f_lst:6.1} ns/call  x{:.0}", utag_lst / f_lst);
    println!("K2 ret-tag    current {ret_t:7.1}  floor {f_ret:6.1} ns/call  x{:.0}", ret_t / f_ret);
}
