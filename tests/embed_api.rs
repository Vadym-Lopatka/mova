//! API-completeness check for `mova::embed`: every test in this file uses
//! ONLY `mova::embed::*` -- no `mova::value`/`mova::eval`/`mova::error`
//! -- so a compile failure here means the facade doesn't cover something a
//! real embedder needs.

use mova::embed::{Engine, Profile, Value, ValueKind};

#[test]
fn eval_arithmetic_and_read_through_accessors() {
    let mut engine = Engine::builder().build();

    let v = engine.eval("(+ 1 2 3)").unwrap();
    assert_eq!(v.kind(), ValueKind::Int);
    assert_eq!(v.as_i64(), Some(6));
    assert_eq!(v.as_f64(), Some(6.0));

    let v = engine.eval("(/ 1.0 4)").unwrap();
    assert_eq!(v.kind(), ValueKind::Float);
    assert_eq!(v.as_f64(), Some(0.25));

    let v = engine.eval("\"hello\"").unwrap();
    assert_eq!(v.as_str(), Some("hello"));

    let v = engine.eval("nil").unwrap();
    assert_eq!(v.kind(), ValueKind::Nil);

    let v = engine.eval("true").unwrap();
    assert_eq!(v.as_bool(), Some(true));
}

#[test]
fn eval_data_structures_and_iterate_them() {
    let mut engine = Engine::builder().build();

    let v = engine.eval("[1 2 3]").unwrap();
    assert_eq!(v.kind(), ValueKind::Vector);
    assert_eq!(v.len(), 3);
    let elems: Vec<i64> = v.iter().map(|e| e.as_i64().unwrap()).collect();
    assert_eq!(elems, vec![1, 2, 3]);

    let v = engine.eval("(list :a :b)").unwrap();
    assert_eq!(v.kind(), ValueKind::List);
    assert_eq!(v.len(), 2);

    let v = engine.eval("#{1 2 3}").unwrap();
    assert_eq!(v.kind(), ValueKind::Set);
    assert_eq!(v.len(), 3);

    let v = engine.eval("{:a 1 :b 2}").unwrap();
    assert_eq!(v.kind(), ValueKind::Map);
    assert_eq!(v.len(), 2);
    let mut sum = 0;
    for (_k, val) in v.entries() {
        sum += val.as_i64().unwrap();
    }
    assert_eq!(sum, 3);
}

#[test]
fn conversions_from_and_try_from() {
    let a: Value = 42i64.into();
    assert_eq!(a.as_i64(), Some(42));
    let b: Value = 3.5f64.into();
    assert_eq!(b.as_f64(), Some(3.5));
    let c: Value = true.into();
    assert_eq!(c.as_bool(), Some(true));
    let d: Value = "hi".into();
    assert_eq!(d.as_str(), Some("hi"));
    let e: Value = String::from("owned").into();
    assert_eq!(e.as_str(), Some("owned"));
    let n: Value = ().into();
    assert_eq!(n.kind(), ValueKind::Nil);

    let vec_val: Value = vec![Value::from(1i64), Value::from(2i64)].into();
    assert_eq!(vec_val.kind(), ValueKind::Vector);
    assert_eq!(vec_val.len(), 2);

    let i: i64 = (&a).try_into().unwrap();
    assert_eq!(i, 42);
    let f: f64 = (&b).try_into().unwrap();
    assert_eq!(f, 3.5);
    let bo: bool = (&c).try_into().unwrap();
    assert!(bo);
    let s: String = (&d).try_into().unwrap();
    assert_eq!(s, "hi");

    // A mismatched conversion is an `Err`, not a panic.
    let bad: Result<i64, _> = (&d).try_into();
    assert!(bad.is_err());
}

#[test]
fn as_keyword_disambiguates_keyword_str_and_symbol() {
    let mut engine = Engine::builder().build();

    let kw = engine.eval(":widget").unwrap();
    assert_eq!(kw.kind(), ValueKind::Keyword);
    assert_eq!(kw.as_keyword(), Some("widget"));
    // Bare name, no leading ":" -- the exact paper cut this closes.
    assert_eq!(kw.as_str(), None);

    let s = engine.eval("\"widget\"").unwrap();
    assert_eq!(s.as_str(), Some("widget"));
    assert_eq!(s.as_keyword(), None);

    let sym = engine.eval("'widget").unwrap();
    assert_eq!(sym.kind(), ValueKind::Symbol);
    assert_eq!(sym.as_keyword(), None);
    assert_eq!(sym.as_str(), None);
}

