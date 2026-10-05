# Not portable

Every file in upstream `test/clojure/test_clojure/` that was **not** vendored
into `tests/clojure-suite/vendor/`, with a concrete, factual reason. This is
the named-exclusion ledger required by the project's compatibility guarantee:
the score cannot be inflated by quietly dropping hard files, because every
drop is listed here with the JVM feature it needs.

Source: `test/clojure/test_clojure/*.clj` at tag `clojure-1.13.0-alpha6`
(commit `15c821d7e7df241c8f9fed42fbc51b09c65b67d9`), top-level files only.
Files that live in subdirectories of `test_clojure/` (e.g.
`annotations/java_8.clj`, `pprint/test_pretty.clj`,
`java/io.clj`, `genclass/examples.clj`,
`compilation/*.clj`) are support/fixture files loaded exclusively by one of
the excluded suites below (or, for `pprint/*`, by a suite excluded for its
own reason); they are not independently vendorable and are not listed as
separate rows here.

Three subdirectory files that ARE genuinely `:require`/`:use`d by a
*vendored* top-level file (`protocols/examples.clj`,
`protocols/more_examples.clj`, `repl/example.clj`) are the exception: as of the
suite-runner's companion-namespace materialization
(`tools/clojure-suite-run.bb` now writes every vendored file's assembled
copy under its own `(ns ...)` module path in the scratch dir, not just the
shim), a flattened `*.clj` in `vendor/` genuinely resolves at runtime. They
are vendored under `protocols_examples.clj`, `protocols_more_examples.clj`,
`repl_example.clj` and counted separately (`companion-count: 3` in
`MANIFEST.sha256`'s header) from the 47/20 top-level arithmetic. See
"Companion namespaces, now vendorable" below for what changed and what
remains unresolved.

45 of 67 top-level files were vendored; 22 were excluded.

A SECOND upstream repository contributes three more vendored files, which
sit outside the 67/45/22 arithmetic above and outside this exclusion
ledger entirely: `spec.clj`, `instr.clj` and `multi_spec.clj`, the
OFFICIAL test suite of `clojure.spec.alpha`
(`github.com/clojure/spec.alpha`, commit
`6b698eed1787c14907082c86de3410dd51e816b4`,
`src/test/clojure/clojure/test_clojure/*.clj`). `clojure.spec.alpha` has
its own git repo but SHIPS WITH Clojure — 1.13.0-alpha6 pulls it in as a
dependency — and these three files declare `clojure.test-clojure.*`
namespaces at the very `clojure/test_clojure/` path the primary suite
uses. They are vendored under identical rules (byte-identical,
hash-locked, never hand-edited, flat in `vendor/`) and counted separately
as `spec-alpha-count: 3` in `MANIFEST.sha256`'s header, exactly as the
three companion files are. Nothing from that repo is EXCLUDED — it has
only these three test files and all three are vendored — so this ledger
gains no rows. Added by SPEC-W6b; see `docs/SPEC-ALPHA-CAMPAIGN.md`.

An exclusion is legitimate ONLY when the file cannot be made to pass without
a JVM. "We have not built it yet" is never a valid exclusion reason: if the
gap is a mova feature that is merely unbuilt (not JVM-bound), the file must
be vendored and allowed to fail honestly, so the missing feature shows up as
scoreable signal instead of being hidden from the ledger.

The exclusion table below must contain exactly `excluded-count` rows as
recorded in `MANIFEST.sha256`; a file that is neither vendored nor listed
here is a hole in the ledger.

| file | reason |
|------|--------|
| `agents.clj` | `clojure.core`'s `agent`/`send`/`send-off` are backed by real JVM thread pools (`Executors`), and `agent-error` surfaces JVM `Throwable`s. |
| `annotations.clj` | Tests Java annotation metadata (`^{:tag ...}` → real `@interface` annotations) on `gen-class`-emitted members; no meaning without a JVM classfile. |
| `array_symbols.clj` | Tests Java primitive-array type-hint syntax (`^ints`, `^"[Ljava.lang.String;"`) and array class reflection. |
| `clearing.clj` | Tests JVM locals clearing by reflecting on compiled function objects' fields (`(:import [java.lang.reflect Field])`, `.getDeclaredFields`, `setAccessible`) to observe closed-overs nulled by the JVM compiler; pure JVM-bytecode introspection with no meaning outside a JVM. (Vendored through 2026-08-21 scoring 0/31; excluded by owner ruling.) |
| `clojure_xml.clj` | Requires `clojure.xml`, whose own `ns` form imports `org.xml.sax.{ContentHandler,Attributes,SAXException}` and `javax.xml.parsers.{SAXParser,SAXParserFactory}` and constructs the JVM-only `clojure.lang.XMLHandler`; the namespace cannot load without a JVM SAX stack. (Vendored through 2026-08-21 scoring 0/1 blocked; excluded by owner ruling.) |
| `compilation.clj` | Tests `*compile-path*`, AOT `.class` file emission, and JVM classloading of compiled artifacts. |
| `data_structures_interop.clj` | Tests that Clojure collections implement `java.util.List`/`Map`/`Collection`/`RandomAccess` Java interfaces. |
| `genclass.clj` | `gen-class` compiles and loads real JVM `.class` files; the entire suite is about that pipeline. |
| `generated_all_fi_adapters_in_let.clj` | Imports custom precompiled JVM test classes (`clojure.test.AdapterExerciser$*`) to test Java-8 functional-interface adapter dispatch inside `let`. |
| `generated_functional_adapters_in_def.clj` | Same `AdapterExerciser$*` JVM test classes, for adapters generated at `def` site. |
| `generated_functional_adapters_in_def_requiring_reflection.clj` | Same, for the subset of adapters that require JVM reflection to resolve. |
| `java_interop.clj` | The entire file is Java interop syntax (`.method`, `Class/staticMethod`, `new`, `instance?` on JVM classes, etc). |
| `main.clj` | Tests `clojure.main/eval-opt` and friends: the JVM process entry point, `System`-level stdio wiring, and JVM exit-code behavior. |
| `metadata.clj` | Its top-level `(doseq [ns public-namespaces] (require ns))` runs before any deftest and names six namespaces that are JVM-bound at their own `ns` forms: `clojure.inspector` (Swing/AWT GUI), `clojure.xml` (SAX + `clojure.lang.XMLHandler`), `clojure.java.io` (java.io/java.net stream classes), `clojure.java.browse` (AWT Desktop reflection + `ProcessBuilder`), `clojure.java.javadoc` (transitively browse), `clojure.java.shell` (`Runtime.exec` subprocesses). The file cannot load without them; none of its 53 assertions test those namespaces' behavior. (Vendored through 2026-08-21 scoring 0/53 blocked on the first require; excluded by owner ruling.) |
| `method_thunks.clj` | Tests JVM method-invocation "thunk" optimization (polymorphic inline caches over reflected `Method`/`Constructor` objects). |
| `param_tags.clj` | Imports custom precompiled JVM test classes (`clojure.test.SwissArmy`, `clojure.test.ConcreteClass`) and `clojure.lang.Compiler`/`Compiler$CompilerException` internals for param-tag reflection metadata. |
| `pprint.clj` | Loads un-vendored subdirectory files via classpath-relative `(load "pprint/test_cl_format")`, and `clojure.pprint`'s formatting is built on JVM `java.io.Writer`. |
| `reducers.clj` | `:require`s two unvendored external libraries (`clojure.test.generative`, `clojure.data.generators`) at namespace load, so the namespace cannot load standalone in this harness. This is a dependency-resolution limitation, not an established JVM-boundness claim -- see "Borderline" below for the full analysis of why the previously-stated `ForkJoinPool` reason does not clean up under the sharpened rule either. |
| `reflect.clj` | The entire file is the `clojure.reflect` JVM reflection API. |
| `serialization.clj` | Tests `java.io.ObjectOutputStream`/`Serializable`, JVM object serialization. |
| `server.clj` | Tests `clojure.core.server`, a `java.net.Socket`-based socket REPL. |
| `streams.clj` | Tests interop with `java.util.stream.{Stream,LongStream}` and `java.util.function.{Consumer,Predicate,Supplier}`. |

## Previously excluded, now vendored

These three files were previously excluded with reasons that did not
actually name a JVM feature the file requires -- the real reason was that
mova has not built the corresponding capability yet, which the sharpened
rule above no longer accepts as an exclusion reason. They are now vendored
in `tests/clojure-suite/vendor/` and score honestly; they are EXPECTED to
fail heavily today, and that failure is the intended signal for the
milestones that own them.

| file | reason it WAS excluded | why that reason failed the naming-a-JVM-feature rule | milestone that owns it |
|------|------------------------|--------------------------------------------------------|-------------------------|
| `numbers.clj` | "mova has no ratio type ... already a documented deviation area" | Not a JVM-feature claim -- it names a gap in mova's own numeric tower (no `Ratio`, no exact `BigInteger`/`BigDecimal` promotion), which is planned work, not a JVM boundary. | **DONE in S5** (SPEC-numtower): Ratio/BigInt/BigInteger/BigDecimal arithmetic, checked-overflow throws, the `'`-suffixed promoting family and the `unchecked-*` family all landed. `numbers.clj` is still `:blocked` in the scoreboard, but for a DIFFERENT reason: its `ns` form requires `clojure.test.generative`, `clojure.data.generators` and `clojure.template`, none of which mova has. With those stubbed, the file runs 44 tests / 892 assertions with 811 passing -- see `compat/numtower-numbersclj-probe.py`. |
| `protocols.clj` | "`defprotocol`/`deftype` compile to real JVM interfaces/classes" | True only for the *compilation strategy* upstream Clojure happens to use. The tests in this file are overwhelmingly about protocol/deftype/defrecord SEMANTICS (dispatch, extend, satisfies?, equality, hashing), which are portable and just require mova to have its own protocol implementation, not a JVM one. | M7 (polymorphism: protocols/records/deftype) in `CLOJURE-COMPAT-PLAN.md` |
| `delays.clj` | "`CyclicBarrier` + `java.util.function` interop" | Only 3 of the file's test forms touch that JVM interop (`CyclicBarrier`-based thread-race control in two tests, `java.util.function.{Supplier,...}` in one); the rest of the 88-line file is ordinary `delay`/`force` semantics (realization-once, laziness, exception caching, `realized?`) with no JVM dependency at all. | Ordinary `delay`/`force` semantics -- not tied to a specific milestone; the isolated `java.util.concurrent`/`java.util.function` interop subset within it is expected to keep failing (or be individually flagged later) since that part genuinely is JVM-bound. |

## Notes on borderline files that WERE vendored

A few vendored files contain isolated JVM-interop assertions inside an
otherwise-portable test suite (e.g. `atoms.clj` checks `instance? on
java.util.function.Supplier`, `fn.clj` checks `ExceptionInfo` cause
messages). These were kept rather than excluded: excluding a whole file
because of one interop `is` form would hide real, scoreable pass/fail
signal on the rest of the file. The scorer counts assertions, not files, so
a handful of unavoidably-JVM assertions inside a portable file just show up
as failed/errored assertions, not a missing file.

## Companion namespaces, now vendorable

Some vendored files reference a companion namespace that lives in another
*vendored* file (e.g. `ns_libs.clj` requires
`clojure.test-clojure.ns-libs-load-later`, and `run_single_test.clj`
requires `clojure.test-clojure.test-fixtures`). As of the
companion-namespace materialization added to
`tools/clojure-suite-run.bb` (every vendored file is additionally written
to the scratch dir under the module path its own `(ns ...)` form implies,
with the `clojure.test`/`clojure.test-helper` shims spliced after that
`ns` form, same placement rule as the primary per-file run), these
cross-file `:require`s now genuinely resolve at runtime -- there is no
remaining harness-architecture limitation for this case. Verified
concretely: `run_single_test.clj` was `:blocked` (`could not locate
namespace clojure.test-clojure.test-fixtures`) and is now `:ok` (3 tests
run, all erroring on a *different*, genuine gap -- see below), with every
other vendored file's scoreboard row unchanged.

`protocols.clj`'s two subdirectory companions
(`clojure.test-clojure.protocols.examples`,
`...protocols.more-examples`, upstream `protocols/examples.clj` and
`protocols/more_examples.clj`) are likewise now vendored, flattened into
`vendor/protocols_examples.clj` and `vendor/protocols_more_examples.clj`
(the runner derives each file's module path from its own `ns` form, not
its filename, so the flattening is cosmetic). Verified concretely too:
before this fix, `protocols.clj` blocked immediately at its own `ns` form
(`could not locate namespace
clojure.test-clojure.protocols.more-examples`); after, it loads past that
`ns` form and over 700 lines further into the file before hitting a
*different*, genuine gap: a top-level `(defrecord MapEntry [k v]
java.util.Map$Entry ...)` at line 751 that implements a JVM interface --
squarely a language-feature gap (mova has no `java.util.Map$Entry`, and
`defrecord` implementing arbitrary JVM interfaces is JVM-interop
territory), not a runner/harness issue, so `protocols.clj` legitimately
stayed `:blocked` (0/196 oracle assertions credited) and was not
re-added to the exclusion table -- this is the intended honest signal
described at the top of this document.

**Resolved in S5** (`definterface` + named-interface impls). That
question -- "whether that specific defrecord form could be special-cased
(e.g. treating `java.util.Map$Entry` as an opaque marker interface)" --
was answered by BUILDING the general feature rather than special-casing
the form: mova now has `definterface`, a `TypeDef` records the interface
names its `defrecord`/`deftype` body declares, `instance?` against an
interface asks that list, and `.method` interop dispatches to the
declared impls (`src/types.rs`, `src/builtins/types.rs`,
`src/eval/types_forms.rs`; conformance rows in
`tests/conformance/corpus/interfaces.corpus`, goldens from the real
oracle). `java.util.Map$Entry` is a one-row host-interface table entry,
not a special case in `defrecord`.

Two further walls fell behind it, both recorded here because each was a
genuine gap and not a harness artifact:

1. `(:use ...)` was a tolerated-ignored `ns` clause, so `protocols.clj`'s
   `(:use clojure.test clojure.test-clojure.protocols.examples)` never
   bound `ExampleProtocol`/`foo`/`bar`/`baz`. `:use` is now `require` +
   `refer` (`crate::ns::Interp::use_spec_value`), still silently skipping
   any namespace it cannot locate -- that tolerance is load-bearing, since
   several vendored files `:use` namespaces (`clojure.test.generative`,
   `clojure.template`, ...) that genuinely do not exist on mova's module
   path, and making `use` strict would turn those files from "runs, scores
   honestly" into `:blocked`.
2. A HARNESS bug the above exposed: `protocols.clj` switches namespace
   part-way through (a mid-file `(ns clojure.test-clojure.protocols.other
   (:use clojure.test))` at line 699, after 23 of its 25 `deftest` forms),
   so the runner's appended `(run-tests)` ran in the *wrong* namespace and
   saw only the last 2 tests. `tools/clojure-suite-run.bb` now re-enters
   the file's own declared namespace before `(run-tests)`. This ALIGNS the
   numerator with the denominator rather than diverging from it:
   `tools/oracle-census.clj` runs `(clojure.test/run-tests ns-sym)` on the
   file's declared namespace, which is exactly why
   `ORACLE-ASSERTIONS.edn` records **23** deftests for `protocols.clj` and
   not the 25 `deftest` forms its text contains.

`protocols.clj` is now `:ok` at 71/196 oracle assertions (23 deftests
run, 6 passing); `protocols_examples.clj` is `:ok` (it defines no tests of
its own -- it is a companion namespace, and its value is that
`protocols.clj` can now `:use` it). Its remaining blockers are `reify`
(unimplemented; `reify-test` plus 2 smaller tests) and the JVM
collection-interop method surface on records (`.size`, `.equals`,
`.containsValue`, `.entrySet`, ... -- `defrecord-interfaces-test`), the
latter being the same `java.util.Map`-implementing-collections territory
that `data_structures_interop.clj` is excluded for above.

A third companion, `repl.clj`'s `clojure.test-clojure.repl.example`
(upstream `repl/example.clj`), was vendored the same way as
`vendor/repl_example.clj` even though `repl.clj` was already `:ok`
(pulled in via `:use`, which is tolerated-ignored, not `:require`) --
included for completeness per the "grep every vendored file for a
`clojure.test-clojure.*` reference" sweep this fix was built on, not
because it was blocking anything.

`run_single_test.clj`'s remaining 3/3 errors, now that the require itself
resolves, are two DIFFERENT, already-documented, pre-existing gaps, not
new ones introduced by this fix: `with-err-string-writer` (used by 2 of
the 3 tests) is explicitly disclosed as unimplemented in
`mova-test-helper-shim.mova`'s own header (no `*err*` dynamic var in
mova), and `run-test` (singular -- run one named test var, as opposed to
the shim's `run-tests`) is simply not a function the shim defines. Both
are shim-completeness gaps for a future session, not runner bugs; recorded
here rather than chased further in this fix.

## Borderline -- flagged for owner decision

Re-auditing the remaining table against the sharpened rule above turned up
two rows whose *stated* reason is not clean under "cannot pass without a
JVM." Left in the exclusion table for now (excluding either changes the
authoritative counts), but flagged here rather than silently resolved:

- **`reducers.clj`**: the given reason bundles two claims. (1) "`:require`s
  `clojure.test.generative` and `clojure.data.generators`, external
  JVM-only test libraries not part of core and not vendored" -- these two
  libraries are ordinary Clojure code (a generative-testing/QuickCheck-style
  library and a random-data generator), not JVM-bound; calling them
  "JVM-only" mischaracterizes what's actually true, which is just that
  they're external dependencies mova/this harness hasn't vendored, i.e. an
  unbuilt/unvendored-dependency reason, not a JVM one. (2) "`fold` is built
  on `java.util.concurrent.ForkJoinPool`" is a real JVM fact about
  `clojure.core.reducers/fold` in general, but it doesn't describe what this
  file actually exercises: of its 9 test forms, 6 use only `r/map`/`r/mapcat`/
  `r/filter`/`r/reduce`/`r/take` (plain sequential reducer semantics, no
  forking), and the 2 `r/fold` call sites use `nil` and a hash-map as input
  (`(r/fold + nil)`, `(r/fold f (zipmap ...))`), neither of which is a vector
  large enough to hit the actual `ForkJoinPool` code path in the default
  `CollFold` implementation. So on inspection, most of this file does not
  clearly require a JVM to pass; the honest exclusion reason, if any, would
  need to be "requires two unvendored external libraries whose `:require`
  keeps the whole namespace from loading" -- a harness/dependency-resolution
  limitation like the `protocols.clj` note above, not a JVM-boundness claim.
- **`pprint.clj`**: the stated reason ("loads un-vendored subdirectory
  files... built on JVM `java.io.Writer`") is true as written, but the file
  itself is a 20-line stub containing zero test forms of its own -- both
  `(load "pprint/test_cl_format")` and `(load "pprint/test_pretty")` are the
  entire body. Vendoring it alone (even under the same-suite-companion
  precedent applied to `protocols.clj` above) would add no scoreable
  assertions, since the file has none outside the two failing subdirectory
  loads. The `java.io.Writer` half of the reason is arguably also an
  unbuilt-feature claim (mova could in principle grow its own writer
  abstraction) rather than a hard JVM boundary. Recommend the owner either
  reword this row to state the real reason (zero-content loader stub,
  harness-architecture limitation, not JVM-boundness) or fold it into the
  precedent note instead of the exclusion table, since vendoring it produces
  no signal either way.
