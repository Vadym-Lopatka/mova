# flow-gold divergences

A `;;DIVERGENT` scenario (see `SPEC.md`'s "Scenario format") is one whose
correct, per-side output legitimately differs between the JVM gold and
mova -- not a bug, a documented semantic difference. For such a scenario:

- `goldens/NN-name.golden` still holds the JVM output (generated the same
  way as every other scenario, by `tools/gen-flow-gold.bb`).
- THIS file holds mova's expected output, in an entry keyed by the
  scenario's basename (no `.mova` extension).
- `tests/flow_gold_test.rs` compares mova's actual output against this
  file's entry instead of the golden for any scenario whose file has a
  `;;DIVERGENT` header directive.

This file plays the same role for flow-gold that
`../DEVIATIONS.md` plays for the corpus lane, including its honesty rule:
**an entry that no longer actually diverges (mova's output now equals the
golden too) fails the test as stale**, and so does an entry with no
corresponding `;;DIVERGENT` scenario, or a `;;DIVERGENT` scenario with no
entry here. Keep this file and the scenarios' own directives in sync.

## Entry format

`tests/flow_gold_test.rs`'s `parse_divergences` parses this file with a
small, deliberately dumb state machine -- read its doc comment for the
authoritative parsing rule. The shape every entry must follow (this illustration deliberately spells
its markers `DIVERGENCE EXAMPLE` / `END DIVERGENCE EXAMPLE` instead of the
real `DIVERGENCE: ` / `END DIVERGENCE` -- a REAL entry using the real
markers, sitting inside this very file's own doc comment, would otherwise
be parsed as a live orphaned entry by the dumb line-scanner below and fail
the test; do not "fix" that by making this example use real markers):

```
<!-- DIVERGENCE EXAMPLE: scenario-name -->
Prose explaining WHY the two sides diverge, with file:line evidence from
BOTH implementations (gold source under $FLOW_GOLD_ROOT
and mova source under src/). This prose is free-form and NOT parsed --
write it for a human, the same honesty standard as DEVIATIONS.md's own
table rows.

```text
RESULT some-deterministic-verdict-line
PASS some-invariant-name
```
<!-- END DIVERGENCE EXAMPLE -->
```

A real entry drops the `EXAMPLE`: opens with `<!-- DIVERGENCE: scenario-name -->`
and closes with `<!-- END DIVERGENCE -->`.

Rules the parser enforces / assumes:
- `scenario-name` must exactly match a `scenarios/scenario-name.mova` file's
  basename (no extension).
- Between the two `<!-- DIVERGENCE ... -->` / `<!-- END DIVERGENCE -->`
  marker lines, the FIRST fenced block opened by a line that is *exactly*
  ` ```text ` and closed by a line that is *exactly* ` ``` ` holds mova's
  expected stdout: one real output line per fence line. The parser
  reconstructs the expected byte stream by appending `\n` after every fence
  line -- so the fence's content must look exactly like the terminal
  output mova actually prints, with no stray leading/trailing blank line
  inside the fence.
- Every entry needs both markers; a missing `<!-- END DIVERGENCE -->`, a
  missing ` ```text ` block, or a duplicate scenario-name entry all panic
  the test loudly (better than a silently-wrong golden compare).
- Any other prose/markdown in the file (including this header) is ignored
  by the parser -- only text between the markers, and only the first
  fenced block within them, is load-bearing.

## Entries

Wave 1 landed four divergences (below). `15-ping-shape` was predicted as a
candidate but probing found IDENTICAL ping-reply key sets on both sides, so
it has no entry -- per this file's own rule, entries exist only for
observed, stable mismatches.

<!-- DIVERGENCE: 11-multi-input-scheduling -->
Gold's running-state read loop (`impl.clj:288-295`) takes
`(async/alts!! read-chans :priority true)` over a vector built by
`reduce-kv` iterating the proc's `ins` map in ITS insertion order (a
`:ins {:a {} :b {}}` describe literal is a small `PersistentArrayMap`, so
:a always precedes :b) -- with `:priority true`, `alts!!` always prefers
the first ready channel. Since port :a is kept continuously non-empty by
the scenario's pre-buffered flood, gold NEVER services port :b until :a
is fully drained: :b's message comes out dead last (position 201 of 202,
0-indexed). mova's read loop (`src/builtins/flow.rs:1674-1716`)
round-robins across the read-set via `rr_index`, advancing after every
successful receive; with both ports ready from the start this alternates
strictly A,B,A,B,..., so :b's message comes out second (position 1).
Both sides verified internally deterministic, 5/5 identical runs each,
via the exact scenario body in `scenarios/11-multi-input-scheduling.mova`
(reduced-N probe first at N=50, then re-confirmed at the shipped N=200).
See FINDINGS.md finding 2.

```text
RESULT total-drained 202
RESULT b-position 1
```
<!-- END DIVERGENCE -->

<!-- DIVERGENCE: 13b-restart-after-stop -->
Gold's `Graph/start` (`impl.clj`) only short-circuits to
`{:already-running true, ...}` when `@chans` is still truthy; `stop`
resets `@chans` to `nil`, so a `start` called AFTER a `stop` finds
`@chans` nil and proceeds through the full fresh-channels/fresh-proc-
threads path exactly like a first start -- restart SUCCEEDS and the flow
is fully functional again, matching `start`'s own docstring ("can be
started again"). Confirmed by direct probe (not just the docstring): a
`flow/start` immediately after `flow/stop` returns normally, no
exception, `restart-ok` prints `true`. mova's `native_start` only
accepts a flow in `FlowPhase::Created`; `stop` transitions it to
`FlowPhase::Stopped`, which never transitions back to `Created`, so a
second `flow/start` always errors -- already pinned by
`tests/flow_test.rs`'s
`starting_a_stopped_flow_again_errors_restart_is_not_supported` (message
contains "already been started"). Verdict-ized via try/catch.

```text
RESULT restart-ok false
```
<!-- END DIVERGENCE -->

<!-- DIVERGENCE: 14-unconnected-out -->
SPEC.md's catalog flagged this scenario "recon-flagged
possibly-divergent... gold behavior UNKNOWN -- probe it!"; probed
directly and it IS divergent. Gold: the proc-level `outs` map has
:dead-end -> nil (no conn means create-flow's `out-chans` entry for that
coordinate is nil). `send-outputs`'s lookup
`(or (outs output) (spi/get-write-chan resolver output))` falls through
to `get-write-chan` on the BARE local out-id `:dead-end` -- but
`get-write-chan`/`write-chan` only resolves FULL `[pid id]` coordinates
via `in-chans`/`out-chans` (both keyed by `[pid cid]` pairs), so a bare
keyword is never found and it throws `"can't resolve channel with
io-id"`. That exception is caught by the proc's own per-message
`try`/`catch` and routed to the error-chan like any other transform
error -- gold ERRORS (via the error-chan) on every unconnected-out
write. mova (`tests/flow_test.rs`'s `unconnected_output_silently_drops`,
also independently re-confirmed here): builds no transport at all for an
unconnected out-port and simply discards those messages -- no error is
ever raised, on the error-chan or otherwise. Both sides still deliver
the CONNECTED :out port normally regardless. Confirmed 5/5 stable each
side.

```text
RESULT connected-out :hi
RESULT unconnected-out-errored false
```
<!-- END DIVERGENCE -->

## L4 (supervision): mova-native additions NOT exercised by any scenario

Not table/marker entries (per this file's own rule -- a `<!-- DIVERGENCE:
... -->` entry needs a corresponding `;;DIVERGENT` scenario file and a
golden to diverge FROM; no flow-gold scenario configures `:supervision` at
all, by design -- see below), free-form the same way
`../DEVIATIONS.md`'s own "Phase F2 (flow.corpus)" section is: real,
deliberate additions, disclosed here rather than silently absent.

L4 (docs/L4-SUPERVISION-DESIGN.md, docs/L4-LANDING-SPEC.md) adds three
mova-native surfaces with no upstream counterpart at all -- not a
divergent behavior for the SAME feature, but a feature upstream does not
have:

- **`flow/stop-proc`.** Upstream's Graph protocol
  (`clojure.core.async.flow/Graph`) has pause/resume/ping and their
  `-proc` variants, but no per-proc STOP -- confirmed by direct source
  read, not inference (design doc §1: "The Graph protocol has no per-proc
  stop/start"). There is no `(stop-proc g pid)` to diverge from; mova's
  version (`src/builtins/flow.rs`'s `native_stop_proc`) is registered and
  shaped exactly like `pause-proc` (same arity, same errors) precisely so
  a reader has only the SEMANTICS to learn, not a new shape.
- **The `:supervision` config key** (`create-flow`'s proc-level and
  flow-level map) and the restart policy it drives. Upstream has no
  restart at all -- `git grep -i restart` is zero hits on both `master`
  and `dev-flow-alpha` (design doc §1) -- and the proc-loop future returned
  by `start` is simply DISCARDED at spawn (`impl.clj:298`), so upstream
  cannot even OBSERVE a proc's death, let alone restart it. This is not an
  invented mechanism out of nowhere, though: the SPI explicitly sanctions
  restart-by-re-init as a valid implementation strategy ("A launcher may be
  called upon to start a process more than once, and should start a new
  process each time start is called", `spi.clj:20-22`) -- L4 is upstream-
  permitted behavior upstream itself never built.
- **`report-chan`'s lifecycle-event producers** (`:proc-exit`/
  `:proc-restart`/`:proc-give-up`/`:proc-wedged`/`:proc-stopped`). Upstream
  has no equivalent channel or event stream for proc lifecycle at all --
  its only liveness signal is `ping`'s own timeout-bounded reply. mova's
  `report_chan` existed pre-L4 (allocated and returned by `flow/start`)
  but had ZERO producers until this wave (design doc §1, grep-verified) --
  L4 is the first thing that ever writes to it.

**All three are corpus-neutral by construction, not by omission.** Every
one is gated behind an opt-in `:supervision` key that no flow-gold scenario
sets (a supervision-config surface has no portable JVM-vs-mova behavior to
pin -- upstream has nothing on the other side of the comparison to record a
golden against), so their existence changes NOTHING about any scenario's
observable output: `flow/start`'s return map, `report-chan`'s post-stop
closure (scenario `13-stop-semantics`, conformant -- no scenario sets
`:supervision`, so no L4 lifecycle event is ever in flight for `stop`'s
close to race), and every other golden stay byte-identical whether or not
L4 is compiled in. `MOVA_NO_SUPERVISION=1` is the belt to that braces --
parses and validates the config, then drops it with a loud stderr note,
restoring the exact pre-L4 engine.
