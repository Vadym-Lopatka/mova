# `core/lib/` — the embedded stdlib module sources

Every file under this directory is compiled into the mova binary with
`include_str!` and made `require`-able with **no `--module-path`** through
the embedded stdlib module table, `src/stdlib.rs`. Loading goes through the
same `ns::Interp::require_ns` path every namespace uses -- same `loaded`
memo, same cycle detection, same `*ns*` save/restore -- so nothing here is
read, parsed or evaluated until something actually `require`s the
namespace. Contrast `core/core.mova` and `core/async.mova`, which are NOT
rows in this table: they are evaluated eagerly at `Interp::new`, before any
namespace machinery exists to require them.
`clojure/core/async/flow.mova` (below) is the one row this table's own
`Interp::new` bootstrap ALSO reaches, but through this SAME lazy
`require_ns` path -- an ordinary internal `(require
'clojure.core.async.flow)`, not a snowflake loader (DESIGN-flow-namespace.md
Part 1 point 2) -- so a later, user-written `(require
'clojure.core.async.flow)` is just the `loaded` memo's ordinary no-op.

**Disk wins.** `ns::Interp::require_ns` consults the module path first and
only falls back to this table on a miss, so a `--module-path` copy always
overrides an embedded namespace.

**One exception: namespaces the bootstrap already marked loaded.**
`require_ns`'s first line is a `loaded`-memo check that returns immediately
on a hit, before the module path is ever consulted. `Interp::new`'s
bootstrap eagerly requires/installs `clojure.core.async.flow`,
`clojure.core.async`, and (as of this fix) `clojure.repl` -- all three are
already in the `loaded` memo before a single user form runs, so a later
`require` of any of them (whether from user code or a `--module-path` file
of the same name) hits that first check and never reaches the disk-vs-
embedded fallback at all. Disk does NOT win here, on purpose: these three
are engine-owned language surfaces, not libraries a module-path file
should be able to shadow -- and it's exactly what makes a stale app-side
bridge file (an app's own pre-upgrade `flow.mova`/`async.mova`/`repl.mova`
sitting on its module path) harmless after an upgrade: it's never read.

## Three kinds of file, and only three

Every file here is vendored, ported, or a native surface, and
`src/stdlib.rs`'s `EmbeddedModule::provenance` (a `Provenance` enum) records
which. Nothing else is allowed: `ported_rows_are_not_in_vendor_libs` fails
the build if a file that exists
in `vendor-libs/` is flagged as a port.

### Vendored — no hand edits, ever

Vendored files (`clojure/test/check/*`, `clojure/walk.clj`) are
**byte-identical copies** of `tests/clojure-suite/vendor-libs/`, whose
SHA-256 hashes are pinned in `tests/clojure-suite/MANIFEST-LIBS.sha256`
and re-checked by `tools/verify-vendor.sh`. `src/stdlib.rs`'s
`stdlib_copies_are_byte_identical_to_vendor_libs` test re-compares each
embedded source against its `vendor-libs` original on every `cargo test`,
so the two can never silently diverge and these files can never quietly
become a fork.

To update a vendored library: update `tests/clojure-suite/vendor-libs/`
and its manifest first (that is the source of record), then copy the file
here.

### Ported — a ledgered mova source port

`clojure/spec/alpha.mova` and `clojure/spec/gen/alpha.mova` are **source
ports**: the upstream `.clj` copied verbatim and then patched in place, so
a future upstream diff stays tractable. Every deviation is marked
`MOVA-PATCH P<n>` in the source and listed in
`docs/SPEC-PORT-PATCHES.md`. They have no `vendor-libs` original, so the
byte check does not apply; their gate is behavioural instead —
`tests/spec-smoke/smoke.mova`, executed by both mova and real Clojure
1.13.0-alpha6, whose stdout must be byte-identical (see
`tests/spec-smoke/RUNNING.md`).

### Native surface — a Clojure-level face on a Rust-native engine

`clojure/core/async/flow.mova` (DESIGN-flow-namespace.md Part 1) is
`clojure.core.async.flow`'s Clojure-level surface (`process`, `map->step`,
`ping`, `ping-proc`) over `src/builtins/flow.rs`'s native proc-graph
engine, which interns every `flow/`-prefixed native directly into this
namespace. Neither a copy of an upstream file nor a patched port of one --
mova implements only the subset of upstream `core.async.flow`'s surface its
native engine needs, so there is no single upstream file this "diverges"
from -- so neither the byte check nor the `MOVA-PATCH` ledger applies. Its
gate is `tests/flow_ns_test.rs` plus the flow-gold conformance suite. The
`flow` spelling (`flow/create-flow`, `::flow/report`) that the whole
existing test/bench corpus uses keeps working through the engine-owned
default alias, `ns.rs`'s `DEFAULT_ALIASES` -- not through a second copy of
any of these vars.

## Contents

### `org.clojure/test.check` 1.1.3

Unzipped from `~/.m2/repository/org/clojure/test.check/1.1.3/test.check-1.1.3.jar`
into `tests/clojure-suite/vendor-libs/` (see `MANIFEST-LIBS.sha256`'s own
provenance header), copied here unchanged.

