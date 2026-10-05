//! `#[derive(MovaStruct)]` -- the derive macro for mova/mova's
//! `Value::HostStruct` zero-copy embedding boundary. Implements
//! `DESIGN-hoststruct-derive.md`'s §1/§2 Option A/§4/§7 "ship in v1" scope
//! exactly: pure codegen over `mova::embed::host`'s PUBLIC
//! `ShapeBuilder<T>`/`Shape<T>`/`wrap_struct` API. This crate never touches
//! `mova`'s `pub(crate)`/`#[doc(hidden)]` internals -- see the design
//! doc's §2 for why that's a hard requirement (the alternative, "Option
//! B", would version-lock this crate to `mova`'s internals; Option A
//! stays on mova's ordinary public-semver contract instead).
//!
//! # Usage
//!
//! ```ignore
//! use mova::embed::host::WrapExt; // the `wrap()` sugar
//! use mova::MovaStruct;
//!
//! #[derive(Clone, MovaStruct)]
//! struct Player {
//!     name: String,
//!     hp: i64,
//!     #[mova(rename = "xp")]
//!     experience: i64,
//!     #[mova(skip)]
//!     internal_seed: u64,
//!     #[mova(getter = "full_label")]
//!     label: (),
//! }
//!
//! impl Player {
//!     fn full_label(&self) -> String {
//!         format!("{}#{}", self.name, self.hp)
//!     }
//! }
//!
//! let player = std::sync::Arc::new(Player {
//!     name: "Rin".into(), hp: 30, experience: 12, internal_seed: 0,
//! });
//! engine.def("player", player.wrap());
//! ```
//!
//! # Attributes
//!
//! - `#[mova(rename = "name")]` (field-level) -- the script-visible key is
//!   `"name"` instead of the Rust field identifier. No leading `:` (same
//!   convention every `mova`/`mova` keyword uses). Default (no
//!   attribute): the field's Rust identifier VERBATIM, including
//!   underscores -- deliberately matching `serde_bridge`'s convention
//!   rather than kebab-casing, so a struct deriving both `Serialize` and
//!   `MovaStruct` presents the SAME keys through both boundaries.
//! - `#[mova(skip)]` (field-level) -- the field is never registered: no
//!   `.field(...)` call is emitted, the field is invisible to script, and
//!   its Rust type is never type-checked by this macro (it can be
//!   anything -- no `Value` mapping is ever attempted).
//! - `#[mova(getter = "method_name")]` (field-level) -- the emitted
//!   closure calls `self.method_name()` and converts ITS return value,
//!   instead of reading a same-named field directly. The field the
//!   attribute sits on can be `()` or any placeholder type; this macro
//!   never reads it.
//! - `#[mova(crate = "path")]` (container-level, i.e. on the `struct`
//!   itself) -- overrides the crate path the generated code assumes (see
//!   "Crate-path hygiene" below). Rare: only needed by a re-exporting
//!   crate or a workspace that renames its `mova`/`mova` dependency.
//!
//! `#[mova(skip)]` and `#[mova(getter = "...")]` on the SAME field is a
//! hard error (they disagree about whether the field is read at all).
//!
//! # Supported field types (direct fields, no `#[mova(getter)]`)
//!
//! `i64`, `f64`, `bool`, `String` -- exactly the scalar+string subset of
//! `mova::embed::Value`'s `From` surface that a struct field can hold
//! without a getter method doing a conversion first (`&str`/`()`/
//! `Vec<Value>` all need either borrowed data with an explicit lifetime,
//! which `#[derive(MovaStruct)]` v1 rejects outright -- see "Adversarial
//! cases" below -- or aren't meaningful STORED field types). Any other
//! field type without `#[mova(skip)]` or `#[mova(getter = "...")]` is a
//! **hard compile error**, spanned at the offending field, never a
//! silent truncation/miscompile.
//!
//! # Error-UX strategy (read this if a field won't compile)
//!
//! This macro deliberately uses TWO different error mechanisms, chosen by
//! how much the macro can actually see at expansion time:
//!
//! 1. **Direct fields: pure-syntactic dispatch + hand-written
//!    `compile_error!`.** A field's TYPE is a bare syntax token the macro
//!    reads directly off the struct definition -- so it can pattern-match
//!    the type's last path segment against exactly `i64`/`f64`/`bool`/
//!    `String` (a **literal, syntactic** match: a type ALIAS to `i64`,
//!    e.g. `type Score = i64; score: Score`, will NOT match this
//!    syntactic check -- the field's path segment is literally `Score`,
//!    not `i64` -- and falls straight to the "no match" arm). Any field
//!    whose type doesn't syntactically match one of those four gets a
//!    hand-written `compile_error!` pointing at the field, with a message
//!    naming the field, its type, and all three fixes (`#[mova(skip)]`,
//!    `#[mova(getter = "...")]`, or narrowing the field's type). This is
//!    the ONLY mechanism used for direct fields -- there is no
//!    trait-resolution fallback for a type-aliased or newtype field; see
//!    "Why not trait-driven for direct fields too?" below.
//! 2. **`#[mova(getter = "...")]` fields: trait-driven, via
//!    `mova::embed::host::IntoMovaValue` + `#[diagnostic::
//!    on_unimplemented]`.** A getter METHOD's return type is not visible
//!    to this macro at all -- only the method's NAME is (as a string
//!    literal in the attribute) -- so there is no syntax to pattern-match
//!    against. The macro emits `IntoMovaValue::into_mova_value(self.
//!    method_name())` and lets the compiler's own trait resolution decide
//!    whether the return type converts; `IntoMovaValue` is blanket-
//!    implemented for every type `mova::embed::Value` already has a
//!    `From` impl for, and carries a `#[diagnostic::on_unimplemented]`
//!    attribute so an unsupported getter return type produces a message
//!    naming the offending TYPE and the supported list, instead of
//!    rustc's generic "the trait bound `Value: From<Foo>` is not
//!    satisfied" pointing at generated code the user never wrote.
//!
//! **Why not trait-driven for direct fields too?** Because a direct
//! field's ACCESS PATTERN differs by type in a way trait resolution alone
//! can't paper over cleanly: a `Copy` scalar (`i64`/`f64`/`bool`) is read
//! as `self.field` (a copy out of `&self`), while `String` is read as
//! `self.field.as_str()` (a borrow, avoiding a second allocation from
//! cloning the field just to convert it back to `mova`'s own `Str`) --
//! the macro has to already know WHICH of those two shapes applies before
//! it can emit anything, which means it has already done the syntactic
//! type check by the time a trait call could even be written. Given that,
//! `compile_error!` at that same syntactic decision point is strictly
//! better error UX than deferring to a trait-bound failure the compiler
//! would report against generated code. **This is a deliberate, coherent
//! split, not an inconsistency**: syntactic-with-`compile_error!` where
//! the macro has the type in hand, trait-driven-with-`on_unimplemented`
//! where it structurally cannot.
//!
//! # Crate-path hygiene
//!
//! Generated code defaults to `::mova::...` paths (the runtime crate's
//! name TODAY). `#[mova(crate = "path")]` on the struct overrides this --
//! e.g. `#[mova(crate = "my_reexport::mova")]`. When the runtime crate
//! renames to `mova` at the planned 0.6 boundary (`EMBED-API-PLAN.md`
//! Phase G), flipping the DEFAULT is a one-line change to this crate's
//! `DEFAULT_CRATE_PATH` constant (`src/lib.rs`) -- search for it, it's the
//! single marked source of truth, nothing else in this crate hard-codes
//! the name `mova`.
//!
//! # Adversarial cases (v1 scope cut, `DESIGN-hoststruct-derive.md` §6/§7)
//!
//! - **Generic type/const parameters are REJECTED outright.**
//!   `ShapeBuilder<T>` requires `T: Any + Send + Sync` (so `T: 'static`);
//!   a struct with an unresolved generic parameter can't satisfy that
//!   without knowing every instantiation the derive will ever see. v1
//!   does not attempt "accepted if `'static`" inference -- ANY generic
//!   type or const parameter on the struct is a hard error. (Note for
//!   anyone who DOES hand-write a generic `MovaStruct` impl outside the
//!   derive: `T::shape()`'s `OnceLock` is monomorphization-scoped -- a
//!   generic `Cache<V>` gets one independently-built `Shape` PER concrete
//!   `V`, never one `Shape` shared across every `Cache<_>`. This can't be
//!   asserted by a runnable test once generics are rejected at the derive
//!   level; it's a doc note for hand-rolled impls only.)
//! - **Non-`'static` lifetime parameters are REJECTED**, with a message
//!   naming the offending lifetime (`MovaStruct requires T: 'static
//!   (found lifetime parameter 'a)`) rather than surfacing `ShapeBuilder`'s
//!   generic `T: Any` trait-bound error against code the user never wrote.
//! - **Tuple structs, unit structs, enums, and unions are REJECTED.** Only
//!   a named-field struct (`struct S { a: T, ... }`) is supported in v1 --
//!   see the design doc §7's deferred list for why (an enum's script
//!   representation isn't designed; a tuple/unit struct has no natural
//!   field-name source).
#![allow(clippy::type_complexity)]

