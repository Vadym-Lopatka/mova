//! `mova.diff` -- lsp/io (clojure-lsp-on-Mova campaign, mova/PLAN.md "reuse
//! Rust crates"): `clojure-lsp.diff`'s two `difflib.DiffUtils` calls
//! (`DiffUtils/diff` building a `Patch`, then `DiffUtils/generateUnifiedDiff`
//! rendering it against `old-name`/`new-name`/context) always compose at
//! their one call site (`unified-diff` in `clojure_lsp/diff.clj`) -- there
//! is no other consumer of a bare `Patch` value anywhere in the vendored or
//! lib sources. So this is ONE native, `unified-diff`, over the `similar`
//! crate's own `TextDiff::unified_diff` (already the git-style unified-diff
//! format java-diff-utils' `generateUnifiedDiff` also produces: `--- a`,
//! `+++ b`, `@@ -l,s +l,s @@` hunk headers, ` `/`-`/`+` prefixed lines).
//!
//! `clojure_lsp/diff.mova`'s overlay passes `original`/`revised` as whole
//! text (not pre-split lines -- `similar::TextDiff::from_lines` does its
//! own line splitting, keeping line endings, which is what the diff
//! algorithm needs to place hunks correctly) and gets back the complete
//! unified-diff TEXT, trimmed of the trailing newline `similar` appends
//! after the last line (real `DiffUtils/generateUnifiedDiff` returns a
//! `List<String>` with no such trailing entry, and the overlay's `unlines`
//! never added one either).

use crate::builtins::ArityHint;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{NativeFn, Symbol, Value};
use std::sync::Arc;

/// Exact duplicate of `builtins::strings::reg_ns` -- private to its own
/// module, same reason every other `mova.*` native module keeps its own
/// copy (see that fn's doc).
fn reg_ns(
    i: &mut Interp,
    ns: &'static str,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let native = NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            return Err(RjError::arity(format!(
                "{name}: expected {}, got {}",
                arity.expected_desc(),
                args.len()
            ))
            .with_stack(interp.stack_snapshot(), interp.source_id));
        }
        f(interp, args)
    });
    i.globals.set_builtin(
        Symbol {
            ns: Some(ns.into()),
            name: name.into(),
        },
        Value::Native(Arc::new(native)),
    );
}

pub fn register(i: &mut Interp) {
    reg_ns(
        i,
        "mova.diff",
        "unified-diff",
        ArityHint::Exact(5),
        |_i, args| unified_diff(args),
    );
}

fn as_str<'a>(v: &'a Value, who: &str) -> Result<&'a str, RjError> {
    match v {
        Value::Str(s) => Ok(s.as_ref()),
        other => Err(RjError::type_err(format!(
            "{who}: expected a string, got {}",
            other.type_name()
        ))),
    }
}

fn unified_diff(args: &[Value]) -> Result<Value, RjError> {
    let old_name = as_str(&args[0], "mova.diff/unified-diff")?;
    let new_name = as_str(&args[1], "mova.diff/unified-diff")?;
    let original = as_str(&args[2], "mova.diff/unified-diff")?;
    let revised = as_str(&args[3], "mova.diff/unified-diff")?;
    let context = match &args[4] {
        Value::Int(n) if *n >= 0 => *n as usize,
        other => {
            return Err(RjError::type_err(format!(
                "mova.diff/unified-diff: expected a non-negative int context, got {}",
                other.type_name()
            )))
        }
    };
    let diff = similar::TextDiff::from_lines(original, revised);
    let mut text = diff
        .unified_diff()
        .context_radius(context)
        .header(old_name, new_name)
        .to_string();
    if text.ends_with('\n') {
        text.pop();
    }
    Ok(Value::Str(text.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_matches_git_style_hunk_format() {
        let out = unified_diff(&[
            Value::Str("a/f".to_string().into()),
            Value::Str("b/f".to_string().into()),
            Value::Str("one\ntwo\nthree\n".to_string().into()),
            Value::Str("one\nTWO\nthree\n".to_string().into()),
            Value::Int(3),
        ])
        .unwrap();
        let Value::Str(s) = out else { panic!("expected string") };
        let s = s.to_string();
        assert!(s.starts_with("--- a/f\n+++ b/f\n@@"), "got: {s}");
        assert!(s.contains("-two\n+TWO\n"), "got: {s}");
        assert!(!s.ends_with('\n'));
    }
}
