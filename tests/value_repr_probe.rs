//! E2 probe: which `Value` representation should Mova's native (Cranelift)
//! tier use at the call boundary? Compares R32-own (today: `Value` by
//! value, clone-in/drop-after), R32-borrow (`&Value`, no clone/drop), and
//! R8 (8-byte NaN/pointer-tagged word: small int/nil/bool/char/interned-kw
//! immediate, else a raw pointer to a heap-boxed `Arc<Value>`).
//!
//! K1-K6 measure STEADY-STATE cost only (args already live in the given
//! repr, no conversion in the timed loop) -- conversion/boundary cost is
//! measured separately (`conv_*`, `k2_r8_with_boundary`) per the task
//! brief, since that is what determines whether a K1-K3 win survives at
//! the actual native<->runtime crossing.
//!
//! Run: `cargo test --release -q --test value_repr_probe -- --ignored --nocapture`

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use mova::internal::{Keyword, PMap, Value};

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

const ITERS: u64 = 10_000_000;

fn median5(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[2]
}

/// ns/op, median of 5 rounds of `ITERS` calls each.
fn bench<F: FnMut() -> ()>(mut f: F) -> f64 {
    // warmup
    for _ in 0..1000 {
        f();
    }
    let mut rounds = Vec::with_capacity(5);
    for _ in 0..5 {
        let t0 = Instant::now();
        for _ in 0..ITERS {
            f();
        }
        rounds.push(t0.elapsed().as_secs_f64() / ITERS as f64 * 1e9);
    }
    median5(rounds)
}

// ---------------------------------------------------------------------------
// R8 encoding: low 3 bits are the tag. Small int is arithmetic-shifted by 3
// (61-bit range, no overflow check here -- a real impl would fall back to a
// boxed bignum path; this probe only needs values that fit). Interned
// keyword id is embedded directly (immediate, no pointer chase). Pointer
// tag is 0 (heap allocations are >=8-byte aligned).
// ---------------------------------------------------------------------------

const TAG_MASK: u64 = 0b111;
const TAG_PTR: u64 = 0b000;
const TAG_INT: u64 = 0b001;
const TAG_KW: u64 = 0b101;

#[inline]
fn r8_encode_int(i: i64) -> u64 {
    ((i as u64) << 3) | TAG_INT
}
#[inline]
fn r8_decode_int(w: u64) -> i64 {
    (w as i64) >> 3
}
#[inline]
fn r8_encode_kw(id: u32) -> u64 {
    ((id as u64) << 3) | TAG_KW
}

/// Boundary conversion: R32 -> R8 for a `Value`. Heap variants box into a
/// FRESH `Arc<Value>` (allocation) since `Value` itself is not already
/// behind a uniform pointer -- this is the real cost of crossing into an
/// R8-word native ABI for anything that isn't an immediate.
#[inline(never)]
fn r32_to_r8(v: &Value) -> u64 {
    match v {
        Value::Int(i) => r8_encode_int(*i),
        Value::Keyword(k) => match k.interned_id() {
            Some(id) => r8_encode_kw(id),
            None => box_ptr(v),
        },
        _ => box_ptr(v),
    }
}

#[inline(never)]
fn box_ptr(v: &Value) -> u64 {
    let arc = Arc::new(v.clone()); // clone: atomic inc for heap-backed variants
    let ptr = Arc::into_raw(arc) as u64;
    debug_assert_eq!(ptr & TAG_MASK, 0);
    ptr | TAG_PTR
}

