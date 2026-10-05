//! S4 special forms: `defmulti`/`defmethod` -- see `crate::multi`'s module
//! doc for the measured hierarchy/dispatch semantics these lean on. Tree-
//! walk tier ONLY, same reasoning as S3's `types_forms`: `compile::resolve`
//! bails on both heads (multimethod definitions are not a hot path).

use crate::env::Env;
use crate::error::RjError;
use crate::reader::{Form, FormValue, Span};
use crate::value::{Keyword, PMap, Value};

use super::{form_as_symbol, Interp};

impl Interp {
    /// `(defmulti name docstring? attr-map? dispatch-fn & {:default v,
    /// :hierarchy h})`.
    ///
    /// Measured defonce-like no-redefinition (`compat/multimethods-
    /// probe.clj` p09/p10): a second `defmulti` on a var already bound to
    /// a REGISTERED multimethod is a silent no-op returning `nil` --
    /// keeps the OLD dispatch-fn/default/hierarchy untouched (real
    /// Clojure's own reason to exist: reloading a file must not wipe
    /// `defmethod`s already installed). The var identity is preserved
    /// either way (`resolve_var_cell`'s "same cell across redefinition"
    /// contract). The FIRST call returns the VAR itself (`#'ns/name`,
    /// measured -- unlike `eval_def`, which returns the value).
    pub(super) fn eval_defmulti(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        let mut idx = 0usize;
        let name = args
            .first()
            .and_then(form_as_symbol)
            .cloned()
            .ok_or_else(|| self.err_here(RjError::other("defmulti: expected a name symbol"), span))?;
        idx += 1;

        // Optional docstring, tolerated and discarded (matches `def`'s own
        // convention elsewhere in this crate) -- only if it's not the LAST
        // form (a dispatch-fn must still follow).
        if idx + 1 < args.len() && matches!(&args[idx].value, FormValue::Atom(Value::Str(_))) {
            idx += 1;
        }
        // Optional attr-map, likewise tolerated and discarded (rare in
        // practice; mova has no var metadata to attach it to).
        if idx + 1 < args.len() && matches!(&args[idx].value, FormValue::Map(_)) {
            idx += 1;
        }

        let dispatch_form = args
            .get(idx)
            .ok_or_else(|| self.err_here(RjError::other("defmulti: expected a dispatch function"), span))?;
        idx += 1;
        let dispatch_fn = self.eval_form_in(dispatch_form, env)?;

        let mut default_val = Value::Keyword(Keyword::from("default"));
        let mut hierarchy_ref: Option<Value> = None;
        while idx + 1 < args.len() {
            let key = match &args[idx].value {
                FormValue::Atom(Value::Keyword(k)) => k.clone(),
                _ => break,
            };
            let val = self.eval_form_in(&args[idx + 1], env)?;
            match key.as_ref() {
                "default" => default_val = val,
                "hierarchy" => hierarchy_ref = Some(val),
                _ => {} // unrecognized options ignored (honest v1)
            }
            idx += 2;
        }

        let qualified = self.qualify_def(&name);
        let cell = self.resolve_var_cell(&qualified);

        // Defonce-like no-op check (measured, see doc above).
        if let Some(cur) = cell.get() {
            if let Some(existing_key) = crate::multi::multi_key(&cur) {
                if crate::sync::lock_read(&self.multimethods.0).contains_key(&existing_key) {
                    return Ok(Value::Nil);
                }
            }
        }

        let (native, key) = crate::multi::make_dispatch_native(name.name.clone());
        crate::sync::lock_write(&self.multimethods.0).insert(
            key,
            crate::multi::MultiDef {
                name: name.name.clone(),
                dispatch_fn,
                default_val,
                hierarchy_ref,
                methods: PMap::new(),
                prefers: PMap::new(),
                cache: std::sync::Arc::new(crate::multi::MultiCache::default()),
            },
        );
        cell.store(Value::Native(native), false);
        Ok(Value::Var(cell))
    }

    /// `(defmethod name dispatch-val [params] body...)` -- measured:
    /// returns the multimethod's CURRENT value (the raw dispatch native,
    /// prints like real Clojure's `#object[clojure.lang.MultiFn ...]`
    /// modulo the printed identity text itself, which mova's own `Native`
    /// printer spells differently -- see `compat/multimethods-probe.clj`'s
    /// header note on why every corpus use of this return value is
    /// `(do ... :ok)`-wrapped instead of asserted directly). A second
    /// `defmethod` on the same dispatch value silently REPLACES the fn
    /// (measured: `compat/multimethods-probe.clj` p88-p91).
    pub(super) fn eval_defmethod(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        let name = args
            .first()
            .and_then(form_as_symbol)
            .cloned()
            .ok_or_else(|| self.err_here(RjError::other("defmethod: expected a multimethod name"), span))?;
        let cell = self
            .for_each_global_candidate(&name, |cand| self.globals.find_bound_cell(cand))
            .ok_or_else(|| {
                self.err_here(
                    RjError::unresolved(format!(
                        "Unable to resolve symbol: {}",
                        crate::printer::pr_str(&Value::Sym(name.clone()))
                    )),
                    args[0].span,
                )
                .with_label("undefined here")
            })?;
        let cur = cell.get().ok_or_else(|| {
            self.err_here(
                RjError::other(format!("defmethod: {} is not a multimethod", name.name)),
                span,
            )
        })?;
        let key = crate::multi::multi_key(&cur).filter(|k| crate::sync::lock_read(&self.multimethods.0).contains_key(k)).ok_or_else(|| {
            self.err_here(
                RjError::other(format!("defmethod: {} is not a multimethod", name.name)),
                span,
            )
        })?;

        let dispatch_val_form = args
            .get(1)
            .ok_or_else(|| self.err_here(RjError::other("defmethod: expected a dispatch value"), span))?;
        let dispatch_val = self.eval_form_in(dispatch_val_form, env)?;
        let fn_val = self.eval_fn_form(&args[2..], span, env)?;

        crate::sync::lock_write(&self.multimethods.0)
            .get_mut(&key)
            .expect("membership just checked above under the same lock discipline")
            .methods
            .insert(dispatch_val, fn_val);
        // W-MULTI: a new/replaced method can change the best-method
        // answer for dispatch values already cached.
        crate::multi::bump_multi_generation();
        Ok(cur)
    }
}
