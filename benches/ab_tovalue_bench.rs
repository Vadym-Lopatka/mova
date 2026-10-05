//! Interleaved A/B harness for the Phase E `to_value` perf work
//! (EMBED-API-PLAN.md §4 Phase E). Per the mandatory measurement
//! discipline (this machine has background cargo builds from another
//! session AND ~30% thermal drift over ~40min -- see
//! bench/optimization-log.md "Embedding-fuel probe"), only SAME-ROUND
//! interleaved comparisons are trustworthy: this binary never reports a
//! number in isolation, always baseline vs candidate vs serde_json vs
//! from_value-canary measured back-to-back within the same round, for
//! every round.
//!
//! `mod baseline` is a FROZEN, self-contained copy of `to_value`'s
//! original (pre-optimization) serializer, pinned to what shipped at the
//! Phase B tip (9639571) -- it must NEVER be edited as the real
//! `src/serde_bridge.rs` changes, so every round of this binary keeps
//! comparing against the exact same fixed point, in the same process,
//! back-to-back with the live (candidate) implementation.
//!
//! Run with: `cargo bench --features serde --bench ab_tovalue_bench`

use std::hint::black_box;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use mova::serde_bridge::{from_value, to_value};
use mova::internal::Value;

// ============================================================================
// mod baseline -- frozen copy of the ORIGINAL to_value serializer (pre-
// Phase-E). Do not touch when iterating on src/serde_bridge.rs.
// ============================================================================
mod baseline {
    use mova::serde_bridge::SerdeError;
    use mova::internal::{PMap, PVec, Str, Value};
    use serde::ser::{self, Serialize};
    use serde::Serializer;

    pub fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, SerdeError> {
        value.serialize(ValueSerializer)
    }

    struct ValueSerializer;

    impl Serializer for ValueSerializer {
        type Ok = Value;
        type Error = SerdeError;
        type SerializeSeq = SeqSerializer;
        type SerializeTuple = SeqSerializer;
        type SerializeTupleStruct = SeqSerializer;
        type SerializeTupleVariant = TupleVariantSerializer;
        type SerializeMap = MapSerializer;
        type SerializeStruct = StructSerializer;
        type SerializeStructVariant = StructVariantSerializer;

