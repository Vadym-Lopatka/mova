//! Embeddable-crate facade: a minimal, host-friendly surface over the mova
//! interpreter for callers who want to eval script text and shuttle values
//! back and forth without depending on `crate::value`/`crate::eval`/
//! `crate::error` directly. Everything a host needs is re-exported here:
//! [`Engine`](crate::embed::Engine)/[`EngineBuilder`](crate::embed::EngineBuilder)/
//! [`Profile`](crate::embed::Profile), the opaque [`Value`](crate::embed::Value)/
//! [`ValueKind`](crate::embed::ValueKind), [`Error`](crate::embed::Error),
//! and -- for the native→script direction -- [`Arity`](crate::embed::Arity)/
//! [`Reentry`](crate::embed::Reentry), the handle a
//! [`Engine::register_fn_with_reentry`](crate::embed::Engine::register_fn_with_reentry)
//! native uses to call back into script mid-eval.
//!
//! ```no_run
//! use mova::embed::{Engine, Profile};
//!
//! let mut engine = Engine::builder().profile(Profile::Pure).build();
//! let v = engine.eval("(+ 1 2 3)").unwrap();
//! assert_eq!(v.as_i64(), Some(6));
//! ```
//!
//! ## `Profile::Pure` vs `Profile::Scripting`
//!
//! `register_all` (`crate::builtins`) is split into capability groups
//! (`register_core`/`register_sys`/`register_conc`/`register_flow`, see
//! that module). `Profile::Pure` registers `register_core` ONLY --
//! numbers/collections/seqs/strings/predicates/atoms/reflection/regex, none
//! of which can reach outside the process. `Profile::Scripting` is every
//! group, byte-for-byte the same builtin surface `Interp::new()`/`mova`'s
//! own CLI has.
//!
//! The subtlety worth knowing if you touch this: it is NOT enough to just
//! skip registering the excluded natives and bootstrap `core/core.mova` as
//! normal. `core/async.mova` and `core/flow.mova` each `def` a bare name
//! straight from a native in the group they document -- `core/async.mova`'s
//! `(def >! >!!)`, `core/flow.mova`'s `(def map->step flow/map->step*)` --
//! at TOP-LEVEL bootstrap time, unquoted. Loading either file without its
//! native group registered isn't a graceful "that symbol will error if you
//! call it" the way an ordinary `(future 1)` in a `Pure` engine is (the
//! `future` macro is defined in `core/core.mova` itself and only expands to
//! a call on the missing `future*` native, so it type-checks as "core
//! loaded fine, THIS call fails at the unresolved symbol" -- exactly the
//! behavior `Profile::Pure` wants); it is a hard bootstrap panic. So
//! `Interp::with_capabilities` skips `core/async.mova`/`core/flow.mova`
//! ENTIRELY for `Profile::Pure`, not just the native registration -- see
//! that fn's doc in `crate::eval`.
//!
//! This module (and everything under it) denies `missing_docs`: `embed` is
//! mova's one documented public surface (alongside `serde_bridge` behind
//! the `serde` feature -- see `lib.rs`'s `internal` module doc for what
//! everything else is), so every public item here carries a real doc
//! comment, checked by the compiler rather than by convention.
#![deny(missing_docs)]

mod engine;
mod error;
/// The `Value::HostStruct` zero-copy embedding boundary's public surface:
/// [`host::ShapeBuilder`]/[`host::Shape`]/[`host::wrap_struct`]/
/// [`host::from_value_typed`]/[`host::from_value_arc`]. See that module's
/// doc for the full contract and a worked example.
pub mod host;
#[cfg(feature = "serde")]
mod serde_adapter;
/// L5/W4: deterministic simulation from the host side — [`sim::enable`],
/// [`sim::SimOpts`]. See that module's doc for why sim is a process-scoped
/// free function rather than an `EngineBuilder` option.
pub mod sim;
mod value;

pub(crate) use engine::is_script_bound_var;
pub use engine::{Arity, Engine, EngineBuilder, Profile, Reentry, ShutdownReport};
pub use error::Error;
#[cfg(feature = "serde")]
pub use serde_adapter::{from_value, to_value};
pub use value::{Value, ValueKind};
