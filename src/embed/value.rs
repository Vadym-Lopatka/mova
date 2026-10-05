//! The embeddable facade's opaque value type. Wraps `crate::value::Value`
//! rather than re-exporting it, so a host crate's dependency on
//! `mova::embed` never has to see (or break against) the internal `Value`
//! enum's representation -- see `crate::embed`'s module doc.

use crate::value::Value as Raw;

use super::Error;

/// An mova value, opaque to the host. Read it through [`Value::kind`] and
/// the `as_*`/[`Value::iter`]/[`Value::entries`]/[`Value::get`]/
/// [`Value::get_kw`] accessors, or convert it with the `From`/`TryFrom`
/// impls below.
pub struct Value(pub(crate) Raw);

impl Value {
    pub(crate) fn wrap(raw: Raw) -> Self {
        Value(raw)
    }

    pub(crate) fn inner(&self) -> &Raw {
        &self.0
    }

    pub(crate) fn into_inner(self) -> Raw {
        self.0
    }

    /// The coarse-grained shape of this value. `#[non_exhaustive]` on
    /// [`ValueKind`] so a later mova version can grow a new `Value`
    /// variant without that being a breaking change for embedders matching
    /// on this enum.
    pub fn kind(&self) -> ValueKind {
        match &self.0 {
            Raw::Nil => ValueKind::Nil,
            Raw::Bool(_) => ValueKind::Bool,
            Raw::Int(_) => ValueKind::Int,
            Raw::Float(_) => ValueKind::Float,
            Raw::Str(_) => ValueKind::Str,
            Raw::Keyword(_) => ValueKind::Keyword,
            Raw::Sym(_) => ValueKind::Symbol,
            Raw::List(_) => ValueKind::List,
            Raw::Vector(_) => ValueKind::Vector,
            Raw::Map(_) => ValueKind::Map,
            // W3: script-indistinguishable from a real map (conformance-
            // by-construction, see `crate::host_struct`'s doc) -- reports
            // the same `ValueKind` so a host `match`ing on `kind()` never
            // has to special-case a `wrap_struct`'d value.
            Raw::HostStruct(_) | Raw::LazyMap(_) => ValueKind::Map,
            Raw::Set(_) => ValueKind::Set,
            // `Macro`/`Var` are deliberately NOT folded in here: a macro
            // value isn't meaningfully callable via `Engine::call` (macros
            // expand at read/compile time, not at application time), and a
            // `Var` is a storage cell whose CONTENTS may or may not be a
            // fn -- `Fn` here means "you can hand this straight to
            // `Engine::call`", which only `Fn`/`Native` truthfully are.
            Raw::Fn(_) | Raw::Native(_) => ValueKind::Fn,
            _ => ValueKind::Other,
        }
    }

