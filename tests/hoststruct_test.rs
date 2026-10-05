//! W3 (LATENCY-CAMPAIGN.md): semantics torture suite for
//! `Value::HostStruct`/`mova::embed::host`. Every test here uses ONLY the
//! public `mova::embed`/`mova::embed::host` surface -- no
//! `mova::value`/`mova::eval` -- matching `tests/embed_api.rs`'s own
//! "a compile failure here means the facade doesn't cover something a real
//! embedder needs" discipline.
//!
//! Conformance-by-construction is checked SEPARATELY (this file never
//! touches the golden corpus): no test here relies on script syntax being
//! able to construct a `HostStruct` -- it can't, by design (see
//! `crate::host_struct`'s module doc) -- only on the host wrapping one and
//! handing it to script via `Engine::def`/`Engine::call`.

use std::sync::Arc;

use mova::embed::host::{from_value_arc, from_value_typed, wrap_struct, ShapeBuilder};
use mova::embed::{Engine, Profile, Value, ValueKind};

#[derive(Clone)]
struct Widget {
    weight: i64,
    name: String,
    active: bool,
}

fn widget_shape() -> mova::embed::host::Shape<Widget> {
    ShapeBuilder::<Widget>::new("Widget")
        .field("weight", |w| Value::from(w.weight))
        .field("name", |w| Value::from(w.name.as_str()))
        .field("active", |w| Value::from(w.active))
        .build()
}

fn engine() -> Engine {
    Engine::builder().profile(Profile::Pure).build()
}

// Compile-time assertion: a `Shape<T>`/wrapped `Value` must be usable from
// a host that is itself multi-threaded -- `Engine` is `Send` (not `Sync`,
// see its own doc), but the host TYPE `T` wrapped via `wrap_struct` must be
// `Send + Sync` for `Value` as a whole to stay `Send + Sync` (see
// `value.rs`'s `_assert_send_sync`). This function is never called; it
// exists purely so the bound fails to *compile* if it ever regresses.
#[allow(dead_code)]
fn assert_widget_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<Widget>();
    f::<mova::embed::host::Shape<Widget>>();
}

#[test]
fn equality_both_directions_and_hash_set_collision() {
    let mut engine = engine();
    let shape = widget_shape();
    let w = Arc::new(Widget {
        weight: 7,
        name: "gizmo".into(),
        active: true,
    });
    engine.def("h", wrap_struct(Arc::clone(&w), &shape));
    engine.def(
        "m",
        Value::map([
            (Value::keyword("weight"), Value::from(7i64)),
            (Value::keyword("name"), Value::from("gizmo")),
            (Value::keyword("active"), Value::from(true)),
        ]),
    );

    // `=` both directions.
    assert_eq!(engine.eval("(= h m)").unwrap().as_bool(), Some(true));
    assert_eq!(engine.eval("(= m h)").unwrap().as_bool(), Some(true));
    // `HostStruct`/`HostStruct` too (self-equality via a second wrap of
    // the SAME underlying Arc).
    engine.def("h2", wrap_struct(Arc::clone(&w), &shape));
    assert_eq!(engine.eval("(= h h2)").unwrap().as_bool(), Some(true));

    // A differing field must make it NOT `=` in both directions.
    let w2 = Arc::new(Widget {
        weight: 8,
        name: "gizmo".into(),
        active: true,
    });
    engine.def("h3", wrap_struct(w2, &shape));
    assert_eq!(engine.eval("(= h3 m)").unwrap().as_bool(), Some(false));
    assert_eq!(engine.eval("(= m h3)").unwrap().as_bool(), Some(false));

    // Hash-as-key collision: `Value::Set` is always CHAMP-hashed (no
    // small/big split), so `(conj #{} m h)` collapsing to ONE element
    // genuinely exercises `Hash`, not just `Eq` -- see
    // `host_struct::as_pmap`'s Hash-tag-matching doc.
    let count = engine.eval("(count (conj #{} m h))").unwrap();
    assert_eq!(count.as_i64(), Some(1));

    // And as an actual map KEY (a real `{m :found}` map, looked up by the
    // `HostStruct`).
    let m_val = engine.get("m").unwrap();
    engine.def("keyed", Value::map([(m_val, Value::from("found"))]));
    let found = engine.eval("(get keyed h)").unwrap();
    assert_eq!(found.as_str(), Some("found"));
}

