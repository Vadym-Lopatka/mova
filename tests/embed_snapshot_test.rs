//! Design-spike tests for `Interp::snapshot` (embed/probe-snapshot): proves
//! the FORK isolation contract the doc comment promises -- a `def`/`swap!`
//! done through one side after `snapshot()` must not be observable through
//! the other, in both directions -- and pins the chosen atom-sharing
//! semantics (shared, matching Clojure reference identity) with an explicit
//! test rather than leaving it as an assumption.

use mova::embed::Engine;

fn eval_int(engine: &mut Engine, src: &str) -> i64 {
    let v = engine
        .eval_named("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()));
    v.as_i64().unwrap_or_else(|| panic!("expected an int, got {v:?}"))
}

fn eval_str(engine: &mut Engine, src: &str) -> String {
    engine
        .eval_named("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
        .to_string()
}

fn eval_err_str(engine: &mut Engine, src: &str) -> String {
    match engine.eval_named("test", src) {
        Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
        Err(e) => e.to_string(),
    }
}

/// A `def` made in the fork after `snapshot()` must not appear in the
/// original.
#[test]
fn def_in_fork_not_visible_in_original() {
    let mut original = Engine::builder().build();
    original.eval_named("test", "(def shared 1)").unwrap();
    let mut fork = original.snapshot();

    fork.eval_named("test", "(def only-in-fork 42)").unwrap();

    // Unresolved symbols are a runtime error in mova (no `resolve`
    // builtin that degrades to nil), so "not visible" is "fails to
    // resolve", not "resolves to nil".
    let original_sees_it = original.eval_named("test", "only-in-fork");
    assert!(original_sees_it.is_err(), "original must not see fork's def, got {original_sees_it:?}");
}

/// A `def` made in the original AFTER `snapshot()` must not appear in the
/// fork -- the mirror-image direction, proving isolation is symmetric, not
/// just "the fork can't see back into the original's future writes" by
/// accident of copy direction.
#[test]
fn def_in_original_after_snapshot_not_visible_in_fork() {
    let mut original = Engine::builder().build();
    original.eval_named("test", "(def shared 1)").unwrap();
    let mut fork = original.snapshot();

    original.eval_named("test", "(def only-in-original 99)").unwrap();

    let fork_sees_it = fork.eval_named("test", "only-in-original");
    assert!(fork_sees_it.is_err(), "fork must not see original's post-snapshot def, got {fork_sees_it:?}");
}

/// A redefinition of a var that existed BEFORE the snapshot, done on one
/// side afterwards, must not change what the other side reads -- the var
/// cells themselves must have been re-wrapped (new `Arc`), not just the
/// map's `Value` payload.
#[test]
fn redefine_of_pre_snapshot_var_does_not_cross() {
    let mut original = Engine::builder().build();
    original.eval_named("test", "(def x 1)").unwrap();
    let mut fork = original.snapshot();

    fork.eval_named("test", "(def x 2)").unwrap();
    assert_eq!(eval_int(&mut fork, "x"), 2);
    assert_eq!(eval_int(&mut original, "x"), 1, "original's x must stay 1 after fork redefines it");

    original.eval_named("test", "(def x 100)").unwrap();
    assert_eq!(eval_int(&mut original, "x"), 100);
    assert_eq!(eval_int(&mut fork, "x"), 2, "fork's x must stay 2 after original redefines it");
}

/// Pins the chosen atom semantics: an atom captured in a pre-snapshot def'd
/// value is Arc-SHARED between the original and every snapshot, matching
/// real Clojure reference-identity semantics ((identical? a a) survives).
/// `snapshot()` forks the VAR TABLE, not the heap -- see `Interp::snapshot`'s
/// doc for the rationale. A `swap!` done through the fork on such an atom
/// IS visible through the original, and vice versa.
#[test]
fn pre_snapshot_atom_is_shared_not_copied() {
    let mut original = Engine::builder().build();
    original.eval_named("test", "(def counter (atom 0))").unwrap();
    let mut fork = original.snapshot();

    fork.eval_named("test", "(swap! counter inc)").unwrap();
    assert_eq!(
        eval_int(&mut original, "@counter"),
        1,
        "swap! through the fork must be visible through the original: shared atom identity"
    );

    original.eval_named("test", "(swap! counter inc)").unwrap();
    assert_eq!(
        eval_int(&mut fork, "@counter"),
        2,
        "swap! through the original must be visible through the fork: shared atom identity"
    );
}

/// A brand-new atom `def`'d independently on each side after the snapshot
/// is, obviously, two different atoms (no shared ancestry) -- sanity check
/// that isolation of the VAR holding it still holds even though atoms in
/// general are shared-by-design.
#[test]
fn post_snapshot_atoms_are_independent() {
    let mut original = Engine::builder().build();
    let mut fork = original.snapshot();

    original.eval_named("test", "(def a (atom 10))").unwrap();
    fork.eval_named("test", "(def a (atom 20))").unwrap();

    original.eval_named("test", "(swap! a inc)").unwrap();
    fork.eval_named("test", "(swap! a + 100)").unwrap();

    assert_eq!(eval_int(&mut original, "@a"), 11);
    assert_eq!(eval_int(&mut fork, "@a"), 120);
}

/// Functions defined before the snapshot keep working identically on both
/// sides (a basic sanity check that the clone is a genuinely usable
/// interpreter, not just an isolated-but-broken var table).
#[test]
fn pre_snapshot_functions_still_work_on_both_sides() {
    let mut original = Engine::builder().build();
    original.eval_named("test", "(defn sq [x] (* x x))").unwrap();
    let mut fork = original.snapshot();

    assert_eq!(eval_int(&mut original, "(sq 7)"), 49);
    assert_eq!(eval_int(&mut fork, "(sq 7)"), 49);
}

/// `require`/namespace bookkeeping done after the snapshot must also stay
/// isolated: an alias added in the fork's current namespace must not leak
/// into the original's view of the same namespace name.
#[test]
fn namespace_alias_added_after_snapshot_does_not_cross() {
    let mut original = Engine::builder().build();
    original.eval_named("test", "(ns probe.ns-a)").unwrap();
    let mut fork = original.snapshot();

    fork.eval_named("test", "(ns probe.ns-a) (def only-fork-ns 7)").unwrap();
    original.eval_named("test", "(ns probe.ns-a)").unwrap();

    let seen = original.eval_named("test", "only-fork-ns");
    assert!(seen.is_err(), "namespace-scoped def in fork must not leak into original, got {seen:?}");
}

// ==================== registry completeness (W-SNAP) ====================
//
// `Interp::snapshot` used to reset `protocols`/`interfaces`/`multimethods`
// to brand-new EMPTY registries instead of deep-copying them like
// `globals`/`namespaces`. A `defprotocol`/`extend-type`/`extend-protocol`/
// `defmulti`/`defmethod` executed BEFORE `snapshot()` was silently absent
// from the clone -- dispatch found no methods even though the protocol/
// multimethod var itself resolved fine. The tests below pin the fixed
// contract: pre-snapshot definitions survive into the clone, and
// definitions made after the snapshot, on either side, stay isolated --
// exactly `globals`' own contract, extended to these three registries.

/// A `defprotocol` + `extend-type` done BEFORE `snapshot()` must dispatch
/// correctly on BOTH the original and the clone afterwards -- the exact gap
/// the audit found (see this section's header comment).
#[test]
fn protocol_extended_before_snapshot_dispatches_on_both_sides() {
    let mut original = Engine::builder().build();
    original
        .eval_named(
            "test",
            "(defprotocol P (m [x])) (defrecord R [a]) (def r (->R 1)) \
             (extend-type R P (m [_] :exact))",
        )
        .unwrap();
    let mut fork = original.snapshot();

    assert_eq!(eval_str(&mut original, "(m r)"), ":exact");
    assert_eq!(eval_str(&mut fork, "(m r)"), ":exact");
}

/// An `extend-type` (the same shape `extend-protocol` compiles down to)
/// added AFTER `snapshot()`, on either side, must stay invisible to the
/// other -- mirroring `def_in_fork_not_visible_in_original` /
/// `def_in_original_after_snapshot_not_visible_in_fork` above, but for the
/// protocol registry instead of `globals`. `R`'s `TypeDef` and `P`'s var
/// cell both predate the snapshot and are legitimately SHARED (`Arc`)
/// between the two engines afterwards (see `Interp::snapshot`'s doc on
/// `globals` cloning the `Value` inside each cell, not the `Arc` payloads
/// it points at) -- this test is exactly what proves that sharing those
/// identities does NOT also share the mutable registry state keyed by them.
#[test]
fn protocol_extension_added_after_snapshot_is_isolated_both_directions() {
    let mut original = Engine::builder().build();
    original
        .eval_named("test", "(defprotocol P (m [x])) (defrecord R [a]) (def r (->R 1))")
        .unwrap();
    let mut fork = original.snapshot();

    fork.eval_named("test", "(extend-type R P (m [_] :fork-only))").unwrap();
    assert_eq!(eval_str(&mut fork, "(m r)"), ":fork-only");
    let original_err = eval_err_str(&mut original, "(m r)");
    assert!(
        original_err.contains("No implementation of method"),
        "original must not see fork's post-snapshot extend-type, got {original_err:?}"
    );

    original.eval_named("test", "(extend-type R P (m [_] :orig-only))").unwrap();
    assert_eq!(eval_str(&mut original, "(m r)"), ":orig-only");
    assert_eq!(
        eval_str(&mut fork, "(m r)"),
        ":fork-only",
        "fork's own extension must be unaffected by original's later, independent one"
    );
}

/// A `defmulti` + `defmethod` done BEFORE `snapshot()` must dispatch
/// correctly on BOTH sides afterwards, and a `defmethod` added on either
/// side AFTER the snapshot stays isolated to that side -- the `Multimethods`
/// half of the same gap `protocol_extended_before_snapshot_dispatches_on_
/// both_sides` pins for `Protocols`.
#[test]
fn multimethod_defined_before_snapshot_dispatches_on_both_sides_then_isolates() {
    let mut original = Engine::builder().build();
    original
        .eval_named("test", "(defmulti m identity) (defmethod m :a [_] :a-method)")
        .unwrap();
    let mut fork = original.snapshot();

    assert_eq!(eval_str(&mut original, "(m :a)"), ":a-method");
    assert_eq!(eval_str(&mut fork, "(m :a)"), ":a-method");

    fork.eval_named("test", "(defmethod m :b [_] :fork-only)").unwrap();
    assert_eq!(eval_str(&mut fork, "(m :b)"), ":fork-only");
    let original_err = eval_err_str(&mut original, "(m :b)");
    assert!(
        original_err.contains("No method in multimethod"),
        "original must not see fork's post-snapshot defmethod, got {original_err:?}"
    );

    original.eval_named("test", "(defmethod m :c [_] :orig-only)").unwrap();
    assert_eq!(eval_str(&mut original, "(m :c)"), ":orig-only");
    let fork_err = eval_err_str(&mut fork, "(m :c)");
    assert!(
        fork_err.contains("No method in multimethod"),
        "fork must not see original's post-snapshot defmethod, got {fork_err:?}"
    );
}

/// The dynamic-binding half of the same audit: a var's ROOT value (from
/// `globals.snapshot()`'s per-cell copy, already covered by the tests
/// above this section) is what the clone starts from, and a `binding`
/// scope entered on one engine must not be visible on the other -- dynamic
/// bindings are a per-`Interp`/per-thread stack (`Interp::stack`), which is
/// reset to `Vec::new()` on both `fork` and `snapshot` already, so this is
/// mostly a sanity pin rather than a fix, but it is the one piece of the
/// audit's checklist not otherwise covered by an existing test.
#[test]
fn dynamic_binding_root_is_copied_and_bindings_do_not_cross() {
    let mut original = Engine::builder().build();
    // ^:dynamic is load-bearing since W-DECL: `binding` a non-dynamic var
    // now throws, as on the JVM (compat/w-decl-binding-oracle-transcript.txt).
    original
        .eval_named("test", "(def ^:dynamic dv :root)")
        .unwrap();
    let mut fork = original.snapshot();

    // The clone starts from the SAME root value the original had at
    // snapshot time.
    assert_eq!(eval_str(&mut fork, "dv"), ":root");

    // A `binding` scope entered through the fork is invisible to the
    // original -- both because it is thread/call-local (`Interp::stack`)
    // AND because the var cells themselves are independent after the
    // globals fork.
    assert_eq!(eval_str(&mut fork, "(binding [dv :fork-bound] dv)"), ":fork-bound");
    assert_eq!(eval_str(&mut original, "dv"), ":root", "original must not see fork's binding scope");

    // And the reverse direction.
    assert_eq!(eval_str(&mut original, "(binding [dv :orig-bound] dv)"), ":orig-bound");
    assert_eq!(eval_str(&mut fork, "dv"), ":root", "fork must not see original's binding scope");
}

/// The `KeywordRegistry` (`find-keyword`'s backing store) gets the SAME
/// deep-copy treatment as `protocols`/`interfaces`/`multimethods`: a
/// keyword interned before the snapshot (via `keyword` or literal
/// evaluation) is `find-keyword`-able on both engines afterwards, and one
/// interned on either side AFTER the snapshot stays local to that side --
/// see `KeywordRegistry::snapshot`'s doc for why this one was a trivial,
/// same-shape (`Arc<RwLock<HashSet<Str>>>`) fix alongside the other three.
#[test]
fn find_keyword_registered_before_snapshot_is_visible_on_both_sides_then_isolates() {
    let mut original = Engine::builder().build();
    original.eval_named("test", "(keyword \"pre-snap-kw\")").unwrap();
    let mut fork = original.snapshot();

    assert_eq!(eval_str(&mut original, "(find-keyword \"pre-snap-kw\")"), ":pre-snap-kw");
    assert_eq!(eval_str(&mut fork, "(find-keyword \"pre-snap-kw\")"), ":pre-snap-kw");

    fork.eval_named("test", "(keyword \"fork-only-kw\")").unwrap();
    assert_eq!(eval_str(&mut fork, "(find-keyword \"fork-only-kw\")"), ":fork-only-kw");
    assert_eq!(
        eval_str(&mut original, "(find-keyword \"fork-only-kw\")"),
        "nil",
        "original must not see fork's post-snapshot keyword intern"
    );
}
