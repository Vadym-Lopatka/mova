//! Perceus-lite phase 2: **last-use analysis** over one compiled arity.
//!
//! Phase 1 (`builtins::reuse`) gave whitelisted natives an args `Vec` they
//! exclusively own, so `assoc`/`conj`/`dissoc` may mutate their receiver
//! instead of cloning it. Its production measurement (bench/optimization-
//! log.md, "E3 PRODUCTIZATION, phase 1", table (D)) then reported 0.00%
//! unique hits on every accumulator shape, and named the reason: the frame
//! slot holding the receiver is STILL BOUND when the builtin runs, so the
//! handle the native owns is never the only one.
//!
//! This pass removes that handle where it is provably dead: a slot's final
//! read becomes [`Ir::LoadSlotTake`], which `std::mem::replace`s the slot
//! with `Value::Nil` and moves the value out.
//!
//! # Scope: the compiled tier only
//!
//! The tree-walker's locals live in an `Env` whose frames are shared with
//! every closure created under them and re-probed by
//! `Ir::CreationEnvLookup`; moving out of one is not a local decision there.
//! Nothing in this module runs unless a fn compiled, so `MOVA_NO_COMPILE=1`
//! semantics are bit-identical to before this landing by construction.
//!
//! # The analysis
//!
//! Textbook **backward liveness**, run over the `Ir` tree of one arity, with
//! `live` meaning *"some path from this point reads this slot before
//! overwriting it"*. Walking a node updates `live` from its live-OUT set to
//! its live-IN set; at an `Ir::LoadSlot(i)` the node is rewritten to
//! `LoadSlotTake(i)` iff `i` is not in the live-out set (and not excluded,
//! below) -- and then `i` is inserted, because the read itself makes the
//! slot live for everything before it.
//!
//! Deliberately NOT a syntactic "last textual occurrence" rule: that is
//! wrong on exactly the shape the targeted tests pin
//! (`(if c (f m) (g m))` -- the `then` read is textually earlier but is a
//! last use on its own path, and the `else` read is a last use on its own,
//! and NEITHER is "the last occurrence"). Path-awareness falls out of
//! processing `if` branches against independent copies of the live-out set
//! and unioning them, which is what makes both reads takeable and would
//! make neither takeable if the two branches were sequenced.
//!
//! ## Kills, and why the analysis needs them at all
//!
//! `resolve.rs` never reuses a slot index, so within straight-line code a
//! slot is written exactly once and kills are irrelevant. They matter at
//! exactly two places, both of them BACK EDGES, and both of them the shape
//! this landing exists to speed up:
//!
//! - `exec_loop`'s `recur` path re-destructures the scratch block into the
//!   loop's binding slots before re-running the body;
//! - `run_compiled_body`'s trampoline re-runs `bind_param_slots` over the
//!   parameter slots before re-running the fn body.
//!
//! Both writes happen before any read of the next iteration, so a loop
//! binding (or a param) is DEAD at its final read of the current iteration
//! even though the next iteration reads it again. That is what makes
//! `(loop [m {}] (if .. (recur (assoc m k v)) m))` -- the accumulator shape
//! -- take `m`, and it is where the whole win lives: at that instant the
//! frame slot is the only handle in the program, so `assoc` mutates in
//! place.
//!
//! A back edge is handled as a least-fixpoint iteration ([`Analyzer::
//! loop_like`]): the live set flowing back into the loop starts empty, the
//! body is analysed against it, the result minus the killed slots becomes
//! the new back-edge set, and the walk repeats until it stops growing. Only
//! then is the body walked once more with rewriting enabled.
//!
//! # THE SAFETY CONDITIONS
//!
//! Each is enforced by construction, and each has its proof at the code that
//! enforces it. Summarised here because a wrong take is a miscompile while a
//! missed take is only a missed optimization -- so every one of these is
//! resolved in the direction of not taking.
//!
//! **S1 -- the only reader of a frame slot is that frame's own `Ir`.**
//! Everything else this analysis claims rests on this. `exec.rs` reads
//! `l.slots` from exactly five places: `Ir::LoadSlot`/`LoadSlotTake`,
//! `make_closure`'s `CaptureSrc::Slot` (by VALUE), `exec_num_loop`'s seeds
//! and invariant loads, `exec_loop`'s re-destructure and
//! `run_compiled_body`'s trampoline (both of which read only SCRATCH slots,
//! which no `Ir` node ever names). User code cannot reach another frame's
//! slots at all -- a nested closure gets a by-value snapshot, not a
//! reference. So "no `Ir` read reaches this point" really is "nothing reads
//! this slot".
//!
//! **S2 -- a slot any `MakeClosure` captures is never taken.** Capture is
//! by value (`ir::CaptureSrc`), so a take AFTER the capture would in fact be
//! harmless; the exclusion is deliberate belt-and-braces, since the whole
//! analysis would silently become wrong if that one `.clone()` in
//! `make_closure` ever became a borrow. Captured slots are collected up
//! front, over the whole arity, and never rewritten anywhere -- so the
//! ordering question ("was the closure created before or after the last
//! use?") never has to be answered.
//!
//! **S3 -- conditional paths join, they do not sequence.** See the `Ir::If`
//! arm.
//!
//! **S4 -- `recur` argument evaluation order is respected.** `exec_recur`
//! evaluates its args left to right into the scratch block; the walk visits
//! them RIGHT TO LEFT, so `(recur (assoc m k v) (count m))` sees the second
//! argument's read first and refuses to take in the first. Every other
//! multi-subexpression node is walked in reverse evaluation order for the
//! same reason.
//!
//! **S5 -- a `recur` continues at its target's back edge, not at its
//! syntactic continuation.** A non-tail `recur` ABANDONS the pending
//! expression (COMPILE-TIER-DESIGN.md constraint 2), so the `Ir::Recur` arm
//! unions in the target's back-edge live set. The syntactic continuation is
//! kept in the set as well: it cannot be reached, so keeping it only costs
//! takes, never correctness.
//!
//! **S6 -- `try`: the handlers are live-out for the WHOLE body.** An error
//! can unwind from any point of a `try` body, so `live_out(body)` is set to
//! `live_in(catch) ∪ live_in(finally)` rather than to what follows the
//! `try`. That is sufficient rather than merely plausible: kills inside the
//! body only ever kill slots BOUND inside the body, which a `catch`/
//! `finally` clause cannot name, so the live set at every interior point
//! still contains everything the handlers read. A take inside a try body is
//! therefore permitted exactly when no handler and nothing after the `try`
//! reads that slot -- e.g. `(try (assoc m ..) (catch e :x))` takes, while
//! `(try (assoc m ..) (catch e m))` does not.
//!
//! **S7 -- a back edge only kills what it unconditionally rewrites.** For a
//! `loop`, that is its binding slots, and only when EVERY binding is a plain
//! symbol: a destructuring pattern interleaves `:or` default *expressions*
//! (which read other slots) and `uncons` calls between the writes, so which
//! slot holds an old value when is no longer a one-line argument. Such loops
//! get an empty kill set, i.e. nothing loop-carried is taken in them. Fn
//! params are always plain slots (`resolve::compile_arity` binds
//! `arity.params` directly), and `bind_param_slots` writes every one of
//! them, so the fn-level back edge always kills the whole parameter block.
//!
//! **S8 -- `Ir::NumLoop` is read-only to this pass.** Its seeds and
//! invariant loads are read at loop ENTRY and its `fallback` may then re-read
//! the same slots, so a take inside the fallback would have to be reasoned
//! about against a re-entry that already sampled them. Since the `NumLoop`
//! grammar admits nothing but scalar arithmetic, a take inside one could
//! never feed a consuming native -- so the whole subtree contributes its
//! reads to the live set and is never rewritten. Declining costs nothing
//! measurable and removes the interleaving question entirely.
//!
//! **S9 -- `Ir::CreationEnvLookup` and `Ir::SelfRef` are not slot-based.**
//! Confirmed against `exec.rs`: `creation_env_get` probes
//! `l.me.env`/`chain`, and `SelfRef` reads `l.me`. Neither touches
//! `l.slots`, so neither can read a taken slot and neither contributes to
//! liveness.
//!
//! **S10 -- anything unproven abandons the whole arity.** The pass runs
//! twice: once with rewriting off, to prove every fixpoint converged, every
//! `recur` found its target and the visit budget held, and only then again
//! with rewriting on. Liveness does not depend on the rewrite (a
//! `LoadSlotTake` is the same read as a `LoadSlot`), so the second walk
//! computes the identical sets -- and a failure in the first means not one
//! node is rewritten.
//!
//! # Kill switch
//!
//! `MOVA_NO_LASTUSE=1` ([`disabled_by_env`], a `OnceLock` read once) stops
//! this pass from RUNNING, so the tier emits plain `LoadSlot`s and there is
//! no moving read anywhere in the process to be wrong about -- the same
//! emission-gate discipline `MOVA_NO_NUMLOOP` uses, and for the same
//! reason. `Interp::lastuse_enabled` is its per-interpreter sibling, which
//! the randomized differential needs in order to run a taking and a
//! non-taking tier side by side in one process.
//!
//! It is independent of `MOVA_NO_REUSE`: this pass decides whether the
//! frame gives up its handle, that one decides whether the native uses the
//! handle it is given. They compose, and the differential guard runs the
//! matrix.

