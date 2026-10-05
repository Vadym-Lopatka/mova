//! SPEC-W1 task 2: the EMBEDDED STDLIB MODULE TABLE -- namespaces that
//! are `require`-able with no `--module-path` at all, because their source
//! ships inside the binary.
//!
//! # Why this exists
//!
//! `core/core.mova`, `core/async.mova` and `core/flow.mova` are already
//! `include_str!`-ed into the binary, but they are EAGERLY evaluated at
//! `Interp::new` (`eval::mod`'s `load_core`) -- they ARE `clojure.core`
//! and the two bare-global libraries. That shape cannot scale to a real
//! library: `clojure.test.check` is ~3500 lines across seven files, and
//! paying for it on every startup (see `LATENCY-CAMPAIGN.md`, where
//! startup is a headline number) to serve the small fraction of programs
//! that generate data would be exactly the wrong trade.
//!
//! So this table is LAZY: nothing here is read, parsed or evaluated until
//! a `(require 'clojure.test.check.generators)` actually asks for it, at
//! which point `ns::Interp::require_ns` loads it through the SAME code
//! path a file on the module path takes -- same `loaded` memo, same
//! cycle detection, same `*ns*` save/restore, same `.cljc`
//! reader-conditional switch. The only difference is where the bytes came
//! from. Cost when unused: the `&'static str`s sit in the binary's
//! read-only data and are never touched.
//!
//! # Disk wins
//!
//! `require_ns` consults `find_ns_file` FIRST and only falls back here on
//! a miss, so `--module-path` always overrides an embedded namespace --
//! the property `tests/clojure-suite`'s runner depends on (it materializes
//! its own byte-identical copy of these same files into a scratch dir and
//! must keep scoring THAT copy), and the property a user who wants to
//! patch a vendored library needs.
//!
//! **One exception: namespaces the bootstrap already marked loaded.**
//! `require_ns`'s very first check is `if self.ns_loaded(ns) { return
//! Ok(()); }` -- BEFORE it ever consults `find_ns_file`. `Interp::new`'s
//! bootstrap eagerly requires/installs three namespaces before any user
//! code (and therefore any user module-path file) is ever consulted:
//! `clojure.core.async.flow` (`require_core_flow`, a real internal
//! `require_ns` call), `clojure.core.async` (`install_core_async_ns`), and
//! `clojure.repl` (`install_clojure_repl_ns`) -- the latter two bind
//! directly rather than `require_ns`-ing a file, but `seed_builtin_
//! namespaces` marks all three `loaded` regardless, right after. So a
//! LATER `(require 'clojure.core.async.flow)` (user code, or a
//! `--module-path` file of the same name) short-circuits at that first
//! check and never reaches the disk-vs-embedded fallback at all -- disk
//! does NOT win for these three. This is deliberate, not an oversight:
//! these are engine-owned language surfaces (flow's proc-graph engine,
//! the channel natives, the repl introspection macros), not ordinary
//! libraries a module-path file is meant to be able to override -- and it
//! is exactly what makes a stale app-side bridge file (a `flow.mova`/
//! `async.mova`/`repl.mova` an app shipped before these became real
//! namespaces) harmless after an upgrade: it sits on the module path
//! unread, because `require_ns` never gets far enough to look.
//!
//! # Reader conditionals follow the FILE NAME
//!
//! Each row carries the upstream file name, `.cljc` or `.clj`, and
//! `require_ns` switches `#?`/`#?@` dispatch on that extension exactly as
//! it does for a file it read off disk. This is load-bearing: test.check
//! has ~74 `#?` sites across its `.cljc` files, and reading one with
//! conditionals off silently turns every `#?(...)` into a literal
//! 4-element list instead of erroring.
//!
//! # Provenance: three kinds of row
//!
//! See `core/lib/README.md`. Every row is one of exactly three things, and
//! [`Provenance`] (`EmbeddedModule::provenance`) says which:
//!
//! * **Vendored** (`clojure.test.check.*`, `clojure.walk`): a
//!   byte-identical copy of the corresponding
//!   `tests/clojure-suite/vendor-libs/` file, whose SHA-256 is pinned in
//!   `tests/clojure-suite/MANIFEST-LIBS.sha256`.
//!   `stdlib_copies_are_byte_identical_to_vendor_libs` (below) is the
//!   mechanical check that keeps the two in step, so a vendored row can
//!   never silently drift into a fork.
//! * **Ported** (`clojure.spec.alpha`, `clojure.spec.gen.alpha`, added by
//!   SPEC-W3): a mova SOURCE PORT of the upstream library, structurally
//!   verbatim but with a small, ledgered set of `MOVA-PATCH` deviations
//!   (`docs/SPEC-PORT-PATCHES.md`). Its gate is behavioural rather than
//!   byte-level: `tests/spec-smoke/smoke.mova`, run on both mova and real
//!   Clojure 1.13.0-alpha6, whose stdout must match byte-for-byte.
//! * **Native surface** (`clojure.core.async.flow`, added by
//!   DESIGN-flow-namespace.md Part 1): a mova-AUTHORED namespace that is
//!   the Clojure-level surface over a Rust-native engine
//!   (`src/builtins/flow.rs`) -- neither a copy of an upstream file nor a
//!   patched port of one, so neither the byte check nor the MOVA-PATCH
//!   ledger applies. Its gate is `tests/flow_ns_test.rs` plus the
//!   flow-gold conformance suite.
//!
//! `ported_rows_are_not_in_vendor_libs` closes the loop for the two
//! non-vendored kinds together, so a vendored file cannot be mislabelled
//! as a port (or a native surface) to escape its byte check.

