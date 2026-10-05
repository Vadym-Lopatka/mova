//! Native fast-path macro expanders. Form-in -> `Value`-out, exactly the
//! same contract `apply_macro` already has (see `eval::mod`'s dispatch and
//! `eval::special_forms::macroexpand_1_value`), wired onto a `Closure` via
//! `Closure::native_macro` (an optional plain fn pointer -- see that
//! field's doc). Every native expander here is a fast path ONLY: any input
//! shape it isn't confident about falls back to the ORIGINAL interpreted
//! `core.mova` closure (kept alive as `fallback`), which stays the
//! authority for every edge case (bad name, malformed fdecl, the `ex-info`
//! throws) -- see `install::install_native_macros`, called once after
//! `load_core`.
use crate::error::RjError;
use crate::eval::Interp;
use crate::keyword::Keyword;
use crate::reader::{form_to_value, Form, Span};
use crate::value::{Closure, PMap, Symbol, Value};
use std::sync::Arc;

fn kw(s: &str) -> Value {
    Value::Keyword(Keyword::from(s))
}

fn map1(k: &str, v: Value) -> Value {
    let mut m = PMap::new();
    m.insert(kw(k), v);
    Value::Map(m)
}

/// `(merge a b)`, restricted to the two shapes `defn`'s body ever feeds it
/// (a map or `nil` on either side) -- see `builtins::collections::merge`
/// for the fully general native this mirrors on that narrow domain.
fn merge2(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Nil, Value::Nil) => Value::Nil,
        (Value::Nil, other) => other,
        (other, Value::Nil) => other,
        (Value::Map(ma), Value::Map(mb)) => {
            let mut m = ma.clone();
            for (k, v) in mb.iter() {
                m.insert(k.clone(), v.clone());
            }
            Value::Map(m)
        }
        (a, _) => a,
    }
}

/// Fast path for `core.mova`'s `(defmacro defn [name & fdecl] ...)`.
/// Returns `None` on any shape it isn't the well-formed common case for;
/// `native_defn` below falls back to the interpreted closure in that case.
fn try_native_defn(args: &[Form]) -> Option<Value> {
    if args.is_empty() {
        return None;
    }
    let name_val = form_to_value(&args[0]);
    if !matches!(name_val.clone().into_unmeta(), Value::Sym(_)) {
        return None;
    }
    let rest = &args[1..];
    let hi = rest.len();
    let mut lo = 0usize;
    let mut m = Value::Map(PMap::new());

    if lo < hi {
        if let Value::Str(s) = form_to_value(&rest[lo]).into_unmeta() {
            m = map1("doc", Value::Str(s));
            lo += 1;
        }
    }
    if lo < hi {
        if let Value::Map(mm) = form_to_value(&rest[lo]).into_unmeta() {
            m = merge2(m, Value::Map(mm));
            lo += 1;
        }
    }
    if lo >= hi {
        return None;
    }

    let single_arity = matches!(form_to_value(&rest[lo]).into_unmeta(), Value::Vector(_));
    let mut clauses: Vec<Value> = if single_arity {
        vec![Value::List(rest[lo..hi].iter().map(form_to_value).collect())]
    } else {
        rest[lo..hi].iter().map(form_to_value).collect()
    };
    if clauses.is_empty() {
        return None;
    }

    let last_idx = clauses.len() - 1;
    if let Value::Map(mm) = clauses[last_idx].clone().into_unmeta() {
        m = merge2(m, Value::Map(mm));
        clauses.pop();
    }
    if clauses.is_empty() {
        return None;
    }

    m = merge2(name_val.obj_meta(), m);

    let mut param_vecs: Vec<Value> = Vec::with_capacity(clauses.len());
    for c in &clauses {
        let items = match c.clone().into_unmeta() {
            Value::List(items) => items,
            _ => return None,
        };
        let first = match items.first() {
            Some(v) => v.clone(),
            None => return None,
        };
        if !matches!(first.clone().into_unmeta(), Value::Vector(_)) {
            return None;
        }
        param_vecs.push(first);
    }

    let arglists = Value::List(param_vecs.into_iter().collect());
    let quoted_arglists =
        Value::List(vec![Value::Sym(Symbol::simple("quote")), arglists].into_iter().collect());
    m = merge2(map1("arglists", quoted_arglists), m);

    // `(cons 'fn (cons name fdecl))` in the original body splices in the
    // RAW `name` param binding, own reader meta and all -- `with-meta`
    // below builds a SEPARATE value (bare name + the merged `m`) only for
    // the outer `def` target, never touching this one.
    let fn_form_items: Vec<Value> = std::iter::once(Value::Sym(Symbol::simple("fn")))
        .chain(std::iter::once(name_val.clone()))
        .chain(clauses.into_iter())
        .collect();
    let fn_form = Value::List(fn_form_items.into_iter().collect());
    let named_name = Value::attach_meta(name_val.into_unmeta(), m);
    let result = Value::List(
        vec![Value::Sym(Symbol::simple("def")), named_name, fn_form].into_iter().collect(),
    );
    Some(result)
}

pub fn native_defn(
    interp: &mut Interp,
    args: &[Form],
    span: Span,
    fallback: &Arc<Closure>,
) -> Result<Value, RjError> {
    if let Some(v) = try_native_defn(args) {
        return Ok(v);
    }
    let raw_call_form = Value::List(
        std::iter::once(Value::Sym(Symbol::simple("defn")))
            .chain(args.iter().map(form_to_value))
            .collect(),
    );
    interp.apply_macro(fallback, args, raw_call_form, span)
}