use super::ir::{CaptureSrc, CompiledPattern, FieldRecv, Ir, NumLoad, NumSeed, SeqStep};

/// True when `MOVA_NO_LASTUSE=1` was set at process start.
///
/// Gates EMISSION, not execution: with it on, no `Ir::LoadSlotTake` is ever
/// built, so `MOVA_NO_LASTUSE=1 cargo test` is a genuine A/B of the feature
/// rather than of one of two code paths. `MOVA_NO_COMPILE=1` subsumes it.
pub(super) fn disabled_by_env() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_NO_LASTUSE").is_ok_and(|v| v == "1"))
}

/// How many `Ir` nodes one arity's analysis may visit, summed over the
/// fixpoint iterations of both walks. A loop nest re-walks its body once per
/// fixpoint step, so a pathologically nested body could otherwise make
/// compilation super-linear -- and compilation happens at closure CREATION
/// time, i.e. inside whatever loop created the closure. Exceeding the budget
/// abandons the analysis for that arity (S10), which costs an optimization
/// and nothing else.
const MAX_VISITS: usize = 200_000;

/// Fixpoint iterations allowed per back edge. The set only grows and is
/// bounded by the slot count, so this is a guard against a bug, not a
/// tuning knob: one iteration computes the answer and the second confirms
/// it, which is why 8 is generous rather than tight.
const MAX_FIXPOINT_ITERS: usize = 8;

