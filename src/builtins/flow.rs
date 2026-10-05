//! `core.async.flow` on a native Rust engine (Phase F1). See FLOW-DESIGN.md
//! for the contract this module implements exactly; that file's shorthand
//! `::flow/foo` always means the literal fully-qualified keyword
//! `clojure.core.async.flow/foo` (mova has no reader-level `::` auto-resolve
//! -- see `kw()` below), matching real core.async.flow's keyword names for
//! maximal upstream compat.
//!
//! ## Architecture
//!
//! The ENTIRE flow runtime -- proc loops, channel wiring, mult fan-out,
//! lifecycle, diagnostics -- is native Rust here; the interpreter (a forked
//! `Interp` per proc, see `Interp::fork`) is entered ONLY to call a user
//! step-fn's four arities (describe/init/transition/transform). This module
//! drives `builtins::async`'s `Chan` primitives directly (`chan_take`,
//! `chan_put`, `chan_try_take`, `chan_try_put`, `chan_close`, promoted to
//! `pub(crate)` there for exactly this) rather than going through the
//! `>!!`/`<!!`/`alts!!` natives, matching this module doc's counterpart in
//! `builtins::async`.
//!
//! `Value::Flow(Arc<FlowCell>)` (data in `value.rs`, mirroring `Chan`'s own
//! "data in value.rs, logic here" split) holds an immutable `FlowDef` (the
//! validated `create-flow` cfg) plus a `phase`/`runtime` pair populated by
//! `start` and torn down by `stop`.
//!
//! ## Control-priority wait design (the one deliberately-documented
//! tradeoff this phase makes)
//!
//! Real core.async's proc loop is `(alts!! [control [outc msg]] :priority
//! true)` -- a single primitive that blocks until EITHER a control command
//! OR a data slot is ready, waking instantly on whichever comes first, with
//! control preferred on simultaneous readiness. Each `Chan` owns its own
//! private `Condvar` (see `builtins::async`'s module doc), which is 1:1 with
//! one `Mutex` and therefore cannot by itself block on "any one of N"
//! sources -- so instead of a single `alts!!`-shaped primitive, every proc
//! gets its own [`Doorbell`] (`value.rs`; a small `Mutex<u64>` generation
//! counter + `Condvar`) and registers it with every event source its wait
//! loop currently cares about: its control chan, every chan in its
//! read-set, its inject chan (the same chan as above -- see
//! `InjectPort`'s doc), and its transport lane if it has one. A put/close on
//! ANY of those rings the shared doorbell (`builtins::async`'s 5
//! `notify_all()` call sites each also ring `ChanState.doorbell` when one is
//! registered), so a proc parked on its own doorbell wakes the instant
//! anything it's registered on changes, without polling:
//!
//! - **Single-input fast path** (the common case: most procs read exactly
//!   one in-port): `try_take_with_timeout` parks on the proc's `Doorbell`
//!   (not the data chan's own condvar) with `PARK_TIMEOUT` as a defensive
//!   safety-net cap, then the outer loop re-checks control non-blocking
//!   before trying again. Control latency is therefore wake-driven in
//!   practice (the safety net is a backstop, not a live mechanism -- see
//!   that constant's doc); ZERO added latency when messages are flowing
//!   (the non-blocking `chan_try_take` fast path never waits at all), and
//!   the hot path allocates nothing per message.
//! - **Multi-input**: no single chan to block on, so it's still a
//!   non-blocking round-robin scan across the (small, cached) read-set, but
//!   the "nothing was ready this pass" tail now snapshots the doorbell's
//!   generation BEFORE the scan and parks on it (bounded by `PARK_TIMEOUT`)
//!   instead of an unconditional `sleep` -- so a message landing on ANY
//!   read-set chan, or a ring from a source that fired during the scan
//!   itself, wakes the proc immediately rather than after up to
//!   `MULTI_INPUT_BACKOFF`.
//! - **Blocked sends**: `>!!`-ing an output whose downstream is full uses
//!   the identical fused pattern ([`send_with_control_priority`]: try_put;
//!   on WouldBlock, check control non-blocking, then wait for room, then
//!   retry) -- this is what "control priority raced against every blocked
//!   send" means in practice: a proc stuck trying to deliver output to a
//!   full downstream still notices `stop` promptly instead of hanging until
//!   the downstream drains. The WAIT is the one place the two proc worlds
//!   fork (L3/W1b): a THREAD proc takes a short, UNCHANGED-from-pre-fix
//!   capped condvar wait on the target chan (`BLOCKED_SEND_TIMEOUT`;
//!   deliberately not a `ChanState::doorbell` registration, since that slot
//!   belongs to the target chan's own reader waiting for data, and a proc
//!   blocked on backpressure is by construction not idle -- see that
//!   constant's doc). A TASK proc takes [`blocked_send_task`], which pushes
//!   the proc's own doorbell into the target's separate `alts_doorbells`
//!   FAMILY (a `Vec`, so no slot is contended for) and parks there: one
//!   doorbell, two ring sources (room freed on the target, a command on
//!   control), one park, wake-driven latency on both arms.
//!
//! Every control check point (idle-park, pre-take, mid-batch every 8, mid-
//! send) shares one function, [`apply_control`], so behavior is identical
//! regardless of where control was noticed.
//!
//! **Missed-wakeup correctness** is [`Doorbell`]'s job, not this module's --
//! every park site here follows the same rule its doc states: snapshot the
//! generation BEFORE the non-blocking scan, park only if it's unchanged
//! after. See `value.rs`'s `Doorbell` doc for the full argument.
//!
//! ## Three proc loops, one set of semantics (v0.3 / N2 / P3)
//!
//! [`run_proc`] is the general one: it `Interp::call`s a 4-arity step-fn
//! per message and parses its `[state' {out-id [msgs...]}]` return.
//! [`run_proc_fast`] is the promoted one, taken when (and only when)
//! [`try_promote_fast`] says a proc's step is a `Value::Native` carrying a
//! `StepFactory` AND the topology is single-in/≤1-out/no-input-filter: the
//! step's state then lives in Rust (`FastStep`), and no message enters the
//! interpreter at all. See NATIVE-STEP-DESIGN.md. [`run_fused`] is the
//! third: ONE thread for a whole unbranching 1:1 chain of procs, calling
//! each member's transform back-to-back per message with no channel hop
//! between them (P3, see [`plan_fusion`] and FLOW-DESIGN.md's "Fusion"
//! section) -- each member keeps its own pid/state/count/status, and each
//! member is independently either interpreted or `FastStep`-promoted, so
//! the fused loop is a composition of the other two rather than a third
//! set of semantics. All three share the park primitive, every timing
//! constant, the blocked-send loop, and the error/ping map builders; the
//! kill switches `MOVA_NO_FASTSTEP=1` and `MOVA_NO_FUSION=1` force every
//! proc back onto the generic loop and onto its own thread respectively.
//!
//! ## Transport selection: which conns leave the general `Chan` (V05 E2/T2)
//!
//! A `Chan` hop costs ~2.4µs (mutex + condvar + a thread park/unpark round
//! trip); `crate::transport`'s 1:1 SPSC kernel costs ~84-99ns for the same
//! hop. Where a conn's traffic is genuinely one producer thread to one
//! consumer thread, the engine carries it on the kernel instead. See
//! FLOW-DESIGN.md's "Transport selection" section for the user-facing
//! statement of everything below.
//!
//! **The topology rule** ([`plan_transport_links`]). A conn
//! `[[A a-out] [B b-in]]` moves to a transport iff:
//! `a-out` has exactly ONE destination (so no mult is in the path),
//! `b-in` has exactly ONE source (fan-in shares one chan per destination
//! in-port -- see the wiring comment below -- so a second source would mean
//! a second producer), `A != B` (a self-loop always routes via mult),
//! `a-out` is A's only out-port and `b-in` is B's only in-port (so each
//! proc has at most ONE lane, and the multi-input round-robin never has to
//! carry more than one), the destination's `:buf-or-n` is at least 1,
//! NEITHER proc is a member of a multi-proc fused run, and the flow is not
//! running under `(simulate {:seed ..})`.
//!
//! **There is no longer a task-proc clause** (L3.6/W1, docs/
//! FLOW-HOP-RECOVERY.md §7). L3 §3.2 excluded any conn touching a task proc
//! because `transport.rs`'s `Ring` parked on `std::thread::Thread` identity;
//! since `1c00c5d` made procs tasks by default that exclusion applied to
//! essentially every hop, and it was measured to cost 280 ns/message -- the
//! entire post-flip throughput regression. The `Ring` now has a task arm
//! (its "THE TASK ARM" section) and the clause is gone: a proc-to-proc hop
//! gets its lane whichever tier the two procs run on.
//!
//! The FUSED-RUN clause above is what makes the three tiers a strict
//! priority order rather than an interaction: **fused > transport > `Chan`**.
//! A fused run
//! has already deleted its internal hops (one thread, no channel at all),
//! so a transport there would be dead weight; excluding those pids means
//! [`run_fused`] needs no transport awareness whatsoever, and a run that
//! DEMOTES at init simply falls back to plain `Chan`s -- correct, just
//! unaccelerated. Under `MOVA_NO_FUSION=1` every run is a singleton, so
//! every eligible conn gets a transport; that combination (native chain,
//! unfused) is where the transport does the most visible work.
//!
//! **Composition with control/pause/stop.** The lock-free SPSC kernel's own
//! wake protocol (`transport.rs`'s module doc, "THE WAKE PROTOCOL") is the
//! most safety-critical, most heavily-proven code in this crate, and its
//! CAS/`PARKED`-bit protocol for data readiness / room-freed / close is
//! completely UNTOUCHED by the `Doorbell` mechanism -- not one line of
//! `Ring::publish`/`seal`/`push`/`pop`/`wait_for_data`/`wait_for_room`
//! changed. What DID change is additive: `Ring` gained two new SIBLING
//! methods, [`transport::Ring::wait_for_data_until_or_doorbell`] and
//! [`transport::Ring::pop_timeout_or_doorbell`] (used through
//! [`transport::SpscRx::take_timeout_or_doorbell`]/`_cold`), which are
//! `wait_for_data_until`/`pop_timeout` with exactly ONE extra branch: after
//! the spin budget is exhausted (same cadence as the existing deadline
//! check, never inside the hot spin loop), also return early if a
//! `Doorbell`'s generation has moved. Two things make this both safe and
//! actually effective, not merely harmless:
//!
//! - **Why it's needed at all**: `Doorbell::ring` (`value.rs`) now also
//!   calls `Thread::unpark()` on the proc's own OS thread, in addition to
//!   its `Condvar` notify -- that's what interrupts an in-progress
//!   `thread::park_timeout` inside the ring. But invariant 7 ("`park()` is
//!   never trusted... every wait re-checks the real condition") means an
//!   unpark ALONE only buys one extra loop iteration; without the
//!   generation check, that iteration finds neither `tc != head_count` nor
//!   `CLOSED` and just re-arms `park_timeout` for the REMAINING deadline,
//!   going back to sleep -- measured directly: an earlier version of this
//!   fix shipped the unpark half alone, widened the transport read
//!   timeout to the long `PARK_TIMEOUT` safety net, and
//!   `transport_lifecycle_surface_matches_the_chan_path` (a `flow/ping
//!   ... 2000` against a paused transport-backed mid-chain proc) went from
//!   passing in ~2s to a DETERMINISTIC failure (a status came back `nil`,
//!   never observed within ping's own 2000ms budget) -- the unpark woke
//!   the thread, and the untouched loop put it right back to sleep anyway.
//! - **Why it's still safe**: the generation check's early return is
//!   provably harmless by the EXACT argument the module doc already makes
//!   for the deadline check it sits beside -- "timing out early is always
//!   safe... nothing has been consumed... the caller simply loops" -- so
//!   this doesn't weaken any of the ten numbered invariants, it just adds
//!   a second, independent reason to reach the SAME `TimedWait::TimedOut`
//!   outcome those invariants already cover.
//!
//! With that in place, a transport-backed read now uses `take_timeout_or_
//! doorbell`/`_cold` (snapshotting `doorbell.current()` first, per the
//! generation-counter "missed-wakeup correctness" rule) at the SAME long
//! `PARK_TIMEOUT` bound the general `Chan` path uses -- not a separate
//! short constant -- because a genuine control/inject ring now reaches it.
//! A transport-backed send keeps that same shape on a THREAD: `try_put`,
//! then `chan_try_take(control)`, then a capped `SpscTx::wait_writable` at
//! the short `BLOCKED_SEND_TIMEOUT` -- backpressure is real signal, not the
//! idle case, so it keeps the pre-fix bound exactly like the `Chan`-based
//! blocked-send path does.
//!
//! **L3.6/W1 added the room direction's `_or_doorbell` sibling, for TASKS
//! only.** The thread arm above needed none: its 1 ms cap IS the
//! control-latency mechanism, so a `pause`/`stop` arriving while the
//! downstream is full is noticed on the next lap regardless. A task's park
//! has no timer at all, so a task blocked on a full lane with only the ring
//! registered would sleep through `stop` until room appeared.
//! [`transport::SpscTx::wait_writable_or_doorbell`]/`_cold` register the
//! proc's own doorbell as a second source -- rung by every control command,
//! since `run_ready` puts that doorbell in the control chan's
//! `ChanState::doorbell` slot at spawn -- which makes the task arm's control
//! latency the ring's, strictly better than the thread arm's 1 ms of poll.
//! [`out_send`] picks the pair by `runtime::in_task()`, so the thread path
//! is byte-for-byte what it was. Every
//! control-check point, the batch-drain shape, the pause/resume/stop
//! bookkeeping, the transition calls and the error reporting are still the
//! same code on both paths -- [`in_take_timeout`]/[`in_try_take`]/
//! [`out_send`] are the only three places that branch at all.
//!
//! **`flow/inject`.** An injection is a third writer arriving on its own
//! thread, which a strictly-1:1 transport cannot carry. So a
//! transport-backed in-port keeps its `Chan` wired as the injection side
//! channel (`FlowRuntime::initial_ins` is unchanged, and `stop` still
//! closes it), and its proc drains both. The consumer only touches that
//! chan's mutex when [`InjectPort::gate`] says something is there, so the
//! hot loop pays one relaxed atomic load per lap and nothing else.
//! Documented consequence, the same one a fused run already has: an
//! injected message interleaves with the conn's own traffic at LAP
//! granularity rather than by strict channel FIFO.
//!
//! **Kill switch.** `MOVA_NO_SPSC=1`, read once per process (an
//! `OnceLock`, like every other switch here): [`plan_transport_links`]
//! returns nothing, no lane is built, and every conn is wired byte-for-byte
//! as it was before this feature existed.
//!
//! **Buffers.** A transport link's capacity is the destination in-port's
//! `:buf-or-n` (default 10) exactly -- `SpscRing` enforces the requested
//! number verbatim rather than its rounded-up slot count, so backpressure
//! begins in the same place as the `Chan` it replaced. `:buf-or-n 0` is
//! deliberately NOT converted: the engine builds `BufferPolicy::Fixed(0)`
//! for it, whose `chan_put` can never find room, and reproducing that
//! degenerate shape on a transport would mean *changing* it (a `Handoff`
//! rendezvous works, where `Fixed(0)` wedges). The engine therefore never
//! constructs an unbuffered conn chan at all, and `Handoff` correspondingly
//! has no call site here -- recorded as a finding, not an oversight.
//!
//! ## Task procs: `:workload` becomes functional (L3, THE DEFAULT)
//!
//! docs/L3-FLOW-PROCS-DESIGN.md is the design; docs/L3-LANDING-SPEC.md §W1a
//! and §W1b are what landed. Until L3 every proc was an OS thread and
//! `flow/process`'s `:workload` was stored inert -- a documented deviation
//! from upstream, whose whole point is that an `:io` proc may block while a
//! `:compute`/`:mixed` one should ride a pool. The keyword now means what
//! upstream says it means, in the DEFAULT build:
//!
//! **Where `:workload` comes from (L3.5).** Matching upstream's `process`
//! docstring exactly: an explicit `:workload` in `flow/process`'s opts
//! overrides any `:workload` the proc's own `:describe` fn returns; if
//! neither supplies one, the default is `:mixed`. [`resolve_workload`]
//! computes this ONCE per proc, at `create-flow` validation time (right
//! where the proc's one-and-only `describe()` call already happens), and
//! `native_create_flow` writes the answer straight back into the
//! `ProcDef`'s stored launcher (normalized to the
//! `{:mova.flow/step sf :mova.flow/workload w}` map shape even for a bare
//! step-fn/var launcher) -- so [`proc_workload`]'s single-source read off
//! that launcher, unchanged from pre-L3.5, is already reading the fully
//! resolved answer. Every consumer below calls [`proc_workload`] rather
//! than re-deriving anything, so the spawn decision and the transport
//! clause can never disagree about what a proc is.
//!
//! - A proc whose [`Workload`] is `:mixed` (the default, and what a bare
//!   fn/var/native launcher gets) or `:compute` spawns as a TASK on the
//!   L1/L2 runtime (`crate::runtime::spawn`) instead of on its own
//!   `std::thread`. A 10,000-proc graph is a benchmark rather than a thread
//!   count, and a co-shard proc-to-proc hop rides L2's ~50 ns rendezvous
//!   where it paid a futex-class ~1-4 µs before.
//! - `:workload :io` keeps its OS thread -- upstream's own escape hatch for
//!   a proc that blocks, and the remedy for the "lying workload" failure
//!   mode (a `:mixed` proc that blocks in a native stalls its shard;
//!   visible, not corrupting -- design §7 R1). A proc body that sleeps or
//!   blocks in natives belongs in `:io`.
//! - `MOVA_FLOW_THREAD_PROCS=1` ([`flow_task_procs_enabled`], an `OnceLock`
//!   env read exactly like [`spsc_disabled_by_env`]) is the KILL SWITCH,
//!   mirroring `MOVA_GO_THREADS=1`: it forces every proc back onto its own
//!   thread, which is the pre-L3 engine byte for byte. Both worlds are
//!   gated -- `cargo test --release --test flow_gold_test` runs the corpus
//!   with the switch unset AND with it set.
//!
//! Three consequences the rest of this module has to live with:
//!
//! - **Task procs ARE transport-eligible, as of L3.6/W1.** This bullet used
//!   to say the opposite, and the reversal is the single biggest throughput
//!   change in this module's history: `transport.rs`'s `Ring` grew a task
//!   arm (a two-source park -- the ring's waiter slot AND the proc's
//!   `Doorbell`, registered and retracted together), so
//!   [`plan_transport_links_with`]'s task clause is gone and both
//!   `debug_assert!`s that guarded it are gone with it. What is left in
//!   their place is a REAL fork, at the park step of each direction:
//!   [`in_take_timeout`]'s lane arm and [`out_send`]'s lane arm each choose
//!   a thread park or a task park by `runtime::in_task()`, and the task park
//!   registers on the SAME `doorbell` (against the SAME `seen` snapshot) the
//!   `Chan` path would have used. See docs/FLOW-HOP-RECOVERY.md §7 for the
//!   measurement that motivated it and the correctness argument in full.
//! - **Fusion and task-hood compose by the simplest correct rule**: a fused
//!   run is task-spawned iff EVERY member is a task proc; a MIXED run (an
//!   `:io` member fused with a `:compute` one) stays an OS thread, exactly
//!   as it was. Nothing in [`plan_fusion`] changes -- only the spawn call
//!   at the bottom of [`native_start`] branches -- so a run is never split
//!   and the two planners can never disagree.
//! - **A task proc has no `JoinHandle`, so `stop` waits on a done-cell.**
//!   EVERY proc, task or thread, owns a dedicated `done` chan
//!   (`ProcRuntime::done`) that carries no value and whose only signal is
//!   `closed`; the run's spawn closure closes it through an [`ExitGuard`]
//!   `Drop` guard, so every exit path signals, panics included.
//!   [`stop_flow_cell`] joins a thread run's head and waits on the cell for
//!   everything else -- uniform across both worlds, which is why nothing
//!   there has to ask which world it is in. Control chans keep their exact
//!   pre-L3 lifecycle: they are ordinary engine-owned chans that `stop`
//!   closes at the end, and nothing observes them closing early.
//!
//! **Placement is the engine's business, not the runtime's** (L3 §3.6, W3).
//! A task run is spawned with [`crate::runtime::spawn_on`] and an explicit
//! shard, because `flow/start` knows something the runtime cannot: the procs
//! form a pipeline, and neighbors hand each other every message. The rule is
//! [`segment_shards`] -- BFS the conn graph from its sources, cut the walk
//! into contiguous segments of at most [`MAX_SEGMENT_LEN`] procs, one shard
//! each, starting from a process-wide round-robin base so successive flows
//! don't stack. Every part of that is a W3/P4a measurement rather than a
//! preference: blind round-robin makes every hop cross-shard (20.9 ns/hop
//! on a 10k chain), whole-chain-one-shard runs the pipeline on one core
//! (64), one-segment-per-shard leaves each shard a 715-deep serial stretch
//! (47.8), and the capped form measures 17.6.
//! `MOVA_FLOW_PLACEMENT=rr` restores blind round-robin (the A/B lever and
//! the escape hatch); `seg:<L>` retunes the cap and `pin:<N>` is a probe
//! seam, both documented on [`Placement`]. Thread runs are untouched in
//! every mode.
//!
//! **The mult (fan-out) too, since L3.5 item 2.** L3 left it an OS thread
//! per fan-out/self-loop out-port -- the last thread the default world
//! spawned per graph node -- because design §3.7 read the conversion as
//! needing "a doorbell-based backoff" first. It does not, and §3.7 is
//! superseded: the mult has NO control-chan awareness by design
//! (FLOW-DESIGN.md -- its only job is draining `source` so the producer
//! never blocks, and it exits when `source` closes), so there is nothing
//! for a doorbell to race a blocking op against. Both of its waits are
//! already task-native after L1/W3 (`chan_take` parks in `task_takers`,
//! `chan_put` in `task_putters`), which makes the task mult the plain loop:
//! blocking take, `try_put` fast pass, blocking `chan_put` per straggler,
//! untap whatever came back closed. What did NOT survive is the 200 µs
//! [`MULTI_INPUT_BACKOFF`] sleep -- a task must never sleep, it burns the
//! whole shard -- and it is deleted rather than replaced. See
//! [`run_mult_task`] (default world) and [`run_mult_thread`] (unchanged,
//! `MOVA_FLOW_THREAD_PROCS=1` only) for the full argument, including why
//! the self-loop shape cannot deadlock.
//!
//! Still not converted, by design: [`join_with_timeout`]'s poll, for the
//! reason one level up -- it is only reachable from a THREAD caller of
//! `stop` (see [`wait_done_with_timeout`]'s doc), so there is no shard to
//! burn there either.
//!
//! `native_ping`/`native_ping_proc`'s poll cadence and
//! [`wait_done_with_timeout`]'s task arm WERE converted, in W2, once
//! `builtins::async::timer_arm` was promoted to `pub(crate)` for exactly
//! this ([`park_tick`] is the one call site every de-polled straggler
//! shares): a `flow/ping` from inside a `go` block, and `flow/stop` from
//! inside a task waiting on a proc's done-cell, both now park on a fresh
//! one-shot timeout chan instead of burning their shard on `thread::sleep`,
//! at the same cadence as before. W1b's "no deadline" gap in
//! `wait_done_with_timeout`'s task arm is also closed as part of this (see
//! that function's doc and [`stop_flow_cell`]'s doc for the single-shared-
//! deadline fix that landed alongside it).
//!
//! Two smaller facts worth writing down. A task proc's `Doorbell` is built
//! inside `run_ready` ON A SHARD THREAD, so its `owner_thread` is the
//! SHARD's thread; `ring()`'s `unpark()` of it is spurious but harmless (the
//! shard loop never trusts `park()`), and it is the task-waker half of the
//! same `ring()` that actually wakes the proc. And a task's stack is
//! `runtime::TASK_STACK_SIZE` (8 MiB), not [`PROC_STACK_SIZE`] (64 MiB): a
//! `transform` doing genuinely deep interpreted recursion has eight times
//! less headroom as a task than as a thread, which is a reason to run such a
//! proc `:io`, not a reason to grow every shard stack.
//!
//! # Supervision (L4)
//!
//! Opt-in, per proc or per flow, through a `:supervision` map
//! (docs/L4-SUPERVISION-DESIGN.md §3.7). A flow with no `:supervision`
//! anywhere is byte-identical to the pre-L4 engine: no chan, no task, no
//! event, not one extra observable. A flow with one gets:
//!
//! - **A death event per proc exit.** Every proc's [`ExitGuard`] renders its
//!   [`ExitReason`] into each member's done-cell AND `try_put`s it onto the
//!   flow's one `sup_chan` (`FlowRuntime::sup_chan`) -- every proc's, not
//!   just the configured ones, so the supervisor sees the whole flow.
//! - **One supervisor TASK per supervised flow** ([`run_supervisor`]), parked
//!   on {a `sup_chan` ring OR the earliest scheduled restart}, never on
//!   anything else. It is the chan's only reader.
//! - **A pure policy** ([`decide`]): (event, config, history, phase, now) ->
//!   ignore / restart-at / give-up. No clock, no locks, no side effects --
//!   [`sup_now`] is the ONE clock read in the whole subsystem, and it is
//!   marked as L5's virtual-time seam.
//! - **Restart = respawn the RUN.** [`RunBlueprint`] + [`spawn_run`] are the
//!   single spawn path `start` and the supervisor share, so incarnation n is
//!   wired exactly as incarnation 0 was. The chans are the identity and
//!   persist across every incarnation (design §3.4); the done-cells,
//!   `ExitGuard` and thread handle are per-incarnation and are swapped into
//!   `FlowRuntime::procs` under the flow's phase lock.
//! - **Events out on `report_chan`** -- its first producer ever:
//!   `:proc-exit` mirrored verbatim, plus `:proc-restart`/`:proc-give-up`/
//!   `:proc-wedged`. Supervised flows only, which is what keeps the corpus's
//!   "nothing arrives post-stop" pin intact for everyone else.
//! - **Stop closes `sup_chan` FIRST** ([`stop_flow_cell`] step 1b), before
//!   the `::flow/stop` broadcast, and waits on the supervisor's own done-cell
//!   alongside every proc's. Deaths that land after that close are dropped:
//!   from step 1b onward, stop's waits own the endgame.
//! - **An escalation ladder, reached through [`native_stop_proc`]** (L4 W3;
//!   `flow/stop-proc` is mova-native -- upstream has no per-proc stop at
//!   all). Rung 1 is a graceful `::flow/stop` on the proc's own control chan,
//!   which is the whole story for a proc that reads its control chan and for
//!   every unsupervised proc. Rung 2, after `:grace-ms`, is a KILL: the
//!   supervisor force-unwinds the run's task where it stands
//!   (`runtime::TaskWaker::kill`), retrying on the [`KILL_RETRY_BUDGET`]
//!   cadence because a kill claims only a task that is PARKED at that
//!   instant. A stop-proc'd run does NOT restart -- `:proc-stopped` is
//!   terminal by user intent, and its `:reason` says which rung ended it.
//!
//! **The honesty section.** Three limits, named rather than papered over:
//! - **`:io` procs are unkillable.** An OS thread cannot be force-unwound, so
//!   the ladder has no rung 2 for one: it gets the graceful stop and, past
//!   its grace, a `:proc-wedged` event (wall W6, owner ruling #4). The same
//!   rule gates their RESTART -- an `:io` run is respawned only once its
//!   previous incarnation's done-cell is confirmed closed.
//! - **A RUNNING task is unreachable too.** Kill is delivered at a park
//!   point, which is what makes it deterministic (design §3.5) and also what
//!   makes a task spinning in user code, or blocked inside a genuinely
//!   blocking native, impossible to destroy. That is the same accepted
//!   footgun `runtime`'s module doc names for blocking natives, and the
//!   budget-exhaustion arm of the ladder reports it as `:proc-wedged`.
//! - **A killed proc's chan waiters outlive it briefly.** The kill claims its
//!   commit cells so no message is ever lost or delivered posthumously
//!   (`value.rs`'s `PUT_KILLED` / `TakeSlot`'s tombstone note), but the dead
//!   waiter stays QUEUED until some deliverer walks past it and culls it.
//!   Bounded by construction: at most one corpse per killed task per chan it
//!   was parked on, and a task is killed at most once.
//!
//! One consequence worth naming out loud. A restart-supervised proc is never
//! given a transport lane (a `SpscRing` half dies with the incarnation that
//! owns it), so its links stay on the general `Chan` path. Since L3.6/W1 made
//! task procs lane-eligible this is no longer a free exclusion -- a
//! supervised hop now genuinely forgoes the lane's throughput -- and that is
//! the deliberate trade: chans are the identity, the proc body is disposable
//! (design §3.4), and a ring half is the one piece of wiring that cannot be
//! handed to the next incarnation.
//!
//! It is also what keeps the L4 kill contract simple. A killed proc is by
//! construction never lane-parked *and* restartable; and even a bare kill of
//! a lane-parked proc is message-safe by the ring's own shape, not by any
//! tombstone: a value only leaves the ring buffer inside `Ring::consume`,
//! which is straight-line, non-parking code on the consumer's own stack, and
//! the producer's blocked send keeps ownership of its value throughout
//! (`SpscTx::wait_writable` is a WAIT, not a timed put). So there is no
//! "value in transit through a cell" state for a kill to land in -- the
//! hazard `TaskWaker::kill`'s tombstone exists for on the `Chan` path.
//!
//! **Auto-resume (L4 W5, owner ruling #5).** `init` still rebuilds a
//! restarted incarnation's state from `args` -- no state handoff/snapshot in
//! L4 -- but the supervisor no longer leaves it there paused and silent. It
//! consults `FlowRuntime::desired`, the per-pid record of what the user LAST
//! asked for (`pause`/`resume`/`pause-proc`/`resume-proc`/`stop-proc`
//! maintain it; `native_start` seeds every pid `Paused`, upstream's own
//! contract), and -- iff the run's `:auto-resume` is `true` (the default)
//! AND every member's desired state is `Running` -- synthesizes ONE
//! `::flow/resume` onto each member's control chan the instant the swap
//! lands, still under the flow's phase lock (`Supervisor::act_restart`). A
//! proc the user had deliberately PAUSED (or `stop-proc`'d) stays paused
//! across the crash regardless: the supervisor mirrors intent, it does not
//! invent it. A fused run resumes as a unit or not at all -- a half-paused
//! fused run stays paused, conservative and documented at the call site.
//! `:proc-restart`'s `:resumed` field says which happened.

// This module keys plain `std::collections::HashMap`s by `Value` throughout
// (pid/port-id -> chan wiring, proc runtimes, ...) rather than `imbl`'s
// persistent map (which `value.rs`'s own `Value::Map` uses, and which
// clippy's `mutable_key_type` lint doesn't recognize at all -- that's why
// this allow has no precedent elsewhere in the crate). The lint's concern
// (a key mutating in place after insertion, silently corrupting the map)
// doesn't apply here: every key actually used is a `Keyword`/`Symbol`/`Str`
// pid or port-id -- immutable scalars all the way down -- and even for the
// `Fn`/`Atom`/etc. variants `Value`'s `Hash`/`Eq` impls (value.rs) are
// pointer-identity (`Arc::as_ptr`), which never changes for the life of the
// `Arc`, interior mutability of the pointee notwithstanding.
#![allow(clippy::mutable_key_type)]

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::builtins::collections::materialize;
use crate::builtins::map_probe;
use crate::builtins::r#async::{
    chan_close, chan_put, chan_take, chan_try_put, chan_try_take, ring_alts, timer_arm, timer_arm_waker, TryPut,
    TryTake,
};
use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::sync::{cv_wait_timeout, lock_mutex};
use crate::transport::{SpscCloser, SpscRing, SpscRx, SpscTx, TryPut as LinkPut, TryTake as LinkTake};
use crate::value::{
    BackoffCfg, BufferPolicy, Chan, DesiredState, Doorbell, FastOut, FastStep, FlowCell, FlowConn, FlowDef, FlowPhase,
    FlowRuntime, FutureCell, InjectPort, FutureState, Keyword, NativeFn, OnGiveUp, PMap, ProcDef, ProcRuntime, PVec,
    StepTransition, Str, SupervisionCfg, Symbol, Value,
};

/// 64 MiB, matching every other thread-spawning builtin's reasoning
/// (`builtins::conc::FUTURE_STACK_SIZE`, `builtins::async::ASYNC_STACK_SIZE`):
/// a proc's `transform` runs through the tree-walking evaluator and needs
/// real Rust stack headroom for deep mova-level recursion.
const PROC_STACK_SIZE: usize = 64 * 1024 * 1024;
/// `create-flow`'s documented default `:buf-or-n` for any io-id not given
/// an explicit `:chan-opts` entry.
const DEFAULT_BUF: usize = 10;
/// Sliding-100 for both `:report-chan` and `:error-chan`, per FLOW-DESIGN.md.
const DIAG_BUF: usize = 100;
const CONTROL_BUF: usize = 10;
/// Single-input fast path: drain up to this many buffered messages per
/// batch before yielding back to the control-check/park cycle.
const BATCH_DRAIN_MAX: usize = 31;
/// ...checking control non-blocking every N drained messages.
const CONTROL_CHECK_EVERY: usize = 8;
/// Safety-net bound on a [`Doorbell`]-driven park -- NOT a live polling
/// interval anymore (see the module doc's "Control-priority wait design"
/// section). Every idle park it bounds (single-input `try_take_with_timeout`,
/// multi-input's scan-then-park tail, `run_fused`'s head-of-chain reads,
/// AND the transport tier's data-direction read -- [`in_take_timeout`]'s
/// lane branch uses this exact constant too, via `transport::SpscRx::
/// take_timeout_or_doorbell`/`_cold` -- see the module doc's "Composition
/// with control/pause/stop" section for the full mechanism)
/// is woken instantly by a [`Doorbell::ring`] in the overwhelmingly common
/// case; this bound exists
/// only to defend against a `Doorbell::ring` this code failed to wire up
/// somewhere, or the vanishingly-rare race where a ring lands in the
/// nanosecond-scale gap between a non-blocking control check and the
/// following park call snapshotting the generation (see `value.rs`'s
/// `Doorbell` doc) -- both cases self-heal on the NEXT outer-loop lap
/// regardless, so this only bounds how long that self-heal can take.
///
/// **On a TASK it is not a bound at all** (L3.6/W1): a task park has no
/// timer, so both task arms -- `Doorbell::wait_for_change_task` and the
/// lane's `Ring::park_task_two_source` -- ignore this argument entirely and
/// suspend until one of their real sources fires. That is the L1 landing
/// stance (the ring is the mechanism, not a backstop), and it is why the
/// snapshot-before-scan rule is load-bearing on a task rather than merely
/// tidy. 2s
/// leaves generous headroom under every `deref ... 5000 :timeout` in
/// `tests/flow_test.rs` while still being ~2000x fewer wakeups/idle-proc
/// than the pre-fix 1ms constant of the same name (and, for the transport
/// tier specifically, ~2000x fewer than the pre-fix `TRANSPORT_PARK_TIMEOUT`
/// this constant has now fully replaced -- see the module doc's
/// "Composition with control/pause/stop" section for the full story of why
/// an earlier version of this fix could NOT simply do that, and what
/// changed).
const PARK_TIMEOUT: Duration = Duration::from_secs(2);
/// Short, UNCHANGED-from-pre-fix bound used only by the two blocked-send
/// paths ([`send_with_control_priority`]'s condvar wait -- its THREAD arm;
/// the task arm's [`blocked_send_task`] passes this same constant to
/// `Doorbell::wait_for_change`, whose task side has no safety net and
/// ignores it -- and the transport tier's
/// `wait_writable`/`wait_writable_cold` calls in [`out_send`]): a
/// proc stuck trying to deliver output to a full downstream still notices
/// `stop` within ~1ms, exactly as before this fix. Deliberately NOT the
/// long `PARK_TIMEOUT` safety net above: a blocked send is real
/// backpressure, not idle, and the target chan's `Doorbell` slot (if any)
/// already belongs to that chan's own READER (waiting for data) -- see the
/// module doc's "control-priority wait design" section for why a blocked
/// SENDER gets a short bounded fallback wait instead of contending for that
/// slot. (The transport tier's OWN blocked-send path,
/// `out_send`'s `wait_writable`/`wait_writable_cold`, is UNCHANGED by the
/// `Doorbell::ring`-also-unparks mechanism for the same "not the idle case"
/// reasoning -- backpressure is real signal, not something to widen a
/// timeout against.)
///
/// **L3.6/W1, the task arm.** [`out_send`]'s TASK path passes this same
/// constant to `wait_writable_or_doorbell`/`_cold`, where -- exactly as on
/// `blocked_send_task` -- it is inert: a task park has no timer. What
/// replaces it there is not a shorter bound but a SECOND SOURCE, the proc's
/// own doorbell, which every control command already rings. So the task arm
/// notices `stop` on the ring rather than within ~1ms of poll, i.e. strictly
/// better than the thread arm this constant still governs, and the constant
/// is passed only so the two arms read as one shape with one bound.
const BLOCKED_SEND_TIMEOUT: Duration = Duration::from_millis(1);

/// Multi-input's idle scan-then-park tail is now `Doorbell`-driven (see
/// `PARK_TIMEOUT`); this constant survives ONLY for [`run_mult_thread`]'s
/// full-downstream retry loop -- i.e. only under
/// `MOVA_FLOW_THREAD_PROCS=1`, since L3.5 item 2 made the default world's
/// mult a task ([`run_mult_task`]) with no sleep in it at all. Kept
/// unchanged there because the kill switch's whole job is to reproduce the
/// pre-L3 engine, and because on an OS thread the spin only runs while a
/// downstream mult target is genuinely full (real backpressure, not idle).
///
/// **Not de-polled, twice over, and for two different reasons.** W2
/// evaluated swapping it for [`park_tick`] alongside [`PING_POLL`] and
/// declined on cost: a fresh `Chan` allocation plus a timer-heap push and
/// pop per spin iteration where a bare `thread::sleep` was already the
/// cheapest possible wait, AND a 5x coarser retry cadence (the shared timer
/// service only arms in whole milliseconds; 200 µs rounds up to 1 ms) on a
/// path that -- like [`BLOCKED_SEND_TIMEOUT`] -- is deliberately real
/// backpressure, not idle. L3.5 then removed the premise W2's OTHER half
/// rested on ("this always runs on an OS thread, so there is no shard for
/// the sleep to burn"): in the default world it no longer runs at all. The
/// task world does not de-poll this wait, it DELETES it -- a blocking
/// `chan_put` parks the task properly, which is both cheaper and the only
/// shape that cannot deadlock a self-loop (see [`run_mult_task`]).
const MULTI_INPUT_BACKOFF: Duration = Duration::from_micros(200);
/// `flow/ping`'s reply-collection poll interval. De-polled via [`park_tick`]
/// (W2, design §3.7): a `flow/ping` from inside a `go` block parks the task
/// instead of stalling its shard. The shared timer service
/// (`builtins::async::timer_arm`) only arms in whole milliseconds, so the
/// actual tick [`park_tick`] arms is `Duration::from_millis(1)`, rounded up
/// from the 500 µs named here -- negligible against `flow/ping`'s
/// default-1000ms/typical-2000ms budgets (`tests/flow_test.rs`).
const PING_POLL: Duration = Duration::from_micros(500);
/// `stop`'s per-proc join timeout before giving up and detaching.
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

const FLOW_NS: &str = "clojure.core.async.flow";

/// Builds the fully-qualified `clojure.core.async.flow/<name>` keyword --
/// see the module doc for why this is spelled out rather than using a `::`
/// reader shorthand (mova's reader has no namespace-alias resolution, so
/// `::flow/foo` would read as a keyword literally named `:flow/foo`, NOT
/// the upstream-compatible fully-qualified name FLOW-DESIGN.md intends).
/// `pub(crate)` so `builtins::flow_steps`' native steps build the exact
/// same fully-qualified keywords the engine matches on (`::flow/in-ports`,
/// `::flow/stop`, ...) from this one definition rather than re-spelling the
/// namespace.
pub(crate) fn kw(name: &str) -> Value {
    Value::Keyword(Keyword::from(format!("{FLOW_NS}/{name}")))
}

fn plain_kw(name: &str) -> Value {
    Value::Keyword(Keyword::from(name))
}

/// Design Part 2 (upstream `impl.clj:161-162`): the two engine-reserved
/// out-targets `native_start` assocs into EVERY proc's resolved outs map,
/// unconditionally -- not just procs that declare them. `send_outputs`'
/// ordinary `(outs out-id)` lookup then routes a transform's
/// `{::flow/report [...]}` / `{::flow/error [...]}` entries exactly like
/// any declared port (same control-priority send, same
/// `Sliding(DIAG_BUF)` non-blocking guarantee). `Keyword::construct` is an
/// L1-cache hit after the first call (keyword.rs's module doc), so calling
/// these repeatedly costs a hash lookup, not an allocation.
fn report_out_key() -> Value {
    kw("report")
}
fn error_out_key() -> Value {
    kw("error")
}

/// True for either reserved out-target. Every "how many out ports does
/// this proc/member declare" count (N2 promotion, run fusion) MUST exclude
/// these -- they are always present after `flow/start` and are never a
/// proc's own declared port.
fn is_reserved_out_key(k: &Value) -> bool {
    *k == report_out_key() || *k == error_out_key()
}

/// The subset of `outs` that are the proc's OWN declared ports -- i.e. every
/// entry except the two engine-reserved out-targets (see
/// [`is_reserved_out_key`]). Every "at most one declared out port" site N2
/// promotion and run fusion depend on (`try_promote_fast`, `run_proc_fast`,
/// `run_fused`, `fusion_still_valid`) counts/extracts through this, so none
/// of them can mistake a reserved key for a proc's own port now that both
/// are always wired.
fn real_outs(
    outs: &HashMap<Value, Option<Arc<Chan>>>,
) -> impl Iterator<Item = (&Value, &Option<Arc<Chan>>)> {
    outs.iter().filter(|(k, _)| !is_reserved_out_key(k))
}

// ---------------------------------------------------------------------------
// cfg parsing helpers
// ---------------------------------------------------------------------------

fn expect_map<'a>(v: &'a Value, ctx: &str) -> Result<&'a PMap, RjError> {
    match v {
        Value::Map(m) => Ok(m),
        other => Err(RjError::type_err(format!("{ctx}: expected a map, got {}", other.type_name()))),
    }
}

fn expect_vector<'a>(v: &'a Value, ctx: &str) -> Result<&'a PVec, RjError> {
    match v {
        // S7: a map entry is a 2-element vector, accepted like any other.
        Value::Vector(v) | Value::List(v) | Value::MapEntry(v) => Ok(v),
        other => Err(RjError::type_err(format!("{ctx}: expected a vector, got {}", other.type_name()))),
    }
}

fn expect_flow<'a>(v: &'a Value, op: &str) -> Result<&'a Arc<FlowCell>, RjError> {
    match v {
        Value::Flow(f) => Ok(f),
        other => Err(RjError::type_err(format!("{op}: expected a flow, got {}", other.type_name()))),
    }
}

/// A `:proc` launcher is either a bare 4-arity step-fn/var, or the
/// `{:mova.flow/step sf :mova.flow/workload w}` map `core/flow.mova`'s
/// `flow/process` builds. This extracts the STEP-FN half; the workload half
/// is [`proc_workload`]'s job, read straight off the same launcher rather
/// than returned alongside here, because the two have completely different
/// lifetimes -- the step-fn is needed once per proc at `create-flow`
/// (validation) and once more at `start` (the spawn record), while the
/// workload is needed only by `start`'s spawn decision (and the placement
/// that follows from it), which already holds the `ProcDef`. Until L3.6/W1
/// it was needed by [`plan_transport_links_with`]'s eligibility clause too;
/// that clause is gone -- see there.
///
/// (Until L3 the workload was parsed nowhere at all: it was stored for
/// upstream-API compat and had no functional effect, "every proc runs on its
/// own OS thread regardless of `:workload`", a documented deviation. As of
/// W1b that deviation is GONE from the default build -- `:mixed`/`:compute`
/// procs are tasks, `:io` procs are threads -- and the old sentence is true
/// only under the `MOVA_FLOW_THREAD_PROCS=1` kill switch. See the module
/// doc's "Task procs" section.)
fn extract_step_fn(launcher: &Value) -> Result<Value, RjError> {
    match launcher {
        // R2: a bare var launcher (`:proc #'my-proc`, no `flow/process`
        // wrapper) is accepted right alongside a bare fn/native -- passed
        // through un-dereffed, so every later `interp.call(&step_fn, ...)`
        // (this module's one call surface for a proc's step-fn) reads the
        // var's CURRENT value each time, late-bound exactly like the
        // `{:mova.flow/step #'my-proc ...}` map shape `flow/process`
        // builds (that shape needs no code here at all: the map lookup
        // below just clones whatever `Value` it finds).
        Value::Fn(_) | Value::Native(_) | Value::Var(_) => Ok(launcher.clone()),
        Value::Map(m) => m
            .get(&plain_kw("mova.flow/step"))
            .cloned()
            .ok_or_else(|| RjError::other("create-flow: :proc launcher map is missing :mova.flow/step")),
        other => Err(RjError::type_err(format!(
            "create-flow: :proc must be a step-fn or a flow/process launcher, got {}",
            other.type_name()
        ))),
    }
}

/// A `:workload` value found somewhere (a launcher's opts, or a
/// `describe()` result's `:workload` key) -- `None` when that SOURCE had
/// nothing to say (the key was absent, or present but `nil`), `Some(_)`
/// when it said ANYTHING, parsed permissively exactly like `proc_workload`
/// always has: a recognized keyword maps to its variant, any other non-nil
/// value (an unrecognized keyword, or a value of the wrong type entirely)
/// is `Some(Workload::Mixed)` -- present but garbled still counts as "this
/// source spoke", which matters for [`resolve_workload`]'s precedence, and
/// still degrades to the documented default rather than erroring, exactly
/// as `create-flow` has always been permissive about this key.
fn parse_workload_opt(v: Option<&Value>) -> Option<Workload> {
    match v {
        None | Some(Value::Nil) => None,
        Some(Value::Keyword(k)) => Some(match k.as_ref() {
            "io" => Workload::Io,
            "compute" => Workload::Compute,
            _ => Workload::Mixed,
        }),
        Some(_) => Some(Workload::Mixed),
    }
}

/// Resolves a proc's EFFECTIVE `:workload`, matching upstream
/// `core.async.flow` exactly (`process`'s docstring): "A :workload supplied
/// as an option to process will override any :workload returned by the
/// :describe fn of the process. If neither are provided the default is
/// :mixed." `launcher` is the raw `:proc` value (bare step-fn/var, or a
/// `flow/process` launcher map -- a bare launcher carries no opts at all,
/// same as an empty opts map); `describe_map` is that same proc's
/// already-computed `describe()` result.
///
/// Called exactly ONCE per proc, at `create-flow` validation time
/// (`native_create_flow`, right where `describe_map` is already in hand
/// from the one `describe()` call this module ever makes). Rather than
/// stash the answer in a side table, `native_create_flow` NORMALIZES the
/// `ProcDef`'s stored `launcher` to always be the
/// `{:mova.flow/step sf :mova.flow/workload w}` map shape with `w` set
/// to this resolved keyword -- so [`proc_workload`]'s plain single-source
/// read off `pdef.launcher` (unchanged from pre-L3.5) is already reading
/// the fully resolved answer, so every later reader of it -- the spawn
/// decision and the placement pass, which call [`proc_workload`] on the
/// same `ProcDef` at different times -- can never disagree. (The transport
/// planner was a third such reader until L3.6/W1 removed its task clause.)
fn resolve_workload(launcher: &Value, describe_map: &PMap) -> Workload {
    let opts_workload = match launcher {
        Value::Map(m) => parse_workload_opt(m.get(&plain_kw("mova.flow/workload"))),
        _ => None,
    };
    opts_workload.or_else(|| parse_workload_opt(describe_map.get(&plain_kw("workload")))).unwrap_or(Workload::Mixed)
}

/// The keyword [`resolve_workload`]'s answer round-trips to when
/// `native_create_flow` writes it back into the normalized launcher map --
/// the inverse of [`proc_workload`]'s `"io"`/`"compute"`/other match.
fn workload_kw(w: Workload) -> Value {
    plain_kw(match w {
        Workload::Io => "io",
        Workload::Compute => "compute",
        Workload::Mixed => "mixed",
    })
}

/// `flow/process`'s `:workload`, the one opt the launcher map carries --
/// upstream's `:mixed`/`:io`/`:compute`, and from L3 the ONE input to "does
/// this proc get an OS thread or a task?" in the DEFAULT build. See the
/// module doc's "Task procs" section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Workload {
    /// The default, and what upstream documents as "may do a bit of both":
    /// a task, on the same footing as [`Workload::Compute`]. The two are
    /// kept as distinct variants (rather than collapsed to a bool) because
    /// they are distinct in the user-facing API and W3's chain-affine
    /// placement is expected to want to tell them apart.
    Mixed,
    /// The blocking escape hatch: this proc keeps its own OS thread, because
    /// its step is expected to block in a native where a task would stall
    /// its shard. The ONE per-proc opt-out from the default task world.
    Io,
    Compute,
}

/// The workload declared by a `:proc` launcher. Anything that is not a
/// `flow/process` launcher map carrying a recognized `:mova.flow/workload`
/// keyword -- a bare fn/var/native launcher, a map without the key, a map
/// whose value is a keyword nobody defined -- is [`Workload::Mixed`], the
/// same default `flow/process` itself applies. Deliberately permissive
/// rather than an error: `create-flow` has never validated this key (it was
/// inert until L3/W1a), and starting to REJECT a cfg that used to work is a
/// bigger break than quietly giving it the documented default.
///
/// As of L3.5, `pdef.launcher` is ALWAYS the normalized
/// `{:mova.flow/step sf :mova.flow/workload w}` map by the time a
/// `ProcDef` exists (`native_create_flow` rewrites even a bare launcher),
/// with `w` already [`resolve_workload`]'s answer -- opts if it supplied
/// one, else the proc's own `describe()` result, else `:mixed`. So this
/// function's job is unchanged from pre-L3.5: read ONE keyword off the
/// launcher map permissively. It just never has to consult `describe()`
/// itself, because by construction the launcher already carries the
/// fully-resolved value.
fn proc_workload(pdef: &ProcDef) -> Workload {
    let Value::Map(m) = &pdef.launcher else { return Workload::Mixed };
    match m.get(&plain_kw("mova.flow/workload")) {
        Some(Value::Keyword(k)) => match k.as_ref() {
            "io" => Workload::Io,
            "compute" => Workload::Compute,
            _ => Workload::Mixed,
        },
        _ => Workload::Mixed,
    }
}

/// True unless `MOVA_FLOW_THREAD_PROCS=1` was set in the environment at
/// process start -- i.e. TRUE BY DEFAULT (L3/W1b's flip), read exactly ONCE
/// (an `OnceLock`, mirroring [`spsc_disabled_by_env`] and every other switch
/// here) so no wiring pass ever touches the environment.
///
/// ONE predicate, negative polarity: `MOVA_FLOW_THREAD_PROCS=1` is a KILL
/// SWITCH (mirroring `MOVA_GO_THREADS=1` and `MOVA_NO_DIRECT_SWITCH=1`,
/// the runtime's own escapes), not a feature flag. Setting it forces every
/// proc onto its own OS thread, which is the exact pre-L3 engine -- the
/// regression escape hatch, and a world `cargo test --release --test
/// flow_gold_test` is run under as its own gate so the old engine can never
/// silently rot. With it unset, `:workload` decides per proc: see
/// [`is_task_proc`] and the module doc's "Task procs" section.
fn flow_task_procs_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| !std::env::var("MOVA_FLOW_THREAD_PROCS").is_ok_and(|v| v == "1"))
}

/// THE predicate: a proc runs as a task iff the kill switch is not forcing
/// threads and the proc did not ask for one itself (`:workload :io`). One
/// definition, so nothing in the engine can disagree about what a proc is.
///
/// It used to have TWO callers -- the spawn seam in [`native_start`] and
/// the transport-eligibility clause in [`plan_transport_links_with`]. As of
/// L3.6/W1 the second is gone: `transport.rs`'s `Ring` has a task arm, so
/// task-hood no longer narrows the transport plan at all (see that
/// function's doc, and docs/FLOW-HOP-RECOVERY.md §7). What remains is the
/// spawn decision and the placement it feeds.
///
/// `enabled` is [`flow_task_procs_enabled`]'s answer, passed in rather than
/// read here, so the unit tests can see BOTH worlds in one process (the real
/// switch is fixed for a process's whole life).
fn is_task_proc(pdef: &ProcDef, enabled: bool) -> bool {
    enabled && proc_workload(pdef) != Workload::Io
}

/// L4 W2: does this pid carry a resolved `:policy :restart` supervision
/// config -- i.e. can the supervisor ever respawn it? (`ProcDef::supervision`
/// is `None` for every other case, including an explicit `:policy :none` and
/// the `MOVA_NO_SUPERVISION=1` kill switch, so this is a single `is_some`.)
/// An unknown pid answers `false`, which keeps callers that walk `conns`
/// (whose endpoints `create-flow` has already validated) free of an
/// `expect`.
fn restart_supervised(def: &FlowDef, pid: &Value) -> bool {
    def.procs.get(pid).is_some_and(|p| p.supervision.is_some())
}

fn sorted_by_pr_str(mut items: Vec<Value>) -> Vec<Value> {
    items.sort_by_key(crate::printer::pr_str);
    items
}

// ---------------------------------------------------------------------------
// `:supervision` opts (L4 W1, docs/L4-SUPERVISION-DESIGN.md §3.7,
// docs/L4-LANDING-SPEC.md §W1.4). Unlike `:workload` this is a wholly
// mova-native surface with no upstream permissiveness to honor, so parsing
// here is LOUD throughout: an unrecognized key or a wrong-typed value is a
// `create-flow` error, never a silently-ignored default.
// ---------------------------------------------------------------------------

const SUPERVISION_KEYS: [&str; 7] =
    ["policy", "max-restarts", "window-ms", "backoff", "grace-ms", "on-give-up", "auto-resume"];
const SUPERVISION_BACKOFF_KEYS: [&str; 3] = ["initial-ms", "factor", "max-ms"];

/// Whether a `:supervision` (or `:backoff`) SOURCE spoke at all -- present
/// and non-`nil` -- mirroring [`parse_workload_opt`]'s `None | Some(Nil)`
/// treatment of "this source had nothing to say" exactly, so a proc that
/// writes `:supervision nil` falls through to the flow-level default rather
/// than being read as "explicitly no supervision" (which is what an actual
/// `{:policy :none}` map is for).
fn supervision_source_present(v: Option<&Value>) -> bool {
    !matches!(v, None | Some(Value::Nil))
}

/// [`supervision_source_present`]'s wrapper for a value that may not even be
/// present as a map key at all (the flow-level `cfg.get(:supervision)` call
/// site) -- `Ok(None)` for "absent or nil" (not an error: `:supervision` is
/// entirely optional), otherwise delegates to [`parse_supervision_map`].
fn resolve_supervision_opt(v: Option<&Value>, ctx: &str) -> Result<Option<SupervisionCfg>, RjError> {
    if supervision_source_present(v) {
        parse_supervision_map(v.expect("supervision_source_present implies Some"), ctx)
    } else {
        Ok(None)
    }
}

/// One `:supervision` map, fully validated, defaulted, and resolved to
/// `Ok(None)` when the effective `:policy` is `:none` -- see
/// `ProcDef::supervision`'s doc for why "explicitly :none" and "absent"
/// collapse to the same `None` rather than a stored `SupervisionPolicy::None`
/// nobody downstream would ever match on. `:policy` itself has NO default
/// (unlike every other key here): a `:supervision` map that does not say
/// what it wants is exactly the kind of ambiguity LOUD validation exists to
/// catch rather than silently guess at, so it errors.
fn parse_supervision_map(v: &Value, ctx: &str) -> Result<Option<SupervisionCfg>, RjError> {
    let m = expect_map(v, &format!("{ctx}: :supervision"))?;
    for k in m.keys() {
        match k {
            Value::Keyword(kwd) if SUPERVISION_KEYS.contains(&kwd.as_ref()) => {}
            _ => {
                return Err(RjError::other(format!(
                    "{ctx}: :supervision has an unrecognized key {}",
                    crate::printer::pr_str(k)
                )))
            }
        }
    }
    let policy_restarts = match m.get(&plain_kw("policy")) {
        Some(Value::Keyword(k)) if k.as_ref() == "restart" => true,
        Some(Value::Keyword(k)) if k.as_ref() == "none" => false,
        Some(other) => {
            return Err(RjError::type_err(format!(
                "{ctx}: :supervision :policy must be :restart or :none, got {}",
                crate::printer::pr_str(other)
            )))
        }
        None => return Err(RjError::other(format!("{ctx}: :supervision is missing :policy"))),
    };

    let max_restarts = nonneg_int_field(m.get(&plain_kw("max-restarts")), 5, &format!("{ctx}: :supervision :max-restarts"))?;
    let window_ms = nonneg_int_field(m.get(&plain_kw("window-ms")), 60_000, &format!("{ctx}: :supervision :window-ms"))?;
    let grace_ms = nonneg_int_field(m.get(&plain_kw("grace-ms")), 1000, &format!("{ctx}: :supervision :grace-ms"))?;
    let backoff = parse_backoff_cfg(m.get(&plain_kw("backoff")), &format!("{ctx}: :supervision :backoff"))?;
    let on_give_up = match m.get(&plain_kw("on-give-up")) {
        None | Some(Value::Nil) => OnGiveUp::Report,
        Some(Value::Keyword(k)) if k.as_ref() == "report" => OnGiveUp::Report,
        Some(Value::Keyword(k)) if k.as_ref() == "stop-flow" => OnGiveUp::StopFlow,
        Some(other) => {
            return Err(RjError::type_err(format!(
                "{ctx}: :supervision :on-give-up must be :report or :stop-flow, got {}",
                crate::printer::pr_str(other)
            )))
        }
    };
    // L4 W5 (owner ruling #5): default TRUE -- "the supervisor mirrors user
    // intent" reads, by default, as "a proc the user had running comes back
    // running". Parsed and validated regardless of `policy_restarts` (LOUD
    // validation applies to every key here, same as every other field above),
    // even though it is meaningless for `:policy :none` and discarded by the
    // early return just below.
    let auto_resume = bool_field(m.get(&plain_kw("auto-resume")), true, &format!("{ctx}: :supervision :auto-resume"))?;

    if !policy_restarts {
        return Ok(None);
    }
    Ok(Some(SupervisionCfg {
        max_restarts: max_restarts as u32,
        window_ms: window_ms as u64,
        backoff,
        grace_ms: grace_ms as u64,
        on_give_up,
        auto_resume,
    }))
}

/// One bool `:supervision` field with a default -- [`nonneg_int_field`]'s
/// sibling for the one field-shape it does not cover (`:auto-resume`, L4 W5).
fn bool_field(v: Option<&Value>, default: bool, label: &str) -> Result<bool, RjError> {
    match v {
        None | Some(Value::Nil) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(RjError::type_err(format!("{label} must be a bool, got {}", crate::printer::pr_str(other)))),
    }
}

/// `:backoff {:initial-ms :factor :max-ms}`, defaulted 100/2.0/5000 -- its
/// own small map, validated the same LOUD way as its parent (unrecognized
/// key or wrong type errors, `None`/`nil` as a whole means "use every
/// default").
fn parse_backoff_cfg(v: Option<&Value>, ctx: &str) -> Result<BackoffCfg, RjError> {
    let bm = match v {
        None | Some(Value::Nil) => return Ok(BackoffCfg { initial_ms: 100, factor: 2.0, max_ms: 5000 }),
        Some(bv) => expect_map(bv, ctx)?,
    };
    for k in bm.keys() {
        match k {
            Value::Keyword(kwd) if SUPERVISION_BACKOFF_KEYS.contains(&kwd.as_ref()) => {}
            _ => return Err(RjError::other(format!("{ctx} has an unrecognized key {}", crate::printer::pr_str(k)))),
        }
    }
    let initial_ms = nonneg_int_field(bm.get(&plain_kw("initial-ms")), 100, &format!("{ctx} :initial-ms"))?;
    let max_ms = nonneg_int_field(bm.get(&plain_kw("max-ms")), 5000, &format!("{ctx} :max-ms"))?;
    let factor = match bm.get(&plain_kw("factor")) {
        None | Some(Value::Nil) => 2.0,
        Some(Value::Int(n)) if *n >= 0 => *n as f64,
        Some(Value::Float(f)) if *f >= 0.0 => *f,
        Some(other) => {
            return Err(RjError::type_err(format!(
                "{ctx} :factor must be a non-negative number, got {}",
                crate::printer::pr_str(other)
            )))
        }
    };
    Ok(BackoffCfg { initial_ms: initial_ms as u64, factor, max_ms: max_ms as u64 })
}

/// One non-negative-int `:supervision` field with a default, shared by
/// every such field ([`parse_supervision_map`]'s `:max-restarts`/
/// `:window-ms`/`:grace-ms`, [`parse_backoff_cfg`]'s `:initial-ms`/
/// `:max-ms`) -- `label` is the fully-formed field name for the error
/// message (e.g. `"create-flow: :supervision :max-restarts"`), since the
/// callers share no common prefix cheap enough to reconstruct here.
fn nonneg_int_field(v: Option<&Value>, default: i64, label: &str) -> Result<i64, RjError> {
    match v {
        None | Some(Value::Nil) => Ok(default),
        Some(Value::Int(n)) if *n >= 0 => Ok(*n),
        Some(other) => {
            Err(RjError::type_err(format!("{label} must be a non-negative int, got {}", crate::printer::pr_str(other))))
        }
    }
}

/// True when `MOVA_NO_SUPERVISION=1` was set in the environment at process
/// start -- the L4 kill switch, read exactly ONCE (an `OnceLock`, mirroring
/// every other switch in this module) so no `create-flow` call ever touches
/// the environment more than the first. Parse-then-drop (like
/// `MOVA_FLOW_THREAD_PROCS`'s own precedent): validation in
/// [`parse_supervision_map`] runs regardless of this flag (a malformed
/// `:supervision` map is still a mistake worth surfacing loudly even under
/// the kill switch), only the RESOLVED `SupervisionCfg` is discarded.
fn supervision_disabled_by_env() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_NO_SUPERVISION").is_ok_and(|v| v == "1"))
}

/// The kill switch's ONE stderr note for the whole process life (not once
/// per proc, not once per flow) -- printed the first time `create-flow`
/// actually had a resolved `SupervisionCfg` to drop, so a script that never
/// touches `:supervision` at all under `MOVA_NO_SUPERVISION=1` prints
/// nothing.
fn note_supervision_dropped_once() {
    static NOTED: OnceLock<()> = OnceLock::new();
    NOTED.get_or_init(|| {
        eprintln!(
            "mova: MOVA_NO_SUPERVISION=1 -- :supervision config parsed, validated, and dropped \
             (flow/create-flow proceeds unsupervised)"
        );
    });
}

/// **L5/W3 fence #1 (design §4): sim refuses the OS-thread proc world — as
/// an ERROR, not a warning.**
///
/// Two ways a flow lands procs on OS threads instead of tasks: a proc whose
/// RESOLVED workload is `:io` (`proc_workload`, already normalized onto the
/// `ProcDef` at this point — opts over `describe()`, one source of truth),
/// and `MOVA_FLOW_THREAD_PROCS=1`, which puts EVERY proc there. Under sim
/// either one is not merely nondeterministic, it returns WRONG ANSWERS:
/// with no tasks runnable the shard is permanently idle, so the advance rule
/// jumps virtual time onto each armed deadline the instant it is armed while
/// the OS-thread flow limps along on wall time. P6b F4 measured it — a
/// `(timeout 5000)` alts collapsed to nothing and dropped 12 of 12 messages.
/// Fences #2/#3 can route a thread to a task; there is nothing to route
/// here, because an `:io` proc's whole declared purpose is to block an OS
/// thread. So the flow is refused before it exists.
///
/// Real mode: one relaxed `sim_enabled()` load per `create-flow` — a call
/// that already evaluated `describe()` once per proc.
fn sim_refuse_thread_world(procs: &HashMap<Value, ProcDef>, proc_order: &[Value]) -> Result<(), RjError> {
    if !crate::clock::sim_enabled() {
        return Ok(());
    }
    if !flow_task_procs_enabled() {
        let first = proc_order.first().map(crate::printer::pr_str).unwrap_or_default();
        return Err(RjError::other(format!(
            "sim mode refuses MOVA_FLOW_THREAD_PROCS=1 (L5 fence #1): it runs EVERY proc on its \
             own OS thread, so the sim shard would be permanently idle and every virtual deadline \
             would fire instantly against wall-clock procs (P6b F4). First proc: {first}. Unset \
             MOVA_FLOW_THREAD_PROCS to run this flow under sim."
        )));
    }
    for pid in proc_order {
        let pdef = procs.get(pid).expect("proc_order is derived from procs");
        if proc_workload(pdef) == Workload::Io {
            return Err(RjError::other(format!(
                "sim mode refuses :io procs (L5 fence #1): proc {}. An :io proc runs on its own OS \
                 thread by contract, which the simulated schedule cannot see -- virtual time would \
                 jump past its work and the flow would give wrong answers, not merely \
                 nondeterministic ones (P6b F4). Declare it :compute (or drop the :workload hint) \
                 to run this flow under sim.",
                crate::printer::pr_str(pid)
            )));
        }
    }
    Ok(())
}

fn native_create_flow(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/create-flow: expected 1 argument, got {}", args.len())));
    }
    let cfg = expect_map(&args[0], "create-flow")?;

    let procs_val = cfg.get(&plain_kw("procs")).cloned().unwrap_or_else(|| Value::Map(PMap::new()));
    let procs_cfg = expect_map(&procs_val, "create-flow: :procs")?;
    if procs_cfg.is_empty() {
        return Err(RjError::other("create-flow: :procs must be a non-empty map"));
    }

    // L4 W1: the flow-level `:supervision` default, resolved ONCE here (not
    // per-proc below) -- every proc whose OWN `:supervision` key is absent
    // or `nil` falls back to this SAME resolved value, `:workload`'s
    // proc-opts-over-describe() precedence shape (592fa55) applied to a
    // proc-cfg-over-flow-cfg precedence instead. Validated even if no proc
    // ends up using it: a malformed flow-level default is still a mistake
    // worth a loud `create-flow` error, not a silently-ignored one.
    let flow_supervision = resolve_supervision_opt(cfg.get(&plain_kw("supervision")), "create-flow: :supervision")?;

    let mut procs: HashMap<Value, ProcDef> = HashMap::new();
    for (pid, proc_cfg_val) in procs_cfg.iter() {
        let proc_cfg = expect_map(proc_cfg_val, &format!("create-flow: proc {} config", crate::printer::pr_str(pid)))?;
        let launcher = proc_cfg.get(&plain_kw("proc")).cloned().ok_or_else(|| {
            RjError::other(format!("create-flow: proc {} is missing :proc", crate::printer::pr_str(pid)))
        })?;
        let args_val = proc_cfg.get(&plain_kw("args")).cloned().unwrap_or_else(|| Value::Map(PMap::new()));
        expect_map(&args_val, &format!("create-flow: proc {} :args", crate::printer::pr_str(pid)))?;

        let mut chan_opts: HashMap<Value, usize> = HashMap::new();
        if let Some(chan_opts_val) = proc_cfg.get(&plain_kw("chan-opts")) {
            let chan_opts_cfg = expect_map(chan_opts_val, &format!("create-flow: proc {} :chan-opts", crate::printer::pr_str(pid)))?;
            for (io_id, opt_val) in chan_opts_cfg.iter() {
                let opt_map = expect_map(opt_val, &format!("create-flow: proc {} chan-opts entry", crate::printer::pr_str(pid)))?;
                let n = match opt_map.get(&plain_kw("buf-or-n")) {
                    None => DEFAULT_BUF,
                    Some(Value::Int(n)) if *n >= 0 => *n as usize,
                    Some(other) => {
                        return Err(RjError::type_err(format!(
                            "create-flow: proc {} chan-opts {}: :buf-or-n must be a non-negative int, got {}",
                            crate::printer::pr_str(pid),
                            crate::printer::pr_str(io_id),
                            other.type_name()
                        )))
                    }
                };
                chan_opts.insert(io_id.clone(), n);
            }
        }

        let step_fn = extract_step_fn(&launcher)?;
        let describe = interp.call(&step_fn, &[])?;
        let describe_map = expect_map(&describe, &format!("proc {}: describe() result", crate::printer::pr_str(pid)))?;
        let ins = port_ids(describe_map, "ins", pid)?;
        let outs = port_ids(describe_map, "outs", pid)?;
        for p in &ins {
            if outs.contains(p) {
                return Err(RjError::other(format!(
                    "create-flow: proc {} port {} is declared as both an in and an out",
                    crate::printer::pr_str(pid),
                    crate::printer::pr_str(p)
                )));
            }
        }
        // Design Part 2 (upstream `impl.clj:161-162`): `::flow/report` and
        // `::flow/error` are engine-reserved out-targets `native_start`
        // assocs into EVERY proc's resolved outs map unconditionally, at
        // spawn time -- see `is_reserved_out_key`'s own doc. A proc that
        // ALSO declares one of them itself (in either `:ins` or `:outs`)
        // is not a port collision `create-flow` can silently tolerate: at
        // `start` time the engine's own wiring would either collide with
        // or (worse, for an `:ins` declaration) be mistaken for the
        // proc's own port, hijacking report/error routing invisibly.
        // Rejected loudly here, at config time, before a single proc is
        // spawned -- the same "validate before spawn" discipline as the
        // in/out-overlap check right above.
        for p in ins.iter().chain(outs.iter()) {
            if is_reserved_out_key(p) {
                return Err(RjError::other(format!(
                    "create-flow: proc {} declares port {} -- {} and {} are engine-reserved \
                     out-targets always wired by flow/start; a proc must not declare them itself",
                    crate::printer::pr_str(pid),
                    crate::printer::pr_str(p),
                    crate::printer::pr_str(&report_out_key()),
                    crate::printer::pr_str(&error_out_key())
                )));
            }
        }
        // Resolved HERE, once, from the exact `describe_map` this proc will
        // ever have (never re-invoked at `start`) plus the launcher's own
        // opts -- see `resolve_workload`'s doc for the precedence. The
        // launcher stored on `ProcDef` is NORMALIZED to carry that resolved
        // answer directly (see `proc_workload`'s doc), so every later
        // reader sees the exact same value without touching `describe_map`
        // or opts again.
        let workload = resolve_workload(&launcher, describe_map);
        let normalized_launcher = {
            let mut m = PMap::new();
            m.insert(plain_kw("mova.flow/step"), step_fn);
            m.insert(plain_kw("mova.flow/workload"), workload_kw(workload));
            Value::Map(m)
        };

        // L4 W1: this proc's OWN `:supervision`, if it spoke (present and
        // non-nil), else the flow-level default resolved once above --
        // proc-level > flow-level > none, resolved ONCE here and stored
        // directly on `ProcDef` (see that field's doc for why `None` covers
        // both "no key anywhere" and "resolved :policy is :none").
        let proc_supervision_raw = proc_cfg.get(&plain_kw("supervision"));
        let supervision = if supervision_source_present(proc_supervision_raw) {
            parse_supervision_map(
                proc_supervision_raw.expect("supervision_source_present implies Some"),
                &format!("create-flow: proc {} :supervision", crate::printer::pr_str(pid)),
            )?
        } else {
            flow_supervision.clone()
        };
        // `MOVA_NO_SUPERVISION=1`: parse-then-drop (kill-switch precedent:
        // `MOVA_FLOW_THREAD_PROCS`). Validation above already ran, loudly,
        // regardless of the switch -- only the RESOLVED answer is discarded
        // here, with one process-wide stderr note the first time it happens.
        let supervision = if supervision_disabled_by_env() {
            if supervision.is_some() {
                note_supervision_dropped_once();
            }
            None
        } else {
            supervision
        };

        procs.insert(
            pid.clone(),
            ProcDef { launcher: normalized_launcher, args: args_val, chan_opts, ins, outs, supervision },
        );
    }

    let conns_val = cfg.get(&plain_kw("conns")).cloned().unwrap_or_else(|| Value::Vector(PVec::new()));
    let conns_cfg = expect_vector(&conns_val, "create-flow: :conns")?;
    let mut conns: Vec<FlowConn> = Vec::with_capacity(conns_cfg.len());
    for c in conns_cfg.iter() {
        let pair = expect_vector(c, "create-flow: conn entry")?;
        if pair.len() != 2 {
            return Err(RjError::other(format!(
                "create-flow: conn entry must be [[from-pid from-port] [to-pid to-port]], got {}",
                crate::printer::pr_str(c)
            )));
        }
        let from = expect_vector(&pair[0], "create-flow: conn from")?;
        let to = expect_vector(&pair[1], "create-flow: conn to")?;
        if from.len() != 2 || to.len() != 2 {
            return Err(RjError::other(format!(
                "create-flow: conn entry must be [[from-pid from-port] [to-pid to-port]], got {}",
                crate::printer::pr_str(c)
            )));
        }
        let (from_pid, from_port) = (from[0].clone(), from[1].clone());
        let (to_pid, to_port) = (to[0].clone(), to[1].clone());

        // Same reserved-key guard as the per-proc `:ins`/`:outs` check
        // above, for a conn's endpoints: a conn naming `::flow/report`/
        // `::flow/error` would wire a proc's own port declaration check to
        // pass trivially (neither endpoint is a proc's DECLARED port, so
        // the `outs.contains`/`ins.contains` lookups just below would
        // reject it anyway with a confusing "no such port" message) but
        // is really the same hijack attempt the proc-level check exists
        // to catch -- reject it with the SAME reserved-key message here,
        // before that happens, rather than let it surface as an unrelated
        // "no out port"/"no in port" error.
        if is_reserved_out_key(&from_port) || is_reserved_out_key(&to_port) {
            return Err(RjError::other(format!(
                "create-flow: conn {} names a reserved endpoint -- {} and {} are engine-reserved \
                 out-targets always wired by flow/start; a conn must not use them",
                crate::printer::pr_str(c),
                crate::printer::pr_str(&report_out_key()),
                crate::printer::pr_str(&error_out_key())
            )));
        }

        let from_def = procs
            .get(&from_pid)
            .ok_or_else(|| RjError::other(format!("create-flow: conn references unknown pid {}", crate::printer::pr_str(&from_pid))))?;
        if !from_def.outs.contains(&from_port) {
            return Err(RjError::other(format!(
                "create-flow: proc {} has no out port {}",
                crate::printer::pr_str(&from_pid),
                crate::printer::pr_str(&from_port)
            )));
        }
        let to_def = procs
            .get(&to_pid)
            .ok_or_else(|| RjError::other(format!("create-flow: conn references unknown pid {}", crate::printer::pr_str(&to_pid))))?;
        if !to_def.ins.contains(&to_port) {
            return Err(RjError::other(format!(
                "create-flow: proc {} has no in port {}",
                crate::printer::pr_str(&to_pid),
                crate::printer::pr_str(&to_port)
            )));
        }

        conns.push(FlowConn { from_pid, from_port, to_pid, to_port });
    }

    let proc_order = sorted_by_pr_str(procs.keys().cloned().collect());
    // **L5/W3 fence #1 (design §4): REFUSE the thread world under sim.**
    // Checked here, at the END of validation and BEFORE a single spawn (the
    // flow does not exist yet, so there is nothing to tear down), and walked
    // in `proc_order` so the proc named in the message is deterministic.
    sim_refuse_thread_world(&procs, &proc_order)?;
    let def = FlowDef { procs, proc_order, conns };
    let cell = Arc::new(FlowCell {
        def,
        phase: Mutex::new(FlowPhase::Created),
        runtime: Mutex::new(None),
    });
    // Tracked WEAKLY for `Engine::shutdown` (`crate::embed::engine`) --
    // see `Interp::flow_registry`'s field doc for the full sharing
    // contract (`fork` shares this registry, `snapshot` starts a fresh
    // one). A plain push: nothing here prunes dead entries as it goes
    // (that's `Engine::shutdown`'s job, the one and only reader), so a
    // script that creates and drops many short-lived flows without ever
    // calling `shutdown` accumulates dead `Weak`s here -- bounded by
    // however many `create-flow` calls the script makes, same shape as
    // any other per-interpreter bookkeeping `Vec`.
    lock_mutex(&interp.flow_registry).push(Arc::downgrade(&cell));
    Ok(Value::Flow(cell))
}

fn port_ids(describe_map: &PMap, key: &str, pid: &Value) -> Result<Vec<Value>, RjError> {
    match describe_map.get(&plain_kw(key)) {
        None => Ok(Vec::new()),
        Some(Value::Map(m)) => Ok(sorted_by_pr_str(m.keys().cloned().collect())),
        Some(other) => Err(RjError::type_err(format!(
            "proc {}: describe()'s :{key} must be a map, got {}",
            crate::printer::pr_str(pid),
            other.type_name()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Transport lanes: the 1:1 kernel substituted for a conn's general `Chan`.
// See the module doc's "Transport selection" section for the whole design.
// ---------------------------------------------------------------------------

/// True when `MOVA_NO_SPSC=1` was set in the environment at process start
/// -- the transport tier's kill switch, read exactly ONCE (an `OnceLock`,
/// mirroring `MOVA_NO_FASTSTEP`/`MOVA_NO_FUSION`/`MOVA_NO_COMPILE`) so no
/// wiring pass ever touches the environment. With it set no link is planned,
/// no lane is built, and every conn is wired exactly as it was pre-T2.
fn spsc_disabled_by_env() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_NO_SPSC").is_ok_and(|v| v == "1"))
}

/// One conn, identified by its two endpoints -- [`plan_transport_links`]'s
/// output and the key the wiring pass looks lanes up by.
pub(crate) type LinkKey = ((Value, Value), (Value, Value));

/// The CONSUMER half of a transport-backed in-port.
///
/// `chan` is the general `Chan` that stays wired for this port (it is what
/// `flow/inject` writes and what `stop` closes) and doubles as the lane's
/// IDENTITY: a lane applies to a read iff the chan being read is pointer-
/// identical to this one, which is both cheaper than comparing port-id
/// keywords per message and exactly the right question -- an `init` that
/// replaced this port via `::flow/in-ports` hands the loop a different
/// `Arc`, and the lane then correctly does not apply.
struct InLane {
    rx: SpscRx,
    chan: Arc<Chan>,
    /// Shared with `FlowRuntime::initial_ins`: `flow/inject` sets it after
    /// every put, this proc clears it before every drain attempt. See
    /// [`InLane::side_take`] for why that order is the strand-free one.
    gate: Arc<AtomicBool>,
    /// "The last bounded wait on `rx` timed out", i.e. the producer is
    /// idle. Makes the next wait park immediately instead of re-spending a
    /// 4000-hint spin budget every millisecond -- which for a quiescent
    /// pipeline would burn a few percent of a core per proc for nothing.
    cold: Cell<bool>,
}

/// The PRODUCER half of a transport-backed out-port. `chan` is the wired
/// destination in-chan, kept for the same identity role [`InLane::chan`]
/// plays (an `init` that replaced this out-port via `::flow/out-ports`
/// hands the loop a different `Arc`, and the lane stops applying).
struct OutLane {
    tx: SpscTx,
    chan: Arc<Chan>,
    /// As [`InLane::cold`], for the "downstream is full" wait.
    cold: Cell<bool>,
}

impl InLane {
    /// Drains ONE injected message from the side channel, if the gate says
    /// there might be one.
    ///
    /// The gate is cleared BEFORE the attempt and re-armed on success. That
    /// order is what makes it strand-free against `native_inject`'s
    /// `chan_put` then `store(true)`: for a message to be missed, this
    /// method's `chan_try_take` would have to run before the put while its
    /// own `store(false)` ran after the injector's `store(true)` -- and the
    /// store comes first here, so that ordering is not constructible. The
    /// worst case is the opposite one, a gate left `true` with an empty
    /// chan, which costs one extra non-blocking take on the next lap.
    fn side_take(&self) -> TryTake {
        if !self.gate.load(Ordering::Acquire) {
            return TryTake::WouldBlock;
        }
        self.gate.store(false, Ordering::Relaxed);
        match chan_try_take(&self.chan) {
            TryTake::Received(v) => {
                self.gate.store(true, Ordering::Relaxed);
                TryTake::Received(v)
            }
            // `Closed`/`WouldBlock` on the SIDE channel says nothing about
            // the port: its engine traffic is on the transport. The caller
            // decides the port's state from `rx`.
            other => other,
        }
    }

    /// **The multi-input park.** Waits for this lane's ring to have data OR
    /// for `doorbell` to ring, consuming nothing, bounded by `timeout`.
    ///
    /// This exists because `builtins::flow`'s multi-input round-robin parks
    /// on the DOORBELL alone, and a doorbell is not a complete wake source
    /// for a proc that holds a lane: engine traffic on a lane-backed port
    /// arrives on the lock-free ring, which rings nothing. A proc can be
    /// multi-input AND lane-holding whenever its `init` widens the read set
    /// past its DECLARED ports (`::flow/in-ports`) -- the planner only sees
    /// declared ports, so it cannot rule that out.
    ///
    /// On a THREAD that gap was merely a slow lap: `PARK_TIMEOUT` (2 s)
    /// expired and the next scan found the message. On a TASK it is a
    /// PERMANENT park -- the task arm of every park in this engine has no
    /// safety net by design (the L1 landing stance) -- so once L3.6/W1 made
    /// task procs lane-eligible this became a hang, caught by
    /// `tests/flow_test.rs`'s init-supplied-in-ports case. Parking on BOTH
    /// sources fixes both tiers at once: the thread stops eating a 2 s
    /// stall, and the task stops hanging.
    ///
    /// Consuming nothing is the whole point (see
    /// `transport::SpscRx::wait_readable_or_doorbell`): the caller re-scans
    /// every port from the top of its next lap, and a primitive that handed
    /// back a message here would deliver it outside that scan.
    fn wait_readable(&self, doorbell: &Doorbell, seen: u64, timeout: Duration) {
        let woke = if self.cold.get() {
            self.rx.wait_readable_or_doorbell_cold(timeout, doorbell, seen)
        } else {
            self.rx.wait_readable_or_doorbell(timeout, doorbell, seen)
        };
        self.cold.set(!woke);
    }
}

/// The lane that applies to a read of `chan`, if any. A pointer compare;
/// see [`InLane::chan`] for why identity is the right test and why no two
/// ports of one proc can ever alias (a proc only gets a lane when the port
/// in question is its ONLY one of that direction).
#[inline(always)]
fn in_lane_for<'a>(lane: Option<&'a InLane>, chan: &Arc<Chan>) -> Option<&'a InLane> {
    lane.filter(|l| Arc::ptr_eq(&l.chan, chan))
}

#[inline(always)]
fn out_lane_for<'a>(lane: Option<&'a OutLane>, chan: &Arc<Chan>) -> Option<&'a OutLane> {
    lane.filter(|l| Arc::ptr_eq(&l.chan, chan))
}

/// Non-blocking read of one wired in-port -- [`chan_try_take`] with the
/// transport substituted where a lane applies. Used by the single-input
/// batch drain and by the multi-input round-robin, so both see the same
/// three outcomes with the same meanings.
///
/// A `Closed` transport does NOT close the port on its own: injected
/// messages may still be sitting in the side channel, and the drain-then-
/// closed rule applies to the port as a whole.
#[inline]
fn in_try_take(lane: Option<&InLane>, chan: &Arc<Chan>) -> TryTake {
    let Some(l) = in_lane_for(lane, chan) else { return chan_try_take(chan) };
    match l.rx.try_take() {
        LinkTake::Received(v) => TryTake::Received(v),
        LinkTake::WouldBlock => l.side_take(),
        LinkTake::Closed => chan_try_take(&l.chan),
    }
}

/// Top up `batch` (which already holds the lap's first message) to at most
/// [`BATCH_DRAIN_MAX`], the cheapest way the port allows.
///
/// With NO lane -- a conn `plan_transport_links_with` didn't grant one to
/// (fan-out/fan-in, a self-loop, a fused-run member) or any conn at all
/// under `sim` (see that fn's doc; a task proc is lane-eligible as of
/// L3.6/W1, not excluded) -- that is ONE `chan_drain_into` call: one
/// chan-mutex acquisition, one `promote_task_putters` scan, one
/// `Condvar::notify_all` and one `Doorbell::ring` for the whole batch,
/// where the old loop paid all four
/// per message (docs/FLOW-HOP-RECOVERY.md). On a saturated 1:1 conn that
/// mutex is contended between two cores, which is what makes the
/// per-message version expensive out of proportion to its ~25ns
/// uncontended price.
///
/// With a lane the loop is the old one, byte for byte: `transport.rs`'s
/// `Ring` is already a lock-free per-message `try_take` with nothing to
/// amortize, and its `WouldBlock` arm has to fall through to the side
/// channel per message anyway.
#[inline]
fn drain_batch(lane: Option<&InLane>, chan: &Arc<Chan>, batch: &mut Vec<Value>) {
    if in_lane_for(lane, chan).is_none() {
        crate::builtins::r#async::chan_drain_into(chan, batch, BATCH_DRAIN_MAX - batch.len());
        return;
    }
    while batch.len() < BATCH_DRAIN_MAX {
        match in_try_take(lane, chan) {
            TryTake::Received(v) => batch.push(v),
            _ => break,
        }
    }
}

/// The bounded blocking read: [`try_take_with_timeout`] with the transport
/// substituted where a lane applies. The `timeout` is the module doc's
/// control-latency cap and means the same thing on both paths; `seen` is the
/// caller's doorbell-generation snapshot, taken at the TOP of its lap --
/// before its non-blocking control check, not after (see
/// [`try_take_with_timeout`]'s doc for why that is a correctness rule and
/// not a style preference).
#[inline]
fn in_take_timeout(lane: Option<&InLane>, chan: &Arc<Chan>, doorbell: &Doorbell, seen: u64, timeout: Duration) -> TryTake {
    // L3.6/W1: BOTH arms below are now task-safe, and the wall that used to
    // stand here is gone. `try_take_with_timeout` has had a task arm since
    // L1/W3 (`Doorbell::wait_for_change_task`); the LANE arm has one as of
    // this wave -- `SpscRx::take_timeout_or_doorbell` forks at its park step
    // into `transport.rs`'s two-source park, which registers on this very
    // `doorbell` alongside the ring's own waiter slot. `seen` is what ties
    // the two together: it is the SAME pre-scan snapshot both arms use, and
    // the lane arm re-uses it as the doorbell's register-then-recheck
    // baseline, so the missed-wakeup argument is one argument, not two.
    let Some(l) = in_lane_for(lane, chan) else { return try_take_with_timeout(chan, doorbell, seen, timeout) };
    if let TryTake::Received(v) = l.side_take() {
        return TryTake::Received(v);
    }
    // `timeout` (== `PARK_TIMEOUT`) -- the SAME long safety net the
    // general `Chan` path uses, not a separate short constant: a genuine
    // control/inject ring reaches this park on a THREAD via `Doorbell::
    // ring`'s `owner_thread.unpark()` PLUS the generation check
    // `take_timeout_or_doorbell` adds inside the ring's own wait loop, and
    // on a TASK via that same generation check plus the doorbell
    // registration `park_task_two_source` makes before suspending
    // (transport.rs). Either way this deadline is a backstop, not the live
    // mechanism -- and on the task arm it is not even reachable, since a
    // task park has no timer at all. `seen` was snapshotted by the
    // CALLER at the top of its lap, per the generation-counter
    // "missed-wakeup correctness" rule (`value.rs`'s `Doorbell` doc) --
    // earlier than this function could take it, and deliberately so.
    let taken = if l.cold.get() {
        l.rx.take_timeout_or_doorbell_cold(timeout, doorbell, seen)
    } else {
        l.rx.take_timeout_or_doorbell(timeout, doorbell, seen)
    };
    match taken {
        LinkTake::Received(v) => {
            l.cold.set(false);
            TryTake::Received(v)
        }
        LinkTake::WouldBlock => {
            l.cold.set(true);
            TryTake::WouldBlock
        }
        // The transport is closed and drained; only injected messages can
        // still arrive. Park on the side channel from here on -- returning
        // `Closed` while that chan is still open would both lie about the
        // port and turn the proc's idle loop into a spin.
        LinkTake::Closed => try_take_with_timeout(&l.chan, doorbell, seen, timeout),
    }
}

/// [`send_with_control_priority`] with the transport substituted where a
/// lane applies: identical shape, identical outcomes, identical control
/// latency -- `try_put`, and on a full downstream check `control`
/// non-blocking (handing any command BACK to the caller to apply) before a
/// capped wait for room.
#[inline]
fn out_send(lane: Option<&OutLane>, chan: &Arc<Chan>, msg: &Value, control: &Chan, doorbell: &Arc<Doorbell>) -> SendOutcome {
    let Some(l) = out_lane_for(lane, chan) else { return send_with_control_priority(chan, msg, control, doorbell) };
    // L3.6/W1: the lane's blocked-send park forks by execution tier, exactly
    // as [`send_with_control_priority`]'s does, and for the same reason. A
    // THREAD keeps the pre-existing pair byte for byte -- a bounded
    // `wait_writable`/`_cold` whose 1 ms cap is itself the control-latency
    // mechanism. A TASK cannot use that: its park has no timer, so the
    // deadline it would wait on does not exist, and a park on the ring
    // ALONE would sleep through `pause`/`stop` until the downstream drained.
    // The `_or_doorbell` pair registers the proc's own doorbell as a second
    // source, which every control command already rings (`run_ready` puts it
    // in the control chan's `ChanState::doorbell` slot at spawn) -- so the
    // task arm's control latency is the ring's, strictly better than the
    // thread arm's 1 ms of poll. One TLS read, hoisted out of the loop.
    let task = crate::runtime::in_task();
    loop {
        match l.tx.try_put(msg.clone()) {
            LinkPut::Sent => {
                l.cold.set(false);
                return SendOutcome::Sent;
            }
            LinkPut::Closed => return SendOutcome::Closed,
            LinkPut::WouldBlock => {
                // The doorbell snapshot goes HERE -- inside the `WouldBlock`
                // arm, before the non-blocking control scan it must precede,
                // and NOT at the top of the loop where the `Sent` fast path
                // would pay a doorbell mutex per message. That placement is
                // the generation-counter rule applied exactly: a ring landing
                // after this snapshot is either found by the `chan_try_take`
                // below, or moves the generation past `seen` so the wait
                // returns immediately instead of parking (`value.rs`'s
                // `Doorbell` doc). Nothing between the snapshot and the park
                // can be lost.
                let seen = if task { doorbell.current() } else { 0 };
                if let TryTake::Received(cmd) = chan_try_take(control) {
                    return SendOutcome::Control(cmd);
                }
                let woke = match (task, l.cold.get()) {
                    (true, false) => l.tx.wait_writable_or_doorbell(BLOCKED_SEND_TIMEOUT, doorbell, seen),
                    (true, true) => l.tx.wait_writable_or_doorbell_cold(BLOCKED_SEND_TIMEOUT, doorbell, seen),
                    (false, false) => l.tx.wait_writable(BLOCKED_SEND_TIMEOUT),
                    (false, true) => l.tx.wait_writable_cold(BLOCKED_SEND_TIMEOUT),
                };
                l.cold.set(!woke);
            }
        }
    }
}

/// WHICH conns leave the general `Chan` for the 1:1 transport kernel. The
/// full rule, and the reasoning behind every clause, is in the module doc's
/// "Transport selection" section; this is that rule as code.
///
/// `runs` is [`plan_fusion`]'s output, needed for the last clause: a member
/// of a multi-proc fused run never uses a channel for its internal hops at
/// all, so those conns are left alone and [`run_fused`] stays entirely
/// transport-unaware.
///
/// `pub(crate)` for the same reason [`plan_fusion_with`] is: a differential
/// can never fail if the transport is silently never selected (falling back
/// to `Chan` is always *correct*), so the DECISION is what has to be
/// asserted on directly -- including the negative cases, which is how
/// `flow-fanout5`, this file's historical wake-regression canary, is pinned
/// to the general `Chan` by a test rather than by hope.
pub(crate) fn plan_transport_links(def: &FlowDef, runs: &[Vec<Value>], enabled: bool) -> Vec<LinkKey> {
    plan_transport_links_with(def, runs, enabled, crate::clock::sim_enabled())
}

/// [`plan_transport_links`] with the task-proc switch passed in rather than
/// read from the process-wide `OnceLock` -- the unit tests' entry point, for
/// exactly the reason [`plan_fusion_with`] has one: the real switch is fixed
/// for a process's whole life, so a test that wants to see the OTHER world
/// has no other way in. (And the narrowing below is a NEGATIVE decision --
/// "this conn does not get a lane" -- which is the kind a behavioral
/// differential can never catch, since falling back to `Chan` is always
/// correct. It has to be asserted on directly.)
///
/// **L3.6/W1: the task-proc exclusion is GONE, and `sim` replaces it as the
/// one non-topological clause.**
///
/// The excluded clause used to read "a conn gets a lane only when NEITHER
/// endpoint is a task proc", because `transport.rs`'s `Ring` parked on
/// `std::thread::Thread` identity and had no task arm (L3 §3.2), so a lane
/// between task procs would not have been slower, it would have been a wall.
/// That wall is down: the `Ring`'s two `_or_doorbell` waits now fork at the
/// park step into a genuine two-source park (`Ring::park_task_two_source`),
/// registered on the ring's waiter slot AND the proc's `Doorbell`, so a
/// lane-parked task notices data, close, control and inject alike. See
/// `transport.rs`'s "THE TASK ARM" section and docs/FLOW-HOP-RECOVERY.md §7.
///
/// What replaces it is `sim`: under `(simulate {:seed ..})` no conn gets a
/// lane, ever. Sim is a DETERMINISM path, not a throughput path, and a
/// lock-free lane is schedule-dependent by construction -- its spin budget,
/// its park/wake interleaving and the order two racing RMWs land in are all
/// facts about real threads that the simulated schedule neither controls nor
/// reproduces. The L5 fences already put EVERY proc of a simulated flow on a
/// task ([`sim_refuse_thread_world`] refuses both `:io` procs and
/// `MOVA_FLOW_THREAD_PROCS=1`), so before this wave sim got no lane purely
/// as a side effect of the task exclusion. Making that a clause of its own
/// keeps the byte-identical-run contract a PROPERTY OF THE PLANNER rather
/// than an accident of an unrelated rule -- and it is asserted directly, by
/// `sim_mode_refuses_every_lane` below, because a missing lane is a negative
/// decision no behavioural differential can ever fail on.
pub(crate) fn plan_transport_links_with(
    def: &FlowDef,
    runs: &[Vec<Value>],
    enabled: bool,
    sim: bool,
) -> Vec<LinkKey> {
    if !enabled || sim {
        return Vec::new();
    }
    let fused: std::collections::HashSet<&Value> =
        runs.iter().filter(|r| r.len() > 1).flat_map(|r| r.iter()).collect();

    // Distinct targets per source out-port and distinct sources per
    // destination in-port. Distinct, not raw counts: `start`'s wiring
    // collapses a repeated identical conn, and so must this.
    let mut targets: HashMap<(Value, Value), Vec<(Value, Value)>> = HashMap::new();
    let mut sources: HashMap<(Value, Value), Vec<(Value, Value)>> = HashMap::new();
    for c in &def.conns {
        let from = (c.from_pid.clone(), c.from_port.clone());
        let to = (c.to_pid.clone(), c.to_port.clone());
        let t = targets.entry(from.clone()).or_default();
        if !t.contains(&to) {
            t.push(to.clone());
        }
        let s = sources.entry(to).or_default();
        if !s.contains(&from) {
            s.push(from);
        }
    }

    let mut links: Vec<LinkKey> = Vec::new();
    for c in &def.conns {
        let from = (c.from_pid.clone(), c.from_port.clone());
        let to = (c.to_pid.clone(), c.to_port.clone());
        if links.iter().any(|(f, t)| f == &from && t == &to) {
            continue; // a repeated identical conn is ONE link
        }
        if c.from_pid == c.to_pid {
            continue; // self-loop: always via mult
        }
        if targets.get(&from).map(Vec::len) != Some(1) {
            continue; // fan-out: the mult (task or thread) is the writer
        }
        if sources.get(&to).map(Vec::len) != Some(1) {
            continue; // fan-in: the destination chan has several writers
        }
        let (Some(from_def), Some(to_def)) = (def.procs.get(&c.from_pid), def.procs.get(&c.to_pid)) else { continue };
        if from_def.outs.len() != 1 || to_def.ins.len() != 1 {
            continue; // at most one lane per proc, per direction
        }
        if buf_for(to_def, &c.to_port) == 0 {
            continue; // `Fixed(0)` is not a rendezvous here; see the module doc
        }
        if fused.contains(&c.from_pid) || fused.contains(&c.to_pid) {
            continue; // fused > transport: that hop is already gone
        }
        links.push((from, to));
    }
    links
}

// ---------------------------------------------------------------------------
// `start`: wiring + proc thread spawn
// ---------------------------------------------------------------------------

fn buf_for(def: &ProcDef, port: &Value) -> usize {
    def.chan_opts.get(port).copied().unwrap_or(DEFAULT_BUF)
}

/// Where a flow's TASK runs go (L3 §3.6, as revised by the landing spec's
/// segment model). Thread runs -- `:workload :io`, mixed fused runs, and
/// everything under `MOVA_FLOW_THREAD_PROCS=1` -- are unaffected by every
/// variant here: an OS thread's placement belongs to the OS.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Placement {
    /// The default: [`segment_shards`], with segments capped at
    /// `max_len` procs ([`MAX_SEGMENT_LEN`], or `MOVA_FLOW_PLACEMENT=seg:<L>`).
    Segment { max_len: usize },
    /// `MOVA_FLOW_PLACEMENT=rr` -- plain `runtime::spawn`, i.e. the
    /// runtime's own blind round-robin (with its family-locality
    /// heuristic, which for a flow means "wherever `flow/start`'s caller
    /// happens to be"). Both the A/B lever P4a measures segments against
    /// and the escape hatch if segments ever lose on a real graph.
    RoundRobin,
    /// `MOVA_FLOW_PLACEMENT=pin:<N>` -- every task run of every flow onto
    /// shard N. A PROBE SEAM, not a supported mode: it deliberately
    /// serializes the graph. P4a needs it to measure a co-shard proc hop in
    /// isolation (segments split a 2-run chain across two shards by
    /// construction -- see [`segment_shards`]), and P4b needs it to park a
    /// flow's procs on the same shard it is saturating with noise.
    ///
    /// "Every task run" is meant literally, and since L3.5 item 2 it covers
    /// the MULT too -- the one placement decision the mult takes part in.
    /// The other two modes leave it to the runtime (a mult belongs to no
    /// run, so segments have nothing to say about it), but this mode's
    /// whole contract is "everything on one shard", and a test needs that
    /// to force the adversarial self-loop shape -- mult and the proc it
    /// feeds on ONE shard, where the blocking put's yield is the only thing
    /// between the flow and a deadlock. See [`run_mult_task`].
    Pin(usize),
}

/// Read exactly ONCE per process (an `OnceLock`, mirroring
/// [`spsc_disabled_by_env`] and every other switch in this module), so no
/// proc spawn ever touches the environment.
fn flow_placement() -> Placement {
    static FLAG: OnceLock<Placement> = OnceLock::new();
    *FLAG.get_or_init(|| match std::env::var("MOVA_FLOW_PLACEMENT") {
        Ok(v) if v == "rr" => Placement::RoundRobin,
        Ok(v) => {
            if let Some(n) = v.strip_prefix("pin:").and_then(|n| n.parse::<usize>().ok()) {
                Placement::Pin(n)
            } else if let Some(l) = v.strip_prefix("seg:").and_then(|n| n.parse::<usize>().ok()) {
                Placement::Segment { max_len: l.max(1) }
            } else {
                // Unrecognized values are the default, not an error: the
                // same permissive reading `proc_workload` gives an unknown
                // keyword.
                Placement::Segment { max_len: MAX_SEGMENT_LEN }
            }
        }
        Err(_) => Placement::Segment { max_len: MAX_SEGMENT_LEN },
    })
}

/// Round-robin base for [`Placement::Segment`], bumped once per
/// `flow/start`. Two flows started back to back therefore begin their
/// segment walk on different shards, so a process running many small flows
/// spreads them instead of stacking every flow's segment 0 onto shard 0.
static FLOW_PLACEMENT_BASE: AtomicUsize = AtomicUsize::new(0);

/// White-box mult-spawn census (L3.5 item 2), one counter per world, bumped
/// at the ONE spawn seam in [`native_start`]. Process-global and
/// monotonically increasing, so a caller measures a DELTA across a
/// `flow/start`, never an absolute.
///
/// Both are kept, not just the task one, because the claim under test is a
/// DIFFERENTIAL -- "a fan-out conn costs an OS thread under
/// `MOVA_FLOW_THREAD_PROCS=1` and costs zero of them by default" -- and a
/// task-only counter could not tell "the mult became a task" from "the mult
/// stopped being wired at all". Read by [`mult_spawn_census`], whose only
/// callers are this file's unit tests.
static MULT_TASK_SPAWNS: AtomicUsize = AtomicUsize::new(0);
static MULT_THREAD_SPAWNS: AtomicUsize = AtomicUsize::new(0);

/// `(task mults, thread mults)` spawned by this process so far. See
/// [`MULT_TASK_SPAWNS`].
pub(crate) fn mult_spawn_census() -> (usize, usize) {
    (MULT_TASK_SPAWNS.load(Ordering::Relaxed), MULT_THREAD_SPAWNS.load(Ordering::Relaxed))
}

/// Longest run of consecutive procs [`segment_shards`] will put on one
/// shard. MEASURED, not chosen (W3/P4a, docs/L3-PROBE-RESULTS or the design
/// doc's §6 table): on a 14-shard box, a 10,000-proc chain cut into 14
/// segments of ~715 runs 47.8 ns/hop, which is 2.3x SLOWER than blind
/// round-robin's 20.9 -- while the same placement at segment lengths 8
/// (N=100) and 72 (N=1000) beats round-robin by 1.48x and 1.31x. Locality
/// pays up to a point and then stops paying; past it, a shard is running a
/// long serial stretch of one pipeline and the machine is better off
/// interleaving. So a segment is capped and a long chain simply gets MORE
/// segments than there are shards -- shard `s` then hosts segments `s`,
/// `s + shard_count`, ... , which are far apart in the chain and do not
/// serialize against each other.
///
/// `MOVA_FLOW_PLACEMENT=seg:<L>` overrides it (the probe's sweep lever,
/// and the tuning knob if a future box moves the crossover).
const MAX_SEGMENT_LEN: usize = 64;

/// The shard index for each of `runs`, or `None` for a run that is not
/// task-spawned (`task_runs[i] == false`) -- those keep their OS thread and
/// this function has nothing to say about them.
///
/// **The segment model, and why it is neither "one chain, one shard" nor
/// "one segment per shard".** A flow's procs pass every message to their
/// successor, so a neighbor pair wants to be co-shard: W3/P4a prices a
/// co-shard proc hop at ~57 ns marginal against ~81 ns cross-shard. Design
/// §3.6's first sketch followed that all the way -- whole chain, one shard
/// -- and it is badly wrong at scale: a 10,000-proc pipeline pinned to one
/// shard runs on one core and measures 64 ns/hop where the shipped
/// placement measures 17. The parallelism a pipeline has IS its stages
/// running at once.
///
/// The landing spec's revision -- cut the walk into exactly `shard_count`
/// segments -- fixes that and then overshoots in the other direction, and
/// P4a caught it: on a 14-shard box that makes a 10k chain 14 segments of
/// ~715, and it measured 47.8 ns/hop against blind round-robin's 20.9. A
/// shard running a 715-deep serial stretch of one pipeline is most of the
/// way back to the pinned case. The same placement at segment lengths 8 and
/// 72 BEAT round-robin (1.48x, 1.31x), so the variable was never
/// "segments vs not" -- it is how long a serial stretch one shard should
/// own. The sweep at N=10,000 is flat from 4 to 128 procs per segment
/// (16.5-16.9 ns/hop) and falls off a cliff after: 23.6 at 512, 46.5
/// uncapped. Hence [`MAX_SEGMENT_LEN`] = 64, mid-plateau.
///
/// So: walk the graph, cut it into `max(shard_count, ceil(n / max_len))`
/// contiguous segments, and hand segment `i` to shard `(base + i) %
/// shard_count`. A 10k chain on 14 shards becomes 157 segments of 64; each
/// shard hosts ~11 of them, chosen 14 apart in the chain so they never feed
/// each other. 9,843 of the 9,999 neighbor pairs are co-shard and 156 cross.
/// Measured: 17.6 ns/hop, 1.17x over round-robin, 2.7x over the uncapped
/// segmentation this replaced.
///
/// A graph SMALLER than the machine gets one run per segment, which is blind
/// round-robin's placement arrived at honestly -- worth stating plainly,
/// because it means segments buy nothing there (every hop crosses either
/// way). Segments are a large-graph mechanism, and P4a measures exactly that.
///
/// **The walk.** BFS from the source procs -- those with no in-conn -- in
/// `proc_order`, following conns. It is the order in which data flows, which
/// is the order that puts producers next to consumers; `proc_order`
/// (pr_str-sorted pids) would place `:p1 :p10 :p11 :p2` adjacent, which is
/// alphabetical, not topological. Cycles and orphans (a proc no BFS ever
/// reaches, e.g. a ring graph where every proc has an in-conn) are appended
/// in `proc_order` afterwards, so the walk is TOTAL and deterministic on
/// every graph shape, DAG or not. A run is ranked by its earliest member,
/// which for a fused run is its head.
///
/// Deterministic given (`def`, `runs`, `base`, `shard_count`, `max_len`) --
/// no clock, no map iteration order, no runtime state. `base` is the
/// caller's business ([`FLOW_PLACEMENT_BASE`]).
fn segment_shards(
    def: &FlowDef,
    runs: &[Vec<Value>],
    task_runs: &[bool],
    base: usize,
    shard_count: usize,
    max_len: usize,
) -> Vec<Option<usize>> {
    debug_assert_eq!(runs.len(), task_runs.len(), "one task-ness flag per run");
    let mut out = vec![None; runs.len()];
    if shard_count == 0 {
        return out;
    }

    // Successors, and who has an in-conn at all (dedup: a repeated conn must
    // not make a pid look like two successors, and a fan-out lists several).
    let mut succ: HashMap<&Value, Vec<&Value>> = HashMap::new();
    let mut has_in: HashMap<&Value, bool> = HashMap::new();
    for c in &def.conns {
        let s = succ.entry(&c.from_pid).or_default();
        if !s.contains(&&c.to_pid) {
            s.push(&c.to_pid);
        }
        has_in.insert(&c.to_pid, true);
    }

    // BFS from every source, then sweep up whatever the walk never reached.
    let mut rank: HashMap<&Value, usize> = HashMap::with_capacity(def.proc_order.len());
    let mut queue: std::collections::VecDeque<&Value> = std::collections::VecDeque::new();
    for pid in &def.proc_order {
        if !has_in.get(pid).copied().unwrap_or(false) {
            queue.push_back(pid);
            rank.insert(pid, rank.len());
        }
    }
    while let Some(pid) = queue.pop_front() {
        for next in succ.get(pid).into_iter().flatten() {
            if !rank.contains_key(*next) {
                rank.insert(next, rank.len());
                queue.push_back(next);
            }
        }
    }
    for pid in &def.proc_order {
        if !rank.contains_key(pid) {
            rank.insert(pid, rank.len());
        }
    }

    // Task runs in walk order. `min` over members rather than `runs[i][0]`
    // because a fused run's member list is built head-first but nothing here
    // depends on that; the tie-break on run index keeps the sort total.
    let mut ordered: Vec<usize> = (0..runs.len()).filter(|i| task_runs[*i]).collect();
    ordered.sort_by_key(|i| {
        let r = runs[*i].iter().filter_map(|pid| rank.get(pid)).copied().min().unwrap_or(usize::MAX);
        (r, *i)
    });

    // Contiguous even slice: position p of n falls in segment `p * k / n`.
    // Segment sizes differ by at most one and no segment is skipped while a
    // later one fills, which a `p / ceil(n / k)` chunking would do (n = 15,
    // k = 14 chunks by 2 and leaves seven shards empty).
    let n = ordered.len();
    // At least one segment per shard (so a short chain still uses the whole
    // machine), and more than that as soon as `max_len` would be exceeded.
    let segments = shard_count.max(n.div_ceil(max_len.max(1)));
    for (p, run_idx) in ordered.into_iter().enumerate() {
        let segment = p * segments / n; // n > 0 whenever this loop runs
        out[run_idx] = Some((base.wrapping_add(segment)) % shard_count);
    }
    out
}

fn native_start(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/start: expected 1 argument, got {}", args.len())));
    }
    let flow = expect_flow(&args[0], "flow/start")?;
    {
        let mut phase = lock_mutex(&flow.phase);
        if *phase != FlowPhase::Created {
            return Err(RjError::other("flow/start: flow has already been started"));
        }
        *phase = FlowPhase::Running;
    }

    let report_chan = Arc::new(Chan::new(BufferPolicy::Sliding(DIAG_BUF)));
    let error_chan = Arc::new(Chan::new(BufferPolicy::Sliding(DIAG_BUF)));
    let mut engine_owned_chans: Vec<Arc<Chan>> = Vec::new();
    let mut engine_owned_links: Vec<SpscCloser> = Vec::new();
    let mut initial_ins: HashMap<(Value, Value), InjectPort> = HashMap::new();
    let mut ins_by_pid: HashMap<Value, HashMap<Value, Arc<Chan>>> = HashMap::new();
    let mut outs_by_pid: HashMap<Value, HashMap<Value, Option<Arc<Chan>>>> = HashMap::new();
    let mut mult_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    // Read ONCE for the whole `start`, so no two decisions below (the mult
    // spawn seam, the proc spawn seam, transport eligibility) can see
    // different answers. Hoisted above the wiring loops in L3.5 because the
    // mult spawn -- the FIRST of the three -- now reads it too.
    let task_procs = flow_task_procs_enabled();
    // L4 W1: this flow is "supervised" iff `create-flow` resolved at least
    // one proc to a `:policy :restart` `SupervisionCfg` (`ProcDef::supervision`'s
    // doc). Decided once, here, before any run is spawned, so every run's
    // `ExitGuard` shares the exact same `Option<Arc<Chan>>` -- `sup_chan` is
    // per-FLOW, not per-proc, matching D-A axiom 4 ("the supervisor holds
    // the complete incarnation table"): every proc's exit lands on it, not
    // just the individually-`:restart`-configured ones, so the (W2)
    // supervisor can see the whole flow rather than a filtered slice.
    // `Fixed(2 * proc_order.len())`: **two** slots per member, which is wall
    // W3's arithmetic after L4 W3 added a second kind of message to this
    // chan. At most one in-flight DEATH event per pid (a pid's next death
    // requires its restart, and a restart is only issued after the
    // supervisor has consumed the previous death), plus at most one
    // in-flight STOP REQUEST per pid -- `flow/stop-proc` de-duplicates
    // against `FlowRuntime::stop_requested` so a script that calls it in a
    // loop still only ever puts once per pid. NOT pushed to
    // `engine_owned_chans` -- see `FlowRuntime::sup_chan`'s doc for why
    // `stop` must not be the one closing it.
    let sup_chan: Option<Arc<Chan>> = if flow.def.procs.values().any(|p| p.supervision.is_some()) {
        // `proc_order` is never empty here: `create-flow` rejects an empty
        // `:procs` map, so `Fixed(n)` below is never the degenerate
        // `Fixed(0)` policy (see the done-cell buffer comment just above
        // this fn's spawn loop for why that distinction matters).
        Some(Arc::new(Chan::new(BufferPolicy::Fixed(2 * flow.def.proc_order.len()))))
    } else {
        None
    };

    // Group conns by their source (from_pid, from_port), collapsing
    // duplicate identical conns (a repeated [[:a :out] [:b :in]] must not
    // double-deliver through the mult path).
    let mut out_targets: HashMap<(Value, Value), Vec<(Value, Value)>> = HashMap::new();
    for c in &flow.def.conns {
        let entry = out_targets
            .entry((c.from_pid.clone(), c.from_port.clone()))
            .or_default();
        let target = (c.to_pid.clone(), c.to_port.clone());
        if !entry.contains(&target) {
            entry.push(target);
        }
    }

    // One chan per DESTINATION in-port, shared by every conn that feeds it
    // -- JVM flow's fan-in semantics: all writers land in the same in-chan
    // (Chan is MPMC, so sharing is sound). Keying creation by SOURCE
    // instead -- the pre-fix shape -- made the last-wired writer's chan win
    // the `ins_by_pid` slot and silently orphaned every earlier writer's:
    // [[:a :out] [:sink :in]] [[:b :out] [:sink :in]] dropped all of :a's
    // messages. Buffer sized off the destination in-port's own :chan-opts
    // (the buffer conceptually belongs to the reader, matching core.async's
    // `(chan n)`-on-the-consumer-side idiom).
    let mut dest_chans: HashMap<(Value, Value), Arc<Chan>> = HashMap::new();
    for targets in out_targets.values() {
        for target in targets {
            if dest_chans.contains_key(target) {
                continue;
            }
            let (to_pid, to_port) = target;
            let to_def = flow.def.procs.get(to_pid).expect("validated at create-flow");
            let buf = buf_for(to_def, to_port);
            let chan = Arc::new(Chan::new(BufferPolicy::Fixed(buf)));
            engine_owned_chans.push(chan.clone());
            ins_by_pid.entry(to_pid.clone()).or_default().insert(to_port.clone(), chan.clone());
            initial_ins.insert(target.clone(), InjectPort { chan: chan.clone(), gate: None });
            dest_chans.insert(target.clone(), chan);
        }
    }

    // Transport selection (see the module doc's "Transport selection"
    // section). Decided HERE, from the same topology the fan-in wiring
    // above just walked, plus the fusion plan -- which is computed once and
    // reused for the spawn loop below, so the two decisions can never
    // disagree about which procs share a thread.
    let runs = plan_fusion(&flow.def);
    let mut in_lanes: HashMap<(Value, Value), InLane> = HashMap::new();
    let mut out_lanes: HashMap<(Value, Value), OutLane> = HashMap::new();
    // L4 W2: a RESTART-supervised proc never gets a transport lane, either
    // end of the link. A lane is a strictly-1:1 `SpscRing` whose two halves
    // are owned BY VALUE by the two proc bodies -- and an incarnation that
    // dies takes its half of the ring with it (along with whatever was
    // queued in it), which is precisely the one piece of a proc's wiring
    // that CANNOT be handed to the next incarnation. The chans, which
    // survive every incarnation, are the identity (design §3.4: "chans are
    // the identity; the proc body is disposable"), so a supervised link
    // simply stays on the general `Chan` path it would have taken anyway
    // under `MOVA_NO_SPSC=1`.
    //
    // Cheap in practice: in the DEFAULT world `plan_transport_links` already
    // narrows lanes to `:io -> :io` hops (neither endpoint may be a task
    // proc -- see its doc), so this only ever fires for a supervised `:io`
    // chain, or for anything supervised under `MOVA_FLOW_THREAD_PROCS=1`.
    // Unsupervised flows -- every flow in the corpus -- plan byte-identically.
    let link_plan: Vec<LinkKey> = plan_transport_links(&flow.def, &runs, !spsc_disabled_by_env())
        .into_iter()
        .filter(|(from, to)| !restart_supervised(&flow.def, &from.0) && !restart_supervised(&flow.def, &to.0))
        .collect();
    for (from, to) in link_plan {
        let to_def = flow.def.procs.get(&to.0).expect("validated at create-flow");
        let chan = dest_chans.get(&to).expect("an eligible link's destination chan was built above").clone();
        // Capacity is the destination in-port's `:buf-or-n`, verbatim (the
        // ring enforces the exact number, not its rounded slot count), so
        // backpressure begins where the `Chan` it replaces began.
        let (tx, rx) = SpscRing::channel(buf_for(to_def, &to.1));
        engine_owned_links.push(tx.closer());
        // The gate lets the consumer skip this chan's mutex on every lap
        // when nobody is injecting; it is shared with `flow/inject` through
        // `initial_ins`.
        let gate = Arc::new(AtomicBool::new(false));
        if let Some(port) = initial_ins.get_mut(&to) {
            port.gate = Some(gate.clone());
        }
        out_lanes.insert(from, OutLane { tx, chan: chan.clone(), cold: Cell::new(false) });
        in_lanes.insert(to, InLane { rx, chan, gate, cold: Cell::new(false) });
    }

    for pid in &flow.def.proc_order {
        let pdef = flow.def.procs.get(pid).expect("proc_order is derived from procs");
        let mut outs: HashMap<Value, Option<Arc<Chan>>> = HashMap::new();
        for out_port in &pdef.outs {
            let targets = out_targets.get(&(pid.clone(), out_port.clone())).cloned().unwrap_or_default();
            if targets.is_empty() {
                outs.insert(out_port.clone(), None);
                continue;
            }
            let is_self_loop = targets.iter().any(|(to_pid, _)| to_pid == pid);
            if targets.len() == 1 && !is_self_loop {
                // 1:1 out: write straight into the destination's shared
                // in-chan (zero copy). Other procs may write the same chan
                // -- that is the fan-in contract above, not a conflict.
                let chan = dest_chans.get(&targets[0]).expect("built above").clone();
                outs.insert(out_port.clone(), Some(chan));
            } else {
                // Fan-out or self-loop: always via a native mult (see the
                // module doc; a self-loop sharing one chan as both a proc's
                // in AND out port would deadlock a single-threaded proc
                // once that chan's buffer fills, since only the proc itself
                // could ever drain it). The mult's dests are the shared
                // destination in-chans, same as the 1:1 arm.
                let source_buf = buf_for(pdef, out_port);
                let source = Arc::new(Chan::new(BufferPolicy::Fixed(source_buf)));
                engine_owned_chans.push(source.clone());
                outs.insert(out_port.clone(), Some(source.clone()));
                let dests: Vec<Arc<Chan>> = targets
                    .iter()
                    .map(|t| dest_chans.get(t).expect("built above").clone())
                    .collect();
                let mult_source = source.clone();
                // L3.5 item 2: the mult rides the runtime in the TASK world
                // ([`run_mult_task`]) and keeps its own OS thread under the
                // kill switch ([`run_mult_thread`], untouched), off the
                // same ONE predicate every other spawn in this `start`
                // reads. Deliberately NOT per-proc: a mult belongs to no
                // `ProcDef`, has no `:workload`, and is pure engine
                // plumbing, so there is nothing for a user to opt out of --
                // `MOVA_FLOW_THREAD_PROCS=1` restores the pre-L3 engine,
                // thread for thread, and nothing else moves it.
                //
                // No placement in either SUPPORTED mode (`spawn`, not
                // `spawn_on`): a mult is not a member of any run, so
                // `segment_shards`' pipeline reasoning has nothing to say
                // about it, and hanging it off a neighbour's segment would
                // only serialize the fan-out against the very procs it
                // feeds. `pin:<N>` is the exception, and only because it is
                // a PROBE SEAM whose entire contract is "EVERYTHING of
                // every flow onto shard N" -- which is what lets a test
                // force the adversarial self-loop (mult and the proc it
                // feeds sharing one shard, so the blocking put's yield is
                // the only thing standing between the flow and a deadlock;
                // see [`run_mult_task`] and `tests/l3_task_procs_test.rs`).
                if task_procs {
                    MULT_TASK_SPAWNS.fetch_add(1, Ordering::Relaxed);
                    match flow_placement() {
                        Placement::Pin(n) => {
                            crate::runtime::spawn_on(n % crate::runtime::shard_count().max(1), move || {
                                run_mult_task(mult_source, dests)
                            });
                        }
                        Placement::Segment { .. } | Placement::RoundRobin => {
                            crate::runtime::spawn(move || run_mult_task(mult_source, dests));
                        }
                    }
                } else {
                    MULT_THREAD_SPAWNS.fetch_add(1, Ordering::Relaxed);
                    let handle = std::thread::Builder::new()
                        .name(format!("flow-mult-{}", crate::printer::display_str(pid)))
                        .spawn(crate::memstat::drained(move || run_mult_thread(mult_source, dests)))
                        .map_err(|e| RjError::other(format!("flow/start: couldn't spawn mult thread: {e}")))?;
                    mult_threads.push(handle);
                }
            }
        }
        // Design Part 2 (upstream impl.clj:161-162): every proc gets both
        // engine-reserved out-targets wired to THIS flow's report/error
        // chans, unconditionally -- whether or not the proc declares them,
        // and regardless of `:outs` shape. `real_outs` is what keeps N2
        // promotion/fusion from mistaking these two for the proc's own
        // ports; the generic dispatch loop needs no change at all (it
        // already routes any out-id present in `ctx.outs`).
        outs.insert(report_out_key(), Some(report_chan.clone()));
        outs.insert(error_out_key(), Some(error_chan.clone()));
        outs_by_pid.insert(pid.clone(), outs);
    }

    // Every DECLARED in-port gets a real chan, whether or not a `:conn`
    // feeds it -- an unconnected in-port is still a valid `flow/inject`
    // target (e.g. a generator/source proc's synthetic input), it simply
    // never receives anything unless injected into or overridden by the
    // proc's own `init` via `clojure.core.async.flow/in-ports`.
    for pid in &flow.def.proc_order {
        let pdef = flow.def.procs.get(pid).expect("proc_order is derived from procs");
        for in_port in &pdef.ins {
            let already_wired = ins_by_pid.get(pid).is_some_and(|m| m.contains_key(in_port));
            if already_wired {
                continue;
            }
            let buf = buf_for(pdef, in_port);
            let chan = Arc::new(Chan::new(BufferPolicy::Fixed(buf)));
            engine_owned_chans.push(chan.clone());
            ins_by_pid.entry(pid.clone()).or_default().insert(in_port.clone(), chan.clone());
            initial_ins.insert((pid.clone(), in_port.clone()), InjectPort { chan, gate: None });
        }
    }

    // Every proc gets its own control chan and its own `ProcSpawn`
    // regardless of fusion -- a fused member is still an independently
    // pingable/pausable proc, it just shares a thread and a read loop with
    // the rest of its run (P3, see `plan_fusion`). A run of ONE is exactly
    // the pre-P3 path, spawn for spawn.
    let mut procs_rt: HashMap<Value, ProcRuntime> = HashMap::new();
    // Task-ness per run, decided once: `segment_shards` needs it before the
    // loop (it places the task runs relative to each other), and the spawn
    // seam below reads the same vector rather than recomputing the `all`.
    let run_is_task: Vec<bool> = runs
        .iter()
        .map(|run| {
            run.iter()
                .all(|pid| is_task_proc(flow.def.procs.get(pid).expect("proc_order is derived from procs"), task_procs))
        })
        .collect();
    // Placement (see [`Placement`] / [`segment_shards`]). `None` for a run
    // means "no explicit shard": either it is a thread run, or the mode is
    // `rr` and the runtime picks.
    let shard_count = crate::runtime::shard_count();
    let run_shard: Vec<Option<usize>> = match flow_placement() {
        Placement::Segment { max_len } => {
            let base = FLOW_PLACEMENT_BASE.fetch_add(1, Ordering::Relaxed);
            segment_shards(&flow.def, &runs, &run_is_task, base, shard_count, max_len)
        }
        Placement::RoundRobin => vec![None; runs.len()],
        Placement::Pin(n) => run_is_task.iter().map(|t| t.then_some(n % shard_count.max(1))).collect(),
    };
    // L4 W2: the per-run RECIPE is built here, once, and the actual spawn is
    // [`spawn_run`] -- the same function the supervisor calls for every later
    // incarnation, so a restarted run is wired by construction exactly as
    // `start` wired it (docs/L4-LANDING-SPEC.md §W2.1). Everything that must
    // SURVIVE a death (chans, step fn, args, placement) lives in the
    // blueprint; everything an incarnation owns (done-cells, `ExitGuard`,
    // thread handle) is minted fresh per call.
    let mut blueprints: Vec<RunBlueprint> = Vec::with_capacity(runs.len());
    for (run_idx, run) in runs.into_iter().enumerate() {
        let mut members: Vec<MemberBlueprint> = Vec::with_capacity(run.len());
        for pid in &run {
            let pdef = flow.def.procs.get(pid).expect("proc_order is derived from procs");
            let control_chan = Arc::new(Chan::new(BufferPolicy::Fixed(CONTROL_BUF)));
            engine_owned_chans.push(control_chan.clone());
            let ins = ins_by_pid.remove(pid).unwrap_or_default();
            let outs = outs_by_pid.remove(pid).unwrap_or_default();
            let step_fn = extract_step_fn(&pdef.launcher)?;
            // At most one lane per direction per proc, by construction (see
            // `plan_transport_links`' single-declared-port clauses), and
            // never any at all for a member of a multi-proc fused run -- nor
            // for a restart-supervised one (see the `link_plan` filter above).
            let in_lane = pdef.ins.iter().find_map(|p| in_lanes.remove(&(pid.clone(), p.clone())));
            let out_lane = pdef.outs.iter().find_map(|p| out_lanes.remove(&(pid.clone(), p.clone())));
            debug_assert!(
                run.len() == 1 || (in_lane.is_none() && out_lane.is_none()),
                "a fused run's members must have no transport lanes"
            );
            members.push(MemberBlueprint {
                pid: pid.clone(),
                step_fn,
                args: pdef.args.clone(),
                ins,
                outs,
                in_lane,
                out_lane,
                control: control_chan,
            });
        }
        blueprints.push(RunBlueprint {
            // ONE fork per RUN, and `spawn_run` forks THIS per member per
            // incarnation. Identical to the pre-W2 shape (which forked the
            // caller's `interp` once per member): `Interp::fork` copies the
            // same snapshot fields and shares the same `Arc`s whether it is
            // called on the caller or on a fork of it, so a fork-of-a-fork is
            // field-for-field what a second direct fork would have been.
            interp: interp.fork(),
            members,
            error_chan: error_chan.clone(),
            sup_chan: sup_chan.clone(),
            // L3's ONE spawn decision, and the simplest correct rule (see the
            // module doc's "Task procs" section): the run rides the runtime
            // iff EVERY member of it is a task proc. A run of one is the
            // ordinary case; a MIXED fused run (an `:io` member chained with
            // a `:compute` one) stays an OS thread rather than being split,
            // so this never has to disagree with `plan_fusion`'s output.
            is_task: run_is_task[run_idx],
            // `Some(s)` => `spawn_on(s)` (segments, or the `pin:` probe
            // seam); `None` => `spawn`, the runtime's own placement (`rr`).
            // Retained rather than recomputed so incarnation n lands on the
            // same shard as incarnation 0 -- segment stability is a property
            // of the PIPELINE, and a restart must not re-shuffle it.
            shard: run_shard[run_idx],
        });
    }
    // L4 W3: incarnation 0's kill handles, in run order, handed to the
    // supervisor below (it owns every later incarnation's, from its own
    // `spawn_run` calls). `None` for a thread run -- see `SpawnedRun::task`.
    let mut initial_tasks: Vec<Option<crate::runtime::TaskHandle>> = Vec::with_capacity(blueprints.len());
    for bp in &mut blueprints {
        let spawned = spawn_run(bp, 0)?;
        initial_tasks.push(spawned.task);
        // The run's ONE thread handle is bookkept against its HEAD pid;
        // every other member gets `None`, so `stop` joins each thread
        // exactly once. A TASK run has no handle at all, so every one of
        // its pids gets `None` and `stop` takes the done-cell path for all
        // of them (see `wait_done_with_timeout`).
        let mut handle = spawned.thread;
        for (m, done) in bp.members.iter().zip(spawned.dones) {
            procs_rt.insert(
                m.pid.clone(),
                ProcRuntime { control_chan: m.control.clone(), done, thread: Mutex::new(handle.take()) },
            );
        }
    }

    if sup_chan.is_some() {
        SUP_CHAN_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
    // L4 W2: the supervisor's own done-cell, created here rather than inside
    // the supervisor so it is already installed in `FlowRuntime` before the
    // task exists -- `stop_flow_cell` must be able to wait on it no matter
    // how early a stop arrives (`FlowRuntime::sup_done`'s doc).
    let sup_done: Option<Arc<Chan>> = sup_chan.as_ref().map(|_| Arc::new(Chan::new(BufferPolicy::Fixed(1))));
    // L4 W5: every pid starts `Paused` -- the same "procs start paused,
    // resume begins reading" contract `FLOW-DESIGN.md` documents for the
    // proc's own `run_status`, now also recorded as the user's INITIAL
    // desired state (`FlowRuntime::desired`'s doc). There is no user intent
    // yet at `start` time; `Paused` is what "no intent expressed" collapses
    // to, matching what a freshly spawned proc actually does until the first
    // `flow/resume`.
    let desired: std::collections::HashMap<Value, DesiredState> =
        flow.def.proc_order.iter().map(|pid| (pid.clone(), DesiredState::Paused)).collect();
    let runtime = FlowRuntime {
        procs: procs_rt,
        report_chan: report_chan.clone(),
        error_chan: error_chan.clone(),
        engine_owned_chans,
        engine_owned_links,
        initial_ins,
        mult_threads: Mutex::new(mult_threads),
        sup_chan: sup_chan.clone(),
        sup_done: sup_done.clone(),
        stop_requested: Mutex::new(HashSet::new()),
        desired: Mutex::new(desired),
    };
    *lock_mutex(&flow.runtime) = Some(runtime);

    // The supervisor is spawned AFTER the runtime is installed, and that
    // ordering is load-bearing: its restart action swaps `ProcRuntime`
    // entries under `flow.runtime`, and a supervisor that could run before
    // `start` published the runtime would have to carry a "not ready yet"
    // state for a window that this one line deletes instead. Nothing can
    // have died before now either -- the first death event is put by an
    // `ExitGuard` on a proc that was spawned above and can therefore only
    // arrive on `sup_chan`, which buffers it (`Fixed(procs.len())`) until
    // the supervisor's first take.
    if let (Some(sup), Some(done)) = (sup_chan, sup_done) {
        spawn_supervisor(flow, sup, done, report_chan.clone(), error_chan.clone(), blueprints, initial_tasks);
    }

    let mut result = PMap::new();
    result.insert(plain_kw("report-chan"), Value::Channel(report_chan));
    result.insert(plain_kw("error-chan"), Value::Channel(error_chan));
    // L4 W2 (§W2.6): `flow/start`'s return map is back to its pre-L4 shape,
    // for a supervised flow and an unsupervised one alike. W1's
    // `:supervision-chan` key was scaffolding for a wave with no consumer:
    // the supervisor is the consumer now, it holds the same `Arc` directly
    // (it is spawned by this very fn), and a SECOND taker on that chan would
    // steal death events out from under it -- exposing it was a hazard, not
    // an observability surface. What a host observes instead is the
    // supervisor's own output: `:proc-exit`/`:proc-restart`/`:proc-give-up`/
    // `:proc-wedged` on `report-chan` (design §3.3, and report-chan's first
    // producer ever).
    Ok(Value::Map(result))
}

/// L4 W1's `sup_chan` allocation census -- `builtins::flow::mult_spawn_census`'s
/// sibling in shape and purpose (process-global, monotonic; measure a DELTA
/// across a `flow/start`, never an absolute), for the same reason: "does
/// this flow allocate a `sup_chan` at all" has no behavioral signature an
/// unsupervised flow's own Mova-visible surface can ever expose (nothing
/// reads an allocated-but-unsupervised chan differently from a `None`) --
/// only a white-box counter can tell the two apart.
///
/// Used directly by this file's own `#[cfg(test)] mod tests` (same crate,
/// so `pub(crate)` is enough there). **Not yet wired to `mova::internal`**
/// (`lib.rs`'s `flow_probe` module, which is what makes `mult_spawn_census`
/// additionally reachable from an EXTERNAL `tests/*.rs` integration test):
/// W1's file ownership is `src/builtins/flow.rs` + `src/value.rs` + its own
/// test file ONLY (`docs/L4-LANDING-SPEC.md`'s file-ownership note), and
/// `lib.rs` belongs to nobody in this wave. Add
/// ```ignore
/// pub fn sup_chan_census() -> usize {
///     crate::builtins::flow::sup_chan_census()
/// }
/// ```
/// to `flow_probe` (mirroring `mult_spawn_census`'s own wrapper immediately
/// above it there) if a later wave wants this delta visible from
/// `tests/l4_supervision_test.rs` too.
#[allow(dead_code)] // called from this file's own #[cfg(test)] module only, for now
pub(crate) fn sup_chan_census() -> usize {
    SUP_CHAN_ALLOCATIONS.load(Ordering::Relaxed)
}

static SUP_CHAN_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// A proc's structured exit, L4 W1 (docs/L4-SUPERVISION-DESIGN.md §3.1,
/// docs/L4-LANDING-SPEC.md §W1.1). Every one of [`run_ready`]/
/// [`run_proc_fast`]/[`run_fused`]/[`demote_run`]/[`run_proc`] now returns
/// one instead of `()`; [`ExitGuard`]'s `Drop` is what turns it into the
/// done-cell value and (for a supervised flow) the `sup_chan` event.
///
/// No `Error` variant: nothing in this engine produces an error EXIT today
/// (the incident invariant means a `transform` throw is a report-and-
/// continue, never a proc death -- see the module's D-B citation), so
/// carrying a dead variant nobody can construct would just be surface area
/// with no test able to touch it. Add it the day something actually
/// produces it, not before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitReason {
    /// The control chan was found closed (`chan_take`/`try_take_with_timeout`
    /// returning "nothing left to read, ever") -- the flow is dissolving
    /// around this proc rather than the proc choosing to leave. Reachable in
    /// practice mainly via `stop`'s `STOP_JOIN_TIMEOUT` abandonment path: a
    /// proc that never noticed its `::flow/stop` broadcast in time has its
    /// control chan closed out from under it once `stop` gives up waiting,
    /// and the NEXT time that proc's loop reads control (having finally
    /// returned from whatever it was blocked in), it reads `Normal`.
    Normal,
    /// `::flow/stop` was honored (`apply_control`/`apply_control_fast`/
    /// `apply_control_member` returned `true` for the `"stop"` op, or a
    /// `transform`'s `ProcOutcome::Stop` propagated up). The ordinary,
    /// overwhelmingly common exit.
    Stopped,
    /// A genuine Rust panic unwound through this proc's frame with the
    /// guard's `reason` cell still unset -- see [`ExitGuard`]'s doc for why
    /// "unset at Drop" IS the panic signal rather than a separate flag.
    Panicked,
    /// The supervisor's escalation ladder destroyed this proc where it
    /// stood: `runtime::TaskWaker::kill` force-unwound its task after a
    /// graceful `::flow/stop` went unanswered for `:grace-ms` (L4 W3,
    /// design §3.5).
    ///
    /// Like [`ExitReason::Panicked`] this is an UNWIND, so the guard's
    /// `reason` cell is unset when `Drop` runs and the two have to be told
    /// apart from outside the stack -- see [`ExitGuard::drop`], which asks
    /// `runtime::current_task_is_killed()`. Restart-eligible in [`decide`]
    /// exactly like `Panicked` (a killed proc is a proc the supervisor
    /// wanted gone, not a proc the USER wanted gone; the "do not come back"
    /// case is `stop-proc`'s, and it is carried by the run's STATE, not by
    /// the reason).
    Killed,
}

/// [`ExitReason`]'s plain-keyword rendering -- the inverse the DONE-CELL
/// reader/W2 supervisor will parse back. `plain_kw`, not `kw`: matches
/// `report_error_params`'s `:op`/`:status` values (flow.rs, `plain_kw("step")`/
/// `status_keyword`), which are plain keywords even though the MAP's own
/// keys are `::flow`-namespaced.
fn exit_reason_kw(r: ExitReason) -> Value {
    plain_kw(match r {
        ExitReason::Normal => "normal",
        ExitReason::Stopped => "stopped",
        ExitReason::Panicked => "panicked",
        ExitReason::Killed => "killed",
    })
}

/// The ONE `#::flow{:pid :op :reason :incarnation}` map L4 W1 renders for a
/// proc's exit -- used identically for the done-cell's value
/// ([`ExitGuard::drop`]'s step 3) and for the `:proc-exit` event put onto
/// `sup_chan` (step 2), per docs/L4-LANDING-SPEC.md §W1.4. `:op`/`:reason`
/// are PLAIN keywords, the map's own KEYS are `::flow`-namespaced via
/// [`kw`] -- exactly [`report_error_params`]'s split (flow.rs:2660-2682:
/// `kw("op")`/`kw("status")` as keys, `plain_kw("step")`/`status_keyword`'s
/// plain-keyword values). `incarnation` is 0 for every proc in W1 (plumbed
/// through [`ExitGuard`], never anything else until W2's restart table
/// exists to increment it).
fn render_exit_reason(pid: &Value, reason: ExitReason, incarnation: u64) -> Value {
    let mut m = PMap::new();
    m.insert(kw("pid"), pid.clone());
    m.insert(kw("op"), plain_kw("proc-exit"));
    m.insert(kw("reason"), exit_reason_kw(reason));
    m.insert(kw("incarnation"), Value::Int(incarnation as i64));
    Value::Map(m)
}

/// This run's done-cells PLUS (L4 W1) its structured exit reason and
/// (supervised flows only) the supervision-event carrier -- owned by its
/// spawn closure, rendered and delivered by `Drop`. The done-cell close is
/// what [`stop_flow_cell`] waits for instead of a join, in BOTH worlds --
/// see `ProcRuntime::done` (value.rs) for what a done-cell is and why every
/// proc has one whether it is a task or a thread.
///
/// One cell per MEMBER, not one per run, because `stop` waits per-pid and a
/// fused run's non-head members have no thread handle to key off either
/// way; correspondingly `cells` pairs each member's done-cell with ITS OWN
/// pid (a fused run's ONE `reason` still fans out to every member -- design
/// §3.4, "the run is the death unit").
///
/// **Why a `Drop` guard rather than a call at the bottom of the closure.**
/// The closure body is already the one place guaranteed to be the single
/// exit of both proc loops ([`run_ready`] can tail-call [`run_proc_fast`]
/// and return from there; [`run_fused`] can demote mid-init), so an explicit
/// call there would cover every ORDINARY exit. `Drop` additionally covers
/// the one that isn't ordinary: a genuine Rust panic escaping the loop
/// unwinds through this frame and still signals. That matters more after
/// W1b's flip than it did before, because `stop`'s wait for a task proc has
/// no thread handle whose `is_finished()` would have noticed the panic for
/// it -- without the guard, a panicked proc would cost `stop` a full
/// `STOP_JOIN_TIMEOUT` of dead waiting instead of returning at once.
///
/// **`reason` unset at `Drop` time IS the panic signal.** The spawn
/// closure's last ordinary act is `guard.set(r)` with `run_proc`/
/// `run_fused`'s returned [`ExitReason`]; a Rust panic unwinding through the
/// closure skips straight past that call, so `Drop` finding `reason.get()
/// == None` can only mean the frame is unwinding -- no separate
/// "am I panicking" flag needed (`std::thread::panicking()` would tell us
/// THAT, not WHAT the reason should render as, and this is cheaper and
/// exact).
struct ExitGuard {
    /// (this member's done-cell, this member's pid), in `run` order.
    cells: Vec<(Arc<Chan>, Value)>,
    reason: Cell<Option<ExitReason>>,
    /// `Some` iff this flow has at least one `:policy :restart` proc
    /// anywhere (`native_start` decides once, at spawn time) -- shared by
    /// EVERY run's guard, supervised proc or not, so the (future, W2)
    /// supervisor sees the complete incarnation table design §3.3/D-A axiom
    /// 4 promises ("a flow already enumerates its procs; the supervisor
    /// holds the complete incarnation table"), not just the pids that
    /// individually opted in.
    sup_chan: Option<Arc<Chan>>,
    /// Always 0 in W1 -- plumbed for W2, which increments it per restart.
    incarnation: u64,
}

impl ExitGuard {
    /// Records this run's outcome. Called exactly once, at the very end of
    /// the spawn closure's ordinary body (`let r = run_proc(spawn);
    /// guard.set(r);`) -- see the struct doc for what it means when this is
    /// NEVER called.
    fn set(&self, r: ExitReason) {
        self.reason.set(Some(r));
    }
}

impl Drop for ExitGuard {
    /// Order, per docs/L4-LANDING-SPEC.md §W1.3 (every step below is a
    /// SEPARATE pass over `self.cells`, not fused member-by-member, so nothing
    /// downstream can observe a partial member's worth of side effects; see
    /// the step-2/step-4 notes for exactly why that separation is
    /// load-bearing):
    /// 1. build this run's ONE reason map, per member (same [`ExitReason`],
    ///    different pid);
    /// 2. if `sup_chan` is `Some`, `try_put` every member's event onto it --
    ///    NEVER a blocking put: a panic already unwinding this frame must
    ///    not park, and parking here at all (blocking or not) would be a
    ///    livelock waiting for a supervisor that may itself be gone. Wall W3
    ///    (design doc): a pid has at most ONE in-flight event at a time
    ///    (its next death requires a restart, and a restart is only issued
    ///    after the supervisor has consumed the previous death), and
    ///    `sup_chan` is sized `Fixed(procs.len())`, so `TryPut::WouldBlock`
    ///    ("full") is an invariant violation, not a race -- `debug_assert!`s
    ///    on it. `TryPut::Closed` is a DIFFERENT, expected outcome (W2's
    ///    stop-ordering, design §3.6, closes `sup_chan` before broadcasting
    ///    stop) and is silently dropped, exactly like a `mult`'s "never
    ///    crash broadcast" untap rule -- an event nobody is listening for
    ///    anymore is not a bug;
    /// 3. put each member's reason map into its OWN done cell (`chan_put` on
    ///    a `Fixed(1)` own-cell cannot block: single producer, first and
    ///    only put; see `ProcRuntime::done`'s doc);
    /// 4. close every cell -- LAST, and only after every other observable
    ///    side effect above, so `stop_flow_cell`'s done-cell wait (which is
    ///    watching for exactly this close) can never race ahead of this
    ///    run's `sup_chan` event still being in flight.
    ///
    /// THE F4 LAW (P5a probe finding, 2026-08-28): when W3's kill
    /// force-unwinds a task, every destructor on that stack -- this one
    /// included -- runs with `in_task() == false` (the yielder is already
    /// cleared at the park the task never returns from). A parking chan op
    /// here would therefore take the OS-thread condvar path ON THE SHARD
    /// THREAD and wedge the shard. Every op below is non-parking by
    /// construction (try_put; first-and-only put into an own Fixed(1) cell
    /// with free space; close) and MUST stay that way -- this is a
    /// correctness law, not a style preference.
    fn drop(&mut self) {
        // Unset means SOME unwind skipped `set` on its way through this
        // frame; `runtime::current_task_is_killed()` is what says which one
        // (L4 W3, and the ONE mechanism that writes the `Killed` reason --
        // documented at the runtime end too). It reads the dying task's own
        // state word, which `kill_task` has published along with the rest of
        // that task's identity before it force-unwinds; on an OS thread, and
        // during an ordinary panic on a task, it is `false` and this is
        // exactly W1's behaviour. Non-parking, so the F4 law below covers it.
        let reason = self.reason.get().unwrap_or_else(|| {
            if crate::runtime::current_task_is_killed() {
                ExitReason::Killed
            } else {
                ExitReason::Panicked
            }
        });
        let events: Vec<(&Arc<Chan>, Value)> =
            self.cells.iter().map(|(done, pid)| (done, render_exit_reason(pid, reason, self.incarnation))).collect();
        if let Some(sup) = &self.sup_chan {
            for (_, event) in &events {
                match chan_try_put(sup, event.clone()) {
                    TryPut::Sent | TryPut::Closed => {}
                    TryPut::WouldBlock => {
                        debug_assert!(
                            false,
                            "sup_chan try_put found its Fixed(procs.len()) buffer full -- \
                             violates the at-most-one-in-flight-event-per-pid invariant \
                             (docs/L4-SUPERVISION-DESIGN.md wall W3)"
                        );
                    }
                }
            }
        }
        for (done, event) in &events {
            chan_put(done, event.clone());
        }
        for (done, _) in &events {
            // Idempotent (`chan_close` is a no-op on an already-closed
            // chan), which is what makes double-signalling a non-question.
            chan_close(done);
        }
    }
}

// ---------------------------------------------------------------------------
// L4 W2: run blueprints and the ONE spawn path
// (docs/L4-LANDING-SPEC.md §W2.1, docs/L4-SUPERVISION-DESIGN.md §3.4)
// ---------------------------------------------------------------------------

/// One member of a run, as everything an incarnation of it needs MINUS the
/// things an incarnation owns. Every field here outlives the proc body that
/// borrows it: the chans especially, which are the flow's addressing fabric
/// ("chans are the identity; the proc body is disposable", design §3.4) --
/// `control` keeps `ping`/`pause-proc`/`stop` reaching the pid across a
/// restart, and `ins`/`outs` keep in-flight messages queued in buffers across
/// the death window, with parked PEER putters/takers on them untouched (they
/// are parked on the chan, not on the proc).
struct MemberBlueprint {
    pid: Value,
    step_fn: Value,
    args: Value,
    ins: HashMap<Value, Arc<Chan>>,
    outs: HashMap<Value, Option<Arc<Chan>>>,
    /// The two transport-lane halves, `Some` on the FIRST incarnation only
    /// and `take`n by [`spawn_run`] -- a `SpscRing` half is owned by value by
    /// the proc body and dies with it, which is why a restart-supervised proc
    /// is never given one in the first place (`native_start`'s `link_plan`
    /// filter). The `take` + the `debug_assert!` in `spawn_run` are what turn
    /// that "never" from a comment into a checked invariant.
    in_lane: Option<InLane>,
    out_lane: Option<OutLane>,
    control: Arc<Chan>,
}

/// One RUN's complete respawn recipe -- the restart unit, because a fused run
/// lives and dies together and per-member restart inside a fused task is
/// fiction (design §3.4). Held by `start` for the initial spawn and then
/// MOVED into the supervisor task, which is the only thing that ever spawns
/// another incarnation.
///
/// **Why the supervisor owns these rather than `FlowRuntime`** (a deliberate
/// deviation from the landing spec's "retained in `FlowRuntime`" phrasing,
/// following design §3.3's "the supervisor owns the blueprint table"): a
/// `FlowRuntime` lives inside `Arc<FlowCell>` and is therefore shared across
/// threads, which would require every field here -- including a whole
/// `Interp` -- to be `Sync`. Nothing but the supervisor ever reads a
/// blueprint, so moving the table into the task that uses it keeps the
/// requirement at `Send` and keeps the shared flow state exactly as small as
/// it was.
struct RunBlueprint {
    /// The prototype every incarnation's members fork from -- see the
    /// construction site in `native_start` for why one fork per run is
    /// field-for-field what the pre-W2 one-fork-per-member was.
    interp: Interp,
    members: Vec<MemberBlueprint>,
    error_chan: Arc<Chan>,
    sup_chan: Option<Arc<Chan>>,
    is_task: bool,
    shard: Option<usize>,
}

/// What one incarnation of a run hands back: the per-member done-cells (in
/// `members` order) plus the run's ONE handle -- an OS thread's, or (L4 W3)
/// a runtime task's. Exactly one of the two is `Some`, decided by
/// `RunBlueprint::is_task`.
struct SpawnedRun {
    dones: Vec<Arc<Chan>>,
    /// A thread run's head pid's join handle (`None` for a task run).
    thread: Option<std::thread::JoinHandle<()>>,
    /// **L4 W3: the kill handle.** `Some` for a task run, `None` for a
    /// thread run -- OS threads cannot be force-unwound, which is wall W6
    /// and owner ruling #4 in one field. ONE per RUN, not per member,
    /// because a run IS one task: a fused run's members share a stack, and
    /// per-member kill inside a fused task is fiction for the same reason
    /// per-member restart is (design §3.4, "the run is the death unit").
    ///
    /// Retained by the supervisor for the current incarnation only
    /// (`Supervisor::live_task`); a restart replaces it, and the previous
    /// incarnation's handle is dropped because a dead task is not a kill
    /// target.
    task: Option<crate::runtime::TaskHandle>,
}

/// Spawns incarnation `incarnation` of one run -- the ONE place a proc body
/// is ever launched, shared verbatim by `flow/start` and by the supervisor's
/// restart action, so the two can never drift (docs/L4-LANDING-SPEC.md
/// §W2.1). Everything created here is per-incarnation and nothing else is
/// touched: fresh done-cells, a fresh [`ExitGuard`] stamped with this
/// incarnation, a fresh `Interp` fork and `ProcSpawn` per member.
///
/// The `&mut` is for the lane `take` only (see [`MemberBlueprint::in_lane`]);
/// every other field is cloned, which is what makes a second call legal at
/// all.
fn spawn_run(bp: &mut RunBlueprint, incarnation: u64) -> Result<SpawnedRun, RjError> {
    debug_assert!(
        incarnation == 0 || bp.members.iter().all(|m| m.in_lane.is_none() && m.out_lane.is_none()),
        "a restarted run must carry no transport lanes -- a `SpscRing` half dies with the \
         incarnation that owned it, which is why `native_start` never plans a lane for a \
         restart-supervised proc (design §3.4)"
    );
    let mut spawns: Vec<ProcSpawn> = Vec::with_capacity(bp.members.len());
    // One done-cell per MEMBER of this run (see `ProcRuntime::done` and
    // [`ExitGuard`]). `Fixed(1)`: since L4 W1 the cell carries exactly one
    // value (this run's rendered exit reason) before it closes, so 1 is the
    // exact capacity rather than the arbitrary one it used to be. NOT pushed
    // to `engine_owned_chans`: `stop` must never close it, or the wait it is
    // about to do would observe its OWN close as the proc's exit signal.
    let mut dones: Vec<Arc<Chan>> = Vec::with_capacity(bp.members.len());
    for m in bp.members.iter_mut() {
        dones.push(Arc::new(Chan::new(BufferPolicy::Fixed(1))));
        spawns.push(ProcSpawn {
            interp: bp.interp.fork(),
            pid: m.pid.clone(),
            step_fn: m.step_fn.clone(),
            args: m.args.clone(),
            ins: m.ins.clone(),
            outs: m.outs.clone(),
            in_lane: m.in_lane.take(),
            out_lane: m.out_lane.take(),
            control: m.control.clone(),
            error_chan: bp.error_chan.clone(),
        });
    }
    // The done-cells, in BOTH worlds (see [`ExitGuard`]): the guard is moved
    // into the closure and, when the closure's frame goes away (whichever way
    // it goes away), renders this run's one `ExitReason` into every one of its
    // members' done cells and (supervised flows only) `sup_chan`. `cells`
    // pairs each member's done-cell with its own pid, in `members` order --
    // see `ExitGuard::cells`'s doc.
    let cells: Vec<(Arc<Chan>, Value)> =
        dones.iter().cloned().zip(bp.members.iter().map(|m| m.pid.clone())).collect();
    let exit_guard = ExitGuard { cells, reason: Cell::new(None), sup_chan: bp.sup_chan.clone(), incarnation };
    let mut task: Option<crate::runtime::TaskHandle> = None;
    let thread = if spawns.len() == 1 {
        let spawn = spawns.pop().expect("length checked");
        let body = move || {
            let guard = exit_guard;
            let r = run_proc(spawn);
            guard.set(r);
        };
        if bp.is_task {
            // L4 W3: `spawn_killable` is `spawn`/`spawn_on` plus the handle
            // the supervisor's escalation ladder needs -- same placement
            // rule (`Some(shard)` => explicit, `None` => the runtime's own),
            // one `Arc` bump more.
            task = Some(crate::runtime::spawn_killable(bp.shard, body));
            None
        } else {
            let thread_name = format!("flow-{}", crate::printer::display_str(&bp.members[0].pid));
            Some(
                std::thread::Builder::new()
                    .name(thread_name)
                    .stack_size(PROC_STACK_SIZE)
                    .spawn(crate::memstat::drained(body))
                    .map_err(|e| RjError::other(format!("flow/start: couldn't spawn proc thread: {e}")))?,
            )
        }
    } else {
        let spawn = FusedSpawn { members: spawns, error_chan: bp.error_chan.clone() };
        let body = move || {
            let guard = exit_guard;
            let r = run_fused(spawn);
            guard.set(r);
        };
        if bp.is_task {
            task = Some(crate::runtime::spawn_killable(bp.shard, body));
            None
        } else {
            let thread_name = format!(
                "flow-fused-{}..{}",
                crate::printer::display_str(&bp.members[0].pid),
                crate::printer::display_str(&bp.members.last().expect("a run is non-empty").pid)
            );
            Some(
                std::thread::Builder::new()
                    .name(thread_name)
                    .stack_size(PROC_STACK_SIZE)
                    .spawn(crate::memstat::drained(body))
                    .map_err(|e| RjError::other(format!("flow/start: couldn't spawn fused proc thread: {e}")))?,
            )
        }
    };
    Ok(SpawnedRun { dones, thread, task })
}

// ===========================================================================
// L4 W2/W3 SUPERVISOR / POLICY REGION -- BEGIN
//
// docs/L4-SUPERVISION-DESIGN.md §3.3/§3.4/§3.5/§3.6,
// docs/L4-LANDING-SPEC.md §W2.2-§W2.5 and §W3.3. Everything between this banner and its END counterpart is
// "supervisor/policy code" in the sense gate G-DET means it:
//
//   * [`sup_now`] is the ONLY clock read in the region. Nothing else here
//     may read the OS clock directly (bare `Instant`/`SystemTime` reads) --
//     the grep is the gate.
//   * [`decide`] is PURE: it takes `now`, the run's history and the config
//     as arguments, returns a value, and touches nothing else. No clock, no
//     randomness, no side effects, no locks.
//   * every wait is either a take from `sup_chan` or a combined
//     {sup_chan-ring OR timer-service deadline} park ([`sup_park`]). The
//     supervisor never sleeps, never polls a clock in a loop, and never
//     parks on anything the L5 simulator would not already control.
// ===========================================================================

/// **THE L5 SEAM -- now LIVE.** Every supervision time read in this file goes
/// through this one function -- backoff deadlines, restart-window
/// accounting, the `:io` confirmation budget, the park's remaining-duration
/// math. `sup_now` delegates to `crate::clock::clock_now()` (design §2): real
/// mode reads the OS clock unchanged, sim mode returns `SIM_ANCHOR +
/// SIM_NOW_NS`. Virtual time enters HERE and nowhere else (D-A axiom 1: "zero
/// bare OS-clock reads in policy logic"), which is only true because
/// [`decide`] takes `now` as an argument instead of reading a clock, and
/// because the supervisor's only other time-dependent act -- parking until a
/// deadline -- goes through the shared timer service, which arms deadlines
/// off the SAME `clock_now()` (L5 W1's sweep; P6a rule 5 showed a mismatched
/// clock here is a deterministic hang).
///
/// Gate G-DET greps this region for bare OS-clock reads and must find
/// exactly one hit: the `Instant`-typed return below. What that hit means has
/// changed -- "exactly one raw OS-clock read in the region" is now "exactly
/// one clock read in the region, and it is `clock_now`".
fn sup_now() -> Instant {
    crate::clock::clock_now()
}

/// How long the supervisor waits between confirmation looks at a dying `:io`
/// run's done-cells (wall W6, see [`Supervisor::act_restart`]). Bounded by
/// the cfg's `:grace-ms` in total, and every tick is a timer-service park,
/// never a sleep.
const IO_CONFIRM_TICK: Duration = Duration::from_millis(5);

/// How many times the escalation ladder re-attempts a kill before calling
/// the run WEDGED (L4 W3, design §3.5's F5: "a single kill attempt fails
/// ~50% of the time against a task that is mid-hop"). One attempt every
/// [`IO_CONFIRM_TICK`], so this is ~160ms of trying.
///
/// The number is not a timing guess, it is a shape argument. A kill claims
/// only a task that is PARKED, and a healthy proc parks on every message it
/// waits for -- microseconds apart -- so a handful of attempts is already
/// overwhelming (P5a measured 50 739/100 000 clean claims against a peer
/// deliberately racing every single round). What the budget is really for is
/// the case that CANNOT be retried into success: a task spinning in user
/// code, or blocked inside a blocking native, which is unreachable by
/// construction and must be NAMED (`:proc-wedged`) rather than retried
/// forever. 32 is comfortably past "the task is just busy" and comfortably
/// short of "the supervisor has stopped supervising".
const KILL_RETRY_BUDGET: u32 = 32;

/// One `:proc-exit` event, parsed off `sup_chan` back into Rust. The wire
/// form is [`render_exit_reason`]'s map; this is its inverse, and the ONLY
/// thing the supervisor's policy layer ever sees of a death.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ExitEvent {
    pid: Value,
    reason: ExitReason,
    incarnation: u64,
}

/// [`exit_reason_kw`]'s inverse. `None` for anything that is not one of the
/// four known reason keywords -- the supervisor treats an unparseable event
/// as "not mine" rather than guessing (there is exactly one producer today,
/// so this is a defensive arm, not a compatibility one).
fn exit_reason_from_kw(v: &Value) -> Option<ExitReason> {
    let Value::Keyword(k) = v else { return None };
    match k.as_ref() {
        "normal" => Some(ExitReason::Normal),
        "stopped" => Some(ExitReason::Stopped),
        "panicked" => Some(ExitReason::Panicked),
        "killed" => Some(ExitReason::Killed),
        _ => None,
    }
}

/// Everything that can arrive on `sup_chan`. Two producers, two shapes:
/// `ExitGuard::drop` puts a death (W1), and `flow/stop-proc` puts a request
/// (L4 W3). Both are `#::flow{...}` maps keyed by `:op`, so the supervisor's
/// parser is one `match` and an unknown `:op` is a `None` rather than a
/// guess.
#[derive(Clone, Debug, PartialEq)]
enum SupMsg {
    /// A proc died. Mirrored to `report-chan` verbatim, then decided.
    Exit(ExitEvent),
    /// The user asked this pid to stop, and the graceful `::flow/stop` is
    /// ALREADY on its control chan (the native does that part itself, so a
    /// stop-proc against an unsupervised proc needs no supervisor at all).
    /// What the supervisor owes it is the grace window and the escalation.
    StopRequest(Value),
}

/// The `#::flow{:op :stop-proc :pid p}` message `flow/stop-proc` puts on
/// `sup_chan`. Deliberately the same `::flow`-keys/plain-keyword-values
/// split as every other event this file renders.
fn render_stop_request(pid: &Value) -> Value {
    let mut m = PMap::new();
    m.insert(kw("pid"), pid.clone());
    m.insert(kw("op"), plain_kw("stop-proc"));
    Value::Map(m)
}

/// Parses one `sup_chan` message, or `None` if it is not a map this file
/// produced.
fn parse_sup_msg(v: &Value) -> Option<SupMsg> {
    let Value::Map(m) = v else { return None };
    let Some(Value::Keyword(op)) = m.get(&kw("op")) else { return None };
    match op.as_ref() {
        "proc-exit" => {
            let pid = m.get(&kw("pid"))?.clone();
            let reason = exit_reason_from_kw(m.get(&kw("reason"))?)?;
            let Some(Value::Int(inc)) = m.get(&kw("incarnation")) else { return None };
            Some(SupMsg::Exit(ExitEvent { pid, reason, incarnation: (*inc).max(0) as u64 }))
        }
        "stop-proc" => Some(SupMsg::StopRequest(m.get(&kw("pid"))?.clone())),
        _ => None,
    }
}

/// What the supervisor knows about ONE run, and the whole of what [`decide`]
/// is allowed to read about it. Deliberately a plain value type with no
/// handles in it: that is what makes `decide` unit-testable against synthetic
/// histories (docs/L4-LANDING-SPEC.md §W2.3).
#[derive(Clone, Debug, PartialEq)]
struct RunRecord {
    /// The incarnation currently supposed to be alive. Incremented by the
    /// restart action, never by anything else -- the ABA discipline the slab
    /// taught us, one level up (wall W5).
    incarnation: u64,
    state: RunState,
    /// The incarnation whose death has ALREADY been decided. A fused run's
    /// death emits k events (one per member, same reason, same incarnation --
    /// the run is the death unit), so the first one decides and the other
    /// k-1 are duplicates. `None` means "this run's current incarnation has
    /// not died yet", which is also what a restart resets it to.
    decided: Option<u64>,
    /// When each restart of this run happened, oldest first -- the
    /// `:window-ms` accounting, pruned lazily by [`decide`]'s reader (which
    /// cannot mutate) and eagerly by the restart action.
    restarts: Vec<Instant>,
    /// Consecutive crashes, i.e. the backoff exponent `k`. Reset when the
    /// restart window goes empty (a full quiet window is what "recovered"
    /// means here), never otherwise.
    consecutive: u32,
}

/// A run's supervision state machine (design §3.3's `Running|Backoff|
/// GivenUp`, with the backoff arm carrying its own schedule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunState {
    /// Alive, or at least not known to be dead.
    Running,
    /// Dead, and a restart is scheduled.
    Backoff {
        /// When to respawn. The supervisor parks until here.
        at: Instant,
        /// What was scheduled, for the `:proc-restart` event's `:delay-ms`.
        delay_ms: u64,
        /// When a `:io` run whose exit is still unconfirmed stops being
        /// "about to finish" and becomes a wedge (`at + :grace-ms`).
        wedge_at: Instant,
    },
    /// **L4 W3: a `flow/stop-proc` is in flight against this run.** The
    /// graceful `::flow/stop` has already been delivered (by the native,
    /// before it ever told the supervisor); this state is the GRACE WINDOW,
    /// and what happens at its end is the escalation ladder's second rung.
    ///
    /// Entered only for a run with a resolved `SupervisionCfg` -- an
    /// unsupervised proc's `stop-proc` is graceful-only, which needs no
    /// state at all. Left in exactly two ways: the run's exit event arrives
    /// (`:proc-stopped`, then [`RunState::GivenUp`] -- a stop-proc'd run
    /// does NOT come back, it is the user's intent and not a fault), or the
    /// ladder gives up on killing it (`:proc-wedged`, also `GivenUp`).
    Stopping {
        /// When to act. First set to `now + :grace-ms` (the graceful
        /// window), then to `now + IO_CONFIRM_TICK` for each kill retry.
        deadline: Instant,
        /// Kill attempts spent, against [`KILL_RETRY_BUDGET`].
        attempts: u32,
    },
    /// No further restarts: the window is exhausted, the flow is stopping,
    /// the respawn itself failed, an `:io` run never confirmed its exit, or
    /// (L4 W3) a `stop-proc` was honored.
    GivenUp,
}

/// [`decide`]'s answer. Pure data -- the supervisor's ACTION layer is what
/// turns one of these into a spawn, an event, or nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Decision {
    Ignore(IgnoreCause),
    ScheduleRestart { at: Instant, delay_ms: u64 },
    GiveUp { restarts: u32 },
}

/// WHY an event was ignored. Carried rather than collapsed to a bare
/// `Ignore` so the unit matrix can assert the REASON (a rule that fires for
/// the wrong reason is a rule that will stop firing when the other one
/// changes), and so a future `:proc-ignored` diagnostic has something to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IgnoreCause {
    /// `:normal`/`:stopped`: an orderly exit is never restarted. Restarting a
    /// deliberate stop would fight the user and fight `stop` itself.
    OrderlyExit,
    /// This pid has no `:policy :restart` config. Every proc's `ExitGuard`
    /// ships its death to `sup_chan` (design §3.2/D-A axiom 4: the supervisor
    /// sees the WHOLE flow), so this arm is the common one in a flow where
    /// only some procs opted in.
    Unsupervised,
    /// A later member of the same fused run reporting the same death.
    Duplicate,
    /// An older incarnation dying slowly (wall W5). Restarting on it would
    /// kill nothing and spawn a second live incarnation of the same run.
    StaleIncarnation,
    /// The flow is stopping (or never started): stop's waits own the endgame
    /// (wall W4). Re-checked at ACTION time too, under the flow lock.
    NotRunning,
    /// The run is already given up on -- max restarts exhausted, or wedged.
    AlreadyGivenUp,
    /// **L4 W3:** the USER asked this run to stop (`flow/stop-proc`), so its
    /// death -- however it died, `:stopped` after the graceful window or
    /// `:killed` after the escalation -- is the fulfilment of that request
    /// and never a fault to recover from. The action layer turns this one
    /// into the `:proc-stopped` event and a terminal `GivenUp`.
    StopRequested,
}

/// The exponential backoff, `initial * factor^k` clamped to `max` (design
/// §3.4). Pure, total, and saturating: `k` is capped before the `powi` so an
/// absurd crash count cannot produce `inf`, and a non-finite or absurd
/// product clamps to `max_ms` rather than wrapping through the `as u64` cast.
fn backoff_delay_ms(b: &BackoffCfg, k: u32) -> u64 {
    // `factor` is validated non-negative at create-flow; 0.0 is legal and
    // means "no growth after the first" (0^0 == 1, so k = 0 still yields
    // `initial_ms`), which is the least surprising reading of a factor of
    // zero and needs no special case.
    let scaled = (b.initial_ms as f64) * b.factor.powi(k.min(64) as i32);
    if !scaled.is_finite() || scaled >= b.max_ms as f64 {
        b.max_ms
    } else {
        scaled as u64
    }
}

/// **The policy, and the whole of it: a pure function of (event, config,
/// history, phase, now).** No clock, no locks, no side effects, no
/// randomness -- D-A axiom 2 ("supervision is a deterministic event-driven
/// state machine") in one signature, and the reason the entire restart
/// timeline can be unit-tested against synthetic streams with a synthetic
/// `now`.
///
/// The rule order below is load-bearing, cheapest-and-most-final first:
/// 1. **Duplicate** -- the first event of a (run, incarnation) decides;
///    a fused run's other k-1 members are echoes of that same death.
/// 2. **Stale incarnation** -- an old incarnation dying after its
///    replacement is already running (wall W5).
/// 3. **Phase** -- a flow that is stopping never restarts (wall W4).
/// 4. **Unsupervised** -- no cfg, nothing to do; W1 ships every pid's death
///    here by design.
/// 5. **Orderly exit** -- `:normal`/`:stopped` are not faults.
/// 6. **Already given up** -- no second life after the window is spent.
///    6b. **Stop requested** (L4 W3) -- see the rule's own comment for why
///    it sits above 5 rather than here.
/// 7. **Window** -- more than `max_restarts` restarts inside `window_ms`
///    gives up; `max_restarts` is the number of restarts ALLOWED in a
///    window, so crash number `max_restarts + 1` is the one that gives up
///    (and `:max-restarts 0` gives up on the first crash).
/// 8. Otherwise: restart after `initial * factor^consecutive`, capped.
fn decide(
    ev: &ExitEvent,
    cfg: Option<&SupervisionCfg>,
    rec: &RunRecord,
    phase: FlowPhase,
    now: Instant,
) -> Decision {
    if rec.decided == Some(ev.incarnation) {
        return Decision::Ignore(IgnoreCause::Duplicate);
    }
    if ev.incarnation != rec.incarnation {
        return Decision::Ignore(IgnoreCause::StaleIncarnation);
    }
    if phase != FlowPhase::Running {
        return Decision::Ignore(IgnoreCause::NotRunning);
    }
    // L4 W3, and it must come BEFORE the reason rules: a `stop-proc` that
    // was honored gracefully dies `:stopped`, which rule 5 would ignore for
    // the wrong reason and thereby swallow the `:proc-stopped` event; one
    // that had to be escalated dies `:killed`, which rule 8 would RESTART.
    // A run only reaches `Stopping` with a cfg in hand, so this rule sitting
    // above the `cfg`-is-`None` check costs nothing.
    if matches!(rec.state, RunState::Stopping { .. }) {
        return Decision::Ignore(IgnoreCause::StopRequested);
    }
    let Some(cfg) = cfg else { return Decision::Ignore(IgnoreCause::Unsupervised) };
    match ev.reason {
        ExitReason::Normal | ExitReason::Stopped => return Decision::Ignore(IgnoreCause::OrderlyExit),
        ExitReason::Panicked | ExitReason::Killed => {}
    }
    if rec.state == RunState::GivenUp {
        return Decision::Ignore(IgnoreCause::AlreadyGivenUp);
    }
    let window = Duration::from_millis(cfg.window_ms);
    let in_window =
        rec.restarts.iter().filter(|t| now.saturating_duration_since(**t) < window).count() as u32;
    if in_window >= cfg.max_restarts {
        return Decision::GiveUp { restarts: in_window };
    }
    let delay_ms = backoff_delay_ms(&cfg.backoff, rec.consecutive);
    Decision::ScheduleRestart { at: now + Duration::from_millis(delay_ms), delay_ms }
}

/// The supervisor task's own state: one per supervised flow, created by
/// [`spawn_supervisor`] and owned entirely by the task (nothing else has a
/// handle on it).
struct Supervisor {
    flow: Arc<FlowCell>,
    /// The death-event stream. Closed by `stop_flow_cell`'s step 1b, which
    /// is this task's ONLY exit condition.
    sup: Arc<Chan>,
    /// Lifecycle events out. `Sliding(DIAG_BUF)`, so every put here is
    /// non-blocking by policy -- see [`Supervisor::report`].
    report_chan: Arc<Chan>,
    /// For the one failure this task can suffer that is nobody's policy
    /// decision: a respawn that the OS refuses (see [`Supervisor::act_restart`]).
    error_chan: Arc<Chan>,
    /// Closed as the last act of this task -- `stop` waits on it.
    sup_done: Arc<Chan>,
    /// Run id -> recipe. Indices into this ARE the run ids, everywhere.
    runs: Vec<RunBlueprint>,
    /// Run id -> live supervision state. Same length, same indices.
    table: Vec<RunRecord>,
    /// Run id -> the CURRENT incarnation's kill handle, `None` for a thread
    /// (`:io`) run (L4 W3, [`SpawnedRun::task`]). Same length, same indices.
    /// Replaced wholesale by every restart, which is what keeps the
    /// escalation ladder from ever aiming at a previous incarnation.
    live_task: Vec<Option<crate::runtime::TaskHandle>>,
    /// pid -> run id. Built once; a flow's proc set is immutable.
    run_of: HashMap<Value, usize>,
    /// Run id -> the RUN's supervision config: the first member's, in run
    /// order, that has one. `None` when no member of the run opted in (see
    /// [`IgnoreCause::Unsupervised`]).
    ///
    /// **Per RUN, not per pid, and that is a semantic choice.** The restart
    /// unit is the run (design §3.4), so the policy must be too: a fused
    /// run's death emits one event per member, and if each member's own cfg
    /// answered the question, WHICH member's event arrived first would decide
    /// whether the run restarts -- a race, and a nasty one, since the losing
    /// members' events are then swallowed as duplicates of a decision made on
    /// the wrong config. The consequence, stated plainly: a `:policy :none`
    /// proc FUSED with a `:restart` one is restarted along with it. That is
    /// inherent to fusion (they are one task, one stack, one death) rather
    /// than a policy this file invents, and a proc that must never be
    /// restarted can always be kept out of a fused run by the same means
    /// anything else is (`:workload :io`, or a topology that does not chain).
    cfg_of_run: Vec<Option<SupervisionCfg>>,
}

/// Builds the supervisor and starts it as a plain runtime TASK (design §3.3:
/// "a plain runtime task, not a thread -- the flow world stays thread-free",
/// and the L5 demand check that justifies it: a task parked on {chan OR
/// timer} needs nothing changed under virtual time).
///
/// Placement is the runtime's (`spawn`, not `spawn_on`): the supervisor is
/// not a member of any run, so `segment_shards`' pipeline reasoning has
/// nothing to say about it -- the same argument [`run_mult_task`]'s spawn
/// site makes for a mult.
fn spawn_supervisor(
    flow: &Arc<FlowCell>,
    sup: Arc<Chan>,
    sup_done: Arc<Chan>,
    report_chan: Arc<Chan>,
    error_chan: Arc<Chan>,
    runs: Vec<RunBlueprint>,
    live_task: Vec<Option<crate::runtime::TaskHandle>>,
) {
    debug_assert_eq!(live_task.len(), runs.len(), "one kill-handle slot per run, in run order");
    let mut run_of: HashMap<Value, usize> = HashMap::new();
    let mut cfg_of_run: Vec<Option<SupervisionCfg>> = Vec::with_capacity(runs.len());
    for (idx, bp) in runs.iter().enumerate() {
        for m in &bp.members {
            run_of.insert(m.pid.clone(), idx);
        }
        cfg_of_run
            .push(bp.members.iter().find_map(|m| flow.def.procs.get(&m.pid).and_then(|p| p.supervision.clone())));
    }
    let table = vec![
        RunRecord { incarnation: 0, state: RunState::Running, decided: None, restarts: Vec::new(), consecutive: 0 };
        runs.len()
    ];
    let supervisor = Supervisor {
        flow: flow.clone(),
        sup,
        report_chan,
        error_chan,
        sup_done,
        runs,
        table,
        live_task,
        run_of,
        cfg_of_run,
    };
    SUPERVISOR_SPAWNS.fetch_add(1, Ordering::Relaxed);
    crate::runtime::spawn(move || run_supervisor(supervisor));
}

/// The supervisor's whole life (design §3.3's loop, realized):
///
/// ```text
/// loop:
///   snapshot the doorbell generation           ; missed-wakeup law
///   non-blocking take from sup_chan
///     -> event  : decide + record, loop
///     -> closed : stop is in progress; exit WITHOUT restarting
///     -> empty  : fall through
///   fire every restart whose deadline has passed; if any fired, loop
///   park on {sup_chan rings OR the earliest scheduled restart}
/// close the supervisor's own done-cell
/// ```
///
/// **Why a `Doorbell` rather than a blocking [`chan_take`].** The park has to
/// be a COMBINED one -- "an event arrived" OR "a backoff expired" -- and a
/// blocking take can only wait for the first. Registering this task's own
/// doorbell in `sup_chan`'s `ChanState::doorbell` slot makes every put and
/// the close ring it (`builtins::async`'s put/close sites do that whenever a
/// doorbell is registered), which is exactly the wake source a proc's read
/// loop uses, and leaves the deadline half free for the timer service. The
/// supervisor is `sup_chan`'s only reader, so claiming that single slot
/// deprives nobody.
///
/// **Draining is automatic.** A closed chan still delivers what was already
/// buffered, so the `Closed` arm is only reached once the last event has been
/// taken and decided (and each of those decisions is an `Ignore` by then --
/// `decide`'s phase rule, since step 1b flips the phase before it closes the
/// chan). "Drain the table, exit" and "exit without restarting" are therefore
/// the same code path, not two.
fn run_supervisor(mut s: Supervisor) {
    // Created HERE, on the task that will park on it -- `Doorbell::new`'s
    // documented requirement (it captures the current thread for its
    // additional `unpark`; inside a task that is the shard thread, which is
    // merely uncorrelated, never wrong -- see `Doorbell::owner_thread`).
    let doorbell = Arc::new(Doorbell::new());
    lock_mutex(&s.sup.state).doorbell = Some(doorbell.clone());
    loop {
        // THE lap snapshot, before the non-blocking scan below -- see
        // `value.rs`'s `Doorbell` doc, "missed-wakeup correctness". An event
        // landing between here and the park makes the park return at once
        // instead of sleeping through it.
        let seen = doorbell.current();
        match chan_try_take(&s.sup) {
            TryTake::Received(ev) => {
                s.on_event(ev);
                continue;
            }
            TryTake::Closed => break,
            TryTake::WouldBlock => {}
        }
        if s.fire_due(sup_now()) {
            continue;
        }
        let next = s.next_deadline();
        sup_park(&doorbell, seen, next);
    }
    // Unregister before the last close: a registration left on a chan that
    // outlives this task would keep the doorbell alive and fire wakes at a
    // task that is gone (harmless by the runtime's stale-wake contract, but
    // `resync_read_set_doorbell` makes the same tidiness argument and this
    // costs one uncontended lock, once per flow).
    lock_mutex(&s.sup.state).doorbell = None;
    chan_close(&s.sup_done);
}

/// The supervisor's ONE wait, in its two shapes (D-A: "parks only on {chan
/// OR timer-service deadline}").
///
/// - **No pending restart** -> park on the doorbell alone. In a task that is
///   an unbounded park woken by `sup_chan`'s next put or its close; the
///   `PARK_TIMEOUT` argument is the thread arm's safety net and is ignored on
///   the task arm by design (`Doorbell::wait_for_change`).
/// - **A restart is scheduled** -> arm ONE timer entry against this task's
///   own waker and park on the doorbell as usual. Either source resumes the
///   task; the caller re-checks both conditions from scratch, so it never
///   has to know which one fired. The token is cancelled on the way out so a
///   spent entry does not chase this task to its next park (lazy
///   cancellation, `timer_arm_waker`'s documented trade).
///
/// **The armed deadline is strictly at-or-after the scheduled one.** The ms
/// budget is `ceil`ed by adding 1 to the truncating `as_millis()`, so the
/// timer's own `clock_now() + ms` can never land BEFORE `deadline` and a
/// re-park past a spent deadline is impossible -- the hazard
/// `timer_arm_waker`'s doc names ("the returned `Instant` is the contract").
/// The caller then tests against the scheduled `at` itself, which is the
/// instant the policy actually chose.
fn sup_park(doorbell: &Doorbell, seen: u64, until: Option<Instant>) {
    let Some(deadline) = until else {
        doorbell.wait_for_change(seen, PARK_TIMEOUT);
        return;
    };
    let remaining = deadline.saturating_duration_since(sup_now());
    if !crate::runtime::in_task() {
        // Unreachable as long as `spawn_supervisor` uses `runtime::spawn`
        // (it does, in BOTH worlds -- the kill switch moves procs back to
        // threads, never the supervisor). Kept honest rather than
        // `unreachable!`: on an OS thread the doorbell's own condvar arm IS
        // the bounded park, so the correct code is one line and needs no
        // second mechanism.
        doorbell.wait_for_change(seen, remaining);
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let ms = (remaining.as_millis() as u64).saturating_add(1);
    let _armed = timer_arm_waker(crate::runtime::current_waker(), ms, cancel.clone());
    doorbell.wait_for_change(seen, PARK_TIMEOUT);
    cancel.store(true, Ordering::Relaxed);
}

impl Supervisor {
    /// One lifecycle event onto `report_chan` -- ALWAYS non-blocking. The
    /// chan is `Sliding(DIAG_BUF)`, so a `try_put` can only ever answer
    /// `Sent` or `Closed`; a supervisor that could block on a diagnostic
    /// stream nobody is draining would be a supervisor that stops
    /// supervising.
    fn report(&self, event: Value) {
        let _ = chan_try_put(&self.report_chan, event);
    }

    /// One message off `sup_chan`, dispatched by shape.
    ///
    /// A DEATH is mirrored to `report_chan` FIRST and unconditionally -- even
    /// for a pid this supervisor will ignore -- because it is the user's
    /// whole view of proc lifetimes (design §3.3: report-chan's first
    /// producer ever), and "which deaths were ignored, and there were some"
    /// is exactly the kind of thing an operator needs to see. It is the
    /// event VERBATIM, so `:pid`/`:reason`/`:incarnation` on report-chan mean
    /// exactly what they mean on `sup_chan`.
    ///
    /// A stop REQUEST is not mirrored: it is an instruction, not a fact about
    /// the flow, and the fact it produces (`:proc-stopped`) is reported when
    /// the run actually dies. Mirroring both would put two events on
    /// report-chan for one user action and make the stream harder to read,
    /// not easier.
    fn on_event(&mut self, event: Value) {
        match parse_sup_msg(&event) {
            Some(SupMsg::Exit(ev)) => {
                self.report(event);
                self.on_exit(ev);
            }
            Some(SupMsg::StopRequest(pid)) => self.on_stop_request(pid),
            None => {
                debug_assert!(false, "sup_chan carried a message this file does not produce: {event:?}");
            }
        }
    }

    /// **L4 W3: the escalation ladder's first rung, recorded.**
    ///
    /// The graceful `::flow/stop` is already on the run's control chan --
    /// `native_stop_proc` puts it there BEFORE it tells the supervisor
    /// anything, so a proc that honors control promptly is usually already
    /// dying by the time this runs, and an UNSUPERVISED proc gets that
    /// graceful stop and nothing else (there is no supervisor to escalate,
    /// and none is needed). All this owes the request is the grace window and
    /// what happens at its end.
    ///
    /// The four states a request can find the run in, and why each answer is
    /// the only sensible one:
    /// - `Running` -> open the grace window ([`RunState::Stopping`]).
    /// - `Backoff` -> the run is ALREADY dead with a restart pending. Grant
    ///   the request by cancelling that restart and reporting `:proc-stopped`
    ///   right now: no further exit event will ever arrive for it (the death
    ///   that scheduled the backoff was it), so waiting for one would leave
    ///   the request silently unfulfilled.
    /// - `Stopping` -> idempotent, and deliberately so: `stop-proc` twice is
    ///   not two grace windows. (The de-dup in `native_stop_proc` means this
    ///   arm is normally unreachable; it is the belt to that braces.)
    /// - `GivenUp` -> nothing to stop.
    fn on_stop_request(&mut self, pid: Value) {
        let Some(&run) = self.run_of.get(&pid) else {
            debug_assert!(false, "a :stop-proc request for a pid this flow never spawned: {pid:?}");
            return;
        };
        // No cfg == the pid never opted into supervision: graceful-only,
        // exactly as the design's `:policy :none` promises. The native has
        // already done that part.
        let Some(cfg) = self.cfg_of_run[run].clone() else { return };
        match self.table[run].state {
            RunState::Running => {
                let deadline = sup_now() + Duration::from_millis(cfg.grace_ms);
                self.table[run].state = RunState::Stopping { deadline, attempts: 0 };
            }
            RunState::Backoff { .. } => {
                self.table[run].state = RunState::GivenUp;
                let pids = self.run_pids(run);
                self.report(render_run_event("proc-stopped", &pids, &[("reason", plain_kw("stopped"))]));
            }
            RunState::Stopping { .. } | RunState::GivenUp => {}
        }
    }

    /// Records and decides ONE death event.
    fn on_exit(&mut self, ev: ExitEvent) {
        let Some(&run) = self.run_of.get(&ev.pid) else {
            debug_assert!(false, "a :proc-exit event for a pid this flow never spawned: {:?}", ev.pid);
            return;
        };
        let cfg = self.cfg_of_run[run].clone();
        // The phase is read under its own lock and released immediately: this
        // is the DECISION-time check (wall W4's first half). The ACTION-time
        // one, which is the load-bearing half, is taken under the same lock
        // in `act_restart` and held across the whole respawn.
        let phase = *lock_mutex(&self.flow.phase);
        let decision = decide(&ev, cfg.as_ref(), &self.table[run], phase, sup_now());
        match decision {
            // A duplicate/stale event changes nothing at all -- in
            // particular it must NOT re-stamp `decided`, which is what makes
            // the first event of a (run, incarnation) the one that decides.
            Decision::Ignore(IgnoreCause::Duplicate | IgnoreCause::StaleIncarnation) => {}
            // L4 W3: the user's `stop-proc` has been served. The reason rides
            // the event so an operator can tell a proc that honored its
            // control chan (`:stopped`) from one that had to be killed
            // (`:killed`) -- the whole observable difference between the two
            // rungs of the ladder. `GivenUp` is terminal BY INTENT here, not
            // by exhaustion: a stopped proc that came back would be a
            // supervisor fighting its user.
            Decision::Ignore(IgnoreCause::StopRequested) => {
                self.table[run].decided = Some(ev.incarnation);
                self.table[run].state = RunState::GivenUp;
                let pids = self.run_pids(run);
                self.report(render_run_event(
                    "proc-stopped",
                    &pids,
                    &[("reason", exit_reason_kw(ev.reason))],
                ));
            }
            Decision::Ignore(_) => {
                self.table[run].decided = Some(ev.incarnation);
            }
            Decision::ScheduleRestart { at, delay_ms } => {
                let grace = Duration::from_millis(cfg.as_ref().map_or(0, |c| c.grace_ms));
                let rec = &mut self.table[run];
                rec.decided = Some(ev.incarnation);
                rec.state = RunState::Backoff { at, delay_ms, wedge_at: at + grace };
            }
            Decision::GiveUp { restarts } => {
                self.table[run].decided = Some(ev.incarnation);
                self.table[run].state = RunState::GivenUp;
                self.give_up(run, restarts, cfg.as_ref().map_or(OnGiveUp::Report, |c| c.on_give_up));
            }
        }
    }

    /// The earliest deadline the supervisor owes an action at -- a scheduled
    /// restart or (L4 W3) a stop-proc's grace window / kill retry -- or
    /// `None` when nothing is pending. Exactly what [`sup_park`] needs to
    /// know, and the only reason this task ever wakes on time rather than on
    /// an event.
    fn next_deadline(&self) -> Option<Instant> {
        self.table
            .iter()
            .filter_map(|r| match r.state {
                RunState::Backoff { at, .. } => Some(at),
                RunState::Stopping { deadline, .. } => Some(deadline),
                _ => None,
            })
            .min()
    }

    /// Acts on every deadline that has passed. Returns `true` if it did
    /// anything, which tells the loop to re-check `sup_chan` before parking
    /// again (a respawn -- and a kill -- can itself produce a death event).
    fn fire_due(&mut self, now: Instant) -> bool {
        let mut acted = false;
        for run in 0..self.table.len() {
            match self.table[run].state {
                RunState::Backoff { at, delay_ms, wedge_at } if at <= now => {
                    acted = true;
                    self.act_restart(run, now, delay_ms, wedge_at);
                }
                RunState::Stopping { deadline, attempts } if deadline <= now => {
                    acted = true;
                    self.act_escalate(run, now, attempts);
                }
                _ => {}
            }
        }
        acted
    }

    /// **L4 W3: the escalation ladder's second rung.** The grace window is
    /// spent and the run has not died, so the graceful `::flow/stop` was not
    /// enough -- the proc is wedged mid-flight (parked on a chan nobody
    /// feeds, most typically) and will never reach the control-check point
    /// that would honor it.
    ///
    /// **The phase lock is held across the kill order**, for the same reason
    /// [`Supervisor::act_restart`] holds it across the respawn: it is the one
    /// thing that orders this action against `stop_flow_cell`, which flips
    /// the phase before it takes the runtime out. The file-wide lock order
    /// (phase, then runtime) is unchanged -- a kill touches neither the flow
    /// runtime nor any chan state, only the target task's state word and its
    /// shard's inbox, and nothing in the runtime ever reaches back for a flow
    /// lock, so no cycle exists to invert.
    ///
    /// **`:io` runs stop here** (wall W6, owner ruling #4): an OS thread
    /// cannot be force-unwound, so the ladder has no second rung for one and
    /// says so out loud with `:proc-wedged` rather than pretending.
    ///
    /// **The retry cadence** ([`KILL_RETRY_BUDGET`], one attempt per
    /// [`IO_CONFIRM_TICK`]) is not defensive coding: a kill claims only a
    /// task that is PARKED at the instant of the CAS, and a task that is
    /// mid-hop -- committing, waking, being resumed -- refuses it (probe
    /// finding F5, plus P5a-bis's salvage arm, which resurrects a task whose
    /// commit landed under the killer and returns `false` on purpose so this
    /// loop tries again at its next park).
    fn act_escalate(&mut self, run: usize, now: Instant, attempts: u32) {
        let flow = self.flow.clone();
        let phase_guard = lock_mutex(&flow.phase);
        if *phase_guard != FlowPhase::Running {
            // Stop owns the endgame from here (wall W4); it broadcasts
            // `::flow/stop` to this very control chan and waits on this very
            // done-cell.
            self.table[run].state = RunState::GivenUp;
            return;
        }
        let killable = self.runs[run].is_task;
        let terminal = self.live_task[run].as_ref().is_none_or(|h| h.is_terminal());
        if !killable || attempts >= KILL_RETRY_BUDGET {
            drop(phase_guard);
            self.wedge(run);
            return;
        }
        if !terminal {
            // `false` == "not parked at this instant"; the next tick retries.
            if let Some(h) = self.live_task[run].as_ref() {
                h.kill();
            }
        }
        drop(phase_guard);
        // Either way we come back in a tick: a claimed kill still has to
        // force-unwind the stack and let `ExitGuard::drop` put the event, and
        // it is that EVENT -- not the claim -- that ends the ladder (the
        // `StopRequested` arm of `on_exit`). A task already terminal is the
        // same wait with the kill already done.
        self.table[run].state = RunState::Stopping { deadline: now + IO_CONFIRM_TICK, attempts: attempts + 1 };
    }

    /// `:proc-wedged` + terminal `GivenUp`, the one answer to "this run will
    /// not die and cannot be made to". Shared by the `:io` confirmed-exit
    /// gate (wall W6) and by the kill ladder's budget exhaustion.
    ///
    /// `decided` is CLEARED on purpose: if the run's exit event does show up
    /// eventually, it decides afresh against the current incarnation instead
    /// of being swallowed as a duplicate of the decision made here. That is
    /// the re-decide hook W2 built and W3 reuses verbatim.
    fn wedge(&mut self, run: usize) {
        self.table[run].state = RunState::GivenUp;
        self.table[run].decided = None;
        let pids = self.run_pids(run);
        self.report(render_run_event("proc-wedged", &pids, &[]));
    }

    /// THE restart action: respawn one run as its next incarnation and swap
    /// its `ProcRuntime` entries into the live flow.
    ///
    /// **The flow's phase lock is held across the whole action** -- check,
    /// spawn, and swap -- which is wall W4's real answer. `stop_flow_cell`
    /// flips the phase to `Stopped` under that same lock BEFORE it takes the
    /// runtime out, so "phase is `Running` while I hold this" implies "the
    /// runtime is still installed and `stop` has not started", and the swap
    /// below cannot land in a flow that is already tearing down. A `stop`
    /// arriving one instant later broadcasts to the SAME persistent control
    /// chans and waits on the done-cells this action just installed, so the
    /// new incarnation is stopped and waited for exactly like any other proc.
    /// The lock order (phase, then runtime) is the one every other call site
    /// in this file already takes, so nothing can invert it.
    ///
    /// **Wall W6, `:io` runs.** A thread proc cannot be killed, so a restart
    /// is only sound once the previous incarnation has actually exited -- and
    /// the only proof of that is its done-cell being CLOSED. The death event
    /// is put by `ExitGuard::drop` BEFORE the cell closes (that ordering is
    /// deliberate, see the guard), so the confirmation can genuinely lag the
    /// event by the width of two chan ops; the supervisor therefore re-checks
    /// on an [`IO_CONFIRM_TICK`] cadence until the cfg's `:grace-ms` is spent
    /// and only THEN calls it a wedge. (The landing spec's shorter form --
    /// "restart only if the cell is closed, else `:proc-wedged`" -- would
    /// report a wedge for that microsecond-wide race; bounding the
    /// confirmation is the same rule with the race taken out of it.) A wedged
    /// run is left with `decided` cleared, so if its exit event ever does
    /// arrive it decides afresh -- which is the hook W3's escalation ladder
    /// hangs its retry on.
    ///
    /// **L4 W5, auto-resume (owner ruling #5).** After the swap above lands,
    /// still under `phase_guard` and reusing the very `rt_guard` the swap
    /// just took (no second lock): if `cfg.auto_resume` and EVERY member's
    /// `FlowRuntime::desired` reads `Running`, one synthesized
    /// `::flow/resume` goes onto each member's control chan via
    /// [`chan_try_put`], never a blocking [`chan_put`]. That non-blocking
    /// choice is provably safe rather than merely convenient: `control` is
    /// `Fixed(CONTROL_BUF)` (10) and this is the FIRST put anyone has ever
    /// made to THIS incarnation's control chan -- the chan is the identity
    /// and persists, but the swap two lines up is the instant this
    /// incarnation's reader starts draining it, so nothing queued for a
    /// previous incarnation is still sitting there waiting to compete for
    /// room (a dead incarnation's control chan is left empty by construction
    /// -- it only ever received `::flow/stop`-shaped commands right before
    /// the death that ended it, or the death raced ahead of them, in which
    /// case there is no live reader for the racer to worry either way).
    /// Concretely: a fresh `Fixed(10)` chan cannot be full, and it cannot be
    /// closed until `stop` closes it, which cannot happen while `phase_guard`
    /// is held (`stop_flow_cell` takes the very same lock first). The
    /// `debug_assert!` below is the same "prove it, don't just believe it"
    /// discipline `ExitGuard::drop`'s event `try_put` and
    /// `native_stop_proc`'s request `try_put` already use for their own
    /// "this cannot be full" arguments.
    ///
    /// A fused run resumes as a UNIT: `all()` over every member, so a run
    /// where even one member's last recorded intent was `Paused` stays
    /// paused entirely, conservative and deliberate (design §3.4's "the run
    /// is the death unit" extended to "the run is the resume unit" too --
    /// there is no coherent way to resume half a shared-stack task). The
    /// outcome rides `:proc-restart`'s new `:resumed` field either way, so an
    /// operator watching report-chan never has to infer it.
    fn act_restart(&mut self, run: usize, now: Instant, delay_ms: u64, wedge_at: Instant) {
        let flow = self.flow.clone();
        let phase_guard = lock_mutex(&flow.phase);
        if *phase_guard != FlowPhase::Running {
            self.table[run].state = RunState::GivenUp;
            return;
        }
        if !self.runs[run].is_task && !self.io_exit_confirmed(&flow, run) {
            if now < wedge_at {
                self.table[run].state = RunState::Backoff { at: now + IO_CONFIRM_TICK, delay_ms, wedge_at };
                return;
            }
            drop(phase_guard);
            self.wedge(run);
            return;
        }
        let incarnation = self.table[run].incarnation + 1;
        let spawned = match spawn_run(&mut self.runs[run], incarnation) {
            Ok(s) => s,
            Err(e) => {
                // The OS refused a thread. Not a policy outcome and not
                // something a retry cadence would help with (the next
                // attempt would ask the same OS the same question), so it
                // is reported on the ERROR chan -- where every other
                // lifecycle failure of this shape goes, `emit_lifecycle_error`
                // -- and the run is given up on, with a `:proc-give-up` whose
                // `:restarts` is simply what the run had spent so far. The
                // error map is what says WHY; the give-up says the same thing
                // it always says, "this run will not come back".
                drop(phase_guard);
                let head = self.runs[run].members[0].pid.clone();
                emit_lifecycle_error(&self.error_chan, &head, "restart", e.message.clone());
                self.table[run].state = RunState::GivenUp;
                let restarts = self.table[run].restarts.len() as i64;
                let pids = self.run_pids(run);
                self.report(render_run_event("proc-give-up", &pids, &[("restarts", Value::Int(restarts))]));
                return;
            }
        };
        // L4 W3: the escalation ladder aims at the CURRENT incarnation and
        // nothing else, so the handle is replaced here, in the one place an
        // incarnation is ever created. `None` for a thread run, always.
        self.live_task[run] = spawned.task;
        // L4 W5: does THIS run's cfg even want auto-resume? Read once, before
        // the swap block, since it needs no lock at all (`cfg_of_run` is the
        // supervisor's own private table).
        let auto_resume = self.cfg_of_run[run].as_ref().is_some_and(|c| c.auto_resume);
        let mut all_desired_running = false;
        {
            let mut rt_guard = lock_mutex(&flow.runtime);
            let Some(rt) = rt_guard.as_mut() else {
                // Unreachable while `phase == Running` (see this fn's doc);
                // the spawned incarnation is not orphaned even so -- its
                // control chan is engine-owned and `stop` closes it, which
                // the proc reads as `ExitReason::Normal`.
                debug_assert!(false, "phase is Running but the flow runtime is gone");
                drop(rt_guard);
                self.table[run].state = RunState::GivenUp;
                return;
            };
            let mut handle = spawned.thread;
            for (m, done) in self.runs[run].members.iter().zip(spawned.dones) {
                rt.procs.insert(
                    m.pid.clone(),
                    ProcRuntime { control_chan: m.control.clone(), done, thread: Mutex::new(handle.take()) },
                );
            }
            // L4 W5: read every member's desired state HERE, under the SAME
            // `rt_guard` the swap just used -- see this fn's doc for why a
            // second lock is neither needed nor taken.
            if auto_resume {
                let desired = lock_mutex(&rt.desired);
                all_desired_running =
                    self.runs[run].members.iter().all(|m| desired.get(&m.pid) == Some(&DesiredState::Running));
            }
        }
        // L4 W5: the resume put itself lands OUTSIDE the `rt_guard` block
        // above (it never touches `FlowRuntime` -- `m.control` is the
        // persistent, engine-owned chan the swap just reinstalled) but is
        // still made while `phase_guard` is held -- see this fn's doc: that
        // is what makes "this chan cannot be closed yet" a proven fact
        // (`stop_flow_cell` takes the very same phase lock before it does
        // anything to any control chan) rather than a timing hope.
        let resumed = auto_resume && all_desired_running;
        if resumed {
            let mut cmd = PMap::new();
            cmd.insert(kw("op"), plain_kw("resume"));
            let cmd = Value::Map(cmd);
            for m in &self.runs[run].members {
                let sent = chan_try_put(&m.control, cmd.clone());
                debug_assert!(
                    matches!(sent, TryPut::Sent),
                    "a fresh incarnation's control chan cannot be full or closed -- see act_restart's doc"
                );
            }
        }
        drop(phase_guard);
        let window = self.cfg_of_run[run].as_ref().map_or(0, |c| c.window_ms);
        let rec = &mut self.table[run];
        rec.incarnation = incarnation;
        rec.state = RunState::Running;
        rec.decided = None;
        rec.restarts.retain(|t| now.saturating_duration_since(*t) < Duration::from_millis(window));
        rec.restarts.push(now);
        rec.consecutive = rec.consecutive.saturating_add(1);
        let pids = self.run_pids(run);
        self.report(render_run_event(
            "proc-restart",
            &pids,
            &[
                ("incarnation", Value::Int(incarnation as i64)),
                ("delay-ms", Value::Int(delay_ms as i64)),
                ("resumed", Value::Bool(resumed)),
            ],
        ));
    }

    /// Wall W6's confirmation: is every member of this `:io` run's PREVIOUS
    /// incarnation actually gone? "Gone" is the done-cell being closed, which
    /// is the last act of the guard on the dying thread's stack, and reading
    /// it is one uncontended lock per member.
    fn io_exit_confirmed(&self, flow: &Arc<FlowCell>, run: usize) -> bool {
        let rt_guard = lock_mutex(&flow.runtime);
        let Some(rt) = rt_guard.as_ref() else { return false };
        self.runs[run]
            .members
            .iter()
            .all(|m| rt.procs.get(&m.pid).is_some_and(|p| lock_mutex(&p.done.state).closed))
    }

    /// `:on-give-up`. `:report` (the default, owner ruling #3) leaves the
    /// proc down and the flow alive -- supervision must never crash the flow
    /// either. `:stop-flow` stops it, from ONE FRESH OS THREAD and never
    /// inline, for two independent reasons: `stop_flow_cell` thread-sleeps in
    /// `join_with_timeout` and must not run on a shard (design §7 R4-bis),
    /// and it waits on this very supervisor's done-cell -- which only closes
    /// when this task returns, so calling it from here would be a self-join.
    fn give_up(&mut self, run: usize, restarts: u32, on_give_up: OnGiveUp) {
        let pids = self.run_pids(run);
        self.report(render_run_event("proc-give-up", &pids, &[("restarts", Value::Int(restarts as i64))]));
        if on_give_up != OnGiveUp::StopFlow {
            return;
        }
        let flow = self.flow.clone();
        // **L5/W3 fence #3 (design §4): in sim, a TASK, not an OS thread.**
        // Both reasons this may not run INLINE still hold (it thread-sleeps
        // in `join_with_timeout`, and it would self-join this supervisor's
        // own done-cell), so it still gets its own runner -- just a task one.
        // The thread-sleeping half is moot under sim: fence #1 refuses `:io`
        // procs, so a sim flow has no proc THREADS and `stop`'s join paths
        // are dead code here; every wait it performs goes through
        // `wait_done_with_timeout`, whose task arm parks on the timer
        // service (`park_tick`) instead of sleeping. As an OS thread it
        // would be invisible to the advance rule (P6b F2) at the exact
        // moment the flow is tearing down.
        if crate::clock::sim_enabled() {
            crate::runtime::spawn(move || {
                stop_flow_cell(&flow);
            });
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("flow-give-up-stop".to_string())
            .spawn(crate::memstat::drained(move || {
                stop_flow_cell(&flow);
            }));
        match spawned {
            // Detached deliberately: `stop` is now the endgame's owner and
            // this task is about to exit (its own `sup_chan` is closed by
            // that very call), so there is nobody left to join it.
            Ok(h) => drop(h),
            Err(e) => emit_lifecycle_error(
                &self.error_chan,
                &pids[0],
                "give-up",
                format!("couldn't spawn the :on-give-up :stop-flow thread: {e}"),
            ),
        }
    }

    fn run_pids(&self, run: usize) -> Vec<Value> {
        self.runs[run].members.iter().map(|m| m.pid.clone()).collect()
    }
}

/// The `#::flow{:pid :pids :op ...}` builder for the supervisor's own
/// lifecycle events (`:proc-restart`, `:proc-give-up`, `:proc-wedged`) --
/// [`render_exit_reason`]'s sibling, same namespacing split (`::flow` KEYS
/// via [`kw`], plain-keyword VALUES).
///
/// `:pid` is the run's HEAD, so every event this file emits can be keyed the
/// same way; `:pids` is the whole run, because the restart unit is the run
/// and a fused run's members are restarted together or not at all (design
/// §3.4). For a run of one -- the ordinary case -- `:pids` is a one-element
/// vector holding exactly `:pid`.
fn render_run_event(op: &str, pids: &[Value], extra: &[(&str, Value)]) -> Value {
    let mut m = PMap::new();
    m.insert(kw("pid"), pids.first().cloned().unwrap_or(Value::Nil));
    m.insert(kw("op"), plain_kw(op));
    m.insert(kw("pids"), Value::Vector(pids.iter().cloned().collect::<PVec>()));
    for (k, v) in extra {
        m.insert(kw(k), v.clone());
    }
    Value::Map(m)
}

/// How many supervisor TASKS this process has spawned -- [`sup_chan_census`]'s
/// sibling, same process-global monotonic-counter discipline (measure a DELTA
/// across a `flow/start`, never an absolute) and same reason for existing:
/// "did this flow spawn a supervisor at all" has no Mova-visible signature,
/// so only a white-box counter can tell an unsupervised flow's silence apart
/// from a supervised flow that happens to have nothing to say.
///
/// This is the byte-identical-default-build half of G-CONF for W2: an
/// unsupervised flow must spawn ZERO supervisors, which is asserted directly
/// in this file's own `#[cfg(test)] mod tests`.
#[allow(dead_code)] // called from this file's own #[cfg(test)] module only, for now
pub(crate) fn supervisor_spawn_census() -> usize {
    SUPERVISOR_SPAWNS.load(Ordering::Relaxed)
}

static SUPERVISOR_SPAWNS: AtomicUsize = AtomicUsize::new(0);

// ===========================================================================
// L4 W2/W3 SUPERVISOR / POLICY REGION -- END
// ===========================================================================

/// The mult's fast pass, shared verbatim by both worlds: a non-blocking
/// `try_put` of `v` to every dest, sorting the ones that could not take it
/// right now into `would_block` (genuinely full -- the caller's world
/// decides how to wait) and `closed` (untap; see [`mult_untap`]). Indices
/// are pushed in `dests` order, which is what makes each dest's own message
/// order the source's order.
fn mult_fast_pass(dests: &[Arc<Chan>], v: &Value, would_block: &mut Vec<usize>, closed: &mut Vec<usize>) {
    for (i, d) in dests.iter().enumerate() {
        match chan_try_put(d, v.clone()) {
            TryPut::Sent => {}
            TryPut::Closed => closed.push(i),
            TryPut::WouldBlock => would_block.push(i),
        }
    }
}

/// The mult's untap, shared verbatim by both worlds: drop every dest that
/// was found closed this lap -- "never crash broadcast, untap only the dest
/// that stopped accepting" per FLOW-DESIGN.md. Sorted/deduped (an index can
/// arrive from both the fast pass and the straggler pass) and removed from
/// the back, so the earlier indices stay valid as the vector shrinks.
fn mult_untap(dests: &mut Vec<Arc<Chan>>, mut closed: Vec<usize>) {
    closed.sort_unstable();
    closed.dedup();
    for i in closed.into_iter().rev() {
        dests.remove(i);
    }
}

/// Native mult (fan-out) THREAD -- the `MOVA_FLOW_THREAD_PROCS=1` world
/// only, since L3.5 item 2 (see [`run_mult_task`], which is what the default
/// world spawns). Blocking-takes from `source`, fast-passes `try_put` to
/// every dest, falls back to a `try_put`/`sleep` retry loop
/// ([`MULTI_INPUT_BACKOFF`]) for any that were full, and silently untaps
/// (removes) any dest found closed -- "never crash broadcast" per
/// FLOW-DESIGN.md. No control awareness needed here (per that doc): the
/// mult's only job is to keep `source` drained so its producer never blocks
/// on it; it exits when `source` closes (which `stop` guarantees via
/// `engine_owned_chans`, after every proc thread has already exited).
///
/// UNCHANGED in behavior by L3.5 -- the kill switch has to reproduce the
/// pre-L3 engine exactly, and the sleep is what it reproduces.
fn run_mult_thread(source: Arc<Chan>, mut dests: Vec<Arc<Chan>>) {
    loop {
        let Some(v) = chan_take(&source) else { break };
        let mut would_block: Vec<usize> = Vec::new();
        let mut closed: Vec<usize> = Vec::new();
        mult_fast_pass(&dests, &v, &mut would_block, &mut closed);
        for i in would_block {
            loop {
                match chan_try_put(&dests[i], v.clone()) {
                    TryPut::Sent => break,
                    TryPut::Closed => {
                        closed.push(i);
                        break;
                    }
                    // L5/W3 fence #17 -- see `warn_sim_os_poll_loop`. This
                    // whole function only runs under
                    // `MOVA_FLOW_THREAD_PROCS=1`, which fence #1 already
                    // refuses at `create-flow` in sim; the warning is the
                    // belt to that braces, for any future caller.
                    TryPut::WouldBlock => {
                        static WARNED: AtomicBool = AtomicBool::new(false);
                        warn_sim_os_poll_loop("run_mult_thread (MULTI_INPUT_BACKOFF)", &WARNED);
                        std::thread::sleep(MULTI_INPUT_BACKOFF)
                    }
                }
            }
        }
        mult_untap(&mut dests, closed);
    }
}

/// Native mult (fan-out) TASK -- the default world since L3.5 item 2, and
/// the last per-graph-node OS thread `flow/start` used to spawn. Same
/// semantics as [`run_mult_thread`], one line different: a straggler is
/// waited for with a plain BLOCKING [`chan_put`] instead of the
/// `try_put`/[`MULTI_INPUT_BACKOFF`]-sleep retry loop.
///
/// **Why no doorbell** (design §3.7 ruled the conversion needed "a
/// doorbell-based backoff"; L3.5 supersedes that). §3.7 predates L1/W3's
/// task-native chan ops. The mult has no control-chan awareness BY DESIGN
/// (FLOW-DESIGN.md), so there is nothing here for a doorbell to race a
/// blocking op against -- the whole reason a proc loop needs one. Both of
/// this loop's waits are already task-native: [`chan_take`] parks a task in
/// `task_takers`, and [`chan_put`] parks one in `task_putters` against its
/// own commit cell (L1/W3). What could NOT survive the move is the sleep --
/// a task that sleeps burns its whole shard, including every proc placed on
/// it -- so it simply dies here rather than being replaced by anything.
///
/// **Closed dests, both ways in.** `chan_put` returns `false` iff the chan
/// is closed -- either already closed on entry (`builtins::async`,
/// `chan_put`'s `if g.closed { return false }`) or closed WHILE this task
/// is parked on it, since `chan_close` drains `task_putters` and stores
/// `PUT_CLOSED` into each waiter's commit cell before waking it, which
/// `park_task_putter` reads back as `false`. So `false` means "untap" in
/// both cases and a dest closing under a parked mult can never wedge it --
/// which is exactly what `stop` relies on: it force-closes every
/// engine-owned chan, and a mult parked on a full dest must come back and
/// then find `source` closed.
///
/// **Why the self-loop shape cannot deadlock** (flow.rs's `plan_fusion`
/// notes: a self-loop, and a pure cycle, always route through a mult).
/// Proc `:l`'s `:self-out` feeds a mult whose only dest is `:l`'s own
/// `:self-in`. Fill that buffer and the mult parks in `chan_put` -- and a
/// task park YIELDS THE SHARD (the value moves into the `PutterWaiter` and
/// the task suspends; `runtime` is free to run anything else on that
/// worker), so `:l` runs even when the runtime placed it on the very same
/// shard as its mult. `:l`'s next `chan_take` on `:self-in` pops the buffer
/// and `promote_task_putters` immediately moves the mult's value into the
/// freed slot and commits it. This is the one shape a `sleep`-based
/// straggler wait WOULD have deadlocked outright once converted (a sleeping
/// task never yields, so `:l` would never run to drain it) -- the blocking
/// put is not an optimization here, it is the correctness argument.
fn run_mult_task(source: Arc<Chan>, mut dests: Vec<Arc<Chan>>) {
    loop {
        let Some(v) = chan_take(&source) else { break };
        let mut would_block: Vec<usize> = Vec::new();
        let mut closed: Vec<usize> = Vec::new();
        mult_fast_pass(&dests, &v, &mut would_block, &mut closed);
        // Sequential, in `dests` order and one dest at a time, exactly as
        // the thread world's retry loop is: a mult must not reorder a
        // dest's own stream, and nothing is gained by racing the stragglers
        // against each other anyway -- this task has one message in hand and
        // cannot take the next until every dest has this one.
        for i in would_block {
            if !chan_put(&dests[i], v.clone()) {
                closed.push(i);
            }
        }
        mult_untap(&mut dests, closed);
    }
}

// ---------------------------------------------------------------------------
// Proc thread: init, main loop, control, transform
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunStatus {
    Paused,
    Running,
}

fn status_keyword(rs: RunStatus) -> Value {
    plain_kw(match rs {
        RunStatus::Paused => "paused",
        RunStatus::Running => "running",
    })
}

enum ProcOutcome {
    Continue,
    Stop,
}

/// Everything `run_proc` needs, bundled so it (and every helper it hands a
/// mutable slice of itself to) stays under clippy's argument-count lint
/// without sacrificing clarity -- see the module doc.
struct ProcSpawn {
    interp: Interp,
    pid: Value,
    step_fn: Value,
    args: Value,
    ins: HashMap<Value, Arc<Chan>>,
    outs: HashMap<Value, Option<Arc<Chan>>>,
    /// This proc's transport lanes, if the wiring gave it any. Always
    /// `None` for a member of a multi-proc fused run, and always `None`
    /// under `MOVA_NO_SPSC=1`. See the module doc's "Transport selection".
    in_lane: Option<InLane>,
    out_lane: Option<OutLane>,
    control: Arc<Chan>,
    error_chan: Arc<Chan>,
}

/// Borrowed view of a running proc's mutable state, threaded through
/// [`apply_control`]/[`process_message`]/[`build_ping_reply`]/
/// [`emit_transform_error`] so none of those need more than a couple of
/// explicit parameters.
struct ProcCtx<'a> {
    interp: &'a mut Interp,
    pid: &'a Value,
    step_fn: &'a Value,
    state: &'a mut Value,
    count: &'a mut u64,
    run_status: &'a mut RunStatus,
    ins: &'a HashMap<Value, Arc<Chan>>,
    outs: &'a HashMap<Value, Option<Arc<Chan>>>,
    out_lane: Option<&'a OutLane>,
    control: &'a Arc<Chan>,
    /// The proc's own [`Doorbell`] -- carried here for exactly ONE consumer,
    /// [`send_with_control_priority`]'s task arm, which registers it in the
    /// target chan's `alts_doorbells` family while a send is blocked
    /// (L3/W1b). Every other park site already has it in hand directly.
    doorbell: &'a Arc<Doorbell>,
}

/// `::flow/input-filter` as a cached `'static` -- `extract_input_filter`
/// runs once per drained batch, and rebuilding the fully-qualified keyword
/// (`format!` + `Str`) each time was a measurable slice of the per-batch
/// allocation traffic (W4, bench/RESULTS-w4-alloc-attrib.md).
fn input_filter_kw() -> &'static Value {
    static K: OnceLock<Value> = OnceLock::new();
    K.get_or_init(|| kw("input-filter"))
}

fn extract_input_filter(state: &Value) -> Option<Value> {
    match state {
        Value::Map(m) => m.get(input_filter_kw()).cloned(),
        _ => None,
    }
}

/// Rebuilds the cached read-set from `ins`, applying `filter` (a predicate
/// of cid) if present. A filter that errors excludes that cid conservatively
/// (documented: a broken `::flow/input-filter` predicate degrades to "read
/// nothing new" rather than crashing the proc). Sorted by `pr_str` so
/// multi-input round-robin order is reproducible across recomputes -- a
/// determinism aid for tests, not something upstream itself guarantees.
///
/// **The sort runs BEFORE the filter (L5/W3, fence #9a -- design §4).** It
/// used to run after, which left the one real hole in an otherwise sealed
/// HashMap-order audit: `ins` is a `HashMap`, so the candidate order handed
/// to `retain` was HASH order -- and `retain` calls the USER's predicate in
/// that order. A pure `::flow/input-filter` cannot tell (the retained set is
/// the same either way, and the sort pinned the output order regardless),
/// but a side-effecting one -- one that logs, counts, or reads an atom --
/// observes hash iteration order, exactly the class of nondeterminism the
/// audit exists to remove. Sorting first makes the predicate's call sequence
/// a function of the port ids alone. Not a sim-only change: it is a plain
/// determinism fix, and both modes take it.
fn recompute_read_set(ins: &HashMap<Value, Arc<Chan>>, filter: Option<&Value>, interp: &mut Interp) -> Vec<(Value, Arc<Chan>)> {
    let mut out: Vec<(Value, Arc<Chan>)> = ins.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    out.sort_by(|(a, _), (b, _)| crate::printer::pr_str(a).cmp(&crate::printer::pr_str(b)));
    if let Some(f) = filter {
        out.retain(|(cid, _)| matches!(interp.call(f, std::slice::from_ref(cid)), Ok(v) if v.truthy()));
    }
    out
}

/// Registers `doorbell` on every chan in `new` and clears it (back to
/// `None`) on every chan that was in `old` but dropped out of `new` -- the
/// module doc's "register/deregister as a proc's read-set changes" step. A
/// chan is compared by pointer (`Arc::ptr_eq`, matching `InLane`/`OutLane`'s
/// own identity test): an `init` that replaced a port via
/// `::flow/in-ports` hands the loop a different `Arc`, which this correctly
/// treats as "old chan gone, new chan added" rather than trying to diff by
/// port id. Clearing a dropped chan's slot is not a correctness requirement
/// (a stray ring is harmless -- see `Doorbell::ring`'s doc) but avoids
/// holding this proc's `Doorbell` alive via a chan that outlives this
/// proc's interest in it (every engine-owned chan is kept alive by
/// `FlowRuntime::engine_owned_chans` regardless, for `stop` to close it).
fn resync_read_set_doorbell(old: &[(Value, Arc<Chan>)], new: &[(Value, Arc<Chan>)], doorbell: &Arc<Doorbell>) {
    for (_, chan) in new {
        lock_mutex(&chan.state).doorbell = Some(doorbell.clone());
    }
    for (_, chan) in old {
        if !new.iter().any(|(_, c)| Arc::ptr_eq(c, chan)) {
            lock_mutex(&chan.state).doorbell = None;
        }
    }
}

/// [`recompute_read_set`] plus [`resync_read_set_doorbell`] against the
/// PREVIOUS read-set (`prev`) in one call, so every call site that
/// reassigns `read_set` can do so in a single statement without
/// re-borrowing it.
fn recompute_read_set_wired(
    ins: &HashMap<Value, Arc<Chan>>,
    filter: Option<&Value>,
    interp: &mut Interp,
    doorbell: &Arc<Doorbell>,
    prev: &[(Value, Arc<Chan>)],
) -> Vec<(Value, Arc<Chan>)> {
    let new = recompute_read_set(ins, filter, interp);
    resync_read_set_doorbell(prev, &new, doorbell);
    new
}

/// Blocking take capped at `timeout` -- the single-input fast path's park
/// primitive (see the module doc). Distinct from `chan_try_take` (never
/// blocks) and `chan_take` (blocks forever): this is `chan_take` with an
/// upper bound so the proc loop can still notice control commands.
///
/// Doorbell-driven (see the module doc and `value.rs`'s `Doorbell` doc): if
/// the non-blocking buffer check finds nothing, parks on `doorbell` rather
/// than on `ch.cv` for at most `timeout`, against the generation `seen`.
///
/// **`seen` is the CALLER's snapshot, and it must be taken at the top of the
/// caller's lap -- BEFORE its non-blocking control check, not after (L3/W1b,
/// and this cost a real deadlock to learn).** The "missed-wakeup
/// correctness" rule is "snapshot before the scan", and the scan a proc loop
/// performs is not just this chan's buffer: it is the control chan first and
/// this chan second. Snapshotting inside this function would leave the
/// control check OUTSIDE the protected window, so a command landing in that
/// gap would ring the doorbell, advance the generation past the snapshot
/// this function was about to take, and then be found by neither the check
/// that already ran nor the park that followed. On a thread proc `timeout`
/// (`PARK_TIMEOUT`, 2s) papered over that: the park expired, the caller
/// looped, the command was seen -- late, but seen. On a TASK proc there is
/// no timeout to expire (see below), so the same gap is a permanent park:
/// `flow/stop` then waits out its whole `STOP_JOIN_TIMEOUT` and abandons a
/// proc that never noticed. Taking `seen` before the control check closes
/// the window for both arms; the price is at most one extra, immediately-
/// returning lap when a ring lands mid-scan.
///
/// **L3/W1a: this is task-safe as written, and that is why it carries no
/// `in_task()` wall anymore.** `Doorbell::wait_for_change` (`value.rs`) has
/// dispatched `in_task()` to `wait_for_change_task` since L1/W3 -- register
/// a waker under the doorbell's own mutex, re-check the generation while
/// still holding it, `park_current_yield()` -- so a task proc parking here
/// suspends and is resumed by the very same `Doorbell::ring` that unparks a
/// thread proc. The one honest asymmetry: `timeout` is IGNORED on the task
/// arm, by design (the L1 landing stance -- the ring is the mechanism, and a
/// task park gets no safety net to hide a missing wake behind), which is
/// exactly what makes the snapshot-ordering rule above load-bearing rather
/// than merely tidy. Everything else on this path is already task-correct:
/// the `waiting_takers` bump is a plain counter under `ch.state`'s mutex,
/// and the `task_putters` hand-off below is the L1/W3 machinery itself.
///
/// Deliberately does NOT loop internally on a wake that turns out to be
/// "nothing new in `ch`'s buffer" -- `doorbell` is shared with THIS proc's
/// control chan (and every other chan in its read-set), so a ring can mean
/// "control has something" just as easily as "this chan has something".
/// Re-parking on the SAME long `timeout` after such a ring would silently
/// swallow that control command for up to `timeout` -- exactly the
/// regression control-priority exists to prevent. So every wake (real data,
/// a spurious OS wake, or a ring for a completely different reason) returns
/// `WouldBlock` here and hands control straight back to the caller, which
/// (per the module doc) always re-checks `control` non-blocking before
/// calling this again -- if the wake WAS this chan's data arriving, that
/// immediate next call finds it via the non-blocking check at the top, at
/// the cost of one extra cheap round trip through the caller's loop, never
/// an extra wait.
/// **This is a chan-state mutation site outside `builtins::async`**, and it
/// therefore owes the same wake obligations that module's 5 `notify_all()`
/// sites carry (see its module doc): it pops `buffer` and it bumps
/// `waiting_takers`, and BOTH are inputs to somebody else's parked scan --
/// `waiting_takers` because `chan_try_put`'s unbuffered gate reads it, so an
/// `alts!!` put op on an unbuffered chan becomes ready at the increment
/// below and nowhere else.
///
/// The one deliberate exception: the increment does NOT ring
/// `ChanState::doorbell`. On every call path that reaches here, that slot
/// holds `doorbell` -- the very doorbell this function parks on two lines
/// later, against the caller's `seen`. Ringing it would move the
/// generation past `seen`, `wait_for_change` would return instantly, the
/// caller would loop straight back in, and the proc's idle park would become
/// the busy poll FLOW-IDLE-CPU-BUG.md exists to have killed. Nothing is lost
/// by the omission: no proc's scan reads `waiting_takers` (they all go
/// through `chan_try_take`), so the only reader that could care is an
/// `alts!!`, and those live in the separate `alts_doorbells` family, which
/// IS rung.
fn try_take_with_timeout(ch: &Chan, doorbell: &Doorbell, seen: u64, timeout: Duration) -> TryTake {
    let mut g = lock_mutex(&ch.state);
    if let Some(v) = g.buffer.pop_front() {
        // L1/W3: this is a take site, so it owes the take side's task-putter
        // obligations too -- the 6th-site rule in `builtins::async`'s module
        // doc, in its newest form. A pop that frees capacity must promote a
        // parked task putter into it (a `go` block CAN put onto a flow chan
        // even though the proc reading it is a thread), or that putter waits
        // on room that is already there. Empty-queue fast path, so pure-flow
        // traffic pays one load.
        let mut promoted = crate::builtins::r#async::Wakes::new();
        crate::builtins::r#async::promote_task_putters(&mut g, &mut promoted);
        // Freeing buffer room can make a parked put op ready. Clone out and
        // ring after `drop(g)` -- `chan_try_take`'s shape, for the reason
        // given there (a woken waiter's first move is to lock this state).
        // Here the `doorbell` ring IS safe and correct: this path returns
        // `Received`, so the caller processes the message and re-snapshots
        // before its next park -- no self-park to defeat.
        let db = g.doorbell.clone();
        let alts = g.alts_doorbells.clone();
        drop(g);
        ch.cv.notify_all();
        if let Some(db) = db {
            db.ring();
        }
        ring_alts(&alts);
        crate::builtins::r#async::wake_all(&promoted);
        return TryTake::Received(v);
    }
    // L1/W3, the other half of the take side's obligation: an empty buffer
    // with a task putter holding a value out is a message this proc must
    // see. Without this the proc would park on its doorbell beside a putter
    // that is offering it something.
    let mut handed = crate::builtins::r#async::Wakes::new();
    if let Some(v) = crate::builtins::r#async::take_from_task_putter(&mut g, &mut handed) {
        drop(g);
        crate::builtins::r#async::wake_all(&handed);
        return TryTake::Received(v);
    }
    if g.closed {
        return TryTake::Closed;
    }
    g.waiting_takers += 1;
    // Under the lock, like `chan_take`'s matching increment, and to the
    // `alts_doorbells` family ONLY -- see this fn's doc for why
    // `g.doorbell` is deliberately skipped here.
    ring_alts(&g.alts_doorbells);
    drop(g);
    doorbell.wait_for_change(seen, timeout);
    // The decrement rings nothing: 1 -> 0 makes an unbuffered put op
    // UNready, and readiness LOSS never needs a wake.
    lock_mutex(&ch.state).waiting_takers -= 1;
    TryTake::WouldBlock
}

fn call_transition(interp: &mut Interp, step_fn: &Value, state: Value, transition: &str) -> Value {
    let t = kw(transition);
    interp.call(step_fn, &[state.clone(), t]).unwrap_or(state)
}

/// Applies one control command (`resume`/`pause`/`stop`/`ping`) to `ctx`.
/// Returns `true` for `stop` (the caller must exit its loop); every other
/// command mutates `ctx` in place and returns `false`. Shared verbatim by
/// every control-check site (idle park, pre-take, mid-batch, mid-send) --
/// see the module doc's "control-priority wait design" section for why this
/// matters (identical behavior no matter where control was noticed).
fn apply_control(ctx: &mut ProcCtx, cmd: Value) -> bool {
    let Value::Map(m) = &cmd else { return false };
    let Some(Value::Keyword(op)) = m.get(&kw("op")) else { return false };
    match op.as_ref() {
        "resume" => {
            let old = std::mem::replace(ctx.state, Value::Nil);
            *ctx.state = call_transition(ctx.interp, ctx.step_fn, old, "resume");
            *ctx.run_status = RunStatus::Running;
            false
        }
        "pause" => {
            let old = std::mem::replace(ctx.state, Value::Nil);
            *ctx.state = call_transition(ctx.interp, ctx.step_fn, old, "pause");
            *ctx.run_status = RunStatus::Paused;
            false
        }
        "stop" => {
            let old = std::mem::replace(ctx.state, Value::Nil);
            *ctx.state = call_transition(ctx.interp, ctx.step_fn, old, "stop");
            true
        }
        "ping" => {
            if let Some(Value::Channel(reply)) = m.get(&kw("reply-chan")) {
                chan_put(reply, build_ping_reply(ctx));
            }
            false
        }
        _ => false,
    }
}

fn build_ping_reply(ctx: &ProcCtx) -> Value {
    build_ping_reply_params(PingParams {
        pid: ctx.pid,
        run_status: *ctx.run_status,
        count: *ctx.count,
        state: ctx.state,
        ins: ctx.ins,
        outs: ctx.outs,
    })
}

/// Everything a `ping` reply is built from, spelled out as plain values so
/// BOTH proc loops can produce a byte-identical reply map: the generic one
/// (from its `ProcCtx`, via [`build_ping_reply`]) and the promoted fast one
/// (from `FastStep::snapshot()` plus its own counters -- N2, see
/// `run_proc_fast`).
struct PingParams<'a> {
    pid: &'a Value,
    run_status: RunStatus,
    count: u64,
    state: &'a Value,
    ins: &'a HashMap<Value, Arc<Chan>>,
    outs: &'a HashMap<Value, Option<Arc<Chan>>>,
}

/// The one `#::flow{:pid :status :count :state :ins :outs}` builder -- see
/// [`PingParams`]. `:outs` excludes both engine-reserved out-targets
/// (design Part 2, upstream `impl.clj:274`: `(dissoc outs ::flow/error
/// ::flow/report)`) -- they are wiring the engine owns, not a port the
/// proc declared, and upstream's own ping reply hides them identically.
/// Filtered here, centrally, rather than at each of the two call sites
/// (`build_ping_reply`'s generic `ctx.outs`, `apply_control_fast`'s
/// promoted `env.outs`): both already carry the reserved keys (design
/// Part 2 wires them into every proc's outs map unconditionally), so one
/// filter keeps the two loops' replies byte-identical without either
/// having to remember to apply it.
fn build_ping_reply_params(p: PingParams) -> Value {
    let mut m = PMap::new();
    m.insert(kw("pid"), p.pid.clone());
    m.insert(kw("status"), status_keyword(p.run_status));
    m.insert(kw("count"), Value::Int(p.count as i64));
    m.insert(kw("state"), p.state.clone());
    let ins_v: PVec = sorted_by_pr_str(p.ins.keys().cloned().collect()).into_iter().collect();
    let outs_v: PVec =
        sorted_by_pr_str(p.outs.keys().filter(|k| !is_reserved_out_key(k)).cloned().collect()).into_iter().collect();
    m.insert(kw("ins"), Value::Vector(ins_v));
    m.insert(kw("outs"), Value::Vector(outs_v));
    Value::Map(m)
}

/// `RjError::message` is a generic `"user exception"` placeholder for a
/// `(throw v)` (the actual payload lives in `e.thrown` -- see error.rs's
/// `RjError::thrown`); this pulls the human-readable form out of `v` for
/// the error-chan's `:ex :message`, matching what `try`/`catch`'s own
/// `error_to_info_map` does for a caught exception's `:message`. Every
/// other `ErrorKind` already carries a descriptive `message`.
fn error_message(e: &RjError) -> String {
    if e.kind == crate::error::ErrorKind::Thrown {
        match &e.thrown {
            Some(v) => crate::printer::display_str(v),
            None => e.message.clone(),
        }
    } else {
        e.message.clone()
    }
}

/// **S3, the killed-unwind gate** (L4 W3; probe finding S3 in
/// `tests/l4_kill_probe.rs`, design §3.5's amendment block). Called FIRST in
/// the cold `Err(payload)` arm of every `catch_unwind` this file wraps user
/// code in -- there are four, all around a `transform` or an `init` -- and
/// it either hands the payload back (an ordinary panic: report it as an
/// incident and continue, the D-B invariant, untouched) or never returns
/// (a KILL: re-raise, so the forced unwind carries on to the coroutine's
/// root).
///
/// Two independent reasons it must exist, both load-bearing:
/// 1. **Liveness.** `corosensei::Coroutine::force_unwind` LOOPS: it
///    re-throws at a coroutine that suspends again mid-unwind until the
///    stack really reaches its root. A user `transform` that parks inside
///    one of these `catch_unwind`s -- which is ordinary code, a `>!!` on a
///    full chan -- would swallow the payload, park, be re-thrown at, swallow
///    it again... i.e. wedge the shard INSIDE the kill.
/// 2. **Honesty.** A killed proc that "recovered" into its incident handler
///    would put a transform-error map on the error chan describing a failure
///    no user code caused, and then go on to exit with the wrong reason. The
///    differential test (`killed_proc_reports_no_phantom_transform_error`)
///    pins both directions: a killed proc reports nothing, and a THROWING
///    transform in the very same shape still reports its incident and
///    continues.
///
/// The check is in the cold arm only, so the hot path is untouched: a
/// `transform` that neither panics nor is killed never executes this line.
#[cold]
#[inline(never)]
fn reraise_if_killed(payload: Box<dyn std::any::Any + Send>) -> Box<dyn std::any::Any + Send> {
    if crate::runtime::current_task_is_killed() {
        std::panic::resume_unwind(payload);
    }
    payload
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn emit_lifecycle_error(error_chan: &Arc<Chan>, pid: &Value, op: &str, message: String) {
    let mut ex = PMap::new();
    ex.insert(plain_kw("message"), Value::Str(Str::from(message)));
    let mut m = PMap::new();
    m.insert(kw("pid"), pid.clone());
    m.insert(kw("op"), plain_kw(op));
    m.insert(kw("ex"), Value::Map(ex));
    chan_put(error_chan, Value::Map(m));
}

/// Outcome of [`send_with_control_priority`]: either the message left
/// (`Sent`, or `Closed` -- both mean "stop trying, move on to the next
/// message", exactly as the pre-N2 inline loop's `TryPut::Sent |
/// TryPut::Closed => break` did), or a control command was noticed while
/// the send was blocked and is handed BACK to the caller to apply (the
/// caller owns the proc state this function deliberately doesn't touch;
/// after applying it, the caller retries the same send).
enum SendOutcome {
    Sent,
    Closed,
    Control(Value),
}

/// The blocked-send half of the module doc's "control-priority wait
/// design", extracted verbatim from `process_message`'s former inline loop
/// (N2) so the generic and fast proc loops share ONE implementation:
/// non-blocking `try_put`; on `WouldBlock` check `control` non-blocking
/// (returning any command found, so the CALLER applies it and retries) and
/// otherwise wait for room before retrying. `msg` is taken by reference and
/// cloned per attempt -- the same clone-per-`try_put` the inline loop did, so
/// this refactor changes neither behavior nor allocation counts.
///
/// TWO park arms, and only the park forks (design §7 R7): a THREAD proc
/// takes the pre-L3 capped `cv_wait_timeout` on the target chan's own
/// condvar, byte for byte; a TASK proc takes [`blocked_send_task`], which is
/// the `alts!!` shape. `doorbell` is the proc's OWN doorbell -- the one it
/// parks on everywhere else -- and is threaded in from the call sites purely
/// so the task arm has something to register; the thread arm never looks at
/// it.
#[inline]
fn send_with_control_priority(chan: &Chan, msg: &Value, control: &Chan, doorbell: &Arc<Doorbell>) -> SendOutcome {
    loop {
        match chan_try_put(chan, msg.clone()) {
            TryPut::Sent => return SendOutcome::Sent,
            TryPut::Closed => return SendOutcome::Closed,
            TryPut::WouldBlock => {
                if let TryTake::Received(cmd) = chan_try_take(control) {
                    return SendOutcome::Control(cmd);
                }
                // L3/W1b: wall #8's task arm. The alternative here is a raw
                // `cv_wait_timeout` on the target chan's condvar -- a THREAD
                // park, invisible to a task waker -- which a task would not
                // merely sleep in: it would hold its shard's OS thread
                // inside a 1ms condvar poll loop that never yields to the
                // scheduler, livelocking every other task placed there.
                if crate::runtime::in_task() {
                    return blocked_send_task(chan, msg, control, doorbell);
                }
                let g = lock_mutex(&chan.state);
                let _ = cv_wait_timeout(&chan.cv, g, BLOCKED_SEND_TIMEOUT);
            }
        }
    }
}

/// [`send_with_control_priority`]'s TASK arm -- wall #8, torn down (design
/// §3.4 as superseded after W1a).
///
/// A blocked send is semantically `alts([put target msg] [take control])`
/// with a bias order, and that is exactly what this is, using `alts!!`'s own
/// already-proven machinery and inventing no synchronization:
///
/// - **One doorbell, two ring sources.** The proc's own [`Doorbell`] is
///   already registered in the CONTROL chan's `ChanState::doorbell` slot
///   (`run_ready`/`run_fused` do that at spawn), so a `pause`/`stop`/`ping`
///   rings it. [`BlockedSendRegistration`] additionally pushes it into the
///   TARGET chan's `alts_doorbells` family for the duration of this call, so
///   every readiness-gaining mutation of the target rings it too -- room
///   freed by the downstream's take, and the target's close. One park
///   covers both arms.
/// - **Registration BEFORE the first generation snapshot, and the snapshot
///   BEFORE each try.** That order is the whole missed-wakeup argument (see
///   `value.rs`'s `Doorbell` doc and `builtins::async`'s module doc): a ring
///   that lands between our try and our park has necessarily already moved
///   the generation past `seen`, so `wait_for_change` returns instantly
///   instead of blocking, and we loop and re-try. The registration is
///   outside the loop because it only has to precede the FIRST snapshot;
///   `Drop` removes it on every exit path (returned, or unwound through).
/// - **Control priority is preserved exactly.** The same non-blocking
///   `chan_try_take(control)` at the same point in the cycle, handing the
///   command back to the same [`apply_control`] seam. What changes is only
///   the latency: the thread arm notices a command within
///   `BLOCKED_SEND_TIMEOUT` (1ms of poll), this arm notices it on the ring,
///   which is strictly better.
///
/// The `BLOCKED_SEND_TIMEOUT` handed to `wait_for_change` is inert on this
/// path -- the task arm of `wait_for_change` has no safety net by design
/// (the L1 landing stance: the ring is the mechanism, not a backstop) and
/// ignores its `safety_net` argument. It is passed anyway so the two arms of
/// this function read as the same shape with the same bound, and so that a
/// future task arm WITH a deadline needs no edit here.
fn blocked_send_task(chan: &Chan, msg: &Value, control: &Chan, doorbell: &Arc<Doorbell>) -> SendOutcome {
    let _reg = BlockedSendRegistration::register(chan, doorbell.clone());
    loop {
        // Snapshot FIRST -- before the try, after the registration above.
        let seen = doorbell.current();
        match chan_try_put(chan, msg.clone()) {
            TryPut::Sent => return SendOutcome::Sent,
            TryPut::Closed => return SendOutcome::Closed,
            TryPut::WouldBlock => {}
        }
        if let TryTake::Received(cmd) = chan_try_take(control) {
            return SendOutcome::Control(cmd);
        }
        doorbell.wait_for_change(seen, BLOCKED_SEND_TIMEOUT);
    }
}

/// RAII registration of ONE blocked send's doorbell on the chan it is
/// waiting for room in -- flow.rs's own small mirror of `builtins::async`'s
/// `AltsRegistration`, deliberately REPLICATED rather than imported.
///
/// The shape is the same and so is the reasoning (register before the first
/// scan; un-register on EVERY exit path, because a leaked entry is a
/// doorbell nobody will wait on again, rung on every mutation of what may be
/// a very hot chan, forever). What is different is the scope: one chan
/// instead of an ops vector, and a borrow instead of an `Arc` clone, because
/// a blocked send's registration cannot outlive the `&Chan` it was made
/// against. Exporting `alts!!`'s type to share ~10 lines would have widened
/// `builtins::async`'s surface for no gain -- the discipline this wave is
/// held to is that it ADDS a registration from a new caller and edits
/// nothing about the commit protocol (design §7 R8's tripwire).
struct BlockedSendRegistration<'a> {
    chan: &'a Chan,
    doorbell: Arc<Doorbell>,
}

impl<'a> BlockedSendRegistration<'a> {
    fn register(chan: &'a Chan, doorbell: Arc<Doorbell>) -> Self {
        lock_mutex(&chan.state).alts_doorbells.push(doorbell.clone());
        BlockedSendRegistration { chan, doorbell }
    }
}

impl Drop for BlockedSendRegistration<'_> {
    fn drop(&mut self) {
        lock_mutex(&self.chan.state)
            .alts_doorbells
            .retain(|db| !Arc::ptr_eq(db, &self.doorbell));
    }
}

/// Calls `transform` (arity-3: `(sf state cid msg)` -> `[state' {out-id
/// [msgs...]}]`), guarded by BOTH a `Result`-level catch (a thrown/user
/// `RjError`) AND `std::panic::catch_unwind` (a genuine Rust panic -- should
/// never happen, but must never take the proc down). On either failure: an
/// error map goes to `error_chan`, PREVIOUS state is kept, and the proc
/// continues (FLOW-DESIGN.md's "transform error" contract). On success,
/// walks the returned `{out-id [msgs...]}` map (iteration order over
/// distinct out-ids is unspecified -- `PMap::Big`'s hash order is not
/// insertion order -- but each out-id's own message VECTOR is sent in order, which is the
/// only ordering FLOW-DESIGN.md actually requires), sending via the inline
/// control-priority-aware loop described in the module doc. Returns
/// `ProcOutcome::Stop` if a `stop` command was noticed while blocked
/// sending.
fn process_message(ctx: &mut ProcCtx, cid: &Value, msg: Value, error_chan: &Arc<Chan>) -> ProcOutcome {
    process_message_to(ctx, cid, msg, error_chan, &mut OutSink::Chans)
}

/// Where one `transform` call's `{out-id [msgs...]}` return goes. `Chans`
/// is the ordinary proc: every out-id is looked up in `ctx.outs` and sent
/// with control priority. `Capture` is the FUSED runner's mid-chain member
/// (P3): a fused non-tail member has EXACTLY one out port by construction,
/// and its messages are handed straight to the next member on the same
/// thread instead of through the (still-wired, still-empty) inter-member
/// chan -- which is the entire point of fusion. Out-ids other than that one
/// are dropped exactly as `Chans` drops an out-id with no `:conns` entry.
enum OutSink<'a> {
    Chans,
    Capture(&'a Value, &'a mut Vec<Value>),
}

fn process_message_to(ctx: &mut ProcCtx, cid: &Value, msg: Value, error_chan: &Arc<Chan>, sink: &mut OutSink) -> ProcOutcome {
    // THE HANDLE AUDIT (Perceus-lite phase 3), stated here because the
    // answer is a *theorem about the contract*, not an implementation
    // accident:
    //
    // - `call_args` is HANDED OVER (`call_owned`): built fresh per message,
    //   dead the instant the call returns, so its three handles move into
    //   the callee's parameter slots instead of being cloned into them.
    // - `*ctx.state` is KEPT, and cannot be released. FLOW-DESIGN.md's
    //   transform-error contract -- "the proc survives on its PREVIOUS
    //   state" -- says precisely that the pre-call state value must still
    //   exist after a failed call. A uniquely-owned handle is one the callee
    //   may destroy (that is what in-place mutation MEANS), so "recoverable
    //   after an error" and "unique inside the callee" are mutually
    //   exclusive by definition. The state map therefore cannot be a unique
    //   receiver in a `transform`, at any hop, ever -- see the
    //   `transform_error_keeps_state` tests.
    // - `msg` is KEPT for the same reason: the error map's `:msg` key
    //   reports the message that failed, so it must outlive a failed call.
    // - `prev_state` is GONE. It was a second recovery handle -- an extra
    //   `Value` clone per message, plus another on every error path -- and a
    //   redundant one: nothing between here and the error paths writes
    //   `*ctx.state` (the interpreter cannot reach it; `apply_control` runs
    //   only later), so `*ctx.state = prev_state.clone()` was assigning a
    //   value its own destination already held. `report_error` reads
    //   `ctx.state` and so sees the identical value it saw before.
    //
    // A STACK array, handed over by `call_with_buf`: a `Vec` here cost a
    // malloc/free pair per message, which `bench/flow-gen-sink.mova` (whose
    // state is a map LITERAL, so it makes no consuming call at all and can
    // only lose) measured at ~6%.
    let mut call_args = [ctx.state.clone(), cid.clone(), msg.clone()];
    let step_fn = ctx.step_fn.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.interp.call_with_buf(&step_fn, &mut call_args)
    }));

    let (new_state, outputs) = match result {
        // S7: `| Value::MapEntry(v)` -- a step fn that returns a map
        // entry as its `[state outs]` pair is vector-shaped like any other.
        Ok(Ok(Value::Vector(v))) | Ok(Ok(Value::List(v))) | Ok(Ok(Value::MapEntry(v))) if v.len() == 2 => {
            (v[0].clone(), v[1].clone())
        }
        Ok(Ok(other)) => {
            // `*ctx.state` is untouched, i.e. still the previous state.
            report_error(ctx, error_chan, cid, &msg, format!(
                "transform must return [state' {{out-id [msgs...]}}], got {}",
                crate::printer::pr_str(&other)
            ));
            return ProcOutcome::Continue;
        }
        Ok(Err(e)) => {
            report_error(ctx, error_chan, cid, &msg, error_message(&e));
            return ProcOutcome::Continue;
        }
        Err(payload) => {
            let message = panic_message(reraise_if_killed(payload));
            ctx.interp.stack.clear();
            report_error(ctx, error_chan, cid, &msg, format!("panic: {message}"));
            return ProcOutcome::Continue;
        }
    };
    if map_probe::enabled() {
        let unchanged = match (&*ctx.state, &new_state) {
            (Value::Map(a), Value::Map(b)) => PMap::ptr_eq(a, b),
            _ => false,
        };
        map_probe::record_state_identity(unchanged);
    }
    *ctx.state = new_state;
    *ctx.count += 1;

    let Value::Map(outs_map) = &outputs else {
        return ProcOutcome::Continue;
    };
    if let OutSink::Capture(port, buf) = sink {
        if let Some(msgs_val) = outs_map.get(port) {
            match msgs_val {
                Value::Vector(v) | Value::List(v) | Value::MapEntry(v) => buf.extend(v.iter_cloned()),
                other => buf.push(other.clone()),
            }
        }
        // Design Part 2: a fused MID-chain member is transport-unaware by
        // construction (it only ever forwards to the run's NEXT member,
        // captured above into `buf`), but its transform can still return
        // `::flow/report`/`::flow/error` entries -- those are not pipeline
        // output, they go straight to the flow's real chans, same as they
        // would if this member were unfused. Route them BEFORE the early
        // return below, or they are silently dropped (the generic loop's
        // `for (out_id, ..) in outs_map` never runs for a `Capture` sink).
        if matches!(route_reserved_outs(ctx, outs_map), ProcOutcome::Stop) {
            return ProcOutcome::Stop;
        }
        return ProcOutcome::Continue;
    }
    for (out_id, msgs_val) in outs_map.iter() {
        let Some(Some(chan)) = ctx.outs.get(out_id) else { continue };
        // W4 diet: iterate the returned message vector IN PLACE instead of
        // collecting it into a per-message `Vec<Value>` first (measured at
        // ~1.6 allocs/msg, bench/RESULTS-w4-alloc-attrib.md). The only
        // reason the collect existed was the borrow of `ctx.outs` across
        // `apply_control(ctx, ..)` below -- cloning the `Arc<Chan>` handle
        // (a refcount bump, no allocation) releases that borrow instead.
        // Same messages, same order, same control-priority behavior.
        let chan = chan.clone();
        match msgs_val {
            Value::Vector(v) | Value::List(v) | Value::MapEntry(v) => {
                for out_msg in v.iter() {
                    if matches!(send_one_with_control(ctx, &chan, out_msg), ProcOutcome::Stop) {
                        return ProcOutcome::Stop;
                    }
                }
            }
            other => {
                if matches!(send_one_with_control(ctx, &chan, other), ProcOutcome::Stop) {
                    return ProcOutcome::Stop;
                }
            }
        }
    }
    ProcOutcome::Continue
}

/// Routes `{::flow/report [...]}` / `{::flow/error [...]}` entries of a
/// transform's return map to their real chans -- the same
/// `send_one_with_control` control-priority send the generic dispatch loop
/// above gives every other out-id, extracted here so [`OutSink::Capture`]
/// (whose early return skips that loop entirely) can call it too. Both
/// reserved keys are always present in `ctx.outs` after `flow/start`
/// (design Part 2), so a lookup miss here only ever means the transform
/// didn't emit that key this message.
fn route_reserved_outs(ctx: &mut ProcCtx, outs_map: &PMap) -> ProcOutcome {
    for key in [report_out_key(), error_out_key()] {
        let Some(msgs_val) = outs_map.get(&key) else { continue };
        let Some(Some(chan)) = ctx.outs.get(&key) else { continue };
        let chan = chan.clone();
        match msgs_val {
            Value::Vector(v) | Value::List(v) | Value::MapEntry(v) => {
                for out_msg in v.iter() {
                    if matches!(send_one_with_control(ctx, &chan, out_msg), ProcOutcome::Stop) {
                        return ProcOutcome::Stop;
                    }
                }
            }
            other => {
                if matches!(send_one_with_control(ctx, &chan, other), ProcOutcome::Stop) {
                    return ProcOutcome::Stop;
                }
            }
        }
    }
    ProcOutcome::Continue
}

/// One message's control-priority send loop, extracted verbatim from
/// `process_message_to`'s former inline `for out_msg in msgs` body: retry
/// the send until it lands (`Sent`/`Closed` both mean "move on"), applying
/// any control command noticed while blocked; `Stop` propagates.
fn send_one_with_control(ctx: &mut ProcCtx, chan: &Arc<Chan>, out_msg: &Value) -> ProcOutcome {
    loop {
        match out_send(ctx.out_lane, chan, out_msg, ctx.control, ctx.doorbell) {
            SendOutcome::Sent | SendOutcome::Closed => return ProcOutcome::Continue,
            SendOutcome::Control(cmd) => {
                if apply_control(ctx, cmd) {
                    return ProcOutcome::Stop;
                }
            }
        }
    }
}

/// Builds and non-blocking-sends (sliding-100, so this never blocks) the
/// upstream-shaped `#::flow{:pid :status :state :count :cid :msg :op :step
/// :ex}` error map (state was already restored to the pre-transform value
/// by the caller before this runs -- "KEEP previous state" per
/// FLOW-DESIGN.md). `:ex` is our own documented shape (upstream's exact
/// internal representation isn't pinned by FLOW-DESIGN.md's recon notes):
/// `{:message "..."}`, a string description of the `RjError`/bad-return/
/// panic that was caught.
fn report_error(ctx: &ProcCtx, error_chan: &Arc<Chan>, cid: &Value, msg: &Value, message: String) {
    report_error_params(
        error_chan,
        ErrorParams {
            pid: ctx.pid,
            run_status: *ctx.run_status,
            state: ctx.state,
            count: *ctx.count,
            cid,
            msg,
            step_fn: ctx.step_fn,
            message,
        },
    );
}

/// Everything a transform-error report is built from -- the params-taking
/// sibling of [`report_error`]'s `ProcCtx`, shared with the promoted fast
/// loop (N2), whose `state` comes from `FastStep::snapshot()` taken AFTER
/// the failed transform (the `FastStep` contract makes that the KEPT
/// PREVIOUS state -- no mutation happens until the last fallible operation
/// has succeeded).
struct ErrorParams<'a> {
    pid: &'a Value,
    run_status: RunStatus,
    state: &'a Value,
    count: u64,
    cid: &'a Value,
    msg: &'a Value,
    step_fn: &'a Value,
    message: String,
}

fn report_error_params(error_chan: &Arc<Chan>, p: ErrorParams) {
    let mut ex = PMap::new();
    ex.insert(plain_kw("message"), Value::Str(Str::from(p.message)));
    let mut m = PMap::new();
    m.insert(kw("pid"), p.pid.clone());
    m.insert(kw("status"), status_keyword(p.run_status));
    m.insert(kw("state"), p.state.clone());
    m.insert(kw("count"), Value::Int(p.count as i64));
    m.insert(kw("cid"), p.cid.clone());
    m.insert(kw("msg"), p.msg.clone());
    // Upstream's transform-error catch (impl.clj's `run` loop) hard-codes
    // the LITERAL value `:step` for `:op` on every transform-time error
    // (it has no other `:op` values at all -- lifecycle-transition errors
    // outside `transform` carry no `:op` key whatsoever there). Matching
    // that literal value (not the previously-used `:transform`, which no
    // real core.async.flow error map ever actually carries) is a
    // conformance fix found via tests/conformance/corpus/flow.corpus's
    // error-chan scenario against the real JVM engine.
    m.insert(kw("op"), plain_kw("step"));
    m.insert(kw("step"), p.step_fn.clone());
    m.insert(kw("ex"), Value::Map(ex));
    chan_put(error_chan, Value::Map(m));
}

/// A proc that has been through `init` (and had any `::flow/in-ports`/
/// `out-ports` merged) but has not started reading yet -- the split point
/// between [`init_proc`] and [`run_ready`]. It exists because P3's fused
/// runner has to run every member's `init` FIRST (that's when
/// `::flow/in-ports`/`out-ports` appear, and therefore when a fusion
/// decision made from DECLARED ports can turn out to be wrong) and then,
/// if it must demote, hand each member to the ordinary proc loop WITHOUT
/// re-running its `init` -- an `init` may have spawned a feeder thread or
/// taken a lock, and running it twice is not an option.
struct ProcReady {
    interp: Interp,
    pid: Value,
    step_fn: Value,
    state: Value,
    ins: HashMap<Value, Arc<Chan>>,
    outs: HashMap<Value, Option<Arc<Chan>>>,
    /// Carried through `init` untouched. `init` is exactly where a lane can
    /// stop applying (an `::flow/in-ports`/`out-ports` entry replaces the
    /// wired chan with a caller's own), which is why the lanes are matched
    /// by chan IDENTITY at use rather than by port id at wiring -- see
    /// [`InLane::chan`].
    in_lane: Option<InLane>,
    out_lane: Option<OutLane>,
    control: Arc<Chan>,
    error_chan: Arc<Chan>,
}

fn run_proc(spawn: ProcSpawn) -> ExitReason {
    run_ready(init_proc(spawn))
}

fn init_proc(spawn: ProcSpawn) -> ProcReady {
    let ProcSpawn { mut interp, pid, step_fn, args, mut ins, mut outs, in_lane, out_lane, control, error_chan } = spawn;

    // Design Part 2 hardening: `outs` already carries the engine's own
    // `::flow/report`/`::flow/error` wiring by the time `ProcSpawn` is
    // built (`native_start`'s `outs.insert(report_out_key(), ...)` right
    // before `outs_by_pid.insert`) -- snapshot those two entries NOW,
    // before the `::flow/out-ports` merge below gets a chance to
    // overwrite them with whatever `init` returned. Invariant: reserved
    // out-targets are engine-owned; a user mapping under those keys is
    // ignored (see the re-insert right after the merge, and
    // `is_reserved_out_key`'s own doc).
    let reserved_outs: Vec<(Value, Option<Arc<Chan>>)> =
        outs.iter().filter(|(k, _)| is_reserved_out_key(k)).map(|(k, v)| (k.clone(), v.clone())).collect();

    let mut init_arg = match &args {
        Value::Map(m) => m.clone(),
        _ => PMap::new(),
    };
    init_arg.insert(kw("pid"), pid.clone());
    let init_call = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| interp.call(&step_fn, &[Value::Map(init_arg)])));
    let state = match init_call {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            emit_lifecycle_error(&error_chan, &pid, "init", error_message(&e));
            Value::Nil
        }
        Err(payload) => {
            emit_lifecycle_error(&error_chan, &pid, "init", panic_message(reraise_if_killed(payload)));
            interp.stack.clear();
            Value::Nil
        }
    };
    if let Value::Map(m) = &state {
        if let Some(Value::Map(extra_ins)) = m.get(&kw("in-ports")) {
            for (k, v) in extra_ins.iter() {
                if let Value::Channel(c) = v {
                    ins.insert(k.clone(), c.clone());
                }
            }
        }
        if let Some(Value::Map(extra_outs)) = m.get(&kw("out-ports")) {
            for (k, v) in extra_outs.iter() {
                if let Value::Channel(c) = v {
                    outs.insert(k.clone(), Some(c.clone()));
                }
            }
        }
    }
    // Invariant: reserved out-targets are engine-owned; a user mapping
    // under those keys is ignored -- re-assert the snapshot taken above
    // so engine wiring always wins over whatever `::flow/out-ports` just
    // merged in, no matter what `init` returned. A transform's own
    // `{::flow/report [...]}` entries still route to the flow's real
    // `:report-chan` exactly as before; a user-supplied channel under
    // `::flow/report`/`::flow/error` simply never gets wired in and so
    // receives nothing.
    for (k, v) in reserved_outs {
        outs.insert(k, v);
    }

    ProcReady { interp, pid, step_fn, state, ins, outs, in_lane, out_lane, control, error_chan }
}

fn run_ready(ready: ProcReady) -> ExitReason {
    let ProcReady { mut interp, pid, step_fn, mut state, mut ins, outs, in_lane, out_lane, control, error_chan } =
        ready;

    // One `Doorbell` for this proc's whole life (see the module doc and
    // `value.rs`'s `Doorbell` doc): registered on `control` right away, and
    // kept in sync with `read_set` (which doubles as the injection port's
    // registration -- see `resync_read_set_doorbell`'s doc) by every
    // `recompute_read_set_wired` call below.
    let doorbell = Arc::new(Doorbell::new());
    lock_mutex(&control.state).doorbell = Some(doorbell.clone());

    let mut run_status = RunStatus::Paused;
    let mut count: u64 = 0;
    let mut input_filter = extract_input_filter(&state);
    let mut read_set = recompute_read_set(&ins, input_filter.as_ref(), &mut interp);
    resync_read_set_doorbell(&[], &read_set, &doorbell);
    let mut rr_index: usize = 0;

    // N2 promotion hook (NATIVE-STEP-DESIGN.md): ONE capability question,
    // asked once per proc, right here -- after `init` has run, after
    // `::flow/in-ports`/`out-ports` have been merged, and after the first
    // read-set computation, so everything the decision depends on is final.
    // A `None` answer is never an error: the proc simply runs the generic
    // loop below, unchanged.
    if let Some(fast) = try_promote_fast(&step_fn, &state, &read_set, input_filter.as_ref(), &outs) {
        let (cid, in_chan) = read_set[0].clone();
        // The promote tail-call: this proc's exit reason IS whatever
        // `run_proc_fast` returns from here on (docs/L4-LANDING-SPEC.md
        // §W1.2).
        return run_proc_fast(FastSpawn {
            interp,
            pid,
            step_fn,
            fast,
            cid,
            in_chan,
            ins,
            outs,
            in_lane,
            out_lane,
            control,
            error_chan,
            doorbell,
        });
    }

    // W4 diet: ONE batch buffer for the life of the proc (`drain` empties
    // it each lap, capacity is retained) instead of a fresh
    // `Vec::with_capacity(1)` per drained batch.
    let mut batch: Vec<Value> = Vec::new();
    // L4 W1: the loop is now an EXPRESSION -- every `break 'outer` below
    // carries the `ExitReason` this proc exited with, so the loop's value
    // (and therefore this fn's return value, as its unadorned tail
    // expression) is exactly that reason. See docs/L4-LANDING-SPEC.md
    // §W1.2's exit-site table for the mapping each site below implements.
    'outer: loop {
        // THE lap snapshot, taken before ANY of this lap's non-blocking
        // scans -- the control check below, the single-input take, and the
        // multi-input round-robin all sit inside the window it opens. See
        // [`try_take_with_timeout`]'s doc for why it has to be here and not
        // inside the park primitive: on a task proc the park has no timeout
        // to fall back on, so a ring that lands between a scan and a
        // too-late snapshot is a permanent park, not a slow lap.
        let seen = doorbell.current();
        if run_status == RunStatus::Paused || read_set.is_empty() {
            // Control chan found closed: `Normal` -- the flow is dissolving
            // around this (idle/paused) proc, not this proc choosing to
            // leave (see `ExitReason::Normal`'s doc).
            let Some(cmd) = chan_take(&control) else { break 'outer ExitReason::Normal };
            let mut ctx = ProcCtx {
                interp: &mut interp,
                pid: &pid,
                step_fn: &step_fn,
                state: &mut state,
                count: &mut count,
                run_status: &mut run_status,
                ins: &ins,
                outs: &outs,
                out_lane: out_lane.as_ref(),
                control: &control,
                doorbell: &doorbell,
            };
            if apply_control(&mut ctx, cmd) {
                break 'outer ExitReason::Stopped;
            }
            input_filter = extract_input_filter(&state);
            read_set = recompute_read_set_wired(&ins, input_filter.as_ref(), &mut interp, &doorbell, &read_set);
            continue 'outer;
        }

        if let TryTake::Received(cmd) = chan_try_take(&control) {
            let mut ctx = ProcCtx {
                interp: &mut interp,
                pid: &pid,
                step_fn: &step_fn,
                state: &mut state,
                count: &mut count,
                run_status: &mut run_status,
                ins: &ins,
                outs: &outs,
                out_lane: out_lane.as_ref(),
                control: &control,
                doorbell: &doorbell,
            };
            if apply_control(&mut ctx, cmd) {
                break 'outer ExitReason::Stopped;
            }
            input_filter = extract_input_filter(&state);
            read_set = recompute_read_set_wired(&ins, input_filter.as_ref(), &mut interp, &doorbell, &read_set);
            continue 'outer;
        }

        if read_set.len() == 1 {
            let (cid, chan) = read_set[0].clone();
            // Resolved ONCE per outer lap, not per message: the lane
            // applies iff this is the very chan it was wired for.
            let lane = in_lane_for(in_lane.as_ref(), &chan);
            match in_take_timeout(lane, &chan, &doorbell, seen, PARK_TIMEOUT) {
                TryTake::Received(first) => {
                    batch.clear();
                    batch.push(first);
                    drain_batch(lane, &chan, &mut batch);
                    for (i, msg) in batch.drain(..).enumerate() {
                        if i > 0 && i % CONTROL_CHECK_EVERY == 0 {
                            if let TryTake::Received(cmd) = chan_try_take(&control) {
                                let mut ctx = ProcCtx {
                                    interp: &mut interp,
                                    pid: &pid,
                                    step_fn: &step_fn,
                                    state: &mut state,
                                    count: &mut count,
                                    run_status: &mut run_status,
                                    ins: &ins,
                                    outs: &outs,
                                    out_lane: out_lane.as_ref(),
                                    control: &control,
                                    doorbell: &doorbell,
                                };
                                if apply_control(&mut ctx, cmd) {
                                    break 'outer ExitReason::Stopped;
                                }
                            }
                        }
                        let mut ctx = ProcCtx {
                            interp: &mut interp,
                            pid: &pid,
                            step_fn: &step_fn,
                            state: &mut state,
                            count: &mut count,
                            run_status: &mut run_status,
                            ins: &ins,
                            outs: &outs,
                            out_lane: out_lane.as_ref(),
                            control: &control,
                            doorbell: &doorbell,
                        };
                        if matches!(process_message(&mut ctx, &cid, msg, &error_chan), ProcOutcome::Stop) {
                            break 'outer ExitReason::Stopped;
                        }
                    }
                    // W4 diet: with NO input filter in play (neither before
                    // this batch nor in the new state), the read-set is a
                    // pure function of `ins`, which nothing in a batch can
                    // change (a `Closed` take is handled in its own arm
                    // below) -- so the per-batch rebuild (a `Vec` + clones +
                    // a `pr_str` sort) is skipped. Any filter, appearing OR
                    // disappearing, recomputes exactly as before.
                    let new_filter = extract_input_filter(&state);
                    if input_filter.is_some() || new_filter.is_some() {
                        input_filter = new_filter;
                        read_set = recompute_read_set_wired(&ins, input_filter.as_ref(), &mut interp, &doorbell, &read_set);
                    }
                }
                TryTake::Closed => {
                    ins.remove(&cid);
                    read_set = recompute_read_set_wired(&ins, input_filter.as_ref(), &mut interp, &doorbell, &read_set);
                }
                TryTake::WouldBlock => {}
            }
        } else {
            let n = read_set.len();
            let mut acted = false;
            for i in 0..n {
                let idx = (rr_index + i) % n;
                let (cid, chan) = read_set[idx].clone();
                match in_try_take(in_lane_for(in_lane.as_ref(), &chan), &chan) {
                    TryTake::Received(msg) => {
                        rr_index = (idx + 1) % n;
                        let mut ctx = ProcCtx {
                            interp: &mut interp,
                            pid: &pid,
                            step_fn: &step_fn,
                            state: &mut state,
                            count: &mut count,
                            run_status: &mut run_status,
                            ins: &ins,
                            outs: &outs,
                            out_lane: out_lane.as_ref(),
                            control: &control,
                            doorbell: &doorbell,
                        };
                        let outcome = process_message(&mut ctx, &cid, msg, &error_chan);
                        acted = true;
                        if matches!(outcome, ProcOutcome::Stop) {
                            break 'outer ExitReason::Stopped;
                        }
                        input_filter = extract_input_filter(&state);
                        read_set = recompute_read_set_wired(&ins, input_filter.as_ref(), &mut interp, &doorbell, &read_set);
                        break;
                    }
                    TryTake::Closed => {
                        ins.remove(&cid);
                        read_set = recompute_read_set_wired(&ins, input_filter.as_ref(), &mut interp, &doorbell, &read_set);
                        acted = true;
                        break;
                    }
                    TryTake::WouldBlock => {}
                }
            }
            if !acted {
                // Snapshot-before-scan, park-on-doorbell (module doc):
                // `seen` was taken at the top of this lap -- before the
                // control check AND before this whole round-robin pass -- so
                // a ring from ANY chan just scanned, or from the control
                // chan, that lands after it is never missed:
                // `wait_for_change` simply returns immediately instead of
                // blocking, and the next outer lap re-scans. `PARK_TIMEOUT`
                // is a safety net for the thread arm only (the task arm has
                // none), which is exactly why the snapshot's position is
                // load-bearing -- see [`try_take_with_timeout`]'s doc.
                //
                // L3.6/W1: ...but the doorbell alone is NOT a complete wake
                // source when one of the scanned ports carries a transport
                // lane, because that port's engine traffic arrives on a
                // lock-free ring that rings nothing. A proc reaches this
                // branch holding a lane whenever its `init` widened the read
                // set past its declared ports (`::flow/in-ports`), which the
                // planner cannot see. So park on BOTH -- see
                // [`InLane::wait_readable`], which is where the whole
                // argument (and the hang it fixes) is written down. At most
                // one lane per proc, so `find_map` finds the only candidate.
                match read_set.iter().find_map(|(_, ch)| in_lane_for(in_lane.as_ref(), ch)) {
                    Some(l) => l.wait_readable(&doorbell, seen, PARK_TIMEOUT),
                    None => {
                        doorbell.wait_for_change(seen, PARK_TIMEOUT);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Promoted (native-step) proc loop -- N2, see NATIVE-STEP-DESIGN.md
//
// Structurally a stripped copy of `run_proc`'s single-input branch above:
// same park primitive, same PARK_TIMEOUT/BATCH_DRAIN_MAX/CONTROL_CHECK_EVERY
// constants, same control-check points, same per-message `catch_unwind`,
// same keep-previous-state-on-error contract, same error/ping map builders
// (`report_error_params`/`build_ping_reply_params`, shared verbatim). What
// it drops is everything that only exists to talk to an INTERPRETED step-fn:
// the `Interp::call` per message, the `[state' {out-id [msgs...]}]` return
// parsing, the `{out-id -> chan}` lookup per message (a promoted proc has
// exactly ONE, pre-resolved out chan), and the per-batch read-set recompute
// (see `run_proc_fast`'s comment for what licenses that hoist).
// ---------------------------------------------------------------------------

/// True when `MOVA_NO_FASTSTEP=1` was set in the environment at process
/// start -- the native-step tier's kill switch, read exactly ONCE (mirroring
/// `crate::compile::disabled_by_env`'s `OnceLock` treatment of
/// `MOVA_NO_COMPILE`) so no proc spawn ever touches the environment. With
/// it set, every proc runs the generic loop; the promoted steps still work
/// (their `NativeFn` shell implements all four arities through the same
/// `FastStep` code), just via the interpreter-facing path -- which is
/// exactly what makes this an honest A/B of the fast path itself.
fn faststep_disabled_by_env() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_NO_FASTSTEP").is_ok_and(|v| v == "1"))
}

/// The promotion decision, in full. Every condition is a structural one
/// `run_proc_fast` then relies on: a native step-fn carrying a
/// `StepFactory`; EXACTLY one input (so the batch-drain park primitive
/// applies and there's no round-robin to do); no `::flow/input-filter` (so
/// the read set can never change under us); AT MOST one DECLARED out port
/// (so `FastOut` needs no out-id -- the two engine-reserved out-targets,
/// design Part 2, are excluded from this count via [`real_outs`]: they are
/// always present in `outs` now but a native `FastStep`'s `transform`
/// returns a bare `FastOut`, which is positional and structurally CANNOT
/// address `::flow/report`/`::flow/error` -- there is no out-id to put one
/// in. That is what lets N2 promotion stay decided purely off the proc's
/// own declared ports, unchanged by Part 2); and a factory that recognizes
/// this proc's init state. Any `None` here means "run the generic loop" --
/// never an error, never a warning.
fn try_promote_fast(
    step_fn: &Value,
    state: &Value,
    read_set: &[(Value, Arc<Chan>)],
    input_filter: Option<&Value>,
    outs: &HashMap<Value, Option<Arc<Chan>>>,
) -> Option<Box<dyn FastStep>> {
    if faststep_disabled_by_env() {
        return None;
    }
    let Value::Native(n) = step_fn else { return None };
    let factory = n.step.as_ref()?;
    if read_set.len() != 1 || input_filter.is_some() || real_outs(outs).count() > 1 {
        return None;
    }
    factory.instantiate(state)
}

/// Everything `run_proc_fast` needs, bundled for the same reason
/// [`ProcSpawn`] is.
struct FastSpawn {
    interp: Interp,
    pid: Value,
    step_fn: Value,
    fast: Box<dyn FastStep>,
    cid: Value,
    in_chan: Arc<Chan>,
    ins: HashMap<Value, Arc<Chan>>,
    outs: HashMap<Value, Option<Arc<Chan>>>,
    in_lane: Option<InLane>,
    out_lane: Option<OutLane>,
    control: Arc<Chan>,
    error_chan: Arc<Chan>,
    /// This proc's `Doorbell`, created by `run_ready` and already
    /// registered on `control` and `in_chan` (its read-set is exactly
    /// `{in_chan}`, which never changes -- see the module doc's N2
    /// promotion section) before the promotion decision was made.
    doorbell: Arc<Doorbell>,
}

/// The promoted proc's MUTABLE state -- the fast counterpart of the
/// generic loop's `state`/`count`/`run_status`/`ins` locals (the step's own
/// state now lives inside `fast`, owned in Rust, and is only ever
/// materialized as a `Value` by `snapshot()` for a ping/error reply).
struct FastState {
    fast: Box<dyn FastStep>,
    count: u64,
    run_status: RunStatus,
    /// Kept (and mutated on close) purely so a `ping` reply's `::flow/ins`
    /// vector matches the generic loop's byte for byte -- that loop drops a
    /// closed in-port from its own `ins` map before parking.
    ins: HashMap<Value, Arc<Chan>>,
}

/// The promoted proc's IMMUTABLE per-run context, built once outside the
/// loop (nothing here can change: no read-set recompute, no out-port
/// rebinding, one cid for the proc's entire life).
struct FastEnv<'a> {
    pid: &'a Value,
    step_fn: &'a Value,
    cid: &'a Value,
    /// The single pre-resolved out chan, or `None` = "drop output
    /// silently" -- either because the step declares no out port at all, or
    /// because its out port has no `:conns` entry (the generic loop's
    /// `let Some(Some(chan)) = ctx.outs.get(out_id) else { continue }`).
    out_chan: Option<Arc<Chan>>,
    outs: &'a HashMap<Value, Option<Arc<Chan>>>,
    /// This proc's out-port transport lane, if any -- pre-resolved against
    /// `out_chan` exactly once, outside the loop, for the same reason
    /// everything else in `FastEnv` is.
    out_lane: Option<&'a OutLane>,
    control: &'a Arc<Chan>,
    error_chan: &'a Arc<Chan>,
    /// As `ProcCtx::doorbell`: the proc's own doorbell, carried for
    /// [`send_with_control_priority`]'s task arm alone (L3/W1b).
    doorbell: &'a Arc<Doorbell>,
}

/// Mirrors [`apply_control`] exactly -- transition first, then status
/// change; `stop` returns `true` for the caller to exit; `ping` replies
/// through the SAME `#::flow{:pid :status :count :state :ins :outs}`
/// builder, with `:state` coming from `FastStep::snapshot()` (the exact
/// Value the generic shell would be holding at this moment).
fn apply_control_fast(st: &mut FastState, env: &FastEnv, cmd: Value) -> bool {
    let Value::Map(m) = &cmd else { return false };
    let Some(Value::Keyword(op)) = m.get(&kw("op")) else { return false };
    match op.as_ref() {
        "resume" => {
            st.fast.transition(StepTransition::Resume);
            st.run_status = RunStatus::Running;
            false
        }
        "pause" => {
            st.fast.transition(StepTransition::Pause);
            st.run_status = RunStatus::Paused;
            false
        }
        "stop" => {
            st.fast.transition(StepTransition::Stop);
            true
        }
        "ping" => {
            if let Some(Value::Channel(reply)) = m.get(&kw("reply-chan")) {
                let snapshot = st.fast.snapshot();
                chan_put(
                    reply,
                    build_ping_reply_params(PingParams {
                        pid: env.pid,
                        run_status: st.run_status,
                        count: st.count,
                        state: &snapshot,
                        ins: &st.ins,
                        outs: env.outs,
                    }),
                );
            }
            false
        }
        _ => false,
    }
}

/// One promoted message: `catch_unwind`-guarded `FastStep::transform`
/// (kept per-message, exactly as on the generic path -- a panic must not
/// take down the messages after it), then the `FastOut` straight to the one
/// pre-resolved out chan. On EITHER failure mode the error map's `:state`
/// is `snapshot()` taken AFTER the failed transform: the `FastStep`
/// contract ("no internal-state mutation until after the last fallible
/// operation succeeds") makes that the KEPT PREVIOUS state, structurally --
/// there is no `prev_state.clone()` per message here at all.
fn process_message_fast(st: &mut FastState, interp: &mut Interp, env: &FastEnv, msg: Value) -> ProcOutcome {
    let result = {
        let fast = &mut st.fast;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fast.transform(interp, &msg)))
    };
    let out = match result {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            report_transform_error_fast(st, env, &msg, error_message(&e));
            return ProcOutcome::Continue;
        }
        Err(payload) => {
            let message = panic_message(reraise_if_killed(payload));
            interp.stack.clear();
            report_transform_error_fast(st, env, &msg, format!("panic: {message}"));
            return ProcOutcome::Continue;
        }
    };
    st.count += 1;
    let Some(chan) = env.out_chan.as_ref() else { return ProcOutcome::Continue };
    match out {
        FastOut::None => ProcOutcome::Continue,
        FastOut::One(v) => send_fast(st, env, chan, &v),
        FastOut::Many(vs) => {
            for v in vs {
                if matches!(send_fast(st, env, chan, &v), ProcOutcome::Stop) {
                    return ProcOutcome::Stop;
                }
            }
            ProcOutcome::Continue
        }
    }
}

fn report_transform_error_fast(st: &FastState, env: &FastEnv, msg: &Value, message: String) {
    let snapshot = st.fast.snapshot();
    report_error_params(
        env.error_chan,
        ErrorParams {
            pid: env.pid,
            run_status: st.run_status,
            state: &snapshot,
            count: st.count,
            cid: env.cid,
            msg,
            step_fn: env.step_fn,
            message,
        },
    );
}

/// One output message onto the pre-resolved out chan, via the shared
/// [`send_with_control_priority`] loop (control noticed mid-send is applied
/// here, then the send is retried -- identical to `process_message`).
fn send_fast(st: &mut FastState, env: &FastEnv, chan: &Arc<Chan>, v: &Value) -> ProcOutcome {
    loop {
        match out_send(env.out_lane, chan, v, env.control, env.doorbell) {
            SendOutcome::Sent | SendOutcome::Closed => return ProcOutcome::Continue,
            SendOutcome::Control(cmd) => {
                if apply_control_fast(st, env, cmd) {
                    return ProcOutcome::Stop;
                }
            }
        }
    }
}

fn run_proc_fast(spawn: FastSpawn) -> ExitReason {
    let FastSpawn {
        mut interp,
        pid,
        step_fn,
        fast,
        cid,
        in_chan,
        ins,
        outs,
        in_lane,
        out_lane,
        control,
        error_chan,
        doorbell,
    } = spawn;
    // L5 fence #9b (design §4): `.next()` on `real_outs` is hash order,
    // which is only deterministic because `try_promote_fast` refused any
    // proc with more than one DECLARED out port -- `real_outs` excludes
    // the two engine-reserved out-targets (design Part 2), which are
    // always present in `outs` now but structurally unreachable from a
    // `FastStep::transform`'s bare (out-id-less) `FastOut` return. Pinned,
    // not assumed.
    debug_assert!(
        real_outs(&outs).count() <= 1,
        "fence #9b: a promoted proc has at most one declared out port, got {}",
        real_outs(&outs).count()
    );
    let out_chan = real_outs(&outs).next().and_then(|(_, v)| v.clone());
    // Both lanes resolved once, here, against the very chans this loop will
    // use for the rest of the proc's life -- a promoted proc's read set and
    // out chan can never change (see the batch-drain comment below), so the
    // identity test cannot become stale.
    let out_lane_ref = out_chan.as_ref().and_then(|c| out_lane_for(out_lane.as_ref(), c));
    let in_lane_ref = in_lane_for(in_lane.as_ref(), &in_chan);
    let env = FastEnv {
        pid: &pid,
        step_fn: &step_fn,
        cid: &cid,
        out_chan: out_chan.clone(),
        outs: &outs,
        out_lane: out_lane_ref,
        control: &control,
        error_chan: &error_chan,
        doorbell: &doorbell,
    };
    let mut st = FastState { fast, count: 0, run_status: RunStatus::Paused, ins };
    // The promoted proc's one input closed: nothing can ever arrive again,
    // so park on control only (the generic loop reaches the same state by
    // way of an empty read set).
    let mut in_closed = false;

    // L4 W1: same "loop as `ExitReason`-valued expression" shape as
    // `run_ready` -- see that fn's comment.
    'outer: loop {
        // THE lap snapshot -- before this lap's control check and before its
        // take, for the reason [`try_take_with_timeout`]'s doc gives (the
        // generic loop takes the identical one at the same place).
        let seen = doorbell.current();
        if st.run_status == RunStatus::Paused || in_closed {
            let Some(cmd) = chan_take(&control) else { break 'outer ExitReason::Normal };
            if apply_control_fast(&mut st, &env, cmd) {
                break 'outer ExitReason::Stopped;
            }
            continue 'outer;
        }

        if let TryTake::Received(cmd) = chan_try_take(&control) {
            if apply_control_fast(&mut st, &env, cmd) {
                break 'outer ExitReason::Stopped;
            }
            continue 'outer;
        }

        match in_take_timeout(in_lane_ref, &in_chan, &doorbell, seen, PARK_TIMEOUT) {
            TryTake::Received(first) => {
                let mut batch = Vec::with_capacity(1);
                batch.push(first);
                drain_batch(in_lane_ref, &in_chan, &mut batch);
                for (i, msg) in batch.into_iter().enumerate() {
                    if i > 0 && i % CONTROL_CHECK_EVERY == 0 {
                        if let TryTake::Received(cmd) = chan_try_take(&control) {
                            if apply_control_fast(&mut st, &env, cmd) {
                                break 'outer ExitReason::Stopped;
                            }
                        }
                    }
                    if matches!(process_message_fast(&mut st, &mut interp, &env, msg), ProcOutcome::Stop) {
                        break 'outer ExitReason::Stopped;
                    }
                }
                // DELIBERATELY no read-set recompute here (the generic loop
                // does one per batch). The read set of a promoted proc can
                // never change: promotion required `input_filter == None`,
                // and a closed step's state is produced by its own
                // `FastStep`, which never invents a `::flow/input-filter`
                // key -- there is no code path by which one could appear
                // mid-run. Dropping the recompute is the single largest
                // per-batch saving on this path (it was an `ins` clone plus
                // a `pr_str` sort per batch).
            }
            TryTake::Closed => {
                st.ins.remove(&cid);
                in_closed = true;
            }
            TryTake::WouldBlock => {}
        }
    }
}

// ---------------------------------------------------------------------------
// P3: engine-level 1:1 chain fusion (FLOW-DESIGN.md's "Fusion" section)
//
// An unbranching run of 1:1-connected procs never needs a scheduling
// decision between stages -- there is exactly one possible next step -- so
// the whole run is collapsed onto ONE OS thread that calls each member's
// transform back-to-back per message. The inter-member chans are still
// CREATED by the wiring (identically to the unfused case) and still
// injectable into; the fused loop simply never reads or writes them on the
// hot path, which is where the win comes from (one park/wake cycle and one
// buffered handoff per hop, gone).
// ---------------------------------------------------------------------------

/// WHICH fusable runs actually get fused. Fusion is not a free win: it
/// removes an inter-member channel handoff (and that thread's park/wake
/// cycle) per hop, but it also removes the PIPELINE PARALLELISM a
/// thread-per-proc chain gets for free on a multi-core machine. Which
/// effect dominates depends entirely on how expensive one member's
/// `transform` is relative to one channel hop, and P3 measured both sides
/// (bench/optimization-log.md's P3 section has the full table):
///
/// - every member promoted onto a `FastStep` (per-message cost tens of ns,
///   so the ~1-4µs hop dominates): **+3.4x to +8.5x** -- flow-4hop-native
///   847k -> 7.4M msg/s, flow-gen-sink-native 1.60M -> 8.4M msg/s.
/// - interpreted `map->step` members (per-message cost ~0.6-1µs of
///   interpreter dispatch, which N cores were previously chewing through
///   N-at-a-time): **-14% to -35%** -- flow-11hop 240k -> 150k msg/s.
///
/// So the DEFAULT policy fuses a run only when every one of its members
/// promotes onto a `FastStep`, which is the engine's own cheap, already-
/// computed proxy for "this member's per-message work is native-small".
/// That question is asked in two halves: the cheap structural half (is the
/// step-fn a native carrying a `StepFactory` at all?) at WIRING time in
/// [`plan_fusion`], so a chain that could never qualify is never planned as
/// a run and takes the byte-identical pre-P3 spawn path; and the rest (does
/// the factory recognize this proc's init state?) at INIT time in
/// [`run_fused`], which demotes if it doesn't.
/// `MOVA_FUSE_ALL=1` fuses every topologically-fusable run regardless of
/// step kind (the right setting when cores are scarce, or per-message work
/// is small but interpreted -- and what the interpreted-chain differential
/// tests drive); `MOVA_NO_FUSION=1` is the kill switch, restoring the
/// exact pre-P3 thread-per-proc engine.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FusionPolicy {
    /// `MOVA_NO_FUSION=1`: never fuse.
    Off,
    /// Default: fuse only runs whose every member promotes onto a `FastStep`.
    PromotedOnly,
    /// `MOVA_FUSE_ALL=1`: fuse every topologically-fusable run.
    All,
}

/// Read exactly ONCE per process (an `OnceLock`, mirroring
/// `MOVA_NO_FASTSTEP`/`MOVA_NO_COMPILE`) so no proc spawn ever touches
/// the environment. `MOVA_NO_FUSION` wins over `MOVA_FUSE_ALL`.
fn fusion_policy() -> FusionPolicy {
    static FLAG: OnceLock<FusionPolicy> = OnceLock::new();
    *FLAG.get_or_init(|| {
        if std::env::var("MOVA_NO_FUSION").is_ok_and(|v| v == "1") {
            FusionPolicy::Off
        } else if std::env::var("MOVA_FUSE_ALL").is_ok_and(|v| v == "1") {
            FusionPolicy::All
        } else {
            FusionPolicy::PromotedOnly
        }
    })
}

/// The wiring-time fusion decision, in full: partitions `def`'s procs into
/// RUNS (one thread each), in `proc_order` of each run's head. A run of
/// length 1 is an ordinary proc; a longer run is fused.
///
/// B is fusable onto its predecessor A iff ALL of:
/// - the conn `[[A a-out] [B b-in]]` is genuinely 1:1 -- `a-out` has
///   exactly ONE destination and `b-in` exactly ONE source (so no mult /
///   fan-out / fan-in is ever fused, by the same reasoning the mult
///   ([`run_mult_task`] / [`run_mult_thread`]) exists at all),
/// - `b-in` is B's ONLY declared in-port,
/// - `a-out` is A's ONLY declared out-port,
/// - `A != B` (a self-loop always routes via mult).
///
/// Runs are MAXIMAL chains under that relation. Everything here is decided
/// from DECLARED (describe-time) ports and conns, because that is all that
/// exists before any proc's `init` has run; a member whose `init` then adds
/// an `::flow/in-ports`/`out-ports` that breaks the assumption is caught
/// inside [`run_fused`], which demotes the whole run (see
/// [`fusion_still_valid`]).
///
/// `pub(crate)` for the unit tests at the bottom of this file: a
/// differential can never fail if fusion silently never happens (falling
/// back to a thread per proc is always *correct*), so the plan itself is
/// what has to be asserted on.
pub(crate) fn plan_fusion(def: &FlowDef) -> Vec<Vec<Value>> {
    plan_fusion_with(def, fusion_policy())
}

/// [`plan_fusion`] with the policy passed in rather than read from the
/// process-wide `OnceLock` -- the unit tests' entry point (the real policy
/// is fixed for a process's whole life, so a test that wants to see a
/// different one has no other way in).
pub(crate) fn plan_fusion_with(def: &FlowDef, policy: FusionPolicy) -> Vec<Vec<Value>> {
    if policy == FusionPolicy::Off {
        return def.proc_order.iter().map(|p| vec![p.clone()]).collect();
    }

    let mut out_degree: HashMap<(Value, Value), usize> = HashMap::new();
    let mut in_degree: HashMap<(Value, Value), usize> = HashMap::new();
    for c in &def.conns {
        *out_degree.entry((c.from_pid.clone(), c.from_port.clone())).or_default() += 1;
        *in_degree.entry((c.to_pid.clone(), c.to_port.clone())).or_default() += 1;
    }

    let mut succ: HashMap<Value, Value> = HashMap::new();
    let mut pred: HashMap<Value, Value> = HashMap::new();
    for c in &def.conns {
        if c.from_pid == c.to_pid {
            continue;
        }
        if out_degree.get(&(c.from_pid.clone(), c.from_port.clone())).copied() != Some(1) {
            continue;
        }
        if in_degree.get(&(c.to_pid.clone(), c.to_port.clone())).copied() != Some(1) {
            continue;
        }
        let (Some(from), Some(to)) = (def.procs.get(&c.from_pid), def.procs.get(&c.to_pid)) else { continue };
        if from.outs.len() != 1 || to.ins.len() != 1 {
            continue;
        }
        // The default policy's step-kind gate, applied HERE rather than at
        // init: a run that would only demote again is better never planned
        // at all, so an interpreted chain takes the byte-identical pre-P3
        // spawn path (measured: routing one through the fused thread's
        // demote path costs ~1.6% on flow-gen-sink-w2000).
        if policy == FusionPolicy::PromotedOnly && !(carries_step_factory(from) && carries_step_factory(to)) {
            continue;
        }
        // Defensive: with the degree checks above, neither of these can
        // already be occupied (a second successor for A would mean a second
        // conn out of A's single out-port, which the out-degree check just
        // rejected) -- but a malformed cfg must never produce a fork here.
        if succ.contains_key(&c.from_pid) || pred.contains_key(&c.to_pid) {
            continue;
        }
        succ.insert(c.from_pid.clone(), c.to_pid.clone());
        pred.insert(c.to_pid.clone(), c.from_pid.clone());
    }

    let mut runs: Vec<Vec<Value>> = Vec::new();
    let mut placed: std::collections::HashSet<Value> = std::collections::HashSet::new();
    for pid in &def.proc_order {
        if pred.contains_key(pid) {
            continue; // not a head; it will be picked up by its predecessor
        }
        let mut run = vec![pid.clone()];
        placed.insert(pid.clone());
        let mut cur = pid.clone();
        while let Some(next) = succ.get(&cur) {
            if placed.contains(next) {
                break;
            }
            run.push(next.clone());
            placed.insert(next.clone());
            cur = next.clone();
        }
        runs.push(run);
    }
    // A pure CYCLE (every member has a predecessor, so the head scan above
    // never reaches it) gets no fusion at all: each member becomes its own
    // proc, exactly as before P3. Fusing a ring would make the tail's send
    // feed the run's own input, which is the self-loop shape the mult
    // deliberately keeps on a separate schedulable unit -- its own task by
    // default (`run_mult_task`), its own OS thread under
    // `MOVA_FLOW_THREAD_PROCS=1` (`run_mult_thread`). Either way it is a
    // party the proc can be blocked on WITHOUT blocking itself, which is
    // the whole point.
    for pid in &def.proc_order {
        if !placed.contains(pid) {
            runs.push(vec![pid.clone()]);
        }
    }
    runs
}

/// Does this proc's step-fn even have a chance of promoting onto a
/// `FastStep`? The cheap, wiring-time half of the default policy's "every
/// member is native-cheap per message" test -- a `Value::Native` carrying a
/// `StepFactory` (see NATIVE-STEP-DESIGN.md). The other half (does the
/// factory actually recognize this proc's init state?) can only be asked
/// after `init`, and is asked there, in [`run_fused`].
fn carries_step_factory(pdef: &ProcDef) -> bool {
    matches!(extract_step_fn(&pdef.launcher), Ok(Value::Native(n)) if n.step.is_some())
}

/// Everything [`run_fused`] needs: the run's members IN CHAIN ORDER (each
/// exactly the [`ProcSpawn`] it would have gotten as an ordinary proc --
/// its own pid, step-fn, args, wired ins/outs and control chan), plus the
/// shared error chan.
struct FusedSpawn {
    members: Vec<ProcSpawn>,
    error_chan: Arc<Chan>,
}

/// One member of a running fused chain. Each keeps its OWN pid, step value,
/// state (a `Value` for an interpreted member, a Rust-owned `FastStep` when
/// that member alone qualifies for N2 promotion), count, and status -- a
/// fused run is deliberately INDISTINGUISHABLE from an unfused one through
/// `ping`/`ping-proc`/`error-chan`, which is what the differential tests
/// assert.
struct FusedMember {
    interp: Interp,
    pid: Value,
    step_fn: Value,
    /// The interpreted member's state. Unused (and left `Nil`) when `fast`
    /// is `Some` -- a promoted member's state lives inside the `FastStep`
    /// and is materialized on demand by `snapshot()`, exactly as
    /// `run_proc_fast` does.
    state: Value,
    fast: Option<Box<dyn FastStep>>,
    count: u64,
    run_status: RunStatus,
    /// This member's single in-port id -- the `cid` every message it
    /// processes is attributed to (ping/error reports included).
    cid: Value,
    in_chan: Arc<Chan>,
    ins: HashMap<Value, Arc<Chan>>,
    outs: HashMap<Value, Option<Arc<Chan>>>,
    control: Arc<Chan>,
    /// The out-port id feeding the NEXT member (`None` for the tail, whose
    /// sends go to real chans).
    link_out: Option<Value>,
    /// Pre-resolved single out chan, used only by a PROMOTED tail (a
    /// `FastOut` carries no out-id -- same pre-resolution `run_proc_fast`
    /// does).
    out_chan: Option<Arc<Chan>>,
    /// Whether this member's `::flow/stop` transition has already run.
    stopped: bool,
    /// A clone of the RUN's single shared [`Doorbell`] (there is one per
    /// run, not one per member -- see [`run_fused`]). Carried per member for
    /// the same single consumer `ProcCtx::doorbell` serves: a blocked TAIL
    /// send from a task run registers it on the target chan (L3/W1b). It is
    /// the same `Arc` in every member, so which one a send reaches through
    /// cannot matter.
    doorbell: Arc<Doorbell>,
}

fn member_state(m: &FusedMember) -> Value {
    match &m.fast {
        Some(f) => f.snapshot(),
        None => m.state.clone(),
    }
}

fn transition_member(m: &mut FusedMember, t: StepTransition) {
    if let Some(f) = m.fast.as_mut() {
        f.transition(t);
        return;
    }
    let name = match t {
        StepTransition::Resume => "resume",
        StepTransition::Pause => "pause",
        StepTransition::Stop => "stop",
    };
    let old = std::mem::replace(&mut m.state, Value::Nil);
    m.state = call_transition(&mut m.interp, &m.step_fn, old, name);
}

/// One control command for ONE member of a fused run -- the fused
/// counterpart of [`apply_control`]/[`apply_control_fast`], handling both
/// member kinds through [`transition_member`]/[`member_state`] and sharing
/// [`build_ping_reply_params`] verbatim (a mid-chain `ping-proc` must be
/// answered from that member's own count/status/state/ports, byte-identical
/// to what the unfused proc would have replied).
///
/// ONE deliberate difference from [`apply_control`]: `stop` does NOT run the
/// transition here, it just returns `true`. The caller unwinds out of the
/// run loop and calls [`stop_all`], which runs every member's stop
/// transition in CHAIN ORDER -- deterministic, and correct no matter which
/// member's control chan the `stop` happened to be noticed on first.
fn apply_control_member(m: &mut FusedMember, cmd: Value) -> bool {
    let Value::Map(map) = &cmd else { return false };
    let Some(Value::Keyword(op)) = map.get(&kw("op")) else { return false };
    match op.as_ref() {
        "resume" => {
            transition_member(m, StepTransition::Resume);
            m.run_status = RunStatus::Running;
            false
        }
        "pause" => {
            transition_member(m, StepTransition::Pause);
            m.run_status = RunStatus::Paused;
            false
        }
        "stop" => true,
        "ping" => {
            if let Some(Value::Channel(reply)) = map.get(&kw("reply-chan")) {
                let state = member_state(m);
                chan_put(
                    reply,
                    build_ping_reply_params(PingParams {
                        pid: &m.pid,
                        run_status: m.run_status,
                        count: m.count,
                        state: &state,
                        ins: &m.ins,
                        outs: &m.outs,
                    }),
                );
            }
            false
        }
        _ => false,
    }
}

/// Services EVERY member's control chan, non-blocking, at one check point.
/// `send_with_control_priority` takes a single control chan, so a blocked
/// TAIL send monitors the TAIL's own chan (semantically, it is the tail's
/// send) -- and the run loop then re-checks all of them here, which is what
/// makes a `stop`/`pause` aimed at an upstream member still land promptly.
fn service_controls(ms: &mut [FusedMember]) -> bool {
    let mut stop = false;
    for m in ms.iter_mut() {
        if let TryTake::Received(cmd) = chan_try_take(&m.control) {
            if apply_control_member(m, cmd) {
                stop = true;
            }
        }
    }
    stop
}

fn stop_all(ms: &mut [FusedMember]) {
    for m in ms.iter_mut() {
        if !m.stopped {
            transition_member(m, StepTransition::Stop);
            m.stopped = true;
        }
    }
}

fn fused_report_error(m: &FusedMember, error_chan: &Arc<Chan>, msg: &Value, message: String) {
    let state = member_state(m);
    report_error_params(
        error_chan,
        ErrorParams {
            pid: &m.pid,
            run_status: m.run_status,
            state: &state,
            count: m.count,
            cid: &m.cid,
            msg,
            step_fn: &m.step_fn,
            message,
        },
    );
}

/// A promoted member's transform, guarded exactly as `process_message_fast`
/// guards its own (`catch_unwind` per message, error reported with THIS
/// member's pid/cid/state, previous state kept structurally by the
/// `FastStep` contract). `None` = the message was consumed by an error and
/// nothing goes downstream.
fn fused_fast_transform(m: &mut FusedMember, error_chan: &Arc<Chan>, msg: &Value) -> Option<FastOut> {
    let result = {
        let FusedMember { fast, interp, .. } = m;
        let fast = fast.as_mut()?;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fast.transform(interp, msg)))
    };
    match result {
        Ok(Ok(out)) => {
            m.count += 1;
            Some(out)
        }
        Ok(Err(e)) => {
            fused_report_error(m, error_chan, msg, error_message(&e));
            None
        }
        Err(payload) => {
            let message = panic_message(reraise_if_killed(payload));
            m.interp.stack.clear();
            fused_report_error(m, error_chan, msg, format!("panic: {message}"));
            None
        }
    }
}

/// One output message from the fused TAIL onto its real out chan, via the
/// shared [`send_with_control_priority`] loop.
fn fused_send(m: &mut FusedMember, v: &Value) -> ProcOutcome {
    loop {
        let outcome = match m.out_chan.as_deref() {
            Some(chan) => send_with_control_priority(chan, v, &m.control, &m.doorbell),
            None => return ProcOutcome::Continue,
        };
        match outcome {
            SendOutcome::Sent | SendOutcome::Closed => return ProcOutcome::Continue,
            SendOutcome::Control(cmd) => {
                if apply_control_member(m, cmd) {
                    return ProcOutcome::Stop;
                }
            }
        }
    }
}

/// One message through ONE mid-chain member: its output messages are
/// appended to `out` for the next member instead of being sent anywhere.
fn fused_process_mid(m: &mut FusedMember, error_chan: &Arc<Chan>, msg: Value, out: &mut Vec<Value>) -> ProcOutcome {
    if m.fast.is_some() {
        match fused_fast_transform(m, error_chan, &msg) {
            Some(FastOut::One(v)) => out.push(v),
            Some(FastOut::Many(vs)) => out.extend(vs),
            Some(FastOut::None) | None => {}
        }
        return ProcOutcome::Continue;
    }
    let Some(port) = m.link_out.as_ref() else { return ProcOutcome::Continue };
    let mut ctx = ProcCtx {
        interp: &mut m.interp,
        pid: &m.pid,
        step_fn: &m.step_fn,
        state: &mut m.state,
        count: &mut m.count,
        run_status: &mut m.run_status,
        ins: &m.ins,
        outs: &m.outs,
        // Always `None`: `plan_transport_links` refuses every conn
        // touching a multi-proc fused run, so this loop is entirely
        // transport-unaware by construction (module doc, "fused >
        // transport > Chan").
        out_lane: None,
        control: &m.control,
        doorbell: &m.doorbell,
    };
    process_message_to(&mut ctx, &m.cid, msg, error_chan, &mut OutSink::Capture(port, out))
}

/// One message through the run's TAIL member: sends go to real chans, with
/// control priority raced against every blocked send -- byte-identical to
/// what the unfused tail proc would do (it IS the same code for an
/// interpreted tail).
fn fused_process_tail(m: &mut FusedMember, error_chan: &Arc<Chan>, msg: Value) -> ProcOutcome {
    if m.fast.is_some() {
        let Some(out) = fused_fast_transform(m, error_chan, &msg) else { return ProcOutcome::Continue };
        if m.out_chan.is_none() {
            return ProcOutcome::Continue;
        }
        return match out {
            FastOut::None => ProcOutcome::Continue,
            FastOut::One(v) => fused_send(m, &v),
            FastOut::Many(vs) => {
                for v in vs {
                    if matches!(fused_send(m, &v), ProcOutcome::Stop) {
                        return ProcOutcome::Stop;
                    }
                }
                ProcOutcome::Continue
            }
        };
    }
    let mut ctx = ProcCtx {
        interp: &mut m.interp,
        pid: &m.pid,
        step_fn: &m.step_fn,
        state: &mut m.state,
        count: &mut m.count,
        run_status: &mut m.run_status,
        ins: &m.ins,
        outs: &m.outs,
        // Always `None`: `plan_transport_links` refuses every conn
        // touching a multi-proc fused run, so this loop is entirely
        // transport-unaware by construction (module doc, "fused >
        // transport > Chan").
        out_lane: None,
        control: &m.control,
        doorbell: &m.doorbell,
    };
    process_message(&mut ctx, &m.cid, msg, error_chan)
}

/// Threads one message through member `start` and every member after it, in
/// order. Depth-first over an explicit stack (never recursion -- a chain can
/// be arbitrarily long): a member returning N messages has its FIRST output
/// carried all the way to the tail before its second is started, which is
/// exactly the order the unfused chain would produce (each hop's chan
/// preserves order, and the next proc drains it in order).
fn fused_deliver(
    ms: &mut [FusedMember],
    error_chan: &Arc<Chan>,
    start: usize,
    msg: Value,
    stack: &mut Vec<(usize, Value)>,
    buf: &mut Vec<Value>,
) -> ProcOutcome {
    let last = ms.len() - 1;
    stack.push((start, msg));
    while let Some((idx, m)) = stack.pop() {
        let outcome = if idx == last {
            fused_process_tail(&mut ms[idx], error_chan, m)
        } else {
            buf.clear();
            let o = fused_process_mid(&mut ms[idx], error_chan, m, buf);
            while let Some(v) = buf.pop() {
                stack.push((idx + 1, v));
            }
            o
        };
        if matches!(outcome, ProcOutcome::Stop) {
            stack.clear();
            return ProcOutcome::Stop;
        }
    }
    ProcOutcome::Continue
}

/// The init-time half of the fusion decision (the wiring-time half is
/// [`plan_fusion`]). `::flow/in-ports`/`out-ports` only appear once `init`
/// has run, and they can invalidate a plan made from declared ports -- so
/// every assumption the fused loop relies on is re-checked here against the
/// POST-init reality, and any violation demotes the whole run back to a
/// thread per proc.
///
/// A HEAD gaining in-ports is the normal, supported case (a generator
/// declares no `:ins` and gains its `:tick` at init; that becomes the run's
/// input) -- as long as it ends up with exactly ONE, which is what the
/// single-in check below says for every member uniformly.
fn fusion_still_valid(
    readys: &[ProcReady],
    link_out_ids: &[Option<Value>],
    link_in_ids: &[Option<Value>],
    link_chans: &[Option<Arc<Chan>>],
) -> bool {
    let n = readys.len();
    for r in readys {
        // Exactly one input, or there is no single chan for the run (head)
        // / no single upstream feed (mid) to read from.
        if r.ins.len() != 1 {
            return false;
        }
        // An `::flow/input-filter` can only turn a single-in member OFF
        // entirely, which a fused run has no way to honor (it has no
        // per-member read set to shrink). Rare enough to simply refuse.
        if extract_input_filter(&r.state).is_some() {
            return false;
        }
    }
    for i in 0..n - 1 {
        // `real_outs`: `readys[i].outs` always carries the two
        // engine-reserved out-targets now (design Part 2) alongside
        // whatever the proc itself declared -- only the latter bears on
        // whether this member still has exactly one link-worthy out port.
        if real_outs(&readys[i].outs).count() != 1 {
            return false;
        }
        let (Some(out_id), Some(in_id), Some(link)) =
            (link_out_ids[i].as_ref(), link_in_ids[i].as_ref(), link_chans[i].as_ref())
        else {
            return false;
        };
        // The link must still be THE wired inter-member chan on both ends:
        // an `init` that replaced either side with an external chan means
        // the two members are no longer actually connected to each other.
        match readys[i].outs.get(out_id) {
            Some(Some(c)) if Arc::ptr_eq(c, link) => {}
            _ => return false,
        }
        match readys[i + 1].ins.get(in_id) {
            Some(c) if Arc::ptr_eq(c, link) => {}
            _ => return false,
        }
    }
    true
}

/// Fusion fell through at init time: run the members as ordinary,
/// independent procs after all. `init` has ALREADY run for each (that's
/// what produced the ports that broke the plan), so each continues from its
/// [`ProcReady`] rather than being re-initialized. This thread becomes the
/// head's proc thread and joins the others on the way out, so `stop`'s
/// join-the-head still means "the whole run has exited".
fn demote_run(readys: Vec<ProcReady>, error_chan: &Arc<Chan>) -> ExitReason {
    let mut handles = Vec::new();
    let mut iter = readys.into_iter();
    // Unreachable in practice (`run_fused` only ever calls this with 2+
    // members -- see its own `debug_assert!`), but `readys` is not typed to
    // rule it out, so: nothing ran, `Normal` is the least-wrong answer
    // (there is no head to have panicked or been stopped).
    let Some(first) = iter.next() else { return ExitReason::Normal };
    for r in iter {
        let pid = r.pid.clone();
        let name = format!("flow-{}", crate::printer::display_str(&pid));
        // L4 W1: the run's ONE `ExitReason` is the HEAD's -- see this fn's
        // own doc and docs/L4-LANDING-SPEC.md §W1.2. A demoted NON-head
        // member's own exit reason has nowhere to go (there is no per-member
        // `ExitGuard` here, only the run-level one the outer spawn closure
        // holds, which fans out the head's ONE reason to every member's
        // done-cell -- design §3.4, "the run is the death unit"), so it is
        // deliberately discarded rather than threaded through
        // `join_with_timeout` (which stays `JoinHandle<()>` for every other
        // caller too).
        match std::thread::Builder::new().name(name).stack_size(PROC_STACK_SIZE).spawn(crate::memstat::drained(move || {
            run_ready(r);
        })) {
            Ok(h) => handles.push(h),
            Err(e) => emit_lifecycle_error(error_chan, &pid, "init", format!("couldn't spawn demoted proc thread: {e}")),
        }
    }
    let reason = run_ready(first);
    for h in handles {
        join_with_timeout(h, STOP_JOIN_TIMEOUT);
    }
    reason
}

/// The fused run's thread. Structurally the generic loop's single-input
/// branch (same park primitive, same `PARK_TIMEOUT`/`BATCH_DRAIN_MAX`/
/// `CONTROL_CHECK_EVERY`, same per-message `catch_unwind`, same
/// error/ping builders) with three additions:
///
/// - the read is the HEAD's single in-chan, and each drained message is
///   threaded through EVERY member back-to-back ([`fused_deliver`]);
/// - control is serviced for ALL members at every check point
///   ([`service_controls`]); a paused member parks the WHOLE run (a
///   documented deviation -- see FLOW-DESIGN.md's Fusion section);
/// - each mid-chain member's (still-wired, otherwise-empty) in-chan is
///   `try_take`n exactly ONCE per outer iteration, so `flow/inject` to a
///   mid-chain in-port still works -- at batch granularity rather than
///   interleaved message-by-message (also documented there).
fn run_fused(spawn: FusedSpawn) -> ExitReason {
    let FusedSpawn { members, error_chan } = spawn;
    let n = members.len();
    debug_assert!(n >= 2, "run_fused is only ever spawned for a run of 2+ members");

    // The links, read from the DECLARED wiring before any `init` can touch
    // it: member i's sole out-port/chan and member i+1's sole in-port.
    let mut link_out_ids: Vec<Option<Value>> = Vec::with_capacity(n - 1);
    let mut link_in_ids: Vec<Option<Value>> = Vec::with_capacity(n - 1);
    let mut link_chans: Vec<Option<Arc<Chan>>> = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        // L5 fence #9b (design §4): each `.next()` here is HashMap order, and
        // is only deterministic because fusion eligibility already refused
        // any member with more than one DECLARED out port or more than one
        // in port (`plan_fusion`). `real_outs` excludes the two
        // engine-reserved out-targets (design Part 2), always present now
        // alongside whatever the member itself declared. Pinned, not assumed.
        debug_assert!(real_outs(&members[i].outs).count() <= 1, "fence #9b: a fused member has at most one out port");
        debug_assert!(members[i + 1].ins.len() == 1, "fence #9b: a fused non-head member has exactly one in port");
        link_out_ids.push(real_outs(&members[i].outs).next().map(|(k, _)| k.clone()));
        link_in_ids.push(members[i + 1].ins.keys().next().cloned());
        link_chans.push(real_outs(&members[i].outs).next().and_then(|(_, v)| v.clone()));
    }

    let readys: Vec<ProcReady> = members.into_iter().map(init_proc).collect();
    if !fusion_still_valid(&readys, &link_out_ids, &link_in_ids, &link_chans) {
        // Demotion propagates the demoted run's reason (docs/L4-LANDING-SPEC.md §W1.2).
        return demote_run(readys, &error_chan);
    }

    // Per-member N2 promotion, asked exactly as `run_ready` asks it for a
    // standalone proc: a native step carrying a `StepFactory`, one input,
    // no input-filter (refused above), at most one out port.
    let mut fasts: Vec<Option<Box<dyn FastStep>>> = Vec::with_capacity(n);
    for r in &readys {
        let read_set: Vec<(Value, Arc<Chan>)> = r.ins.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        fasts.push(try_promote_fast(&r.step_fn, &r.state, &read_set, None, &r.outs));
    }
    // The measured default policy (see [`FusionPolicy`]): a run with any
    // NON-promoted member is worth more as a parallel pipeline than as a
    // fused chain, so it demotes here -- after `init`, before a single
    // message has moved, and through the exact same path an init-time
    // port surprise takes.
    if fusion_policy() == FusionPolicy::PromotedOnly && fasts.iter().any(|f| f.is_none()) {
        drop(fasts);
        return demote_run(readys, &error_chan);
    }

    // One `Doorbell` for the WHOLE run (not per member) -- see the
    // registration block below for what it is registered on and why.
    // Created BEFORE the members so each can carry a clone of it (a blocked
    // tail send needs one; `FusedMember::doorbell`).
    let doorbell = Arc::new(Doorbell::new());

    let mut ms: Vec<FusedMember> = Vec::with_capacity(n);
    for (i, (r, fast)) in readys.into_iter().zip(fasts).enumerate() {
        let ProcReady { interp, pid, step_fn, state, ins, outs, control, .. } = r;
        // L5 fence #9b (design §4): both `.next()`s below are HashMap order,
        // deterministic only because fusion eligibility refused any member
        // with more than one in port or more than one DECLARED out port --
        // `real_outs` excludes the two engine-reserved out-targets (design
        // Part 2), always present in `outs` now. `outs` itself keeps BOTH
        // reserved keys when moved into `FusedMember` below: a mid-chain
        // member's `Capture`-sink routing (`route_reserved_outs`) and this
        // run's ping replies both need them still resolvable there. Pinned.
        debug_assert!(ins.len() == 1, "fence #9b: a fused member has exactly one in port, got {}", ins.len());
        debug_assert!(
            real_outs(&outs).count() <= 1,
            "fence #9b: a fused member has at most one out port, got {}",
            real_outs(&outs).count()
        );
        let (cid, in_chan) = ins.iter().next().map(|(k, v)| (k.clone(), v.clone())).expect("checked: exactly one in");
        let state = if fast.is_some() { Value::Nil } else { state };
        let link_out = if i + 1 < n { link_out_ids[i].clone() } else { None };
        let out_chan = real_outs(&outs).next().and_then(|(_, v)| v.clone());
        ms.push(FusedMember {
            interp,
            pid,
            step_fn,
            state,
            fast,
            count: 0,
            run_status: RunStatus::Paused,
            cid,
            in_chan,
            ins,
            outs,
            control,
            link_out,
            out_chan,
            stopped: false,
            doorbell: doorbell.clone(),
        });
    }

    // The run's ONE `Doorbell`, registered (not per member): the run has a
    // single read loop, so there is exactly one place that ever BLOCKS --
    // the HEAD's control/in-chan reads below (`try_take_with_timeout`).
    // Registered there, AND on every OTHER member's control chan (so a
    // `pause`/`stop`/`ping` aimed at a mid-chain member wakes the run
    // immediately instead of only ≤`PARK_TIMEOUT` later via
    // `service_controls`'s next pass), AND on every OTHER member's in-chan
    // too -- NOT because the loop ever blocks on those (it doesn't: they're
    // only ever non-blockingly `chan_try_take`n, once per outer lap, in the
    // mid-chain-inject scan below), but because registering them means a
    // `flow/inject` to a mid-chain port rings this SAME shared doorbell,
    // which wakes the head's blocking read early and sends the outer loop
    // back around to that scan right away instead of leaving the injected
    // message sitting unnoticed for up to a full `PARK_TIMEOUT` (see
    // `tests/flow_wake_test.rs`'s
    // `injected_message_to_mid_chain_port_wakes_a_fused_run_promptly`). The
    // module doc's "no channel hop between fused members" is unaffected by
    // any of this -- these are all EXTERNAL boundary chans of the run, not
    // inter-member hops, which stay exactly as fast as before: no channel,
    // no wait, no doorbell involved.
    for m in &ms {
        lock_mutex(&m.control.state).doorbell = Some(doorbell.clone());
    }
    lock_mutex(&ms[0].in_chan.state).doorbell = Some(doorbell.clone());
    for m in &ms[1..] {
        lock_mutex(&m.in_chan.state).doorbell = Some(doorbell.clone());
    }

    let mut head_closed = false;
    let mut stack: Vec<(usize, Value)> = Vec::new();
    let mut buf: Vec<Value> = Vec::new();
    let mut batch: Vec<Value> = Vec::with_capacity(BATCH_DRAIN_MAX);

    // L4 W1: same "loop as `ExitReason`-valued expression" shape as
    // `run_ready`/`run_proc_fast` -- see `run_ready`'s comment. Two sites
    // below (both a `try_take_with_timeout` on the HEAD's own control chan)
    // additionally gained an explicit `TryTake::Closed` arm that did not
    // exist before W1: previously `Closed` and `WouldBlock` were folded into
    // the same "nothing happened this lap" `if let Received = ...` miss, but
    // `Closed` never times out (`try_take_with_timeout`'s `g.closed` fast
    // path returns immediately, every time), so a run parked here when
    // `stop`'s `STOP_JOIN_TIMEOUT` abandonment closed `control` out from
    // under it would busy-spin this park forever instead of exiting. `Normal`
    // is the correct reason for exactly the same cause `run_ready`'s own
    // `chan_take(&control) else` site names it for.
    let exit_reason = 'outer: loop {
        // THE lap snapshot, before `service_controls` scans EVERY member's
        // control chan and before either of this lap's parks. A fused run
        // makes the ordering rule especially visible: its scan covers N
        // control chans, so the window between "scanned them all" and "took
        // the snapshot" would be the widest anywhere in this module. See
        // [`try_take_with_timeout`]'s doc.
        let seen = doorbell.current();
        // A paused member parks the whole run (deviation, documented): the
        // fused chain has one read loop, so there is no "upstream keeps
        // running until the inter-member buffer fills" to reproduce.
        if ms.iter().any(|m| m.run_status == RunStatus::Paused) {
            if service_controls(&mut ms) {
                break 'outer ExitReason::Stopped;
            }
            if ms.iter().any(|m| m.run_status == RunStatus::Paused) {
                // Park on the HEAD's control chan (capped, doorbell-driven --
                // see above), so a broadcast resume/stop wakes the run
                // immediately instead of after a poll interval; every other
                // member's own control chan rings this SAME doorbell too, so
                // a command aimed at any one of them also wakes this park
                // right away (`service_controls`'s next pass then finds it).
                match try_take_with_timeout(&ms[0].control, &doorbell, seen, PARK_TIMEOUT) {
                    TryTake::Received(cmd) => {
                        if apply_control_member(&mut ms[0], cmd) {
                            break 'outer ExitReason::Stopped;
                        }
                    }
                    TryTake::Closed => break 'outer ExitReason::Normal,
                    TryTake::WouldBlock => {}
                }
            }
            continue 'outer;
        }

        if service_controls(&mut ms) {
            break 'outer ExitReason::Stopped;
        }

        // `flow/inject` to a MID-chain in-port: that chan exists and is
        // wired exactly as it would be unfused, it just never carries
        // engine traffic. One `try_take` per mid member per outer
        // iteration keeps it cheap.
        for idx in 1..n {
            if let TryTake::Received(m) = chan_try_take(&ms[idx].in_chan) {
                if matches!(fused_deliver(&mut ms, &error_chan, idx, m, &mut stack, &mut buf), ProcOutcome::Stop) {
                    break 'outer ExitReason::Stopped;
                }
            }
        }

        if head_closed {
            match try_take_with_timeout(&ms[0].control, &doorbell, seen, PARK_TIMEOUT) {
                TryTake::Received(cmd) => {
                    if apply_control_member(&mut ms[0], cmd) {
                        break 'outer ExitReason::Stopped;
                    }
                }
                TryTake::Closed => break 'outer ExitReason::Normal,
                TryTake::WouldBlock => {}
            }
            continue 'outer;
        }

        match try_take_with_timeout(&ms[0].in_chan, &doorbell, seen, PARK_TIMEOUT) {
            TryTake::Received(first) => {
                batch.clear();
                batch.push(first);
                while batch.len() < BATCH_DRAIN_MAX {
                    match chan_try_take(&ms[0].in_chan) {
                        TryTake::Received(v) => batch.push(v),
                        _ => break,
                    }
                }
                for (i, msg) in batch.drain(..).enumerate() {
                    if i > 0 && i % CONTROL_CHECK_EVERY == 0 && service_controls(&mut ms) {
                        break 'outer ExitReason::Stopped;
                    }
                    if matches!(fused_deliver(&mut ms, &error_chan, 0, msg, &mut stack, &mut buf), ProcOutcome::Stop) {
                        break 'outer ExitReason::Stopped;
                    }
                }
            }
            TryTake::Closed => {
                // Match the generic loop's own bookkeeping so a `ping`'s
                // `::flow/ins` vector agrees: a closed in-port is dropped.
                let cid = ms[0].cid.clone();
                ms[0].ins.remove(&cid);
                head_closed = true;
            }
            TryTake::WouldBlock => {}
        }
    };

    stop_all(&mut ms);
    exit_reason
}

// ---------------------------------------------------------------------------
// Flow-wide / single-proc lifecycle natives
// ---------------------------------------------------------------------------

fn join_with_timeout(handle: std::thread::JoinHandle<()>, timeout: Duration) {
    let deadline = crate::clock::clock_now() + timeout;
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        if crate::clock::clock_now() >= deadline {
            // Detach: drop the handle without joining. The thread (holding
            // its own Arc<Chan> clones) keeps running independently; this
            // just stops `stop` from hanging on a wedged proc forever.
            drop(handle);
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One de-poll tick, shared by every straggler this module still polls
/// (W2, design §3.7): arms a fresh one-shot chan on `builtins::async`'s
/// shared timer service ([`timer_arm`], promoted to `pub(crate)` for
/// exactly this -- see that function's doc) and blocks on its close. A
/// THREAD caller parks on that chan's own condvar -- the same OS-level wait
/// a `thread::sleep` of the same duration was -- and a TASK caller parks via
/// `chan_take`'s `task_takers` registration, which YIELDS the shard instead
/// of burning it, which is the entire point everywhere this is called
/// ([`native_ping`]/[`native_ping_proc`]'s reply-collection loop,
/// [`wait_done_with_timeout`]'s task arm). `ms` is floored at 1: the shared
/// timer service only arms in whole milliseconds, so a caller whose
/// historical cadence was sub-millisecond (`PING_POLL`'s documented 500 µs)
/// gets 1ms instead -- see that constant's doc for why the difference is
/// negligible against every caller's actual budget.
/// **L5/W3 fence #17 (design §4): the OS-thread poll loops, warned about.**
///
/// A poll loop that sleeps wall time and tests a `clock_now()` deadline arms
/// NO timer. Under sim, with an empty heap, its deadline is therefore not
/// merely late — it is UNREACHABLE, and the loop is an infinite loop rather
/// than a timeout (P6b F3 hung twice on exactly this). Fences #1/#3 plus the
/// §5 boundary contract are what make these arms unreachable in sim: no `:io`
/// procs, no OS-thread producers, and Mova evaluated inside the task world.
///
/// The design's wording for this entry is `debug_assert(!sim_enabled())`.
/// **Declared deviation, principal-approved:** a once-per-site stderr warning
/// instead. The env lever (`MOVA_SIM_SEED`) is documented in §5 as a
/// per-program SMOKE lever, and smoke runs deliberately drive programs from
/// an OS thread — a `debug_assert` would abort those runs at the first lap
/// instead of letting them report what they found. The warning says the same
/// thing at the same site and keeps the diagnostic alive in release builds
/// too, where a `debug_assert` compiles to nothing at all.
///
/// One `AtomicBool` per site (passed in), so a program that reaches two
/// different poll loops hears about both, and a program that laps one of them
/// ten thousand times hears about it once. Real mode: one relaxed
/// `sim_enabled()` load per lap of a loop that is already sleeping 10 ms.
fn warn_sim_os_poll_loop(site: &str, warned: &AtomicBool) {
    if !crate::clock::sim_enabled() || warned.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "mova sim: OS-thread poll loop reached under sim (L5 fence #17) at {site} -- out of \
         contract, may hang: this loop sleeps WALL time and tests a VIRTUAL deadline it arms no \
         timer for, so with an empty timer heap that deadline is unreachable. Run the program \
         inside a task (design §5's boundary contract)."
    );
}

fn park_tick(ms: u64) {
    let tick = Arc::new(Chan::new(BufferPolicy::Fixed(0)));
    timer_arm(tick.clone(), ms.max(1));
    let _ = chan_take(&tick);
}

/// [`join_with_timeout`] for a proc that has no thread to join: wait for its
/// DONE-CELL to close. See `ProcRuntime::done` (value.rs) for what the cell
/// is; it is a dedicated engine-created chan that never carries a value, so
/// this wait can never disturb anything (W1a's "the control chan doubles as
/// the done-cell" overload is gone, and with it the question of whether
/// `stop` might observe a control chan closed early).
///
/// Two arms, because the CALLER may itself be a task (`flow/stop` inside a
/// `go` block -- design §7 R4-bis) -- but, since W2, ONE cadence and ONE cap
/// between them:
///
/// - **Thread caller**: the pre-L3 join cadence, verbatim -- poll every
///   10ms, give up at `deadline`. Deliberately NOT converted to a doorbell
///   park: `Doorbell::wait_for_change`'s thread arm counts a `SAFETY_NET_
///   HITS` on every deadline it actually reaches, and a shutdown poll firing
///   those would put noise into the one counter G-SOAK reads as a
///   correctness signal.
/// - **Task caller**: the SAME 10ms cadence, but each tick is a
///   [`park_tick`] rather than a bare clock-read spin -- a task parks
///   on a fresh one-shot timeout chan (a proper `task_takers` park) between
///   checks instead of busy-polling the clock, so the shard is YIELDED for
///   each 10ms slice instead of spun on. This is deliberately NOT the old
///   W1b shape (an uncapped `chan_take(done)` that woke instantly on the
///   cell's own close but never gave up): that traded away exactly the cap
///   this function now restores. Ordinary shutdown pays for the trade in
///   at most one 10ms tick of extra latency -- a proc that actually exits
///   closes its cell promptly, and the very next tick observes it -- which
///   is the same latency class the thread arm already has and always has
///   had.
///
/// **W1b's gap, closed.** The task arm used to have NO deadline at all
/// (`timeout` was honored on the thread arm only) because bounding a task's
/// park needed the timer service in `builtins::async`, which W1b did not
/// own. W2 owns exactly one line of that module ([`timer_arm`]'s
/// visibility) and spends it here: `flow/stop` called from inside a `go`
/// block against a proc that never returns now abandons it at `deadline`,
/// exactly like the thread arm always has, instead of parking forever.
///
/// Timing out (either arm) means the same thing it means for a join: give up
/// and leave the proc running -- a task leaks parked instead of a handle
/// being dropped, the same observable with one different noun.
fn wait_done_with_timeout(done: &Chan, timeout: Duration) {
    let deadline = crate::clock::clock_now() + timeout;
    if crate::runtime::in_task() {
        loop {
            if lock_mutex(&done.state).closed {
                return;
            }
            let now = crate::clock::clock_now();
            if now >= deadline {
                return;
            }
            // Never overshoot `deadline` by a whole tick: the last slice
            // before it is whatever remains, not a full 10ms.
            let remaining = (deadline - now).min(Duration::from_millis(10));
            park_tick(remaining.as_millis() as u64);
        }
    }
    // L5/W3 fence #17 -- see `warn_sim_os_poll_loop`. Reached in sim only out
    // of contract (fence #1 leaves a sim flow with no proc threads, and a
    // task caller took the arm above).
    static WARNED: AtomicBool = AtomicBool::new(false);
    warn_sim_os_poll_loop("wait_done_with_timeout (thread arm)", &WARNED);
    loop {
        if lock_mutex(&done.state).closed {
            return;
        }
        if crate::clock::clock_now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The actual `stop` mechanics: broadcasts `::flow/stop`, joins every proc
/// thread it has a handle for (5s timeout guard, then detach) and waits on
/// the DONE-CELL of every proc it doesn't (task runs, and a thread run's
/// non-head fused members -- see [`wait_done_with_timeout`]), then
/// closes every engine-owned
/// chan -- NEVER a user-supplied `::flow/in-ports`/`out-ports` chan (see
/// `FlowRuntime`'s doc for why those are structurally excluded from
/// `engine_owned_chans`) -- and finally `report_chan`/`error_chan`
/// themselves, last of all (design Part 3 item 3).
///
/// **Phase ordering, with L4 W2's step 1b (design §3.6).** The sequence is
/// now, in order and for reasons that are each load-bearing:
///
/// 1. **phase -> `Stopped`, under the flow mutex.** The supervisor's
///    `decide()` consults the phase and never restarts a flow that is
///    stopping (wall W4), and its restart ACTION re-takes this very lock and
///    holds it across the respawn -- so from the moment this step returns,
///    either a restart already completed (and its done-cell is in `procs`,
///    below) or no restart will ever start.
/// 2. **1b (NEW, L4 W2): close `sup_chan`.** The supervisor drains whatever
///    is already buffered (a closed chan still delivers its buffer), decides
///    `Ignore` on all of it by rule 3 of `decide` -- the phase is already
///    `Stopped` -- then sees the close, exits, and closes its own done-cell.
///    Deaths racing INTO this window are events to a closed chan and are
///    dropped (`ExitGuard::drop`'s `TryPut::Closed` arm goes live here, and
///    only here): correct, because from step 1 onward the waits below own the
///    endgame and a restart decided out of such an event would be racing the
///    teardown it belongs to.
/// 3. the `::flow/stop` broadcast, then the joins/done-cell waits under ONE
///    shared deadline -- which the supervisor's `sup_done` JOINS, as a
///    supervised flow's k+1-th proc for shutdown purposes.
/// 4. close every engine-owned chan and link.
/// 5. close `report_chan` and `error_chan` (design Part 3 item 3), LAST --
///    strictly after step 4, and separately from `engine_owned_chans`
///    (which step 4 already closes): the supervisor's own final
///    lifecycle/DEATH events (step 3's wait is what lets it drain and
///    exit) and a proc's last `report_error` are both last-chance writers
///    to these two chans, so closing any earlier would race them. By step
///    5 every proc and the supervisor have EITHER actually exited (the
///    ordinary case: steps 3-4's joins/done-cell waits returned because
///    the work genuinely finished) OR been abandoned after burning the
///    shared `STOP_JOIN_TIMEOUT` deadline (`wait_done_with_timeout`'s
///    give-up path, W2's doc above) -- and a wedged, detached proc on
///    THAT second path is not actually gone, just no longer waited on. If
///    it later, honestly, calls `report_error` (or its transform emits
///    `::flow/report`), that put lands on an already-closed chan and is
///    silently dropped (`chan_try_put`/`chan_put`'s closed-chan arm is a
///    documented no-op, not a panic) -- so "no writer left to race" is
///    true for the join-succeeded case but overstated for the abandon-
///    after-timeout case: a BOUNDED window of loss is possible there, by
///    construction (the whole point of a detach timeout is to stop
///    waiting on a proc that may never finish). This is still same-or-
///    better than upstream, which closes both chans immediately on `stop`
///    with no join/wait step at all -- upstream's window for exactly this
///    same drop is unbounded-in-practice (any in-flight write after
///    `stop` is called races the close), where this engine's is bounded
///    by `STOP_JOIN_TIMEOUT` and only reachable via the already-abnormal
///    wedged-proc path.
///
/// An unsupervised flow has no `sup_chan` and no `sup_done`, so steps 1b and
/// the extra wait are two `Option` checks that do nothing -- the default
/// world's stop is unchanged, instruction for instruction.
///
/// **One shared deadline across the whole done-cell wait loop (W2).** Every
/// join keeps its own independent `STOP_JOIN_TIMEOUT` (a real OS thread,
/// unaffected), but every [`wait_done_with_timeout`] call below shares ONE
/// `done_deadline` computed before the per-pid loop starts. Before this fix,
/// a wedged FUSED run -- one thread or task shared by k members, each with
/// its own `procs_rt` entry and its own done-cell, all of which close
/// TOGETHER only when that one run's closure finally exits -- cost `stop`
/// `STOP_JOIN_TIMEOUT` for the head's join PLUS `STOP_JOIN_TIMEOUT` AGAIN
/// for each of the other k-1 members' independent waits, because a wedged
/// run's cells never close and each wait had no idea any of its siblings had
/// already burned the whole budget. Total: `k * STOP_JOIN_TIMEOUT` for one
/// wedged run. With one shared deadline, the first wait to actually block
/// (whichever pid's turn it is when nothing has closed yet) is the only one
/// that pays real time; every wait after it sees a deadline already in the
/// past and returns immediately. Total: `~1 * STOP_JOIN_TIMEOUT` regardless
/// of k. `tests/l3_task_procs_test.rs`'s stop-deadline test pins this.
///
/// `report_chan`/`error_chan` ARE closed by `stop` (step 5 above) --
/// design Part 3 item 3, matching upstream (`start` assocs `::flow/error
/// error-chan, ::flow/report report-chan` into every proc's outs map, and
/// `stop` tears the whole graph down including its chans) and flow-gold
/// scenario 13 (`tests/conformance/flow-gold/SPEC.md:152`: "stop closes
/// report+error chans (read -> nil after)"). This reverses an earlier,
/// explicitly documented deviation (leaving them open so a caller still
/// draining diagnostics wouldn't see a spurious close) now that design
/// Part 2 makes both chans always-wired out-targets: a caller draining
/// `:report-chan`/`:error-chan` after `stop` gets exactly what `<!!`-ing a
/// closed, drained `Chan` already means everywhere else in this engine --
/// `nil` once the buffer empties, not a hang -- so there is no longer a
/// "surprise" to avoid, only an inconsistency (chans this design says are
/// engine-owned wiring that outlive nothing).
///
/// Idempotent: called on an already-`Stopped` (or never-`Running`) flow, it
/// is a no-op. Returns `true` iff THIS call was the one that actually
/// performed the stop (phase was `Running`), `false` otherwise -- the
/// distinction `crate::embed::engine::Engine::shutdown` uses to decide
/// whether a flow counts toward its `flows_stopped` report at all.
///
/// Shared by `flow/stop` (`native_stop`, below) and `Engine::shutdown` --
/// ONE stop implementation, reused directly (not through an eval'd
/// `(flow/stop ...)` call) so the two call sites can never drift and so
/// `shutdown` works even on an engine with no `flow` capability registered
/// at all (its registry is simply always empty in that case).
pub(crate) fn stop_flow_cell(flow: &Arc<FlowCell>) -> bool {
    {
        let mut phase = lock_mutex(&flow.phase);
        if *phase != FlowPhase::Running {
            *phase = FlowPhase::Stopped;
            return false;
        }
        *phase = FlowPhase::Stopped;
    }
    let Some(runtime) = lock_mutex(&flow.runtime).take() else {
        return false;
    };

    // Step 1b (L4 W2, design §3.6) -- see this fn's phase-ordering doc.
    // BEFORE the broadcast: the supervisor must be on its way out before the
    // procs start dying of the stop, or it would spend the whole teardown
    // deciding about deaths that `stop` is already waiting for.
    if let Some(sup) = &runtime.sup_chan {
        chan_close(sup);
    }

    let mut stop_cmd = PMap::new();
    stop_cmd.insert(kw("op"), plain_kw("stop"));
    let stop_cmd = Value::Map(stop_cmd);
    for pid in &flow.def.proc_order {
        if let Some(p) = runtime.procs.get(pid) {
            chan_put(&p.control_chan, stop_cmd.clone());
        }
    }
    // ONE deadline for every done-cell wait below (W2), computed before the
    // loop even starts -- see this function's doc for the pathology this
    // closes. Each JOIN keeps its own full, independent `STOP_JOIN_TIMEOUT`
    // (pre-L3 exact: a thread run's head is a real OS thread and detaching
    // from a wedged one is unaffected by anything below).
    let done_deadline = crate::clock::clock_now() + STOP_JOIN_TIMEOUT;
    for pid in &flow.def.proc_order {
        if let Some(p) = runtime.procs.get(pid) {
            let handle = lock_mutex(&p.thread).take();
            match handle {
                // A thread run's head: the pre-L3 path, unchanged.
                Some(h) => join_with_timeout(h, STOP_JOIN_TIMEOUT),
                // No handle: EITHER a task run (nothing to join, ever) OR a
                // thread run's non-head fused member. The done-cell wait is
                // right for both and needs to know which is which for
                // neither -- every proc closes its cell in both worlds
                // (L3/W1b), which is what keeps this branch world-agnostic.
                // For a non-head member the cell is normally already closed
                // by the time this is reached (the run's head was joined
                // above), so it returns on its first look; when proc_order
                // happens to visit a non-head member BEFORE its head, this
                // waits for the run to finish instead -- the same wait, paid
                // in a different place. The budget passed in is what's LEFT
                // of `done_deadline`, not a fresh `STOP_JOIN_TIMEOUT` each
                // time (W2) -- see this function's doc.
                None => wait_done_with_timeout(&p.done, done_deadline.saturating_duration_since(crate::clock::clock_now())),
            }
        }
    }
    // The supervisor joins the SAME shared-deadline wait set (L4 W2), last:
    // step 1b closed its input chan, so by now it has drained, exited and
    // closed this cell -- in the overwhelmingly common case this returns on
    // its first look. It is waited for AFTER the procs because its own last
    // possible action (a restart) installs a proc done-cell into the table
    // the loop above just walked, and the natural reading of "everything this
    // flow was running has stopped" puts the thing that could spawn more of
    // it at the end. `None` for every unsupervised flow: no supervisor, no
    // wait.
    if let Some(sup_done) = &runtime.sup_done {
        wait_done_with_timeout(sup_done, done_deadline.saturating_duration_since(crate::clock::clock_now()));
    }
    for c in &runtime.engine_owned_chans {
        chan_close(c);
    }
    // Every engine-created 1:1 transport link, on exactly the same footing
    // as the engine-created chans above: after every proc thread has
    // exited, idempotently, and never a user-supplied channel (a transport
    // is only ever wired between two engine ports, so there is no
    // user-supplied case to exclude).
    for l in &runtime.engine_owned_links {
        l.close();
    }
    // Step 5 (design Part 3 item 3): LAST, and strictly after every proc
    // AND the supervisor have exited (steps 3-4 above) -- both are
    // last-chance writers to these two chans (a proc's final
    // `report_error`, the supervisor's final lifecycle/DEATH event), so
    // closing any earlier would race them. See this fn's own doc, step 5,
    // for why these two are closed separately from (and after)
    // `engine_owned_chans` rather than simply being added to that vec.
    chan_close(&runtime.report_chan);
    chan_close(&runtime.error_chan);
    true
}

fn native_stop(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/stop: expected 1 argument, got {}", args.len())));
    }
    let flow = expect_flow(&args[0], "flow/stop")?;
    stop_flow_cell(flow);
    Ok(Value::Nil)
}

/// L4 W5: the desired-lifecycle-state update one of `broadcast_command`'s or
/// `single_command`'s ops implies, or `None` for an op that says nothing
/// about it (`ping` is the only other `single_command` op, and never reaches
/// this: it has its own dedicated native, not one of these two dispatchers).
/// `"stop"` (`native_stop_proc`'s rung 1, the only `single_command` caller
/// that passes it) resolves to `Paused` for the same reason `"pause"` does:
/// owner ruling #5's own wording, "a proc the user deliberately paused stays
/// paused across a crash-restart" -- and a proc the user asked to STOP is
/// that same "no desire to run" a fortiori.
fn desired_state_for_op(op: &str) -> Option<DesiredState> {
    match op {
        "resume" => Some(DesiredState::Running),
        "pause" | "stop" => Some(DesiredState::Paused),
        _ => None,
    }
}

fn broadcast_command(flow_val: &Value, op: &str, native_name: &str) -> Result<Value, RjError> {
    let flow = expect_flow(flow_val, native_name)?;
    if *lock_mutex(&flow.phase) != FlowPhase::Running {
        return Err(RjError::other(format!("{native_name}: flow is not running")));
    }
    let runtime_guard = lock_mutex(&flow.runtime);
    let Some(runtime) = runtime_guard.as_ref() else {
        return Err(RjError::other(format!("{native_name}: flow is not running")));
    };
    // L4 W5: record the user's INTENT for every pid BEFORE the control put --
    // see `FlowRuntime::desired`'s doc. `act_restart` is the only reader, and
    // it takes this same `runtime` lock to reach it, so there is no ordering
    // hazard in updating it first; doing so first (rather than after the
    // puts below) means a restart racing this very call can never observe a
    // stale intent.
    if let Some(state) = desired_state_for_op(op) {
        let mut desired = lock_mutex(&runtime.desired);
        for pid in &flow.def.proc_order {
            desired.insert(pid.clone(), state);
        }
    }
    let mut cmd = PMap::new();
    cmd.insert(kw("op"), plain_kw(op));
    let cmd = Value::Map(cmd);
    for pid in &flow.def.proc_order {
        if let Some(p) = runtime.procs.get(pid) {
            chan_put(&p.control_chan, cmd.clone());
        }
    }
    Ok(Value::Nil)
}

fn single_command(flow_val: &Value, pid: &Value, op: &str, native_name: &str) -> Result<Value, RjError> {
    let flow = expect_flow(flow_val, native_name)?;
    if *lock_mutex(&flow.phase) != FlowPhase::Running {
        return Err(RjError::other(format!("{native_name}: flow is not running")));
    }
    let runtime_guard = lock_mutex(&flow.runtime);
    let Some(runtime) = runtime_guard.as_ref() else {
        return Err(RjError::other(format!("{native_name}: flow is not running")));
    };
    let p = runtime
        .procs
        .get(pid)
        .ok_or_else(|| RjError::other(format!("{native_name}: unknown pid {}", crate::printer::pr_str(pid))))?;
    // L4 W5: this pid's desired state, same "before the put" ordering as
    // `broadcast_command` above.
    if let Some(state) = desired_state_for_op(op) {
        lock_mutex(&runtime.desired).insert(pid.clone(), state);
    }
    let mut cmd = PMap::new();
    cmd.insert(kw("op"), plain_kw(op));
    chan_put(&p.control_chan, Value::Map(cmd));
    Ok(Value::Nil)
}

fn native_pause(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/pause: expected 1 argument, got {}", args.len())));
    }
    broadcast_command(&args[0], "pause", "flow/pause")
}

fn native_resume(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 1 {
        return Err(RjError::arity(format!("flow/resume: expected 1 argument, got {}", args.len())));
    }
    broadcast_command(&args[0], "resume", "flow/resume")
}

fn native_pause_proc(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 2 {
        return Err(RjError::arity(format!("flow/pause-proc: expected 2 arguments, got {}", args.len())));
    }
    single_command(&args[0], &args[1], "pause", "flow/pause-proc")
}

fn native_resume_proc(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 2 {
        return Err(RjError::arity(format!("flow/resume-proc: expected 2 arguments, got {}", args.len())));
    }
    single_command(&args[0], &args[1], "resume", "flow/resume-proc")
}

/// `(flow/stop-proc fl :pid)` -- **stop ONE proc** (L4 W3,
/// docs/L4-LANDING-SPEC.md §W3.3).
///
/// **mova-native surface, and deliberately so.** Upstream
/// `core.async.flow` has no per-proc stop at all: the Graph protocol is
/// `start`/`stop`/`pause`/`resume`/`ping`/`inject` plus their `-proc`
/// variants for pause/resume/ping only, and there is nothing in the SPI to
/// copy (docs/L4-SUPERVISION-DESIGN.md §1: "The Graph protocol has no
/// per-proc stop/start"). It is registered and shaped EXACTLY like
/// `pause-proc` -- same arity, same errors, same `single_command` body --
/// so the one thing a reader has to learn is the semantics, not the shape.
///
/// Semantics, in one line: **graceful stop, escalating to a kill after
/// `:grace-ms` if the proc is supervised, and graceful-only if it is not.**
/// In full:
/// - A `::flow/stop` command goes on the proc's control chan, exactly as
///   `flow/stop` broadcasts one to every proc. A proc that honors control --
///   which is every proc that is not wedged -- exits `:stopped` and that is
///   the end of it.
/// - If the proc has a resolved `:supervision` config, the supervisor is
///   also told, and it opens the `:grace-ms` window. If the proc has not
///   died when that window closes, the ladder escalates: a task proc is
///   KILLED (force-unwound at its park point), an `:io` thread proc cannot
///   be and gets `:proc-wedged` instead (wall W6, owner ruling #4).
/// - **A stopped proc does NOT restart**, whatever its `:policy` says. The
///   user asked for it to be gone; a supervisor that brought it back would
///   be fighting its user, which is the same principle that makes `:normal`
///   and `:stopped` exits ignore-always in [`decide`]. The report-chan event
///   is `:proc-stopped` (distinct from `:proc-give-up`, which means the
///   supervisor ran out of patience, and from `:proc-exit`, which is the
///   death itself).
/// - Unsupervised flow, or unsupervised pid: the graceful stop happens and
///   nothing else does. No events (an unsupervised flow's report-chan stays
///   silent -- the corpus pin), no escalation, no kill.
///
/// Errors identically to `pause-proc`: a flow that is not running, or a pid
/// this flow never spawned.
fn native_stop_proc(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 2 {
        return Err(RjError::arity(format!("flow/stop-proc: expected 2 arguments, got {}", args.len())));
    }
    // Rung 1 FIRST, and unconditionally: the graceful stop is the whole of
    // this native for an unsupervised proc, and for a supervised one it is
    // what the grace window is a window ON. It also does all the validation
    // (phase, pid), so an error here means nothing was requested of anyone.
    single_command(&args[0], &args[1], "stop", "flow/stop-proc")?;
    let flow = expect_flow(&args[0], "flow/stop-proc")?;
    let rt_guard = lock_mutex(&flow.runtime);
    let Some(rt) = rt_guard.as_ref() else { return Ok(Value::Nil) };
    let Some(sup) = rt.sup_chan.as_ref() else { return Ok(Value::Nil) };
    // De-duplicated for the life of the flow, which is what keeps
    // `sup_chan`'s `Fixed(2n)` sizing exact against wall W3 -- see
    // `FlowRuntime::stop_requested`. A repeat call still delivers rung 1
    // above (a second `::flow/stop` on a control chan is harmless and is
    // what `flow/stop` itself would do), it just does not queue a second
    // grace window.
    if !lock_mutex(&rt.stop_requested).insert(args[1].clone()) {
        return Ok(Value::Nil);
    }
    // `try_put`, never a blocking put: this is a user-facing native and it
    // must not park a caller behind a supervisor that is mid-restart. The
    // buffer cannot be full by the sizing argument above; if it somehow
    // were, the graceful stop has already been delivered and the escalation
    // is what is lost -- the safe direction.
    let _ = chan_try_put(sup, render_stop_request(&args[1]));
    Ok(Value::Nil)
}

fn require_timeout_ms(v: Option<&Value>, native_name: &str) -> Result<u64, RjError> {
    match v {
        None => Ok(1000),
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        Some(other) => Err(RjError::type_err(format!(
            "{native_name}: expected a non-negative int timeout-ms, got {}",
            other.type_name()
        ))),
    }
}

/// `flow/ping`: pings every proc (fresh buf-1 reply chan each), collects
/// replies into a `{pid -> reply-map}` map until every proc has answered or
/// `timeout-ms` (default 1000) elapses -- pids that didn't answer in time
/// are simply absent from the result (documented: no `:timeout` sentinel in
/// v0). De-polled (W2, design §3.7): the tail's [`park_tick`] parks a TASK
/// caller (`flow/ping` from inside a `go` block) instead of burning its
/// shard on `thread::sleep` -- the very first pass through the loop below
/// almost always finds every reply chan still empty in practice (the ping
/// commands were JUST enqueued, so the pinged procs haven't necessarily run
/// yet), which is what makes this the common case rather than a corner one.
fn native_ping(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.is_empty() || args.len() > 2 {
        return Err(RjError::arity(format!("flow/ping: expected 1 or 2 arguments, got {}", args.len())));
    }
    let flow = expect_flow(&args[0], "flow/ping")?;
    let timeout_ms = require_timeout_ms(args.get(1), "flow/ping")?;

    let mut pending: Vec<(Value, Arc<Chan>)> = Vec::new();
    {
        if *lock_mutex(&flow.phase) != FlowPhase::Running {
            return Err(RjError::other("flow/ping: flow is not running"));
        }
        let runtime_guard = lock_mutex(&flow.runtime);
        let Some(runtime) = runtime_guard.as_ref() else {
            return Err(RjError::other("flow/ping: flow is not running"));
        };
        for pid in &flow.def.proc_order {
            let Some(p) = runtime.procs.get(pid) else { continue };
            let reply = Arc::new(Chan::new(BufferPolicy::Fixed(1)));
            let mut cmd = PMap::new();
            cmd.insert(kw("op"), plain_kw("ping"));
            cmd.insert(kw("reply-chan"), Value::Channel(reply.clone()));
            chan_put(&p.control_chan, Value::Map(cmd));
            pending.push((pid.clone(), reply));
        }
    }

    let deadline = crate::clock::clock_now() + Duration::from_millis(timeout_ms);
    let mut result = PMap::new();
    while !pending.is_empty() {
        pending.retain(|(pid, rc)| match chan_try_take(rc) {
            TryTake::Received(v) => {
                result.insert(pid.clone(), v);
                false
            }
            _ => true,
        });
        if pending.is_empty() || crate::clock::clock_now() >= deadline {
            break;
        }
        // L5/W3 fence #17 -- see `warn_sim_os_poll_loop`. `park_tick` DOES
        // arm a timer, so this lap is not the unreachable-deadline shape by
        // itself; what makes it out of contract in sim is the CALLER being an
        // OS thread, which means the shard can call the world quiescent while
        // this thread is between laps and jump virtual time past the replies
        // it is collecting (P6b F2, the same mechanism fence #3 routes
        // around). Named at the lap because that is where it is observable.
        if !crate::runtime::in_task() {
            static WARNED: AtomicBool = AtomicBool::new(false);
            warn_sim_os_poll_loop("flow/ping (thread caller)", &WARNED);
        }
        park_tick(PING_POLL.as_millis() as u64);
    }
    Ok(Value::Map(result))
}

/// `flow/ping-proc`: same as `flow/ping` but for a single pid, returning
/// that proc's reply map directly (`nil` on timeout). De-polled the same way
/// as [`native_ping`] -- see that function's doc.
fn native_ping_proc(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(RjError::arity(format!("flow/ping-proc: expected 2 or 3 arguments, got {}", args.len())));
    }
    let flow = expect_flow(&args[0], "flow/ping-proc")?;
    let pid = args[1].clone();
    let timeout_ms = require_timeout_ms(args.get(2), "flow/ping-proc")?;

    let reply = {
        if *lock_mutex(&flow.phase) != FlowPhase::Running {
            return Err(RjError::other("flow/ping-proc: flow is not running"));
        }
        let runtime_guard = lock_mutex(&flow.runtime);
        let Some(runtime) = runtime_guard.as_ref() else {
            return Err(RjError::other("flow/ping-proc: flow is not running"));
        };
        let p = runtime
            .procs
            .get(&pid)
            .ok_or_else(|| RjError::other(format!("flow/ping-proc: unknown pid {}", crate::printer::pr_str(&pid))))?;
        let reply = Arc::new(Chan::new(BufferPolicy::Fixed(1)));
        let mut cmd = PMap::new();
        cmd.insert(kw("op"), plain_kw("ping"));
        cmd.insert(kw("reply-chan"), Value::Channel(reply.clone()));
        chan_put(&p.control_chan, Value::Map(cmd));
        reply
    };

    let deadline = crate::clock::clock_now() + Duration::from_millis(timeout_ms);
    loop {
        if let TryTake::Received(v) = chan_try_take(&reply) {
            return Ok(v);
        }
        if crate::clock::clock_now() >= deadline {
            return Ok(Value::Nil);
        }
        // L5/W3 fence #17 -- see [`native_ping`]'s identical lap.
        if !crate::runtime::in_task() {
            static WARNED: AtomicBool = AtomicBool::new(false);
            warn_sim_os_poll_loop("flow/ping-proc (thread caller)", &WARNED);
        }
        park_tick(PING_POLL.as_millis() as u64);
    }
}

/// `flow/inject [pid io-id] msgs`: eagerly materializes `msgs` on the
/// calling thread (so laziness is forced here, not on the spawned thread,
/// keeping that thread interp-free), then spawns a thread that
/// blocking-puts each in order onto the target in-port and resolves the
/// returned future to `nil`. The target is looked up in `initial_ins` --
/// the wiring `start` computed BEFORE any proc's own `init` may have
/// overridden that port via `::flow/in-ports` (a test/caller that supplies
/// its own external in-port already holds that chan directly and has no
/// need for `inject`).
///
/// An injection is a THIRD writer on its own thread, so it always goes to
/// the general `Chan` -- including when that in-port's conn traffic has
/// moved to a 1:1 transport, in which case the chan is that port's
/// injection side channel and the proc drains both (see the module doc's
/// "Transport selection" section, and [`InLane::side_take`] for why the
/// gate is stored AFTER each put).
///
/// **FutureCell await: CLOSED in W2b.** W2's VERIFY item (design §3.7's
/// "whether `FutureCell` await has a task arm") confirmed a real gap here:
/// the `Value::Future` this returns is deref'd through `builtins::atoms`'s
/// `deref` dispatch into `builtins::conc::future_deref`, which had no task
/// arm at all, so `(<!! (go (deref (flow/inject fl [:a :in] msgs))))` parked
/// a task on a raw `Condvar` and burned its whole shard until the injector
/// thread resolved the cell. W2b gave `future_deref`/`promise_deref` proper
/// task arms (wake-driven with no timeout, bounded-poll with one) and made
/// every resolver drain the cell's task wakers -- including this one, which
/// calls `conc::resolve_future` below rather than storing the state itself.
/// `native_inject`'s own injector thread is unchanged: a one-shot OS thread
/// by design.
fn native_inject(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if args.len() != 3 {
        return Err(RjError::arity(format!(
            "flow/inject: expected 3 arguments (flow [pid io-id] msgs), got {}",
            args.len()
        )));
    }
    let flow = expect_flow(&args[0], "flow/inject")?;
    let target = expect_vector(&args[1], "flow/inject")?;
    if target.len() != 2 {
        return Err(RjError::other("flow/inject: expected [pid io-id]"));
    }
    let pid = target[0].clone();
    let io_id = target[1].clone();
    let msgs = materialize(interp, &args[2])?;

    let (chan, gate) = {
        if *lock_mutex(&flow.phase) != FlowPhase::Running {
            return Err(RjError::other("flow/inject: flow is not running"));
        }
        let runtime_guard = lock_mutex(&flow.runtime);
        let Some(runtime) = runtime_guard.as_ref() else {
            return Err(RjError::other("flow/inject: flow is not running"));
        };
        let port = runtime.initial_ins.get(&(pid.clone(), io_id.clone())).ok_or_else(|| {
            RjError::other(format!(
                "flow/inject: no wired in-port {} on {}",
                crate::printer::pr_str(&io_id),
                crate::printer::pr_str(&pid)
            ))
        })?;
        (port.chan.clone(), port.gate.clone())
    };

    let cell = Arc::new(FutureCell::pending());
    let thread_cell = cell.clone();
    let body = move || {
        for m in msgs {
            chan_put(&chan, m);
            // AFTER the put, never before: the consumer clears this
            // flag before it drains, so a store that follows the put
            // can never be swallowed by a clear that preceded it. See
            // `InLane::side_take`. `None` = this port has no transport,
            // so nobody is gating on the chan at all.
            if let Some(g) = gate.as_ref() {
                g.store(true, Ordering::Release);
            }
        }
        // L3/W2b: wakes the condvar AND drains the cell's task wakers,
        // so a `go` block awaiting this future is resumed rather than
        // stranded (see `builtins::conc::future_deref`'s ordering
        // proof).
        crate::builtins::conc::resolve_future(&thread_cell, FutureState::Done(Value::Nil));
    };
    // **L5/W3 fence #3 (design §4): in sim, a TASK, not an OS thread.** The
    // injector is the exhibit P6b's F2 was written against
    // (`p6b-fanin.mova`): between two `alts!!` calls the shard saw an empty
    // runnable set, called the world quiescent, and jumped virtual time 3000
    // ms straight onto the deadline the injection was racing -- so the
    // second message never arrived. As a task the injector IS the task
    // world, the shard cannot be idle while it is runnable, and its puts
    // join the seeded schedule. Its blocking `chan_put`s become parks, which
    // is exactly what a task is for. Real mode keeps the one-shot OS thread.
    if crate::clock::sim_enabled() {
        crate::runtime::spawn(body);
        return Ok(Value::Future(cell));
    }
    std::thread::Builder::new()
        .name("flow-inject".to_string())
        .spawn(crate::memstat::drained(body))
        .map_err(|e| RjError::other(format!("flow/inject: couldn't spawn thread: {e}")))?;
    Ok(Value::Future(cell))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Registers a `flow/`-prefixed-ONLY native (no bare-name alias -- unlike
/// `clojure.string`'s natives in `strings.rs`, these deliberately do NOT
/// also land as bare globals: names like `start`/`stop`/`ping` are far more
/// likely to collide with user code than `upper-case`/`split`). `full_name`
/// is only used as the `NativeFn`'s diagnostic name (arity/error messages);
/// the actual lookup key is the namespaced `Symbol` built from `bare`.
#[track_caller]
pub(crate) fn reg_flow(i: &mut Interp, bare: &'static str, full_name: &'static str, f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static) {
    let native = NativeFn::new(full_name, f);
    // DESIGN-flow-namespace.md Part 1 point 1: the natives' one true home
    // is the canonical namespace, `clojure.core.async.flow` -- NOT the
    // `flow` spelling (which is now merely a default alias, see
    // `ns.rs`'s `DEFAULT_ALIASES`). `full_name`'s diagnostic strings
    // (arity/error messages) still read "flow/..." -- unchanged, cosmetic
    // only.
    i.globals.set_builtin(
        Symbol { ns: Some(Str::from("clojure.core.async.flow")), name: Str::from(bare) },
        Value::Native(Arc::new(native)),
    );
}

pub fn register(i: &mut Interp) {
    reg(i, "flow?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Flow(_)))));
    // DESIGN-flow-namespace.md Part 1 point 1: `flow?` also gets a home in
    // `clojure.core.async.flow` (same cell as the bare registration above,
    // via `bind_alias` -- not a second `reg_flow` registration, which
    // would create an INDEPENDENT cell and split identity). Its bare
    // spelling is unaffected: `for_each_global_candidate`'s trailing
    // bare-name probe still finds it directly. This is what keeps
    // `(flow/flow? x)` alive once item 5 (`ns: restrict qualified->bare
    // fallback to clojure.core spellings`) narrows that trailing probe to
    // only fire when the EXPANDED namespace is `clojure.core` -- `flow?`'s
    // expanded namespace is `clojure.core.async.flow`, which needs its own
    // real entry to keep resolving.
    if let Some(cell) = i.globals.find_bound_cell(&Symbol::simple("flow?")) {
        crate::srcindex::note_var_alias("clojure.core.async.flow", "flow?", "flow?");
        i.globals.bind_alias(
            Symbol { ns: Some(Str::from("clojure.core.async.flow")), name: Str::from("flow?") },
            cell,
        );
    }

    reg_flow(i, "create-flow", "flow/create-flow", native_create_flow);
    reg_flow(i, "start", "flow/start", native_start);
    reg_flow(i, "stop", "flow/stop", native_stop);
    reg_flow(i, "pause", "flow/pause", native_pause);
    reg_flow(i, "resume", "flow/resume", native_resume);
    reg_flow(i, "pause-proc", "flow/pause-proc", native_pause_proc);
    reg_flow(i, "resume-proc", "flow/resume-proc", native_resume_proc);
    // mova-native (L4 W3): upstream has no per-proc stop -- see the fn's doc.
    reg_flow(i, "stop-proc", "flow/stop-proc", native_stop_proc);
    reg_flow(i, "ping", "flow/ping", native_ping);
    reg_flow(i, "ping-proc", "flow/ping-proc", native_ping_proc);
    reg_flow(i, "inject", "flow/inject", native_inject);
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the N2 promotion DECISION itself. Every other
    //! native-step test is a differential (fast vs generic must agree),
    //! and a differential can never fail if promotion silently never
    //! happens -- falling back to the generic loop is always *correct*.
    //! These tests are what assert the tier is actually on, in the same
    //! spirit as `crate::compile`'s `compiles_its_documented_surface`.
    use super::*;

    fn passthrough_step_fn() -> Value {
        let mut interp = Interp::new();
        interp.eval_str("promotion-test", "(flow/step-passthrough)").expect("step-passthrough")
    }

    fn one_chan() -> Arc<Chan> {
        Arc::new(Chan::new(BufferPolicy::Fixed(1)))
    }

    fn single_out() -> HashMap<Value, Option<Arc<Chan>>> {
        let mut outs = HashMap::new();
        outs.insert(plain_kw("out"), Some(one_chan()));
        outs
    }

    fn single_read_set() -> Vec<(Value, Arc<Chan>)> {
        vec![(plain_kw("in"), one_chan())]
    }

    #[test]
    fn a_single_in_single_out_native_step_is_promoted() {
        if faststep_disabled_by_env() {
            // `MOVA_NO_FASTSTEP=1 cargo test` is a supported mode (it
            // proves the suite passes with the tier off); this one test
            // asserts the tier IS on, so it has nothing to say there.
            return;
        }
        let sf = passthrough_step_fn();
        let state = Value::Map(PMap::new());
        assert!(try_promote_fast(&sf, &state, &single_read_set(), None, &single_out()).is_some());
        // ...and with no out port at all (a sink), still promoted.
        assert!(try_promote_fast(&sf, &state, &single_read_set(), None, &HashMap::new()).is_some());
    }

    /// Design Part 2: `native_start` now wires `::flow/report`/`::flow/error`
    /// into EVERY proc's outs map, so a real `outs` promotion sees at
    /// `flow/start` time always has 2 (sink) or 3 (declared-out) entries,
    /// never 0 or 1. N2 promotion must still fire -- `real_outs` is what
    /// excludes the two reserved keys from the "at most one out port" count
    /// (see `try_promote_fast`'s doc); this is the regression that a bare
    /// `outs.len() > 1` check (pre-Part-2 shape) would introduce, pinned
    /// directly rather than only via the existing `single_out`/`HashMap::new`
    /// cases above, neither of which ever carried the reserved keys.
    #[test]
    fn promotion_still_fires_with_the_two_reserved_out_keys_present() {
        if faststep_disabled_by_env() {
            return;
        }
        let sf = passthrough_step_fn();
        let state = Value::Map(PMap::new());

        // sink shape: only the two reserved keys, no declared out port.
        let mut reserved_only = HashMap::new();
        reserved_only.insert(report_out_key(), Some(one_chan()));
        reserved_only.insert(error_out_key(), Some(one_chan()));
        assert!(try_promote_fast(&sf, &state, &single_read_set(), None, &reserved_only).is_some());

        // declared-out shape: one real port plus both reserved keys.
        let mut with_reserved = single_out();
        with_reserved.insert(report_out_key(), Some(one_chan()));
        with_reserved.insert(error_out_key(), Some(one_chan()));
        assert!(try_promote_fast(&sf, &state, &single_read_set(), None, &with_reserved).is_some());

        // two DECLARED out ports plus both reserved keys must still refuse
        // (the reserved keys must never mask a genuine multi-out proc).
        let mut two_real_plus_reserved = with_reserved.clone();
        two_real_plus_reserved.insert(plain_kw("out2"), Some(one_chan()));
        assert!(try_promote_fast(&sf, &state, &single_read_set(), None, &two_real_plus_reserved).is_none());
    }

    #[test]
    fn every_documented_precondition_falls_back_to_the_generic_loop() {
        let sf = passthrough_step_fn();
        let state = Value::Map(PMap::new());

        // not a native step at all (an interpreted `map->step` shell)
        let mut interp = Interp::new();
        let generic = interp
            .eval_str(
                "promotion-test",
                "(flow/map->step* {:describe (fn [] {}) :transform (fn [s _ m] [s {}])})",
            )
            .expect("map->step*");
        assert!(try_promote_fast(&generic, &state, &single_read_set(), None, &single_out()).is_none());

        // multi-input
        let mut two_ins = single_read_set();
        two_ins.push((plain_kw("in2"), one_chan()));
        assert!(try_promote_fast(&sf, &state, &two_ins, None, &single_out()).is_none());

        // zero inputs
        assert!(try_promote_fast(&sf, &state, &[], None, &single_out()).is_none());

        // an `::flow/input-filter` in the state (the read set could change
        // under the loop, which `run_proc_fast` structurally can't handle)
        let filter = Value::Keyword(Keyword::from("some-pred"));
        assert!(try_promote_fast(&sf, &state, &single_read_set(), Some(&filter), &single_out()).is_none());

        // more than one out port (FastOut carries no out-id)
        let mut two_outs = single_out();
        two_outs.insert(plain_kw("out2"), Some(one_chan()));
        assert!(try_promote_fast(&sf, &state, &single_read_set(), None, &two_outs).is_none());
    }

    // -----------------------------------------------------------------
    // P3: the fusion PLAN itself. Same reasoning as the promotion tests
    // above -- every fusion differential still passes if fusion silently
    // never happens, so the plan is what has to be asserted on directly.
    // -----------------------------------------------------------------

    /// A step-fn source expression with the given declared ports, e.g.
    /// `step(":ins {:in {}} :outs {:out {}}")`.
    fn step(ports: &str) -> String {
        format!("(flow/process (flow/map->step {{:describe (fn [] {{{ports}}}) :transform (fn [s _ m] [s {{}}])}}))")
    }

    /// `create-flow`s `src` and returns the fusion plan (under `policy`,
    /// which the process-wide `OnceLock` would otherwise pin for the
    /// suite's whole life) as printed pids, e.g. `[[":a", ":b"], [":c"]]`.
    fn plan_for_with(src: &str, policy: FusionPolicy) -> Vec<Vec<String>> {
        let mut interp = Interp::new();
        let v = interp
            .eval_str("fusion-test", src)
            .unwrap_or_else(|e| panic!("create-flow failed: {}", crate::error::render(&e, "fusion-test", src)));
        let Value::Flow(f) = v else { panic!("expected a flow value") };
        plan_fusion_with(&f.def, policy).iter().map(|run| run.iter().map(crate::printer::pr_str).collect()).collect()
    }

    /// Topology tests use `map->step` procs (the only way to declare
    /// arbitrary port shapes), so they ask for `FusionPolicy::All` -- the
    /// DEFAULT policy's extra "every member is a native step" gate is
    /// asserted separately by
    /// `the_default_policy_only_plans_runs_of_native_steps`.
    fn plan_for(src: &str) -> Vec<Vec<String>> {
        plan_for_with(src, FusionPolicy::All)
    }

    #[test]
    fn fusion_plan_collapses_a_maximal_unbranching_chain_into_one_run() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        let plan = plan_for(&format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc {relay}}} :b {{:proc {relay}}} :c {{:proc {relay}}} :d {{:proc {sink}}}}}
                   :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]] [[:c :out] [:d :in]]]}})"#
        ));
        assert_eq!(plan, vec![vec![":a", ":b", ":c", ":d"]]);
    }

    #[test]
    fn fusion_plan_refuses_fan_out_fan_in_multi_port_and_self_loop_topologies() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");

        // fan-out: :a's single :out feeds TWO destinations (a mult), so
        // neither destination is 1:1 with it.
        assert_eq!(
            plan_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:a {{:proc {relay}}} :b {{:proc {sink}}} :c {{:proc {sink}}}}}
                       :conns [[[:a :out] [:b :in]] [[:a :out] [:c :in]]]}})"#
            )),
            vec![vec![":a"], vec![":b"], vec![":c"]]
        );

        // fan-in: :c has two declared in-ports, so it is never fusable onto
        // either feeder (and neither feeder's out is 1:1 with a sole in).
        let two_in = step(":ins {:in-a {} :in-b {}} :outs {}");
        assert_eq!(
            plan_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:a {{:proc {relay}}} :b {{:proc {relay}}} :c {{:proc {two_in}}}}}
                       :conns [[[:a :out] [:c :in-a]] [[:b :out] [:c :in-b]]]}})"#
            )),
            vec![vec![":a"], vec![":b"], vec![":c"]]
        );

        // two out-ports on the upstream: even though only ONE of them is
        // connected, the other could emit at any time and would need a
        // chan of its own -- refused (this is `try_promote_fast`'s
        // "at most one out port" rule, restated at chain level).
        let two_out = step(":ins {:in {}} :outs {:out {} :other {}}");
        assert_eq!(
            plan_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:a {{:proc {two_out}}} :b {{:proc {sink}}}}}
                       :conns [[[:a :out] [:b :in]]]}})"#
            )),
            vec![vec![":a"], vec![":b"]]
        );

        // self-loop: always routed via mult, never fused (and the shape is
        // multi-in/multi-out anyway).
        let looper = step(":ins {:in {} :self-in {}} :outs {:self-out {} :out {}}");
        assert_eq!(
            plan_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:l {{:proc {looper}}} :s {{:proc {sink}}}}}
                       :conns [[[:l :self-out] [:l :self-in]] [[:l :out] [:s :in]]]}})"#
            )),
            vec![vec![":l"], vec![":s"]]
        );
    }

    #[test]
    fn fusion_plan_partitions_a_flow_into_several_runs_around_a_fan_out() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        // :a fans out to :b and :x; each of those then has its OWN 1:1
        // tail, so the flow is three runs, not one and not five.
        let plan = plan_for(&format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc {relay}}}
                          :b {{:proc {relay}}} :c {{:proc {sink}}}
                          :x {{:proc {relay}}} :y {{:proc {sink}}}}}
                   :conns [[[:a :out] [:b :in]] [[:a :out] [:x :in]]
                           [[:b :out] [:c :in]] [[:x :out] [:y :in]]]}})"#
        ));
        assert_eq!(plan, vec![vec![":a"], vec![":b", ":c"], vec![":x", ":y"]]);
    }

    #[test]
    fn the_default_policy_only_plans_runs_of_native_steps() {
        // The measured default (see `FusionPolicy`): a chain of native
        // steps is fused...
        let native = r#"(flow/create-flow
             {:procs {:a {:proc (flow/process (flow/step-count))}
                      :b {:proc (flow/process (flow/step-passthrough))}
                      :c {:proc (flow/process (flow/step-sink-deliver 1 (promise)))}}
              :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]})"#;
        assert_eq!(plan_for_with(native, FusionPolicy::PromotedOnly), vec![vec![":a", ":b", ":c"]]);

        // ...while the identical TOPOLOGY built from interpreted
        // `map->step` procs is not (it would lose more in pipeline
        // parallelism than it saves in channel hops), even though
        // `MOVA_FUSE_ALL=1` would fuse it.
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        let interpreted = format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc {relay}}} :b {{:proc {relay}}} :c {{:proc {sink}}}}}
                   :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}})"#
        );
        assert_eq!(
            plan_for_with(&interpreted, FusionPolicy::PromotedOnly),
            vec![vec![":a"], vec![":b"], vec![":c"]]
        );
        assert_eq!(plan_for_with(&interpreted, FusionPolicy::All), vec![vec![":a", ":b", ":c"]]);

        // A mixed chain is refused by the default policy at the boundary
        // only: the two native procs still fuse with each other.
        let mixed = format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc (flow/process (flow/step-count))}}
                          :b {{:proc (flow/process (flow/step-passthrough))}}
                          :c {{:proc {sink}}}}}
                   :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}})"#
        );
        assert_eq!(plan_for_with(&mixed, FusionPolicy::PromotedOnly), vec![vec![":a", ":b"], vec![":c"]]);
    }

    // -----------------------------------------------------------------
    // T2: WHICH conns leave the general `Chan` for the 1:1 transport.
    //
    // Same reasoning as the two plan-test blocks above, and one degree
    // stronger: a behavioral differential can never fail if the transport
    // is silently never selected (falling back to `Chan` is always
    // *correct* and always *slower*), so both the positive and -- more
    // importantly -- the NEGATIVE cases have to be asserted on the
    // decision itself. `flow-fanout5` is this file's historical canary for
    // wake regressions (bench/optimization-log.md's notify-elision entry);
    // `fanout_conns_are_never_converted` is what pins its topology to the
    // general `Chan` by test rather than by hope.
    // -----------------------------------------------------------------

    /// `create-flow`s `src` and returns the transport links selected under
    /// `policy`'s fusion plan, as printed `"pid/port -> pid/port"` strings.
    fn links_for_with(src: &str, policy: FusionPolicy) -> Vec<String> {
        links_for_full(src, policy, false)
    }

    /// [`links_for_with`] with the SIM switch passed in too -- `false` is
    /// real mode (where, as of L3.6/W1, task-hood no longer narrows
    /// anything) and `true` is `(simulate {:seed ..})`, which refuses every
    /// lane. This parameter used to be L3's task-proc switch; the clause it
    /// drove is gone.
    fn links_for_full(src: &str, policy: FusionPolicy, sim: bool) -> Vec<String> {
        let mut interp = Interp::new();
        let v = interp
            .eval_str("transport-test", src)
            .unwrap_or_else(|e| panic!("create-flow failed: {}", crate::error::render(&e, "transport-test", src)));
        let Value::Flow(f) = v else { panic!("expected a flow value") };
        let runs = plan_fusion_with(&f.def, policy);
        plan_transport_links_with(&f.def, &runs, true, sim)
            .iter()
            .map(|((fp, fo), (tp, to))| {
                format!(
                    "{}/{} -> {}/{}",
                    crate::printer::pr_str(fp),
                    crate::printer::pr_str(fo),
                    crate::printer::pr_str(tp),
                    crate::printer::pr_str(to)
                )
            })
            .collect()
    }

    /// Topology tests use interpreted `map->step` procs (the only way to
    /// declare arbitrary port shapes); under the DEFAULT fusion policy
    /// those never fuse, so every topologically-eligible conn is visible.
    fn links_for(src: &str) -> Vec<String> {
        links_for_with(src, FusionPolicy::PromotedOnly)
    }

    #[test]
    fn every_hop_of_an_unbranching_11_proc_chain_is_converted() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        let procs: String = (0..10).map(|i| format!(":r{i} {{:proc {relay}}} ")).collect();
        let conns: String = (0..9).map(|i| format!("[[:r{i} :out] [:r{} :in]] ", i + 1)).collect();
        let links = links_for(&format!(
            r#"(flow/create-flow
                 {{:procs {{{procs} :sink {{:proc {sink}}}}}
                   :conns [{conns} [[:r9 :out] [:sink :in]]]}})"#
        ));
        assert_eq!(links.len(), 10, "got {links:?}");
        assert_eq!(links[0], ":r0/:out -> :r1/:in");
        assert_eq!(links[9], ":r9/:out -> :sink/:in");
    }

    /// THE CANARY. `bench/flow-fanout5.mova`'s exact topology: one generator
    /// whose single out-port feeds five sinks through a native mult.
    /// The mult is the writer on every one of those five destination chans,
    /// so not one of them is 1:1 and not one may be converted.
    #[test]
    fn fanout_conns_are_never_converted() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        let procs: String = (0..5).map(|i| format!(":s{i} {{:proc {sink}}} ")).collect();
        let conns: String = (0..5).map(|i| format!("[[:gen :out] [:s{i} :in]] ")).collect();
        assert_eq!(
            links_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:gen {{:proc {relay}}} {procs}}}
                       :conns [{conns}]}})"#
            )),
            Vec::<String>::new()
        );
    }

    #[test]
    fn fan_in_self_loop_multi_port_and_zero_buffer_conns_are_never_converted() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");

        // Fan-in: one shared chan per DESTINATION in-port means :c's :in
        // has two writer threads. This is the specific shape the 3e83441
        // wiring fix introduced and the one an SPSC ring cannot carry.
        assert_eq!(
            links_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:a {{:proc {relay}}} :b {{:proc {relay}}} :c {{:proc {sink}}}}}
                       :conns [[[:a :out] [:c :in]] [[:b :out] [:c :in]]]}})"#
            )),
            Vec::<String>::new()
        );

        // Self-loop: always routed via mult, so the writer is that thread.
        let looper = step(":ins {:in {} :self-in {}} :outs {:self-out {} :out {}}");
        assert_eq!(
            links_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:l {{:proc {looper}}} :s {{:proc {sink}}}}}
                       :conns [[[:l :self-out] [:l :self-in]] [[:l :out] [:s :in]]]}})"#
            )),
            Vec::<String>::new()
        );

        // Multi-port on either end: at most one lane per proc per
        // direction, so a proc declaring two ins (or two outs) keeps every
        // conn on the general Chan and the multi-input round-robin never
        // has to carry more than one transport.
        let two_in = step(":ins {:in-a {} :in-b {}} :outs {}");
        assert_eq!(
            links_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:a {{:proc {relay}}} :c {{:proc {two_in}}}}}
                       :conns [[[:a :out] [:c :in-a]]]}})"#
            )),
            Vec::<String>::new()
        );
        let two_out = step(":ins {:in {}} :outs {:out {} :other {}}");
        assert_eq!(
            links_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:a {{:proc {two_out}}} :b {{:proc {sink}}}}}
                       :conns [[[:a :out] [:b :in]]]}})"#
            )),
            Vec::<String>::new()
        );

        // `:buf-or-n 0` builds a `BufferPolicy::Fixed(0)` whose put can
        // never find room. Converting it would CHANGE that (a rendezvous
        // works where Fixed(0) wedges), so it is left exactly as it is --
        // see the module doc's "Buffers" note.
        assert_eq!(
            links_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:a {{:proc {relay}}}
                              :b {{:proc {sink} :chan-opts {{:in {{:buf-or-n 0}}}}}}}}
                       :conns [[[:a :out] [:b :in]]]}})"#
            )),
            Vec::<String>::new()
        );
        // ...while :buf-or-n 1 is perfectly ordinary and IS converted.
        assert_eq!(
            links_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:a {{:proc {relay}}}
                              :b {{:proc {sink} :chan-opts {{:in {{:buf-or-n 1}}}}}}}}
                       :conns [[[:a :out] [:b :in]]]}})"#
            )),
            vec![":a/:out -> :b/:in".to_string()]
        );
    }

    /// A repeated identical conn must produce ONE link, not two -- the same
    /// collapse `start`'s wiring does before deciding 1:1-vs-mult.
    #[test]
    fn a_duplicated_conn_is_one_link_not_two() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        assert_eq!(
            links_for(&format!(
                r#"(flow/create-flow
                     {{:procs {{:a {{:proc {relay}}} :b {{:proc {sink}}}}}
                       :conns [[[:a :out] [:b :in]] [[:a :out] [:b :in]]]}})"#
            )),
            vec![":a/:out -> :b/:in".to_string()]
        );
    }

    /// fused > transport > `Chan`, as a decision rather than as prose. The
    /// SAME topology yields no links when its procs share a fused thread
    /// (the hop is already gone) and every link when they do not.
    #[test]
    fn the_priority_order_between_fusion_and_the_transport_is_strict() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        let src = format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc {relay}}} :b {{:proc {relay}}} :c {{:proc {sink}}}}}
                   :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}})"#
        );
        // MOVA_FUSE_ALL's policy fuses this interpreted chain into one run
        // -> no channel hop to accelerate, so no link.
        assert_eq!(links_for_with(&src, FusionPolicy::All), Vec::<String>::new());
        // The default policy leaves interpreted procs unfused -> both hops
        // are real, and both are converted.
        assert_eq!(
            links_for_with(&src, FusionPolicy::PromotedOnly),
            vec![":a/:out -> :b/:in".to_string(), ":b/:out -> :c/:in".to_string()]
        );
        // And the kill switch overrides everything.
        let mut interp = Interp::new();
        let Value::Flow(f) = interp.eval_str("transport-test", &src).expect("create-flow") else {
            panic!("expected a flow")
        };
        let runs = plan_fusion_with(&f.def, FusionPolicy::PromotedOnly);
        assert!(plan_transport_links(&f.def, &runs, false).is_empty());
    }

    /// A run that fuses only PART of a flow still converts the conns
    /// outside it -- including the one crossing the run's boundary.
    #[test]
    fn conns_at_the_boundary_of_a_fused_run_stay_on_the_general_chan() {
        // :a -> :b fuses (both native steps); :b -> :c does not, because
        // :c is interpreted. The boundary conn touches a fused member, so
        // it is left alone: `run_fused` is deliberately transport-unaware.
        let sink = step(":ins {:in {}} :outs {}");
        let src = format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc (flow/process (flow/step-count))}}
                          :b {{:proc (flow/process (flow/step-passthrough))}}
                          :c {{:proc {sink}}}}}
                   :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}})"#
        );
        assert_eq!(plan_for_with(&src, FusionPolicy::PromotedOnly), vec![vec![":a", ":b"], vec![":c"]]);
        assert_eq!(links_for_with(&src, FusionPolicy::PromotedOnly), Vec::<String>::new());
    }

    // -----------------------------------------------------------------
    // L3/W1a: `:workload`. And -- as of L3.6/W1 -- what task-hood no
    // longer does to the transport plan.
    //
    // The `:workload` predicate itself ([`is_task_proc`]) still decides
    // the SPAWN tier and is still pinned here. What is gone is the
    // transport-eligibility clause it used to feed: `transport.rs`'s
    // `Ring` has a task arm now (a two-source park -- see
    // [`plan_transport_links_with`] and docs/FLOW-HOP-RECOVERY.md §7), so
    // a task proc gets its lane like anybody else. The two tests below
    // that used to assert the narrowing now assert the REVERSAL, and
    // `sim_mode_refuses_every_lane` asserts the one clause that replaced
    // it -- still on the DECISION rather than on behaviour, for the same
    // reason as ever: a missing lane is a purely NEGATIVE change, falling
    // back to a general `Chan` is always correct, and no behavioural
    // differential anywhere can fail if such a clause silently stops (or
    // starts) firing.
    // -----------------------------------------------------------------

    /// [`step`] with an explicit `:workload` -- the only launcher shape
    /// that produces anything but [`Workload::Mixed`].
    fn step_workload(ports: &str, workload: &str) -> String {
        format!(
            "(flow/process (flow/map->step {{:describe (fn [] {{{ports}}}) :transform (fn [s _ m] [s {{}}])}}) \
             {{:workload {workload}}})"
        )
    }

    /// [`step`] whose `:describe` fn ALSO returns `:workload dw` alongside
    /// its ports, with launcher opts `opts` passed verbatim (`"{}"` for no
    /// opts at all) -- L3.5's opts-vs-describe-map precedence needs a
    /// launcher that can vary both `resolve_workload` inputs independently.
    fn step_describe_and_opts_workload(ports: &str, describe_workload: &str, opts: &str) -> String {
        format!(
            "(flow/process (flow/map->step {{:describe (fn [] {{{ports} :workload {describe_workload}}}) \
             :transform (fn [s _ m] [s {{}}])}}) {opts})"
        )
    }

    #[test]
    fn workload_defaults_to_mixed_and_only_io_keeps_its_thread() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let src = format!(
            r#"(flow/create-flow
                 {{:procs {{:bare {{:proc (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                                                          :transform (fn [s _ m] [s {{}}])}})}}
                          :dflt {{:proc {relay}}}
                          :io {{:proc {}}}
                          :cpu {{:proc {}}}
                          :bogus {{:proc {}}}}}
                   :conns []}})"#,
            step_workload(":ins {:in {}} :outs {:out {}}", ":io"),
            step_workload(":ins {:in {}} :outs {:out {}}", ":compute"),
            step_workload(":ins {:in {}} :outs {:out {}}", ":not-a-workload"),
        );
        let mut interp = Interp::new();
        let Value::Flow(f) = interp.eval_str("workload-test", &src).expect("create-flow") else {
            panic!("expected a flow")
        };
        let w = |pid: &str| proc_workload(f.def.procs.get(&plain_kw(pid)).expect("declared above"));
        // A bare step-fn launcher carries no workload map at all.
        assert_eq!(w("bare"), Workload::Mixed);
        // `flow/process` with no opts defaults to `:mixed` itself.
        assert_eq!(w("dflt"), Workload::Mixed);
        assert_eq!(w("io"), Workload::Io);
        assert_eq!(w("cpu"), Workload::Compute);
        // Permissive by design (see `proc_workload`): an unrecognized
        // keyword is the documented default, not a `create-flow` error.
        assert_eq!(w("bogus"), Workload::Mixed);

        // And the predicate the whole wave keys on: only `:io` opts out,
        // and under the kill switch nothing is a task at all.
        for pid in ["bare", "dflt", "io", "cpu", "bogus"] {
            let pdef = f.def.procs.get(&plain_kw(pid)).expect("declared above");
            assert!(!is_task_proc(pdef, false), "{pid} must be a thread proc under MOVA_FLOW_THREAD_PROCS=1");
            assert_eq!(is_task_proc(pdef, true), pid != "io", "{pid} took the wrong side of the default world");
        }
    }

    /// L3.5: upstream's `process` docstring precedence -- "A :workload
    /// supplied as an option to process will override any :workload
    /// returned by the :describe fn... If neither are provided the default
    /// is :mixed" -- exercised with the describe-map as the ONLY source,
    /// then with opts present too, to prove opts still wins.
    #[test]
    fn describe_map_workload_feeds_the_same_precedence_and_opts_still_wins() {
        let src = format!(
            r#"(flow/create-flow
                 {{:procs {{:describe-io {{:proc {}}}
                          :describe-io-opts-compute {{:proc {}}}
                          :describe-compute {{:proc {}}}
                          :describe-bogus {{:proc {}}}}}
                   :conns []}})"#,
            step_describe_and_opts_workload(":ins {:in {}} :outs {:out {}}", ":io", "{}"),
            step_describe_and_opts_workload(":ins {:in {}} :outs {:out {}}", ":io", "{:workload :compute}"),
            step_describe_and_opts_workload(":ins {:in {}} :outs {:out {}}", ":compute", "{}"),
            step_describe_and_opts_workload(":ins {:in {}} :outs {:out {}}", ":not-a-workload", "{}"),
        );
        let mut interp = Interp::new();
        let Value::Flow(f) = interp.eval_str("describe-workload-test", &src).expect("create-flow") else {
            panic!("expected a flow")
        };
        let w = |pid: &str| proc_workload(f.def.procs.get(&plain_kw(pid)).expect("declared above"));

        // (a) describe says :io, opts silent -> describe wins, thread kept.
        assert_eq!(w("describe-io"), Workload::Io);
        // (b) describe says :io, opts say :compute -> opts win (upstream:
        // an opts :workload overrides describe's), task.
        assert_eq!(w("describe-io-opts-compute"), Workload::Compute);
        // (c) describe says :compute, opts silent -> describe wins, task.
        assert_eq!(w("describe-compute"), Workload::Compute);
        // (d) describe returns a garbage workload, opts silent -> the same
        // permissive default an unrecognized OPTS keyword gets: :mixed.
        assert_eq!(w("describe-bogus"), Workload::Mixed);

        for (pid, is_io) in [
            ("describe-io", true),
            ("describe-io-opts-compute", false),
            ("describe-compute", false),
            ("describe-bogus", false),
        ] {
            let pdef = f.def.procs.get(&plain_kw(pid)).expect("declared above");
            assert!(!is_task_proc(pdef, false), "{pid} must be a thread proc under MOVA_FLOW_THREAD_PROCS=1");
            assert_eq!(is_task_proc(pdef, true), !is_io, "{pid} took the wrong side of the default world");
        }
    }

    /// **L3.6/W1: the reversal, asserted directly.** This test used to be
    /// `no_conn_touching_a_task_proc_is_ever_converted` and asserted the
    /// empty vector for the task world. The `Ring` now has a task arm, so a
    /// chain of default (`:mixed`, i.e. TASK) procs converts every hop --
    /// which is the whole 1.61x, since every proc is a task by default.
    #[test]
    fn every_hop_of_a_task_proc_chain_is_converted() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        let src = format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc {relay}}} :b {{:proc {relay}}} :c {{:proc {sink}}}}}
                   :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}})"#
        );
        // All three procs default to `:mixed` -- every conn has a task proc
        // at BOTH ends, and every one of them gets a lane.
        assert_eq!(
            links_for_full(&src, FusionPolicy::PromotedOnly, false),
            vec![":a/:out -> :b/:in".to_string(), ":b/:out -> :c/:in".to_string()]
        );
    }

    /// Task-hood is no longer a per-CONN narrowing in EITHER direction: a
    /// mixed chain (`:io` -> `:io` -> task -> `:io`) converts all three
    /// hops, including the two that straddle the tier boundary. Before
    /// L3.6/W1 only the first survived.
    #[test]
    fn a_hop_straddling_the_task_thread_boundary_keeps_its_lane() {
        let io_relay = step_workload(":ins {:in {}} :outs {:out {}}", ":io");
        let io_sink = step_workload(":ins {:in {}} :outs {}", ":io");
        let task_relay = step(":ins {:in {}} :outs {:out {}}");
        let src = format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc {io_relay}}} :b {{:proc {io_relay}}}
                          :c {{:proc {task_relay}}} :d {{:proc {io_sink}}}}}
                   :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]] [[:c :out] [:d :in]]]}})"#
        );
        assert_eq!(
            links_for_full(&src, FusionPolicy::PromotedOnly, false),
            vec![
                ":a/:out -> :b/:in".to_string(),
                ":b/:out -> :c/:in".to_string(),
                ":c/:out -> :d/:in".to_string()
            ]
        );
    }

    /// **The L5 determinism fence, as a planner assertion.** `(simulate
    /// {:seed ..})` gets NO lane, ever -- a lock-free lane is
    /// schedule-dependent by construction and sim is a determinism path, not
    /// a throughput path. Before L3.6/W1 sim got no lane only as a side
    /// effect of the task exclusion (the L5 fences put every simulated proc
    /// on a task); now it is a clause of its own, so the byte-identical-run
    /// contract does not depend on an unrelated rule staying put.
    ///
    /// A NEGATIVE decision, so it has to be asserted on the planner --
    /// falling back to `Chan` is always *correct*, and no behavioural
    /// differential could ever fail on it.
    #[test]
    fn sim_mode_refuses_every_lane() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        let src = format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc {relay}}} :b {{:proc {relay}}} :c {{:proc {sink}}}}}
                   :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}})"#
        );
        // Real mode: two lanes (the test just above).
        assert_eq!(links_for_full(&src, FusionPolicy::PromotedOnly, false).len(), 2);
        // Sim: none.
        assert_eq!(links_for_full(&src, FusionPolicy::PromotedOnly, true), Vec::<String>::new());
    }

    /// The kill switch still outranks everything: `MOVA_NO_SPSC=1` means
    /// no lane, in real mode or sim.
    #[test]
    fn the_spsc_kill_switch_still_outranks_everything() {
        let io_relay = step_workload(":ins {:in {}} :outs {:out {}}", ":io");
        let io_sink = step_workload(":ins {:in {}} :outs {}", ":io");
        let src = format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc {io_relay}}} :b {{:proc {io_sink}}}}}
                   :conns [[[:a :out] [:b :in]]]}})"#
        );
        let mut interp = Interp::new();
        let Value::Flow(f) = interp.eval_str("transport-test", &src).expect("create-flow") else {
            panic!("expected a flow")
        };
        let runs = plan_fusion_with(&f.def, FusionPolicy::PromotedOnly);
        assert_eq!(plan_transport_links_with(&f.def, &runs, true, false).len(), 1);
        assert!(plan_transport_links_with(&f.def, &runs, false, false).is_empty());
        assert!(plan_transport_links_with(&f.def, &runs, false, true).is_empty());
    }

    // -----------------------------------------------------------------
    // L3/W3: segment placement. Asserted on the DECISION, for the same
    // reason the transport block above is: placement is a performance
    // choice that no behavioral test can fail on -- every shard runs
    // every task correctly, just faster or slower.
    // -----------------------------------------------------------------

    /// A chain `:p0 -> :p1 -> ... -> :pn`, unfused (interpreted steps under
    /// [`FusionPolicy::Off`]), as `(def, one run per proc in walk order)`.
    fn chain_def(n: usize) -> (Value, Vec<Vec<Value>>) {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let sink = step(":ins {:in {}} :outs {}");
        let procs: String = (0..n)
            .map(|i| format!(":p{i} {{:proc {}}} ", if i + 1 == n { &sink } else { &relay }))
            .collect();
        let conns: String = (0..n.saturating_sub(1)).map(|i| format!("[[:p{i} :out] [:p{} :in]] ", i + 1)).collect();
        let src = format!("(flow/create-flow {{:procs {{{procs}}} :conns [{conns}]}})");
        let mut interp = Interp::new();
        let Value::Flow(f) = interp.eval_str("placement-test", &src).expect("create-flow") else {
            panic!("expected a flow")
        };
        let runs = plan_fusion_with(&f.def, FusionPolicy::Off);
        (Value::Flow(f), runs)
    }

    /// `chain_def`'s flow value, as the `FlowDef` `segment_shards` wants
    /// (`FlowDef` is not `Clone`, and the cell has to outlive the borrow).
    fn def_of(v: &Value) -> &FlowDef {
        let Value::Flow(f) = v else { panic!("expected a flow") };
        &f.def
    }

    /// The headline claim of [`segment_shards`]: contiguous segments, so a
    /// chain longer than the machine is mostly co-shard. Nine procs on three
    /// shards = three runs per shard = two cross-shard boundaries, not eight.
    #[test]
    fn segments_are_contiguous_so_most_neighbors_are_co_shard() {
        let (flow, runs) = chain_def(9);
        let placed = segment_shards(def_of(&flow), &runs, &vec![true; runs.len()], 0, 3, MAX_SEGMENT_LEN);
        let shards: Vec<usize> = placed.iter().map(|s| s.expect("all task runs")).collect();
        assert_eq!(shards, vec![0, 0, 0, 1, 1, 1, 2, 2, 2]);
        let boundaries = shards.windows(2).filter(|w| w[0] != w[1]).count();
        assert_eq!(boundaries, 2, "one per segment boundary, not one per hop");
    }

    /// The LENGTH cap ([`MAX_SEGMENT_LEN`]), which is the whole reason
    /// there can be more segments than shards. 40 procs, 4 shards, cap 8:
    /// five segments of eight, and shard 0 gets two of them -- the two ends
    /// of the chain, which is exactly the point (they never hand each other
    /// a message, so they do not serialize).
    #[test]
    fn a_long_chain_gets_more_segments_than_there_are_shards() {
        let (flow, runs) = chain_def(40);
        // `runs` is in `proc_order`, which is pr_str-sorted (:p0 :p1 :p10
        // ... :p2 ...), so read the answer back in CHAIN order -- that is
        // the order the claim is about, and the ordering `segment_shards`
        // exists to recover.
        let by_pid: HashMap<String, usize> =
            runs.iter().enumerate().map(|(j, r)| (crate::printer::display_str(&r[0]), j)).collect();
        let chain_order = |placed: &Vec<Option<usize>>| -> Vec<usize> {
            (0..40).map(|i| placed[by_pid[&format!(":p{i}")]].expect("all task runs")).collect()
        };

        let placed = segment_shards(def_of(&flow), &runs, &vec![true; runs.len()], 0, 4, 8);
        let mut want = Vec::new();
        for seg in [0, 1, 2, 3, 0] {
            want.extend(std::iter::repeat_n(seg, 8));
        }
        assert_eq!(chain_order(&placed), want);
        // ...and uncapped, the same graph would be four segments of ten,
        // which is the shape P4a measured as the slow one at scale.
        let uncapped = segment_shards(def_of(&flow), &runs, &vec![true; runs.len()], 0, 4, usize::MAX);
        assert_eq!(chain_order(&uncapped).iter().filter(|s| **s == 0).count(), 10);
    }

    /// Fewer runs than shards: one run per segment, which is round-robin
    /// arrived at honestly. Stated as a test because it is the case where
    /// segments buy nothing, and a reader of the numbers needs to know it.
    #[test]
    fn a_graph_smaller_than_the_machine_spreads_one_run_per_segment() {
        let (flow, runs) = chain_def(2);
        let placed = segment_shards(def_of(&flow), &runs, &vec![true; runs.len()], 0, 8, MAX_SEGMENT_LEN);
        assert_eq!(placed, vec![Some(0), Some(4)]);
    }

    /// The base rotates the whole assignment, so two flows started back to
    /// back do not both begin on shard 0. Rotation only -- the segment
    /// SHAPE is identical.
    #[test]
    fn the_round_robin_base_rotates_a_flows_segments() {
        let (flow, runs) = chain_def(6);
        let def = def_of(&flow);
        let flags = vec![true; runs.len()];
        assert_eq!(
            segment_shards(def, &runs, &flags, 0, 3, MAX_SEGMENT_LEN),
            vec![Some(0), Some(0), Some(1), Some(1), Some(2), Some(2)]
        );
        assert_eq!(
            segment_shards(def, &runs, &flags, 1, 3, MAX_SEGMENT_LEN),
            vec![Some(1), Some(1), Some(2), Some(2), Some(0), Some(0)]
        );
    }

    /// Thread runs get no shard at all (their placement is the OS's), and
    /// they do not consume a segment either -- the task runs are sliced
    /// among themselves.
    #[test]
    fn thread_runs_are_skipped_and_do_not_consume_a_segment() {
        let (flow, runs) = chain_def(4);
        let flags = vec![true, false, true, false];
        let placed = segment_shards(def_of(&flow), &runs, &flags, 0, 2, MAX_SEGMENT_LEN);
        assert_eq!(placed, vec![Some(0), None, Some(1), None]);
    }

    /// A graph the BFS cannot reach from a source (a pure cycle has no
    /// source at all) is still TOTAL and deterministic: the walk falls back
    /// to `proc_order`, and every task run comes out placed.
    #[test]
    fn a_cycle_with_no_source_is_still_placed() {
        let relay = step(":ins {:in {}} :outs {:out {}}");
        let src = format!(
            r#"(flow/create-flow
                 {{:procs {{:a {{:proc {relay}}} :b {{:proc {relay}}} :c {{:proc {relay}}}}}
                   :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]] [[:c :out] [:a :in]]]}})"#
        );
        let mut interp = Interp::new();
        let Value::Flow(f) = interp.eval_str("placement-test", &src).expect("create-flow") else {
            panic!("expected a flow")
        };
        let runs = plan_fusion_with(&f.def, FusionPolicy::Off);
        let placed = segment_shards(&f.def, &runs, &vec![true; runs.len()], 0, 3, MAX_SEGMENT_LEN);
        assert!(placed.iter().all(Option::is_some), "every task run is placed: {placed:?}");
    }

    // -----------------------------------------------------------------
    // L4 W1 (docs/L4-LANDING-SPEC.md §W1): exit reasons + supervision-chan
    // plumbing. Coverage split deliberately: `tests/l4_supervision_test.rs`
    // (the wave's required deliverable) carries every claim reachable
    // through the Mova-level `flow/*` surface; the two tests below live HERE
    // instead because neither claim has ANY Mova-level trigger to hang off
    // of --
    // - `ExitReason::Normal` requires a proc's control chan to be found
    //   CLOSED WITH AN EMPTY BUFFER. `flow/stop` (`stop_flow_cell`) always
    //   `chan_put`s exactly one `::flow/stop` command before ever closing a
    //   proc's control chan, and that message survives the close (a closed
    //   chan still delivers what was already buffered) -- so every
    //   Mova-reachable stop is seen by the proc as `Stopped`, never
    //   `Normal`, no matter how long the proc is wedged first. `Normal`'s
    //   own code site (`run_ready`'s `chan_take(&control) else { break
    //   'outer ExitReason::Normal }`, and its `run_proc_fast`/`run_fused`
    //   siblings) is real and load-bearing for whatever FUTURE mechanism
    //   closes a control chan without first queuing a stop command (a
    //   candidate: W3's kill), but W1 introduces no such mechanism, so this
    //   is exercised directly instead of left untested.
    // - "no `sup_chan` allocated for an unsupervised flow" needs
    //   [`sup_chan_census`], which is `pub(crate)` (this file's own W1
    //   census counter, `mult_spawn_census`'s sibling) and therefore
    //   invisible to an external `tests/*.rs` crate unless `lib.rs`'s
    //   `flow_probe` module re-exports it -- out of W1's file ownership
    //   (`lib.rs` belongs to nobody in this wave). Both directions
    //   (allocated / not allocated) are asserted here instead, where the
    //   `pub(crate)` counter is directly callable.
    // -----------------------------------------------------------------

    /// A trivial native step, used only as something `init_proc` can call --
    /// its own behavior is irrelevant to every test below (none of them
    /// ever let a message move).
    fn normal_test_step_fn(interp: &mut Interp) -> Value {
        interp.eval_str("l4-w1-unit-test", "(flow/step-passthrough)").expect("step-passthrough")
    }

    /// **`ExitReason::Normal`, exercised directly** -- see this section's
    /// doc for why no Mova program can trigger it. `run_status` starts
    /// `Paused` on every proc (`run_ready`'s first local), so the very
    /// first lap always takes the blocking `chan_take(&control)` branch
    /// regardless of `ins`/read-set shape -- closing `control` before the
    /// proc ever starts is sufficient, no message plumbing required.
    #[test]
    fn control_closed_with_an_empty_buffer_exits_normal() {
        let mut interp = Interp::new();
        let step_fn = normal_test_step_fn(&mut interp);
        let control = Arc::new(Chan::new(BufferPolicy::Fixed(CONTROL_BUF)));
        // Closed with NOTHING ever put -- the one shape `flow/stop` itself
        // can never produce (it always puts the `::flow/stop` command
        // first). See the section doc.
        chan_close(&control);
        let spawn = ProcSpawn {
            interp,
            pid: plain_kw("p"),
            step_fn,
            args: Value::Map(PMap::new()),
            ins: HashMap::new(),
            outs: HashMap::new(),
            in_lane: None,
            out_lane: None,
            control,
            error_chan: Arc::new(Chan::new(BufferPolicy::Sliding(DIAG_BUF))),
        };
        assert_eq!(run_proc(spawn), ExitReason::Normal);
    }

    /// Sanity companion: the ordinary `::flow/stop` shape -- one buffered
    /// command, THEN close -- exits `Stopped`, not `Normal`. Pins the
    /// buffered-message-survives-close behavior the section doc's claim
    /// depends on, so a transport/chan regression that started dropping
    /// buffered items on close would be caught here rather than silently
    /// making the `Normal` test above pass for the wrong reason.
    #[test]
    fn control_closed_after_a_buffered_stop_still_exits_stopped() {
        let mut interp = Interp::new();
        let step_fn = normal_test_step_fn(&mut interp);
        let control = Arc::new(Chan::new(BufferPolicy::Fixed(CONTROL_BUF)));
        let mut stop_cmd = PMap::new();
        stop_cmd.insert(kw("op"), plain_kw("stop"));
        assert!(chan_put(&control, Value::Map(stop_cmd)), "control chan must accept the buffered stop");
        chan_close(&control);
        let spawn = ProcSpawn {
            interp,
            pid: plain_kw("p"),
            step_fn,
            args: Value::Map(PMap::new()),
            ins: HashMap::new(),
            outs: HashMap::new(),
            in_lane: None,
            out_lane: None,
            control,
            error_chan: Arc::new(Chan::new(BufferPolicy::Sliding(DIAG_BUF))),
        };
        assert_eq!(run_proc(spawn), ExitReason::Stopped);
    }

    fn eval_l4(interp: &mut Interp, src: &str) -> Value {
        interp.eval_str("l4-w1-unit-test", src).unwrap_or_else(|e| panic!("eval error for {src:?}: {e:?}"))
    }

    /// [`sup_chan_census`] is a single process-global counter (mirroring
    /// `mult_spawn_census`'s own reasoning), and `cargo test` runs this
    /// module's tests concurrently by default -- every test that reads a
    /// before/after DELTA off it therefore has to serialize against every
    /// OTHER such test in this file, the same discipline
    /// `tests/flow_wake_test.rs`'s `test_lock` documents for
    /// `Doorbell::safety_net_hits()`.
    fn sup_chan_census_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    /// **White-box: an unsupervised flow allocates no `sup_chan`.** See this
    /// section's doc for why [`sup_chan_census`] (not any Mova-visible
    /// signal) is the only way to see this.
    #[test]
    fn unsupervised_flow_allocates_no_sup_chan() {
        let _guard = sup_chan_census_lock();
        let mut interp = Interp::new();
        let before = sup_chan_census();
        eval_l4(
            &mut interp,
            r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                            :transform (fn [s _ m] [s {}])})
                      fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})]
                  (flow/start fl)
                  (flow/stop fl)
                  :done)"#,
        );
        assert_eq!(sup_chan_census(), before, "an unsupervised flow must not allocate a sup_chan");
    }

    /// **White-box: a supervised flow allocates exactly one `sup_chan`**,
    /// regardless of how many of its procs individually opted in -- design
    /// §3.2/D-A axiom 4: the chan is per-FLOW, so every proc's `ExitGuard`
    /// shares the SAME one.
    #[test]
    fn supervised_flow_allocates_exactly_one_sup_chan() {
        let _guard = sup_chan_census_lock();
        let mut interp = Interp::new();
        let before = sup_chan_census();
        eval_l4(
            &mut interp,
            r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                            :transform (fn [s _ m] [s {}])})
                      fl (flow/create-flow
                          {:procs {:p {:proc (flow/process step)
                                       :supervision {:policy :restart}}
                                   :q {:proc (flow/process step)}}
                           :conns []})]
                  (flow/start fl)
                  (flow/stop fl)
                  :done)"#,
        );
        assert_eq!(sup_chan_census(), before + 1, "one supervised proc must allocate exactly one flow-wide sup_chan");
    }

    /// `MOVA_NO_SUPERVISION=1` is a process-wide `OnceLock`
    /// (`supervision_disabled_by_env`) like every other kill switch in this
    /// module, so it cannot be exercised as a second case inside a normal
    /// `cargo test` binary (the first test to touch the flag fixes it for
    /// the whole process) -- `tests/l4_supervision_test.rs` covers it out of
    /// process, the same pattern `tests/l3_task_procs_test.rs` uses for
    /// `MOVA_FLOW_THREAD_PROCS`.
    #[test]
    fn supervision_precedence_proc_overrides_flow_and_unknown_key_errors_loudly() {
        let _guard = sup_chan_census_lock();
        let mut interp = Interp::new();
        // proc-level :policy :none overrides a flow-level :restart default.
        let before = sup_chan_census();
        eval_l4(
            &mut interp,
            r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                            :transform (fn [s _ m] [s {}])})
                      fl (flow/create-flow
                          {:procs {:p {:proc (flow/process step) :supervision {:policy :none}}}
                           :supervision {:policy :restart}
                           :conns []})]
                  (flow/start fl)
                  (flow/stop fl)
                  :done)"#,
        );
        assert_eq!(
            sup_chan_census(),
            before,
            "proc-level :policy :none must override a :restart flow-level default -- no sup_chan"
        );

        // A flow-level default with no proc-level override DOES apply.
        let before = sup_chan_census();
        eval_l4(
            &mut interp,
            r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                            :transform (fn [s _ m] [s {}])})
                      fl (flow/create-flow
                          {:procs {:p {:proc (flow/process step)}}
                           :supervision {:policy :restart}
                           :conns []})]
                  (flow/start fl)
                  (flow/stop fl)
                  :done)"#,
        );
        assert_eq!(sup_chan_census(), before + 1, "the flow-level :restart default must apply when no proc overrides it");

        // Unknown key -> loud create-flow error, never a silent default.
        let mut interp2 = Interp::new();
        let err = interp2.eval_str(
            "l4-w1-unit-test",
            r#"(flow/create-flow
                {:procs {:p {:proc (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                                     :transform (fn [s _ m] [s {}])})
                             :supervision {:policy :restart :bogus-key 1}}}
                 :conns []})"#,
        );
        assert!(err.is_err(), "an unrecognized :supervision key must error create-flow, not be silently ignored");

        // Missing :policy -> loud error (no implicit default for :policy
        // itself, unlike every other :supervision key).
        let mut interp3 = Interp::new();
        let err = interp3.eval_str(
            "l4-w1-unit-test",
            r#"(flow/create-flow
                {:procs {:p {:proc (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                                     :transform (fn [s _ m] [s {}])})
                             :supervision {:max-restarts 3}}}
                 :conns []})"#,
        );
        assert!(err.is_err(), "a :supervision map with no :policy must error create-flow");
    }

    // -----------------------------------------------------------------
    // L4 W2 (docs/L4-LANDING-SPEC.md §W2): the supervisor's POLICY, unit
    // tested. [`decide`] is a private pure fn -- no Mova surface reaches it,
    // and that is the point: the whole restart timeline is decided here, so
    // it is tested here, against synthetic histories and a synthetic `now`,
    // with not one thread, chan or clock involved (D-A axiom 2, gate G-DET's
    // "decide() pure-unit-tested" clause).
    //
    // `tests/l4_supervision_test.rs` carries the end-to-end half (a real
    // crash, a real restart, real report-chan events); every rule BELOW is
    // asserted directly rather than inferred from a timing window.
    // -----------------------------------------------------------------

    fn test_cfg(max_restarts: u32, window_ms: u64, backoff: BackoffCfg) -> SupervisionCfg {
        // `auto_resume: true` -- the default, and irrelevant to every rule
        // `decide()` itself expresses (auto-resume is consulted only by
        // `act_restart`, never by the pure decision function this module's
        // unit matrix exercises), so the default is the correct constant
        // here rather than a parameter this fn's callers would all repeat.
        SupervisionCfg { max_restarts, window_ms, backoff, grace_ms: 1000, on_give_up: OnGiveUp::Report, auto_resume: true }
    }

    fn test_backoff(initial_ms: u64, factor: f64, max_ms: u64) -> BackoffCfg {
        BackoffCfg { initial_ms, factor, max_ms }
    }

    /// A run that is alive at incarnation `inc`, has never restarted, and
    /// whose current incarnation's death has not been decided yet.
    fn fresh_record(inc: u64) -> RunRecord {
        RunRecord { incarnation: inc, state: RunState::Running, decided: None, restarts: Vec::new(), consecutive: 0 }
    }

    fn death(reason: ExitReason, incarnation: u64) -> ExitEvent {
        ExitEvent { pid: plain_kw("p"), reason, incarnation }
    }

    /// **Orderly exits are never restarted** -- rule 5, and the one that
    /// keeps supervision from fighting `flow/stop` (or a proc that chose to
    /// leave).
    #[test]
    fn decide_ignores_normal_and_stopped_exits() {
        let cfg = test_cfg(5, 60_000, test_backoff(100, 2.0, 5000));
        let now = Instant::now();
        for reason in [ExitReason::Normal, ExitReason::Stopped] {
            assert_eq!(
                decide(&death(reason, 0), Some(&cfg), &fresh_record(0), FlowPhase::Running, now),
                Decision::Ignore(IgnoreCause::OrderlyExit),
                "{reason:?} is an orderly exit"
            );
        }
    }

    /// **Both fault reasons are restart-eligible**, `:killed` included --
    /// W3 constructs it, and the policy must already be right about it.
    #[test]
    fn decide_restarts_panicked_and_killed() {
        let cfg = test_cfg(5, 60_000, test_backoff(100, 2.0, 5000));
        let now = Instant::now();
        for reason in [ExitReason::Panicked, ExitReason::Killed] {
            assert_eq!(
                decide(&death(reason, 0), Some(&cfg), &fresh_record(0), FlowPhase::Running, now),
                Decision::ScheduleRestart { at: now + Duration::from_millis(100), delay_ms: 100 },
                "{reason:?} is a system fault"
            );
        }
    }

    /// **An unsupervised pid's death is recorded and ignored.** Every proc's
    /// `ExitGuard` ships to `sup_chan` by design (W1), so this arm is load
    /// bearing rather than defensive.
    #[test]
    fn decide_ignores_an_unsupervised_run() {
        assert_eq!(
            decide(&death(ExitReason::Panicked, 0), None, &fresh_record(0), FlowPhase::Running, Instant::now()),
            Decision::Ignore(IgnoreCause::Unsupervised)
        );
    }

    /// **The fused dedup**: k members, one death, one decision. The first
    /// event stamps `decided`; every later event of the same (run,
    /// incarnation) is a duplicate -- and duplicates outrank EVERY other
    /// rule, including the ones that would otherwise have restarted.
    #[test]
    fn decide_ignores_duplicate_events_of_one_death() {
        let cfg = test_cfg(5, 60_000, test_backoff(100, 2.0, 5000));
        let mut rec = fresh_record(3);
        rec.decided = Some(3);
        assert_eq!(
            decide(&death(ExitReason::Panicked, 3), Some(&cfg), &rec, FlowPhase::Running, Instant::now()),
            Decision::Ignore(IgnoreCause::Duplicate)
        );
    }

    /// **Wall W5**: an OLD incarnation dying after its replacement is already
    /// running must not restart the new one. Checked before the phase and the
    /// config, so a stale event is inert no matter what else is true.
    #[test]
    fn decide_ignores_a_stale_incarnation() {
        let cfg = test_cfg(5, 60_000, test_backoff(100, 2.0, 5000));
        let rec = fresh_record(2);
        assert_eq!(
            decide(&death(ExitReason::Panicked, 1), Some(&cfg), &rec, FlowPhase::Running, Instant::now()),
            Decision::Ignore(IgnoreCause::StaleIncarnation)
        );
        // And the other direction (an event from the future) is equally
        // inert rather than accidentally matching.
        assert_eq!(
            decide(&death(ExitReason::Panicked, 9), Some(&cfg), &rec, FlowPhase::Running, Instant::now()),
            Decision::Ignore(IgnoreCause::StaleIncarnation)
        );
    }

    /// **Wall W4**: a flow that is not `Running` never restarts. (The
    /// action-time re-check under the flow lock is the half that makes this
    /// race-free; this is the decision-time half.)
    #[test]
    fn decide_ignores_everything_once_the_flow_is_not_running() {
        let cfg = test_cfg(5, 60_000, test_backoff(100, 2.0, 5000));
        for phase in [FlowPhase::Stopped, FlowPhase::Created] {
            assert_eq!(
                decide(&death(ExitReason::Panicked, 0), Some(&cfg), &fresh_record(0), phase, Instant::now()),
                Decision::Ignore(IgnoreCause::NotRunning),
                "{phase:?} must never restart"
            );
        }
    }

    /// A run already given up on stays given up: no second life without a
    /// successful restart to reset the state.
    #[test]
    fn decide_ignores_a_run_it_has_already_given_up_on() {
        let cfg = test_cfg(5, 60_000, test_backoff(100, 2.0, 5000));
        let mut rec = fresh_record(0);
        rec.state = RunState::GivenUp;
        assert_eq!(
            decide(&death(ExitReason::Panicked, 0), Some(&cfg), &rec, FlowPhase::Running, Instant::now()),
            Decision::Ignore(IgnoreCause::AlreadyGivenUp)
        );
    }

    /// **The window**: `:max-restarts` is how many restarts fit in one
    /// window, so crash number max+1 gives up -- and `:max-restarts 0` gives
    /// up on the first crash.
    #[test]
    fn decide_gives_up_once_the_window_is_full() {
        let now = Instant::now();
        let cfg = test_cfg(2, 60_000, test_backoff(100, 2.0, 5000));
        let mut rec = fresh_record(0);
        rec.restarts = vec![now - Duration::from_millis(10)];
        assert!(
            matches!(
                decide(&death(ExitReason::Panicked, 0), Some(&cfg), &rec, FlowPhase::Running, now),
                Decision::ScheduleRestart { .. }
            ),
            "one restart in a window of two still has room"
        );
        rec.restarts.push(now - Duration::from_millis(5));
        assert_eq!(
            decide(&death(ExitReason::Panicked, 0), Some(&cfg), &rec, FlowPhase::Running, now),
            Decision::GiveUp { restarts: 2 }
        );

        let zero = test_cfg(0, 60_000, test_backoff(100, 2.0, 5000));
        assert_eq!(
            decide(&death(ExitReason::Panicked, 0), Some(&zero), &fresh_record(0), FlowPhase::Running, now),
            Decision::GiveUp { restarts: 0 },
            ":max-restarts 0 gives up on the first crash"
        );
    }

    /// Restarts that fell OUT of the window do not count -- the window
    /// slides, it does not accumulate. A flow that crashes once an hour with
    /// a one-minute window restarts forever, which is the point of having a
    /// window at all.
    #[test]
    fn decide_prunes_restarts_older_than_the_window() {
        let now = Instant::now();
        let cfg = test_cfg(1, 1000, test_backoff(100, 2.0, 5000));
        let mut rec = fresh_record(0);
        rec.restarts = vec![now - Duration::from_millis(5000), now - Duration::from_millis(2000)];
        assert!(
            matches!(
                decide(&death(ExitReason::Panicked, 0), Some(&cfg), &rec, FlowPhase::Running, now),
                Decision::ScheduleRestart { .. }
            ),
            "both restarts are older than the 1s window"
        );
        rec.restarts.push(now - Duration::from_millis(500));
        assert_eq!(
            decide(&death(ExitReason::Panicked, 0), Some(&cfg), &rec, FlowPhase::Running, now),
            Decision::GiveUp { restarts: 1 },
            "the one INSIDE the window fills it"
        );
    }

    /// The backoff schedule itself: `initial * factor^consecutive`, capped.
    #[test]
    fn decide_schedules_the_exponential_backoff() {
        let now = Instant::now();
        let cfg = test_cfg(99, 60_000, test_backoff(100, 2.0, 350));
        let mut rec = fresh_record(0);
        for (consecutive, expected) in [(0u32, 100u64), (1, 200), (2, 350), (7, 350)] {
            rec.consecutive = consecutive;
            assert_eq!(
                decide(&death(ExitReason::Panicked, 0), Some(&cfg), &rec, FlowPhase::Running, now),
                Decision::ScheduleRestart { at: now + Duration::from_millis(expected), delay_ms: expected },
                "consecutive={consecutive}"
            );
        }
    }

    /// [`backoff_delay_ms`] on its own, including the arithmetic edges the
    /// integration tests can never reach: a factor of 0, and a crash count
    /// big enough to overflow `f64::powi` into `inf` (which must clamp to
    /// `max_ms`, never wrap through the `as u64` cast).
    #[test]
    fn backoff_delay_is_total_and_saturating() {
        assert_eq!(backoff_delay_ms(&test_backoff(100, 2.0, 5000), 0), 100);
        assert_eq!(backoff_delay_ms(&test_backoff(100, 2.0, 5000), 5), 3200);
        assert_eq!(backoff_delay_ms(&test_backoff(100, 2.0, 5000), 6), 5000, "clamped to :max-ms");
        assert_eq!(backoff_delay_ms(&test_backoff(100, 2.0, 5000), u32::MAX), 5000, "no overflow, no wrap");
        assert_eq!(backoff_delay_ms(&test_backoff(100, 0.0, 5000), 0), 100, "factor 0: the first delay is initial");
        assert_eq!(backoff_delay_ms(&test_backoff(100, 0.0, 5000), 1), 0, "factor 0: and nothing after it");
        assert_eq!(backoff_delay_ms(&test_backoff(0, 2.0, 5000), 3), 0, ":initial-ms 0 means restart at once");
        assert_eq!(backoff_delay_ms(&test_backoff(9000, 2.0, 5000), 0), 5000, "initial above the cap is capped");
    }

    /// The `#::flow{:pid :pids :op ...}` shape of the supervisor's own
    /// events, pinned once (the integration tests read these keys back
    /// through Mova, which cannot tell a missing key from a `nil` one).
    #[test]
    fn run_events_carry_the_head_pid_the_whole_run_and_the_extras() {
        let ev = render_run_event(
            "proc-restart",
            &[plain_kw("a"), plain_kw("b")],
            &[("incarnation", Value::Int(2)), ("delay-ms", Value::Int(100))],
        );
        let Value::Map(m) = &ev else { panic!("expected a map") };
        assert_eq!(m.get(&kw("pid")), Some(&plain_kw("a")), ":pid is the run's HEAD");
        assert_eq!(m.get(&kw("op")), Some(&plain_kw("proc-restart")));
        assert_eq!(m.get(&kw("incarnation")), Some(&Value::Int(2)));
        assert_eq!(m.get(&kw("delay-ms")), Some(&Value::Int(100)));
        let Some(Value::Vector(pids)) = m.get(&kw("pids")) else { panic!("expected :pids to be a vector") };
        assert_eq!(pids.len(), 2, ":pids is the WHOLE run -- the restart unit");
    }

    /// [`parse_sup_msg`] is [`render_exit_reason`]'s inverse, exactly -- the
    /// supervisor's whole view of a death depends on this round trip, and a
    /// rename on either side that broke it would otherwise show up only as
    /// "restarts silently stopped happening". L4 W3 adds the second wire
    /// shape, `flow/stop-proc`'s request, to the same round trip.
    #[test]
    fn exit_events_round_trip_through_the_wire_form() {
        for reason in [ExitReason::Normal, ExitReason::Stopped, ExitReason::Panicked, ExitReason::Killed] {
            let wire = render_exit_reason(&plain_kw("p"), reason, 7);
            assert_eq!(
                parse_sup_msg(&wire),
                Some(SupMsg::Exit(ExitEvent { pid: plain_kw("p"), reason, incarnation: 7 })),
                "{reason:?} must survive the round trip"
            );
        }
        assert_eq!(
            parse_sup_msg(&render_stop_request(&plain_kw("p"))),
            Some(SupMsg::StopRequest(plain_kw("p"))),
            "L4 W3: a stop request must survive the same round trip"
        );
        // Anything else is not a `sup_chan` message -- including the
        // supervisor's OWN lifecycle events, which share the `::flow`
        // namespace and the `:pid`/`:op` keys.
        assert_eq!(parse_sup_msg(&render_run_event("proc-restart", &[plain_kw("p")], &[])), None);
        assert_eq!(parse_sup_msg(&Value::Nil), None);
    }

    /// **White-box: an unsupervised flow spawns NO supervisor task.** The
    /// byte-identical-default-build half of G-CONF for W2 -- and, like
    /// [`sup_chan_census`], invisible to any Mova-level assertion (a
    /// supervisor with nothing to supervise is silent, which is exactly what
    /// having no supervisor looks like from outside).
    #[test]
    fn unsupervised_flow_spawns_no_supervisor_task() {
        let _guard = sup_chan_census_lock();
        let mut interp = Interp::new();
        let before = supervisor_spawn_census();
        eval_l4(
            &mut interp,
            r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                            :transform (fn [s _ m] [s {}])})
                      fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})]
                  (flow/start fl)
                  (flow/stop fl)
                  :done)"#,
        );
        assert_eq!(supervisor_spawn_census(), before, "an unsupervised flow must not spawn a supervisor task");
    }

    /// **White-box: a supervised flow spawns exactly ONE supervisor**, no
    /// matter how many of its procs opted in -- one per flow, like the chan
    /// it reads.
    #[test]
    fn supervised_flow_spawns_exactly_one_supervisor_task() {
        let _guard = sup_chan_census_lock();
        let mut interp = Interp::new();
        let before = supervisor_spawn_census();
        eval_l4(
            &mut interp,
            r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                            :transform (fn [s _ m] [s {}])})
                      fl (flow/create-flow
                          {:procs {:p {:proc (flow/process step)
                                       :supervision {:policy :restart}}
                                   :q {:proc (flow/process step)
                                       :supervision {:policy :restart}}}
                           :conns []})]
                  (flow/start fl)
                  (flow/stop fl)
                  :done)"#,
        );
        assert_eq!(supervisor_spawn_census(), before + 1, "two supervised procs still share ONE supervisor");
    }
}