        fn serialize_bool(self, v: bool) -> Result<Value, SerdeError> {
            Ok(Value::Bool(v))
        }
        fn serialize_i8(self, v: i8) -> Result<Value, SerdeError> {
            Ok(Value::Int(v as i64))
        }
        fn serialize_i16(self, v: i16) -> Result<Value, SerdeError> {
            Ok(Value::Int(v as i64))
        }
        fn serialize_i32(self, v: i32) -> Result<Value, SerdeError> {
            Ok(Value::Int(v as i64))
        }
        fn serialize_i64(self, v: i64) -> Result<Value, SerdeError> {
            Ok(Value::Int(v))
        }
        fn serialize_i128(self, v: i128) -> Result<Value, SerdeError> {
            i64::try_from(v)
                .map(Value::Int)
                .map_err(|_| SerdeError::IntOutOfRange(format!("i128 value {v} doesn't fit in mova's i64 Int")))
        }
        fn serialize_u8(self, v: u8) -> Result<Value, SerdeError> {
            Ok(Value::Int(v as i64))
        }
        fn serialize_u16(self, v: u16) -> Result<Value, SerdeError> {
            Ok(Value::Int(v as i64))
        }
        fn serialize_u32(self, v: u32) -> Result<Value, SerdeError> {
            Ok(Value::Int(v as i64))
        }
        fn serialize_u64(self, v: u64) -> Result<Value, SerdeError> {
            if v <= i64::MAX as u64 {
                Ok(Value::Int(v as i64))
            } else {
                Err(SerdeError::U64Overflow(v))
            }
        }
        fn serialize_u128(self, v: u128) -> Result<Value, SerdeError> {
            i64::try_from(v)
                .map(Value::Int)
                .map_err(|_| SerdeError::IntOutOfRange(format!("u128 value {v} doesn't fit in mova's i64 Int")))
        }
        fn serialize_f32(self, v: f32) -> Result<Value, SerdeError> {
            Ok(Value::Float(v as f64))
        }
        fn serialize_f64(self, v: f64) -> Result<Value, SerdeError> {
            Ok(Value::Float(v))
        }
        fn serialize_char(self, v: char) -> Result<Value, SerdeError> {
            Ok(Value::Char(v))
        }
        fn serialize_str(self, v: &str) -> Result<Value, SerdeError> {
            Ok(Value::Str(Str::from(v)))
        }
        fn serialize_bytes(self, v: &[u8]) -> Result<Value, SerdeError> {
            Ok(Value::Vector(PVec::from_iter(v.iter().map(|b| Value::Int(*b as i64)))))
        }
        fn serialize_none(self) -> Result<Value, SerdeError> {
            Ok(Value::Nil)
        }
        fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Value, SerdeError> {
            value.serialize(self)
        }
        fn serialize_unit(self) -> Result<Value, SerdeError> {
            Ok(Value::Nil)
        }
        fn serialize_unit_struct(self, _name: &'static str) -> Result<Value, SerdeError> {
            Ok(Value::Nil)
        }
        fn serialize_unit_variant(self, _name: &'static str, _index: u32, variant: &'static str) -> Result<Value, SerdeError> {
            Ok(Value::Keyword(Str::from(variant)))
        }
        fn serialize_newtype_struct<T: ?Sized + Serialize>(self, _name: &'static str, value: &T) -> Result<Value, SerdeError> {
            value.serialize(self)
        }
        fn serialize_newtype_variant<T: ?Sized + Serialize>(
            self,
            _name: &'static str,
            _index: u32,
            variant: &'static str,
            value: &T,
        ) -> Result<Value, SerdeError> {
            let inner = value.serialize(ValueSerializer)?;
            Ok(Value::Map(PMap::from_iter([(Value::Keyword(Str::from(variant)), inner)])))
        }
        fn serialize_seq(self, len: Option<usize>) -> Result<SeqSerializer, SerdeError> {
            Ok(SeqSerializer {
                items: Vec::with_capacity(len.unwrap_or(0)),
            })
        }
        fn serialize_tuple(self, len: usize) -> Result<SeqSerializer, SerdeError> {
            self.serialize_seq(Some(len))
        }
        fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<SeqSerializer, SerdeError> {
            self.serialize_seq(Some(len))
        }
        fn serialize_tuple_variant(
            self,
            _name: &'static str,
            _index: u32,
            variant: &'static str,
            len: usize,
        ) -> Result<TupleVariantSerializer, SerdeError> {
            Ok(TupleVariantSerializer {
                variant,
                items: Vec::with_capacity(len),
            })
        }
        fn serialize_map(self, _len: Option<usize>) -> Result<MapSerializer, SerdeError> {
            Ok(MapSerializer {
                entries: Vec::new(),
                pending_key: None,
            })
        }
        fn serialize_struct(self, _name: &'static str, len: usize) -> Result<StructSerializer, SerdeError> {
            Ok(StructSerializer {
                entries: Vec::with_capacity(len),
            })
        }
        fn serialize_struct_variant(
            self,
            _name: &'static str,
            _index: u32,
            variant: &'static str,
            len: usize,
        ) -> Result<StructVariantSerializer, SerdeError> {
            Ok(StructVariantSerializer {
                variant,
                entries: Vec::with_capacity(len),
            })
        }
    }

    struct SeqSerializer {
        items: Vec<Value>,
    }
    impl ser::SerializeSeq for SeqSerializer {
        type Ok = Value;
        type Error = SerdeError;
        fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerdeError> {
            self.items.push(value.serialize(ValueSerializer)?);
            Ok(())
        }
        fn end(self) -> Result<Value, SerdeError> {
            Ok(Value::Vector(PVec::from_iter(self.items)))
        }
    }
    impl ser::SerializeTuple for SeqSerializer {
        type Ok = Value;
        type Error = SerdeError;
        fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerdeError> {
            self.items.push(value.serialize(ValueSerializer)?);
            Ok(())
        }
        fn end(self) -> Result<Value, SerdeError> {
            Ok(Value::Vector(PVec::from_iter(self.items)))
        }
    }
    impl ser::SerializeTupleStruct for SeqSerializer {
        type Ok = Value;
        type Error = SerdeError;
        fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerdeError> {
            self.items.push(value.serialize(ValueSerializer)?);
            Ok(())
        }
        fn end(self) -> Result<Value, SerdeError> {
            Ok(Value::Vector(PVec::from_iter(self.items)))
        }
    }