/// A set of slot indices, as a bitset.
///
/// The first 64 slots live INLINE, and `rest` stays empty for any arity with
/// 64 or fewer of them -- which is essentially all of them. That matters
/// because this analysis runs at closure CREATION time (a `fn` form
/// evaluated in a loop is recompiled per iteration, `compile::mod`'s
/// "compiled once per creation"), and a `Vec` allocation per branch-set
/// clone would be a per-closure cost paid to save a per-CALL one. Indices
/// beyond 64 grow `rest` on demand rather than being dropped: a dropped
/// insert would read back as "not live", the one direction that turns a
/// missed take into a wrong one.
#[derive(Clone, PartialEq, Eq)]
struct SlotSet {
    w0: u64,
    rest: Vec<u64>,
}

impl SlotSet {
    fn new(n_slots: usize) -> SlotSet {
        SlotSet {
            w0: 0,
            rest: if n_slots > 64 {
                vec![0; (n_slots - 64).div_ceil(64)]
            } else {
                Vec::new()
            },
        }
    }

    fn contains(&self, i: u16) -> bool {
        let i = i as usize;
        if i < 64 {
            return (self.w0 >> i) & 1 == 1;
        }
        self.rest
            .get((i - 64) / 64)
            .is_some_and(|x| (x >> ((i - 64) % 64)) & 1 == 1)
    }

    fn insert(&mut self, i: u16) {
        let i = i as usize;
        if i < 64 {
            self.w0 |= 1 << i;
            return;
        }
        let (w, b) = ((i - 64) / 64, (i - 64) % 64);
        if w >= self.rest.len() {
            self.rest.resize(w + 1, 0);
        }
        self.rest[w] |= 1 << b;
    }

    fn remove(&mut self, i: u16) {
        let i = i as usize;
        if i < 64 {
            self.w0 &= !(1 << i);
            return;
        }
        if let Some(x) = self.rest.get_mut((i - 64) / 64) {
            *x &= !(1 << ((i - 64) % 64));
        }
    }

    fn union(&mut self, other: &SlotSet) {
        self.w0 |= other.w0;
        if other.rest.len() > self.rest.len() {
            self.rest.resize(other.rest.len(), 0);
        }
        for (a, b) in self.rest.iter_mut().zip(other.rest.iter()) {
            *a |= *b;
        }
    }

    /// Removes every member of `other` -- the kill half of a back edge.
    fn difference(&mut self, other: &SlotSet) {
        self.w0 &= !other.w0;
        for (a, b) in self.rest.iter_mut().zip(other.rest.iter()) {
            *a &= !*b;
        }
    }

    fn is_subset_of(&self, other: &SlotSet) -> bool {
        self.w0 & !other.w0 == 0
            && self
                .rest
                .iter()
                .enumerate()
                .all(|(i, a)| a & !other.rest.get(i).copied().unwrap_or(0) == 0)
    }
}

/// One live `recur` target: the scratch block that identifies it, and the
/// live set that flows back into its body (S5).
struct Target {
    scratch_base: u16,
    back: SlotSet,
}

struct Analyzer<'a> {
    /// Slots a nested `fn` captures (S2) -- never rewritten.
    excluded: &'a SlotSet,
    targets: Vec<Target>,
    /// `false` during a validation/fixpoint walk, `true` on the final one.
    rewrite: bool,
    budget: usize,
    /// Cleared by anything unproven; the caller then discards the whole
    /// analysis for this arity (S10).
    ok: bool,
    n_slots: usize,
}

