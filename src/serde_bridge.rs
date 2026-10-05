//! `serde` bridge for mova's embedding API: `to_value`/`from_value` implement
//! `serde::Serializer`/`serde::Deserializer` directly over [`crate::value::Value`],
//! so a Rust host can hand a `#[derive(Serialize)]` struct straight to a
//! mova script (and read a script's output straight back into a
//! `#[derive(Deserialize)]` struct) with no JSON/text hop in between.
//! Entirely opt-in: this whole module only exists when the crate is built
//! with `--features serde` (see `Cargo.toml`); nothing here is reachable,
//! or even compiled, otherwise.
//!
//! # Encoding contract
//!
//! | Rust shape | `Value` shape | notes |
//! |---|---|---|
//! | `bool` | `Bool` | |
//! | `i8`/`i16`/`i32`/`i64`/`u8`/`u16`/`u32` | `Int` | widened losslessly |
//! | `u64`/`u128`/`i128` | `Int` | **errors** if it doesn't fit `i64` (mova has no bignum/u64>`i64::MAX`) |
//! | `f32`/`f64` | `Float` | NaN/inf round-trip bit-for-bit (`f64::to_bits`); see "NaN" below |
//! | `char` | `Char` | |
//! | `String`/`&str` | `Str` | |
//! | `Vec<u8>`/`&[u8]` | `Vector` of `Int` (0..=255) | mova has no dedicated byte-string type |
//! | `Option<T>` | `Nil` (`None`) / `T`'s encoding (`Some`) | a missing struct-field key also deserializes as `None`, standard serde behavior for `Option<T>` fields |
//! | `Vec<T>`/`[T; N]`/tuple | `Vector` | |
//! | `HashMap<K, V>`/`BTreeMap<K, V>` | `Map` | key is serialized as a `Value` directly (see "map keys" below) |
//! | struct (named fields) | `Map`, `Keyword` keys | field `user_id` -> key `:user_id` -- **verbatim**, no case conversion (see "field naming" below) |
//! | unit struct | `Nil` | it carries no data, so this is a value-preserving choice, not a semantic loss |
//! | newtype struct | inner value's own encoding | transparent, standard serde convention |
//! | tuple struct | `Vector` | field names aren't available to preserve |
//! | unit enum variant | `Keyword` of the variant name | `Variant` -> `:Variant`, verbatim |
//! | newtype enum variant | `Map` `{:Variant payload}` | one entry, payload is the inner value's own encoding |
//! | tuple enum variant | `Map` `{:Variant [v0 v1 ...]}` | one entry, payload is a `Vector` |
//! | struct enum variant | `Map` `{:Variant {:field0 v0 ...}}` | one entry, payload is a field `Map` like a plain struct |
//!
//! ## Field/variant naming: verbatim, not kebab-cased
//!
//! v1 keywordizes Rust field and variant names **exactly as spelled** --
//! `user_id` becomes `:user_id`, NOT `:user-id`. This is a deliberate
//! decision, not an oversight: automatic `snake_case` -> `kebab-case`
//! conversion is a lossy, guessable-but-not-invertible transform (what
//! about a field that's already got an underscore for a reason, or a
//! variant name with mixed case?), and every rename has to invert
//! perfectly for `from_value` to recover the original struct. A future
//! version could add an opt-in `#[serde(rename = "...")]`-driven or
//! blanket kebab-casing pass, but v1 ships the simple, unambiguous
//! contract: what you name the field in Rust is what the script sees in
//! `(:user_id m)`.
//!
//! ## Map keys: any `Value`, not just strings
//!
//! `serde`'s data model allows a map's key type to be anything
//! `Serialize`/`Deserialize` (see `Serializer::serialize_key`), and mova's
//! `Value::Map` already supports arbitrary `Value` keys -- so this bridge
//! takes the direct, no-compromise route: a `HashMap<K, V>`'s key is run
//! through the SAME serializer as any other value and used as the `Map`
//! entry's key `Value` verbatim (an integer key becomes an `Int` key, a
//! struct key becomes a nested `Map` key, etc). No string-only restriction,
//! no error path needed for "impractical" key types.
//!
//! Going the other direction, a struct's fields (as opposed to a generic
//! map's keys) are matched leniently: `from_value` accepts EITHER a
//! `Keyword` OR a `Str` map key for a given field name, since a script
//! calling `(assoc m "user_id" ...)` or `(assoc m :user_id ...)` are both
//! reasonable ways to build a map bound for a Rust struct. `to_value`
//! always emits `Keyword` keys for struct fields (see the table above);
//! the `Str`-key acceptance is a `from_value`-only convenience for values
//! that originated in a script rather than round-tripped through
//! `to_value`.
//!
//! Unknown map keys are silently ignored when deserializing into a struct
//! (`serde`'s default `#[derive(Deserialize)]` behavior, not something
//! this bridge has to implement) -- a script that `assoc`s extra debug
//! keys onto a map before handing it back doesn't break `from_value`.
//!
//! ## u64/u128/i128 overflow
//!
//! mova's `Value::Int` is a plain `i64` -- there is no bignum and no way
//! to represent an unsigned value bigger than `i64::MAX` (see
//! `README.md`'s "Deferred" section: "ratios/bignums" is explicitly out of
//! scope). `to_value` therefore returns `Err(SerdeError::U64Overflow(v))`
//! for a `u64`/`u128` value `> i64::MAX`, and `Err(SerdeError::IntOutOfRange(_))`
//! for an `i128` outside `i64`'s range, rather than silently truncating or
//! panicking.
//!
//! ## NaN / infinity
//!
//! `f64::NAN`/`INFINITY`/`NEG_INFINITY` all round-trip through `Value::Float`
//! exactly as IEEE-754 bit patterns -- `to_value`/`from_value` never inspect
//! or reject them (unlike, say, strict JSON, which has no NaN/Inf literal
//! at all). `Value`'s own `PartialEq` compares floats by `f64::to_bits`
//! (see `value.rs`), so `to_value(&f64::NAN) == to_value(&f64::NAN)` at the
//! `Value` level is actually `true` (two NaNs with the same bit pattern
//! compare equal there, unlike IEEE-754 `==`). The catch is one level up:
//! a plain Rust `f64` field's DERIVED `PartialEq` (what
//! `assert_eq!(original, round_tripped)` on a `#[derive(PartialEq)]` struct
//! actually uses) follows real IEEE-754 semantics, where `NaN != NaN` --
//! so a struct-level round-trip equality assertion on a NaN-containing
//! field will report unequal even though `to_value`/`from_value` preserved
//! the bits exactly. This module's own `nan_bits_round_trip_exactly` test
//! asserts the BITS match instead, which is the correct check here.
//!
//! ## Depth
//!
//! Both directions recurse once per nesting level with no manual
//! trampolining, same as `serde_json` and most other serde backends -- a
//! deeply nested structure is bounded by the real Rust call stack, not a
//! mova-level guard. Verified to depth 100 by this module's own test
//! (`deeply_nested_round_trip`); pathological depths (tens of thousands)
//! would need a larger thread stack, exactly like `serde_json` on the same
//! shape.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};