    /// `Some` only for an `Int`-kinded value; `None` for anything else
    /// (including a `Float` -- see [`Value::as_f64`] for the widening
    /// accessor).
    pub fn as_i64(&self) -> Option<i64> {
        match &self.0 {
            Raw::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Widens an `Int` too (as Clojure's own numeric tower would), so a host
    /// reading a script result as `f64` doesn't have to special-case an
    /// integer literal.
    pub fn as_f64(&self) -> Option<f64> {
        match &self.0 {
            Raw::Float(x) => Some(*x),
            Raw::Int(n) => Some(*n as f64),
            _ => None,
        }
    }

    /// `Some` only for a `Bool`-kinded value; `None` for anything else
    /// (including truthy/falsy collections -- mova truthiness isn't
    /// exposed through this accessor, only a literal boolean).
    pub fn as_bool(&self) -> Option<bool> {
        match &self.0 {
            Raw::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// `Some` only for a `Str`-kinded value; `None` for anything else
    /// (including a `Keyword`/`Symbol`, which don't share `Str`'s kind --
    /// see [`Value::as_keyword`], the mirror-image accessor: a `Keyword`
    /// never answers `as_str`, and a `Str` never answers `as_keyword`).
    pub fn as_str(&self) -> Option<&str> {
        match &self.0 {
            Raw::Str(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    /// `Some` (the bare name, WITHOUT the leading `:` -- same convention
    /// [`Value::keyword`] takes on construction) only for a `Keyword`-kinded
    /// value; `None` for anything else, including a `Str`/`Symbol` (see
    /// [`Value::as_str`]'s doc, which cross-links back here). Closes a real
    /// dogfooding paper cut: before this, a host reading a keyword result
    /// had to `Display`-format the value and strip a leading `":"` off the
    /// `pr_str`-rendered text by hand.
    pub fn as_keyword(&self) -> Option<&str> {
        match &self.0 {
            Raw::Keyword(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    /// Element count for a collection or string; `0` for nil and every
    /// scalar. (Not `Option<usize>` -- unlike the `as_*` accessors, "how
    /// long is this" has an unambiguous answer of zero for anything that
    /// isn't a collection, rather than a meaningful absence.)
    pub fn len(&self) -> usize {
        match &self.0 {
            Raw::Nil => 0,
            Raw::List(v) | Raw::Vector(v) => v.len(),
            Raw::Map(m) => m.len(),
            // O(1): the shape's field count, no materialize -- see
            // `crate::host_struct::count`'s doc.
            Raw::HostStruct(hs) => crate::host_struct::count(hs),
            Raw::LazyMap(lm) => crate::lazy_map::count(lm),
            Raw::Set(s) => s.len(),
            Raw::Str(s) => s.char_count_cached(),
            _ => 0,
        }
    }

    /// `true` iff [`Value::len`] is `0` (nil, a scalar, or a genuinely
    /// empty collection/string all count).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterates a list/vector/set's elements; empty for anything else
    /// (including a map -- see [`Value::entries`]).
    pub fn iter(&self) -> Box<dyn Iterator<Item = Value> + '_> {
        match &self.0 {
            Raw::List(v) | Raw::Vector(v) => Box::new(v.iter_cloned().map(Value)),
            Raw::Set(s) => Box::new(s.iter().cloned().map(Value)),
            _ => Box::new(std::iter::empty()),
        }
    }

    /// Iterates a map's key/value pairs; empty for anything else.
    pub fn entries(&self) -> Box<dyn Iterator<Item = (Value, Value)> + '_> {
        match &self.0 {
            Raw::Map(m) => Box::new(m.iter().map(|(k, v)| (Value(k.clone()), Value(v.clone())))),
            // Shape order (not `as_pmap`'s materialize path) -- same
            // touch-only policy as `keys`/`vals`/print, see
            // `crate::host_struct`'s module doc.
            Raw::HostStruct(hs) => Box::new((0..hs.shape.fields.len()).map(move |idx| {
                let key = Raw::Keyword(crate::keyword::Keyword::from(&hs.shape.fields[idx].key));
                (Value(key), Value(crate::host_struct::get_field(hs, idx)))
            })),
            Raw::LazyMap(lm) => Box::new(
                crate::lazy_map::as_pmap(lm)
                    .iter()
                    .map(|(k, v)| (Value(k.clone()), Value(v.clone()))),
            ),
            _ => Box::new(std::iter::empty()),
        }
    }

    /// Clojure `get` semantics, delegating to whatever internal lookup the
    /// matching collection already uses (never reimplemented here):
    ///
    /// - `Map`: the value for `key` under mova's own `Value` equality
    ///   ([`crate::value::PMap::get`]), `None` if absent.
    /// - `Vector`/`List`: `key` must be an `Int` inside `[0, len)` --
    ///   negative, out-of-range, or non-`Int` all answer `None` (Clojure's
    ///   own `get` on a sequential collection never panics the way `nth`
    ///   does on an out-of-bounds index).
    /// - `HostStruct`: `key` must be a `Keyword` naming one of the value's
    ///   shape fields (`crate::host_struct::lookup`, the SAME shape-
    ///   dispatch [`Value::entries`] above and script's own `(:kw h)` both
    ///   go through) -- any other key kind, or a keyword naming no field,
    ///   is `None`.
    /// - `Set`: membership, Clojure's "a set used as a fn returns the found
    ///   element" convention -- `Some(key.clone())` if `key` is a member
    ///   (there's nothing else FOR it to return: `champ::
    ///   PersistentHashSet` only exposes `contains`, not the stored
    ///   element, but mova `Value` equality means the query and the stored
    ///   element are indistinguishable anyway), `None` otherwise.
    /// - Everything else (`nil`, every scalar): `None`.
    pub fn get(&self, key: &Value) -> Option<Value> {
        match &self.0 {
            Raw::Map(m) => m.get(&key.0).cloned().map(Value),
            Raw::List(v) | Raw::Vector(v) => match &key.0 {
                Raw::Int(n) if *n >= 0 => v.get_owned(*n as usize).map(Value),
                _ => None,
            },
            Raw::HostStruct(hs) => match &key.0 {
                Raw::Keyword(kw) => crate::host_struct::lookup(hs, kw.text_ref()).map(Value),
                _ => None,
            },
            Raw::LazyMap(lm) => crate::lazy_map::get(lm, &key.0).map(Value),
            Raw::Set(s) => {
                if s.contains(&key.0) {
                    Some(key.clone())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Convenience for the overwhelmingly common case of [`Value::get`]'s
    /// callers -- looking up a keyword field/key by its bare name --
    /// equivalent to `self.get(&Value::keyword(name))`. Hosts read
    /// keyword-keyed map/`HostStruct` fields constantly; this skips
    /// building the intermediate `Value::keyword` at every call site.
    pub fn get_kw(&self, name: &str) -> Option<Value> {
        self.get(&Value::keyword(name))
    }
}

impl Value {
    /// Builds a keyword value (mova's `Str` stores it WITHOUT the leading
    /// `:`, same convention `crate::value::Value::Keyword` uses -- pass the
    /// bare name, e.g. `Value::keyword("name")` for script-visible `:name`).
    /// Not in the original accessor/conversion list, but needed for a host
    /// to hand back Clojure-map-shaped data (map keys are almost always
    /// keywords) -- see [`Value::map`] and `crate::embed`'s module doc for
    /// where this earns its place.
    pub fn keyword(name: &str) -> Value {
        Value(Raw::Keyword(crate::keyword::Keyword::construct(name)))
    }

    /// Builds a map value from key/value pairs -- the counterpart to
    /// `From<Vec<Value>>`'s vector for the other collection shape a host
    /// commonly needs to construct (e.g. a `register_fn` closure returning
    /// structured data to script).
    pub fn map(pairs: impl IntoIterator<Item = (Value, Value)>) -> Value {
        let raw: crate::value::PMap = pairs
            .into_iter()
            .map(|(k, v)| (k.into_inner(), v.into_inner()))
            .collect();
        Value(Raw::Map(raw))
    }

    /// Builds a vector value from an iterator of elements -- the third
    /// primitive constructor alongside [`Value::keyword`]/[`Value::map`],
    /// for a host that wants to build vector-shaped script data without
    /// routing through `Vec<Value>` and the `From<Vec<Value>>` impl (e.g.
    /// building straight off an iterator/generator without collecting an
    /// intermediate `Vec` first). Equivalent to `Value::from(vs.into_iter().
    /// collect::<Vec<_>>())`, just without the intermediate allocation's
    /// name showing up at call sites.
    pub fn vector(vs: impl IntoIterator<Item = Value>) -> Value {
        let raw: crate::value::PVec = vs.into_iter().map(Value::into_inner).collect();
        Value(Raw::Vector(raw))
    }
}

impl Clone for Value {
    fn clone(&self) -> Self {
        Value(self.0.clone())
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Value").field(&crate::printer::pr_str(&self.0)).finish()
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", crate::printer::pr_str(&self.0))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

/// Coarse-grained shape of an [`Value`]. `#[non_exhaustive]`: matching on
/// this must always carry a wildcard arm, since a future mova version can
/// add a `Value` variant that folds into a NEW kind rather than an
/// existing one.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// `nil`.
    Nil,
    /// `true`/`false`.
    Bool,
    /// A 64-bit integer.
    Int,
    /// A 64-bit float.
    Float,
    /// A string.
    Str,
    /// A keyword (`:name`).
    Keyword,
    /// A symbol (`name`, unevaluated).
    Symbol,
    /// A list (`(...)`).
    List,
    /// A vector (`[...]`).
    Vector,
    /// A map (`{...}`).
    Map,
    /// A set (`#{...}`).
    Set,
    /// A fn or native -- callable via [`crate::embed::Engine::call`].
    Fn,
    /// Everything else: chars, atoms, futures/promises/delays, channels,
    /// flows, regexes, macros, vars, ...
    Other,
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value(Raw::Int(n))
    }
}

impl From<f64> for Value {
    fn from(x: f64) -> Self {
        Value(Raw::Float(x))
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value(Raw::Bool(b))
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value(Raw::Str(crate::value::Str::from(s)))
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value(Raw::Str(crate::value::Str::from(s)))
    }
}

impl From<()> for Value {
    fn from(_: ()) -> Self {
        Value(Raw::Nil)
    }
}

/// Builds a `Vector` (not a `List`) from `vs` -- the collection literal a
/// host constructing "some data to hand the script" almost always means,
/// and the one real Clojure's own `(vec ...)` would produce from a
/// `Vec`-shaped source.
impl From<Vec<Value>> for Value {
    fn from(vs: Vec<Value>) -> Self {
        let raw: crate::value::PVec = vs.into_iter().map(Value::into_inner).collect();
        Value(Raw::Vector(raw))
    }
}

impl TryFrom<&Value> for i64 {
    type Error = Error;
    fn try_from(v: &Value) -> Result<Self, Error> {
        v.as_i64()
            .ok_or_else(|| Error::other(format!("expected an int, got a {}", v.0.type_name())))
    }
}

impl TryFrom<&Value> for f64 {
    type Error = Error;
    fn try_from(v: &Value) -> Result<Self, Error> {
        v.as_f64()
            .ok_or_else(|| Error::other(format!("expected a number, got a {}", v.0.type_name())))
    }
}

impl TryFrom<&Value> for bool {
    type Error = Error;
    fn try_from(v: &Value) -> Result<Self, Error> {
        v.as_bool()
            .ok_or_else(|| Error::other(format!("expected a bool, got a {}", v.0.type_name())))
    }
}

impl TryFrom<&Value> for String {
    type Error = Error;
    fn try_from(v: &Value) -> Result<Self, Error> {
        v.as_str()
            .map(String::from)
            .ok_or_else(|| Error::other(format!("expected a string, got a {}", v.0.type_name())))
    }
}