#[test]
fn seq_keys_vals_print_are_shape_order() {
    let mut engine = engine();
    let shape = widget_shape();
    let w = Arc::new(Widget {
        weight: 7,
        name: "gizmo".into(),
        active: true,
    });
    engine.def("h", wrap_struct(w, &shape));

    let keys = engine.eval("(keys h)").unwrap();
    let key_names: Vec<String> = keys.iter().map(|k| format!("{k}")).collect();
    assert_eq!(key_names, vec![":weight", ":name", ":active"]);

    // `embed::Value`'s `Display` goes through `pr_str` (readable form, see
    // `embed/value.rs`), so a string element prints quoted here.
    let vals = engine.eval("(vals h)").unwrap();
    let val_strs: Vec<String> = vals.iter().map(|v| format!("{v}")).collect();
    assert_eq!(val_strs, vec!["7", "\"gizmo\"", "true"]);

    // `seq` over a HostStruct is a seq of [k v] pairs, shape order.
    // (`vec` forces `map`'s lazy-seq eagerly -- `embed::Value::iter` only
    // walks an already-realized `List`/`Vector`/`Set`, see that method's
    // doc.)
    let s = engine.eval("(vec (map first (seq h)))").unwrap();
    let seq_keys: Vec<String> = s.iter().map(|k| format!("{k}")).collect();
    assert_eq!(seq_keys, vec![":weight", ":name", ":active"]);

    // print (`pr-str`) -- shape order, not the generic `Map` printer's
    // alphabetical-by-key sort (see `printer.rs`'s `HostStruct` arm).
    // Nested values inside a collection print quoted even under `str`
    // (Clojure's own behavior: `(str {:a "x"})` => `"{:a \"x\"}"`) --
    // `pr-str`/`str` on `h` itself agree, both shape-order.
    let printed = engine.eval("(pr-str h)").unwrap();
    assert_eq!(printed.as_str(), Some("{:weight 7, :name \"gizmo\", :active true}"));
    let displayed = engine.eval("(str h)").unwrap();
    assert_eq!(displayed.as_str(), Some("{:weight 7, :name \"gizmo\", :active true}"));
}

#[test]
fn merge_into_reduce_kv_and_destructuring() {
    let mut engine = engine();
    let shape = widget_shape();
    let w = Arc::new(Widget {
        weight: 7,
        name: "gizmo".into(),
        active: true,
    });
    engine.def("h", wrap_struct(w, &shape));

    // merge: HostStruct as an argument, widened into a real Map.
    let merged = engine.eval("(merge h {:extra :yes})").unwrap();
    assert_eq!(merged.kind(), ValueKind::Map);
    assert_eq!(merged.len(), 4);

    // into: HostStruct as the destination collection.
    let into_res = engine.eval("(into h [[:extra :yes]])").unwrap();
    assert_eq!(into_res.kind(), ValueKind::Map);
    assert_eq!(into_res.len(), 4);

    // reduce-kv: shape order, matching seq/keys/vals.
    let names = engine.eval("(reduce-kv (fn [acc k v] (conj acc k)) [] h)").unwrap();
    let names: Vec<String> = names.iter().map(|k| format!("{k}")).collect();
    assert_eq!(names, vec![":weight", ":name", ":active"]);

    // `{:keys [...]}` destructuring.
    let bound = engine.eval("(let [{:keys [weight name]} h] [weight name])").unwrap();
    assert_eq!(bound.iter().next().unwrap().as_i64(), Some(7));

    // fn-param destructuring too.
    let extractor = engine.eval("(fn [{:keys [name]}] name)").unwrap();
    engine.def("f", extractor);
    let r = engine.eval("(f h)").unwrap();
    assert_eq!(r.as_str(), Some("gizmo"));
}

#[test]
fn assoc_widens_to_map_original_unchanged() {
    let mut engine = engine();
    let shape = widget_shape();
    let w = Arc::new(Widget {
        weight: 7,
        name: "gizmo".into(),
        active: true,
    });
    engine.def("h", wrap_struct(Arc::clone(&w), &shape));

    let widened = engine.eval("(assoc h :extra :new)").unwrap();
    assert_eq!(widened.kind(), ValueKind::Map);
    assert_eq!(widened.len(), 4);

    // The ORIGINAL `h` global is untouched: still HostStruct-kinded (well,
    // reports ValueKind::Map -- see below), still 3 fields, no `:extra`.
    let h_again = engine.eval("h").unwrap();
    assert_eq!(h_again.len(), 3);
    assert_eq!(engine.eval("(:extra h)").unwrap().kind(), ValueKind::Nil);

    // dissoc: also widens.
    let shrunk = engine.eval("(dissoc h :active)").unwrap();
    assert_eq!(shrunk.kind(), ValueKind::Map);
    assert_eq!(shrunk.len(), 2);
    assert_eq!(engine.eval("(count h)").unwrap().as_i64(), Some(3));
}

