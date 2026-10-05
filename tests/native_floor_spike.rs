//! Spike (see mova/HANDOVER.md / bench/spike): real floor of "JIT-emitted"
//! native code for a kondo-shaped analyzer walk over Mova's REAL runtime
//! values (not a synthetic repr) -- built by evaluating
//! `bench/spike/kondo-walk.clj`'s defs/tree through `Interp`, then walked
//! by hand-written Rust that is what a good JIT would emit: an inline
//! cache on record-type identity (`Arc::ptr_eq` on `InstVal::tdef`,
//! W-GEO's real dispatch key, see `types.rs::class_key`), real `PMap`
//! field reads, and the real owned/mutating `PMap::insert` fast path that
//! `assoc`/`update`'s `*_owned` entry points use (`builtins/collections.rs
//! ::assoc_owned`). C2 repeats it but calls `update`'s real `NativeFn` for
//! the `:tokens` branch, passing `inc` as a first-class fn through the
//! generic `Interp::call_owned` apply path (no `inc` inlining) -- the
//! delta from C is exactly apply-path overhead.
//!
//! Run: `cargo test --release -q --test native_floor_spike -- --ignored --nocapture`

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use mova::internal::{Interp, Keyword, Value};

const SRC: &str = r#"
(defprotocol Node (tag [n]))
(defrecord TokenNode [value] Node (tag [_] :token))
(defrecord SeqNode [tag* children] Node (tag [_] tag*))

