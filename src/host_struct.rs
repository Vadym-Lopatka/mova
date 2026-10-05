//! W3 (LATENCY-CAMPAIGN.md): the `Value::HostStruct` zero-copy embedding
//! boundary. A host Rust struct wrapped this way is NOT copied into a
//! `PMap` up front (`to_value`'s ~555ns for a flat 10-field struct) --
//! `keyword_lookup` on it dispatches straight to a per-type [`Shape`]
//! descriptor (a shared, pre-built ordered field list), and the whole
//! `Arc<HostStructInner>` wrap is just an `Arc::clone`/`Arc::new` (3.6ns /
//! ~40ns, see the `zerocopy-probe` scratchpad measurements this design is
//! built from).
//!
//! Every map-WIDE operation (`=`, hash-as-key, `merge`, `assoc`/`dissoc`,
//! `reduce-kv`, generic `into`) needs a REAL [`PMap`] to delegate to, so
//! this module provides exactly ONE choke point, [`as_pmap`], that
//! materializes (and caches, via `OnceLock`) the full map on first such
//! touch. Touch-only ops that don't need full map semantics -- `count`
//! (shape length is O(1)), keyword lookup, `keys`/`vals`/`seq`/print
//! (shape order, not `as_pmap`'s alphabetical print order -- see
//! `printer::write_value`'s `HostStruct` arm) -- read the [`Shape`]
//! directly and never materialize anything.
//!
//! # Conformance-by-construction
//!
//! No script-reachable operation can ever construct a `Value::HostStruct`
//! -- there is no reader syntax, no builtin, nothing in `core.mova` that
//! produces one. The only way one enters the interpreter is a host Rust
//! crate calling [`crate::embed::wrap_struct`] and handing the resulting
//! `embed::Value` to `Engine::call`/`Engine::def`. This is what keeps the
//! 756-form conformance corpus and every differential suite untouched by
//! this feature: ordinary script text can never observe a `HostStruct`
//! that this module's `=`/hash/print delegation doesn't already make
//! indistinguishable from a real `Map`.
//!
//! # The identity-keyed inline cache (IC) -- and its one honest residual
//!
//! `keyword_lookup`'s fast path ([`shape_field_index`]) is keyed on the
//! lookup keyword `Str`'s ALLOCATION ADDRESS, not its bytes -- the
//! zerocopy-probe measured a byte-eq descriptor scan at 22.7ns, WORSE than
//! today's `PMap::get` (13.8ns warm); only pointer identity gets under
//! that bar (4.0-9.3ns). This is safe in the overwhelmingly common case
//! because mova keyword LITERALS are read once at parse time and the same
//! `Str` `Arc` is reused on every subsequent eval of that AST/IR node (a
//! hot `(dotimes [_ N] (:weight m))` loop touches the identical `Arc` on
//! every iteration) -- so the IC's "first touch byte-eq, then ptr-hit"
//! policy converges after exactly one miss per call site.
//!
//! The residual, and its interim fix (`perf/ic-strong-arc`): keywords are
//! NOT interned in today's tree (W5/KW-INTERN, Wave 2, closes this
//! structurally), so two DIFFERENT keyword literals with identical text
//! get different `Arc` allocations, and if one is dropped and the
//! allocator reuses its address for an unrelated `Str`, a long-lived
//! `Shape`'s IC slot could alias a stale entry -- this was not
//! theoretical: reproduced at ~91% failure rate over 200 runs of
//! `tests/hoststruct_test.rs` on this tree before the fix below (a freed
//! `:nope`-shaped keyword's address reused by a real field's keyword,
//! `get`'s "absent" check hitting the IC and returning the aliased
//! field's value instead of the fallback default). The fix: each
//! installed IC slot now holds a STRONG `Arc` clone of the keyword `Str`
//! it names, in `Shape::ic_pins` -- a `Mutex`-guarded side array,
//! populated only on the (rare, post-convergence: at most `IC_SLOTS`
//! times ever, per `Shape`) miss/install path, never touched by the hot
//! ptr-eq read loop. As long as a pin lives, its keyword's allocation can
//! never be freed, so its address can never be reused, so the hazard
//! above is now impossible for the life of the `Shape` -- see
//! [`shape_field_index`]'s install path for the pin-before-publish
//! ordering argument. W5/KW-INTERN remains the structural closure (and
//! will make the pin's own bookkeeping unnecessary, since interned
//! keyword cells are never freed at all) but CI does not need to wait for
//! it: the pin alone closes the hazard today, at the cost of one `Arc`
//! clone per distinct keyword a `Shape`'s IC ever caches (bounded by
//! `IC_SLOTS`, i.e. at most 8 extra strong references per `Shape`,
//! process-wide, forever).
use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::value::{Keyword, PMap, Str, Value};