/// A row's provenance -- see the module doc's "Provenance" section.
/// `require_ns` neither knows nor cares which of these a row is; the only
/// readers are the provenance tests below, whose whole job is to make
/// mislabelling one fail the build instead of quietly disabling a check.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// A byte-identical copy of a `tests/clojure-suite/vendor-libs/`
    /// file, hash-locked by `stdlib_copies_are_byte_identical_to_vendor_libs`
    /// so it can never quietly become a fork.
    Vendored,
    /// A mova SOURCE PORT of an upstream library: the upstream `.clj`
    /// copied verbatim and then patched in place, every deviation marked
    /// `MOVA-PATCH P<n>` in the source and listed in
    /// `docs/SPEC-PORT-PATCHES.md`. Has no `vendor-libs` original, so the
    /// byte check does not apply; its gate is behavioural instead --
    /// `tests/spec-smoke/smoke.mova`, oracle-diffed against real Clojure
    /// 1.13.0-alpha6.
    Ported,
    /// DESIGN-flow-namespace.md Part 1 point 2: a mova-AUTHORED namespace
    /// that is the Clojure-level surface for a Rust-native engine --
    /// `clojure.core.async.flow`'s `process`/`map->step`/`ping`/
    /// `ping-proc` over `src/builtins/flow.rs`'s natives. Neither a copy
    /// nor a port: there is no upstream `.clj` this diverges from at all
    /// (`core.async.flow`'s own upstream Clojure-level surface is much
    /// larger and mova deliberately implements only the subset its native
    /// engine needs -- see FLOW-DESIGN.md), so a byte check and a
    /// MOVA-PATCH ledger would both be meaningless here. Its gate is the
    /// acceptance corpus in `tests/flow_ns_test.rs` plus the flow-gold
    /// conformance suite.
    NativeSurface,
}

/// One embedded namespace: the namespace it provides, the upstream file
/// name it came from (which decides reader-conditional handling and is
/// what diagnostics report as the source), and its source text.
pub struct EmbeddedModule {
    pub ns: &'static str,
    /// Upstream file name, module-path-relative -- e.g.
    /// `"clojure/test/check/generators.cljc"`. The EXTENSION is
    /// semantic (see the module doc); the rest is for diagnostics.
    pub file: &'static str,
    pub source: &'static str,
    /// SPEC-W3 / DESIGN-flow-namespace.md: which of the three
    /// [`Provenance`] kinds this row is -- see that type's doc.
    #[cfg_attr(not(test), allow(dead_code))]
    pub provenance: Provenance,
}

