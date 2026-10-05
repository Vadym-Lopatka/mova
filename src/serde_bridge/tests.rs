//! Round-trip and edge-case tests for the `serde` bridge (`super::to_value`/
//! `super::from_value`), plus a cross-boundary test that actually runs a
//! mova script over a `to_value`d struct and `from_value`s the script's
//! output back. See `src/serde_bridge.rs`'s module doc for the encoding
//! contract these tests are pinning down.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{from_value, to_value, SerdeError};
use crate::eval::Interp;
use crate::value::{Symbol, Value};
use crate::{pmap, pvec};

// ------------------------------- fixtures --------------------------------

/// A flat, 10-leaf-field struct -- (a) in the perf brief's measurement
/// list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FlatStruct {
    id: u64,
    name: String,
    score: f64,
    active: bool,
    count: i32,
    ratio: f32,
    tag: char,
    weight: i64,
    note: String,
    verified: bool,
}

fn flat_fixture() -> FlatStruct {
    FlatStruct {
        id: 42,
        name: "widget-9000".to_string(),
        score: 98.6,
        active: true,
        count: -17,
        ratio: 0.5,
        tag: 'Q',
        weight: 123_456_789,
        note: "hello, world".to_string(),
        verified: false,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LineItem {
    sku: String,
    qty: i32,
    unit_price: f64,
    gift_wrapped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct UnitMarker;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Status {
    Pending,
    Cancelled(String),
    Delayed(u32, String),
    Shipped { carrier: String, tracking: String },
}

/// The order/event-style nested fixture -- (b) in the perf brief's
/// measurement list, and the shape `nested_fixture_round_trip` below pins
/// down: strings, ints, floats, bools, `Option`, `Vec<nested struct>`,
/// `HashMap<String, T>`, an enum with unit/newtype/tuple/struct variants,
/// a unit struct, and a tuple -- roughly 50 leaf `Value`s once flattened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OrderEvent {
    order_id: u64,
    customer: String,
    total: f64,
    paid: bool,
    discount_code: Option<String>,
    gift_note: Option<String>,
    tags: Vec<String>,
    items: Vec<LineItem>,
    attributes: HashMap<String, String>,
    status: Status,
    marker: UnitMarker,
    warehouse_coords: (f64, f64, f64),
}

fn nested_fixture() -> OrderEvent {
    let mut attributes = HashMap::new();
    attributes.insert("source".to_string(), "web".to_string());
    attributes.insert("campaign".to_string(), "spring-sale".to_string());
    attributes.insert("referrer".to_string(), "newsletter".to_string());
    OrderEvent {
        order_id: 908_123,
        customer: "Ada Lovelace".to_string(),
        total: 249.99,
        paid: true,
        discount_code: Some("SPRING10".to_string()),
        gift_note: None,
        tags: vec!["priority".to_string(), "gift".to_string(), "fragile".to_string()],
        items: vec![
            LineItem {
                sku: "SKU-1".to_string(),
                qty: 2,
                unit_price: 19.99,
                gift_wrapped: true,
            },
            LineItem {
                sku: "SKU-2".to_string(),
                qty: 1,
                unit_price: 209.99,
                gift_wrapped: false,
            },
            LineItem {
                sku: "SKU-3".to_string(),
                qty: 4,
                unit_price: 0.01,
                gift_wrapped: false,
            },
        ],
        attributes,
        status: Status::Shipped {
            carrier: "UPS".to_string(),
            tracking: "1Z999AA10123456784".to_string(),
        },
        marker: UnitMarker,
        warehouse_coords: (37.7749, -122.4194, 16.0),
    }
}

// ---------------------------- round-trip tests ----------------------------

#[test]
fn flat_struct_round_trip() {
    let original = flat_fixture();
    let value = to_value(&original).expect("to_value");
    let back: FlatStruct = from_value(&value).expect("from_value");
    assert_eq!(original, back);
}

#[test]
fn nested_fixture_round_trip() {
    let original = nested_fixture();
    let value = to_value(&original).expect("to_value");
    let back: OrderEvent = from_value(&value).expect("from_value");
    assert_eq!(original, back);
}

#[test]
fn nested_fixture_each_status_variant_round_trips() {
    for status in [
        Status::Pending,
        Status::Cancelled("out of stock".to_string()),
        Status::Delayed(3, "weather".to_string()),
        Status::Shipped {
            carrier: "FedEx".to_string(),
            tracking: "abc123".to_string(),
        },
    ] {
        let mut ev = nested_fixture();
        ev.status = status.clone();
        let value = to_value(&ev).expect("to_value");
        let back: OrderEvent = from_value(&value).expect("from_value");
        assert_eq!(ev, back, "status variant {status:?} did not round-trip");
    }
}

#[test]
fn encoding_shapes_match_the_documented_contract() {
    // Unit variant -> bare Keyword.
    assert_eq!(to_value(&Status::Pending).unwrap(), Value::Keyword("Pending".into()));
    // Newtype variant -> single-entry Map, Keyword tag, payload as-is.
    assert_eq!(
        to_value(&Status::Cancelled("x".to_string())).unwrap(),
        Value::Map(pmap! { Value::Keyword("Cancelled".into()) => Value::Str("x".into()) })
    );
    // Tuple variant -> single-entry Map, payload is a Vector.
    assert_eq!(
        to_value(&Status::Delayed(3, "weather".to_string())).unwrap(),
        Value::Map(pmap! {
            Value::Keyword("Delayed".into()) => Value::Vector(pvec![Value::Int(3), Value::Str("weather".into())])
        })
    );
    // Struct variant -> single-entry Map, payload is a field Map.
    assert_eq!(
        to_value(&Status::Shipped {
            carrier: "UPS".to_string(),
            tracking: "T1".to_string()
        })
        .unwrap(),
        Value::Map(pmap! {
            Value::Keyword("Shipped".into()) => Value::Map(pmap! {
                Value::Keyword("carrier".into()) => Value::Str("UPS".into()),
                Value::Keyword("tracking".into()) => Value::Str("T1".into())
            })
        })
    );
    // Unit struct -> Nil.
    assert_eq!(to_value(&UnitMarker).unwrap(), Value::Nil);
    // Struct fields -> Keyword keys, verbatim (not kebab-cased).
    let li = LineItem {
        sku: "S".to_string(),
        qty: 1,
        unit_price: 2.5,
        gift_wrapped: false,
    };
    let Value::Map(m) = to_value(&li).unwrap() else {
        panic!("expected a Map")
    };
    assert!(m.contains_key(&Value::Keyword("unit_price".into())), "field key must be verbatim `unit_price`, not kebab-cased");
}

// ------------------------------ edge cases --------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct HasU64 {
    big: u64,
}

#[test]
fn u64_within_i64_range_round_trips() {
    let v = to_value(&HasU64 { big: 42 }).expect("to_value");
    assert_eq!(v, Value::Map(pmap! { Value::Keyword("big".into()) => Value::Int(42) }));
    let back: HasU64 = from_value(&v).expect("from_value");
    assert_eq!(back.big, 42);
}

#[test]
fn u64_overflowing_i64_errors() {
    let err = to_value(&HasU64 { big: u64::MAX }).expect_err("u64::MAX must not silently fit mova's i64 Int");
    match err {
        SerdeError::U64Overflow(v) => assert_eq!(v, u64::MAX),
        other => panic!("expected U64Overflow, got {other:?}"),
    }

    // The boundary value: i64::MAX fits, i64::MAX + 1 doesn't.
    assert!(to_value(&HasU64 { big: i64::MAX as u64 }).is_ok());
    let err = to_value(&HasU64 { big: (i64::MAX as u64) + 1 }).unwrap_err();
    assert!(matches!(err, SerdeError::U64Overflow(_)));
}

#[test]
fn u64_overflow_also_rejected_bare_not_just_in_a_struct() {
    let err = to_value(&u64::MAX).unwrap_err();
    assert!(matches!(err, SerdeError::U64Overflow(v) if v == u64::MAX));
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct HasI64 {
    v: i64,
}

#[test]
fn i64_min_max_round_trip() {
    for v in [i64::MIN, i64::MAX, 0, -1, 1] {
        let value = to_value(&HasI64 { v }).unwrap();
        let back: HasI64 = from_value(&value).unwrap();
        assert_eq!(back, HasI64 { v });
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct HasF64 {
    v: f64,
}

#[test]
fn nan_bits_round_trip_exactly() {
    // See the module doc's "NaN / infinity" section: `Value`'s own
    // `PartialEq` compares floats by `to_bits`, so NaN IS `==` to itself
    // at the `Value` level -- but a derived `PartialEq` on the Rust struct
    // uses real IEEE-754 comparison (`NaN != NaN`), so the correct
    // round-trip check here is bit-for-bit, not `==`.
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0_f64, 0.0_f64] {
        let value = to_value(&HasF64 { v }).unwrap();
        assert_eq!(value, Value::Map(pmap! { Value::Keyword("v".into()) => Value::Float(v) }));
        let back: HasF64 = from_value(&value).unwrap();
        assert_eq!(back.v.to_bits(), v.to_bits(), "bit pattern must round-trip exactly for {v}");
    }
}

#[test]
fn int_widens_into_f64_field() {
    // A script-built map is free to use an Int where a Rust f64 field is
    // expected (e.g. `{:v 3}` instead of `{:v 3.0}`) -- `from_value` must
    // widen it rather than erroring. See the module doc's "encoding
    // contract" table.
    let value = Value::Map(pmap! { Value::Keyword("v".into()) => Value::Int(3) });
    let back: HasF64 = from_value(&value).expect("Int must widen into an f64 field");
    assert_eq!(back.v, 3.0);
}

// Recursive structure, used to check depth-100 nesting doesn't blow the
// stack in either direction -- (see the module doc's "Depth" section).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Nested {
    Leaf(i64),
    Node(Box<Nested>),
}

fn build_nested(depth: usize) -> Nested {
    if depth == 0 {
        Nested::Leaf(0)
    } else {
        Nested::Node(Box::new(build_nested(depth - 1)))
    }
}

#[test]
fn deeply_nested_round_trip() {
    let original = build_nested(100);
    let value = to_value(&original).expect("to_value at depth 100");
    let back: Nested = from_value(&value).expect("from_value at depth 100");
    assert_eq!(original, back);
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct SmallStruct {
    sku: String,
    qty: i32,
}

#[test]
fn unknown_map_keys_are_ignored_by_default() {
    let m = Value::Map(pmap! {
        Value::Keyword("sku".into()) => Value::Str("S1".into()),
        Value::Keyword("qty".into()) => Value::Int(5),
        Value::Keyword("totally-unknown-debug-key".into()) => Value::Str("ignore me".into())
    });
    let back: SmallStruct = from_value(&m).expect("unknown keys must not error");
    assert_eq!(
        back,
        SmallStruct {
            sku: "S1".to_string(),
            qty: 5
        }
    );
}

#[test]
fn struct_fields_accept_either_keyword_or_str_keys() {
    // A script may build a map with either `:sku`/`:qty` keyword keys, or
    // plain string keys via `(assoc {} "sku" ... "qty" ...)` -- both must
    // deserialize into the same struct (module doc, "map keys" section).
    let mixed = Value::Map(pmap! {
        Value::Str("sku".into()) => Value::Str("S2".into()),
        Value::Keyword("qty".into()) => Value::Int(7)
    });
    let back: SmallStruct = from_value(&mixed).expect("mixed Str/Keyword keys must both be accepted");
    assert_eq!(
        back,
        SmallStruct {
            sku: "S2".to_string(),
            qty: 7
        }
    );
}

#[test]
fn missing_option_field_deserializes_as_none() {
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct WithOption {
        name: String,
        nickname: Option<String>,
    }
    let m = Value::Map(pmap! { Value::Keyword("name".into()) => Value::Str("Ada".into()) });
    let back: WithOption = from_value(&m).expect("missing Option field must default to None, not error");
    assert_eq!(
        back,
        WithOption {
            name: "Ada".to_string(),
            nickname: None
        }
    );
}

/// A newtype that routes through `Serializer::serialize_bytes`/
/// `Deserializer::deserialize_bytes` directly (`Vec<u8>`'s own `Serialize`
/// impl goes through the ordinary sequence path instead -- each byte as
/// its own `u8`/`Int` element -- unless wrapped with the external
/// `serde_bytes` crate, which this probe doesn't depend on; this newtype
/// exercises the SAME `serialize_bytes`/`deserialize_bytes` methods
/// `serde_bytes` would call, without adding the dependency).
struct Bytes(Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;
        impl<'de> serde::de::Visitor<'de> for BytesVisitor {
            type Value = Bytes;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a byte buffer")
            }
            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes(v))
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes(v.to_vec()))
            }
        }
        deserializer.deserialize_bytes(BytesVisitor)
    }
}

