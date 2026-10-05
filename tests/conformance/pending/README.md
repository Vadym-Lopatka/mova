# Pending conformance: the debt ledger

`tests/conformance/corpus/` (see `tests/conformance_test.rs` and
`CONFORMANCE-GUARANTEE.md`) is the "already conforms" ledger: every form
there matches real Clojure today, and a mismatch fails the build. This
directory is its mirror image -- the "does not conform *yet*" ledger. A
form belongs here **only while it currently diverges** from real Clojure
1.13.0-alpha6. The moment a form here starts conforming (mova got fixed,
or the form turns out not to prove anything), it must be promoted out.
`tests/pending_conformance_test.rs` enforces that mechanically: it is a
compile error of sorts to let a conforming form sit here undetected.

## Format

For an area named `<area>`, three files:

```
<area>.corpus   one form per line; `;;`-prefixed and blank lines skipped
                (identical rules to tests/conformance_test.rs's corpus
                format). An optional `;;PRELUDE <code>` first line is
                evaluated once, before any form, and produces no golden
                line of its own -- e.g. numerics.corpus uses it to
                `require` `clojure.math` before its interop forms run.
<area>.golden   one line per form, from REAL Clojure 1.13.0-alpha6
                (pinned in tests/conformance/CLOJURE_VERSION):
                  OK<TAB><pr-str of the result>
                  ERR<TAB><ExceptionSimpleName>
                Note the format difference from the main corpus's bare
                `ERR`: the exception's simple class name is kept around
                so a future error-taxonomy pass has something to build
                on (see "ERR-BOTH" below for why the driver doesn't use
                it yet). No unwrapping of
                `clojure.lang.Compiler$CompilerException` to its cause --
                the RAW/outer exception class is recorded, exactly as
                thrown.
<area>.mova     one line per form, mova's CURRENT behavior:
                  OK<TAB><pr-str>
                  ERR<TAB><pr-str of the caught value>
                  TIMEOUT
                  UNREACHED
                Documentation only -- tests/pending_conformance_test.rs
                never reads this file. It exists so a human (or the next
                area-writing agent) can `git diff` mova's error text
                changing without re-running anything, and so it never
                makes the actual test fail merely because mova's error
                message changed wording. The `ERR` payload is whatever
                mova's own untyped `(catch e ...)` bound `e` to, pr-str'd
                -- not CLI stderr text -- because every form in the file
                runs inside one generated mova program (see "Session
                model" below), not as a standalone `mova -e` invocation.
```

`<area>.corpus` is hand-authored. `<area>.golden` and `<area>.mova` are
always machine-generated -- see "Regenerating" below. Never hand-edit a
`.golden` or `.mova` file.

## The driver: `tests/pending_conformance_test.rs`

Runs every area file as ONE `mova <script>` process (see "Session model"
below) and classifies each form's result against the Clojure golden:

- **`CONFORM`** -- Clojure's golden is `OK v` and mova returns the same
  `OK v`. **Fails the test.** See "Promotion workflow" below.
- **`ERR-BOTH`** -- both sides threw. Counted and reported separately as
  "needs error taxonomy", and **never** auto-promoted: mova has no
  exception-class taxonomy yet, so "both threw" does not prove the same
  thing was thrown.
- **`DIVERGE`** -- anything else (`OK` vs `ERR`, or two different `OK`
  values). The expected, healthy state for a pending form.