/// Every namespace the binary can `require` with no module path.
///
/// Seeded (SPEC-W1) with `clojure.test.check` -- the generator backend
/// `clojure.spec.alpha` needs, and the reason the table exists. The two
/// remaining files in the upstream jar,
/// `clojure/test/check/clojure_test.cljc` and
/// `clojure/test/check/clojure_test/assertions.cljc`, are DELIBERATELY
/// absent: both are `clojure.test` integration (`defspec`, the `is`
/// assertion hook) and both `(:require [clojure.test ...])`, a namespace
/// mova does not provide -- the clojure-suite runner materializes a
/// hand-written shim for it, which is a TEST-harness artifact and has no
/// business inside the shipped binary. Nothing in `clojure.spec.alpha`
/// touches either file.
pub fn embedded_modules() -> &'static [EmbeddedModule] {
    &[
        EmbeddedModule {
            ns: "clojure.test.check",
            file: "clojure/test/check.cljc",
            source: include_str!("../core/lib/clojure/test/check.cljc"),
            provenance: Provenance::Vendored,
        },
        EmbeddedModule {
            ns: "clojure.test.check.generators",
            file: "clojure/test/check/generators.cljc",
            source: include_str!("../core/lib/clojure/test/check/generators.cljc"),
            provenance: Provenance::Vendored,
        },
        EmbeddedModule {
            ns: "clojure.test.check.properties",
            file: "clojure/test/check/properties.cljc",
            source: include_str!("../core/lib/clojure/test/check/properties.cljc"),
            provenance: Provenance::Vendored,
        },
        EmbeddedModule {
            ns: "clojure.test.check.rose-tree",
            file: "clojure/test/check/rose_tree.cljc",
            source: include_str!("../core/lib/clojure/test/check/rose_tree.cljc"),
            provenance: Provenance::Vendored,
        },
        EmbeddedModule {
            ns: "clojure.test.check.results",
            file: "clojure/test/check/results.cljc",
            source: include_str!("../core/lib/clojure/test/check/results.cljc"),
            provenance: Provenance::Vendored,
        },
        EmbeddedModule {
            ns: "clojure.test.check.impl",
            file: "clojure/test/check/impl.cljc",
            source: include_str!("../core/lib/clojure/test/check/impl.cljc"),
            provenance: Provenance::Vendored,
        },
        // SPEC-W6a: `clojure.test.check.random` used to be a row here
        // (a `.clj`, the only one test.check ships). It is NOT any
        // more, and its absence is deliberate rather than an oversight
        // -- see `crate::builtins::tcrandom`. That namespace is now
        // Rust-native, and a native veneer's namespace is marked
        // `loaded` at `Interp::new` by `ns::seed_builtin_namespaces`,
        // so `require_ns` returns before it ever consults this table
        // (or the module path). A row here could therefore never be
        // selected, and this table's whole job is to say truthfully
        // what the binary ships and can load.
        //
        // SPEC-W3: `clojure.walk`, vendored verbatim from the same
        // hash-locked tree as test.check. `clojure.spec.alpha`'s ns form
        // opens with `(:require [clojure.walk :as walk])`, so without this
        // row `(require '[clojure.spec.alpha :as s])` cannot work with a
        // bare binary at all. mova has no native `clojure.walk` (probed:
        // `(require 'clojure.walk)` => "could not locate namespace"), and
        // writing one would be a gratuitous fork of a 130-line pure-Clojure
        // file that already loads unmodified -- W1's `.clj` extension
        // support is exactly what makes keeping the upstream name free.
        EmbeddedModule {
            ns: "clojure.walk",
            file: "clojure/walk.clj",
            source: include_str!("../core/lib/clojure/walk.clj"),
            provenance: Provenance::Vendored,
        },
        // clojure-lsp campaign (mova/PLAN.md): rewrite-clj's zipper
        // (`rewrite-clj.zip`) sits on top of `clojure.zip`'s generic
        // zipper protocol (`up`/`down`/`right`/`replace`/`remove`/...),
        // and clojure-lsp's own code navigates with it too. Vendored
        // verbatim, same as `clojure.walk` above: zero host interop
        // besides plain `Exception` construction, zero imports (see
        // `tests/clojure-suite/MANIFEST-LIBS.sha256`'s note on this file),
        // so it loads unmodified -- no overlay patch needed.
        EmbeddedModule {
            ns: "clojure.zip",
            file: "clojure/zip.clj",
            source: include_str!("../core/lib/clojure/zip.clj"),
            provenance: Provenance::Vendored,
        },
        // clojure-lsp campaign (mova/PLAN.md): cljfmt's `cljfmt.report`
        // requires this for `print-stack-trace` in its error-reporting
        // branch. Vendored verbatim, same pinned oracle tree as
        // `clojure.walk`/`clojure.zip` above -- its host interop
        // (`.getCause`/`.getStackTrace`/`.getMessage`) is already covered
        // by mova's exception veneer (`getStackTrace` answers an empty
        // array; measured elsewhere in this campaign), so no overlay
        // patch needed to load it.
        EmbeddedModule {
            ns: "clojure.stacktrace",
            file: "clojure/stacktrace.clj",
            source: include_str!("../core/lib/clojure/stacktrace.clj"),
            provenance: Provenance::Vendored,
        },
        // nREPL gaps: mova-authored `clojure.pprint` (see its header for what
        // is covered) and the nREPL printing namespace on top of it.
        EmbeddedModule {
            ns: "clojure.pprint",
            file: "clojure/pprint.mova",
            source: include_str!("../core/lib/clojure/pprint.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "mova.error-text",
            file: "mova/error_text.mova",
            source: include_str!("../core/lib/mova/error_text.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.util.print",
            file: "nrepl/util/print.mova",
            source: include_str!("../core/lib/nrepl/util/print.mova"),
            provenance: Provenance::NativeSurface,
        },
        // nREPL middleware lane (P6): Mova-level API of nrepl.middleware / .transport / .misc.
        EmbeddedModule {
            ns: "nrepl.misc",
            file: "nrepl/misc.mova",
            source: include_str!("../core/lib/nrepl/misc.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.transport",
            file: "nrepl/transport.mova",
            source: include_str!("../core/lib/nrepl/transport.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.middleware",
            file: "nrepl/middleware.mova",
            source: include_str!("../core/lib/nrepl/middleware.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.middleware.print",
            file: "nrepl/middleware/print.mova",
            source: include_str!("../core/lib/nrepl/middleware/print.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.middleware.caught",
            file: "nrepl/middleware/caught.mova",
            source: include_str!("../core/lib/nrepl/middleware/caught.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.middleware.completion",
            file: "nrepl/middleware/completion.mova",
            source: include_str!("../core/lib/nrepl/middleware/completion.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.middleware.lookup",
            file: "nrepl/middleware/lookup.mova",
            source: include_str!("../core/lib/nrepl/middleware/lookup.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.middleware.io",
            file: "nrepl/middleware/io.mova",
            source: include_str!("../core/lib/nrepl/middleware/io.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.middleware.load-file",
            file: "nrepl/middleware/load_file.mova",
            source: include_str!("../core/lib/nrepl/middleware/load_file.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.middleware.interruptible-eval",
            file: "nrepl/middleware/interruptible_eval.mova",
            source: include_str!("../core/lib/nrepl/middleware/interruptible_eval.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.middleware.session",
            file: "nrepl/middleware/session.mova",
            source: include_str!("../core/lib/nrepl/middleware/session.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.server",
            file: "nrepl/server.mova",
            source: include_str!("../core/lib/nrepl/server.mova"),
            provenance: Provenance::NativeSurface,
        },
        EmbeddedModule {
            ns: "nrepl.lane",
            file: "nrepl/lane.mova",
            source: include_str!("../core/lib/nrepl/lane.mova"),
            provenance: Provenance::NativeSurface,
        },
        // SPEC-W3: the two PORTED namespaces (`vendored: false` -- see the
        // field's doc). Order matters only for readability; `require_ns`
        // resolves dependencies itself, and `clojure.spec.alpha` pulls in
        // `clojure.spec.gen.alpha` and `clojure.walk` on its own.
        //
        // `clojure.spec.gen.alpha` reaches test.check through `dynaload`
        // (a `delay` around `require` + `resolve`), so requiring spec pays
        // for the generator backend only when a generator is actually
        // used -- the same laziness this table exists for, one level up.
        EmbeddedModule {
            ns: "clojure.spec.alpha",
            file: "clojure/spec/alpha.mova",
            source: include_str!("../core/lib/clojure/spec/alpha.mova"),
            provenance: Provenance::Ported,
        },
        EmbeddedModule {
            ns: "clojure.spec.gen.alpha",
            file: "clojure/spec/gen/alpha.mova",
            source: include_str!("../core/lib/clojure/spec/gen/alpha.mova"),
            provenance: Provenance::Ported,
        },
        // SPEC-W5: the third ported namespace, and the last of spec's own.
        // `clojure.spec.test.alpha` is spec's TESTING half --
        // `instrument`/`unstrument`/`check` -- which only became portable
        // once W3 landed `s/fdef` and the fspec machinery it instruments.
        // Its patch set (P14-P18) is in `docs/SPEC-PORT-PATCHES.md`; the one
        // engine capability it needed is `builtins::reflect::
        // callstack_native`, which is what lets it name the CALLER of a
        // non-conforming call. Its gate is `tests/spec-smoke/stest-smoke.
        // mova`, oracle-diffed like `smoke.mova`.
        EmbeddedModule {
            ns: "clojure.spec.test.alpha",
            file: "clojure/spec/test/alpha.mova",
            source: include_str!("../core/lib/clojure/spec/test/alpha.mova"),
            provenance: Provenance::Ported,
        },
        // DESIGN-flow-namespace.md Part 1 point 2: `clojure.core.async.flow`'s
        // Clojure-level surface (`process`, `map->step`, `ping`,
        // `ping-proc`) over `src/builtins/flow.rs`'s native engine --
        // `Provenance::NativeSurface`, not vendored or ported (see that
        // variant's doc). Unlike every other row above, this one is loaded
        // EAGERLY too: `eval::Interp::require_core_flow` runs an internal
        // `(require 'clojure.core.async.flow)` through this SAME
        // `find_embedded`/`require_ns` path at bootstrap, right after
        // `register_flow` interns the natives it needs and BEFORE
        // `ns::seed_builtin_namespaces` runs (see that fn's doc for why the
        // order matters) -- so by the time a user's own `require` of this
        // namespace can run, it is already a no-op via the ordinary
        // `loaded` memo, same as every namespace `require` only ever loads
        // once.
        EmbeddedModule {
            ns: "clojure.core.async.flow",
            file: "clojure/core/async/flow.mova",
            source: include_str!("../core/lib/clojure/core/async/flow.mova"),
            provenance: Provenance::NativeSurface,
        },
    ]
}

/// The embedded module providing `ns`, or `None`.
///
/// Linear scan: the table is tiny and this runs at most once per
/// namespace per process (`require_ns`'s `loaded` memo short-circuits
/// every repeat), so a map would cost more to build than it could ever
/// save.
pub fn find_embedded(ns: &str) -> Option<&'static EmbeddedModule> {
    embedded_modules().iter().find(|m| m.ns == ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anti-fork check: every embedded source must still be
    /// byte-identical to the `tests/clojure-suite/vendor-libs/` file it
    /// was copied from -- the copy whose SHA-256 `tools/verify-vendor.sh`
    /// already pins against `MANIFEST-LIBS.sha256`. Chaining the two
    /// gives the embedded copy the same hash lock without duplicating the
    /// manifest.
    ///
    /// SPEC-W3: applies to `Provenance::Vendored` rows only. A ported or
    /// native-surface row is a deliberate divergence with its own gate
    /// (see [`Provenance`]'s doc); the `ported_rows_are_not_in_vendor_libs`
    /// test below is their half of the contract, so no kind of row can be
    /// mislabelled unnoticed.
    #[test]
    fn stdlib_copies_are_byte_identical_to_vendor_libs() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for m in embedded_modules().iter().filter(|m| m.provenance == Provenance::Vendored) {
            let vendored = root.join("tests/clojure-suite/vendor-libs").join(m.file);
            let disk = std::fs::read_to_string(&vendored)
                .unwrap_or_else(|e| panic!("reading {}: {e}", vendored.display()));
            assert_eq!(
                disk, m.source,
                "core/lib/{} has drifted from {} -- embedded stdlib copies are \
                 byte-identical vendored sources, never a fork (see src/stdlib.rs)",
                m.file,
                vendored.display()
            );
        }
    }

    /// Namespace names and file names agree with Clojure's own munging,
    /// so a `--module-path` copy of the same library shadows the embedded
    /// one at exactly the path `ns_file_names` derives.
    #[test]
    fn every_row_file_name_matches_its_namespace() {
        for m in embedded_modules() {
            let stem = m.ns.replace('.', "/").replace('-', "_");
            assert!(
                m.file == format!("{stem}.cljc")
                    || m.file == format!("{stem}.clj")
                    || m.file == format!("{stem}.mova"),
                "{} does not munge to {}",
                m.ns,
                m.file
            );
            assert!(find_embedded(m.ns).is_some());
        }
        // SPEC-W3: the ported namespaces are rows now (W1 asserted the
        // opposite, which is what this line replaces), and so is the
        // `clojure.walk` that `clojure.spec.alpha`'s ns form requires.
        // SPEC-W5 added `clojure.spec.test.alpha`, which used to be THIS
        // test's "not embedded" example.
        for ns in [
            "clojure.spec.alpha",
            "clojure.spec.gen.alpha",
            "clojure.spec.test.alpha",
            "clojure.walk",
            // DESIGN-flow-namespace.md Part 1 point 2: the native-surface row.
            "clojure.core.async.flow",
        ] {
            assert!(find_embedded(ns).is_some(), "{ns} is not embedded");
        }
        // The "not embedded" side of the same check still needs an example.
        // `clojure.pprint` is the honest one now: mova ships a
        // namespace-ONLY stub for it (`core/core.mova`'s C3h note) and the
        // real vendored library is a `tests/clojure-suite` artifact, never a
        // row here -- which is exactly why `clojure.spec.test.alpha`'s port
        // had to drop it from its `ns` form (MOVA-PATCH P14).
        // (nREPL gaps: a mova-authored `clojure.pprint` row now exists; the
        // vendored library is still not it.) `clojure.java.shell` is the example.
        assert!(find_embedded("clojure.pprint").is_some());
        assert!(find_embedded("clojure.java.shell").is_none());
    }

    /// SPEC-W3 / DESIGN-flow-namespace.md, the other half of the
    /// vendored/non-vendored contract: a row flagged `Ported` or
    /// `NativeSurface` must NOT have a `vendor-libs` original. Without
    /// this, mislabelling a genuinely vendored file as either would
    /// silently switch off its byte check.
    #[test]
    fn ported_rows_are_not_in_vendor_libs() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for m in embedded_modules().iter().filter(|m| m.provenance != Provenance::Vendored) {
            let vendored = root.join("tests/clojure-suite/vendor-libs").join(m.file);
            assert!(
                !vendored.exists(),
                "{} is flagged as a mova port/native-surface row but {} exists -- flip \
                 `provenance` to `Vendored` so the byte check covers it",
                m.file,
                vendored.display()
            );
            assert!(
                m.file.ends_with(".mova"),
                "{} is a mova source port or native-surface row and must carry the \
                 .mova extension",
                m.file
            );
        }
    }

    /// SPEC-W3: the ported spec sources carry their patch markers. A
    /// deviation from upstream that is not marked `MOVA-PATCH` cannot be
    /// found again by whoever diffs against a later upstream release, so
    /// the markers are part of the port's contract, not decoration.
    #[test]
    fn ported_spec_sources_carry_their_patch_markers() {
        let alpha = find_embedded("clojure.spec.alpha").unwrap().source;
        let gen = find_embedded("clojure.spec.gen.alpha").unwrap().source;
        // SPEC-W6b: P9 is GONE -- mova's `#()` reader now emits `fn*`,
        // Clojure's own head, so `unfn` is upstream-verbatim again.
        for marker in ["P1", "P3", "P5", "P6", "P12", "P13"] {
            assert!(
                alpha.contains(&format!("MOVA-PATCH {marker}")),
                "alpha.mova lost its {marker} marker -- if the patch was reverted \
                 because the engine grew the feature, drop it from this list AND \
                 from docs/SPEC-PORT-PATCHES.md in the same commit"
            );
        }
        assert!(
            alpha.contains(":refer-clojure :exclude [+ * and assert or cat def keys merge]"),
            "alpha.mova lost its upstream ns form"
        );
        assert!(
            gen.contains("(defmacro ^:skip-wiki lazy-combinators"),
            "gen/alpha.mova lost its dynaload combinator machinery"
        );
        // SPEC-W5: `clojure.spec.test.alpha`'s own five.
        let stest = find_embedded("clojure.spec.test.alpha").unwrap().source;
        for marker in ["P14", "P15", "P16", "P17", "P18"] {
            assert!(
                stest.contains(&format!("MOVA-PATCH {marker}")),
                "spec/test/alpha.mova lost its {marker} marker -- if the patch was \
                 reverted because the engine grew the feature, drop it from this list \
                 AND from docs/SPEC-PORT-PATCHES.md in the same commit"
            );
        }
        // The two things the caller-introspection contract rests on, spelled
        // out so a well-meaning "cleanup" cannot quietly undo either: the
        // live-stack native, and the `::caller` map shape upstream's own
        // `clojure/test_clojure/instr.clj` asserts on.
        assert!(
            stest.contains("(callstack*)"),
            "spec/test/alpha.mova lost its `callstack*` call -- `::caller` cannot be \
             computed without it (see builtins::reflect::callstack_native)"
        );
        assert!(
            stest.contains("{::caller (dissoc caller :class :method)}"),
            "spec/test/alpha.mova lost upstream's ::caller shape"
        );
    }
}