| file | namespace |
| --- | --- |
| `clojure/test/check.cljc` | `clojure.test.check` |
| `clojure/test/check/generators.cljc` | `clojure.test.check.generators` |
| `clojure/test/check/properties.cljc` | `clojure.test.check.properties` |
| `clojure/test/check/rose_tree.cljc` | `clojure.test.check.rose-tree` |
| `clojure/test/check/results.cljc` | `clojure.test.check.results` |
| `clojure/test/check/impl.cljc` | `clojure.test.check.impl` |

**`clojure/test/check/random.clj` was embedded here until SPEC-W6a and no
longer is.** `clojure.test.check.random` is now a **Rust-native veneer**
(`src/splitrandom.rs` + `src/builtins/tcrandom.rs`) — a bit-exact
transcription of upstream's splitmix64, because test.check splits the RNG
once per generated element and the interpreted `deftype` made that the
entire cost of generative testing (10 000 `split` + `rand-long`: 1.90 s
interpreted). A native veneer's namespace is marked *loaded* at
`Interp::new` by `ns::seed_builtin_namespaces`, so `require` of it returns
before the module path or this table is consulted — **which means the
"disk wins" rule above does NOT hold for this one namespace**, and an
embedded row for it could never have been selected. The other five
test.check files still load from source exactly as before and still call
`random/split` etc. verbatim; only what those calls resolve to changed.

**Deliberately NOT embedded**, though present in the same jar and in
`vendor-libs/`:

- `clojure/test/check/clojure_test.cljc` (`clojure.test.check.clojure-test`)
- `clojure/test/check/clojure_test/assertions.cljc`

Both are `clojure.test` integration (`defspec`, the `is` assertion hook)
and both `(:require [clojure.test ...])` — a namespace mova does not
provide. The clojure-suite runner materializes a hand-written
`clojure.test` shim for its own scoring, which is a test-harness artifact
with no business inside a shipped binary. Nothing in `clojure.spec.alpha`
touches either file.

Also skipped upstream (cljs-targeted, never reached under `:clj`):
`random.cljs`, `random/doubles.cljs`, `random/longs.cljs`,
`random/longs/bit_count_impl.cljs`, `clojure_test/assertions/cljs.cljc`.

### `clojure.walk` (SPEC-W3)

| file | namespace |
| --- | --- |
| `clojure/walk.clj` | `clojure.walk` |

Same provenance as the test.check files: a byte-identical copy of
`tests/clojure-suite/vendor-libs/clojure/walk.clj`, hash-pinned in
`MANIFEST-LIBS.sha256` (`98ab54d0…`). It is here because
`clojure.spec.alpha`'s own `ns` form opens with `(:require [clojure.walk
:as walk])`, so without it `(require '[clojure.spec.alpha :as s])` cannot
work on a bare binary. mova has no native `clojure.walk` (probed:
`(require 'clojure.walk)` → "could not locate namespace"), and writing one
would fork a 130-line pure-Clojure file that already loads unmodified —
W1's `.clj` extension support is what lets it keep the upstream name.

### `org.clojure/spec.alpha` (SPEC-W3) — **ported, not vendored**

| file | namespace |
| --- | --- |
| `clojure/spec/alpha.mova` | `clojure.spec.alpha` |
| `clojure/spec/gen/alpha.mova` | `clojure.spec.gen.alpha` |

Ported from `spec.alpha`'s `src/main/clojure/clojure/spec/alpha.clj` and
`clojure/spec/gen/alpha.clj` (the 0.6.250 line). See "Ported" above and
`docs/SPEC-PORT-PATCHES.md`. `clojure.spec.gen.alpha` reaches the
generator backend through `dynaload` (a `delay` around `require` +
`resolve`), so requiring spec pays for test.check only when a generator is
actually used.

`clojure.spec.test.alpha` is **not** ported yet — a later wave.

### `clojure.core.async.flow` (DESIGN-flow-namespace.md) — **native surface, not vendored or ported**

| file | namespace |
| --- | --- |
| `clojure/core/async/flow.mova` | `clojure.core.async.flow` |

The only row this repo's own bootstrap ALSO reaches (via `eval::Interp::
require_core_flow`'s internal `(require 'clojure.core.async.flow)`) --
see "Why this exists" above. `process`/`map->step`/`ping`/`ping-proc` over
`src/builtins/flow.rs`'s native proc-graph engine, which interns every
`flow/`-prefixed native directly into this namespace (`reg_flow`). See
"Native surface" above for why neither the byte check nor the
`MOVA-PATCH` ledger applies.

## Known warnings

Loading `clojure.test.check` emits boxed-math warnings from
`rose_tree.cljc` (SPEC-W6a: `random.clj`'s twelve went away with the file
itself), and `WARNING: <name> already refers to ...` lines
from `rose-tree`/`generators` shadowing core names. Both are the same
diagnostics real Clojure prints for the same source; neither affects
behavior.