use serde::de::{self, DeserializeOwned, DeserializeSeed, Visitor};
use serde::ser::{self, Serialize};
use serde::{Deserializer, Serializer};

use crate::value::{Keyword, PMap, PMapIter, PVec, PVecIter, Str, Value};

// ============================ field-name cache ============================

/// A minimal FxHash-style hasher (the well-known `rotate-xor-multiply`
/// construction popularized by rustc/Firefox's `rustc-hash` crate; no
/// dependency pulled in, just the ~10-line algorithm) for
/// [`KEYWORD_CACHE`]'s `HashMap`. The default `HashMap` hasher (SipHash)
/// is deliberately DoS-resistant, which costs real per-call overhead
/// that's wasted here: the key is always a pointer-sized `usize` (never
/// attacker-controlled -- it's the address of a `&'static str` literal
/// baked into the binary), and `benches/ab_tovalue_bench.rs`'s isolated
/// "hasher_probe" measured SipHash costing ~1.6x an FxHash-style hasher
/// on this exact access pattern (10 lookups/round, cache warm).
#[derive(Default)]
struct FxHasher(u64);

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
        // The only method a `usize` key's `Hash` impl actually calls;
        // `write` above stays correct (if unused on this key type) so
        // `FxHasher` is a complete, honest `Hasher`, not one that only
        // works by accident for this one caller.
        const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(SEED);
    }
}

type FxBuildHasher = BuildHasherDefault<FxHasher>;

