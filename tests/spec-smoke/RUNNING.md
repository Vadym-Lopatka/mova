# Running the spec smoke corpora

There are **three** oracle-diffed corpora here, and they work the same way:
the same file is executed by mova and by real Clojure 1.13.0-alpha6 (which
ships `clojure.spec.alpha`), and the two **stdout** streams must be
byte-identical.

| file | what it pins | status |
| --- | --- | --- |
| `smoke.mova` | the `clojure.spec.alpha` + `clojure.spec.gen.alpha` API surface. Non-generative forms print values; generative forms print *properties* (booleans / counts / distinct sets). | **180 lines, byte-identical** (2026-09-03, W3) |
| `stest-smoke.mova` | `clojure.spec.test.alpha` — instrument / unstrument / check. | **125 lines, byte-identical** (2026-09-03, W5) |
| `seeded-gen.mova` | SEEDED generation, printed as raw VALUES: the RNG itself, test.check's generators and combinators, `s/gen`, and a failing `quick-check`'s shrink result. | **33 lines, byte-identical** (2026-09-03, W6a) |

Everything below applies to all three; substitute the file name.

The third corpus exists because W6a made
`clojure.test.check.random` a Rust-native, bit-exact transcription of
upstream's splitmix64 (`src/splitrandom.rs`). Before it, "the two hosts
have different PRNGs" was a rule this directory had to write around;
now the two hosts have the SAME PRNG, and a seeded draw is reproducible
across them. `smoke.mova` and `stest-smoke.mova` keep their
property-printing style anyway -- they are not about generation, and
properties are the better assertion for what they do test.

**Since W6b these three files are also a `cargo test` gate**, not just a
manual recipe: `tests/spec_smoke_test.rs` runs each `.mova` through the
same in-process machinery the conformance corpus uses and diffs it
against the committed oracle capture in `*.golden`. Regenerate a golden
only from the oracle command in section 2, never from mova's own output.

## 1. Run it on mova

**No staging, and no `--module-path`.** W3 embedded `clojure.spec.alpha`,
`clojure.spec.gen.alpha`, `clojure.walk` and the whole
`clojure.test.check` stack in the binary (`src/stdlib.rs`), so a bare
`mova` can run the corpus:

```sh
cargo build --release
./target/release/mova tests/spec-smoke/smoke.mova       2>/dev/null > /tmp/mova.txt
./target/release/mova tests/spec-smoke/stest-smoke.mova 2>/dev/null > /tmp/mova-stest.txt
./target/release/mova tests/spec-smoke/seeded-gen.mova  2>/dev/null > /tmp/mova-seeded.txt
```

W2 needed a `mktemp -d` staging directory here (spec's `clojure.walk`
dependency and the `.clj`→`.cljc` renames test.check's `random.clj`
required). All of that is gone; if you find yourself re-creating it,
something in the embedded table has regressed — that is the bug, not the
recipe.

`2>/dev/null` drops stderr only: mova prints `WARNING:` /
`Reflection warning,` / `Boxed math warning,` lines there, exactly as real
Clojure does, and neither side's stderr is part of the comparison.

## 2. Run it on the oracle

```sh
clojure -Sdeps '{:deps {org.clojure/clojure {:mvn/version "1.13.0-alpha6"}
                        org.clojure/test.check {:mvn/version "1.1.1"}}}' \
        -M -i tests/spec-smoke/smoke.mova -e ':end' 2>/dev/null \
  | sed '$d' > /tmp/oracle.txt      # drop the trailing `:end` marker
diff /tmp/mova.txt /tmp/oracle.txt && echo IDENTICAL

clojure -Sdeps '{:deps {org.clojure/clojure {:mvn/version "1.13.0-alpha6"}
                        org.clojure/test.check {:mvn/version "1.1.1"}}}' \
        -M -i tests/spec-smoke/seeded-gen.mova -e ':end' 2>/dev/null \
  | sed '$d' > /tmp/oracle-seeded.txt
diff /tmp/mova-seeded.txt /tmp/oracle-seeded.txt && echo IDENTICAL
```

`stest-smoke.mova` uses the identical command with its own file name (its
capture goes to `/tmp/oracle-stest.txt`). All three corpora need
`org.clojure/test.check` on the oracle's deps (spec's generators, and
`stest/check` itself, are test.check); it is already in the `-Sdeps` map
above.

The `*.golden` files committed beside each corpus ARE these three oracle
captures, byte-for-byte, taken with exactly the `-Sdeps` map above
(`org.clojure/clojure 1.13.0-alpha6`, `org.clojure/test.check 1.1.1`).

`-i` loads the file without echoing each top-level form's value; the
trailing `-e ':end'` exists only so the exit status is meaningful, and its
printed line is stripped.

