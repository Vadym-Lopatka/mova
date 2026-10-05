//! The public half of the `Value::HostStruct` zero-copy embedding boundary
//! (W3, LATENCY-CAMPAIGN.md): register a [`Shape`] once per host Rust type
//! via [`ShapeBuilder`], then hand script code a live view of any number
//! of instances via [`wrap_struct`] -- no `to_value`-style up-front copy,
//! no allocation beyond the one `Arc` wrapper (see `crate::host_struct`'s
//! module doc for the full engine-side delegation story: `=`, hashing-as-
//! a-key, `seq`/`keys`/`vals`/print, `merge`/`assoc`/`dissoc`/`into` all
//! behave exactly like a real `{...}` map from script's point of view --
//! this is a read-mostly VIEW, not a new collection type scripts have to
//! know about).
//!
//! ```no_run
//! use std::sync::Arc;
//! use mova::embed::{Engine, Profile};
//! use mova::embed::host::{ShapeBuilder, wrap_struct};
//!
//! struct Player {
//!     name: String,
//!     hp: i64,
//! }
//!
//! let shape = ShapeBuilder::<Player>::new("Player")
//!     .field("name", |p| mova::embed::Value::from(p.name.as_str()))
//!     .field("hp", |p| mova::embed::Value::from(p.hp))
//!     .build();
//!
//! let mut engine = Engine::builder().profile(Profile::Pure).build();
//! let player = Arc::new(Player { name: "Rin".into(), hp: 30 });
//! engine.def("player", wrap_struct(player, &shape));
//! let hp = engine.eval("(:hp player)").unwrap();
//! assert_eq!(hp.as_i64(), Some(30));
//! ```
//!
//! ## v1 scope (honest limits)
//!
//! - **No derive macro.** [`ShapeBuilder`] is a runtime registration API --
//!   each `.field(name, getter)` call installs one boxed closure. A future
//!   `#[derive(HostStruct)]` (out of scope for this experiment, see
//!   LATENCY-CAMPAIGN.md §W3) could generate genuinely monomorphic
//!   accessors and shave the one remaining vtable-call indirection; v1
//!   ships the hand-registration path the design doc calls "required
//!   either way".
//! - **`assoc`/`dissoc` v1 = materialize + update, no overlay.** The first
//!   `assoc` on a `HostStruct` pays the same one-time materialize
//!   `crate::host_struct::as_pmap` already does for `=`/hashing, then
//!   updates the resulting plain `Map` -- the RESULT of `assoc` is always
//!   a real `Value::Map`, never another `HostStruct` (a script that
//!   mutates a host view stops getting the zero-copy fast path for that
//!   branch, which is the correct, conservative choice for v1: overlay
//!   semantics are deferred until a real workload asks for them).
#![allow(rustdoc::private_intra_doc_links)]

use std::any::Any;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::host_struct::{HostObj as InternalHostObj, HostStructInner, Shape as InternalShape, ShapeField};
use crate::value::{Str, Value as RawValue};

use super::Value;

/// A registered field/getter layout for host Rust type `T`, built once via
/// [`ShapeBuilder`] and shared (via one internal `Arc`) by every
/// [`wrap_struct`] call for that type -- `Clone` is a cheap `Arc::clone`,
/// not a rebuild.
pub struct Shape<T> {
    pub(crate) inner: Arc<InternalShape>,
    _marker: PhantomData<fn(&T)>,
}

impl<T> Clone for Shape<T> {
    fn clone(&self) -> Self {
        Shape {
            inner: Arc::clone(&self.inner),
            _marker: PhantomData,
        }
    }
}

/// Builds a [`Shape`] for host Rust type `T`: an ordered list of
/// script-visible fields (`(:field-name host-value)`/`(get host-value
/// :field-name)`), each with a getter that computes its `Value` from a
/// `&T`. Register a `Shape` ONCE per host type (typically at engine/
/// process startup); every [`wrap_struct`] call for an instance of `T`
/// shares it.
pub struct ShapeBuilder<T> {
    type_name: &'static str,
    fields: Vec<ShapeField>,
    _marker: PhantomData<fn(&T)>,
}