/// Boundary conversion: R8 -> R32.
#[inline(never)]
fn r8_to_r32(w: u64) -> Value {
    match w & TAG_MASK {
        TAG_INT => Value::Int(r8_decode_int(w)),
        TAG_KW => Value::Keyword(Keyword::Interned((w >> 3) as u32)),
        TAG_PTR => {
            let ptr = w as *const Value;
            let arc = unsafe { Arc::from_raw(ptr) };
            (*arc).clone() // atomic inc; `arc` drops below -> atomic dec (+ maybe dealloc)
        }
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn kw(s: &str) -> Keyword {
    Keyword::construct(s)
}

fn make_map4() -> Value {
    let mut m = PMap::new();
    for (k, v) in [("a", 1), ("b", 2), ("c", 3), ("d", 4)] {
        m.insert(Value::Keyword(kw(k)), Value::Int(v));
    }
    Value::Map(m)
}

// ---------------------------------------------------------------------------
// K1: f(x) -> x+1, int arg
// ---------------------------------------------------------------------------

#[inline(never)]
fn k1_own(x: Value) -> Value {
    match x {
        Value::Int(i) => Value::Int(i + 1),
        _ => unreachable!(),
    }
}
#[inline(never)]
fn k1_borrow(x: &Value) -> Value {
    match x {
        Value::Int(i) => Value::Int(i + 1),
        _ => unreachable!(),
    }
}
#[inline(never)]
fn k1_r8(w: u64) -> u64 {
    r8_encode_int(r8_decode_int(w) + 1)
}

// ---------------------------------------------------------------------------
// K2: f(m) -> count-ish, 4-key map arg
// ---------------------------------------------------------------------------

#[inline(never)]
fn k2_own(m: Value) -> Value {
    match &m {
        Value::Map(pm) => Value::Int(pm.len() as i64),
        _ => unreachable!(),
    }
}
#[inline(never)]
fn k2_borrow(m: &Value) -> Value {
    match m {
        Value::Map(pm) => Value::Int(pm.len() as i64),
        _ => unreachable!(),
    }
}
/// bare: `w` is already a live boxed pointer, no conversion in the loop.
#[inline(never)]
fn k2_r8_bare(w: u64) -> i64 {
    let v = unsafe { &*(w as *const Value) };
    match v {
        Value::Map(pm) => pm.len() as i64,
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// K3a: (:a m), real generic PMap::get path
// K3b: (:a m), shape inline cache -- 4-key slot array, keyword id compare
// ---------------------------------------------------------------------------

#[inline(never)]
fn k3a_own(m: Value, key: Value) -> Option<Value> {
    match &m {
        Value::Map(pm) => pm.get(&key).cloned(),
        _ => unreachable!(),
    }
}
#[inline(never)]
fn k3a_borrow(m: &Value, key: &Value) -> Option<Value> {
    match m {
        Value::Map(pm) => pm.get(key).cloned(),
        _ => unreachable!(),
    }
}
#[inline(never)]
fn k3a_r8(mw: u64, key_id: u32) -> Option<Value> {
    let m = unsafe { &*(mw as *const Value) };
    let key = Value::Keyword(Keyword::Interned(key_id)); // immediate -> Value, no alloc
    match m {
        Value::Map(pm) => pm.get(&key).cloned(),
        _ => unreachable!(),
    }
}

/// shape IC: 4 keyword ids + 4 values, slot array, id compare (no PMap
/// descent at all -- this is what a monomorphic native call site gets
/// after inline-caching the map's shape).
struct Ic4 {
    ids: [u32; 4],
    vals: [Value; 4],
}
fn ic4_from_map4() -> Ic4 {
    Ic4 {
        ids: [kw("a").interned_id().unwrap(), kw("b").interned_id().unwrap(), kw("c").interned_id().unwrap(), kw("d").interned_id().unwrap()],
        vals: [Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)],
    }
}
#[inline(never)]
fn k3b_own(ic: Ic4, key_id: u32) -> Option<Value> {
    for i in 0..4 {
        if ic.ids[i] == key_id {
            return Some(ic.vals[i].clone());
        }
    }
    None
}
#[inline(never)]
fn k3b_borrow(ic: &Ic4, key_id: u32) -> Option<Value> {
    for i in 0..4 {
        if ic.ids[i] == key_id {
            return Some(ic.vals[i].clone());
        }
    }
    None
}
#[inline(never)]
fn k3b_r8(ic: &Ic4, key_id: u32) -> Option<u64> {
    for i in 0..4 {
        if ic.ids[i] == key_id {
            return Some(r8_encode_int(match ic.vals[i] {
                Value::Int(n) => n,
                _ => unreachable!(),
            }));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// K4: assoc :e on a 4-key map (real mova assoc semantics: clone-then-insert,
// same as `builtins::collections::assoc_one`'s `Value::Map` arm for a
// non-Lazy key -- only the ARG PASSING convention changes across reprs).
// ---------------------------------------------------------------------------

#[inline(never)]
fn k4_own(m: Value, k: Value, v: Value) -> Value {
    match &m {
        Value::Map(pm) => {
            let mut m2 = pm.clone();
            m2.insert(k, v);
            Value::Map(m2)
        }
        _ => unreachable!(),
    }
}
#[inline(never)]
fn k4_borrow(m: &Value, k: &Value, v: &Value) -> Value {
    match m {
        Value::Map(pm) => {
            let mut m2 = pm.clone();
            m2.insert(k.clone(), v.clone());
            Value::Map(m2)
        }
        _ => unreachable!(),
    }
}
#[inline(never)]
fn k4_r8_bare(mw: u64, key_id: u32, val: i64) -> u64 {
    let m = unsafe { &*(mw as *const Value) };
    match m {
        Value::Map(pm) => {
            let mut m2 = pm.clone();
            m2.insert(Value::Keyword(Keyword::Interned(key_id)), Value::Int(val));
            box_ptr(&Value::Map(m2))
        }
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// K5: sum 100 ints from a vector via `nth` (mova's real underlying index
// op is `PVec::get`; the public `nth` builtin adds Indexed-deftype/
// interface dispatch unrelated to Value representation, so this measures
// the representation-sensitive part directly). Unit = ONE index op.
// ---------------------------------------------------------------------------

fn make_vec100() -> Value {
    let mut v = mova::internal::PVec::new();
    for i in 0..100i64 {
        v.push_back(Value::Int(i));
    }
    Value::Vector(v)
}

#[inline(never)]
fn k5_own(v: Value, idx: usize) -> i64 {
    match &v {
        Value::Vector(pv) => match pv.get(idx) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        },
        _ => unreachable!(),
    }
}
#[inline(never)]
fn k5_borrow(v: &Value, idx: usize) -> i64 {
    match v {
        Value::Vector(pv) => match pv.get(idx) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        },
        _ => unreachable!(),
    }
}
#[inline(never)]
fn k5_r8(vw: u64, idx: usize) -> i64 {
    let v = unsafe { &*(vw as *const Value) };
    match v {
        Value::Vector(pv) => match pv.get(idx) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        },
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// K6: keyword equality / hash, real mova `Keyword` impls vs R8 immediate id
// ---------------------------------------------------------------------------

#[inline(never)]
fn k6_eq_r32(a: &Value, b: &Value) -> bool {
    a == b
}
#[inline(never)]
fn k6_eq_r8(a: u64, b: u64) -> bool {
    a == b
}
#[inline(never)]
fn k6_hash_r32(a: &Value) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    match a {
        Value::Keyword(k) => k.hash(&mut h),
        _ => unreachable!(),
    }
    h.finish()
}
#[inline(never)]
fn k6_hash_r8(id: u32) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// atomic inc+dec pair cost
// ---------------------------------------------------------------------------

fn atomic_uncontended_ns() -> f64 {
    let a = Arc::new(0i64);
    bench(|| {
        let c = black_box(a.clone()); // atomic inc
        drop(c); // atomic dec
    })
}

fn atomic_contended_ns() -> f64 {
    const N: u64 = 5_000_000;
    let shared = Arc::new(0i64);
    let s1 = shared.clone();
    let s2 = shared.clone();
    let t0 = Instant::now();
    let h1 = std::thread::spawn(move || {
        for _ in 0..N {
            let c = black_box(s1.clone());
            drop(c);
        }
    });
    let h2 = std::thread::spawn(move || {
        for _ in 0..N {
            let c = black_box(s2.clone());
            drop(c);
        }
    });
    h1.join().unwrap();
    h2.join().unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    elapsed / (N as f64) * 1e9 // ns/op experienced per thread, contended
}

// ---------------------------------------------------------------------------
// conversion cost (boundary), int and map, both directions
// ---------------------------------------------------------------------------

fn conv_int_32_to_8_ns() -> f64 {
    let v = Value::Int(41);
    bench(|| {
        black_box(r32_to_r8(black_box(&v)));
    })
}
fn conv_int_8_to_32_ns() -> f64 {
    let w = r8_encode_int(41);
    bench(|| {
        black_box(r8_to_r32(black_box(w)));
    })
}
fn conv_map_32_to_8_ns() -> f64 {
    let v = make_map4();
    bench(|| {
        let w = r32_to_r8(black_box(&v));
        // symmetric cleanup: drop the box we just made (mirrors a call that
        // never got a chance to escape further) so we don't leak/measure
        // an ever-growing heap across 5e7 iterations.
        drop(unsafe { Arc::from_raw(w as *const Value) });
    })
}
fn conv_map_8_to_32_ns() -> f64 {
    // unboxing CONSUMES the box (Arc::from_raw), so each iter needs a fresh
    // one -- the alloc cost is charged to r32_to_r8 above, not duplicated
    // here; this isolates the from_raw+clone+drop side alone by boxing
    // outside is impossible without reusing a freed ptr, so we box fresh
    // per iter and report the unbox-only delta by subtracting box_ptr's
    // own cost (measured separately as conv_map_32_to_8_ns) in the doc.
    let v = make_map4();
    bench(|| {
        let w = box_ptr(black_box(&v));
        black_box(r8_to_r32(w));
    })
}

// ---------------------------------------------------------------------------
// main probe
// ---------------------------------------------------------------------------

#[test]
fn value_repr_roundtrip_sanity() {
    assert_eq!(std::mem::size_of::<Value>(), 32);
    let i = Value::Int(-7);
    let w = r32_to_r8(&i);
    assert!(matches!(r8_to_r32(w), Value::Int(-7)));
    let m = make_map4();
    let w = r32_to_r8(&m);
    match r8_to_r32(w) {
        Value::Map(pm) => assert_eq!(pm.len(), 4),
        _ => panic!("expected map"),
    }
    drop(unsafe { Arc::from_raw(w as *const Value) });
}

#[test]
#[ignore]
fn value_repr_probe() {
    println!("size_of::<Value>() = {}", std::mem::size_of::<Value>());
    println!("size_of::<u64>() (R8) = {}", std::mem::size_of::<u64>());
    println!();
    println!("atomic uncontended inc+dec pair: {:.2} ns/op", atomic_uncontended_ns());
    println!("atomic contended (2 threads)   : {:.2} ns/op", atomic_contended_ns());
    println!();
    println!("conv int  R32->R8: {:.2} ns/op", conv_int_32_to_8_ns());
    println!("conv int  R8->R32: {:.2} ns/op", conv_int_8_to_32_ns());
    println!("conv map  R32->R8: {:.2} ns/op", conv_map_32_to_8_ns());
    println!("conv map  R8->R32: {:.2} ns/op", conv_map_8_to_32_ns());
    println!();

    let one = Value::Int(1);
    let one_r8 = r8_encode_int(1);
    println!("K1 own   : {:.2}", bench(|| { black_box(k1_own(black_box(one.clone()))); }));
    println!("K1 borrow: {:.2}", bench(|| { black_box(k1_borrow(black_box(&one))); }));
    println!("K1 r8    : {:.2}", bench(|| { black_box(k1_r8(black_box(one_r8))); }));
    println!();

    let m = make_map4();
    let m_r8 = box_ptr(&m);
    println!("K2 own       : {:.2}", bench(|| { black_box(k2_own(black_box(m.clone()))); }));
    println!("K2 borrow    : {:.2}", bench(|| { black_box(k2_borrow(black_box(&m))); }));
    println!("K2 r8 bare   : {:.2}", bench(|| { black_box(k2_r8_bare(black_box(m_r8))); }));
    println!("K2 r8 +bound.: {:.2}", bench(|| {
        let w = r32_to_r8(black_box(&m));
        black_box(k2_r8_bare(w));
        drop(unsafe { Arc::from_raw(w as *const Value) });
    }));
    println!();

    let key = Value::Keyword(kw("a"));
    let key_id = kw("a").interned_id().unwrap();
    println!("K3a own   : {:.2}", bench(|| { black_box(k3a_own(black_box(m.clone()), black_box(key.clone()))); }));
    println!("K3a borrow: {:.2}", bench(|| { black_box(k3a_borrow(black_box(&m), black_box(&key))); }));
    println!("K3a r8    : {:.2}", bench(|| { black_box(k3a_r8(black_box(m_r8), black_box(key_id))); }));

    let ic = ic4_from_map4();
    println!("K3b own   : {:.2}", bench(|| { black_box(k3b_own(black_box(Ic4 { ids: ic.ids, vals: ic.vals.clone() }), black_box(key_id))); }));
    println!("K3b borrow: {:.2}", bench(|| { black_box(k3b_borrow(black_box(&ic), black_box(key_id))); }));
    println!("K3b r8    : {:.2}", bench(|| { black_box(k3b_r8(black_box(&ic), black_box(key_id))); }));
    println!();

    let e_key = Value::Keyword(kw("e"));
    let e_val = Value::Int(5);
    let e_id = kw("e").interned_id().unwrap();
    println!("K4 own       : {:.2}", bench(|| { black_box(k4_own(black_box(m.clone()), black_box(e_key.clone()), black_box(e_val.clone()))); }));
    println!("K4 borrow    : {:.2}", bench(|| { black_box(k4_borrow(black_box(&m), black_box(&e_key), black_box(&e_val))); }));
    println!("K4 r8 (bare) : {:.2}", bench(|| {
        let w = black_box(k4_r8_bare(black_box(m_r8), black_box(e_id), black_box(5)));
        drop(unsafe { Arc::from_raw(w as *const Value) });
    }));
    println!();

    let vec100 = make_vec100();
    let vec100_r8 = box_ptr(&vec100);
    println!("K5 own   : {:.2}", bench({
        let mut idx = 0usize;
        move || { black_box(k5_own(black_box(vec100.clone()), idx)); idx = (idx + 1) % 100; }
    }));
    let vec100b = make_vec100();
    println!("K5 borrow: {:.2}", bench({
        let mut idx = 0usize;
        move || { black_box(k5_borrow(black_box(&vec100b), idx)); idx = (idx + 1) % 100; }
    }));
    println!("K5 r8    : {:.2}", bench({
        let mut idx = 0usize;
        move || { black_box(k5_r8(black_box(vec100_r8), idx)); idx = (idx + 1) % 100; }
    }));
    println!();

    let a_key = Value::Keyword(kw("a"));
    let b_key = Value::Keyword(kw("b"));
    let a_id = kw("a").interned_id().unwrap();
    let b_id = kw("b").interned_id().unwrap();
    println!("K6 eq   r32: {:.2}", bench(|| { black_box(k6_eq_r32(black_box(&a_key), black_box(&b_key))); }));
    println!("K6 eq   r8 : {:.2}", bench(|| { black_box(k6_eq_r8(black_box(a_id as u64), black_box(b_id as u64))); }));
    println!("K6 hash r32: {:.2}", bench(|| { black_box(k6_hash_r32(black_box(&a_key))); }));
    println!("K6 hash r8 : {:.2}", bench(|| { black_box(k6_hash_r8(black_box(a_id))); }));

    drop(unsafe { Arc::from_raw(m_r8 as *const Value) });
    drop(unsafe { Arc::from_raw(vec100_r8 as *const Value) });
}