use proc_macro::TokenStream;
use proc_macro2::{Span as Span2, TokenStream as TokenStream2};
use quote::{quote, quote_spanned, ToTokens};
use syn::spanned::Spanned;
use syn::{parse_macro_input, Data, DeriveInput, Fields, GenericParam, LitStr, Type};

/// The runtime crate's path TODAY (`mova`). This is the single,
/// clearly-marked place this crate hard-codes that name -- flip this one
/// constant at the planned `mova` -> `mova` rename (`EMBED-API-PLAN.md`
/// Phase G) and every derive-generated call site follows, with no other
/// source change required. `#[mova(crate = "...")]` overrides this
/// per-invocation without needing a rebuild of `mova-derive` itself.
const DEFAULT_CRATE_PATH: &str = "mova";

/// `#[derive(MovaStruct)]`. See the crate-level docs for the full
/// attribute/error-UX contract.
#[proc_macro_derive(MovaStruct, attributes(mova))]
pub fn derive_mova_struct(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input).unwrap_or_else(|err| err.to_compile_error()).into()
}

/// Parsed `#[mova(...)]` field attributes.
#[derive(Default)]
struct FieldAttrs {
    rename: Option<LitStr>,
    skip: bool,
    getter: Option<syn::Ident>,
}

fn parse_field_attrs(field: &syn::Field) -> syn::Result<FieldAttrs> {
    let mut out = FieldAttrs::default();
    for attr in &field.attrs {
        if !attr.path().is_ident("mova") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                out.rename = Some(lit);
                Ok(())
            } else if meta.path.is_ident("skip") {
                out.skip = true;
                Ok(())
            } else if meta.path.is_ident("getter") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                let ident = syn::parse_str::<syn::Ident>(&lit.value()).map_err(|_| {
                    syn::Error::new(
                        lit.span(),
                        format!(
                            "`#[mova(getter = \"{}\")]` is not a valid method name",
                            lit.value()
                        ),
                    )
                })?;
                out.getter = Some(ident);
                Ok(())
            } else {
                Err(meta.error(
                    "unknown `#[mova(...)]` field attribute key -- expected one of \
                     `rename`, `skip`, `getter`",
                ))
            }
        })?;
    }
    if out.skip && out.getter.is_some() {
        return Err(syn::Error::new(
            field.span(),
            "`#[mova(skip)]` and `#[mova(getter = \"...\")]` conflict on the same field -- \
             `skip` means this field is never read at all, `getter` means it's read via a \
             method call; pick one",
        ));
    }
    Ok(out)
}

