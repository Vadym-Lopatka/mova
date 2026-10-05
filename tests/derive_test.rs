//! `#[derive(MovaStruct)]` (the `derive` feature): end-to-end coverage
//! through a real `Engine`, matching `tests/hoststruct_test.rs`'s own
//! "public surface only" discipline. Exercises every attribute
//! (`#[mova(rename/skip/getter)]`) in one fixture (`DESIGN-hoststruct-
//! derive.md` §1.1's `FlatStruct`, verbatim) plus the design's §5.2
//! `Player` ergonomics example.

#![cfg(feature = "derive")]

use std::sync::Arc;

// A single `use` path brings in BOTH the `MovaStruct` trait (type
// namespace) and the `#[derive(MovaStruct)]` macro (macro namespace) --
// they share a name at the same re-export path, exactly like
// `serde::Serialize`'s trait+derive pair.
use mova::embed::host::{MovaStruct, WrapExt};
use mova::embed::{Engine, Profile, ValueKind};

/// DESIGN-hoststruct-derive.md §1.1's fixture, verbatim: 10 fields plus one
/// renamed, one skipped, one computed -- every attribute exercised at
/// once.
// `note`/`label` are read only through the derive-generated getter/skip
// paths (script-side `(:label obj)`, never `s.label` directly), not by any
// ordinary Rust code in this file -- `#[allow(dead_code)]` so that doesn't
// trip `-D warnings` under the project's clippy gate.
#[allow(dead_code)]
#[derive(Clone, MovaStruct)]
struct FlatStruct {
    id: i64,
    #[mova(rename = "display_name")]
    name: String,
    score: f64,
    active: bool,
    count: i64,
    ratio: f64,
    tag: String,
    weight: i64,
    #[mova(skip)]
    note: String, // host-internal only, never script-visible
    verified: bool,
    #[mova(getter = "full_label")]
    label: (), // computed field, no backing storage read directly
}

impl FlatStruct {
    fn full_label(&self) -> String {
        format!("{}#{}", self.tag, self.id)
    }
}

fn engine() -> Engine {
    Engine::builder().profile(Profile::Pure).build()
}

fn fixture() -> Arc<FlatStruct> {
    Arc::new(FlatStruct {
        id: 42,
        name: "Rin".into(),
        score: 9.5,
        active: true,
        count: 3,
        ratio: 0.25,
        tag: "hero".into(),
        weight: 12,
        note: "host-only, never leaks".into(),
        verified: false,
        label: (),
    })
}

#[test]
fn field_reads_via_keyword_lookup() {
    let mut engine = engine();
    let s = fixture();
    engine.def("obj", s.wrap());

    assert_eq!(engine.eval("(:id obj)").unwrap().as_i64(), Some(42));
    assert_eq!(engine.eval("(:score obj)").unwrap().as_f64(), Some(9.5));
    assert_eq!(engine.eval("(:active obj)").unwrap().as_bool(), Some(true));
    assert_eq!(engine.eval("(:count obj)").unwrap().as_i64(), Some(3));
    assert_eq!(engine.eval("(:ratio obj)").unwrap().as_f64(), Some(0.25));
    assert_eq!(engine.eval("(:tag obj)").unwrap().as_str(), Some("hero"));
    assert_eq!(engine.eval("(:weight obj)").unwrap().as_i64(), Some(12));
    assert_eq!(engine.eval("(:verified obj)").unwrap().as_bool(), Some(false));
}

#[test]
fn renamed_key_visible_original_absent() {
    let mut engine = engine();
    let s = fixture();
    engine.def("obj", s.wrap());

    // The renamed key is script-visible under `display_name`, NOT `name`.
    assert_eq!(engine.eval("(:display_name obj)").unwrap().as_str(), Some("Rin"));
    assert_eq!(engine.eval("(:name obj)").unwrap().kind(), ValueKind::Nil);
    assert_eq!(engine.eval("(contains? obj :display_name)").unwrap().as_bool(), Some(true));
    assert_eq!(engine.eval("(contains? obj :name)").unwrap().as_bool(), Some(false));
}

#[test]
fn skipped_field_invisible() {
    let mut engine = engine();
    let s = fixture();
    engine.def("obj", s.wrap());

    assert_eq!(engine.eval("(:note obj)").unwrap().kind(), ValueKind::Nil);
    assert_eq!(engine.eval("(contains? obj :note)").unwrap().as_bool(), Some(false));
    // 10 registered fields: 11 total minus the one skipped (`note`).
    assert_eq!(engine.eval("(count obj)").unwrap().as_i64(), Some(10));
}