thread_local! {
    /// Per-thread cache from a struct field/enum-variant name's `&'static
    /// str` ADDRESS (not its text -- see below) to the `Value::Keyword`
    /// `to_value` builds for it.
    ///
    /// `#[derive(Serialize)]`-generated code hands `serialize_field`/
    /// `serialize_unit_variant`/etc. the SAME `&'static str` (a string
    /// literal baked into the binary) every single time it's called, for
    /// every instance of that struct/enum ever serialized. Without this
    /// cache, `to_value` on a `Vec<Struct, 1000>` re-ran `Str::from` (a
    /// `Box<str>` copy + an `Arc<StrInner>` allocation -- see `value.rs`'s
    /// `Str::build`) on the literal `"field_name"` a thousand times over,
    /// for every field, even though it produces byte-identical output
    /// every time. The allocation profile
    /// (`benches/serde_alloc_profile.rs`) measured this at ~2 allocator
    /// calls per struct field, ~59% of a flat 10-field struct's total
    /// allocation traffic.
    ///
    /// Keyed by POINTER (`&'static str as *const str as *const u8 as
    /// usize`), not by the string's contents: a `HashMap<&'static str,
    /// Value>` would need to hash/compare the field name's bytes on every
    /// lookup, which is real (if smaller) work; a `&'static str` from a
    /// string literal has one fixed address for the life of the program
    /// (each literal is deduplicated into `.rodata` by rustc/LLVM, though
    /// two DIFFERENT literals with the same text are not guaranteed to
    /// share an address -- that's fine here, it would only mean a
    /// same-text field name from two different derive sites gets its own
    /// cache entry, never an incorrect one), so pointer identity is a
    /// valid, cheaper cache key: no hashing of the text, no chance of two
    /// *different* addresses colliding into the wrong Keyword.
    ///
    /// Content-safe by construction: `Value`'s `PartialEq`/`Hash` are
    /// representation-blind (see `value.rs`), so returning a cached
    /// `Value::Keyword` (an `Arc` clone -- a cheap atomic bump, no
    /// allocation) rather than a freshly-allocated one is observationally
    /// identical to the pre-cache behavior in every way a caller of
    /// `to_value` can detect: equality, hashing, printing, iteration.
    ///
    /// Thread-local (not a single process-wide cache behind a `Mutex`/
    /// `RwLock`): `to_value` must stay usable from multiple threads
    /// without lock contention on this exact hot path being the reason a
    /// multi-threaded embedder's throughput doesn't scale -- each thread
    /// just pays its own one-time cache-fill cost per distinct field name
    /// it happens to serialize, which is bounded by the number of
    /// distinct `#[derive(Serialize)]` field/variant name literals in the
    /// whole program (small, typically dozens to low hundreds), so this
    /// never grows unbounded.
    static KEYWORD_CACHE: RefCell<HashMap<usize, Value, FxBuildHasher>> = RefCell::new(HashMap::default());
}

/// Returns the `Value::Keyword` for a struct field or enum variant name,
/// reusing a cached one keyed by `name`'s address when this exact
/// `&'static str` has been seen before on this thread. See
/// `KEYWORD_CACHE`'s doc for the full rationale.
#[inline]
fn cached_keyword(name: &'static str) -> Value {
    let key = name.as_ptr() as usize;
    KEYWORD_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.entry(key).or_insert_with(|| Value::Keyword(Keyword::from(name))).clone()
    })
}

// ============================== errors ==================================

/// Everything that can go wrong converting between a Rust type and
/// [`Value`], in either direction.
#[derive(Debug, Clone, PartialEq)]
pub enum SerdeError {
    /// A `u64`/`u128` value is bigger than `i64::MAX` -- mova's `Value::Int`
    /// is a plain `i64`, with no bignum fallback (see the module doc's
    /// "u64/u128/i128 overflow" section).
    U64Overflow(u64),
    /// An `i128`/`u128` value doesn't fit in `i64`'s range.
    IntOutOfRange(String),
    /// A map/struct-field key or enum-variant tag was neither a
    /// `Value::Keyword` nor a `Value::Str` where one of those was required.
    KeyMustBeStringOrKeyword,
    /// Catch-all: every message produced via `serde::ser::Error::custom`/
    /// `serde::de::Error::custom` (which is what `#[derive(Serialize/
    /// Deserialize)]`-generated code -- and most of this module's own
    /// type-mismatch error paths -- actually goes through) lands here.
    Message(String),
}