(defn mk-vector-node []
  (->SeqNode :vector [(->TokenNode :kw) (->TokenNode 'sym) (->TokenNode 42)]))

(defn mk-list-node []
  (->SeqNode :list
             (vec (concat
                   (for [i (range 9)]
                     (->TokenNode (case (mod i 3) 0 :kw 1 'sym 2 i)))
                   [(mk-vector-node)]))))

(def forms (vec (repeatedly 1000 mk-list-node)))
"#;

const NODE_COUNT: usize = 14_000;
const REPS: u64 = 3000;

struct Cache {
    tokennode_witness: Value,
    seqnode_witness: Value,
    kw_children: Value,
    kw_tokens: Value,
    kw_depth: Value,
    kw_config: Value,
    kw_lang: Value,
    kw_tag_star: Value,
    update_fn: Value,
    inc_fn: Value,
}

fn is_same_record_type(a: &Value, witness: &Value) -> bool {
    match (a, witness) {
        (Value::Inst(x), Value::Inst(y)) => Arc::ptr_eq(&x.tdef, &y.tdef),
        _ => false,
    }
}

fn init_ident_and_cache() -> (Interp, Value, Cache) {
    let mut interp = Interp::new();
    interp.eval_str("spike", SRC).expect("eval defs+tree");
    let forms = interp.eval_str("spike", "forms").expect("eval forms");
    let update_fn = interp.eval_str("spike", "update").expect("eval update");
    let inc_fn = interp.eval_str("spike", "inc").expect("eval inc");

    // witnesses: first token inside first list, and the first list itself.
    let (tokennode_witness, seqnode_witness) = if let Value::Vector(top) = &forms {
        let first_list = top.get(0).unwrap().clone();
        let token_w = if let Value::Inst(inst) = &first_list {
            inst.data
                .get(&Value::Keyword(Keyword::construct("children")))
                .and_then(|c| if let Value::Vector(v) = c { v.get(0) } else { None })
                .unwrap()
                .clone()
        } else {
            panic!("expected SeqNode")
        };
        (token_w, first_list)
    } else {
        panic!("forms is not a vector")
    };

    let cache = Cache {
        tokennode_witness,
        seqnode_witness,
        kw_children: Value::Keyword(Keyword::construct("children")),
        kw_tokens: Value::Keyword(Keyword::construct("tokens")),
        kw_depth: Value::Keyword(Keyword::construct("depth")),
        kw_config: Value::Keyword(Keyword::construct("config")),
        kw_lang: Value::Keyword(Keyword::construct("lang")),
        kw_tag_star: Value::Keyword(Keyword::construct("tag*")),
        update_fn,
        inc_fn,
    };
    (interp, forms, cache)
}

fn init_ctx() -> Value {
    let mut cfg = mova::internal::PMap::new();
    cfg.insert(Value::Keyword(Keyword::construct("lang")), Value::Keyword(Keyword::construct("clj")));
    let mut ctx = mova::internal::PMap::new();
    ctx.insert(Value::Keyword(Keyword::construct("tokens")), Value::Int(0));
    ctx.insert(Value::Keyword(Keyword::construct("depth")), Value::Int(0));
    ctx.insert(Value::Keyword(Keyword::construct("config")), Value::Map(cfg));
    Value::Map(ctx)
}

/// The get-in read every node pays: `(get-in ctx [:config :lang])`,
/// inlined to its constant-depth shape (two `PMap::get`s) -- what a JIT
/// would emit for a literal 2-key path, no generic path-vector walk.
#[inline(always)]
fn get_in_config_lang(ctx_map: &mova::internal::PMap, c: &Cache) {
    if let Some(Value::Map(cfg)) = ctx_map.get(&c.kw_config) {
        black_box(cfg.get(&c.kw_lang));
    }
}

/// C: `inc` inlined (constant +1 on the owned `Value::Int`).
fn analyze_c(ctx: Value, node: &Value, c: &Cache) -> Value {
    let Value::Map(mut ctx_map) = ctx else { unreachable!() };
    get_in_config_lang(&ctx_map, c);

    if is_same_record_type(node, &c.tokennode_witness) {
        // (tag node) => :token constant, no field read needed (real impl
        // body is a constant keyword literal).
        let old = match ctx_map.get(&c.kw_tokens) {
            Some(Value::Int(n)) => *n,
            _ => unreachable!(),
        };
        ctx_map.insert(c.kw_tokens.clone(), Value::Int(old + 1));
        Value::Map(ctx_map)
    } else {
        debug_assert!(is_same_record_type(node, &c.seqnode_witness));
        let Value::Inst(inst) = node else { unreachable!() };
        // (tag node) => field read of `tag*` (real impl body).
        black_box(inst.data.get(&c.kw_tag_star));

        let old_depth = match ctx_map.get(&c.kw_depth) {
            Some(Value::Int(n)) => *n,
            _ => unreachable!(),
        };
        ctx_map.insert(c.kw_depth.clone(), Value::Int(old_depth + 1));
        let mut acc = Value::Map(ctx_map);

        if let Some(Value::Vector(children)) = inst.data.get(&c.kw_children) {
            for i in 0..children.len() {
                let child = children.get(i).unwrap();
                acc = analyze_c(acc, child, c);
            }
        }
        acc
    }
}

/// C2: same as C, but the `:token` branch calls the REAL `update` builtin
/// (its `NativeFn.f`) with `inc` passed as a first-class fn value, so
/// `update`'s own body does the generic `Interp::call_owned(inc, [cur])`
/// apply dispatch instead of C's inlined `+1`.
fn analyze_c2(ctx: Value, node: &Value, c: &Cache, interp: &mut Interp) -> Value {
    if let Value::Map(m) = &ctx {
        get_in_config_lang(m, c);
    }

    if is_same_record_type(node, &c.tokennode_witness) {
        let Value::Native(nf) = &c.update_fn else { unreachable!() };
        let args = [ctx, c.kw_tokens.clone(), c.inc_fn.clone()];
        (nf.f)(interp, &args).expect("update via apply path")
    } else {
        debug_assert!(is_same_record_type(node, &c.seqnode_witness));
        let Value::Inst(inst) = node else { unreachable!() };
        black_box(inst.data.get(&c.kw_tag_star));

        let Value::Map(mut ctx_map) = ctx else { unreachable!() };
        let old_depth = match ctx_map.get(&c.kw_depth) {
            Some(Value::Int(n)) => *n,
            _ => unreachable!(),
        };
        ctx_map.insert(c.kw_depth.clone(), Value::Int(old_depth + 1));
        let mut acc = Value::Map(ctx_map);

        if let Some(Value::Vector(children)) = inst.data.get(&c.kw_children) {
            for i in 0..children.len() {
                let child = children.get(i).unwrap();
                acc = analyze_c2(acc, child, c, interp);
            }
        }
        acc
    }
}

fn median5(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[2]
}

fn run_forms<F: FnMut(Value, &Value) -> Value>(mut analyze: F, forms: &Value, ctx0: &dyn Fn() -> Value) -> i64 {
    let mut ctx = ctx0();
    if let Value::Vector(top) = forms {
        for i in 0..top.len() {
            let node = top.get(i).unwrap();
            ctx = analyze(ctx, node);
        }
    }
    if let Value::Map(m) = &ctx {
        if let Some(Value::Int(n)) = m.get(&Value::Keyword(Keyword::construct("tokens"))) {
            return *n;
        }
    }
    -1
}

#[test]
#[ignore]
fn native_floor_c() {
    let (_interp, forms, cache) = init_ident_and_cache();

    for _ in 0..200 {
        black_box(run_forms(|ctx, n| analyze_c(ctx, n, &cache), &forms, &init_ctx));
    }
    let mut rounds = Vec::with_capacity(5);
    let mut checksum = 0;
    for _ in 0..5 {
        let t0 = Instant::now();
        for _ in 0..REPS {
            checksum = run_forms(|ctx, n| analyze_c(ctx, n, &cache), &forms, &init_ctx);
            black_box(checksum);
        }
        rounds.push(t0.elapsed().as_secs_f64() / REPS as f64 / NODE_COUNT as f64 * 1e9);
    }
    println!("C  ns/node median: {:.2}  checksum: {}", median5(rounds), checksum);
}

#[test]
#[ignore]
fn native_floor_c2() {
    let (mut interp, forms, cache) = init_ident_and_cache();

    for _ in 0..200 {
        let ctx0 = init_ctx();
        black_box(run_forms(|ctx, n| analyze_c2(ctx, n, &cache, &mut interp), &forms, &|| ctx0.clone()));
    }
    let mut rounds = Vec::with_capacity(5);
    let mut checksum = 0;
    for _ in 0..5 {
        let t0 = Instant::now();
        for _ in 0..REPS {
            let ctx0 = init_ctx();
            checksum = run_forms(|ctx, n| analyze_c2(ctx, n, &cache, &mut interp), &forms, &|| ctx0.clone());
            black_box(checksum);
        }
        rounds.push(t0.elapsed().as_secs_f64() / REPS as f64 / NODE_COUNT as f64 * 1e9);
    }
    println!("C2 ns/node median: {:.2}  checksum: {}", median5(rounds), checksum);
}