#[test]
fn bytes_round_trip_as_a_vector_of_ints() {
    let original = Bytes(vec![0, 1, 255, 128, 42]);
    let value = to_value(&original).expect("to_value");
    assert_eq!(
        value,
        Value::Vector(pvec![Value::Int(0), Value::Int(1), Value::Int(255), Value::Int(128), Value::Int(42)])
    );
    let back: Bytes = from_value(&value).expect("from_value");
    assert_eq!(back.0, original.0);
}

// -------------------------- cross-boundary test ---------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct UserProfile {
    user_id: u64,
    display_name: String,
    active: bool,
}

/// The end-to-end embedding scenario this whole bridge exists for: a Rust
/// host `to_value`s a struct, hands it to a mova script as a global, the
/// script reads a field via keyword access and `assoc`s a new key onto it,
/// and the host `from_value`s the result back into an updated struct --
/// with the script's extra debug key silently ignored (same "unknown
/// fields" contract `unknown_map_keys_are_ignored_by_default` pins down
/// above, just exercised through a real `Interp` this time instead of a
/// hand-built `Value`).
#[test]
fn cross_boundary_script_reads_and_updates_a_to_valued_struct() {
    let profile = UserProfile {
        user_id: 42,
        display_name: "Ada".to_string(),
        active: true,
    };
    let value = to_value(&profile).expect("to_value");

    let mut interp = Interp::new();
    // Field keys are keywordized VERBATIM (`user_id` -> `:user_id`, see the
    // module doc) -- inject the struct as a global the script can see.
    interp.globals.set(Symbol::simple("m"), value);

    // The script sees the exact same key the struct's field was named.
    let seen_id = interp.eval_str("test", "(:user_id m)").unwrap_or_else(|e| {
        panic!("eval error: {}", crate::error::render(&e, "test", "(:user_id m)"));
    });
    assert_eq!(seen_id, Value::Int(42));

    // The script updates one field and adds a new, unknown-to-Rust key.
    let src = r#"(def m (assoc m :display_name "Ada Lovelace" :last-seen-epoch 1234567890))"#;
    let updated_value = interp
        .eval_str("test", src)
        .unwrap_or_else(|e| panic!("eval error: {}", crate::error::render(&e, "test", src)));

    let updated: UserProfile = from_value(&updated_value).expect("from_value on the script's updated map");
    assert_eq!(
        updated,
        UserProfile {
            user_id: 42,
            display_name: "Ada Lovelace".to_string(),
            active: true,
        },
        "the script's extra :last-seen-epoch key must be silently ignored"
    );
}

#[test]
fn cross_boundary_vec_of_structs_round_trips_through_a_script() {
    let items = vec![
        LineItem {
            sku: "A".to_string(),
            qty: 1,
            unit_price: 1.5,
            gift_wrapped: false,
        },
        LineItem {
            sku: "B".to_string(),
            qty: 2,
            unit_price: 2.5,
            gift_wrapped: true,
        },
    ];
    let value = to_value(&items).expect("to_value");
    let mut interp = Interp::new();
    interp.globals.set(Symbol::simple("items"), value);
    // A script-side transform: bump every qty by 1.
    let src = "(def items (mapv (fn [it] (update it :qty inc)) items))";
    let out = interp
        .eval_str("test", src)
        .unwrap_or_else(|e| panic!("eval error: {}", crate::error::render(&e, "test", src)));
    let back: Vec<LineItem> = from_value(&out).expect("from_value");
    assert_eq!(back[0].qty, 2);
    assert_eq!(back[1].qty, 3);
    assert_eq!(back[0].sku, "A");
    assert_eq!(back[1].sku, "B");
}