/// Parsed `#[mova(...)]` container (struct-level) attributes.
#[derive(Default)]
struct ContainerAttrs {
    crate_path: Option<LitStr>,
}

fn parse_container_attrs(input: &DeriveInput) -> syn::Result<ContainerAttrs> {
    let mut out = ContainerAttrs::default();
    for attr in &input.attrs {
        if !attr.path().is_ident("mova") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                out.crate_path = Some(lit);
                Ok(())
            } else {
                Err(meta.error(
                    "unknown `#[mova(...)]` container attribute key -- expected `crate` \
                     (the only struct-level `#[mova(...)]` attribute; `rename`/`skip`/`getter` \
                     are field-level only)",
                ))
            }
        })?;
    }
    Ok(out)
}

/// Recognizes a bare, syntactically-literal path type (`i64`, `f64`,
/// `bool`, `String`) by its LAST path segment -- see the crate docs'
/// "Error-UX strategy" section for why this is a literal syntactic check,
/// not a trait-resolution fallback (a type alias to one of these names
/// will NOT match).
fn literal_type_name(ty: &Type) -> Option<&'static str> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    if type_path.qself.is_some() {
        return None;
    }
    let last = type_path.path.segments.last()?;
    match last.ident.to_string().as_str() {
        "i64" => Some("i64"),
        "f64" => Some("f64"),
        "bool" => Some("bool"),
        "String" => Some("String"),
        _ => None,
    }
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    // Reject any generic parameter outright (v1 restriction, design §6/§7:
    // lifetimes fail `T: 'static`; type/const params can't be resolved to
    // a concrete `Any` type by this macro). Only the FIRST offending
    // parameter is reported -- equivalent to iterating and returning on
    // the first match, since every arm below unconditionally returns.
    if let Some(param) = input.generics.params.first() {
        return Err(match param {
            GenericParam::Lifetime(lt) => syn::Error::new(
                lt.lifetime.span(),
                format!(
                    "MovaStruct requires T: 'static (found lifetime parameter '{})",
                    lt.lifetime.ident
                ),
            ),
            GenericParam::Type(ty) => syn::Error::new(
                ty.ident.span(),
                "#[derive(MovaStruct)] does not support generic type parameters (v1 \
                 restriction -- see mova-derive's crate docs' \"Adversarial cases\" \
                 section): every instantiation would need its own independently concrete, \
                 'static field-type dispatch, which this macro cannot resolve at expansion \
                 time",
            ),
            GenericParam::Const(c) => syn::Error::new(
                c.ident.span(),
                "#[derive(MovaStruct)] does not support const generic parameters (v1 \
                 restriction -- see mova-derive's crate docs)",
            ),
        });
    }

    let container_attrs = parse_container_attrs(&input)?;
    let crate_path_str = container_attrs
        .crate_path
        .as_ref()
        .map(|lit| lit.value())
        .unwrap_or_else(|| DEFAULT_CRATE_PATH.to_string());
    let krate: syn::Path = syn::parse_str(&crate_path_str).map_err(|_| {
        syn::Error::new(
            container_attrs
                .crate_path
                .as_ref()
                .map(|l| l.span())
                .unwrap_or_else(Span2::call_site),
            format!("`#[mova(crate = \"{crate_path_str}\")]` is not a valid crate path"),
        )
    })?;

    let ident = &input.ident;
    let type_name_lit = LitStr::new(&ident.to_string(), ident.span());

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            Fields::Unnamed(_) => {
                return Err(syn::Error::new(
                    data.fields.span(),
                    "#[derive(MovaStruct)] does not support tuple structs (v1 restriction -- \
                     named fields are required, since the script-visible key comes from the \
                     field's Rust identifier or #[mova(rename = \"...\")])",
                ));
            }
            Fields::Unit => {
                return Err(syn::Error::new(
                    ident.span(),
                    "#[derive(MovaStruct)] does not support unit structs (v1 restriction -- a \
                     unit struct has no fields to register; if you want a script-visible empty \
                     HostStruct, write `struct S {}` instead)",
                ));
            }
        },
        Data::Enum(data_enum) => {
            return Err(syn::Error::new(
                data_enum.enum_token.span(),
                "#[derive(MovaStruct)] does not support enums (v1 restriction -- \
                 Value::HostStruct wraps map-shaped data only; an enum needs its own script \
                 representation design, see DESIGN-hoststruct-derive.md §7's deferred list)",
            ));
        }
        Data::Union(data_union) => {
            return Err(syn::Error::new(
                data_union.union_token.span(),
                "#[derive(MovaStruct)] does not support unions",
            ));
        }
    };

    let mut field_calls = Vec::with_capacity(fields.len());
    for field in fields {
        let attrs = parse_field_attrs(field)?;
        if attrs.skip {
            continue;
        }
        // `unwrap` is safe: `Fields::Named` guarantees every field has an
        // identifier.
        let field_ident = field.ident.as_ref().unwrap();
        let key = attrs
            .rename
            .map(|lit| lit.value())
            .unwrap_or_else(|| field_ident.to_string());
        let key_lit = LitStr::new(&key, field_ident.span());
        let field_span = field.span();

        let value_expr = if let Some(getter_ident) = attrs.getter {
            // Getter fields: the return type isn't visible to this macro
            // at all, so dispatch through `IntoMovaValue` (trait-driven,
            // see the crate docs' "Error-UX strategy").
            quote_spanned! {field_span=>
                #krate::embed::host::IntoMovaValue::into_mova_value(s.#getter_ident())
            }
        } else {
            match literal_type_name(&field.ty) {
                Some("i64") | Some("f64") | Some("bool") => quote_spanned! {field_span=>
                    #krate::embed::Value::from(s.#field_ident)
                },
                Some("String") => quote_spanned! {field_span=>
                    #krate::embed::Value::from(s.#field_ident.as_str())
                },
                _ => {
                    let ty_str = field.ty.to_token_stream().to_string();
                    return Err(syn::Error::new(
                        field_span,
                        format!(
                            "field `{field_ident}: {ty_str}` has no Value conversion; add \
                             #[mova(skip)], or #[mova(getter = \"...\")] with a method \
                             returning a supported type, or narrow the field",
                        ),
                    ));
                }
            }
        };

        field_calls.push(quote_spanned! {field_span=>
            .field(#key_lit, |s| #value_expr)
        });
    }

    Ok(quote! {
        #[automatically_derived]
        impl #krate::embed::host::MovaStruct for #ident {
            fn shape() -> &'static #krate::embed::host::Shape<#ident> {
                // Function-scoped, so it never needs to be unique across
                // `impl MovaStruct` blocks for different types -- each
                // lives in its own `shape()` fn body. Matches
                // DESIGN-hoststruct-derive.md §1.2's sketch verbatim.
                static SHAPE: ::std::sync::OnceLock<#krate::embed::host::Shape<#ident>> =
                    ::std::sync::OnceLock::new();
                SHAPE.get_or_init(|| {
                    #krate::embed::host::ShapeBuilder::<#ident>::new(#type_name_lit)
                        #(#field_calls)*
                        .build()
                })
            }
        }

        #[automatically_derived]
        impl #krate::embed::host::WrapExt for #ident {
            fn wrap(self: ::std::sync::Arc<Self>) -> #krate::embed::Value {
                #krate::embed::host::wrap_struct(self, <Self as #krate::embed::host::MovaStruct>::shape())
            }
        }
    })
}