- **`TIMEOUT`** -- the session ended (killed after the per-file timeout,
  or crashed) while this form was in flight, with no result recorded for
  it. mova has a known bug where a non-tail `recur` loops forever; pending
  forms deliberately probe for it (`errors.corpus`'s last form). Reported
  as its own bucket, EXCLUSIVE of `DIVERGE` -- a hang is a materially
  worse failure than a wrong answer.
- **`UNREACHED`** -- the session ended before evaluation ever reached this
  form. No evidence either way, so also EXCLUSIVE of `DIVERGE` and never
  counted as debt or as agreement. A nonzero `UNREACHED` count for an area
  not deliberately testing early termination means a hang or crash landed
  somewhere other than that file's last form -- see "Session model".

Every run prints a per-area summary (`N forms | D DIVERGE | E ERR-BOTH |
T TIMEOUT | U UNREACHED`), so `cargo test --test pending_conformance_test`
output is itself the debt report. A healthy run is green.
`tools/generate-compat-report.bb` derives the identical table (same
exclusive-bucket math) from the committed `.golden`/`.mova` columns for
`COMPATIBILITY.md`; the two must always agree exactly per area, since they
classify the same forms the same way from two independent code paths --
if they ever disagree, the live driver is authoritative and the `.mova`
column is stale (`tools/gen-pending.sh` regenerates it).

### Session model: ONE process per area file

Both `<area>.golden` (via `tools/jvm-pending-runner.clj`) and `<area>.mova`
(via the live driver and `tools/gen-pending.sh`) run each area file as
**one session**: a `def` on an earlier line is visible to a later line,
exactly like a real REPL. The two sides MUST use the same session model,
or a diff between them means nothing -- an earlier revision of this
driver ran mova in **per-form isolation** (a fresh `mova -e` process per
form) instead, which was a real correctness bug: a form that genuinely
depends on an earlier `def` would see `ERR unresolved symbol` where the
real, in-session answer might match Clojure's golden exactly, hiding a
paid-off `CONFORM` behind a fake `DIVERGE`/`ERR-BOTH`. Concretely:

```
mova -e '(def zzz)' ; mova -e '(nil? zzz)'   ->  ERR "Unable to resolve symbol: zzz"
printf '(def zzz)\n(println (nil? zzz))\n' | mova file  ->  true
```

Hang safety is why this is a process boundary at all: mova has a known
bug where a non-tail `recur` loops forever, and pending forms deliberately
probe for it. The driver (and `tools/gen-pending.sh`) compile every form
in an area file into ONE generated mova program -- each form's source
embedded as a string literal, evaluated via `(eval (read-string ...))`
(the same technique `tools/jvm-pending-runner.clj` uses on the golden
side) and wrapped in mova's own `try`/`catch`, printing a tagged
`##PENDING-{BEGIN,OK,ERR}##<index>` marker line per form so one form
throwing can never stop the forms after it. stdout and stderr are merged
into one captured stream (mova writes diagnostics to stderr, and a
previous run in this campaign lost an entire session's crash diagnostics
by only capturing stdout). A per-FILE timeout bounds the whole session;
on timeout, the form whose `BEGIN` marker has no matching `OK`/`ERR` is
`TIMEOUT`, and every form after it (no `BEGIN` marker at all) is
`UNREACHED`.

`errors.corpus` deliberately places its hanging form LAST in the file so
that nothing in it is `UNREACHED` today -- that placement is a
corpus-authoring convention the `UNREACHED` count polices, not something
enforced structurally. A hang placed mid-file would blind the ledger to
every real form after it in that area.

## Promotion workflow

When `cargo test --test pending_conformance_test` reports a `CONFORM`
failure for `<area>.corpus:<line>`:

1. Copy the form to `tests/conformance/corpus/<some-file>.corpus` (an
   existing file if it fits topically, or a new one).
2. Regenerate that main corpus's golden the way its own tooling expects
   (`tools/gen-golden.bb` -- owned by a different workstream; see
   `tests/conformance_test.rs`'s module doc).
3. Delete the form's line from `<area>.corpus`, `<area>.golden`, and
   `<area>.mova` (all three, same line position) here.
4. Re-run both `cargo test --test conformance_test` and
   `cargo test --test pending_conformance_test` to confirm both are
   green.

## Regenerating

```sh
tools/gen-pending.sh                # regenerate every area's .golden and .mova
tools/gen-pending.sh numerics       # just one area
tools/gen-pending.sh numerics errors  # or a few
```

Requires a real `clojure` CLI on PATH with network access to Maven
(pinned to `tests/conformance/CLOJURE_VERSION` via `-Sdeps`, same as the
main corpus's `tools/gen-golden.bb`), and GNU `timeout`/`gtimeout` on
PATH. Builds `mova --release` once up front, then regenerates `.golden`
(one real-JVM process per area file, via `tools/jvm-pending-runner.clj`)
and `.mova` (one `mova <script>` process per area file, the identical
session model, via the SAME `##PENDING-*##`-tagged generated-program
technique the live driver uses) for each requested area.

## Canonicalization lint

`tests/pending_conformance_test.rs`'s `lint_nondeterminism` runs against
every pending form before it's even evaluated, and hard-fails the test if
it fires (message names the file, line, and which rule caught it). It
exists because nothing else stops a nondeterministic golden from landing
-- and a nondeterministic golden is worse than a stale one, since it can
flip between `DIVERGE` and `CONFORM` from one regeneration to the next for
reasons that have nothing to do with mova.

**What it catches:**

- The corpus form's source calling an unconditionally-banned
  nondeterministic/wall-clock source: `shuffle`, `gensym`,
  `System/currentTimeMillis`, `System/nanoTime`, `Math/random`,
  `random-uuid`, `java.util.UUID/randomUUID`, `java.util.Date`.
- The corpus form's source calling `rand`/`rand-int`/`rand-nth` **unless**
  the form's outer call is a coarse, shape-only predicate that makes the
  random value irrelevant to the printed result (`number?`, `int?`,
  `string?`, `coll?`, `count`, `class`, ... -- the full allowlist is in
  `COARSE_PREDICATE_ALLOWLIST`). This distinction exists because
  `numerics.corpus` legitimately uses `(number? (rand))` and
  `(int? (rand-int 10))` to test that these functions exist and return
  the right shape, without needing (or wanting) a reproducible value.
- The golden's `OK` payload containing `#object[` -- JVM object
  identity/hashcode text is never stable across runs.
- The golden's `OK` payload containing a gensym-shaped token
  (`__<digits>`, optionally followed by `__auto__`) -- Clojure's
  syntax-quote auto-gensym and `(gensym)` naming convention.
- The golden's `OK` payload containing a flat (non-nested) multi-entry
  `{...}` map/set literal whose keys are not already in ascending order,
  UNLESS the form's own source mentions `sorted-map`/`sorted-set` (i.e.
  the author already canonicalized it deliberately). Recommends wrapping
  in `(into (sorted-map) ...)` / `(into (sorted-set) ...)`, per
  `CONFORMANCE-GUARANTEE.md`'s own canonicalization rule 2.

**What it structurally cannot catch:**

- Nondeterminism hidden behind a coarse predicate not on the allowlist.
- Nondeterminism inside a *nested* map/set literal -- the unsorted-literal
  check only looks at flat, non-nested `{...}` spans (a real reader-level
  parser would be needed to walk nested structures; this is a fast,
  in-process, string-level heuristic instead).
- A nondeterministic value that happens to print identically on every run
  by coincidence (e.g. a hash-bucket order that is, for this JVM/Clojure
  version and these particular keys, stable today but not guaranteed to
  stay that way). Only running the golden generator twice and diffing
  would catch that class of risk -- `tools/gen-pending.sh` does not do
  this today (it would double the JVM-shell-out cost of every
  regeneration); if a pending form's golden ever changes between two
  otherwise-identical regenerations, that is exactly this failure mode,
  and the form should be fixed (usually by canonicalizing or by adding it
  to the banned-call list above) rather than re-committed.
- Any nondeterminism that is purely on the `.mova` side. The lint runs
  against the golden (Clojure) and the corpus source text, not against
  mova's own output -- mova's output is compared fresh on every test run
  anyway, so a flaky mova answer shows up as flaky `CONFORM`/`DIVERGE`
  test results directly rather than needing a separate static check.