## Notes for whoever extends this corpus

* Put `(set! *print-namespace-maps* false)` at the top (already there).
  `clojure.main` turns namespace-map printing ON, mova leaves it off, so
  fixing it explicitly on both sides is what makes `explain-data` output
  comparable.
* **Never print an auto-gensym.** Two forms leak one and are therefore
  normalized here rather than compared raw:
  * `(s/describe (s/keys* ...))` ends in `mspec__<n>__auto__` — the corpus
    prints `(take 3 ...)`.
  * A `#(...)` nested *two* levels deep inside a spec macro (e.g. inside an
    `s/and` inside an `s/or`) is **not** un-`fn`'d by upstream's own `unfn`,
    so real Clojure leaks its reader gensym (`p1__211#`) into `s/form`
    while mova shows `%1`. The corpus lifts such predicates into their own
    named specs instead.
* In `smoke.mova`, generative forms print properties, not values — but
  since W6a that is a style choice, not a necessity: the two hosts share a
  bit-exact PRNG, and `seeded-gen.mova` is the corpus that exploits it. An
  UNSEEDED generative form still has to print properties on either host,
  because `(make-random)` with no seed reads the clock by design.
* `sort` any generated SET (or normalize any generated map) before printing
  it. Hash iteration order is a property of each host's hash table, not of
  the RNG, and the two differ — measured while writing `seeded-gen.mova`:
  identical MEMBERS, different print order.
* W3 added sections 9-11 (`s/fspec`, `s/multi-spec`, `s/inst-in`), the
  three constructs W2 had to leave out. All three are upstream-verbatim in
  the port and all three now oracle-diff clean, so `docs/SPEC-PORT-
  PATCHES.md`'s "UNTESTED-W2" list is empty.
  * `s/*fspec-iterations*` (an upstream var) is bound down from its
    default of 21 around the generative `s/valid?` calls, so the section
    stays quick on both hosts. Do NOT drop it to 0 -- `validate-fn` then
    checks nothing and every fn "conforms".
  * Section 11 prints `#inst` values directly, so it also pins
    `print-method`-for-Date output byte-for-byte. An instant literal with
    no time part reads as midnight UTC and prints back in full
    (`#inst "1990"` -> `#inst "1990-01-01T00:00:00.000-00:00"`).
  * Section 10's `(sort (distinct (map :event/type (gen/sample ...))))`
    is the deterministic way to assert a multi-spec generator reaches
    BOTH `defmethod` branches -- the sample itself is random, the set of
    dispatch values it covers is not.

## Notes specific to `stest-smoke.mova` (W5)

`clojure.spec.test.alpha` is the one namespace whose output can depend on
WHERE a call was made from, so this corpus has three normalizations the
`smoke.mova` rules do not cover. All three are deliberate, and each is
explained inline in the file as well as here.

* **`:file` is reduced to its basename** by the `caller-file` helper. A JVM
  `StackTraceElement`'s `getFileName` is the basename; mova's `callstack*`
  reports `Interp::source_name`, the path as given, matching mova's own
  `:file` var metadata and its rendered stack traces. Both sides are
  reduced the same way, and the resulting string IS compared.
* **`:line` is asserted as `(pos-int? ...)`, not as a number.** mova's
  macro expander rebuilds a macro's output carrying the macro CALL form's
  span, so a call inside a `defn` body reports the `defn`'s own line where
  the JVM reports the call's. That is a general property of every mova
  stack trace (`docs/SPEC-PORT-PATCHES.md` section D item 11), not
  something spec introduced. The exact mova-side number is pinned by
  `tests/spec_test_alpha_test.rs::stest_caller_line_is_the_defn_head`, so
  it cannot drift just because the corpus stopped looking.
* **`::stest/caller` is `dissoc`'d from the one ex-data key-set row that
  would see it.** For a bad call made at TOP LEVEL, the JVM has a frame
  (every top-level form is compiled into an `evalNNN` fn) and mova has
  none, so the key is present there and absent here. Every OTHER row in the
  corpus makes its bad call from inside a `defn`'d helper, which is both
  the shape `clojure/test_clojure/instr.clj` asserts on and the shape a
  real program has.

Two more things worth copying if you extend this file:

* `summarize-results` PRINTS through `clojure.pprint/pprint` upstream and
  through `prn` here (MOVA-PATCH P14). The corpus captures that output with
  `with-out-str` and discards it, comparing only the summary MAP the fn
  returns — which is its documented contract.
* `check` results carry a random test.check seed. Print `:sym`, `:pass?`,
  `:num-tests`, `(contains? r :failure)` and `abbrev-result` — never the
  raw `::stc/ret` map.