impl fmt::Display for SerdeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SerdeError::U64Overflow(v) => write!(
                f,
                "u64 value {v} overflows mova's i64-only Value::Int representation (max {})",
                i64::MAX
            ),
            SerdeError::IntOutOfRange(msg) => write!(f, "{msg}"),
            SerdeError::KeyMustBeStringOrKeyword => {
                write!(f, "map/struct key must be a Value::Str or Value::Keyword")
            }
            SerdeError::Message(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for SerdeError {}

impl ser::Error for SerdeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        SerdeError::Message(msg.to_string())
    }
}

impl de::Error for SerdeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        SerdeError::Message(msg.to_string())
    }
}

/// Human-readable name of a `Value`'s variant, for error messages only.
fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Nil => "nil",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "string",
        Value::Sym(_) => "symbol",
        Value::Keyword(_) => "keyword",
        Value::Char(_) => "char",
        Value::List(_) => "list",
        Value::Vector(_) => "vector",
        // S7: an entry deserializes as the 2-element SEQUENCE it is (the
        // `Value::Vector(v) | Value::List(v) | Value::MapEntry(v)` arms
        // below), so it reports the same shape name a Serde error message
        // would want for a vector.
        Value::MapEntry(_) => "vector",
        Value::Map(_) => "map",
        Value::HostStruct(_) | Value::LazyMap(_) => "map",
        Value::Set(_) => "set",
        Value::Fn(_) => "fn",
        Value::Native(_) => "native-fn",
        Value::Macro(_) => "macro",
        Value::Atom(_) => "atom",
        // M4b: added alongside `BigInt`/`Ratio`/`BigDec` above, which this
        // match was ALREADY missing before this change (a pre-existing gap
        // in the `serde` feature, unrelated to M4b -- `cargo test --release`
        // with default features never compiles this module, so it went
        // undetected; out of this milestone's scope to fix).
        Value::Volatile(_) => "volatile",
        Value::Lazy(_) => "lazy-seq",
        Value::Future(_) => "future",
        Value::Promise(_) => "promise",
        Value::Delay(_) => "delay",
        Value::Channel(_) => "channel",
        Value::Flow(_) => "flow",
        Value::Regex(_) => "regex",
        Value::Var(_) => "var",
    }
}

/// A map/struct-field key or enum-variant tag as a `&str`: accepts either
/// a `Value::Keyword` (name without the leading colon, per `Value`'s own
/// contract) or a `Value::Str`, per the module doc's "map keys" section.
fn key_text(v: &Value) -> Result<&str, SerdeError> {
    match v {
        Value::Keyword(s) => Ok(s.as_ref()),
        Value::Str(s) => Ok(s.as_ref()),
        _ => Err(SerdeError::KeyMustBeStringOrKeyword),
    }
}

// ============================== to_value =================================

/// Serializes any `T: Serialize` into a mova [`Value`]. See the module
/// doc for the full encoding contract.
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
        Ok(cached_keyword(variant))
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
        Ok(Value::Map(PMap::from_iter([(cached_keyword(variant), inner)])))
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
        Ok(Value::Map(PMap::from_iter([(cached_keyword(self.variant), seq)])))
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
        // `self.entries` came from a real `HashMap`/`BTreeMap`/etc via
        // `serde`'s `SerializeMap` protocol -- one `serialize_key`/
        // `serialize_value` pair per source entry, so keys are already
        // guaranteed unique; `from_unique_pairs` (see its doc) is safe and
        // skips `PMap::from_iter`'s per-insert incremental rebuild.
        Ok(Value::Map(PMap::from_unique_pairs(self.entries)))
    }
}

struct StructSerializer {
    entries: Vec<(Value, Value)>,
}

impl ser::SerializeStruct for StructSerializer {
    type Ok = Value;
    type Error = SerdeError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), SerdeError> {
        self.entries.push((cached_keyword(key), value.serialize(ValueSerializer)?));
        Ok(())
    }
    fn end(self) -> Result<Value, SerdeError> {
        Ok(Value::Map(PMap::from_unique_pairs(self.entries)))
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
        self.entries.push((cached_keyword(key), value.serialize(ValueSerializer)?));
        Ok(())
    }
    fn end(self) -> Result<Value, SerdeError> {
        let inner = Value::Map(PMap::from_unique_pairs(self.entries));
        Ok(Value::Map(PMap::from_iter([(cached_keyword(self.variant), inner)])))
    }
}

// ============================= from_value ================================