/// Marker bound for anything a host Rust struct wrapped as a
/// `Value::HostStruct` must satisfy. Blanket-implemented for every `T: Any
/// + Send + Sync` (see below) -- a host never implements this by hand.
pub trait HostObj: Any + Send + Sync {
    /// Downcasting entry point [`shape_field_index`]'s getters use.
    fn as_any(&self) -> &dyn Any;
    /// Owned-`Arc` downcasting entry point for
    /// [`crate::embed::from_value_arc`]'s typed extraction fast path.
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

impl<T: Any + Send + Sync> HostObj for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// One field's descriptor within a [`Shape`]: the script-visible keyword
/// text (WITHOUT the leading `:`, same convention `Value::Keyword` uses)
/// plus a getter that reads it off the type-erased host object.
///
/// `get` is a boxed closure rather than a bare `fn` pointer: a
/// runtime-registered [`Shape`] (`crate::embed::ShapeBuilder`, v1's only
/// registration path -- a derive macro is explicitly future work, see this
/// module's doc and LATENCY-CAMPAIGN.md §W3) has no way to hand the
/// compiler a genuinely monomorphic, non-capturing `fn` item per field;
/// the getter has to close over the concrete downcast type `T` and the
/// host's own accessor closure. This costs one vtable call per miss/refresh
/// that a codegen'd (derive-macro) getter wouldn't -- a disclosed, bounded
/// deviation from the zerocopy-probe's toy `type Getter = fn(&FlatStruct)
/// -> Value` (which was monomorphic only because the probe hard-coded one
/// concrete struct type).
pub struct ShapeField {
    pub key: Str,
    pub get: GetterFn,
}

/// `ShapeField::get`'s type, named to keep `clippy::type_complexity`
/// quiet -- see `ShapeField`'s doc for why this is a boxed closure rather
/// than the zerocopy-probe's toy bare `fn` pointer.
pub type GetterFn = Box<dyn Fn(&dyn HostObj) -> Value + Send + Sync>;

/// Number of append-only inline-cache slots per [`Shape`]. One cache line
/// (8 slots * 8 bytes) -- generous for the realistic case (a hot call site
/// touches 1-3 distinct fields of a given shape), and a full IC just
/// degrades to the still-correct byte-eq linear scan for any keyword past
/// the first 8 distinct ones touched against this shape process-wide.
const IC_SLOTS: usize = 8;

/// Ordered field descriptors for one host Rust type, built ONCE (not per
/// instance) via `crate::embed::ShapeBuilder::build` and shared by every
/// `Value::HostStruct` wrapping an instance of that type through one
/// `Arc<Shape>`.
///
/// publication pattern: see ARCHITECTURE.md, "Publication patterns
/// (lock-free read paths)" -- OnceLock inline cache (packed-word variant).
pub struct Shape {
    /// Host-facing name, for diagnostics/printing only (not part of any
    /// equality/hash contract).
    pub type_name: &'static str,
    pub fields: Vec<ShapeField>,
    /// See [`shape_field_index`]/`ic_pack` for the packed-pointer encoding
    /// and this module's doc for the identity-keyed IC's residual.
    ic: [AtomicU64; IC_SLOTS],
    /// The ABA-hazard fix (`perf/ic-strong-arc`): one slot-indexed STRONG
    /// `Arc` pin per installed IC entry, so the keyword `Str` a live `ic`
    /// slot names can never be freed (and its address therefore never
    /// reused) for the life of this `Shape`. Written ONLY by the
    /// miss/install path (`install`, below) -- the hot ptr-eq read loop in
    /// [`shape_field_index`] never touches this `Mutex`, so a lookup that
    /// hits stays a single relaxed atomic load + compare, unchanged from
    /// before this fix. A `Mutex` (not per-slot atomics) is deliberate:
    /// installs are cold (at most `IC_SLOTS` ever, per `Shape`, process-
    /// wide, once the IC has converged), so serializing them fully is
    /// free perf-wise and makes the install path's correctness trivial to
    /// see -- no two threads can ever be mid-install on the same `Shape`
    /// at once, so there is no way for one installer's pin write to race
    /// another's.
    ic_pins: Mutex<[Option<Str>; IC_SLOTS]>,
}

impl Shape {
    pub fn new(type_name: &'static str, fields: Vec<ShapeField>) -> Shape {
        Shape {
            type_name,
            fields,
            ic: [const { AtomicU64::new(0) }; IC_SLOTS],
            ic_pins: Mutex::new(std::array::from_fn(|_| None)),
        }
    }