#[test]
fn get_and_get_kw_on_maps_vectors_and_nested_data() {
    let mut engine = Engine::builder().build();

    // Map: hit and miss, both plain `get` and the `get_kw` convenience.
    let m = engine.eval("{:a 1 :b 2}").unwrap();
    assert_eq!(m.get(&Value::keyword("a")).and_then(|v| v.as_i64()), Some(1));
    assert_eq!(m.get_kw("a").and_then(|v| v.as_i64()), Some(1));
    assert!(m.get_kw("nope").is_none());
    assert!(m.get(&Value::from(1i64)).is_none());

    // Vector: in-range, negative, and out-of-range indices.
    let v = engine.eval("[10 20 30]").unwrap();
    assert_eq!(v.get(&Value::from(0i64)).and_then(|x| x.as_i64()), Some(10));
    assert_eq!(v.get(&Value::from(2i64)).and_then(|x| x.as_i64()), Some(30));
    assert!(v.get(&Value::from(3i64)).is_none());
    assert!(v.get(&Value::from(-1i64)).is_none());
    // Non-int key on a vector.
    assert!(v.get(&Value::keyword("a")).is_none());

    // List: same index semantics as a vector.
    let l = engine.eval("(list :x :y)").unwrap();
    assert_eq!(l.get(&Value::from(1i64)).and_then(|x| x.as_keyword().map(String::from)), Some("y".to_string()));
    assert!(l.get(&Value::from(2i64)).is_none());

    // Nested access: get on a map found inside a vector.
    let nested = engine.eval("[{:name \"gizmo\"} {:name \"widget\"}]").unwrap();
    let first = nested.get(&Value::from(0i64)).unwrap();
    assert_eq!(first.get_kw("name").and_then(|x| x.as_str().map(String::from)), Some("gizmo".to_string()));
    let second = nested.get(&Value::from(1i64)).unwrap();
    assert_eq!(second.get_kw("name").and_then(|x| x.as_str().map(String::from)), Some("widget".to_string()));

    // Set membership.
    let set = engine.eval("#{:a :b :c}").unwrap();
    assert_eq!(set.get(&Value::keyword("a")), Some(Value::keyword("a")));
    assert!(set.get(&Value::keyword("z")).is_none());

    // nil/scalars: always None.
    let n = engine.eval("nil").unwrap();
    assert!(n.get(&Value::from(0i64)).is_none());
    let i = engine.eval("42").unwrap();
    assert!(i.get(&Value::keyword("a")).is_none());
}

#[test]
fn define_fn_in_script_and_call_from_rust() {
    let mut engine = Engine::builder().build();
    engine.eval("(defn add2 [a b] (+ a b))").unwrap();

    let f = engine.get("add2").expect("add2 should be defined");
    assert_eq!(f.kind(), ValueKind::Fn);

    let args = vec![Value::from(10i64), Value::from(32i64)];
    let result = engine.call(&f, &args).unwrap();
    assert_eq!(result.as_i64(), Some(42));

    // `call_by_name` skips the manual `get`.
    let result = engine.call_by_name("add2", &args).unwrap();
    assert_eq!(result.as_i64(), Some(42));

    assert!(engine.get("no-such-var").is_none());
}