/// Deserializes a mova [`Value`] into any `T: DeserializeOwned`. See the
/// module doc for the full encoding contract, in particular the "map
/// keys" section (struct fields accept either `Keyword` or `Str` keys) and
/// the note that unknown map keys are silently ignored.
pub fn from_value<T: DeserializeOwned>(value: &Value) -> Result<T, SerdeError> {
    T::deserialize(ValueDeserializer { value })
}

#[derive(Clone, Copy)]
struct ValueDeserializer<'de> {
    value: &'de Value,
}

impl<'de> ValueDeserializer<'de> {
    fn err(&self, expected: &str) -> SerdeError {
        SerdeError::Message(format!("expected {expected}, found a mova {}", value_kind(self.value)))
    }
}

macro_rules! deserialize_via_i64 {
    ($($method:ident),+ $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
                match self.value {
                    Value::Int(n) => visitor.visit_i64(*n),
                    _ => Err(self.err("an integer")),
                }
            }
        )+
    };
}

impl<'de> Deserializer<'de> for ValueDeserializer<'de> {
    type Error = SerdeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Nil => visitor.visit_unit(),
            Value::Bool(b) => visitor.visit_bool(*b),
            Value::Int(n) => visitor.visit_i64(*n),
            Value::Float(f) => visitor.visit_f64(*f),
            // Keyword has no natural primitive in serde's data model; the
            // most useful default for a fully-generic (`deserialize_any`)
            // consumer is its bare name (no colon), same text `name` gives
            // a script. Structured consumers (derived structs/enums) never
            // go through this arm -- they call `deserialize_struct`/
            // `deserialize_enum` directly, which DO distinguish Keyword.
            Value::Str(s) => visitor.visit_borrowed_str(s.as_ref()),
            // W-GEO stage 1: `Keyword` is no longer a `Str`, so this can no
            // longer share `Str`'s or-pattern arm -- but the text it hands
            // the visitor is byte-identical, and still a BORROW (never a
            // copy): `Keyword::text_ref` returns a reference into either
            // the process-wide intern table (valid for `'static`) or the
            // keyword's own `Arc<Str>` (valid for `'de`), so
            // `visit_borrowed_str` -- and therefore deserializing into a
            // `&'de str` field -- keeps working exactly as before.
            Value::Keyword(k) => visitor.visit_borrowed_str(k.as_ref()),
            Value::Char(c) => visitor.visit_char(*c),
            Value::Vector(v) | Value::List(v) | Value::MapEntry(v) => visitor.visit_seq(ValueSeqAccess { iter: v.iter() }),
            Value::Map(m) => visitor.visit_map(ValueMapAccess {
                iter: m.iter(),
                value: None,
                as_identifier: false,
            }),
            Value::HostStruct(hs) => visitor.visit_map(ValueMapAccess {
                iter: crate::host_struct::as_pmap(hs).iter(),
                value: None,
                as_identifier: false,
            }),
            Value::LazyMap(lm) => visitor.visit_map(ValueMapAccess {
                iter: crate::lazy_map::as_pmap(lm).iter(),
                value: None,
                as_identifier: false,
            }),
            other => Err(SerdeError::Message(format!(
                "a mova {} has no generic serde representation",
                value_kind(other)
            ))),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Bool(b) => visitor.visit_bool(*b),
            _ => Err(self.err("a bool")),
        }
    }

    deserialize_via_i64!(
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_i128,
        deserialize_u128,
    );

    // f32/f64: also accept an Int, widening it -- the visitor generated
    // for an f32/f64 Rust field always implements `visit_i64` as a
    // widening cast (same mechanism `serde_json` leans on for the same
    // "an integer literal in a float field" case), so handing it the raw
    // i64 via `visit_i64` here does the right thing without us needing to
    // know which numeric type the caller actually wants.
    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Float(f) => visitor.visit_f64(*f),
            Value::Int(n) => visitor.visit_i64(*n),
            _ => Err(self.err("a float")),
        }
    }
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Float(f) => visitor.visit_f64(*f),
            Value::Int(n) => visitor.visit_i64(*n),
            _ => Err(self.err("a float")),
        }
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Char(c) => visitor.visit_char(*c),
            Value::Str(s) => {
                let mut chars = s.as_ref().chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => visitor.visit_char(c),
                    _ => Err(self.err("a single-char string")),
                }
            }
            _ => Err(self.err("a char")),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Str(s) => visitor.visit_borrowed_str(s.as_ref()),
            _ => Err(self.err("a string")),
        }
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        self.deserialize_str(visitor)
    }

    // No dedicated byte-string type in `Value` -- mirrors `serialize_bytes`
    // above: a `Vector` of `Int`s in 0..=255.
    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Vector(v) | Value::List(v) | Value::MapEntry(v) => {
                let mut bytes = Vec::with_capacity(v.len());
                for item in v.iter() {
                    match item {
                        Value::Int(n) if (0..=255).contains(n) => bytes.push(*n as u8),
                        _ => return Err(self.err("a byte (Int in 0..=255) element")),
                    }
                }
                visitor.visit_byte_buf(bytes)
            }
            _ => Err(self.err("bytes (a Vector of Int 0..=255)")),
        }
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Nil => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Nil => visitor.visit_unit(),
            _ => Err(self.err("nil (unit)")),
        }
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, SerdeError> {
        self.deserialize_unit(visitor)
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, SerdeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Vector(v) | Value::List(v) | Value::MapEntry(v) => visitor.visit_seq(ValueSeqAccess { iter: v.iter() }),
            _ => Err(self.err("a sequence (Vector or List)")),
        }
    }
    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, SerdeError> {
        self.deserialize_seq(visitor)
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, SerdeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Map(m) => visitor.visit_map(ValueMapAccess {
                iter: m.iter(),
                value: None,
                as_identifier: false,
            }),
            // W3: a `HostStruct` deserializes exactly like the real `Map`
            // it's script-indistinguishable from -- delegates through
            // `host_struct::as_pmap`'s ONE materialize choke point.
            Value::HostStruct(hs) => visitor.visit_map(ValueMapAccess {
                iter: crate::host_struct::as_pmap(hs).iter(),
                value: None,
                as_identifier: false,
            }),
            Value::LazyMap(lm) => visitor.visit_map(ValueMapAccess {
                iter: crate::lazy_map::as_pmap(lm).iter(),
                value: None,
                as_identifier: false,
            }),
            _ => Err(self.err("a map")),
        }
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Map(m) => visitor.visit_map(ValueMapAccess {
                iter: m.iter(),
                value: None,
                as_identifier: true,
            }),
            Value::HostStruct(hs) => visitor.visit_map(ValueMapAccess {
                iter: crate::host_struct::as_pmap(hs).iter(),
                value: None,
                as_identifier: true,
            }),
            Value::LazyMap(lm) => visitor.visit_map(ValueMapAccess {
                iter: crate::lazy_map::as_pmap(lm).iter(),
                value: None,
                as_identifier: true,
            }),
            _ => Err(self.err("a map (for a struct)")),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, SerdeError> {
        match self.value {
            Value::Keyword(k) => visitor.visit_enum(ValueEnumAccess {
                tag: k.as_ref(),
                payload: None,
            }),
            Value::Map(m) => {
                let mut it = m.iter();
                let first = it.next();
                if first.is_none() || it.next().is_some() {
                    return Err(SerdeError::Message(
                        "enum map representation must have exactly one entry ({:variant-keyword payload})".into(),
                    ));
                }
                let (k, v) = first.expect("checked is_none above");
                let tag = key_text(k)?;
                visitor.visit_enum(ValueEnumAccess { tag, payload: Some(v) })
            }
            _ => Err(self.err("an enum (Keyword for a unit variant, or a single-entry Map for newtype/tuple/struct)")),
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        match self.value {
            // See `deserialize_any`'s keyword arm for why these are two
            // arms now and why the borrow is still sound.
            Value::Keyword(k) => visitor.visit_borrowed_str(k.as_ref()),
            Value::Str(s) => visitor.visit_borrowed_str(s.as_ref()),
            _ => Err(self.err("an identifier (Keyword or Str)")),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, SerdeError> {
        self.deserialize_any(visitor)
    }
}

struct ValueSeqAccess<'de> {
    iter: PVecIter<'de>,
}

impl<'de> de::SeqAccess<'de> for ValueSeqAccess<'de> {
    type Error = SerdeError;
    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, SerdeError> {
        match self.iter.next() {
            Some(v) => seed.deserialize(ValueDeserializer { value: v }).map(Some),
            None => Ok(None),
        }
    }
    fn size_hint(&self) -> Option<usize> {
        let (lo, hi) = self.iter.size_hint();
        if Some(lo) == hi {
            hi
        } else {
            None
        }
    }
}

