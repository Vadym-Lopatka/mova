# mova-test-shim: known limits

Blunt list of what `mova-test-shim.mova` cannot do faithfully, what it does
instead, and why that is (or isn't) safe to build a compatibility score on.
If you're consuming `#RESULT`/`#SUMMARY` lines from this shim, read this
first -- a scoreboard built on a shim that silently passes things is
worthless.

## `thrown?` / `thrown-with-msg?` / `thrown-with-cause-msg?` / `fails-with-cause?` -- CLASS-AWARE as of C3g

**Historical context (S3-S7), superseded below:** mova's `try` used to
support exactly one **untyped** `catch` clause (confirmed at the time:
`(try ... (catch e ...) (catch f ...))` errored with "only one catch
clause is supported"). There was no way to distinguish "an exception of
class C whose message matches re" from "literally anything else got
thrown, including a bug in the shim itself" -- every form in this family
was therefore CLASS-BLIND: `C` was embedded only as inert quoted data in
failure messages, never evaluated or checked, and a wrong-class throw
false-passed exactly like a right-class one. `thrown-with-msg?`/
`thrown-with-cause-msg?`/`fails-with-cause?` additionally checked `re`
against `(ex-message e)` whenever that was a non-nil string (true for
`ex-info`-created exceptions, confirmed by probe), and passed
unconditionally wherever it was `nil` (most other throws -- a plain
string throw, an internal arithmetic error) -- a second, message-blind
false-pass family layered on top of the class-blind one. `thrown?` had
no message check at all: any throw was a pass, full stop.

**C3g changes the premise this whole section was built on**: `try` now
supports any number of TYPED `catch` clauses, matched by class at
throw time (`eval::special_forms::catch_class_matches`, engine-side --
see COMPATIBILITY.md / the C3g landing commit for the ErrorKind-to-class
mapping table and its oracle evidence). All four `is` heads now splice
their `C` argument straight into a real `(catch ~klass e# ...)` (see
`mova-test-shim.mova`'s own comments on the `thrown?` and
`thrown-with-msg?` branches), so:

- **`thrown?`**: `expr` is evaluated inside a `try` typed to `C`. If it
  does not throw, `:fail` ("did not throw"), unchanged. If it throws
  something of a DIFFERENT class than `C`, the typed catch does not fire
  -- the exception propagates past the whole `is` expansion into the
  OUTER untyped catch, which reports `:error` ("unexpected exception"),
  never a false `:pass`. Only a throw whose class genuinely matches `C`
  reaches the `:pass` branch. This is the headline fix: a `thrown?` pass
  is now real evidence of the documented class, not merely "didn't return
  normally".
- **`thrown-with-msg?`**: same class gate as `thrown?`, applied FIRST;
  only once inside a class-matched catch does the message check run
  (`ex-message` + `re-find` when accessible, unconditional pass on any
  throw of the MATCHED class where `ex-message` is `nil` -- the one
  remaining disclosed approximation, narrower than before because it now
  additionally requires the class to match).
- **`thrown-with-cause-msg?` / `fails-with-cause?`**: W-ERR (field2) added
  a real `ex-cause` to `core.mova` (mirrors `ex-message`'s polymorphic
  `ex-info`/host-`Throwable` pattern; `.getCause` already worked on a host
  `Throwable`, `ex-cause` was the missing piece), so mova now genuinely
  HAS a cause chain to walk -- but these two shim heads were deliberately
  NOT upgraded to use it this wave (upgrading them changes census
  accounting, out of scope here; flagged as a follow-up opportunity for
  whichever wave next touches the shim). Both forms still check `C`
  against the exception ACTUALLY CAUGHT rather than against a would-be
  cause -- a disclosed, deliberate approximation, not a full port of
  upstream's cause-walking `defmethod assert-expr`. This is either
  coincidentally exact (mova never wraps one exception inside another, so
  "the caught exception" and "its would-be cause" are the same thing
  here) or a narrower miss than the old class-blind behavior (previously
  blind on both the top exception and any cause; now checked against the
  top exception only) -- never a NEW false pass relative to before. The
  message check is otherwise identical to `thrown-with-msg?`'s.

**What is NOT fixed**: the message-blind fallback (pass on any throw of
the matched class when `ex-message` returns `nil`) is unchanged and still
a disclosed false-pass family, narrower than the pre-C3g one but real.
`causes` (the standalone helper upstream exports, which walks
`.getCause`) is still **not exported** by `mova-test-helper-shim.mova` --
as of W-ERR (field2) mova's `core.mova` DOES have `ex-cause` to walk with
now (see above), so a real, non-fake `causes` port is now POSSIBLE; it
was deliberately left unexported this wave (same "shim upgrades are
out of scope, report as follow-up" reasoning as `thrown-with-cause-msg?`
above), not because there is still nothing to walk it with. No vendored
file calls `causes` directly (verified by grep), so this remains
low-priority.

**Net effect on scoring**: a pass from any of these four heads should now
be read as "the documented class was thrown, and where a message was
recoverable it matched" -- a materially stronger claim than "something
was thrown". This is a HONESTY UPGRADE, not a pure score improvement: it
can and does FLIP some previously-passing assertions to `:error` wherever
the vendored test threw a class other than the one it documented and the
old class-blind `catch` was silently papering over that mismatch. See the
C3g landing commit's per-file delta table for exactly which files moved,
and `vendor/test.clj`'s own "Should error"/"Wrong class of exception"
assertions (previously counted as 2 of that file's 6 disclosed misses,
see "`test-ns-hook`" below) for the worked example this section used to
cite as the state of the art in false-passing.

**MEASURED delta** (worktree baseline vs. this landing, `tools/check-regression.sh`,
2026-08-21): net **-96 assertions-passed** across 15 regressed files, 1
improved (`test.clj` +2, exactly the `can-test-thrown`/`can-test-thrown-with-msg`
"Should error" rows this section's earlier draft predicted), 35 unchanged,
0 new/vanished files, global attempted-and-passed 8979 -> 8997/10508 (two
message-sniffed `ErrorKind::Other` refinements -- `IndexOutOfBoundsException`/
`StringIndexOutOfBoundsException` for "out of bounds" messages, and
`UnsupportedOperationException` for record-mutation attempts, both landed
alongside the engine change -- recovered 18 of an initial -114 before
those were added). Every regressed file was individually spot-checked
against what mova ACTUALLY throws for the specific call in question
(via the live binary, not guessed): every flip found is a class or
message mismatch between what the vendored test documents (a specific
JVM exception the assertion's author measured against real Clojure) and
what mova's own coarser internal error taxonomy can currently express --
never a bug in `catch_class_matches`/`catch`'s dispatch itself. Confirmed
patterns, worst-to-best understood:

- **`fn.clj` (8 -> 0), `def.clj` (9 -> 3)**: both check
  `clojure.lang.ExceptionInfo` against `(eval '(fn ...))`/`(defn ...)`
  forms whose arglist real Clojure rejects via `clojure.spec` at macro-
  expansion time. mova has no `clojure.spec`-equivalent arglist
  validation for `fn`/`defn` at all (confirmed: these forms simply don't
  throw ExceptionInfo under mova, if they throw anything) -- was ALWAYS
  a mova gap, invisible only because the old class-blind catch turned
  "threw SOMETHING" into a false pass regardless of what.
- **`try_catch.clj` (4 -> 1)**: 3 of its 5 assertions name
  `ReflectorTryCatchFixture$Cookies`, a real Java test-fixture class
  needing genuine JVM reflection mova does not have; unreachable by
  design, same false-pass-then-honest-error pattern as above (verified:
  the 2 assertions unaffected by typed `catch` -- a plain `(catch
  java.lang.Throwable t t)` inside a user `defn`, never routed through
  the shim's class-aware heads -- kept their PRE-EXISTING oracle mismatch
  unchanged, confirming this file's delta is 100% attributable to the
  three `thrown-with-msg?` heads).
- **`data_structures.clj` (1973 -> 1967)**: mixed -- some (`(pop ())`
  expecting `IllegalStateException`) are a genuinely un-sniffed mova
  condition (`ErrorKind::Other`, message "pop: can't pop empty list",
  no stable substring worth a dedicated sniff for 2 occurrences); others
  (`(get-in {:a 1} 5)` expecting `IllegalArgumentException`) reveal that
  `ErrorKind::TypeErr`'s single mapping to `ClassCastException` is too
  coarse for every call site that produces it -- real Clojure sometimes
  wants `ClassCastException` and sometimes `IllegalArgumentException`
  from what mova collapses into one kind. Splitting `TypeErr` further is
  a real error-taxonomy project (tracked as "needs error taxonomy" in
  `tests/pending_conformance_test.rs`'s own ERR-BOTH bucket), out of this
  task's scope.
- **`numbers.clj`, `multimethods.clj`, `ns_libs.clj`, `other_functions.clj`,
  `protocols.clj` (partial), `repl.clj`, `sequences.clj` (partial),
  `special.clj`, `string.clj`, `transients.clj`, `vectors.clj` (partial)**:
  same two root causes recur -- (a) a class this corpus needs that mova's
  internal errors don't distinguish from a broader sibling (mostly
  `IllegalArgumentException` vs. `ClassCastException` vs. plain
  `RuntimeException`, and `NullPointerException` specifically, which
  mova's "expected X, got nil" `TypeErr` wording is used far too broadly
  to sniff without guessing -- see `string.clj`'s `nil-handling` `are`
  block, the single largest remaining contributor), or (b) a real Clojure
  feature mova doesn't have at all (`clojure.lang.Compiler$CompilerException`
  wrapping compile-time spec failures, `java.io.FileNotFoundException`
  from real file I/O, JVM reflection). None of these are catch-dispatch
  bugs; all are pre-existing, now-visible mova capability gaps.
- **`protocols.clj`, `sequences.clj`, `vectors.clj`, `other_functions.clj`
  (recovered portion)**: the two message-sniffed refinements above fixed
  the `UnsupportedOperationException`-on-record-mutation and
  `IndexOutOfBoundsException`-on-vector/array-index assertions in these
  files for real -- confirmed via the live binary, not assumed from the
  message pattern alone.

This delta was reported to the conductor for review, not auto-blessed:
`BASELINE.edn` was NOT re-blessed as part of this landing (`tools/check-
regression.sh` still reports `REGRESSION DETECTED` against the pre-C3g
baseline) -- whether -96 is an acceptable price for closing the larger
dishonesty this section spent years describing is an owner call, not an
engineering one.

## `clojure.test-helper` -- what's real, what's undefined, and why

`mova-test-helper-shim.mova`, materialized as `clojure/test_helper.mova`
by `tools/clojure-suite-run.bb` (mirroring how `mova-test-shim.mova`
becomes `clojure/test.mova`) and also spliced into the assembled temp file
right after the vendored file's own `ns` form (same reasoning as the
`clojure.test` shim -- see that script's module doc), so vendored files
`(:require [clojure.test-helper ...])` or `(:use clojure.test-helper)`.

**Implemented, and how (each individually probed against a live mova
before being written -- see the file's own header for the exact probes):**

- `platform-newlines` -- **identity**, not an approximation. Upstream
  replaces `"\n"` with `(System/getProperty "line.separator")`. mova has
  neither `System/getProperty` nor string `.replace` interop (both
  confirmed unresolvable), but the vendored suite's own oracle is
  hard-pinned to a real JVM on THIS machine (macOS), and `line.separator`
  is `"\n"` on every JVM on macOS/Linux (only Windows reports `"\r\n"`).
  Replacing `"\n"` with `"\n"` IS the identity function on this platform,
  exactly, not a stand-in for it.
- `exception` -- `(throw "Exception which should never occur")`. No
  `Exception.` constructor exists in mova (confirmed unresolvable), so
  this throws a plain string instead of a typed object; every catcher in
  this corpus is untyped anyway (mova's `try` has exactly one untyped
  `catch`), so nothing observable is lost.
- `with-var-roots` -- a **macro**, built on `with-redefs` (landed M4b).
  Upstream's `with-var-roots*`/`set-var-roots` (the function versions)
  need `alter-var-root`, which mova doesn't have; mova's `set!` (which
  `with-redefs` itself is presumably built on) requires a LITERAL symbol
  at the call site, confirmed by probe (passing a `Var` value through a
  helper fn and calling `(set! v newval)` there errors "Unable to resolve
  symbol: v" -- it wants a lexical name, not a runtime value), so there is
  no way to recover "the symbol to redefine" from an arbitrary runtime
  `Var`. But `with-var-roots` itself is a macro, and its `root-map`
  argument is UNEVALUATED DATA at the call site -- confirmed by probe that
  a map literal `{#'foo 2}` reads, before evaluation, as `{(var foo) 2}`,
  i.e. the key is the literal form `(var foo)`, not a runtime `Var`. Every
  real usage in this corpus (and upstream) writes its root-map as exactly
  such a literal (`(with-var-roots {#'clojure.core/global-hierarchy
  (make-hierarchy)} ...)` in `multimethods.clj`), so the symbol IS
  available at macro-expansion time even though it isn't at runtime. The
  macro walks the map's keys, requires each to be literally `(var sym)`,
  and rewrites the call as one `with-redefs` over the flattened bindings.
  A root-map that is NOT a literal map at the call site (e.g. built at
  runtime and passed through a variable) is rejected with a loud
  macro-expansion-time error naming the offending key, rather than
  silently doing nothing -- no vendored usage needs that shape.

**NOT implemented -- left undefined, not stubbed, so a vendored file using
one of these errors honestly (unresolved symbol, at the point it's
actually called) rather than silently no-op'ing or false-passing:**

- `with-var-roots*`, `set-var-roots` -- the function versions genuinely
  need a runtime `Var` -> symbol-name recovery mova cannot do (see
  `with-var-roots` above); `intern`, which could have sidestepped this,
  also does not exist (confirmed unresolvable).
- `causes` -- needs `.getCause`/`ex-cause`; `ex-cause` now exists in
  `core.mova` as of W-ERR (field2), so this is no longer a hard
  "unresolvable" block, but `causes` itself is still not exported here --
  see the `fails-with-cause?` section above for why (out of scope this
  wave, flagged as a follow-up).
- `get-field` -- reflection (`.getDeclaredField`/`.setAccessible`); mova
  has no reflection at all.
- `temp-ns`, `eval-in-temp-ns` -- both need `in-ns` to switch namespaces
  at runtime, which does not exist in mova (confirmed: "Unable to resolve
  symbol: in-ns").
- `should-print-err-message`, `should-not-reflect` -- provided (wave-C
  small sweep item 3, and C3a respectively), but honest approximations,
  not full implementations: mova itself still has no interpreter-level
  `*err*` a real `print`/`println` writes through (confirmed: still
  unresolvable as a bare builtin, unlike `*out*`, which DOES exist
  post-M4b), so neither can intercept genuine process stderr the way real
  Clojure's JVM-bound compiler diagnostics need. Both run for real against
  this shim's own (non-JVM) `*err*` and pass or fail honestly rather than
  rubber-stamping -- see each macro's own comment in
  `mova-test-helper-shim.mova` and "`should-print-err-message`" below for
  exactly which rows this closes vs. leaves JVM-bound.
- `with-err-string-writer` -- **now implemented (S5)**, moved out of this
  list; see "`*err*` / `run-test` (S5)" below for how and its own caveats.
- `with-err-print-writer` -- **now implemented (W4-SHIM, 2026-08-21)**,
  moved out of this list. Measured against the oracle
  (test_helper.clj:99 vs. :109): upstream's `with-err-print-writer` and
  `with-err-string-writer` differ only in which `java.io` Writer class
  wraps `*err*` (`PrintWriter`-around-`StringWriter` vs. bare
  `StringWriter`), never in what a caller observes -- both just return the
  captured text as a string. mova has neither Writer class and this
  shim's `*err*` is a single hand-rolled atom-capture mechanism with no
  Writer-flavor distinction to preserve, so `with-err-print-writer` is
  implemented as a literal reuse of `with-err-string-writer`'s own
  expansion. Closes `def.clj`'s `non-dynamic-warnings` deftest (both
  assertions; see the new section below for the Rust-side warning this
  unblocked, and the harder-won cross-namespace `*err*`-capture fix it
  required).

**Consequence for vendored files using the undefined names:** because
mova resolves symbols inside an unexecuted `fn`/`deftest` body lazily, at
CALL time rather than at define time (confirmed by probe), a vendored
file that merely mentions e.g. `with-err-string-writer` in a `deftest`
body it never manages to reach still loads and runs its OTHER tests
fine; only the specific assertion that calls the undefined name errors
(caught by the shim's own `is`/`ts-run-one` try/catch, reported as
`:error`, never silently dropped). A `:require ... :refer [...]` of an
undefined name does not block the file either -- `:refer` of a
non-existent symbol is tolerated by mova at require time (confirmed by
probe against a two-file test module) and only errors when that specific
name is actually called. The one path that DOES still hard-block a whole
file regardless of this shim: a vendored file's `ns` form requiring a
namespace that doesn't exist ANYWHERE on the module path at all (as
opposed to an existing namespace missing one referred symbol) fails
eagerly while processing the `ns` form itself, before any `deftest` ever
registers (confirmed by probe). `run_single_test.clj` hits exactly this
with `clojure.test-clojure.test-fixtures` (a real vendored companion file,
but the runner loads each vendored file standalone, so cross-file
requires between vendored files are expected to fail -- see
`NOT-PORTABLE.md`'s "harness-architecture limitation" section, predating
this shim and unrelated to it).

## Fixtures / `use-fixtures` / `test-vars` -- IMPLEMENTED, selftested (compat/s3-fixtures)

`use-fixtures :each`/`:once`, fixture composition (`ts-compose-fixtures`/
`ts-join-fixtures`), and `test-vars` are now real, working pieces of this
shim (previously entirely absent). Landed to unblock
`tests/clojure-suite/vendor/test_fixtures.clj` (8/8 oracle assertions,
was `:blocked` on `Unable to resolve symbol: use-fixtures`). Every
semantic below was MEASURED against real Clojure 1.13.0-alpha6 with small
probe `deftest`s capturing side-effect order into an atom, not recalled
from memory -- see `mova-test-shim.mova`'s own comments on
`ts-join-fixtures`/`test-vars`/`ts-default-report` for the exact probes
and their output.

- **Composition order**: multiple fixtures registered in one
  `(use-fixtures :each f1 f2 f3)` compose with `f1` OUTERMOST (its
  pre-test code runs first, post-test code runs last) -- measured
  directly, not assumed from reading clojure.test's source.
- **`:once` fixtures** wrap the entire `run-tests` (or `test-vars`) call
  once, not once per test -- also measured, and also composes
  first-outermost.
- **`use-fixtures` REPLACES, not appends**, on a repeat call for the same
  key -- measured from test_fixtures.clj's own `use-fixtures-replaces`
  deftest, which relies on exactly this.
- **`test-vars`** runs a var's `:each` fixtures (and reports
  `:begin-test-var`/`:end-test-var` through `report`, inside the
  fixtures) only for a var that this shim's own `deftest` registered
  (`ts-registry-find`, standing in for real clojure.test's `(:test (meta
  v))` -- mova has no var metadata); a var never `deftest`-registered
  (e.g. a plain `defn`) skips `:each` fixtures entirely but `:once`
  fixtures still wrap the call unconditionally -- both measured, not
  assumed.
- **`declare`** is shadowed by this shim (see its own doc comment in
  `mova-test-shim.mova`) because mova's own `declare` interns a
  genuinely UNBOUND global that `binding` cannot push a value onto
  (confirmed by hand-probing a live mova: an unbound-but-interned var
  reads as "not bound" the same way a truly-unknown symbol does, and
  `binding` fails identically on both). This shim's `declare` instead
  `def`s each name to `nil` -- a real, bound-if-valueless global
  `binding` can use normally. This is a real, disclosed DEVIATION from
  core.mova's own `declare` (which stays faithful to real Clojure's
  "genuinely unbound until def'd" semantics) scoped to files assembled
  with this shim; it also happened to flip `def.clj`'s
  `nested-dynamic-declaration` deftest from `:error` to `:pass` as a
  side effect (confirmed via `tools/check-regression.sh`), since that
  test hits the exact same `declare`-then-`binding` gap independently of
  fixtures.
- **What's still NOT replicated**: fixtures scoped to
  more than one namespace (this shim has exactly one, spliced, registry);
  a fixture itself throwing is caught per-test and reported as an
  `:error` on that test (real clojure.test lets it propagate and abort
  the whole run) -- a deliberate, disclosed extension of this shim's
  existing "a test never silently drops the whole run" philosophy, not a
  regression, and unreachable by every file in the current corpus (no
  vendored fixture throws).

## `report` -- a REAL `(defmulti ^:dynamic report :type)` as of S7

Real clojure.test's `report` is
`(defmulti ^{... :dynamic true ...} report :type)` (read verbatim from
`.oracle/clojure-src/src/clj/clojure/test.clj`). This shim's used to be a
single `cond`-dispatched function (`ts-default-report`) behind a plain
`(def report ts-default-report)`, because mova had no `defmulti` when the
shim was written. mova has them now, and this section's old claim that
the `cond` was "a structural simplification, not a scoring risk" was
**wrong** -- it cost 1256 oracle assertions:

`vendor-libs/clojure/test/check/clojure_test.cljc` gates its entire
reporting setup, at LOAD time, on

```clojure
(if-not (instance? clojure.lang.MultiFn ct/report)
  (binding [*out* *err*] (println "clojure.test/report is not a multimethod, ..."))
  (let [...] (defmethod ct/report :begin-test-var ...) ...))
```

Measured: mova genuinely implements this `instance?` check --
`(instance? clojure.lang.MultiFn f)` is `false` for a plain fn and `true`
for a `defmulti`. With a plain-fn `report` the file took the warning
branch and died on bare `*err*`, which mova has no interpreter-level
binding for (still true, and not something a test shim can or should
paper over). That single expression is what kept `sequences.clj` (1148
oracle assertions) and `transducers.clj` (108) `:blocked` even after
`assert-expr` landed.

`report` is now a genuine multimethod with one `defmethod` per `:type`
(`:pass`, `:fail`, `:error`, and a `:default` no-op that catches
`:begin-test-var`/`:end-test-var`, test.check's own `::trial`/`::complete`
types, and maps with no `:type` at all). The bookkeeping inside each
method is unchanged from `ts-default-report`'s corresponding `cond`
branch.

Rebindability is unchanged and still load-bearing: `(binding [report
...])` on a `^:dynamic` defmulti var genuinely pushes/pops (probed), which
is what test_fixtures.clj's `can-run-a-single-test-with-fixtures` needs
(it intercepts `:begin-test-var`/`:pass`/`:end-test-var` events while
calling `test-vars`, and this shim's :pass/:fail/:error bookkeeping lives
INSIDE the methods specifically so a rebound `report` bypasses it,
matching real clojure.test's own `*report-counters*`-via-`report`
architecture -- confirmed: assertions evaluated under that test's
`test-vars` call do NOT inflate its own `:assertions` count, matching the
file's oracle total of exactly 8, not 10).

Guarded by `shim-selftest.mova`, which asserts `(instance?
clojure.lang.MultiFn report)` directly. That check has been sabotage-
tested (reverting `report` to the old plain-fn shape turns it red and
nothing else).

## `assert-expr` / `do-report` -- real, extensible (S7)

Real clojure.test's public extension seam for `is`:
`(defmulti assert-expr (fn [msg form] (cond (nil? form) :always-fail
(seq? form) (first form) :else :default)))`, plus a `do-report` a custom
method calls to hand results to `report`. The shim had neither, which is
the other half of why `sequences.clj`/`transducers.clj` were `:blocked`:
`vendor-libs/clojure/test/check/clojure_test/assertions.cljc` does
`(defmethod t/assert-expr 'clojure.test.check.clojure-test/check? ...)`
at load time and failed with "Unable to resolve symbol: t/assert-expr"
while the `ns` form was still being processed, so not one deftest in
either file ever registered.

Both now exist, with real clojure.test's exact dispatch function
(dispatching on the UNRESOLVED `(first form)` -- test.check's
`assert-check` writes the head symbol out fully qualified at the call
site, matching the `defmethod` key verbatim, so adding a resolution step
here would DIVERGE from real clojure.test rather than help). `is` routes
to a custom method only in its generic fallthrough, and only when
`(get-method assert-expr head)` is not identical to `(get-method
assert-expr :default)` -- mova returns the `:default` method itself for an
unregistered dispatch value, so that identity check is the whole test.

**Deliberate deviations, all disclosed:**

- **`is`'s existing special cases win.** `thrown?`,
  `thrown-with-msg?`, `thrown-with-cause-msg?`, `fails-with-cause?` and
  `=` are still handled by `is`'s own `cond` branches, byte-identically,
  *before* the assert-expr check. A vendored `(defmethod assert-expr
  'thrown? ...)` would be silently ignored rather than swapping this
  shim's measured degraded semantics for an expansion that needs a typed
  `catch`. Nothing in the current corpus registers a method for any of
  those heads (upstream `test-helper.clj` registers
  `thrown-with-cause-msg?`/`fails-with-cause?`, which is exactly why
  those two are hardcoded here -- see their own section above).
- **`assert-expr`'s `:default` method is `assert-any`-shaped only.** Real
  clojure.test's `:default` splits into `assert-predicate` (when `(first
  form)` resolves to a non-macro fn, giving per-argument failure
  reporting) and `assert-any`; the shim implements only the latter, for
  the same reason its own generic `is` branch is coarse (no var-metadata
  `:macro` flag to tell a fn from a macro). `is` never routes through
  this method -- it exists so `get-method` has a `:default` to compare
  against, and so a direct `(assert-expr msg form)` call gets working
  code back rather than `nil`.
- **`do-report` does not decorate with `:file`/`:line`.** Real
  clojure.test's scrapes them off a live stack trace; there is none here
  (below), so the map passes through with whatever `:file`/`:line` its
  caller already put in it.
- **Stack traces degrade to nil, MEASURED.** `(.getStackTrace
  (Thread/currentThread))` under this mova build does not throw -- it
  returns a degenerate empty array object on which `(seq ...)` is `nil`,
  `(count ...)` is `0`, and `(drop-while pred ...)` is `nil` *without
  ever calling `pred`*. That last part is what saves the vendored path:
  `test-context-stacktrace`'s predicate uses `.getClassName`, which mova
  does not have, and it is never reached. `file-and-line*` therefore
  takes its `(if (seq stacktrace) ... {:file nil :line nil})`
  else-branch, and the vendored `check-results` code runs intact with
  degraded (nil) file/line. Both halves of this are asserted in
  `shim-selftest.mova` (the `drop-while` check uses a predicate that
  THROWS, so "it got called after all" is a hard failure, not a silent
  one) and sabotage-tested.

## `:message` in report maps (S7)

Report maps emitted by `is` now carry real clojure.test's `:message` key,
holding the `msg` argument of `(is form msg)` VERBATIM. The shim used to
fold `msg` only into its own free-text `:detail` prose. That is invisible
to ordinary consumers but fatal to a vendored file that rebinds `report`
and DISPATCHES on the message -- `vendor/test.clj` is exactly that file
(see `test-ns-hook` below). The change is purely additive: `:detail`,
which `#RESULT`'s `:first-failure` is built from, is untouched, and
nothing else in this shim or the corpus reads `:message`.

`msg` is an arbitrary runtime expression, so it is evaluated EXACTLY once
per assertion, on the taken path only, and AFTER the form under test --
real clojure.test's own order, measured against 1.13.0-alpha6 rather than
assumed. The failure paths (which need the value twice: once for the
`:detail` prose, once for `:message`) bind one gensym-ed local in the
NARROWEST scope, at the report site. Hoisting that binding to the top of
the expansion was tried and rejected: it makes `msg` run before the form,
which is observable whenever `msg` is itself an assertion --
`vendor/string.clj`'s `t-ends-with?` has an upstream paren typo that
makes a whole second `(is ...)` the first one's `msg` argument, and the
oracle (measured) reports the nested assertion first and the outer one
second, which only the narrow binding reproduces. Both properties
(exactly-once, and after-the-form) are separately asserted in
`shim-selftest.mova` and sabotage-tested.

One honest side effect: because the failure paths now really do evaluate
`msg`, `string.clj`'s assertion count moved from 130 to **133 -- the
oracle's own number**. Those three assertions were previously never run
at all.

## `testing` context strings -- real, not faked, but not part of the contract

`testing` pushes/pops a string stack in an atom (no `binding` available) and
prefixes the first-failure message with the joined context (e.g. `"outer
inner: (is (= 1 2)) ..."`). This is genuinely functional (nesting works,
verified in shim-selftest.mova), not a stub -- but the `#RESULT`/`#SUMMARY`
contract has no dedicated field for it, so it only shows up folded into
`:first-failure`'s free-text tail. A runner parsing `:first-failure` as
prose for a human is fine; a runner trying to machine-parse testing context
out of it should not rely on the format being stable.

## `*test-out*` / output redirection -- not implemented

Real clojure.test lets you rebind `*test-out*` to redirect report output
(e.g. to a file). No dynamic vars in mova, so this shim always prints
straight to stdout via `println`. Not a faithfulness risk (there's nothing
to silently drop), just a missing feature -- a runner wanting captured
output must capture the child process's stdout itself.

## `(is expr)` / generic predicate reporting -- approximated, not fully general

Real clojure.test's `assert-predicate` reconstructs `(pred v1 v2 ...)` for
ANY list form whose head is a function (checked at compile time via
`resolve`+`ifn?`), so e.g. `(is (vector? x))` reports `(vector? x-value)`
on failure. This shim only special-cases `=` that way (every operand
independently reported, any arity -- see below); anything else falls
through to a generic "evaluate the whole form once, pass if truthy"
branch that reports the form and the single result value, not each
sub-evaluated argument. This is strictly less informative on failure (you
see `(pos? -1)` failed with `actual: false`, not with `-1` broken out
separately) but never wrong -- pass/fail correctness is unaffected, only
the diagnostic detail is coarser for anything other than `=`.

## `(is (= a b c ...))` used to false-pass on 3+ operands -- FIXED, selftested

Found in an audit, not by a vendored test tripping over it: `is`'s `=`
special case read exactly two operands (`(second form)` / `(nth form
2)`), so `(is (= a b c))` was evaluated and scored as `(= a b)` -- the
third (and any further) operand was never looked at. Concretely:
`(deftest three-arg (is (= 1 1 2)))` reported `:status :pass`. That is a
silent FALSE PASS -- the exact failure mode this document's bottom line
claims never happens, and while it existed, that claim was false. It
affected every `(is (= ...))` with 3 or more operands in the vendored
corpus (22 such forms at the time this was found), and would have
affected more as additional vendored files unblock.

Fixed: the `=` special case now handles any arity. Every operand is
evaluated exactly once (into fresh gensym-ed locals -- operands may have
side effects, and neither real clojure.test nor this shim may evaluate
one twice), and the assertion passes iff all evaluated values are `=` to
each other (`(apply = ...)` over all of them). On failure, every evaluated
operand is reported, not just the first two. The degenerate arities are
handled without indexing off the end of `form`: `(is (= x))` passes (one
operand, matching real Clojure's `(= x)` => `true`), and `(is (=))`
produces a runtime arity error from `=` itself, caught by `is`'s own
try/catch and reported as `:error` -- honest, not a shim crash and not a
silent pass.

This is now covered by `shim-selftest.mova`, which mechanically checks
(by inspecting the shim's own `ts-t-status`/`ts-t-assertions` atoms after
running probe assertions, not by eyeballing printed output): `(is (= 1 1
2))` fails, `(is (= 1 1 1))` and `(is (= 1))` pass, `(is (=))` reports
`:error` rather than crashing, and each operand of a variadic `=` -- on
both the passing and the failing path -- is evaluated exactly once. A
regression here cannot land silently: the selftest's process exit code is
nonzero if any of these checks fail.

## `are` template substitution into map/set literals -- FIXED, selftested

`ts-are-subst` (the tree-walker that substitutes `are`'s bound symbols
into its template form, mirroring real clojure.test's
`clojure.template/do-template`) only had branches for `symbol?`, `seq?`,
and `vector?`, falling through to `:else form` for everything else --
including maps and sets. A template containing a map or set literal, e.g.
`(are [x] (= {:k x} {:k 99}) 99)`, therefore never had its `x` replaced
inside the map, and evaluating the expanded `is` threw `"Unable to
resolve symbol: x"`.

Unlike the bug above, this did NOT produce a false pass -- it failed
loud, as an unhandled exception inside `is`'s try/catch (`:error`). But it
mis-attributed a harness limitation to the code under test, the opposite
of what this shim exists to do: a vendored file using `are` over a map or
set template would show a spurious `:error`, that reads exactly like a
real mova bug, for a gap that was actually in this shim.

Fixed: `ts-are-subst` now also handles `map?` (substituting into both keys
and values, via `reduce-kv`) and `set?` (substituting into elements, via
`into #{}`), rebuilding the same collection type. Anything else (records,
other opaque types) still falls through to `:else form` unchanged, same as
before. Covered by `shim-selftest.mova`, which mechanically verifies `are`
substitutes correctly into both a map-literal and a set-literal template
(including a template whose substituted comparison is deliberately made
to fail, to prove the walk is doing a real comparison and not just
returning "always equal").

## The shim exists TWICE per assembled file -- spliced, and as `clojure/test.mova`

`tools/clojure-suite-run.bb` both (a) splices the shim's source into the
assembled temp file right after its `(ns ...)` form (the original
mechanism, still what actually RUNS the tests) and (b) materializes the
same source, prefixed with `(ns clojure.test)`, at
`<scratch>/clojure/test.mova` on the module path. (b) exists only so the
six vendored files that say `(:require [clojure.test :refer :all])`
(rather than `(:use clojure.test)`, a tolerated-ignored clause) can get
past namespace resolution. The spliced copy's defs shadow every referred
name inside the file body, so the deftest registry, `run-tests`, and all
reporting stay the SPLICED copy's -- one registry, no split-brain. The
one shape that would bypass the shadowing is `(:require [clojure.test
:as t])` + qualified `t/deftest` calls, which would register tests in the
materialized copy's registry and silently drop them from the spliced
`run-tests`; **no vendored file does this** (verified with an anchored
grep 2026-08-20 -- beware the unescaped-dot false positive from
`clojure.test.check.clojure-test :as ctest`, a different namespace) --
re-verify if the vendored set is ever re-pinned. This whole double-copy
arrangement dissolves at M8 when a real ported `clojure.test` replaces
the shim and the splice is deleted.

**W4C update: the 2026-08-20 grep's premise was too narrow.** It checked
whether a VENDORED FILE itself qualifies into the materialized copy --
none does. It did not (and could not, by construction) catch a vendored
LIBRARY doing the same thing internally: `clojure.test.check.clojure-
test` (`vendor-libs/clojure/test/check/clojure_test.cljc`) itself
`(:require [clojure.test :as ct])` and calls `ct/is`/`ct/report` from
INSIDE `assert-check`, which every `defspec`'s `:test` fn calls. Since
that require resolves against the ONE materialized copy (never the
spliced one, regardless of which vendored file happens to be running),
`assert-check`'s reporting lands in the materialized copy's counting
atoms -- disconnected from the spliced copy's `ts-run-one`, which is what
actually prints `#RESULT`/updates the `#SUMMARY` line. See "`ts-meta-
tests` / `defspec` accounting" below for the measured effect (a
genuinely passing `defspec` reporting 0 assertions, not 1) and the fix
(route `defspec` accounting around `assert-check` entirely, rather than
trying to unify the two copies -- which remains exactly the "no vendored
file does this" gap this section describes, now honestly scoped to
"no vendored FILE does this directly").

## `*err*` / `run-test` (S5)

`mova-test-shim.mova` now defines a shim-OWN `*err*` (`def ^:dynamic
*err* nil`) + `ts-err-write` writer pair, and a `run-test` macro (the
SINGULAR form -- `run-tests`, plural, already existed). Neither is a real
mova interpreter feature: mova still has no interpreter-level `*err*`
that a real `print`/`println` writes through (confirmed still
unresolvable as a bare builtin) -- `*err*` here is just this shim's own
plain dynamic var, and "writing to it" means `ts-err-write` `swap!`ing a
string onto whatever atom it's currently `binding`-bound to (or silently
dropping the write if unbound/`nil` -- no bypass to genuine process
stderr exists or is attempted). `mova-test-helper-shim.mova`'s
`with-err-string-writer` builds directly on this pair.

`run-test`'s message text (`"Unable to resolve <sym> to a test
function."` / `"<sym> is not a test."`) embeds a NAMESPACE-QUALIFIED
symbol, matching real `clojure.test/run-test`'s own message shape (real
Clojure gets the qualification for free from `resolve`/its Var). mova's
syntax-quote does **not** auto-qualify a bare symbol to `current-ns/name`
the way real Clojure's backtick does (confirmed by probe: `` `sym ``
reads back as the bare `sym`, no `ns/` prefix -- `src/eval/quasiquote.rs`
only handles auto-gensym `#` symbols and unquote/splice, nothing else).
Every vendored caller builds its `run-test` argument inside a
syntax-quote specifically to get the qualified shape real Clojure
produces (`` `(run-test function-missing) ``), so `run-test` here
qualifies the symbol itself, with `*ns*`, at the point it prints the
message (`ts-qualify-sym`) -- reproducing the shape the vendored
assertions' regexes expect, rather than silently emitting an unqualified
name that would fail them.

"Is this var a registered test" reuses `ts-registry-find` (the same
var-identity standin `deftest`'s own comment documents for real
`(:test (meta v))`). This used to be a genuine architectural limit of the
splice-everywhere design described in the section above: `ts-registry` is
a plain atom, freshly `(def ts-registry (atom []))`'d by EVERY splice of
the shim, so it is scoped PER NAMESPACE, not shared process-wide the way
real Clojure's var metadata is -- `run_single_test.clj`'s
`can-run-test-with-fixtures` deftest calls `(run-test
tf/can-use-once-fixtures)`, where `tf` is an alias for the materialized,
separately-spliced `clojure.test-clojure.test-fixtures` companion
namespace, and `ts-registry-find` used to fail there because the var was
registered into THAT namespace's OWN copy of `ts-registry`, a different
atom instance from the caller's. **W3f closed this** (see `deftest`'s own
comment in `mova-test-shim.mova`): `deftest` now ALSO attaches real
`:test` var metadata (`vary-meta`), which is visible identically
regardless of which namespace's code reads it, so `ts-registry-find`
falls back to checking it whenever the direct-identity atom scan misses
-- `run-test` on a cross-namespace var now correctly finds it as a
registered test.

**What W3f did NOT close, and W4-SHIM (2026-08-21) now does**: finding
the var was never the whole story -- `can-run-test-with-fixtures`'s two
assertions (`*a* = 3`, `*b* = 5`) need `test_fixtures.clj`'s own `(use-
fixtures :once fixture-a fixture-b)` to have actually RUN first, and
`ts-run-test-single`'s `:else` branch used to call `ts-run-test-var-
summary` directly, which only ever wraps with `@ts-each-fixtures` (via
`ts-invoke-test`) -- `:once` fixtures were never applied AT ALL by
singular `run-test`, in ANY namespace, same-namespace calls included.
Worse, even fixing that naively (wrap with `@ts-once-fixtures`) would
have been wrong for the cross-namespace case specifically: `ts-once-
fixtures`/`ts-each-fixtures` are ALSO plain per-namespace atoms (module
header, "use-fixtures state"), so the CALLING namespace's copy (always
empty for a file like `run_single_test.clj` that never calls
`use-fixtures` itself) is not the same atom `test_fixtures.clj`'s own
`use-fixtures` call populated. `ts-cross-ns-fixtures` (next to
`ts-run-test-single`) fixes this properly: `(:ns (meta v))` gives the
target var's namespace as a SYMBOL (real mova var metadata --
`publish_var_meta` in `src/eval/special_forms.rs` -- unlike real
Clojure's `:ns` meta, which is a full `Namespace` object), and
`ns-resolve` on it reaches THAT namespace's own fixtures atom directly
(one `deref` for the Var's bound value, a second for the vector inside
the atom). Falls back to the caller's own atom when the var carries no
`:ns` or `ns-resolve` misses (defensive; not expected to fire for any
vendored caller). `can-run-test-with-fixtures` now returns the real
`{:test 1 :pass 2 :fail 0 :error 0 :type :summary}` scoreboard shows.

Regression-tested in `shim-selftest.mova`: `run-test` on an unresolvable
symbol and on a resolvable-but-unregistered symbol both check the exact
`*err*`-captured message text; a passing `run-test` on a real registered
test checks the returned summary map; a dedicated `:once`-fixture check
(`sf-rt-once-log`) proves `run-test` (singular) now applies `:once`
fixtures at all -- see that check's own comment for exactly what this
harness can and cannot prove about the cross-namespace half specifically
(the flat, `ns`-less selftest file has no second namespace of its own;
that half is exercised for real by `run_single_test.clj` against the
live vendored suite instead).

## `def.clj` non-dynamic-earmuff warning + cross-namespace `*err*` capture (W4-SHIM, 2026-08-21)

Closes `def.clj`'s `non-dynamic-warnings` deftest (both assertions).
Two independent pieces, both required:

**The warning itself**, `Interp::warn_if_non_dynamic_earmuff`
(`src/eval/special_forms.rs`, called from `eval_def` next to the
pre-existing `warn_if_def_shadows`): transcribed verbatim from
`.oracle/clojure-src/src/jvm/clojure/lang/Compiler.java`'s
`DefExpr.Parser.parse` -- `!isDynamic && sym.name.startsWith("*") &&
sym.name.endsWith("*") && sym.name.length() > 2` prints a `"Warning: ...
not declared dynamic ..."` line to `*err*`. `**` (length 2) never warns;
`*hello*` (length 8) does -- the exact two rows the deftest checks
(`(defn ** ...)` must produce `empty?`; `(def *hello* "hi")` must not).
`^:dynamic`-ness is read straight off the name symbol's `^{...}` meta the
same way `publish_var_meta` reads every other key there (mova's `def` has
no separate "is this var dynamic" flag of its own -- confirmed by grep).
Unlike the shadow warning, there is no "already interned" early return:
real Clojure re-warns on every redefinition of the same earmuffed name,
since the check is unconditional on every `def`.

**The harder-won half**: routing that warning through the existing
`write_shim_err` channel (`builtins::nsfns`) did not work out of the box.
def.clj's own deftest wraps both probes in `eval-in-temp-ns`
(`with-err-print-writer (eval-in-temp-ns (def *hello* "hi"))`), and
`eval-in-temp-ns` switches `current_ns` to a freshly gensym'd namespace
the shim was never spliced into -- `write_shim_err`'s original lookup
(`<current_ns>/*err*`, an exact namespace-qualified `Env::get`) found
nothing there and silently dropped the warning (measured directly: the
probe returned `""` instead of the real text, even though the SAME probe
with no `eval-in-temp-ns` wrapper correctly captured it). The dynamic
`binding` itself was never the problem -- `binding` pushes onto a
specific `Arc<VarCell>` by IDENTITY (`eval_binding`/
`resolve_binding_pairs`), which stays live for the whole dynamic extent
regardless of which namespace is "current" -- the problem was that
nothing at the write site could REACH that cell once `current_ns` no
longer named the namespace it lives in (mova's shim splices a SEPARATE
`*err*` into every namespace, unlike real Clojure's single process-wide
var, so there genuinely is no `*err*` cell at all in a fresh temp
namespace to fall back to via ordinary resolution).

Fixed with `Env::find_var_cells_named` (`src/env.rs`): given a bare name,
walks every currently-interned var cell across every namespace and
returns every one whose name matches, letting `write_shim_err` fall back
to whichever `*err*` cell is CURRENTLY bound to an atom (i.e. genuinely
being captured right now) when the namespace-qualified lookup misses.
Safe for this corpus because each vendored file runs as its own mova
process, so at most one `*err*` is ever actively bound at a time --
mirroring real Clojure, where "whichever `*err*` is live" is never
ambiguous either, because there is only one.

Regression-tested in `shim-selftest.mova`: four checks exercise the
warning rule (earmuffed+non-dynamic warns, `^:dynamic` suppresses it,
length-2 `**` suppresses it, a plain name suppresses it), and -- load-
bearing -- every one of them goes through `eval-in-temp-ns`, specifically
to exercise the cross-namespace `*err*`-capture fix rather than just the
warning-text rule; a same-namespace-only check would have missed the
real bug found while building this (see the section's own comment for
the before/after). Both the Rust length-rule and the `Env::find_var_
cells_named` fallback were sabotage-tested: reverting `> 2` to `>= 2`
flipped the `**`-does-not-warn check red; disabling the fallback (`_ =>
None`) flipped the eval-in-temp-ns warning check red; both reverts
restored a clean selftest run.

## `should-print-err-message` -- ten rows closed (W3e-4 + W4B-WARNINGS), two JVM-bound

Every `should-print-err-message` call site in the vendored corpus asserts
that a specific line of *compiler diagnostics* reached `*err*`. As of
W4B-WARNINGS, ten of them are mova-implementable rules and pass for real
(the `prefers` var-shadowing row since W3e-4, plus all 9 `rt.clj`
reflection rows since W4B-WARNINGS); the remaining two (`control.clj`'s
case-codegen rows) ask for JVM compiler internals mova genuinely does not
have. They are listed here individually, by regex, so nobody is tempted
to make the closed ones green by emitting text mova does not mean, and
the two still-open ones stay an honestly documented deviation rather than
a silently abandoned TODO.

**Closed (W3e-4), passes honestly:**

| file | assertion |
|------|-----------|
| `rt.clj` | `#"WARNING: prefers already refers to: #'clojure.core/prefers in namespace: .*"` -- `Namespace.checkReplacement`'s var-shadowing warning. Not a compiler-internals question at all: it is a namespace-mapping rule mova has, and `def`/`defn` now emit it through the same `Interp::shadow_warning` the `intern` native uses. Oracle text measured in `compat/def-shadow-warning-probe.{clj,mova}` / `compat/def-shadow-warning-transcript.txt`. |

**Closed (W4B-WARNINGS, 2026-08-21), passes honestly -- all 9, both
`with-err-string-writer`/`with-err-print-writer` variants:**

`*warn-on-reflection*`/`*unchecked-math*` `:warn-on-boxed` went from
inert plain globals (SPEC-D: "nothing in mova actually CONSULTS them
yet") to real consultation. `src/reflwarn.rs` walks a `defn`/`fn` body
(or any top-level form) exactly once, at the moment it is DEFINED --
hooked from `Interp::eval_form`, the one non-recursive top-level entry
every file-load/`eval`-native/REPL path already funnels through, so a
`defn`'d closure's body is analyzed once at definition time and never
again on each call -- against mova's OWN veneer member tables for
`java.lang.String` (`length`/`charAt`/`concat`/`getBytes`), its real
constructor overload set, `java.math.BigDecimal.divide`'s real two-arg
overload set, and `java.lang.Integer.valueOf`'s real overload set. Every
row below is a TRUE statement about mova's own static analysis: "can't be
resolved" means mova's own type-hint tracking (param tags, let-binding
tags, expression-level meta tags, literal types, ctor-result types, `defn`
return-type tags) genuinely cannot narrow the real overload set to
exactly one candidate, so the call falls back to mova's own dynamic,
name-based dispatch -- this repo's equivalent of the JVM's reflective
fallback. `tests/clojure-suite/mova-test-helper-shim.mova`'s
`should-print-err-message` is now a faithful transcription of upstream's
own shape (`(binding [*warn-on-reflection* true] ...)`, `eval-in-temp-ns`,
`re-matches` against BOTH writer flavors) rather than the documented gap
it used to be (see that macro's own comment for the before/after).

| file | assertion | how it resolves |
|------|-----------|------------------|
| `rt.clj` | `reference to field blah can't be resolved.` | Target type unknown (no hint anywhere in scope) -- `crate::reflwarn::check_dot_form`'s `Hint::Unknown` branch. |
| `rt.clj` | `reference to field blah on java.lang.String can't be resolved.` | Target hinted `^String`; `blah` is absent from `string_instance_sigs` at any arity. |
| `rt.clj` | `call to method zap on java.lang.String can't be resolved (no such method).` | Target hinted `^String`; `zap` absent -> 0 raw candidates at arity 1. |
| `rt.clj` | `call to method getBytes on java.lang.String can't be resolved (argument types: java.util.regex.Pattern).` | `getBytes` has 2 real one-arg overloads (`getBytes(String)`, `getBytes(Charset)`); the `#"boom"` (`Pattern`) argument's known type filters neither out, so ambiguity stands. |
| `rt.clj` | `call to method getBytes on java.lang.String can't be resolved (argument types: unknown).` | Same 2 overloads; the arg is an unhinted param -> `Hint::Unknown` never narrows a filter. |
| `rt.clj` | `call to method divide on java.math.BigDecimal can't be resolved (argument types: unknown, unknown).` | `BigDecimal.divide`'s real 2-arg overload set has 3 members (`divide(BigDecimal,int)`/`divide(BigDecimal,RoundingMode)`/`divide(BigDecimal,MathContext)`); both args unhinted/`nil`. |
| `rt.clj` | `call to method zap can't be resolved (target class is unknown).` | Target unhinted, arity >= 1 -> `Hint::Unknown`'s method-shaped branch. |
| `rt.clj` | `call to static method valueOf on java.lang.Integer can't be resolved (argument types: java.util.regex.Pattern).` | `Integer/valueOf`'s real 2 one-arg overloads (`valueOf(String)`/`valueOf(int)`); a `Pattern` arg matches neither. |
| `rt.clj` | `call to java.lang.String ctor can't be resolved.` | `String`'s real 3-arg ctors (`String(byte[],int,int)`/`String(char[],int,int)`) both require an array-typed first arg; three `long`-typed literals match neither. |

**Still JVM-bound, failing honestly (2 in `control.clj`'s `test-case`; 4
assertions against the ORACLE-ASSERTIONS.edn census, counting both writer
variants):**

| file | assertion | why it cannot be mova's |
|------|-----------|--------------------------|
| `control.clj` | `case has int tests, but tested expression is not primitive.` | A `case*` codegen decision: the JVM compiler is choosing between a `tableswitch` and a hash-based dispatch. mova's `case` is not compiled to a switch at all. |
| `control.clj` | `hash collision of some case test constants; if selected, those entries will be tested sequentially.` | Same -- it reports a property of the emitted jump table. |

These two are the reason `should-print-err-message` is *provided at all*
rather than left unresolvable (see the macro's own comment in
`mova-test-helper-shim.mova`): an unresolved-symbol throw mid-deftest
aborts every assertion after it, and in `control.clj` that was ~49
unrelated assertions. `body` still runs for real, and -- now that
`*warn-on-reflection*` is genuinely consulted -- so does
`crate::reflwarn`'s analysis of it; neither `case*` body contains a
dot-form/ctor-form/static-call for it to have an opinion about, so both
rows still fail exactly as before, now via a `re-matches` miss on real
(empty-of-this-warning) captured text instead of a vacuous `re-find` miss
-- same net "not passing", never a regression.

## `test-ns-hook` / `test-all-vars` -- IMPLEMENTED (S7)

Real clojure.test's `test-ns` calls a namespace's own `test-ns-hook` var
INSTEAD of `test-all-vars`, if one exists:

```clojure
(if-let [v (find-var (symbol (str (ns-name ns-obj)) "test-ns-hook"))]
  ((var-get v))
  (test-all-vars ns-obj))
```

`run-tests` now does the same, using `resolve` on a bare `test-ns-hook`
(the shim is spliced into the file's OWN namespace, so that resolves
there and nowhere else; `resolve` on an undefined name returns `nil`
rather than throwing -- probed). Exactly ONE vendored file defines a
`test-ns-hook` (`vendor/test.clj`, verified by grep), so this path is
unreachable for the other 50.

`test-all-vars` accepts an `ns` argument for signature parity and ignores
it: mova has no `ns-interns` (confirmed unresolvable) and no var
metadata, and this shim's stand-in for "the tests in this namespace" has
always been `ts-registry` -- which, because the shim is spliced once per
namespace, IS the set real `ns-interns` would return here. It runs each
registered test through `ts-run-one`, so per-test `#RESULT` lines are
still printed and the scoreboard contract does not change shape just
because a file routes its run through a hook.

**Why it matters:** `vendor/test.clj`'s entire design is a `test-ns-hook`
that rebinds `report` to a `custom-report` which INVERTS deliberately-
failing assertions (an `is` tagged `"Should fail"` that genuinely fails
is reported as a `:pass`). Without the hook, every "Should fail" /
"Should error" assertion in that file scored as a real failure, its
`original-report` var stayed `nil`, and the file scored 11/58. With it,
52/58 -- and the 58 itself is reproduced structurally, not coincidentally:
30 real `is` assertions + 28 `:begin-test-var`/`:end-test-var` events that
`custom-report` converts to `:pass` (14 tests x 2), which is exactly how
the oracle arrives at 58 too.

**C3g update**: of the 6 misses this section used to list, 2 are now
FIXED by the class-aware `thrown?`/`thrown-with-msg?` upgrade (measured:
`test.clj` moved 52 -> 54 of 58, exactly these two) -- `can-test-thrown`'s
"Should error" row (`(thrown? ArithmeticException (throw
(RuntimeException.)))`: wrong class, now correctly propagates as `:error`
instead of false-passing) and `can-test-thrown-with-msg`'s "Wrong class of
exception" row (`(thrown-with-msg? IllegalArgumentException #"Divide by
zero" (/ 1 0))`: `IllegalArgumentException` is not in `ArithmeticException`'s
real ancestry, so this now correctly errors too). The remaining 4 misses
are all previously-disclosed limits, not new ones:

- 1 x `thrown-with-msg?` "Wrong message string" -- UNCHANGED by C3g: this
  row's class (`ArithmeticException`) DOES genuinely match a division by
  zero, so it reaches the message check, but mova's internal
  divide-by-zero error is not an `ex-info` map (no `:ex/message` key), so
  `ex-message` still returns `nil` and the message-blind fallback still
  passes unconditionally -- the one false-pass family this upgrade does
  NOT close (see the section above).
- 3 x inside `clj-1102-empty-stack-trace-should-not-throw-exceptions`,
  whose body dies on `(Class/forName "java.lang.StackTraceElement")` --
  a genuine mova-core gap (no JVM reflection), not a shim gap. Those 3
  assertions never run at all, which is why the shim reports 55
  assertions where the oracle reports 58.

## `ts-meta-tests` / `defspec` accounting -- WIRED UP (W4C)

`test-all-vars`'s `ts-meta-tests` walk (previous section) was ADDED
specifically so `clojure.test.check.clojure-test/defspec` -- which
expands to a plain `defn` carrying real `:test` var metadata, never a
`deftest` call, so `ts-registry` never sees it -- gets found and run at
all. `sequences.clj`'s two `defspec` forms (`longrange-equals-range`,
`iteration-seq-equals-reduce`) are the only vendored occurrences.

That mechanism shipped with two stacked bugs, both found by direct
measurement against a live mova build and both fixed this session (see
`compat/w4c-defspec-ts-registry-find-probe.clj`,
`compat/w4c-defspec-cross-namespace-probe.clj`, and
`compat/w4c-defspec-transcript.txt` for the full bisection):

1. **`ts-meta-tests` always returned `()`.** Its exclusion filter used
   `ts-registry-find`, whose deliberate meta-fallback (`(:test (meta
   v))`, added later for a genuine cross-namespace `deftest`-lookup gap)
   is JUST as truthy for a `defspec` var's own `:test` metadata as it is
   for a `deftest` var's -- so EVERY `:test`-meta-bearing var, including
   the ones this walk exists to find, got excluded right back out. Fixed
   by giving `ts-meta-tests` the narrower `ts-registry-atom-find` (the
   same-namespace atom-scan half only) instead of the full
   `ts-registry-find` (which keeps its broader fallback for its other,
   still-valid caller).

2. **`assert-check`'s reporting lands in the wrong namespace's atoms.**
   Once `ts-meta-tests` found the vars, running them still reported 0
   assertions (not 1). `defspec`'s `:test` fn calls test.check's own
   `assert-check`, which calls `(ct/is (check? m))` -- `ct` being test.check's
   OWN internal alias for `clojure.test`, which resolves against the ONE
   materialized `clojure.test` module (see "The shim exists TWICE..."
   section above), never the copy spliced into the CURRENT vendored
   file's own namespace. So `assert-check`'s `do-report`/`report`/
   `ts-report-pass` chain increments the materialized copy's disconnected
   counting atoms, invisible to `ts-run-one`'s (correctly, file-locally
   scoped) read of its OWN `ts-t-assertions`. Fixed by NOT routing a
   `defspec` var's accounting through `assert-check` at all:
   `ts-meta-test-thunk` detects the `::clojure.test.check.clojure-test/
   defspec` marker key `defspec`'s own macro attaches (not a guess -- the
   literal contract that macro documents) and, for exactly those vars,
   calls the var directly (running the SAME `quick-check` with the SAME
   declared options -- nothing about the actual generative test changes)
   and translates its `:pass?` into exactly ONE `ts-report-pass`/
   `ts-report-fail` call through the CURRENT (correct) namespace's own
   reporting. Unifying the two namespaces' counting atoms generally would
   fix this more completely but is a much bigger, riskier change
   touching all 50 files' scoring -- out of scope for wiring up one
   corpus-wide gap with exactly two occurrences.