    struct TupleVariantSerializer {
        variant: &'static str,
        items: Vec<Value>,
    }
    impl ser::SerializeTupleVariant for TupleVariantSerializer {
        type Ok = Value;
        type Error = SerdeError;
        fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerdeError> {
            self.items.push(value.serialize(ValueSerializer)?);
            Ok(())
        }
        fn end(self) -> Result<Value, SerdeError> {
            let seq = Value::Vector(PVec::from_iter(self.items));
            Ok(Value::Map(PMap::from_iter([(Value::Keyword(Str::from(self.variant)), seq)])))
        }
    }

    struct MapSerializer {
        entries: Vec<(Value, Value)>,
        pending_key: Option<Value>,
    }
    impl ser::SerializeMap for MapSerializer {
        type Ok = Value;
        type Error = SerdeError;
        fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), SerdeError> {
            self.pending_key = Some(key.serialize(ValueSerializer)?);
            Ok(())
        }
        fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), SerdeError> {
            let k = self
                .pending_key
                .take()
                .ok_or_else(|| SerdeError::Message("serialize_value called before serialize_key".into()))?;
            self.entries.push((k, value.serialize(ValueSerializer)?));
            Ok(())
        }
        fn end(self) -> Result<Value, SerdeError> {
            Ok(Value::Map(PMap::from_iter(self.entries)))
        }
    }

    struct StructSerializer {
        entries: Vec<(Value, Value)>,
    }
    impl ser::SerializeStruct for StructSerializer {
        type Ok = Value;
        type Error = SerdeError;
        fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), SerdeError> {
            self.entries.push((Value::Keyword(Str::from(key)), value.serialize(ValueSerializer)?));
            Ok(())
        }
        fn end(self) -> Result<Value, SerdeError> {
            Ok(Value::Map(PMap::from_iter(self.entries)))
        }
    }

    struct StructVariantSerializer {
        variant: &'static str,
        entries: Vec<(Value, Value)>,
    }
    impl ser::SerializeStructVariant for StructVariantSerializer {
        type Ok = Value;
        type Error = SerdeError;
        fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), SerdeError> {
            self.entries.push((Value::Keyword(Str::from(key)), value.serialize(ValueSerializer)?));
            Ok(())
        }
        fn end(self) -> Result<Value, SerdeError> {
            let inner = Value::Map(PMap::from_iter(self.entries));
            Ok(Value::Map(PMap::from_iter([(Value::Keyword(Str::from(self.variant)), inner)])))
        }
    }
}

// ============================================================================
// fixtures (mirrors serde_bridge_bench.rs)
// ============================================================================

#[derive(Clone, Serialize, Deserialize)]
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

/// Diagnostic fixture: all-`Int` fields (no `String`, no allocation on the
/// value side) so an 8-vs-9-field timing delta isolates the `PMap` Small/
/// Big tier crossover cost (`PMAP_SMALL_MAX == 8`) from field-value
/// serialization cost.
#[derive(Clone, Serialize, Deserialize)]
struct EightInts {
    a: i64,
    b: i64,
    c: i64,
    d: i64,
    e: i64,
    f: i64,
    g: i64,
    h: i64,
}
#[derive(Clone, Serialize, Deserialize)]
struct NineInts {
    a: i64,
    b: i64,
    c: i64,
    d: i64,
    e: i64,
    f: i64,
    g: i64,
    h: i64,
    i: i64,
}
fn eight_ints() -> EightInts {
    EightInts { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8 }
}
fn nine_ints() -> NineInts {
    NineInts { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9 }
}

// ============================================================================
// mod hasher_probe -- isolates ONE specific question: does the keyword
// cache's `HashMap<usize, Value>` (default SipHash) cost meaningfully more
// than a hand-rolled FxHash-style hasher would, given the cache's actual
// access pattern (RefCell borrow + entry/or_insert_with + Value clone,
// hashing a POINTER-sized usize key)? Self-contained, not wired into
// src/serde_bridge.rs -- a decision probe, not a candidate to ship as-is
// unless it's a clear win.
// ============================================================================
mod hasher_probe {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::hash::{BuildHasherDefault, Hasher};

    use mova::internal::{Str, Value};