#[test]
fn computed_getter_value_correct() {
    let mut engine = engine();
    let s = fixture();
    let expected = s.full_label();
    engine.def("obj", s.wrap());

    assert_eq!(engine.eval("(:label obj)").unwrap().as_str(), Some(expected.as_str()));
    assert_eq!(expected, "hero#42");
}

#[test]
fn equality_and_printing_are_sane() {
    let mut engine = engine();
    let s = fixture();
    engine.def("obj", Arc::clone(&s).wrap());
    engine.def("obj2", s.wrap());

    // Two wraps of the same underlying Arc are `=`.
    assert_eq!(engine.eval("(= obj obj2)").unwrap().as_bool(), Some(true));

    // A real map built from the SAME 9 keys/values is also `=` in both
    // directions (map-view conformance, matching hoststruct_test.rs).
    let as_map = engine
        .eval(
            "{:id 42 :display_name \"Rin\" :score 9.5 :active true :count 3 :ratio 0.25 \
             :tag \"hero\" :weight 12 :verified false :label \"hero#42\"}",
        )
        .unwrap();
    engine.def("m", as_map);
    assert_eq!(engine.eval("(= obj m)").unwrap().as_bool(), Some(true));
    assert_eq!(engine.eval("(= m obj)").unwrap().as_bool(), Some(true));

    // `pr-str` prints shape order, map-shaped, no crash / sane output.
    let printed = engine.eval("(pr-str obj)").unwrap();
    let printed = printed.as_str().unwrap();
    assert!(printed.starts_with('{') && printed.ends_with('}'));
    assert!(printed.contains(":id 42"));
    assert!(printed.contains(":display_name \"Rin\""));
    assert!(!printed.contains(":name"), "original field name must not leak");
    assert!(!printed.contains(":note"), "skipped field must not leak");
}

#[test]
fn shape_is_cached_across_wraps() {
    // `MovaStruct::shape()` is `OnceLock`'d -- two calls return the SAME
    // `&'static Shape`, matching DESIGN-hoststruct-derive.md §1.2's
    // "one call to `.build()` per process per type" contract.
    let a = FlatStruct::shape();
    let b = FlatStruct::shape();
    assert!(std::ptr::eq(a, b));
}

// -- DESIGN-hoststruct-derive.md §5.2's `Player` example, verbatim --------

#[derive(Clone, MovaStruct)]
struct Player {
    name: String,
    hp: i64,
    level: i64,
    inventory_size: i64,
}

#[test]
fn design_doc_player_example_verbatim() {
    let mut engine = Engine::builder().build();
    let player = Arc::new(Player {
        name: "Rin".into(),
        hp: 30,
        level: 3,
        inventory_size: 12,
    });
    engine.def("player", player.wrap()); // WrapExt::wrap

    assert_eq!(engine.eval("(:name player)").unwrap().as_str(), Some("Rin"));
    assert_eq!(engine.eval("(:hp player)").unwrap().as_i64(), Some(30));
    assert_eq!(engine.eval("(:level player)").unwrap().as_i64(), Some(3));
    assert_eq!(engine.eval("(:inventory-size player)").unwrap().kind(), ValueKind::Nil);
    assert_eq!(engine.eval("(:inventory_size player)").unwrap().as_i64(), Some(12));
}

// -- `#[mova(crate = "...")]` override, sanity check (Cargo-path hygiene) -

mod reexported {
    // Exercises the container-level override: points the generated code at
    // `::mova` via an explicit re-export path instead of the derive's
    // default (`::mova` too, today -- but this proves the override
    // mechanism itself resolves and compiles, independent of what the
    // default happens to be).
    pub use mova as my_mova;
}

#[derive(Clone, MovaStruct)]
#[mova(crate = "crate::reexported::my_mova")]
struct CratePathOverride {
    n: i64,
}

#[test]
fn crate_path_override_compiles_and_works() {
    let mut engine = engine();
    let v = Arc::new(CratePathOverride { n: 7 });
    engine.def("obj", v.wrap());
    assert_eq!(engine.eval("(:n obj)").unwrap().as_i64(), Some(7));
}