#[test]
fn register_fn_plain_and_namespaced() {
    let mut engine = Engine::builder().build();

    engine.register_fn("host/add", |args| {
        let a = args[0].as_i64().ok_or_else(|| mova::embed::Error::other("expected int"))?;
        let b = args[1].as_i64().ok_or_else(|| mova::embed::Error::other("expected int"))?;
        Ok(Value::from(a + b))
    });

    engine.register_fn("db/lookup", |_args| {
        Ok(Value::map(vec![
            (Value::keyword("name"), Value::from("widget")),
            (Value::keyword("qty"), Value::from(7i64)),
        ]))
    });

    let v = engine.eval("(host/add 3 4)").unwrap();
    assert_eq!(v.as_i64(), Some(7));

    let v = engine.eval("(db/lookup :widget-1)").unwrap();
    assert_eq!(v.kind(), ValueKind::Map);
    assert_eq!(v.len(), 2);

    let v = engine.eval("(:qty (db/lookup :widget-1))").unwrap();
    assert_eq!(v.as_i64(), Some(7));

    let v = engine.eval("(:name (db/lookup :widget-1))").unwrap();
    assert_eq!(v.as_str(), Some("widget"));
}

#[test]
fn pure_profile_has_no_sys_conc_or_flow() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();

    for src in ["(slurp \"x\")", "(sh \"ls\")", "(future 1)", "(chan)"] {
        let err = engine
            .eval(src)
            .expect_err(&format!("{src} should error under Profile::Pure, not succeed"));
        let msg = err.to_string();
        assert!(
            msg.contains("Unable to resolve symbol"),
            "{src} should fail as an undefined symbol under Profile::Pure, got: {msg}"
        );
    }

    // Pure-profile core surface still works.
    let v = engine.eval("(+ 1 2)").unwrap();
    assert_eq!(v.as_i64(), Some(3));
}

#[test]
fn scripting_profile_resolves_sys_conc_and_flow() {
    let mut engine = Engine::builder().profile(Profile::Scripting).build();

    // These all resolve now -- they're real natives, not undefined
    // symbols -- even though `slurp` below still fails (missing file).
    let v = engine.eval("(sh \"true\")").unwrap();
    assert_eq!(v.kind(), ValueKind::Map);

    let v = engine.eval("(future 1)").unwrap();
    assert_eq!(v.kind(), ValueKind::Other); // a future cell, not a plain scalar

    let v = engine.eval("(chan)").unwrap();
    assert_eq!(v.kind(), ValueKind::Other); // a channel

    let err = engine
        .eval("(slurp \"/definitely/does/not/exist/mova-embed-test\")")
        .expect_err("slurp of a missing file should error");
    let msg = err.to_string();
    assert!(
        !msg.contains("Unable to resolve symbol"),
        "a missing-file slurp must be a system error, not an undefined symbol: {msg}"
    );
    assert!(
        msg.contains("couldn't slurp"),
        "expected the system-error message, got: {msg}"
    );
    // The rendered diagnostic carries the errno line too (see
    // `crate::error::render`), which the bare `Display` message above
    // doesn't -- a second, independent signal that this is the sys-error
    // path, not the undefined-symbol path.
    assert!(err.render_plain().contains("errno"));
}

#[test]
fn error_rendering_has_a_span_label_marker() {
    let mut engine = Engine::builder().build();
    // Unclosed list -- a reader error with a real span.
    let err = engine.eval("(+ 1 2").expect_err("unclosed form should be a reader error");
    let rendered = err.render_plain();
    // miette's graphical span header (`,-[repl:1:1]`) plus either the
    // caret underline (`^^^`, for a bounded span) or the elbow-pointer
    // label (`` `-- ``, for an unclosed-at-EOF span like this one) --
    // either is a genuine span/label marker, not just the bare message.
    assert!(
        rendered.contains(",-["),
        "expected a miette span header in the rendered diagnostic, got:\n{rendered}"
    );
    assert!(
        rendered.contains('^') || rendered.contains("`--"),
        "expected a caret or label-pointer marker in the rendered diagnostic, got:\n{rendered}"
    );
}

#[test]
fn lazy_map_reads_like_a_map_through_accessors() {
    let mut engine = Engine::builder().build();
    let v = engine.eval(r#"(mova.edn/read-string-lazy "{:a 1 :b [2 3]}")"#).unwrap();
    assert_eq!(v.kind(), ValueKind::Map);
    assert_eq!(v.len(), 2);
    assert_eq!(v.get_kw("a").unwrap().as_i64(), Some(1));
    assert!(v.get_kw("zz").is_none());
    assert_eq!(v.entries().count(), 2);
}