#[test]
fn identical_string_field_reads_are_stable() {
    let mut engine = engine();
    let shape = widget_shape();
    let w = Arc::new(Widget {
        weight: 7,
        name: "gizmo".into(),
        active: true,
    });
    engine.def("h", wrap_struct(w, &shape));
    // Two reads of the same String-valued field, through script, must be
    // `identical?` (same `Arc<StrInner>` allocation) -- the leaf-slab
    // cache's whole point (see `host_struct::HostStructInner::leaf_slab`).
    let r = engine.eval("(identical? (:name h) (:name h))").unwrap();
    assert_eq!(r.as_bool(), Some(true));
}

#[test]
fn map_predicate_type_name_count_contains_get() {
    let mut engine = engine();
    let shape = widget_shape();
    let w = Arc::new(Widget {
        weight: 7,
        name: "gizmo".into(),
        active: true,
    });
    engine.def("h", wrap_struct(w, &shape));

    assert_eq!(engine.eval("(map? h)").unwrap().as_bool(), Some(true));
    assert_eq!(engine.eval("(coll? h)").unwrap().as_bool(), Some(true));
    assert_eq!(engine.eval("(count h)").unwrap().as_i64(), Some(3));
    assert_eq!(engine.eval("(contains? h :weight)").unwrap().as_bool(), Some(true));
    assert_eq!(engine.eval("(contains? h :nope)").unwrap().as_bool(), Some(false));
    assert_eq!(engine.eval("(get h :weight)").unwrap().as_i64(), Some(7));
    assert_eq!(engine.eval("(get h :nope \"default\")").unwrap().as_str(), Some("default"));
    assert_eq!(engine.eval("(:weight h)").unwrap().as_i64(), Some(7));
    assert_eq!(engine.eval("(empty? h)").unwrap().as_bool(), Some(false));
    let emptied = engine.eval("(empty h)").unwrap();
    assert_eq!(emptied.kind(), ValueKind::Map);
    assert_eq!(emptied.len(), 0);
}

#[test]
fn embed_value_get_and_get_kw_reach_shape_fields() {
    let mut engine = engine();
    let shape = widget_shape();
    let w = Arc::new(Widget {
        weight: 7,
        name: "gizmo".into(),
        active: true,
    });
    engine.def("h", wrap_struct(w, &shape));
    let h = engine.eval("h").unwrap();

    // `Value::get`/`get_kw` on a `HostStruct` go through the same
    // `crate::host_struct::lookup` shape-dispatch `entries()`/script's own
    // `(:kw h)` use -- no materialize.
    assert_eq!(h.get(&Value::keyword("weight")).and_then(|v| v.as_i64()), Some(7));
    assert_eq!(h.get_kw("name").and_then(|v| v.as_str().map(String::from)), Some("gizmo".to_string()));
    assert_eq!(h.get_kw("active").and_then(|v| v.as_bool()), Some(true));
    // A field that doesn't exist on the shape.
    assert!(h.get_kw("nope").is_none());
    // A non-keyword key on a HostStruct is always `None`, same as a map.
    assert!(h.get(&Value::from(0i64)).is_none());
}

#[test]
fn typed_extraction_ref_and_arc() {
    let shape = widget_shape();
    let w = Arc::new(Widget {
        weight: 7,
        name: "gizmo".into(),
        active: true,
    });
    let v = wrap_struct(Arc::clone(&w), &shape);

    let cloned: Widget = from_value_typed(&v).expect("still a HostStruct<Widget>");
    assert_eq!(cloned.weight, 7);

    let arc: Arc<Widget> = from_value_arc(&v).expect("still a HostStruct<Widget>");
    assert_eq!(arc.weight, 7);
    assert!(Arc::ptr_eq(&arc, &w));

    // After a script `assoc`, the result is a plain Map -- typed
    // extraction must NOT misfire (return `None`, not a wrongly-typed
    // clone of stale data).
    let mut engine = engine();
    engine.def("h", v);
    let widened = engine.eval("(assoc h :extra 1)").unwrap();
    assert!(from_value_typed::<Widget>(&widened).is_none());
    assert!(from_value_arc::<Widget>(&widened).is_none());

    // A `HostStruct` of a DIFFERENT host type must also fail cleanly.
    #[derive(Clone)]
    struct Other {
        n: i64,
    }
    let other_shape = ShapeBuilder::<Other>::new("Other").field("n", |o| Value::from(o.n)).build();
    let other_v = wrap_struct(Arc::new(Other { n: 1 }), &other_shape);
    assert!(from_value_typed::<Widget>(&other_v).is_none());
}