/// Runs the analysis over one arity's body, rewriting proven-final slot
/// reads into moving ones in place.
///
/// `n_recur` is the parameter block's width (`n_params + variadic`), which
/// is exactly what `run_compiled_body`'s trampoline rewrites on a fn-level
/// `recur`; `scratch_base` identifies that trampoline as a `recur` target.
pub(super) fn analyze_arity(body: &mut [Ir], n_slots: usize, n_recur: usize, scratch_base: u16) {
    if body.is_empty() || n_slots == 0 {
        return;
    }
    // One survey walk answers all three preliminaries at once: which slots
    // are excluded (S2), whether the body is big enough to be worth a budget
    // check, and whether it contains a BACK EDGE at all.
    let mut excluded = SlotSet::new(n_slots);
    let mut nodes = 0usize;
    let mut back_edges = false;
    for ir in body.iter() {
        walk(ir, &mut |n| {
            nodes += 1;
            match n {
                // A `Loop` is a back edge; a `Recur` is one whose target may
                // be the fn trampoline. Either means fixpoints and target
                // lookups, hence the full protocol below.
                Ir::Loop { .. } | Ir::Recur { .. } | Ir::NumLoop(_) => back_edges = true,
                Ir::MakeClosure { caps, .. } => {
                    for c in caps {
                        if let CaptureSrc::Slot(i) = c {
                            excluded.insert(*i);
                        }
                    }
                }
                // A recursive binding group snapshots every member's OUTER
                // captures out of this frame, exactly like a `MakeClosure`
                // does, so the same S2 exclusion applies to every one of
                // them. (Its member SLOTS are writes, not reads, and
                // `resolve.rs` never reuses a slot index -- so not treating
                // them as kills only costs takes, never correctness.)
                Ir::MakeRecGroup { members, .. } => {
                    for m in members {
                        for c in &m.caps {
                            if let CaptureSrc::Slot(i) = c {
                                excluded.insert(*i);
                            }
                        }
                    }
                }
                // field3/W-RESOLVE: an `Ir::Escape`'s bridge reads slots
                // by index, exactly like `MakeClosure`'s captures, and for
                // exactly the same reason it must never move out of one:
                // the escaped form is tree-walked in an env built from
                // those reads, and the slot may still be read afterwards
                // by ordinary compiled code. Same arm, same treatment.
                Ir::Escape(e) => {
                    for (_, src) in &e.binds {
                        if let CaptureSrc::Slot(i) = src {
                            excluded.insert(*i);
                        }
                    }
                }
                _ => {}
            }
        });
    }
    if nodes > MAX_VISITS {
        return;
    }
    if !back_edges {
        // THE COMMON CASE, and deliberately the cheap one: with no back edge
        // there is no fixpoint to iterate and no `recur` target to look up,
        // so a single backward walk IS the analysis -- and its one remaining
        // failure mode, the visit budget, was just ruled out. Paying the
        // two-pass protocol here would double a per-CLOSURE-CREATION cost to
        // guard against conditions this body cannot produce.
        let mut a = Analyzer::new(&excluded, n_slots, true);
        let mut live = SlotSet::new(n_slots);
        a.body(body, &mut live);
        debug_assert!(a.ok, "a back-edge-free body cannot fail the analysis");
        return;
    }

    // S10: prove first, rewrite second.
    let mut dry = Analyzer::new(&excluded, n_slots, false);
    dry.arity(body, n_recur, scratch_base);
    if !dry.ok {
        return;
    }
    let mut rw = Analyzer::new(&excluded, n_slots, true);
    rw.arity(body, n_recur, scratch_base);
    debug_assert!(
        rw.ok,
        "the rewriting walk must reach the same conclusion as the validating one"
    );
}