A related but SEPARATE bug surfaced only once both of the above were
fixed and the specs' bodies actually ran: `tests/clojure-suite/vendor-
libs/clojure/test/check/random.clj`'s `mix-gamma` macro writes bare
`(Long/bitCount ...)` inside its own syntax-quoted template, which mova's
syntax-quote qualification resolves to `java.lang.Long/bitCount` at
macro-definition time -- a spelling `src/builtins/statics.rs` never
registered (only the bare `Long/bitCount`, already fully implemented).
Fixed engine-side (`reg_static_fn`/`reg_static_value` now also register
under the fully-qualified class name whenever it differs from the bare
one), not in this shim -- noted here because it was found DURING this
defspec bisection, not before it.

**Current status**: both `sequences.clj` `defspec`s genuinely pass on
mova (`:pass? true`, measured directly, 1000 trials each -- ~0.8s for
`longrange-equals-range`, ~43s for `iteration-seq-equals-reduce`, both
comfortably inside the suite's 400s-per-file final-run timeout) and are
now correctly accounted: `sequences.clj` moved 1146/1148 -> 1148/1148
(`tests: 71` -> `73`, exactly `ORACLE-ASSERTIONS.edn`'s count). No other
vendored file uses `defspec`, so this closes the gap corpus-wide, not
just for these two rows. Regression-guarded in `shim-selftest.mova`
("W4C: ts-meta-tests / defspec accounting" section) with three checks,
each independently sabotage-verified (reverting either fix in isolation
turns the corresponding check(s) red and nothing else, restoring turns
them green again): a plain `:test`-meta var (not `deftest`-registered) is
found by `ts-meta-tests`; a mock `defspec`-shaped var is recognized by
`ts-defspec-var?`; and `ts-meta-test-thunk` reports a mock spec's
`:pass?`/no-`:pass?` result correctly WITHOUT ever invoking its raw
(deliberately poisoned, in the mock) `:test` fn.

## Bottom line for scoring

Every approximation above is designed to fail loud or fail honest, never
to quietly convert a real bug into a reported pass -- with the deliberate,
disclosed exception of the message-blind throw-assertion family
(`thrown-with-msg?` / `thrown-with-cause-msg?` / `fails-with-cause?`
whenever `ex-message` returns `nil` for a throw whose CLASS already
matched), which is the one place a false-pass is still structurally
possible. As of C3g this family is SMALLER than it used to be: `thrown?`
and all three message-checking heads are now genuinely CLASS-gated first
(see the section above) -- a wrong-class throw no longer false-passes at
all, only a right-class throw with an unrecoverable message still does.
That gate was landed deliberately and is measurable -- see that section
for the full per-file delta this cost (net -96 assertions-passed,
reported to the conductor, baseline NOT re-blessed) and
`vendor/test.clj`'s now-4 remaining misses (was 6) for a worked example
of what closing the class-blind half of this family bought. Any score
derived from this shim should be read with the message-blind caveat in
mind; everything else either works for real or reports `:error`/`:fail`
rather than guessing.

This claim was FALSE for a period of this shim's life (see the variadic
`=` section above) -- it silently converted a real bug into a reported
pass, which is precisely what this section says never happens. The claim
is only as strong as the mechanism that enforces it, and eyeballing
`#RESULT` lines by hand is not that mechanism; `shim-selftest.mova` is.
Anyone relying on this document's "never a silent false pass" claim
should confirm `shim-selftest.mova` still exits 0 (`#SELFTEST {:status
:pass ...}`) on the current shim before trusting a score derived from it,
not just take this file's word for it.