    #[derive(Default)]
    pub struct FxHasher(u64);
    impl Hasher for FxHasher {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
            for &b in bytes {
                self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(SEED);
            }
        }
        fn write_usize(&mut self, i: usize) {
            const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
            self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(SEED);
        }
    }
    pub type FxBuildHasher = BuildHasherDefault<FxHasher>;

    thread_local! {
        static STD_CACHE: RefCell<HashMap<usize, Value>> = RefCell::new(HashMap::new());
        static FX_CACHE: RefCell<HashMap<usize, Value, FxBuildHasher>> = RefCell::new(HashMap::default());
    }

    pub fn std_cached_keyword(name: &'static str) -> Value {
        let key = name.as_ptr() as usize;
        STD_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.entry(key).or_insert_with(|| Value::Keyword(Str::from(name))).clone()
        })
    }
    pub fn fx_cached_keyword(name: &'static str) -> Value {
        let key = name.as_ptr() as usize;
        FX_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            c.entry(key).or_insert_with(|| Value::Keyword(Str::from(name))).clone()
        })
    }
}

// ============================================================================
// harness -- interleaved: every round measures baseline, candidate,
// serde_json, and the from_value canary back-to-back, THEN moves to the
// next round. Medians are taken across rounds per label, never across a
// mix of different processes/sessions.
// ============================================================================

const ROUNDS: usize = 7;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}
fn minmax(v: &[f64]) -> (f64, f64) {
    (v.iter().cloned().fold(f64::INFINITY, f64::min), v.iter().cloned().fold(f64::NEG_INFINITY, f64::max))
}

