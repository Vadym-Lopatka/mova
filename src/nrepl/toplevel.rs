//! Top-level forms that contain a loop run compiled.
//!
//! Only fn bodies reach the compiler, so a bare `(dotimes [i 1000000] ...)`
//! typed at the REPL would run in the tree-walker (about 0.7 us per bare
//! iteration against 0.04 us compiled). The JVM treats every top-level form
//! as the body of an anonymous zero-arg fn, so evaluating `(fn* [] form)` and
//! calling it once is the same thing, with the same results. It costs about
//! 1.3 us to compile, so it is done only when a loop is found.
//!
//! Not wrapped (they must run at top level): `ns`, `in-ns`, `def`-like heads,
//! `require`-like heads, `set!`. A top-level `do` is split, each child is a
//! top-level form of its own (as the JVM does), so a `def` in one child is
//! visible to the next.

use crate::compile::explain::find_top_level_loop;
use crate::error::RjError;
use crate::eval::Interp;
use crate::reader::{Form, FormValue};
use crate::value::{Symbol, Value};

/// Heads that must run at top level, as they are.
fn stays_top_level(name: &str) -> bool {
    matches!(
        name,
        "ns" | "in-ns" | "def" | "defonce" | "defmacro" | "defmulti" | "defmethod" | "declare" | "defprotocol"
            | "defrecord" | "deftype" | "definterface" | "require" | "use" | "import" | "refer" | "load"
            | "load-file" | "set!" | "alias" | "refer-clojure" | "ns-unalias" | "remove-ns" | "create-ns"
    )
}

/// Macros the loop finder cannot see through, but that only wrap their body.
fn transparent(name: &str) -> bool {
    matches!(name, "when" | "when-not" | "binding" | "time" | "with-out-str" | "with-redefs" | "let" | "if-let" | "when-let")
}

fn head(form: &Form) -> Option<&str> {
    match &form.value {
        FormValue::List(items) => match items.first().map(|f| &f.value) {
            Some(FormValue::Atom(Value::Sym(s))) if s.ns.is_none() => Some(&s.name),
            _ => None,
        },
        _ => None,
    }
}

fn has_loop(interp: &Interp, form: &Form) -> bool {
    if find_top_level_loop(interp, &interp.globals, form).is_some() {
        return true;
    }
    match (&form.value, head(form)) {
        (FormValue::List(items), Some(h)) if transparent(h) => items[1..].iter().any(|f| has_loop(interp, f)),
        _ => false,
    }
}

/// Evaluates one top-level form (what `Interp::eval_form` does), compiled when it holds a loop.
pub(crate) fn eval_form(interp: &mut Interp, form: &Form) -> Result<Value, RjError> {
    if !interp.compile_enabled() {
        return interp.eval_form(form);
    }
    match head(form) {
        Some("do") => {
            let FormValue::List(items) = &form.value else { unreachable!() };
            if items.len() < 2 {
                return interp.eval_form(form);
            }
            let mut last = Value::Nil;
            for f in &items[1..] {
                last = eval_form(interp, f)?;
            }
            Ok(last)
        }
        Some(h) if stays_top_level(h) => interp.eval_form(form),
        _ if form.meta.is_none() && has_loop(interp, form) => {
            let sp = form.span;
            let wrapped = Form::bare(
                FormValue::List(vec![
                    Form::bare(FormValue::Atom(Value::Sym(Symbol::simple("fn*"))), sp),
                    Form::bare(FormValue::Vector(Vec::new()), sp),
                    form.clone(),
                ]),
                sp,
            );
            let f = interp.eval_form(&wrapped)?;
            interp.call(&f, &[])
        }
        _ => interp.eval_form(form),
    }
}
