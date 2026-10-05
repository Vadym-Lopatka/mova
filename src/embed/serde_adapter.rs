//! Facade-level serde adapter: thin wrappers over `crate::serde_bridge`'s
//! `to_value`/`from_value` that speak `embed::Value`/`embed::Error` instead
//! of the crate-internal `value::Value`/`serde_bridge::SerdeError` -- so a
//! host using `mova::embed` doesn't have to reach past the facade to move
//! a `#[derive(Serialize)]`/`#[derive(Deserialize)]` type in and out of a
//! script. Behind the same `serde` feature (default off) as
//! `crate::serde_bridge` itself; see that module's doc for the full
//! encoding contract (struct fields -> verbatim-keywordized map keys,
//! `Option` -> nil/value, u64/i128 overflow errors, NaN bit-exactness,
//! etc.) -- this module changes none of it, only the types at the boundary.

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::{Error, Value};

/// Serializes `value` into an [`Value`] the same way `serde_json::to_value`
/// would produce a `serde_json::Value` -- see `crate::serde_bridge`'s
/// module doc for the exact encoding (structs become `Map`s with keyword
/// keys, etc). The returned `Value` can be `def`'d into an [`Engine`](super::Engine)
/// or handed to [`Engine::call`](super::Engine::call) like any other value.
///
/// Errors (e.g. a `u64`/`u128` field that doesn't fit `i64`) come back as an
/// [`Error`] built from the original `SerdeError`'s message text via
/// `Error::other` -- the text is preserved verbatim, just no longer tied to
/// the internal `serde_bridge::SerdeError` type.
pub fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, Error> {
    crate::serde_bridge::to_value(value)
        .map(Value::wrap)
        .map_err(|e| Error::other(e.to_string()))
}

/// Deserializes an [`Value`] (typically one a script produced, or built
/// with [`Value::map`]/[`Value::vector`]) into a Rust `T`. See
/// `crate::serde_bridge`'s module doc for the lenient struct-field
/// convention (`Keyword` OR `Str` map keys both match a field name) and
/// what happens with unknown keys (silently ignored, standard serde
/// `#[derive(Deserialize)]` behavior).
pub fn from_value<T: DeserializeOwned>(value: &Value) -> Result<T, Error> {
    crate::serde_bridge::from_value(value.inner()).map_err(|e| Error::other(e.to_string()))
}