struct ValueMapAccess<'de> {
    iter: PMapIter<'de>,
    value: Option<&'de Value>,
    /// `true` for `deserialize_struct`/a variant's struct payload: keys are
    /// fed through as bare identifier strings (accepting either `Keyword`
    /// or `Str`), which is what lets serde's derive-generated field-enum
    /// matching (and its "unknown field -> ignore" fallback) do its thing.
    /// `false` for a generic `deserialize_map`/`deserialize_any`: keys are
    /// deserialized as full `Value`s, per the module doc's "map keys"
    /// section.
    as_identifier: bool,
}

impl<'de> de::MapAccess<'de> for ValueMapAccess<'de> {
    type Error = SerdeError;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, SerdeError> {
        match self.iter.next() {
            Some((k, v)) => {
                self.value = Some(v);
                if self.as_identifier {
                    let text = key_text(k)?;
                    seed.deserialize(de::value::BorrowedStrDeserializer::<SerdeError>::new(text)).map(Some)
                } else {
                    seed.deserialize(ValueDeserializer { value: k }).map(Some)
                }
            }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, SerdeError> {
        let v = self
            .value
            .take()
            .ok_or_else(|| SerdeError::Message("next_value_seed called before next_key_seed".into()))?;
        seed.deserialize(ValueDeserializer { value: v })
    }
    fn size_hint(&self) -> Option<usize> {
        let (lo, hi) = self.iter.size_hint();
        if Some(lo) == hi {
            hi
        } else {
            None
        }
    }
}

struct ValueEnumAccess<'de> {
    tag: &'de str,
    payload: Option<&'de Value>,
}