fn time_ns_per_iter<F: FnMut()>(mut f: F, iters: u64) -> f64 {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

fn report(label: &str, samples: &[f64]) {
    let med = median(samples.to_vec());
    let (lo, hi) = minmax(samples);
    println!("{label:<44} {med:>9.1} ns  [{lo:>8.1} - {hi:>8.1}]");
}

fn main() {
    println!("Interleaved A/B: baseline (frozen) vs candidate (live) to_value -- median of {ROUNDS} rounds\n");

    let flat = flat_fixture();
    let flat_value = to_value(&flat).unwrap();
    let vec1000: Vec<FlatStruct> = (0..1000).map(|_| flat_fixture()).collect();
    let vec1000_value: Value = to_value(&vec1000).unwrap();

    const FLAT_ITERS: u64 = 100_000;
    const VEC_ITERS: u64 = 300;
    const N: f64 = 1000.0;

    let mut baseline_flat = Vec::with_capacity(ROUNDS);
    let mut candidate_flat = Vec::with_capacity(ROUNDS);
    let mut json_flat = Vec::with_capacity(ROUNDS);
    let mut from_value_flat = Vec::with_capacity(ROUNDS);
    let mut baseline_vec = Vec::with_capacity(ROUNDS);
    let mut candidate_vec = Vec::with_capacity(ROUNDS);
    let mut json_vec = Vec::with_capacity(ROUNDS);
    let mut from_value_vec = Vec::with_capacity(ROUNDS);

    // one warmup round, discarded, so lazy one-time init (e.g. a
    // thread-local cache's first fill) doesn't pollute round 1.
    for _ in 0..(ROUNDS + 1) {
        let b = time_ns_per_iter(|| { black_box(baseline::to_value(black_box(&flat)).unwrap()); }, FLAT_ITERS);
        let c = time_ns_per_iter(|| { black_box(to_value(black_box(&flat)).unwrap()); }, FLAT_ITERS);
        let j = time_ns_per_iter(|| { black_box(serde_json::to_string(black_box(&flat)).unwrap()); }, FLAT_ITERS);
        let f = time_ns_per_iter(|| { let _: FlatStruct = from_value(black_box(&flat_value)).unwrap(); }, FLAT_ITERS);

        let bv = time_ns_per_iter(|| { black_box(baseline::to_value(black_box(&vec1000)).unwrap()); }, VEC_ITERS) / N;
        let cv = time_ns_per_iter(|| { black_box(to_value(black_box(&vec1000)).unwrap()); }, VEC_ITERS) / N;
        let jv = time_ns_per_iter(|| { black_box(serde_json::to_string(black_box(&vec1000)).unwrap()); }, VEC_ITERS) / N;
        let fv =
            time_ns_per_iter(|| { let _: Vec<FlatStruct> = from_value(black_box(&vec1000_value)).unwrap(); }, VEC_ITERS) / N;

        baseline_flat.push(b);
        candidate_flat.push(c);
        json_flat.push(j);
        from_value_flat.push(f);
        baseline_vec.push(bv);
        candidate_vec.push(cv);
        json_vec.push(jv);
        from_value_vec.push(fv);
    }
    // drop the warmup round (index 0) from every series
    for v in [
        &mut baseline_flat,
        &mut candidate_flat,
        &mut json_flat,
        &mut from_value_flat,
        &mut baseline_vec,
        &mut candidate_vec,
        &mut json_vec,
        &mut from_value_vec,
    ] {
        v.remove(0);
    }

    println!("-- flat 10-field struct (per-call) --");
    report("baseline::to_value(flat)", &baseline_flat);
    report("candidate::to_value(flat)", &candidate_flat);
    report("serde_json::to_string(flat)", &json_flat);
    report("from_value(flat)  [canary]", &from_value_flat);
    println!(
        "  candidate/serde_json ratio: {:.2}x   baseline/serde_json ratio: {:.2}x",
        median(candidate_flat.clone()) / median(json_flat.clone()),
        median(baseline_flat.clone()) / median(json_flat.clone())
    );
    println!();

    println!("-- Vec<flat, 1000> (amortized ns/item) --");
    report("baseline::to_value(Vec/1000)/item", &baseline_vec);
    report("candidate::to_value(Vec/1000)/item", &candidate_vec);
    report("serde_json::to_string(Vec/1000)/item", &json_vec);
    report("from_value(Vec/1000)/item  [canary]", &from_value_vec);
    println!(
        "  candidate/serde_json ratio: {:.2}x   baseline/serde_json ratio: {:.2}x",
        median(candidate_vec.clone()) / median(json_vec.clone()),
        median(baseline_vec.clone()) / median(json_vec.clone())
    );
    println!();

    // -- hasher probe: SipHash (std default) vs a hand-rolled FxHash-style
    // hasher for the keyword cache's HashMap<usize, Value>, isolated from
    // everything else to_value does (map building, value serialization).
    // 10 lookups/round == one flat-struct's worth of field names.
    const FIELD_NAMES: [&str; 10] = ["id", "name", "score", "active", "count", "ratio", "tag", "weight", "note", "verified"];
    const HASH_ITERS: u64 = 200_000;
    let mut std_hash = Vec::with_capacity(ROUNDS);
    let mut fx_hash = Vec::with_capacity(ROUNDS);
    for _ in 0..(ROUNDS + 1) {
        let s = time_ns_per_iter(
            || {
                for n in FIELD_NAMES {
                    black_box(hasher_probe::std_cached_keyword(black_box(n)));
                }
            },
            HASH_ITERS,
        );
        let x = time_ns_per_iter(
            || {
                for n in FIELD_NAMES {
                    black_box(hasher_probe::fx_cached_keyword(black_box(n)));
                }
            },
            HASH_ITERS,
        );
        std_hash.push(s);
        fx_hash.push(x);
    }
    std_hash.remove(0);
    fx_hash.remove(0);

    println!("-- keyword-cache hasher probe (10 lookups/call, cache warm) --");
    report("std HashMap (SipHash)", &std_hash);
    report("FxHash-style hasher", &fx_hash);
    println!("  FxHash/std ratio: {:.2}x", median(fx_hash.clone()) / median(std_hash.clone()));
    println!();

    // -- PMap Small/Big tier-crossover diagnostic: all-Int fields (no
    // value-side allocation), 8 fields (stays Small) vs 9 (crosses into
    // Big, PMAP_SMALL_MAX == 8) -- isolates the tier-crossover's OWN cost
    // (hashing Values into a CHAMP HAMT, node traffic) from field-value
    // serialization cost, to characterize to_value's residual gap.
    let eight = eight_ints();
    let nine = nine_ints();
    const TIER_ITERS: u64 = 200_000;
    let mut eight_ns = Vec::with_capacity(ROUNDS);
    let mut nine_ns = Vec::with_capacity(ROUNDS);
    for _ in 0..(ROUNDS + 1) {
        let e = time_ns_per_iter(|| { black_box(to_value(black_box(&eight)).unwrap()); }, TIER_ITERS);
        let n = time_ns_per_iter(|| { black_box(to_value(black_box(&nine)).unwrap()); }, TIER_ITERS);
        eight_ns.push(e);
        nine_ns.push(n);
    }
    eight_ns.remove(0);
    nine_ns.remove(0);

    println!("-- PMap Small(<=8)/Big(>8) tier-crossover diagnostic (all-Int fields) --");
    report("to_value(8 Int fields)  [Small tier]", &eight_ns);
    report("to_value(9 Int fields)  [Big tier]", &nine_ns);
    println!(
        "  Big-tier crossover adds ~{:.1}ns for ONE extra field (Small's own per-field cost is included in both)",
        median(nine_ns.clone()) - median(eight_ns.clone())
    );
}
