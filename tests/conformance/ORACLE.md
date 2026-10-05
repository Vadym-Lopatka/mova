# Getting the oracle

CONFORMANCE-GUARANTEE.md's whole claim rests on one move: every golden is produced by shelling out to
**real Clojure**, not by this project's own idea of what Clojure does. Rule 5 there says "anyone can
reproduce it in minutes on a clean checkout." This file is how. If you don't trust the goldens already in
this repo, stop reading prose and run the commands below yourself.

---

## The pin

One version of real Clojure, one commit of its source, both recorded in two places that are checked
against each other on every `tools/bootstrap-oracle.sh` run so they cannot silently drift apart:

| | |
|---|---|
| Clojure version | `1.13.0-alpha6` |
| Git tag | `clojure-1.13.0-alpha6` |
| Commit | `15c821d7e7df241c8f9fed42fbc51b09c65b67d9` |
| Repo | https://github.com/clojure/clojure |
| JDK tested against | 21 |

- **`tests/conformance/CLOJURE_VERSION`** — the bare one-line version string every existing tool
  (`tools/gen-golden.bb`, `tools/gen-pending.sh`, `tools/jvm-runner.clj`, `tools/jvm-pending-runner.clj`,
  `tools/jvm-flow-runner.clj`) already reads. Its format does not change here.
- **`tests/conformance/ORACLE.edn`** — the same version plus the git tag, commit, repo URL and tested JDK,
  in one machine-readable map, for tools (like this one) that need more than a bare version string.

Neither file is the "real" one — they're asserted equal on every bootstrap run. Edit both together.

---

## `tools/bootstrap-oracle.sh`

One command that turns "I have this repo" into "I have a working oracle," idempotently, auditable in one
read (like `tools/verify-vendor.sh`, which it deliberately mirrors in shape).

```sh
tools/bootstrap-oracle.sh                    # materialize + verify, print a summary
tools/bootstrap-oracle.sh --check            # verify only; exit 1 if incomplete/wrong; creates nothing
tools/bootstrap-oracle.sh --print-deps-dir         # print .oracle/deps, nothing else, on the last stdout line
tools/bootstrap-oracle.sh --print-deps-async-dir   # same, for .oracle/deps-async (adds core.async)
tools/bootstrap-oracle.sh --print-src-dir          # same, for .oracle/clojure-src
```

It produces `.oracle/` at the repo root (override with `ORACLE_DIR`), gitignored, containing:

- **`deps/deps.edn`** — `{:deps {org.clojure/clojure {:mvn/version "1.13.0-alpha6"}}}`. `cd` in and
  `clojure -M -e '(pr *clojure-version*)'` is real Clojure 1.13.0-alpha6.
- **`deps-async/deps.edn`** — the same, plus `org.clojure/core.async {:mvn/version "1.9.808-alpha1"}`.
  `async.corpus` and `flow.corpus` need core.async on the classpath (via their own `;;PRELUDE`/`;;ENGINE`
  directives); `tools/gen-golden.bb` adds that exact coordinate to every JVM invocation regardless of
  which corpus file is running, since it's cheap to have on the classpath and unused otherwise — this
  directory mirrors that choice rather than special-casing which corpus wants it.
- **`clojure-src/`** — a git checkout of `clojure/clojure` at the pinned *commit* (not just the tag), used
  as the spec of record when a golden looks wrong and you want to read `src/jvm/clojure/lang/*.java`,
  `src/clj/clojure/core*.clj`, or `test/clojure/test_clojure/` directly instead of trusting this project's
  paraphrase of what Clojure does. It's a `git worktree add --detach` off a local clone when one is
  available (never a network fetch), or a fresh blobless clone otherwise — the script logs which strategy
  it took.

The script hard-fails, with the exact mismatch printed, if: the two pin files disagree; the resulting
`clojure-src` HEAD isn't the pinned commit; or `clojure -M -e '(pr *clojure-version*)'` in `deps/` doesn't
report major 1 / minor 13 / qualifier `alpha6`. It never silently downgrades to "close enough."

Re-running it is free — every step checks whether its target is already correct before doing anything, and
nothing is ever force-deleted; a target found in the *wrong* state is a hard failure telling you what to
remove by hand, not a silent overwrite.

---

## Regenerating goldens and diffing them

Once `.oracle/` exists (or even without it — `tools/gen-golden.bb` and `tools/gen-pending.sh` build their
own `-Sdeps` string from `tests/conformance/CLOJURE_VERSION` directly and don't require `.oracle/` to
exist), regenerate and diff:

```sh
bb tools/gen-golden.bb          # rewrites tests/conformance/corpus/*.golden from real Clojure
tools/gen-pending.sh            # rewrites tests/conformance/pending/*.{golden,mova}
git diff tests/conformance      # nothing should move; anything that does is either a real
                                 # divergence (goes to DEVIATIONS.md) or a bug to fix
```

Both tools shell out to a real `clojure` process per corpus/area file, pinned to `CLOJURE_VERSION`;
`core.async` (`1.9.808-alpha1`) rides along on every JVM invocation for the corpora that need it, exactly
as it does in `.oracle/deps-async`. See `tools/jvm-runner.clj`'s and `tools/gen-golden.bb`'s own module
docs for the full session model and output contract if you're auditing the mechanism itself, not just
running it.

---

## Bumping the pin to a new Clojure version

In this order, in one commit, because a partial bump makes the goldens and the pin describe two different
implementations:

1. Update `tests/conformance/CLOJURE_VERSION` and `tests/conformance/ORACLE.edn`'s `:clojure-version`,
   `:git-tag`, `:commit` together — `tools/bootstrap-oracle.sh` refuses to run while they disagree.
2. Remove any stale `.oracle/` (it's gitignored and disposable) and run `tools/bootstrap-oracle.sh` fresh
   against the new pin.
3. Regenerate every golden: `bb tools/gen-golden.bb` and `tools/gen-pending.sh`.
4. `git diff` every changed `.golden`/`.mova` file **one by one** — a version bump can change real
   semantics, and a diff here is not noise to `git add -A` away. Anything that changed and is intentional
   upstream behavior gets folded in; anything that looks like this project's own bug gets fixed first and
   re-diffed.
5. Re-hash `tests/clojure-suite/vendor/` against the new tag (see `tools/verify-vendor.sh` and
   `tests/clojure-suite/MANIFEST.sha256`'s own regeneration instructions) — the vendored upstream test
   suite is pinned to the same commit and moves in the same commit as everything above.
6. Update the commit/tag prose in `COMPATIBILITY.md` and `CONFORMANCE-GUARANTEE.md` to match.

There is no partial-bump state that is safe to merge.