impl<'de> de::EnumAccess<'de> for ValueEnumAccess<'de> {
    type Error = SerdeError;
    type Variant = ValueVariantAccess<'de>;
    fn variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Self::Variant), SerdeError> {
        let value = seed.deserialize(de::value::BorrowedStrDeserializer::<SerdeError>::new(self.tag))?;
        Ok((value, ValueVariantAccess { payload: self.payload }))
    }
}

struct ValueVariantAccess<'de> {
    payload: Option<&'de Value>,
}

impl<'de> de::VariantAccess<'de> for ValueVariantAccess<'de> {
    type Error = SerdeError;
    fn unit_variant(self) -> Result<(), SerdeError> {
        match self.payload {
            None => Ok(()),
            Some(_) => Err(SerdeError::Message(
                "expected a unit variant (bare Keyword), found a payload map".into(),
            )),
        }
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, SerdeError> {
        let payload = self
            .payload
            .ok_or_else(|| SerdeError::Message("expected a newtype-variant payload, found a bare Keyword".into()))?;
        seed.deserialize(ValueDeserializer { value: payload })
    }
    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, SerdeError> {
        let payload = self
            .payload
            .ok_or_else(|| SerdeError::Message("expected a tuple-variant payload, found a bare Keyword".into()))?;
        match payload {
            Value::Vector(v) | Value::List(v) | Value::MapEntry(v) => visitor.visit_seq(ValueSeqAccess { iter: v.iter() }),
            _ => Err(SerdeError::Message(format!(
                "expected a Vector payload for a tuple variant, found a mova {}",
                value_kind(payload)
            ))),
        }
    }
    fn struct_variant<V: Visitor<'de>>(self, _fields: &'static [&'static str], visitor: V) -> Result<V::Value, SerdeError> {
        let payload = self
            .payload
            .ok_or_else(|| SerdeError::Message("expected a struct-variant payload, found a bare Keyword".into()))?;
        match payload {
            Value::Map(m) => visitor.visit_map(ValueMapAccess {
                iter: m.iter(),
                value: None,
                as_identifier: true,
            }),
            Value::HostStruct(hs) => visitor.visit_map(ValueMapAccess {
                iter: crate::host_struct::as_pmap(hs).iter(),
                value: None,
                as_identifier: true,
            }),
            Value::LazyMap(lm) => visitor.visit_map(ValueMapAccess {
                iter: crate::lazy_map::as_pmap(lm).iter(),
                value: None,
                as_identifier: true,
            }),
            _ => Err(SerdeError::Message(format!(
                "expected a Map payload for a struct variant, found a mova {}",
                value_kind(payload)
            ))),
        }
    }
}

#[cfg(test)]
mod tests;