    /// `true` iff `key` matches shape field index -- for the (rare) case a
    /// caller wants the boolean without the field's `Value` (avoids the
    /// leaf-slab touch `get_field` would do). `count`/`contains?`-style
    /// touch-only ops use this via [`shape_field_index`] directly instead.
    pub fn field_index_by_bytes(&self, key: &Str) -> Option<usize> {
        self.fields.iter().position(|f| &f.key == key)
    }
}

/// Packs a `Str` allocation address (48 usable bits on every real x86-64/
/// ARM64 userspace address space) and a field index (16 high bits, so up
/// to 65535 fields -- shapes in practice have single-digit-to-low-hundreds
/// fields) into one `u64` so an IC hit is a SINGLE relaxed atomic load +
/// compare, never a torn read of two separately-updated words.
#[inline]
fn ic_pack(ptr: usize, idx: usize) -> u64 {
    debug_assert!(idx <= 0xFFFF, "Shape field index overflowed the IC's 16-bit budget");
    ((ptr as u64) & 0x0000_FFFF_FFFF_FFFF) | ((idx as u64) << 48)
}

#[inline]
fn ic_unpack(word: u64) -> (usize, usize) {
    ((word & 0x0000_FFFF_FFFF_FFFF) as usize, (word >> 48) as usize)
}

/// Resolves `kw` to a field index within `shape`, via the IC's ptr-eq fast
/// path first, falling back to (and then installing into the IC) a byte-eq
/// linear scan of `shape.fields` -- exactly the "first touch byte-eq, then
/// ptr-hit" policy this module's doc describes. `None` iff `kw` genuinely
/// names no field of this shape.
///
/// HOT PATH (unchanged by the `perf/ic-strong-arc` fix, byte-for-byte):
/// one relaxed atomic load + one `usize` compare per slot, zero locks,
/// zero new loads on a hit. The fix lives entirely in `install`, below.
pub fn shape_field_index(shape: &Shape, kw: &Str) -> Option<usize> {
    let ptr = kw.identity_addr();
    for slot in &shape.ic {
        let word = slot.load(Ordering::Relaxed);
        if word == 0 {
            continue;
        }
        let (p, idx) = ic_unpack(word);
        if p == ptr {
            // field4/W-LENS-1: the hit half of the IC's rate. One TLS load
            // and one uncontended relaxed load/store -- notably NOT the
            // shared-cacheline RMW this IC's whole ordering argument exists
            // to keep off the hit path.
            crate::lens::event(crate::lens::Event::HostStructIcHit);
            return Some(idx);
        }
    }
    // field4/W-LENS-1: a miss re-scans the shape's field list by BYTES.
    // Counted before the `?` so a lookup for a keyword that names no field
    // at all still registers as a miss -- it did the scan either way.
    crate::lens::event(crate::lens::Event::HostStructIcMiss);
    let idx = shape.field_index_by_bytes(kw)?;
    install(shape, kw, ptr, idx);
    Some(idx)
}

/// Miss/install path ONLY -- never reached on an IC hit. Pins `kw` (a
/// strong `Arc` clone, in `shape.ic_pins`) before publishing the packed
/// word into the first still-empty `shape.ic` slot, closing the ABA
/// hazard this module's doc describes: a slot's packed pointer can never
/// outlive the `Arc` that pin holds, so the address it names can never be
/// freed-and-reused out from under a future hit.
///
/// # Ordering argument
///
/// The hot read loop above stays a `Relaxed` load (a hard requirement --
/// no new loads on the hit path), so this can't lean on a matching
/// `Acquire` to hand the pin's CONTENT across threads the usual
/// publish/consume way. It doesn't need to: the pin's job is a real,
/// physical side effect -- an `Arc` strong-count increment that keeps an
/// allocation alive -- not a value any reader ever inspects. What matters
/// is that this increment has UNCONDITIONALLY already happened, in real
/// execution order, by the time any thread's `Relaxed` load of
/// `shape.ic[slot]` can possibly observe the new packed word. That's
/// guaranteed by two facts together:
///
/// 1. Within this function, the pin write (`pins[slot_idx] = Some(..)`)
///    is sequenced-before the atomic store into `shape.ic[slot_idx]` in
///    program order, on the SAME thread -- a single thread's own writes
///    are never reordered with each other from that thread's point of
///    view.
/// 2. The atomic store is a `Release` store. `Release` forbids the
///    compiler/CPU from hoisting the packed-word write's globally-visible
///    effect ahead of any earlier write in this thread's program order --
///    so the pin's refcount bump is guaranteed to have actually executed
///    before the packed word becomes observable to ANYONE, including a
///    `Relaxed` reader that never synchronizes with this store at all.
///    (This is the standard "publish a pointer via `Release`, consume via
///    plain load" idiom used when the reader only needs the PUBLICATION
///    to be ordered, not the payload -- exactly this case.)
///
/// There is therefore no window in which a slot's packed pointer is
/// visible before its pin is in place.
///
/// Slots are append-only (this fn only ever writes a currently-`0` slot,
/// never overwrites a populated one -- see the loop below), so there is,
/// today, no "drop the old pin" case: once a slot's pin is installed it
/// lives exactly as long as its `Shape` does (dropped together, when the
/// `Shape`'s `Arc` finally reaches zero). If a future
/// eviction/refresh policy is ever added to this IC, the same rule
/// applies in reverse and must be preserved: clear `shape.ic[slot]`
/// FIRST (to a word no reader can match against), and only drop the old
/// pin from `ic_pins` AFTER that clear is durably published -- never
/// the other order, or a reader could still be mid-hit against an
/// address whose pin has already been dropped.
fn install(shape: &Shape, kw: &Str, ptr: usize, idx: usize) {
    // Single per-`Shape` mutex serializes ALL installers -- see
    // `Shape::ic_pins`'s doc for why that's free (installs are cold) and
    // sufficient (no two installers can ever race on this `Shape` at
    // once, so there's nothing further to reason about for correctness
    // between competing installs).
    let mut pins = shape.ic_pins.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for (slot_idx, slot) in shape.ic.iter().enumerate() {
        if slot.load(Ordering::Relaxed) == 0 {
            // Pin BEFORE publish -- see this fn's ordering argument.
            pins[slot_idx] = Some(kw.clone());
            let packed = ic_pack(ptr, idx);
            slot.store(packed, Ordering::Release);
            return;
        }
    }
    // All `IC_SLOTS` already claimed by other keywords this shape has
    // seen before -- this keyword just never gets cached; future lookups
    // keep falling back to the byte-eq scan, correct, only slower, exactly
    // as before this fix.
}

/// A live `Value::HostStruct` cell: the type-erased host object, its
/// shared [`Shape`], and the two lazy caches every op above delegates
/// through -- see this module's doc for which ops touch which cache.
pub struct HostStructInner {
    pub obj: Arc<dyn HostObj>,
    pub shape: Arc<Shape>,
    /// The ONE materialize choke point's cache -- see [`as_pmap`].
    snap: OnceLock<PMap>,
    /// Per-instance, per-field `Value` cache (lazily sized to
    /// `shape.fields.len()` on first field touch, not eagerly at wrap
    /// time -- most instances never have every field read). Makes a
    /// repeated read of the same `String`/collection-valued field
    /// alloc-free after the first touch AND `identical?`-stable (every
    /// read after the first returns a clone of the SAME cached `Value`,
    /// same `Arc` -- matching every other cell-backed `Value`'s
    /// `identical?` contract instead of minting a fresh allocation, and
    /// therefore a fresh identity, on every single lookup).
    leaf_slab: OnceLock<Box<[OnceLock<Value>]>>,
}

impl HostStructInner {
    pub fn new(obj: Arc<dyn HostObj>, shape: Arc<Shape>) -> HostStructInner {
        HostStructInner {
            obj,
            shape,
            snap: OnceLock::new(),
            leaf_slab: OnceLock::new(),
        }
    }
}

/// Field count -- O(1), no materialize (`shape.fields.len()` directly).
/// `builtins::collections`'s `count` uses this instead of `as_pmap(..)
/// .len()` for exactly the reason `LATENCY-CAMPAIGN.md` calls out: no
/// reason to pay a full materialize just to answer "how many fields".
pub fn count(inner: &HostStructInner) -> usize {
    inner.shape.fields.len()
}

/// Reads field `idx` (already resolved via [`shape_field_index`]),
/// through the leaf slab so a repeated read of the same field is
/// alloc-free and `identical?`-stable after the first touch (see
/// [`HostStructInner::leaf_slab`]'s doc).
pub fn get_field(inner: &HostStructInner, idx: usize) -> Value {
    let slab = inner
        .leaf_slab
        .get_or_init(|| (0..inner.shape.fields.len()).map(|_| OnceLock::new()).collect::<Vec<_>>().into_boxed_slice());
    slab[idx].get_or_init(|| (inner.shape.fields[idx].get)(inner.obj.as_ref())).clone()
}

/// `keyword_lookup`'s `HostStruct` fast path: shape-dispatch, no
/// materialize. `None` iff `kw` names no field of `inner`'s shape (caller
/// falls back to whatever default `:kw`/`get` semantics dictate).
pub fn lookup(inner: &HostStructInner, kw: &Str) -> Option<Value> {
    let idx = shape_field_index(&inner.shape, kw)?;
    Some(get_field(inner, idx))
}

/// THE materialize choke point (see this module's doc): builds (once,
/// caching in `inner.snap`) a real [`PMap`] with one `(Keyword, Value)`
/// entry per shape field, IN SHAPE ORDER (so a shape with <= `PMAP_SMALL_MAX`
/// fields stays `PMap::Small` and its own iteration order happens to match
/// shape order too -- not relied upon by anything: `keys`/`vals`/`seq`/print
/// all read `inner.shape`/`get_field` directly instead of this map, exactly
/// to keep their ordering guarantee independent of `PMap`'s own promotion
/// policy). Every map-WIDE op (`=` against a real `Map`, hash-as-key,
/// `merge`, `assoc`/`dissoc` [v1: materialize+update, no overlay -- see
/// this module's doc], `reduce-kv`, generic `into`) goes through this.
pub fn as_pmap(inner: &HostStructInner) -> &PMap {
    inner.snap.get_or_init(|| {
        let pairs: Vec<(Value, Value)> = inner
            .shape
            .fields
            .iter()
            .enumerate()
            .map(|(idx, f)| (Value::Keyword(Keyword::from(&f.key)), get_field(inner, idx)))
            .collect();
        PMap::from_unique_pairs(pairs)
    })
}

/// Typed extraction fast path (`crate::embed::from_value_typed::<T>`):
/// `TypeId`-checked downcast + `T::clone`, ~30ns per the zerocopy-probe
/// (vs `from_value`'s ~100-200ns deserialize-from-`PMap` path). `None` if
/// `inner`'s underlying object is not exactly a `T` (a script `assoc`
/// widened it into a plain `Map` long before this is ever called -- see
/// `crate::embed::from_value_typed`'s own doc for why that can never
/// misfire: a `Value::Map` never reaches this fn at all, only a genuine
/// still-`HostStruct` value does).
pub fn downcast_ref<T: Any>(inner: &HostStructInner) -> Option<&T> {
    // `.as_ref()` FIRST is load-bearing, not stylistic: `inner.obj` is an
    // owned `Arc<dyn HostObj>`, and `Arc<dyn HostObj>` itself satisfies the
    // blanket `impl<T: Any + Send + Sync> HostObj for T` (an `Arc` of a
    // `'static + Send + Sync` trait object is itself `Any + Send + Sync`).
    // Calling `.as_any()` directly on the `Arc` value resolves -- via
    // ordinary method-call autoref, which tries the receiver's OWN type
    // before deref-ing -- to THAT blanket impl (the getter for the `Arc`
    // wrapper's own `TypeId`, not the wrapped object's), silently
    // downcasting against the wrong type and always returning `None`.
    // `.as_ref()` forces the receiver to `&dyn HostObj` explicitly, so
    // method resolution reaches the trait object's own vtable dispatch
    // instead. Confirmed with a standalone repro before landing this fix.
    inner.obj.as_ref().as_any().downcast_ref::<T>()
}

/// `Arc<T>` extraction (`crate::embed::from_value_arc::<T>`): no clone at
/// all, just a shared-ownership handle to the SAME host allocation --
/// ~3.6ns (an `Arc::clone` + one `downcast` check), the "read-mostly round
/// trip" the campaign doc measures against `to_value`'s ~555ns +
/// `from_value`'s own deserialize cost.
pub fn downcast_arc<T: Any + Send + Sync>(inner: &HostStructInner) -> Option<Arc<T>> {
    Arc::clone(&inner.obj).as_any_arc().downcast::<T>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Widget {
        weight: i64,
        name: String,
    }

    fn shape() -> Arc<Shape> {
        Arc::new(Shape::new(
            "Widget",
            vec![
                ShapeField {
                    key: Str::from("weight"),
                    get: Box::new(|o: &dyn HostObj| Value::Int(o.as_any().downcast_ref::<Widget>().unwrap().weight)),
                },
                ShapeField {
                    key: Str::from("name"),
                    get: Box::new(|o: &dyn HostObj| Value::Str(Str::from(o.as_any().downcast_ref::<Widget>().unwrap().name.as_str()))),
                },
            ],
        ))
    }

    #[test]
    fn lookup_and_ic_convergence() {
        let sh = shape();
        let obj: Arc<dyn HostObj> = Arc::new(Widget {
            weight: 42,
            name: "gadget".into(),
        });
        let inner = HostStructInner::new(obj, sh.clone());
        let kw = Str::from("weight");
        // First touch: byte-eq fallback + IC install.
        assert_eq!(lookup(&inner, &kw), Some(Value::Int(42)));
        // Second touch, SAME Arc: ptr-hit.
        assert_eq!(lookup(&inner, &kw), Some(Value::Int(42)));
        assert_eq!(lookup(&inner, &Str::from("nope")), None);
    }

    #[test]
    fn leaf_slab_identical_stable() {
        let sh = shape();
        let obj: Arc<dyn HostObj> = Arc::new(Widget {
            weight: 1,
            name: "same-alloc".into(),
        });
        let inner = HostStructInner::new(obj, sh);
        let a = lookup(&inner, &Str::from("name")).unwrap();
        let b = lookup(&inner, &Str::from("name")).unwrap();
        match (&a, &b) {
            (Value::Str(x), Value::Str(y)) => assert!(Str::ptr_eq(x, y), "leaf slab must return the same Str allocation"),
            _ => panic!("expected Str"),
        }
    }

    #[test]
    fn as_pmap_materializes_all_fields_in_shape_order() {
        let sh = shape();
        let obj: Arc<dyn HostObj> = Arc::new(Widget {
            weight: 7,
            name: "m".into(),
        });
        let inner = HostStructInner::new(obj, sh);
        let m = as_pmap(&inner);
        assert_eq!(m.len(), 2);
        let keys: Vec<&str> = m.iter().map(|(k, _)| match k {
            Value::Keyword(s) => s.as_ref(),
            _ => unreachable!(),
        }).collect();
        assert_eq!(keys, vec!["weight", "name"]);
    }

    #[test]
    fn downcast_ref_and_arc() {
        let sh = shape();
        let obj: Arc<dyn HostObj> = Arc::new(Widget {
            weight: 5,
            name: "d".into(),
        });
        let inner = HostStructInner::new(obj, sh);
        assert_eq!(downcast_ref::<Widget>(&inner).unwrap().weight, 5);
        let arc = downcast_arc::<Widget>(&inner).unwrap();
        assert_eq!(arc.weight, 5);
    }
}