impl<'a> Analyzer<'a> {
    fn new(excluded: &'a SlotSet, n_slots: usize, rewrite: bool) -> Analyzer<'a> {
        Analyzer {
            excluded,
            targets: Vec::new(),
            rewrite,
            budget: MAX_VISITS,
            ok: true,
            n_slots,
        }
    }

    /// The fn body, treated as the body of the `recur` trampoline it is.
    /// Nothing is live after a fn returns: its value is a `Value`, and no
    /// slot outlives the call.
    fn arity(&mut self, body: &mut [Ir], n_recur: usize, scratch_base: u16) {
        // S7: `bind_param_slots` rewrites the whole parameter block (all
        // `n_params`, plus the rest slot when variadic) from the scratch
        // block before the body re-runs, and `resolve::compile_arity` binds
        // params to plain slots only -- so the fn-level back edge kills
        // exactly `0..n_recur`.
        let mut killed = SlotSet::new(self.n_slots);
        for i in 0..n_recur {
            killed.insert(i as u16);
        }
        let after = SlotSet::new(self.n_slots);
        self.loop_like(body, scratch_base, &killed, &after);
    }

    /// A body with a back edge: `Ir::Loop`, and the fn-level trampoline.
    /// Returns the body's live-IN set.
    ///
    /// The back-edge set is the least fixpoint of `back = live_in(body) \
    /// killed`, computed by iterating from the empty set. Iterating from
    /// EMPTY and growing is what makes the result sound to rewrite against:
    /// the loop exits only once `back` has stopped growing, so the set the
    /// rewriting walk uses is a genuine fixpoint rather than a first guess.
    fn loop_like(
        &mut self,
        body: &mut [Ir],
        scratch_base: u16,
        killed: &SlotSet,
        after: &SlotSet,
    ) -> SlotSet {
        let mut back = SlotSet::new(self.n_slots);
        for _ in 0..MAX_FIXPOINT_ITERS {
            let mut probe = after.clone();
            probe.union(&back);
            let saved = self.rewrite;
            self.rewrite = false;
            self.targets.push(Target {
                scratch_base,
                back: back.clone(),
            });
            self.body(body, &mut probe);
            self.targets.pop();
            self.rewrite = saved;
            if !self.ok {
                return after.clone();
            }
            let mut next = probe;
            next.difference(killed);
            if next.is_subset_of(&back) {
                // Converged. One rewriting walk against the fixpoint.
                let mut live = after.clone();
                live.union(&back);
                self.targets.push(Target { scratch_base, back });
                self.body(body, &mut live);
                self.targets.pop();
                return live;
            }
            back.union(&next);
        }
        // Did not converge in the allowance: abandon rather than guess.
        self.ok = false;
        after.clone()
    }

    /// A `do`-style body: forms run in order, so liveness flows through them
    /// in REVERSE order.
    fn body(&mut self, forms: &mut [Ir], live: &mut SlotSet) {
        for ir in forms.iter_mut().rev() {
            self.node(ir, live);
        }
    }

    fn node(&mut self, ir: &mut Ir, live: &mut SlotSet) {
        if self.budget == 0 {
            self.ok = false;
            return;
        }
        self.budget -= 1;
        if !self.ok {
            return;
        }
        match ir {
            // S9: none of these reads a frame slot. `SiblingRef` joins them
            // -- it reads `l.me`'s recursive binding group, never `l.slots`.
            Ir::Const(_)
            | Ir::LoadCapture(_)
            | Ir::SelfRef
            | Ir::SiblingRef(_)
            | Ir::GlobalRef { .. }
            | Ir::CreationEnvLookup { .. } => {}

            Ir::LoadSlot(i) => {
                let s = *i;
                if self.rewrite && !live.contains(s) && !self.excluded.contains(s) {
                    *ir = Ir::LoadSlotTake(s);
                }
                live.insert(s);
            }
            // Only reachable if this pass ever ran twice over one body; it
            // is the same read either way, which is what makes the
            // validate-then-rewrite protocol (S10) exact.
            Ir::LoadSlotTake(i) => live.insert(*i),

            // S3: the two branches are alternatives, not a sequence. Each is
            // walked against its OWN copy of the live-out set and the
            // results are unioned, so a read on one branch does not keep the
            // slot alive on the other -- `(if c (f m) (g m))` takes in both
            // -- while a read AFTER the `if` (which is in the live-out set
            // both copies start from) suppresses both.
            Ir::If { test, then, els } => {
                let mut taken = live.clone();
                self.node(then, &mut taken);
                // A missing else branch reads nothing, so its live-in IS the
                // live-out set -- which is what `untaken` already holds.
                let mut untaken = live.clone();
                if let Some(e) = els {
                    self.node(e, &mut untaken);
                }
                taken.union(&untaken);
                *live = taken;
                self.node(test, live);
            }

            Ir::Do(body) => self.body(body, live),

            Ir::Let { binds, body } => {
                self.body(body, live);
                self.binds(binds, live);
            }

            Ir::Loop {
                binds,
                scratch_base,
                body,
            } => {
                // S7: only an all-plain-symbol binding list has a kill set;
                // anything destructured interleaves `:or` defaults and
                // `uncons` between the writes, so nothing loop-carried is
                // taken there.
                let mut killed = SlotSet::new(self.n_slots);
                if binds
                    .iter()
                    .all(|(p, _)| matches!(p, CompiledPattern::Slot(_)))
                {
                    for (p, _) in binds.iter() {
                        if let CompiledPattern::Slot(i) = p {
                            killed.insert(*i);
                        }
                    }
                }
                *live = self.loop_like(body, *scratch_base, &killed, live);
                // The inits run once, before the loop, in order.
                self.binds(binds, live);
            }

            // S8: read-only. Its seeds/invariant loads are read at entry and
            // its `fallback` re-reads them, so the whole subtree contributes
            // liveness and is never rewritten.
            Ir::NumLoop(nl) => {
                let mut reads = SlotSet::new(self.n_slots);
                collect_slot_reads(ir_of_num_loop(nl), &mut reads);
                for s in &nl.seeds {
                    if let NumSeed::Slot(i) = s {
                        reads.insert(*i);
                    }
                }
                for (_, l) in &nl.loads {
                    if let NumLoad::Slot(i) = l {
                        reads.insert(*i);
                    }
                }
                live.union(&reads);
            }

            // S5: control leaves for the target's back edge; the syntactic
            // continuation already in `live` is unreachable but harmless to
            // keep. S4: args are written into the scratch block left to
            // right, so they are walked right to left.
            Ir::Recur { args, scratch_base } => {
                let back = match self
                    .targets
                    .iter()
                    .rev()
                    .find(|t| t.scratch_base == *scratch_base)
                {
                    Some(t) => t.back.clone(),
                    // `resolve.rs` only ever emits a `Recur` inside the body
                    // of the target it names, so this is unreachable --
                    // abandoning rather than assuming keeps it that way.
                    None => {
                        self.ok = false;
                        return;
                    }
                };
                live.union(&back);
                for a in args.iter_mut().rev() {
                    self.node(a, live);
                }
            }

            // S6. `exec_try`'s order is: body; then either `finally` (normal
            // exit) or a catch arm then `finally` (error), and an error can
            // divert from ANY point of the body -- so the body's live-out is
            // every handler's live-in, not what follows the `try`. C3g: any
            // number of catch arms, but only ONE ever runs for a given
            // throw -- since which one is not known statically, each arm's
            // live-in (independently computed from `fin_in`, what's live
            // after IT, with its OWN binding slot removed) is UNIONED into
            // `catches_in` below, a safe over-approximation of "live after
            // some catch arm ran".
            Ir::Try {
                body,
                catches,
                finally,
            } => {
                let mut fin_in = live.clone();
                if let Some(f) = finally {
                    self.body(f, &mut fin_in);
                }
                // Catch arms run before `finally`, so `finally`'s live-in is
                // what is live after whichever catch body ran.
                let mut catches_in = fin_in.clone();
                for arm in catches.iter_mut().rev() {
                    let mut this_in = fin_in.clone();
                    self.body(&mut arm.body, &mut this_in);
                    // The catch binding is written on entry to the clause.
                    this_in.remove(arm.slot);
                    catches_in.union(&this_in);
                }
                let mut body_out = fin_in;
                body_out.union(&catches_in);
                self.body(body, &mut body_out);
                *live = body_out;
            }

            Ir::Def { value, .. } => {
                if let Some(v) = value {
                    self.node(v, live);
                }
            }

            // C1: strictly sequential (inits in order, then body); the
            // unwind reads no slots, and an error's handler live-in is the
            // enclosing `try`'s S6 business -- so this is exactly `Do`.
            Ir::DynBind(d) => {
                self.body(&mut d.body, live);
                for (_, _, init) in d.pairs.iter_mut().rev() {
                    self.node(init, live);
                }
            }

            // Callee before args (`eval_list`'s order), hence args first
            // walking backwards.
            Ir::Call { callee, args, .. } => {
                self.args(args, live);
                self.node(callee, live);
            }
            // The head is a cell/env read, not a slot read, so only the args
            // matter here.
            Ir::CallGlobal { args, .. }
            | Ir::CallCreationEnv { args, .. }
            | Ir::Intrinsic { args, .. } => self.args(args, live),

            Ir::VectorLit(items) | Ir::SetLit(items) => self.args(items, live),
            // `exec_map` evaluates key then value, per pair, in order.
            Ir::MapLit(pairs) => {
                for (k, v) in pairs.iter_mut().rev() {
                    self.node(v, live);
                    self.node(k, live);
                }
            }
            Ir::Throw { value, .. } => self.node(value, live),

            // S2: the capture is a read, at the moment the closure is built.
            // The template's own body has its own slot space and is analysed
            // when IT is compiled, so this pass must not descend into it.
            Ir::MakeClosure { caps, .. } => {
                for c in caps.iter() {
                    if let CaptureSrc::Slot(i) = c {
                        live.insert(*i);
                    }
                }
            }
            // S2, once per member: every member's captures are read at the
            // moment this node runs, so every slot they name is live coming
            // IN. The templates have their own slot spaces and are analysed
            // when they are compiled, so this must not descend into them.
            Ir::MakeRecGroup { members, .. } => {
                for m in members.iter() {
                    for c in m.caps.iter() {
                        if let CaptureSrc::Slot(i) = c {
                            live.insert(*i);
                        }
                    }
                }
            }
            // S2 again (field3/W-RESOLVE): the bridge's reads happen when
            // this node runs, so every slot it names is live coming IN.
            // The escaped `Form` is source, not `Ir`, and has no slots of
            // its own to descend into.
            Ir::Escape(e) => {
                for (_, src) in e.binds.iter() {
                    if let CaptureSrc::Slot(i) = src {
                        live.insert(*i);
                    }
                }
            }
            // W-FIELDGET: both halves of this node read frame slots -- the
            // fast path reads `recv`, and the fallback is the `Ir::Escape`
            // this node was built from, whose bridge reads its binds. Neither
            // may move out of a slot: the fallback runs AFTER the fast path
            // has already read the receiver, and ordinary compiled code may
            // read the same slot after both. So this is the `Ir::Escape` arm
            // above, plus the receiver.
            // K5: fallback bridge slots are escape-excluded (never moved); args are ordinary reads.
            Ir::New(n) => {
                self.node(&mut n.fallback, live);
                self.args(&mut n.args, live);
            }
            Ir::FieldGet(fg) => {
                if let FieldRecv::Slot(i) = fg.recv {
                    live.insert(i);
                }
                if let Ir::Escape(e) = &fg.fallback {
                    for (_, src) in e.binds.iter() {
                        if let CaptureSrc::Slot(i) = src {
                            live.insert(*i);
                        }
                    }
                }
            }
            // lsp/setf: `owner_slot` is genuinely read (to reach the
            // instance); `field_slot` is unconditionally OVERWRITTEN, never
            // read, by this node -- but conservatively marking it live too
            // (rather than killing it) only forfeits a possible
            // `LoadSlotTake` upstream, never causes an incorrect one, so
            // there is no dedicated kill path here (matching `FieldGet`'s
            // conservative treatment of its own receiver slot).
            Ir::SetMutField { owner_slot, field_slot, value, .. } => {
                live.insert(*owner_slot);
                live.insert(*field_slot);
                self.node(value, live);
            }
        }
    }

    /// Arguments (or literal elements): evaluated left to right, so walked
    /// right to left (S4).
    fn args(&mut self, args: &mut [Ir], live: &mut SlotSet) {
        for a in args.iter_mut().rev() {
            self.node(a, live);
        }
    }

    /// A `let`/`loop` binding list: `init0 bind0 init1 bind1 ..`, so
    /// backwards it is `.. bind1 init1 bind0 init0`. Binding kills the
    /// pattern's slots -- which is sound for the initial pass over a binding
    /// list because `resolve.rs` never reuses a slot index, so an init can
    /// never read the slot its own binding is about to write.
    fn binds(&mut self, binds: &mut [(CompiledPattern, Ir)], live: &mut SlotSet) {
        for (pat, init) in binds.iter_mut().rev() {
            self.pattern(pat, live);
            self.node(init, live);
        }
    }

    /// A binding-site pattern: every leaf slot is written unconditionally
    /// (`exec_pattern` has no path that skips one), so every leaf kills. A
    /// map entry's `:or` default is an ordinary expression evaluated BEFORE
    /// that entry's write and after the previous entry's, and is walked
    /// accordingly; treating its reads as unconditional when it only runs
    /// for an absent key is conservative in the safe direction.
    fn pattern(&mut self, pat: &mut CompiledPattern, live: &mut SlotSet) {
        if self.budget == 0 {
            self.ok = false;
            return;
        }
        self.budget -= 1;
        match pat {
            CompiledPattern::Slot(i) => live.remove(*i),
            CompiledPattern::Seq(steps) => {
                for step in steps.iter_mut().rev() {
                    match step {
                        SeqStep::Elem(p) | SeqStep::Rest(p) | SeqStep::As(p) => {
                            self.pattern(p, live)
                        }
                    }
                }
            }
            CompiledPattern::Map(m) => {
                // `:as` binds last.
                if let Some(p) = &mut m.as_pat {
                    self.pattern(p, live);
                }
                for e in m.entries.iter_mut().rev() {
                    self.pattern(&mut e.target, live);
                    if let Some(d) = &mut e.default {
                        self.node(d, live);
                    }
                }
            }
        }
    }
}

/// The generic `Ir::Loop` a `NumLoop` keeps as its fallback.
fn ir_of_num_loop(nl: &super::ir::NumLoop) -> &Ir {
    &nl.fallback
}

/// Every slot a subtree READS, ignoring order and control flow -- the
/// conservative summary S8 uses for a `NumLoop`.
fn collect_slot_reads(ir: &Ir, out: &mut SlotSet) {
    walk(ir, &mut |n| match n {
        Ir::LoadSlot(i) | Ir::LoadSlotTake(i) => out.insert(*i),
        Ir::MakeClosure { caps, .. } => {
            for c in caps {
                if let CaptureSrc::Slot(i) = c {
                    out.insert(*i);
                }
            }
        }
        Ir::MakeRecGroup { members, .. } => {
            for m in members {
                for c in &m.caps {
                    if let CaptureSrc::Slot(i) = c {
                        out.insert(*i);
                    }
                }
            }
        }
        Ir::Escape(e) => {
            for (_, src) in &e.binds {
                if let CaptureSrc::Slot(i) = src {
                    out.insert(*i);
                }
            }
        }
        // lsp/setf: conservative S8 summary -- both slots this node
        // touches, read or written, count as "read" for a NumLoop
        // fallback's purposes (see the `node()` arm's identical call).
        Ir::SetMutField { owner_slot, field_slot, .. } => {
            out.insert(*owner_slot);
            out.insert(*field_slot);
        }
        Ir::NumLoop(nl) => {
            for s in &nl.seeds {
                if let NumSeed::Slot(i) = s {
                    out.insert(*i);
                }
            }
            for (_, l) in &nl.loads {
                if let NumLoad::Slot(i) = l {
                    out.insert(*i);
                }
            }
        }
        _ => {}
    });
}

/// Pre-order walk over every `Ir` node of a subtree, INCLUDING the generic
/// loop a `NumLoop` carries as its fallback and every expression buried in a
/// binding pattern's `:or` defaults. It deliberately does NOT descend into an
/// `Ir::MakeClosure` template: that is a different fn, with its own slots.
fn walk(ir: &Ir, f: &mut impl FnMut(&Ir)) {
    f(ir);
    match ir {
        Ir::Const(_)
        | Ir::LoadSlot(_)
        | Ir::LoadSlotTake(_)
        | Ir::LoadCapture(_)
        | Ir::SelfRef
        | Ir::GlobalRef { .. }
        | Ir::CreationEnvLookup { .. }
        | Ir::Escape(_)
        | Ir::SiblingRef(_)
        // Same rule as `MakeClosure`: a group's member templates are other
        // fns, with their own slot spaces.
        | Ir::MakeRecGroup { .. }
        | Ir::MakeClosure { .. } => {}
        // W-FIELDGET: descend into the `Ir::Escape` this node carries, so
        // every visitor keyed on `Ir::Escape` (slot-move exclusion, the
        // read-slot collector) sees the bridge and treats it exactly as it
        // did before this node wrapped it.
        Ir::FieldGet(fg) => walk(&fg.fallback, f),
        Ir::New(n) => {
            n.args.iter().for_each(|a| walk(a, f));
            walk(&n.fallback, f);
        }
        Ir::If { test, then, els } => {
            walk(test, f);
            walk(then, f);
            if let Some(e) = els {
                walk(e, f);
            }
        }
        Ir::Do(body) => walk_all(body, f),
        Ir::Let { binds, body } | Ir::Loop { binds, body, .. } => {
            for (pat, init) in binds {
                walk_pattern(pat, f);
                walk(init, f);
            }
            walk_all(body, f);
        }
        Ir::NumLoop(nl) => walk(&nl.fallback, f),
        Ir::Recur { args, .. }
        | Ir::CallGlobal { args, .. }
        | Ir::CallCreationEnv { args, .. }
        | Ir::Intrinsic { args, .. }
        | Ir::VectorLit(args)
        | Ir::SetLit(args) => walk_all(args, f),
        Ir::Call { callee, args, .. } => {
            walk(callee, f);
            walk_all(args, f);
        }
        Ir::MapLit(pairs) => {
            for (k, v) in pairs {
                walk(k, f);
                walk(v, f);
            }
        }
        Ir::Throw { value, .. } => walk(value, f),
        Ir::SetMutField { value, .. } => walk(value, f),
        Ir::Try {
            body,
            catches,
            finally,
        } => {
            walk_all(body, f);
            for arm in catches {
                walk_all(&arm.body, f);
            }
            if let Some(fin) = finally {
                walk_all(fin, f);
            }
        }
        Ir::Def { value, .. } => {
            if let Some(v) = value {
                walk(v, f);
            }
        }
        Ir::DynBind(d) => {
            for (_, _, init) in &d.pairs {
                walk(init, f);
            }
            walk_all(&d.body, f);
        }
    }
}

pub(crate) fn walk_all(irs: &[Ir], f: &mut impl FnMut(&Ir)) {
    for ir in irs {
        walk(ir, f);
    }
}

fn walk_pattern(pat: &CompiledPattern, f: &mut impl FnMut(&Ir)) {
    match pat {
        CompiledPattern::Slot(_) => {}
        CompiledPattern::Seq(steps) => {
            for step in steps {
                match step {
                    SeqStep::Elem(p) | SeqStep::Rest(p) | SeqStep::As(p) => walk_pattern(p, f),
                }
            }
        }
        CompiledPattern::Map(m) => {
            for e in &m.entries {
                walk_pattern(&e.target, f);
                if let Some(d) = &e.default {
                    walk(d, f);
                }
            }
            if let Some(p) = &m.as_pat {
                walk_pattern(p, f);
            }
        }
    }
}
