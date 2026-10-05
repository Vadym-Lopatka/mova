//! `#[derive(MovaStruct)]` UI-test suite (`trybuild`): every case here must
//! FAIL to compile, and fail with a message pointing at the field/struct
//! the user actually got wrong -- see `DESIGN-hoststruct-derive.md` §6
//! ("Compile-time error UX... this is the single biggest quality bar for a
//! derive macro's first impression") and this crate's own top-level docs'
//! "Error-UX strategy" section.
//!
//! ## The `.stderr` snapshots, and the rustc-version pinning caveat
//!
//! `trybuild` requires a committed `tests/ui/<case>.stderr` snapshot for
//! every `compile_fail` case -- without one, it writes the actual output to
//! `wip/<case>.stderr` and fails the run on purpose, asking a human to
//! review and move it into place (the same "record, then commit" workflow
//! `insta` uses). The seven `.stderr` files alongside the `fail_*.rs`
//! fixtures here were generated exactly that way, on `rustc 1.94.0`.
//!
//! **Honest caveat**: this pins the suite to that rustc version's exact
//! diagnostic RENDERING (span underline columns, `error:`/`help:` line
//! wrapping) -- not just this crate's own error message text. A future
//! rustc bump that reformats diagnostics (a real, recurring thing rustc
//! does) will make one or more of these `.stderr` comparisons fail even
//! though the derive's OWN behavior hasn't changed at all; the fix in that
//! case is to re-run this suite, review the new `wip/*.stderr` output to
//! confirm the macro's authored message text is still correct, and copy
//! the regenerated file over the stale one -- not to treat a `.stderr`
//! mismatch alone as a regression in the derive.
#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_basic.rs");
    t.compile_fail("tests/ui/fail_*.rs");
}
