# mova

**A system-level Clojure dialect hosted on Rust.**

mova asks what a from-scratch Clojure in Rust would look like if it made
different bets than [jank][jank] (the native-code Clojure dialect built on
LLVM): precise reference counting instead of a tracing GC, persistent
structures from a Rust crate instead of a hand-rolled HAMT, and `libc`
wrapped directly instead of hidden behind a runtime. It is a Rust
interpreter with a compiled-fn tier on top (see the performance section),
and it tries to be honest about failure.

[jank]: https://github.com/jank-lang/jank

## What is in this repository

- **The language.** One binary, `mova`: REPL, script runner and embedding
  API.
- **An nREPL server**, built into the same binary: `mova nrepl`. See
  "nREPL server" below. Code: `crates/mova-nrepl` and `src/nrepl`.
- **`nx-core`** (`crates/nx-core`): the Rust engine of nx, an LSP server
  and command line tool for Clojure. It builds the `nx` binary. The server
  part of nx lives in [mova-lsp](https://github.com/Vadym-Lopatka/mova-lsp).
- **`champ`** (`vendor/champ`): the persistent map, set, vector and text
  library Mova uses. It is also published alone as
  [mova-champ](https://github.com/Vadym-Lopatka/mova-champ).

## Status

Experimental. Mova is not a complete Clojure and the language and
embedding API can change. It was developed and tested on macOS on Apple
silicon. The Linux epoll path of the nREPL server compiles but has not
been run. Other platforms are untested.

## Install

You need Rust 1.94 or newer, installed with [rustup](https://rustup.rs).
(`vendor/champ` declares `rust-version = "1.94"`; no other manifest sets a
minimum. The build was checked with Rust 1.98.)

```sh
cargo install --locked --git https://github.com/Vadym-Lopatka/mova mova
```

This puts `mova` in `~/.cargo/bin`, which rustup adds to your PATH. Open a
new terminal and run `mova --version`.

To remove Mova:

```sh
cargo uninstall mova && rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/mova" "$HOME/.mova_history" "$HOME"/.cargo/git/checkouts/mova-* "$HOME"/.cargo/git/db/mova-*
```

This removes the binary, the core image cache (`mova/` under your cache
directory), the REPL history file, and the source that `cargo install --git`
downloaded (`checkouts/mova-*` and `db/mova-*` under `~/.cargo/git`; these
are cargo's download caches). Nothing else is touched. Mova also
reads nREPL config files (`.nrepl.edn`, `~/.nrepl/nrepl.edn`,
`~/.config/nrepl/nrepl.edn`) but never writes them, and `mova nrepl`
writes `.nrepl-port` in the directory where you start it and deletes it on
exit.

`cargo install --git` ignores the repository's `.cargo/config.toml`. A build
from a clone applies one allocator tuning for Apple silicon from that file;
the installed binary does not have it.

### Build from a clone

For contributors:

```sh
git clone https://github.com/Vadym-Lopatka/mova
cd mova
cargo build --release
```

The binary is `target/release/mova`. Developer helper binaries under
`src/bin` are built with `--features dev-bins`.

## Quick start

```sh
./target/release/mova                          # REPL
./target/release/mova examples/fizzbuzz.mova   # run a file
./target/release/mova -e "(+ 1 2 3)"           # evaluate an expression, print the result
./target/release/mova --module-path src:lib app.mova   # where (:require ...) looks
./target/release/mova --help
```

## nREPL server

```sh
./target/release/mova nrepl -p 7888
```

The server prints a banner and writes the port to `.nrepl-port` in the
current directory. It removes the file when it exits. Connect from CIDER,
Calva, Conjure or any other nREPL client. Options follow the JVM nREPL
command line (`-b`/`--bind`, `-p`/`--port`, `-s`/`--socket`, and others).
If you give no port, the server picks a free one (`-p 0` says the same).

To start the server for a project, pass the same `--module-path` that a
script gets (before or after `nrepl`). `(require ...)` then finds your files:

```sh
./target/release/mova nrepl --module-path src:lib
```

Supported ops: `clone`, `close`, `completions`, `describe`, `eval`,
`forward-system-output`, `interrupt`, `load-file`, `lookup`, `ls-sessions`,
`stdin`.

cider-nrepl and other JVM-only middleware are not supported. TLS needs the
`tls` cargo feature: `cargo build --release --features tls`.

The server runs any code a client sends, and it binds to 127.0.0.1 by
default. Read [SECURITY.md](SECURITY.md) before you change that.

## Tests

```sh
cargo test --release
```

Some tests use an external corpus and print `skipped: set MOVA_..._DIR`
when the directory is not set. Set one of these to run them:

- `MOVA_EDN_CORPUS_DIR` (EDN reader tests and bench)
- `MOVA_CLOJUREDOCS_DIR` (lazy map test)
- `MOVA_NX_CORPUS_DIR`, `MOVA_NX_LSP_DIR`, `MOVA_NX_E2TEST_DIR` (nx-core tests)

## License

EPL-1.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

## The pitch

- **Precise `Arc`, no tracing GC.** Every value is reference-counted
  (`Rc` in v0.1; `Arc` since v0.2's concurrency migration, so values are
  `Send + Sync` and shareable across real OS threads). There is no hidden
  root set, no stop-the-world pause, and no class of
  GC-finds-a-dead-object-still-live bugs — because there's no tracing GC to
  have that class of bug in the first place.
- **Persistent structures from [`imbl`][imbl]** (a maintained fork of
  Clojure-inspired `im`): RRB vectors and HAMT maps/sets with real
  structural sharing, not a copy-on-write `Vec` pretending to be Clojure's
  data structures.
- **[miette][miette]-grade diagnostics.** Every reader and runtime error
  carries a labeled source span, a real mova call stack, and — for system
  calls — the failing syscall's name and errno. Errors look like this:

  ```
  system error

    x couldn't slurp "/nope"
     ,-[repl:1:1]
   1 | (slurp "/nope")
     : ^^^^^^^^^^^^^^^
     `----
    system: open(2) failed: ENOENT — No such file or directory (errno 2)
  ```

- **`libc` without ceremony.** `(slurp p)`, `(spit p s)`, `(sh "cmd" ...)`,
  `(list-dir p)`, and friends are thin, direct wrappers around the real
  syscalls, with real errno on failure — not a generic `IOException`.

[imbl]: https://crates.io/crates/imbl
[miette]: https://crates.io/crates/miette

## The language and the REPL

Namespaces are real as of v0.5: `def` inside `(ns a.b)` interns `a.b/name`,
unqualified names resolve current-ns-first and then core, and `(ns x
(:require [a.b :as b] [c.d :refer [f]]))` loads `a/b.mova` from
the module path, once. Without `--module-path`, a file's requires resolve
against its own directory.

The REPL has line-editing/history (`~/.mova_history`) and multi-line input
(it keeps reading while a form is unclosed), plus `*1`/`*2`/`*3` result
history.

`examples/` has runnable showcases: `fizzbuzz.mova`, `macros.mova`
(defmacro/quasiquote/threading), `sys-tour.mova` (errno-aware system calls),
`async-tour.mova` (v0.2: go-loop producer/consumer, `alts!!` fan-in, a
timeout-select pattern, future/promise coordination), and `flow-tour.mova`
(v0.3: a `core.async.flow` pipeline with fan-out, the paused/resume
lifecycle, `ping` introspection, and a caught transform error with state
kept — see the flow section below).

## Feature table

| Area | What's there |
|---|---|
| Core forms | `def`, `fn` (multi-arity, variadic `& rest`, full destructuring params), `defmacro`, `let`, `if`, `do`, `loop`/`recur`, `quote`/`quasiquote`/`unquote`/`unquote-splicing` (with auto-gensym `x#` hygiene), `throw`/`try`/`catch`/`finally`, `#(...)` lambdas |
| Destructuring | sequential `[a b & rest :as all]` (incl. nested) and map `{:keys [..]} {:strs [..]} {:as m} {:or {..}}` patterns, everywhere a binding happens — `let`, `loop`/`recur` (re-destructures every iteration), `fn`/`defn` params (incl. the `[& {:keys [..]}]` variadic-kwargs idiom), `if-let`/`when-let` |
| Bootstrap macros | `defn` (docstring-tolerant, multi-arity), `when`, `when-not`, `if-not`, `cond`, `and`, `or`, `->`, `->>`, `if-let`, `when-let`, `dotimes`, `while`, `doseq`/`for` (`:let`/`:when` modifiers), `case`/`condp`, `letfn` (mutual recursion), `comment`, `lazy-seq` |
| Higher-order fns | `comp`, `partial`, `juxt`, `fnil`, `max-key`/`min-key`, `not-empty`, `gensym` |
| Collections | persistent `vector`/`list`/`hash-map`/`hash-set` with `conj`/`assoc`/`dissoc`/`get`/`nth`/`contains?`/`update`/`assoc-in`/`get-in`/`update-in`/`into`/`peek`/`pop`/`subvec`/`empty` — vectors, maps, sets, and keywords are all callable, like real Clojure |
| Lazy seqs | `lazy-seq`, `range`, `iterate`, `repeat`, `cycle`, `map` (1..N collections), `filter`, `remove`, `take`/`drop`/`take-while`/`drop-while`, `concat` — composes lazily and is stack-safe for 100k+-element pipelines (chunked internally, not per-element Rust recursion) |
| Seq library | `reduce`, `apply`, `sort`/`sort-by`, `distinct`, `group-by`, `frequencies`, `partition`/`partition-all`, `interpose`, `mapcat`, `some`/`every?`/`not-any?`/`not-every?`, `doall`/`dorun` |
| Strings | `str`, `pr-str`, `println`/`prn`/`print`, `name`/`keyword`/`symbol`, `subs`, and a `clojure.string`-ish `split`/`join`/`upper-case`/`lower-case`/`trim`/`starts-with?`/`ends-with?`/`includes?`/`replace` |
| System | `slurp`/`spit`/`sh`/`getenv`/`exit`/`read-line`/`file-exists?`/`delete-file`/`list-dir`/`cwd`/`time-ms`, all errno-aware |
| Concurrency | `atom`/`deref`/`@`/`swap!`/`reset!`; real-OS-thread `future`/`deref` (2-arity `(deref f timeout-ms default)` too), `promise`/`deliver`, `delay`/`force`, `sleep-ms` |
| core.async | real channels (`chan`, fixed/`dropping-buffer`/`sliding-buffer` policies, unbuffered rendezvous), blocking `>!!`/`<!!`, `close!` (drain-then-nil semantics), `put!`/`take!` (callback), `timeout`, `alts!!`/`offer!`/`poll!`, and `go`/`go-loop`/`thread`/`>!`/`<!`/`onto-chan!` (v0.2: real OS threads under the hood, not IOC state machines — see the async section below) |
| core.async.flow | `flow/create-flow`/`start`/`stop`/`pause`/`resume`/`pause-proc`/`resume-proc`/`ping`/`ping-proc`/`inject`/`process`/`map->step` — a native-Rust proc-graph engine, API-faithful to real `clojure.core.async.flow` v1.9.808-alpha1 (v0.3: see the flow section below) |
| Diagnostics | miette-rendered reader and runtime errors, mova call stack, errno + syscall name for system errors |

## Performance model (for arriving Clojure developers)

mova has a tier ladder, not a single execution mode: every `defn` body is
first tried through a whole-fn compiler that resolves symbols, arithmetic,
and calls into direct IR at definition time; a `loop`/`recur` inside a
compiled fn is further specialized into NumLoop (unboxed numeric-loop IR)
and, when its shape allows, vectorized lanes and superloops on top of
that; anything the compiler can't prove safe — interop dot-forms,
expression-position metadata, a handful of other shapes — `Bail`s to a
tree-walking interpreter that is a verified whole-fn fallback, not a
degraded mode: semantics are identical between tiers, only speed differs.
This is all on by default, with no flags and no build modes.

Three things about this invert JVM Clojure instinct, and are worth
knowing before you reach for old habits:

1. **Type hints do nothing for speed.** `Symbol` carries no metadata
   field, so a `^long`/`^longs`/`^String` hint on a param or binding is
   discarded before compilation — there's no reflection for a hint to
   bypass, because dispatch runs on the runtime `Value` variant directly.
   Clean code is the fast code; there's no annotation debt to pay down.
   `*warn-on-reflection*`/`:warn-on-boxed` still work as diagnostics —
   hints just don't change runtime behavior.
2. **Protocols are IC-cached and near-plain-fn now.** Protocol method
   dispatch on a `defrecord` runs through a lock-free per-protocol inline
   cache (the same pointer-identity design as `HostStruct` field access).
   Multimethods, fn-tables,
   and `cond` chains keyed on a keyword are all still fine, unchanged
   options. Pick your dispatch mechanism for openness and clarity — it's
   a design choice again, not a performance trap.
3. **The fast grammar is real but specific, and it tells you when you
   miss it.** Not every loop shape reaches NumLoop/lanes/superloops —
   `MOVA_EXPLAIN=1` prints one line per fn that tree-walks, or per loop
   that stays on the generic path, with a reason and a `file:line:col`;
   `(compile-explain f)` gives you the same decision from the REPL:

   ```clojure
   (defn dotty [s] (loop [i 0 acc 0]
     (if (< i 1000) (recur (inc i) (+ acc (.length s))) acc)))
   (println (compile-explain dotty))
   ;=> {:tier :tree-walk, :reason interop dot-form (.method/Ctor.) -- interop runs only in the tree-walker, :at ...:1:1}
   ```

   Zero cost when unused.

## v0.2: real concurrency, core.async, and a less-toy core

v0.1 was single-threaded (`Rc`-based values, no way to share mutable state
across threads). v0.2 migrated every `Value` to `Arc`/`Mutex`/`RwLock`
(`Send + Sync` throughout) and built real concurrency on top: OS-thread
`future`/`promise`/`delay`, and a `core.async`-flavored channel library —
`Mutex`+`Condvar` channels with real fixed/dropping/sliding buffer
semantics, unbuffered rendezvous, blocking and callback-based put/take, and
`alts!!` for select-style fan-in. `go`/`go-loop`/`thread` are documented as
**real OS threads, not IOC-rewritten state machines** — a deliberate v0.2
tradeoff (see `core/async.mova`'s module doc): don't spawn thousands of
`go` blocks, but do get genuine channel semantics you can differentially
verify against real Clojure (see the conformance section below).

```clojure
;; producer/consumer over a channel, plus a future/promise handoff --
;; see examples/async-tour.mova for the full tour (fan-in via alts!!,
;; a timeout-select "slow service" pattern, and more).
(let [ch (chan 10)
      producer (go-loop [i 0]
                 (if (< i 10)
                   (do (>! ch i) (recur (inc i)))
                   (close! ch)))
      consumer (go-loop [acc 0]
                 (let [v (<! ch)]
                   (if (nil? v) acc (recur (+ acc v)))))]
  (<!! producer)
  (println "sum of 0..9 =" (<!! consumer))) ;=> sum of 0..9 = 45
```

v0.2 also de-toyed the language core: full destructuring (sequential and
map patterns, everywhere a binding happens), `gensym` and auto-gensym
(`x#`) syntax-quote hygiene, and the `case`/`condp`/`doseq`/`for`/`letfn`/
`comp`/`partial`/`juxt`/`fnil` core.mova additions from the feature table
above.

## v0.3: core.async.flow — a native Rust proc-graph engine

**The pitch**: the ENTIRE flow runtime — proc loops, channel wiring, mult
fan-out, lifecycle, control-priority scheduling, diagnostics — is native
Rust (`src/builtins/flow.rs`); the interpreter is entered ONLY to call a
user step-fn's four arities (`describe`/`init`/`transition`/`transform`).
The public API (`flow/create-flow`, `flow/process`, `flow/map->step`,
`flow/start`/`stop`/`pause`/`resume`/`pause-proc`/`resume-proc`,
`flow/ping`/`ping-proc`, `flow/inject`) is API-faithful to real
`clojure.core.async.flow` v1.9.808-alpha1 — not "inspired by", but
**golden-verified against the actual JVM implementation**: a
corpus (`tests/conformance/corpus/flow.corpus`) runs through both engines
and diffs their output, with goldens generated by shelling out to real
`clojure.core.async.flow` (`tools/jvm-flow-runner.clj`), the same
differential-conformance discipline the rest of mova already holds
itself to (see below).

```clojure
;; a 3-proc pipeline: doubler -> fan-out to two sinks. See
;; examples/flow-tour.mova for the full tour (paused-by-default lifecycle,
;; ping introspection, a caught transform error with state kept, clean
;; stop).
(def out-ch (chan 20))
(def doubler
  (flow/map->step
   {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
    :transform (fn [s _ m] [s {:out [(* 2 m)]}])}))
(def sink
  (flow/map->step
   {:describe (fn [] {:ins {:in {}} :outs {}})
    :transform (fn [s _ m] (>!! out-ch m) [s {}])}))
(def fl (flow/create-flow
         {:procs {:d {:proc (flow/process doubler)} :s {:proc (flow/process sink)}}
          :conns [[[:d :out] [:s :in]]]}))
(flow/start fl)     ;=> procs start PAUSED
(flow/resume fl)
(flow/inject fl [:d :in] [1 2 3])
(println (<!! out-ch) (<!! out-ch) (<!! out-ch)) ;=> 2 4 6
(flow/stop fl)
```

`bench/` holds the flow benchmark programs and `bench/run.sh` runs them.
This tree does not include measured results.

Deviations from upstream (documented, reasoned, machine-checked against
the corpus) live in
[`tests/conformance/DEVIATIONS.md`](tests/conformance/DEVIATIONS.md)'s
"Phase F2 (flow.corpus)" section — no `:xform` chan-opts, no executor
pools/compute-timeout (every proc runs on its own OS thread regardless of
`:workload`), `ping`/`ping-proc`'s positional (not kwarg) timeout-ms, and
a few error-shape/unconnected-output edge cases.

## The conformance-harness story

The riskiest thing about building a second implementation of a language is
silent semantic drift — a stdlib function that's subtly wrong in a way unit
tests, written by the same person who wrote the bug, don't catch. mova's
answer is a **differential conformance harness**, `tests/conformance/`:

1. `tests/conformance/corpus/*.corpus` holds about 2,500 plain Clojure forms, one
   per line, grouped by area (`numbers`, `collections`, `seqs`, `strings`,
   `control`, `predicates`, `destructuring`, `higher-order`, `async`).
2. `tools/gen-golden.bb` runs every form through **real Clojure**
   (babashka v1.13 — same JVM-Clojure semantics, instant startup, and it
   ships `clojure.core.async` too, so `async.corpus`'s channel semantics
   get genuine differential coverage, not just our own unit tests) and
   records `OK\t<pr-str result>` or `ERR` next to it in a `.golden` file.
   Each corpus *file* is one babashka session, so later forms can build on
   earlier `def`s. A corpus file may open with a `;;PRELUDE <code>` comment
   line — evaluated once, bb-side only, before that file's forms (e.g.
   `async.corpus` uses it to `require` core.async's namespaced vars into
   scope under their bare names; mova needs no such prelude, since its
   `chan`/`>!!`/`go`/etc. are already global builtins).
3. `tests/conformance_test.rs` replays the same corpus through mova (one
   `Interp` per file, same session model) and diffs line-by-line against
   the golden file.
4. Any mismatch must be either a real mova bug — fixed, not shrugged
   at — or an explicit, reasoned entry in
   [`tests/conformance/DEVIATIONS.md`](tests/conformance/DEVIATIONS.md),
   a machine-checked table (`file | line | form | clojure | mova | why`).
   The test also asserts every whitelisted entry still actually mismatches,
   so a fixed deviation can't rot into a stale, misleading entry.

Regenerating goldens is one command: `bb tools/gen-golden.bb`. Known
differences are listed in DEVIATIONS.md with the reason for each.

This corpus already caught and fixed real bugs during development:
`map` not supporting multiple collections, `drop`/`range`/`repeat`
returning bare `nil` instead of an empty seq for exhausted/empty results,
`name` not stripping the namespace off a namespaced keyword, map
destructuring not implicitly `(apply hash-map ...)`-ing a seq value (which
is what makes both `(let [{:keys [a]} (list :a 1)] a)` and the `(fn [&
{:keys [..]}] ...)` variadic-kwargs idiom work in real Clojure), and
core.async's `offer!` collapsing "would block" and "closed" to the same
`false` instead of real Clojure's `nil`-vs-`false` distinction between
them.

**Roadmap**: point a `:mova` reader-conditional branch at
[jank-lang/clojure-test-suite](https://github.com/jank-lang/clojure-test-suite)
(or a similar upstream corpus) once mova has enough surface area
(destructuring landed in v0.2, namespaces in v0.5; protocols are still
deferred, see below) to run more of it unmodified, turning this from a
hand-authored corpus into continuous coverage against upstream Clojure's
own test suite.