impl<T: Any + Send + Sync> ShapeBuilder<T> {
    /// Starts building a `Shape` for `T`. `type_name` is diagnostic-only
    /// (never script-visible -- not part of any printed/compared form).
    pub fn new(type_name: &'static str) -> Self {
        ShapeBuilder {
            type_name,
            fields: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// Registers one script-visible field named `name` (without a leading
    /// `:`, same convention every other mova keyword uses). `get` is
    /// called AT MOST ONCE per wrapped instance for this field (cached
    /// afterward in a per-instance leaf slab -- see
    /// `crate::host_struct::HostStructInner`'s doc -- so a repeated read
    /// of the same field, e.g. inside a hot loop, is allocation-free and
    /// `identical?`-stable after the first touch).
    pub fn field(mut self, name: &str, get: impl Fn(&T) -> Value + Send + Sync + 'static) -> Self {
        let key = Str::from(name);
        self.fields.push(ShapeField {
            key,
            get: Box::new(move |obj: &dyn InternalHostObj| {
                let t = obj.as_any().downcast_ref::<T>().expect(
                    "ShapeBuilder<T>::field getter invoked against a HostStruct not wrapping a T -- \
                     this would only happen if a Shape<T> were attached to the wrong wrap_struct call, \
                     which the type parameter on both makes impossible through this public API",
                );
                get(t).into_inner()
            }),
        });
        self
    }

    /// Finishes registration, returning a [`Shape`] ready to pass to
    /// [`wrap_struct`] for every instance of `T`.
    pub fn build(self) -> Shape<T> {
        Shape {
            inner: Arc::new(InternalShape::new(self.type_name, self.fields)),
            _marker: PhantomData,
        }
    }
}

/// Wraps `obj` as a script-visible `Value` (`Value::HostStruct`
/// internally) laid out by `shape`. Zero-copy: this is exactly one
/// `Arc::new` (the wrapper cell) -- `obj` itself is never cloned or
/// touched until a script actually reads one of its fields, and even then
/// only that field's getter runs (see `crate::host_struct`'s module doc
/// for the full "charge for what's touched" story). If `obj` is already
/// shared elsewhere (a host keeping its own `Arc<T>` around, e.g. a game
/// engine's live entity table), this is the cheapest possible boundary:
/// script and host both see the SAME allocation.
pub fn wrap_struct<T: Any + Send + Sync>(obj: Arc<T>, shape: &Shape<T>) -> Value {
    let dyn_obj: Arc<dyn InternalHostObj> = obj;
    let inner = HostStructInner::new(dyn_obj, Arc::clone(&shape.inner));
    Value::wrap(RawValue::HostStruct(Arc::new(inner)))
}

/// Typed extraction fast path: if `v` is still a `Value::HostStruct`
/// wrapping EXACTLY a `T` -- not widened into a plain `Map` by a script
/// `assoc`/`dissoc` (see this module's doc, "v1 scope"), and not some
/// OTHER host type's `HostStruct` -- clones the underlying `T` out
/// directly (a `TypeId` check + `T::clone`, ~30ns per the zerocopy-probe
/// measurements this design is built from) instead of paying
/// `serde_bridge::from_value`'s deserialize-from-`Map` cost. `None`
/// otherwise; this can never "misfire" onto the wrong type -- a
/// `Value::Map` (including one an `assoc` produced from a former
/// `HostStruct`) never reaches the `HostStruct` match arm at all, and a
/// `HostStruct` of a different host type fails the internal `downcast_ref`
/// -- both return `None`, never a wrongly-typed `T`.
pub fn from_value_typed<T: Any + Clone + Send + Sync>(v: &Value) -> Option<T> {
    match v.inner() {
        RawValue::HostStruct(hs) => crate::host_struct::downcast_ref::<T>(hs).cloned(),
        _ => None,
    }
}

/// Like [`from_value_typed`], but returns a shared `Arc<T>` handle to the
/// SAME host allocation instead of cloning `T` itself -- ~3.6ns (an
/// `Arc::clone` + `TypeId` check), the fastest extraction this boundary
/// offers, and the right choice whenever the host doesn't need an
/// independent owned copy. Same `None` cases as [`from_value_typed`].
pub fn from_value_arc<T: Any + Send + Sync>(v: &Value) -> Option<Arc<T>> {
    match v.inner() {
        RawValue::HostStruct(hs) => crate::host_struct::downcast_arc::<T>(hs),
        _ => None,
    }
}

// --- Derive plumbing (DESIGN-hoststruct-derive.md §2 Option A) -----------
//
// `MovaStruct`/`WrapExt`/`IntoMovaValue` below are the three items
// `#[derive(MovaStruct)]` (the `mova-derive` crate, gated behind this
// crate's `derive` feature) generates code against. They are unconditional
// -- not feature-gated -- because they're ordinary, hand-implementable
// public API: a host can implement `MovaStruct`/`WrapExt` BY HAND (it's
// exactly "build a `Shape` once via `ShapeBuilder`, cache it in a
// `OnceLock`", the same discipline this module's own doc example already
// follows), with or without ever enabling `derive`. Only the derive MACRO
// itself (re-exported below) is feature-gated, matching serde/serde_derive's
// convention.

/// Implemented by a host Rust type that has a registered [`Shape`].
/// `#[derive(MovaStruct)]` (the `derive` feature, see the `mova-derive`
/// crate's docs) generates this automatically; it's also safe and
/// straightforward to implement by hand -- see this module's own doc
/// example (`ShapeBuilder`/`OnceLock`) for the exact shape a hand-written
/// impl takes.
///
/// [`MovaStruct::shape`] is monomorphization-scoped: for a generic type
/// `Cache<V>`, each concrete `Cache<V>` gets its own independently-built,
/// process-wide-cached `Shape` -- never one `Shape` shared across every
/// `Cache<_>` instantiation. (`#[derive(MovaStruct)]` itself rejects
/// generic type parameters for v1 -- see `mova-derive`'s crate docs -- so
/// this note applies only to a hand-written `impl<V: ...> MovaStruct for
/// Cache<V>`.)
pub trait MovaStruct: Sized {
    /// Returns the process-wide `Shape` for `Self`, building it on first
    /// call (typically via a private `OnceLock`, see [`ShapeBuilder`]) and
    /// returning the same cached `&'static Shape` on every call after.
    fn shape() -> &'static Shape<Self>;
}

/// Sugar so a host writes `arc.wrap()` instead of `wrap_struct(arc,
/// T::shape())`. `#[derive(MovaStruct)]` (the `derive` feature) generates
/// this automatically alongside [`MovaStruct`]; hand-implementable too.
pub trait WrapExt {
    /// Wraps `self` as a script-visible [`Value`]. Equivalent to
    /// `wrap_struct(self, Self::shape())` for a `Self: MovaStruct`.
    fn wrap(self: Arc<Self>) -> Value;
}

/// Helper trait behind `#[derive(MovaStruct)]`'s `#[mova(getter = "...")]`
/// field codegen: converts a getter method's return value into a
/// [`Value`]. Blanket-implemented for every type [`Value`] already has a
/// `From` impl for (`i64`, `f64`, `bool`, `&str`, `String`, `()`,
/// `Vec<Value>` -- see `crate::embed::value`'s `impl From<...>` block, the
/// single source of truth this blanket impl reads off of; nothing is
/// listed here twice, and if that `From` surface ever grows, this trait
/// covers the addition automatically).
///
/// ## Why this exists
///
/// A direct struct FIELD's type is a bare syntax token `#[derive(
/// MovaStruct)]` can read at macro-expansion time (literally `i64`/`f64`/
/// `bool`/`String`), so an unsupported field type is caught with a
/// hand-written `compile_error!` pointing at the field. A `#[mova(getter =
/// "...")]` method's RETURN type is NOT visible to the macro at all --
/// only the method's NAME is, as a string -- so there's no syntax to
/// pattern-match against; the macro instead emits a call through this
/// trait and lets the compiler's own trait resolution decide. The
/// `#[diagnostic::on_unimplemented]` attribute below exists so that
/// failure is still readable: without it, an unsupported getter return
/// type would surface as "the trait bound `Value: From<Foo>` is not
/// satisfied" pointing at generated code the user never wrote, instead of
/// a message naming `Foo` and the fix directly.
#[diagnostic::on_unimplemented(
    message = "`#[mova(getter = \"...\")]` method returns `{Self}`, which has no `embed::Value` conversion",
    label = "no `embed::Value` conversion for `{Self}`",
    note = "supported getter return types: `i64`, `f64`, `bool`, `&str`, `String`, `()`, \
            `Vec<embed::Value>` -- narrow the getter's return type, or convert inside the \
            getter method body"
)]
pub trait IntoMovaValue {
    /// Converts `self` into a [`Value`].
    fn into_mova_value(self) -> Value;
}

impl<T> IntoMovaValue for T
where
    Value: From<T>,
{
    fn into_mova_value(self) -> Value {
        Value::from(self)
    }
}

/// `#[derive(MovaStruct)]`: generates a [`MovaStruct`]/[`WrapExt`] impl
/// pair for a named-field struct. See the `mova-derive` crate's docs for
/// the full attribute surface (`#[mova(rename/skip/getter/crate)]`) and
/// error-UX contract. Gated behind the `derive` feature (off by default),
/// mirroring serde/serde_derive's convention: `mova = { version = "...",
/// features = ["derive"] }`, `use mova::embed::host::MovaStruct;` (one
/// `use` path brings in both the trait and this macro -- see this
/// module's `MovaStruct` trait doc for why that's not a naming conflict).
#[cfg(feature = "derive")]
pub use mova_derive::MovaStruct;
