//! Lane tag-flow fixpoint (W1, LATENCY-CAMPAIGN.md).
//!
//! For an already-built `Ir::NumLoop`, computes the set of STABLE per-
//! binding "tag vectors" its own `recur` transition admits: assignments of
//! `Int`/`Float` to each binding register that reproduce THEMSELVES after
//! one trip through the loop's own op list. A tag vector that reproduces
//! itself is a fixed point of the recurrence's steady-state typing, and is
//! exactly what a precompiled "lane variant" specializes for: an unboxed
//! i64/f64 register file, no per-op tag dispatch, `checked_*` deopt on an
//! i64 op only (the abstract semantics below treat `Int op Int` as staying
//! `Int` -- the overflow edge is precisely the runtime deopt case, not a
//! second steady state, matching the campaign doc's "constants have known
//! tags; add/mul(I,I)=I-with-overflow-edge; anything touching F is F").
//!
//! This is a STATIC over-approximation of what run time can observe, not a
//! brute-force search: `NumSeed::Const` bindings enter with a KNOWN tag; a
//! `NumSeed::Slot` binding or a `NumLoad` is unknown at compile time, so
//! both are enumerated, and each resulting entry state is forward-iterated
//! to ITS fixed point (never searched for among all abstract fixed points --
//! most of those are unreachable from any real entry, e.g. an all-`Float`
//! vector for a loop with no `Float` anywhere in it). At run time
//! `exec_num_loop` runs the FIRST iteration in the tagged machine (which is
//! what actually observes the concrete tags), and looks up the resulting
//! vector against the ones computed here.
//!
//! `/` is never reachable from a `NumLoop` op list (`resolve::
//! build_num_expr` has no arm for `IntrinOp::Div2`), so every op this module
//! sees is total -- there is no failure mode to model, only a tag.

use super::ir::{NumBin, NumBranch, NumCmp, NumLoop, NumOp, NumSeed};
use crate::builtins::numbers::Num;

/// The abstract type of a register: everything a `NumLoop` register can
/// hold, minus the concrete value.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Tag {
    I,
    F,
}

fn num_tag(n: Num) -> Tag {
    match n {
        Num::I(_) => Tag::I,
        Num::F(_) => Tag::F,
    }
}

/// `add`/`sub`/`mul`'s abstract tag: `Int op Int` stays `Int` (the
/// overflow edge is a runtime deopt, not a second steady state); anything
/// else touching a `Float` is `Float`. `combine(Tag::I, x) == x` for every
/// `x`, which is also why `AddFold`/`MulFold` (identity-folded-in-first)
/// collapse to the same rule as plain `Add`/`Mul` below: folding in the
/// `Int` identity can never change a tag.
fn combine(a: Tag, b: Tag) -> Tag {
    if a == Tag::F || b == Tag::F {
        Tag::F
    } else {
        Tag::I
    }
}

/// One full static "world" a lane variant would specialize for: a stable
/// tag for every binding register (what `recur` rebinds every iteration)
/// and an assumed tag for every loop-invariant load THAT ACTUALLY FEEDS AN
/// OP (fixed for the whole loop run, but unknown at compile time).
///
/// `loads` deliberately omits any `NumLoad` the loop only ever COMPARES
/// (never uses as an arithmetic operand): such a load's concrete tag can
/// never change what a lane variant computes (`numbers::lt`/`num_eq`
/// already handle both tags uniformly, and no `LaneOp` ever reads it), so
/// requiring it to match a specific tag at run time would reject a
/// perfectly good lane match over a register nothing in the lane even
/// looks at -- exactly the bug this comment replaced (see `bench/
/// optimization-log.md`'s W1 section).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneWorld {
    /// Registers `0..seeds.len()`, in binding order -- see `NumLoop::seeds`.
    pub binds: Vec<Tag>,
    /// `(register, tag)` pairs, one per load this world's ops actually
    /// read -- NOT parallel to `NumLoop::loads` (a load absent here can be
    /// any tag at run time; see this struct's doc).
    pub loads: Vec<(u8, Tag)>,
}

const REGS: usize = super::ir::NUM_REGS;

/// Builds the register tag file consts/bindings/loads seed, leaving every
/// temporary register `None` (unwritten) for `run_ops_tags` to fill in.
fn seed_regs(nl: &NumLoop, binds: &[Tag], loads: &[Tag]) -> [Option<Tag>; REGS] {
    let mut regs = [None; REGS];
    for (r, n) in &nl.consts {
        regs[(*r as usize) & (REGS - 1)] = Some(num_tag(*n));
    }
    for (i, t) in binds.iter().enumerate() {
        regs[i] = Some(*t);
    }
    for ((r, _), t) in nl.loads.iter().zip(loads.iter()) {
        regs[(*r as usize) & (REGS - 1)] = Some(*t);
    }
    regs
}

/// One op's abstract result tag, given its operands' tags -- shared by
/// [`run_ops_tags`] (tag-only, for [`feasible_worlds`]'s fixpoint) and
/// [`lower_ops_and_propagate`] (tag propagation fused with lowering, for
/// [`build_lane_variants`]). AddFold/MulFold fold the operator's `Int`
/// identity in first, but `combine(Tag::I, a) == a`, so their abstract tag
/// is exactly `combine(a, b)` -- the identity step cannot change a TAG,
/// only a value (see [`LaneOp`]'s doc for where it changes a VALUE).
fn combine_op_tag(_op: NumBin, a: Tag, b: Tag) -> Tag {
    combine(a, b)
}

/// Runs one op list abstractly, MUTATING `regs` as it goes -- so a later op
/// reading a register an EARLIER op in this same list just wrote sees that
/// write, not this list's overall final state. This matters: an n-ary
/// `+`/`*`'s fold reuses ONE destination register as its accumulator across
/// every step (`resolve::build_num_expr`'s `IntrinOp::Add | IntrinOp::Mul`
/// arm), so "read the tag of register R" genuinely means different things
/// at different points in the SAME op list. `false` means an operand's tag
/// was never seeded -- unreachable for a `NumLoop` that passed
/// `validate_regs`, since every register an op reads is either a const, a
/// binding, a load, or an earlier op's `dst` in the same list -- but
/// checked rather than assumed, since this module must never panic on a
/// hostile/future op list.
fn run_ops_tags(ops: &[NumOp], regs: &mut [Option<Tag>; REGS]) -> bool {
    for op in ops {
        let (Some(a), Some(b)) = (
            regs[(op.a as usize) & (REGS - 1)],
            regs[(op.b as usize) & (REGS - 1)],
        ) else {
            return false;
        };
        regs[(op.dst as usize) & (REGS - 1)] = Some(combine_op_tag(op.op, a, b));
    }
    true
}

/// One step of the loop's OWN transition: runs every `Recur` branch present
/// (there can be one or two -- `(if t (recur ..) (recur ..))` is in-grammar)
/// from `binds`/`loads` and returns the next binding vector, or `None` if a
/// register was read before it was ever written (unreachable for a
/// `NumLoop` that passed `resolve::validate_regs`).
///
/// With two `Recur` branches the ACTUAL next state depends on which the
/// test picks at run time, which this static pass cannot know -- so the two
/// candidates are joined register-wise with [`combine`] (`Tag::I` only
/// where BOTH branches would leave it `Int`), a sound over-approximation:
/// the lattice `Int` (sqsubseteq) `Float` in play here has `Float`
/// absorbing, so "could be `Float`" is always the safe direction to round
/// toward when two branches disagree.
fn step(nl: &NumLoop, binds: &[Tag], loads: &[Tag]) -> Option<Vec<Tag>> {
    let mut acc: Option<Vec<Tag>> = None;
    for br in [&nl.then, &nl.els] {
        // Only a `Recur` moves the state; a `Ret` and (W-NUMLOOP) a
        // `RetNil` both END the loop, so neither contributes to the
        // transition this fixpoint is over.
        let NumBranch::Recur { ops, next } = br else {
            continue;
        };
        let mut regs = seed_regs(nl, binds, loads);
        if !run_ops_tags(ops, &mut regs) {
            return None;
        }
        if next.len() != binds.len() {
            return None;
        }
        let next_tags: Vec<Tag> = next
            .iter()
            .map(|r| regs[(*r as usize) & (REGS - 1)])
            .collect::<Option<Vec<_>>>()?;
        acc = Some(match acc {
            None => next_tags,
            Some(prev) => prev.iter().zip(next_tags.iter()).map(|(a, b)| combine(*a, *b)).collect(),
        });
    }
    acc
}

/// Which registers are ever READ, anywhere a lane variant would need to
/// know their array to read them: as an operand of some `NumOp` (the test
/// AND both branches' op lists -- a `Ret` branch's ops matter too, since
/// even though they run once at exit rather than every iteration, a lane
/// variant's exit path still needs one concrete instruction, typed for
/// whichever world it belongs to), AND every DIRECT register reference with
/// no op involved: the test's own two operands (`(< i k)` reads `k`
/// directly, no `NumOp` -- this was the bug this fn's first version had,
/// caught by the differential suite's own corpus shape, not a probe: a load
/// read only this way was missing from "used" entirely, so `full_reg_tags`
/// never seeded it and `tag_of` panicked resolving `test.b`), and every
/// `Recur`'s `next` / `Ret`'s `out`.
///
/// Used to prune [`feasible_worlds`]'s load enumeration down to loads that
/// can actually change what a lane variant reads or computes -- and, since
/// `full_reg_tags`/`lower_reg` still need a decided tag for every register
/// this fn marks, it MUST be a superset of every register any lowering step
/// dereferences, not just `NumOp` operands.
fn ops_operand_regs(nl: &NumLoop) -> [bool; REGS] {
    let mut used = [false; REGS];
    fn mark(ops: &[NumOp], used: &mut [bool; REGS]) {
        for op in ops {
            used[(op.a as usize) & (REGS - 1)] = true;
            used[(op.b as usize) & (REGS - 1)] = true;
        }
    }
    fn mark_reg(r: u8, used: &mut [bool; REGS]) {
        used[(r as usize) & (REGS - 1)] = true;
    }
    mark(&nl.test.ops, &mut used);
    mark_reg(nl.test.a, &mut used);
    mark_reg(nl.test.b, &mut used);
    for br in [&nl.then, &nl.els] {
        match br {
            NumBranch::Recur { ops, next } => {
                mark(ops, &mut used);
                for r in next {
                    mark_reg(*r, &mut used);
                }
            }
            NumBranch::Ret { ops, out } => {
                mark(ops, &mut used);
                mark_reg(*out, &mut used);
            }
            // W-NUMLOOP: a nil-terminal branch reads no register at all --
            // it has neither an op list nor an output. Nothing to mark.
            NumBranch::RetNil => {}
        }
    }
    used
}

/// Every feasible lane world for `nl`: forward-reachable fixed points of the
/// loop's own transition, starting from its ACTUAL possible entry states --
/// `NumSeed::Const` bindings enter with a KNOWN tag (never enumerated;
/// that's the campaign doc's "constants have known tags"), `NumSeed::Slot`
/// bindings and `loads` are unknown at compile time and enumerated both
/// ways.
///
/// This is deliberately NOT "every self-consistent tag vector": brute-forcing
/// that (2^NUM_MAX_BINDS candidates, checked for self-consistency with no
/// regard for whether any entry state reaches them) counts abstract fixed
/// points no entry state can ever actually reach -- e.g. an all-`Float`
/// vector for a loop with no `Float` anywhere in it, which is exactly what
/// inflated the W1 kill-probe's first (wrong) run to 256 "feasible" worlds
/// for an 8-binding all-`Int`-constant loop; see `bench/optimization-log.md`.
/// [`combine`] only ever turns `Int` INTO `Float`, never back, so
/// forward-iterating from a concrete entry state converges to a UNIQUE
/// fixed point in at most `seeds.len()` steps (each non-final step flips at
/// least one register `Int`->`Float`, which can happen to each register at
/// most once) -- there is no cycle to search for.
pub fn feasible_worlds(nl: &NumLoop) -> Vec<LaneWorld> {
    // A loop with a `Recur` in BOTH branches has no single per-branch
    // typed lowering that stays sound (see `build_lane_variants`'s doc):
    // declining is always correct, so lane specialization simply doesn't
    // apply. No loop in this crate's own corpus has this shape (`bench/
    // optimization-log.md`'s W1 kill-probe numbers), so this costs nothing
    // observed today and keeps a real gap closed rather than papered over.
    if matches!(nl.then, NumBranch::Recur { .. }) && matches!(nl.els, NumBranch::Recur { .. }) {
        return Vec::new();
    }
    let n = nl.seeds.len();
    let m = nl.loads.len();
    debug_assert!(n + m <= 20, "tag-vector enumeration would be too wide");
    // A load read only by the TEST (e.g. `k` in `(< i k)`, never an operand
    // of any `NumOp`) never influences a binding's propagated tag --
    // comparisons go through `as_f64`/`num_eq`, which already handle both
    // tags uniformly, so such a load doesn't need a lane decision at all.
    // Restricting enumeration to loads that actually feed an op keeps the
    // world count from doubling for every purely-compared invariant.
    let used_loads = ops_operand_regs(nl);
    let entry_known: Vec<Option<Tag>> = nl
        .seeds
        .iter()
        .map(|s| match s {
            NumSeed::Const(v) => Some(num_tag(*v)),
            NumSeed::Slot(_) => None,
        })
        .collect();
    let free_binds: Vec<usize> = entry_known
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.is_none().then_some(i))
        .collect();
    let free_loads: Vec<usize> = (0..m)
        .filter(|&i| used_loads[(nl.loads[i].0 as usize) & (REGS - 1)])
        .collect();
    let mut out: Vec<LaneWorld> = Vec::new();
    for load_bits in 0..(1u32 << free_loads.len()) {
        let mut loads: Vec<Tag> = vec![Tag::I; m];
        for (j, &idx) in free_loads.iter().enumerate() {
            loads[idx] = if load_bits & (1 << j) != 0 { Tag::F } else { Tag::I };
        }
        for free_bits in 0..(1u32 << free_binds.len()) {
            let mut cur: Vec<Tag> = entry_known.iter().map(|t| t.unwrap_or(Tag::I)).collect();
            for (j, &idx) in free_binds.iter().enumerate() {
                cur[idx] = if free_bits & (1 << j) != 0 { Tag::F } else { Tag::I };
            }
            let mut ok = true;
            for _ in 0..=n {
                match step(nl, &cur, &loads) {
                    Some(next) if next == cur => break,
                    Some(next) => cur = next,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            // Confirm `cur` is actually stable (the loop above may have
            // exited on the iteration budget rather than a true fixed
            // point, though the monotonicity argument above says it can't).
            if step(nl, &cur, &loads).as_deref() != Some(&cur[..]) {
                continue;
            }
            // Only the loads this world's ops actually read go into the
            // OUTPUT world -- see `LaneWorld::loads`'s doc.
            let loads_pairs: Vec<(u8, Tag)> = free_loads.iter().map(|&idx| (nl.loads[idx].0, loads[idx])).collect();
            let world = LaneWorld { binds: cur, loads: loads_pairs };
            if !out.contains(&world) {
                out.push(world);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Lane codegen: lowering one `LaneWorld` into a flat, fully-typed op list.
//
// This is the part that actually removes the per-op tag dispatch: every
// `LaneOp` below reads its operands from a KNOWN array (an unboxed `[i64;
// NUM_REGS]` or `[f64; NUM_REGS]`, chosen once, at BUILD time, from the
// register's fixed tag in this world) and calls the primitive Rust op
// directly -- `i64::checked_add`/`checked_sub`/`checked_mul` (the exact
// primitive `numbers::add`/`sub`/`mul`'s `Int`/`Int` arm calls) or a plain
// `f64` operator (the exact primitive their blend/`Float`/`Float` arms
// use). Nothing here re-derives arithmetic semantics; it routes around the
// `Num` ENUM wrapper that makes each op's target array a run-time decision,
// not around the arithmetic itself.
// ---------------------------------------------------------------------------

/// A lane-typed operand or destination: which array to read/write, and the
/// index into it. The array is fixed by the register's tag in the
/// [`LaneVariant`]'s world, decided once at build time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneReg {
    I(u8),
    F(u8),
}

/// One flat instruction over the two typed register files. `checked_*`
/// (the `I*` ops) return "deopt" (see `exec::run_lane_variant`) instead of
/// promoting, unlike `numbers::add`/`sub`/`mul`: promotion would change
/// this register's TAG mid-loop, which is precisely the event a lane
/// variant is built to assume never happens -- see `LaneVariant`'s doc.
///
/// `FAddFoldFI`/`FAddFoldFF` exist ONLY because `AddFold`'s identity step
/// is value-observable when its first operand is already `Float`: `0.0 +
/// (-0.0)` is `+0.0`, not `-0.0` (`(+ -0.0 -0.0)` is `0.0` in Clojure
/// *because* `+` folds from `Int(0)`, per `ir::NumBin`'s doc) -- so that
/// step must actually run, not be elided as a value no-op the way it
/// algebraically could be for every other tag combination. See
/// [`lower_op`] for the full case analysis (in particular why `MulFold`
/// and `AddFold` with an `Int`-tagged first operand need NO distinct op at
/// all: both reduce to the plain `Add`/`Mul` lowering exactly).
#[derive(Clone, Copy, Debug)]
pub enum LaneOp {
    IAdd { dst: u8, a: u8, b: u8 },
    ISub { dst: u8, a: u8, b: u8 },
    IMul { dst: u8, a: u8, b: u8 },
    FAdd { dst: u8, a: u8, b: u8 },
    FSub { dst: u8, a: u8, b: u8 },
    FMul { dst: u8, a: u8, b: u8 },
    /// `a` is an `i64` register (promoted to `f64`), `b` an `f64` register.
    FAddIF { dst: u8, a: u8, b: u8 },
    FSubIF { dst: u8, a: u8, b: u8 },
    FMulIF { dst: u8, a: u8, b: u8 },
    /// `a` is an `f64` register, `b` an `i64` register (promoted).
    FAddFI { dst: u8, a: u8, b: u8 },
    FSubFI { dst: u8, a: u8, b: u8 },
    FMulFI { dst: u8, a: u8, b: u8 },
    /// `AddFold(a, b)` with `a`'s tag `Float`, `b`'s tag `Float`: computes
    /// `(0.0 + fr[a]) + fr[b]`, both additions performed.
    FAddFoldFF { dst: u8, a: u8, b: u8 },
    /// `AddFold(a, b)` with `a`'s tag `Float`, `b`'s tag `Int` (promoted).
    FAddFoldFI { dst: u8, a: u8, b: u8 },
}

/// The typed test: an op list, then a comparison. Deliberately reuses
/// `ir::NumCmp` and `builtins::numbers::{lt,le,gt,ge,num_eq}` UNCHANGED
/// (wrapping a lane read back into a `Num` first) rather than lowering the
/// comparison itself into typed variants: a comparison runs ONCE per
/// iteration, off the arithmetic chain the campaign doc's mechanistic
/// finding is about, and `numbers::num_eq` in particular has enough
/// signed-zero/NaN/huge-`Int` subtlety (see `ir::NumCmp`'s doc) that
/// sharing the exact function is worth far more than the two-branch
/// dispatch it costs.
#[derive(Debug)]
pub struct LaneTest {
    pub ops: Vec<LaneOp>,
    pub cmp: NumCmp,
    pub a: LaneReg,
    pub b: LaneReg,
}

#[derive(Debug)]
pub enum LaneBranch {
    Recur { ops: Vec<LaneOp>, next: Vec<LaneReg> },
    Ret { ops: Vec<LaneOp>, out: LaneReg },
    /// W-NUMLOOP: `ir::NumBranch::RetNil`, lowered. There is nothing to
    /// TYPE here -- no ops, no output register -- so this variant is the
    /// same in every world, and a loop whose exit is nil can never be the
    /// branch that decides a lane.
    RetNil,
}

/// One precompiled lane: a register-file world (which array each binding
/// and load lives in) plus the typed op lists for the test and both
/// branches. `exec_num_loop` matches the tags OBSERVED after running one
/// iteration in the tagged machine against `binds`/`loads` here; on a
/// match it converts the tagged registers to `ir`/`fr` arrays once and
/// runs this variant until it exits, deopts (an `I*` op overflows), or --
/// never, by construction, since [`feasible_worlds`] only emits worlds
/// that are fixed points of the loop's own transition -- would otherwise
/// need to switch to a different variant.
///
/// Built ONLY when exactly one of `then`/`els` is a `Recur` (see
/// `feasible_worlds`'s guard): with a `Recur` in both, this world's
/// `binds` is a over-approximating JOIN of what either branch would
/// actually produce, so a branch lowered on its OWN terms could compute a
/// value whose true tag disagrees with what the other branch (and hence
/// the NEXT iteration's lane dispatch) expects. Rather than carry that
/// unsoundness, that shape simply never gets a lane variant.
#[derive(Debug)]
pub struct LaneVariant {
    pub world: LaneWorld,
    pub test: LaneTest,
    pub then: LaneBranch,
    pub els: LaneBranch,
    /// W6: this variant re-read as a shape-specialized superloop, when its
    /// shape is in the closed set [`build_superloop`] recognizes. Stored INLINE, not
    /// behind a `Box`: boxing was tried and measured WORSE on both counts
    /// (-7% with superloops on, and -17% on the interpreted int-sum cell
    /// with them off, versus +12% for the inline form) -- see W6's
    /// switch-off table in `bench/optimization-log.md`, and the note there
    /// on how layout-sensitive that particular cell is.
    /// `None`
    /// (the only value `build_lane_variants` ever constructs -- see
    /// [`attach_superloops`], the one place this is ever filled in) leaves
    /// `exec::run_lane_variant` running the interpreted op lists exactly as
    /// W1 landed them.
    pub sup: Option<SuperLoop>,
}

/// Seeds `regs` with every const/bind/load register's tag -- no ops
/// applied yet.
fn seed_reg_tags(binds: &[Tag], consts_pairs: &[(u8, Num)], loads: &[(u8, Tag)]) -> [Option<Tag>; REGS] {
    let mut regs = [None; REGS];
    for (r, n) in consts_pairs {
        regs[(*r as usize) & (REGS - 1)] = Some(num_tag(*n));
    }
    for (i, t) in binds.iter().enumerate() {
        regs[i] = Some(*t);
    }
    for (r, t) in loads {
        regs[(*r as usize) & (REGS - 1)] = Some(*t);
    }
    regs
}

fn lower_reg_typed(r: u8, t: Tag) -> LaneReg {
    match t {
        Tag::I => LaneReg::I(r),
        Tag::F => LaneReg::F(r),
    }
}

/// Looks up a register's CURRENT tag in `regs` and lowers it to a
/// [`LaneReg`]. Only ever called on a register some earlier step has
/// already seeded or written -- `None` propagates out as `?` from every
/// caller, ending in `build_lane_variants` returning `None` for this world
/// (unreachable for a `NumLoop` that passed `resolve::validate_regs`, same
/// as `run_ops_tags`'s `false`, but checked rather than assumed).
fn lower_reg(regs: &[Option<Tag>; REGS], r: u8) -> Option<LaneReg> {
    Some(lower_reg_typed(r, regs[(r as usize) & (REGS - 1)]?))
}

/// Lowers one `NumOp` into its typed `LaneOp`, GIVEN its operands' tags AT
/// THIS POINT in the op list (not looked up from `regs` here -- see
/// [`lower_ops_and_propagate`] for why that distinction is load-bearing).
/// See [`LaneOp`]'s doc for why `MulFold` and an `Int`-tagged-first-operand
/// `AddFold` need no distinct variant at all.
fn lower_op_typed(op: &NumOp, ta: Tag, tb: Tag) -> LaneOp {
    use LaneOp::*;
    let (dst, a, b) = (op.dst, op.a, op.b);
    match op.op {
        NumBin::Add | NumBin::Mul => plain_binop(op.op, dst, a, ta, b, tb),
        NumBin::Sub => match (ta, tb) {
            (Tag::I, Tag::I) => ISub { dst, a, b },
            (Tag::F, Tag::F) => FSub { dst, a, b },
            (Tag::I, Tag::F) => FSubIF { dst, a, b },
            (Tag::F, Tag::I) => FSubFI { dst, a, b },
        },
        // `add(add(Int(0), a), b)`: the inner step is a true value no-op
        // when `a` is `Int` (`0i64.checked_add(x)` never overflows and
        // always yields `x`), so this reduces EXACTLY to `Add(a, b)`. Only
        // an `a`-tag of `Float` makes the inner step observable.
        NumBin::AddFold if ta == Tag::I => plain_binop(NumBin::Add, dst, a, ta, b, tb),
        NumBin::AddFold => match tb {
            Tag::F => FAddFoldFF { dst, a, b },
            Tag::I => FAddFoldFI { dst, a, b },
        },
        // `mul(mul(Int(1), a), b)`: `1 * x == x` bit-for-bit for EVERY `Num`
        // (integer multiply-by-one never overflows; `1.0 * f` is `f` for
        // every `f64` including `-0.0` and `NaN`'s payload/sign -- IEEE 754
        // multiplication by exactly `1.0` never touches the mantissa or
        // sign), so `MulFold` reduces to `Mul(a, b)` unconditionally.
        NumBin::MulFold => plain_binop(NumBin::Mul, dst, a, ta, b, tb),
    }
}

fn plain_binop(op: NumBin, dst: u8, a: u8, ta: Tag, b: u8, tb: Tag) -> LaneOp {
    use LaneOp::*;
    match (op, ta, tb) {
        (NumBin::Add, Tag::I, Tag::I) => IAdd { dst, a, b },
        (NumBin::Add, Tag::F, Tag::F) => FAdd { dst, a, b },
        (NumBin::Add, Tag::I, Tag::F) => FAddIF { dst, a, b },
        (NumBin::Add, Tag::F, Tag::I) => FAddFI { dst, a, b },
        (NumBin::Mul, Tag::I, Tag::I) => IMul { dst, a, b },
        (NumBin::Mul, Tag::F, Tag::F) => FMul { dst, a, b },
        (NumBin::Mul, Tag::I, Tag::F) => FMulIF { dst, a, b },
        (NumBin::Mul, Tag::F, Tag::I) => FMulFI { dst, a, b },
        (NumBin::Sub | NumBin::AddFold | NumBin::MulFold, ..) => {
            unreachable!("plain_binop is only ever called for Add/Mul (including their Fold/reduced forms)")
        }
    }
}

/// Lowers a whole op list AND propagates tags through `regs` in the SAME
/// pass, one op at a time -- this is the fix for a real bug the W1
/// differential suite caught (`bench/optimization-log.md`'s W1 section): an
/// n-ary `+`/`*` reuses ONE destination register as its fold accumulator
/// across every step, so a later op's operand tag must be read at THIS
/// point in the sequence, not from a snapshot taken after the whole list
/// (or worse, after `then`/`els` too) had already run and overwritten that
/// same register again. Mirrors [`run_ops_tags`] exactly, plus lowering.
fn lower_ops_and_propagate(ops: &[NumOp], regs: &mut [Option<Tag>; REGS]) -> Option<Vec<LaneOp>> {
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        let ta = regs[(op.a as usize) & (REGS - 1)]?;
        let tb = regs[(op.b as usize) & (REGS - 1)]?;
        out.push(lower_op_typed(op, ta, tb));
        regs[(op.dst as usize) & (REGS - 1)] = Some(combine_op_tag(op.op, ta, tb));
    }
    Some(out)
}

/// Lowers one branch from ITS OWN copy of the post-test register state --
/// `then` and `els` never share a register (one monotonic allocator across
/// the whole loop, see `resolve::NumCtx`), so which one runs first cannot
/// matter, but each needs the state AS OF right after the test, not
/// whatever the OTHER branch's lowering left behind.
fn lower_branch(br: &NumBranch, regs: [Option<Tag>; REGS]) -> Option<LaneBranch> {
    let mut regs = regs;
    match br {
        NumBranch::Recur { ops, next } => {
            let ops = lower_ops_and_propagate(ops, &mut regs)?;
            let next = next.iter().map(|r| lower_reg(&regs, *r)).collect::<Option<Vec<_>>>()?;
            Some(LaneBranch::Recur { ops, next })
        }
        NumBranch::Ret { ops, out } => {
            let ops = lower_ops_and_propagate(ops, &mut regs)?;
            let out = lower_reg(&regs, *out)?;
            Some(LaneBranch::Ret { ops, out })
        }
        // W-NUMLOOP: nothing to lower and no tag to decide.
        NumBranch::RetNil => Some(LaneBranch::RetNil),
    }
}

/// Builds every lane variant `nl` admits: one per [`feasible_worlds`]
/// entry. Called once per loop, at RESOLVE time (never per call, never per
/// iteration) -- `resolve::specialize_num_loop` attaches the result to
/// `ir::NumLoop::lane_variants`.
pub fn build_lane_variants(nl: &NumLoop) -> Vec<LaneVariant> {
    feasible_worlds(nl)
        .into_iter()
        .filter_map(|world| {
            let mut regs = seed_reg_tags(&world.binds, &nl.consts, &world.loads);
            let test_ops = lower_ops_and_propagate(&nl.test.ops, &mut regs)?;
            let test_a = lower_reg(&regs, nl.test.a)?;
            let test_b = lower_reg(&regs, nl.test.b)?;
            let then = lower_branch(&nl.then, regs)?;
            let els = lower_branch(&nl.els, regs)?;
            Some(LaneVariant {
                test: LaneTest { ops: test_ops, cmp: nl.test.cmp, a: test_a, b: test_b },
                then,
                els,
                world,
                // The ONE construction site for `LaneVariant`; W6's shape
                // pass is attached afterwards by `attach_superloops`, which
                // is where the `MOVA_NO_SUPERLOOP` emission gate lives.
                sup: None,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// W6 (LATENCY-CAMPAIGN.md §7): SUPERLOOPS -- hoisting a lane variant's
// loop-carried state out of the interpretive frame.
//
// W5's autopsy (`bench/optimization-log.md`) decomposed the 4.7x that
// separates the landed lane machine from a hand-wire into 1.39x "the
// register file is memory" x 3.35x "the interpretive frame": per iteration
// the landed `run_lane_variant` re-reads `lv.test.ops`/`lv.then`/`lv.els`
// out of the `LaneVariant`, re-matches a `LaneBranch` discriminant, and
// round-trips every binding through `Num` twice (once to compare, once to
// rebind). None of that is the arithmetic. Op-level fusion cannot reach it
// (collapsing the entire body to ONE op measured +0.2%); only moving the
// loop-carried values into Rust LOCALS can, and locals require CONSTANT
// register indices, which in turn requires a shape-specialized loop.
//
// A superloop is that shape, recovered statically from an already-built
// `LaneVariant`:
//
//   (loop [x0 .. x1 ..]           ; 1 or 2 bindings, each with a fixed lane
//     (if (x0 <cmp> INV)          ; test over binding 0 and a loop invariant
//       (recur CHAIN0 CHAIN1)     ; each next value a straight-line chain
//       RET))                     ; any exit expression -- see below
//
// where a CHAIN is a sequence of at most [`MAX_STEPS`] binary steps applied
// to that binding's OWN previous value, each step's other operand being a
// loop invariant (read ONCE, at lane entry) or another binding's
// iteration-start value. That is exactly the state the runtime can keep in
// `i64`/`f64` locals for the whole call: nothing in the iteration touches
// the register-file arrays at all.
//
// What a superloop deliberately does NOT specialize: the `Ret` branch's own
// op list (it runs once, at exit) and the deopt path. Both write the
// bindings back into the register files and hand off to the existing
// generic code (`exec::run_lane_ops`, `exec::run_num_loop`) unchanged, so a
// superloop is exactly as observable as the interpreted variant it replaces
// -- same value, same deopt iteration, same fuel accounting.
//
// Shapes this pass DECLINES (declining is always correct -- the interpreted
// lane variant still runs them): more than 2 bindings; a test with its own
// op list, or one whose left operand is not binding 0, or whose right
// operand is not loop-invariant; a `next` slot that is not a chain rooted at
// its OWN binding (`(recur b a)`-style permutations); any op whose result is
// read more than once (the chains would have to share a temporary); chains
// longer than [`MAX_STEPS`].
// ---------------------------------------------------------------------------

/// The longest per-binding step chain a superloop shape accepts. Four
/// covers every shape in this crate's corpus (the longest is the 3-step
/// n-ary fold `(+ a 1 2 3)`); a longer one is declined rather than run
/// half-specialized.
pub const MAX_STEPS: usize = 4;

impl LaneReg {
    /// The register index, whichever array it names.
    pub fn idx(self) -> u8 {
        match self {
            LaneReg::I(r) | LaneReg::F(r) => r,
        }
    }
    /// Which array: `true` = the `f64` file.
    pub fn is_f(self) -> bool {
        matches!(self, LaneReg::F(_))
    }
}

/// A step's second operand: a loop INVARIANT register (read once, at lane
/// entry -- a const or a `NumLoad`, never written by the loop), or another
/// binding's ITERATION-START value (`X`, which the runtime keeps in a local
/// exactly like the chain's own accumulator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src {
    K(LaneReg),
    X(u8),
}

/// One step of an `i64`-lane chain. Every arithmetic op is the `checked_*`
/// the interpreted `LaneOp` runs, so an overflow deopts on exactly the same
/// iteration. `Rsub` is `k - x` (the reversed operand order); `Add`/`Mul`
/// need no reversed form because `checked_add`/`checked_mul` are
/// commutative in both value and overflow condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IStep {
    Add(Src),
    Sub(Src),
    Rsub(Src),
    Mul(Src),
}

/// One step of an `f64`-lane chain. Unlike the `i64` lane, the reversed
/// forms are kept EXPLICIT for every op: `f64` addition and multiplication
/// are commutative in value but not necessarily in which NaN payload a
/// two-NaN operation propagates, and this pass is not in the business of
/// deciding that. `AddFold`/`RaddFold` are `AddFold`'s value-observable
/// identity step (`(0.0 + a) + b`, see [`LaneOp`]'s doc) with the carried
/// value on the left and the right respectively.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FStep {
    Add(Src),
    Radd(Src),
    Sub(Src),
    Rsub(Src),
    Mul(Src),
    Rmul(Src),
    AddFold(Src),
    RaddFold(Src),
}

/// One binding's next-value program. Exactly one of the two vectors is
/// non-empty (the one matching that binding's lane); BOTH empty is the
/// identity chain (`(recur .. x ..)` rebinding a value unchanged).
#[derive(Debug, Default)]
pub struct Chain {
    pub i: Vec<IStep>,
    pub f: Vec<FStep>,
}

/// A lane variant re-read as a superloop: everything `exec::run_superloop`
/// needs to run the whole loop with its state in Rust locals.
#[derive(Debug)]
pub struct SuperLoop {
    /// `true` when `then` is the `Recur` branch (so `els` is the `Ret`).
    pub recur_then: bool,
    pub cmp: NumCmp,
    /// The test's RIGHT operand -- a loop-invariant register, read once at
    /// lane entry. (The LEFT operand is always binding 0; see the module
    /// section doc for why other shapes are declined.)
    pub test_inv: LaneReg,
    /// Per-binding next-value chains, `[0]` always present, `[1]` used only
    /// when the loop has two bindings.
    pub c: [Chain; 2],
    /// Number of bindings: 1 or 2.
    pub n_binds: usize,
    /// Lane of each binding (`true` = `f64`), from the variant's world.
    pub f0: bool,
    pub f1: bool,
}

/// `(dst, a, b)` plus each operand's lane, for one already-typed `LaneOp`.
/// The lanes are a property of the OP KIND (that is the whole point of lane
/// lowering), not of any register file, so this is a total function.
fn op_parts(op: &LaneOp) -> (u8, u8, u8, bool, bool) {
    use LaneOp::*;
    match *op {
        IAdd { dst, a, b } | ISub { dst, a, b } | IMul { dst, a, b } => (dst, a, b, false, false),
        FAdd { dst, a, b } | FSub { dst, a, b } | FMul { dst, a, b } | FAddFoldFF { dst, a, b } => (dst, a, b, true, true),
        FAddIF { dst, a, b } | FSubIF { dst, a, b } | FMulIF { dst, a, b } => (dst, a, b, false, true),
        FAddFI { dst, a, b } | FSubFI { dst, a, b } | FMulFI { dst, a, b } | FAddFoldFI { dst, a, b } => (dst, a, b, true, false),
    }
}

/// Whether this op writes the `f64` file.
fn op_is_f(op: &LaneOp) -> bool {
    use LaneOp::*;
    !matches!(op, IAdd { .. } | ISub { .. } | IMul { .. })
}

/// True when `p`'s result is read EXACTLY once before that register is
/// redefined -- the condition that makes "decompose the op list into one
/// straight-line chain per binding" exact rather than an approximation. A
/// shared temporary would have to appear in two chains, and a dead one is a
/// shape this pass has not understood; both are declined.
///
/// Note the redefinition cutoff: an n-ary fold reuses ONE destination
/// register across all its steps (`resolve::build_num_expr`), so counting
/// reads of a register index over the WHOLE remaining list would count
/// reads of a LATER value as reads of this one.
fn single_use(ops: &[LaneOp], p: usize, next: &[LaneReg]) -> bool {
    let (r, ..) = op_parts(&ops[p]);
    let mut count = 0usize;
    for op in &ops[p + 1..] {
        let (d, a, b, ..) = op_parts(op);
        count += usize::from(a == r) + usize::from(b == r);
        if d == r {
            return count == 1;
        }
    }
    count += next.iter().filter(|lr| lr.idx() == r).count();
    count == 1
}

/// Recovers one binding's chain by walking BACKWARDS from `next[j]` to
/// binding `j` itself, consuming one op per step.
///
/// `inv` says which registers are loop-invariant; `consumed` records which
/// ops have already been claimed (by this chain or the other binding's), so
/// the caller can insist that every op in the `Recur` list belongs to
/// exactly one chain.
#[allow(clippy::too_many_arguments)]
fn build_chain(
    j: usize,
    n_binds: usize,
    ops: &[LaneOp],
    next: &[LaneReg],
    lane_f: bool,
    inv: &[bool; REGS],
    consumed: &mut [bool],
) -> Option<Chain> {
    let mut isteps: Vec<IStep> = Vec::new();
    let mut fsteps: Vec<FStep> = Vec::new();
    let mut cur = next[j];
    let mut limit = ops.len();
    if cur.is_f() != lane_f {
        return None;
    }
    while cur.idx() as usize != j {
        let p = (0..limit).rev().find(|&p| op_parts(&ops[p]).0 == cur.idx())?;
        if consumed[p] {
            return None;
        }
        let op = &ops[p];
        if op_is_f(op) != lane_f {
            return None;
        }
        let (_, a, b, fa, fb) = op_parts(op);
        // Which operand carries the chain? The one that is NOT loop
        // invariant. With both non-invariant, a temporary (a register this
        // list writes) wins over a binding, and with two bindings the
        // chain's own is the carrier.
        let bind = |r: u8| (r as usize) < n_binds;
        let temp = |r: u8| !inv[r as usize] && !bind(r);
        let rev = if temp(a) {
            false
        } else if temp(b) {
            true
        } else if bind(a) && bind(b) {
            if a as usize == j {
                false
            } else if b as usize == j {
                true
            } else {
                return None;
            }
        } else if !inv[a as usize] {
            false
        } else if !inv[b as usize] {
            true
        } else {
            return None;
        };
        let (carry, carry_f, other, other_f) = if rev { (b, fb, a, fa) } else { (a, fa, b, fb) };
        if carry_f != lane_f {
            return None;
        }
        let src = if bind(other) {
            Src::X(other)
        } else if inv[other as usize] {
            Src::K(if other_f { LaneReg::F(other) } else { LaneReg::I(other) })
        } else {
            return None;
        };
        use LaneOp::*;
        if lane_f {
            fsteps.push(match op {
                FAdd { .. } | FAddIF { .. } | FAddFI { .. } => {
                    if rev {
                        FStep::Radd(src)
                    } else {
                        FStep::Add(src)
                    }
                }
                FSub { .. } | FSubIF { .. } | FSubFI { .. } => {
                    if rev {
                        FStep::Rsub(src)
                    } else {
                        FStep::Sub(src)
                    }
                }
                FMul { .. } | FMulIF { .. } | FMulFI { .. } => {
                    if rev {
                        FStep::Rmul(src)
                    } else {
                        FStep::Mul(src)
                    }
                }
                FAddFoldFF { .. } | FAddFoldFI { .. } => {
                    if rev {
                        FStep::RaddFold(src)
                    } else {
                        FStep::AddFold(src)
                    }
                }
                IAdd { .. } | ISub { .. } | IMul { .. } => return None,
            });
        } else {
            isteps.push(match op {
                IAdd { .. } => IStep::Add(src),
                ISub { .. } => {
                    if rev {
                        IStep::Rsub(src)
                    } else {
                        IStep::Sub(src)
                    }
                }
                IMul { .. } => IStep::Mul(src),
                _ => return None,
            });
        }
        consumed[p] = true;
        cur = if carry_f { LaneReg::F(carry) } else { LaneReg::I(carry) };
        limit = p;
        if isteps.len() + fsteps.len() > MAX_STEPS {
            return None;
        }
    }
    if cur.is_f() != lane_f {
        return None;
    }
    isteps.reverse();
    fsteps.reverse();
    Some(Chain { i: isteps, f: fsteps })
}

/// Reads an already-built [`LaneVariant`] as a [`SuperLoop`], or `None`
/// when its shape is outside the closed set (see the module section doc for
/// the full list of declined shapes -- declining is always correct).
pub fn build_superloop(lv: &LaneVariant) -> Option<SuperLoop> {
    let n = lv.world.binds.len();
    if n == 0 || n > 2 {
        return None;
    }
    if !lv.test.ops.is_empty() {
        return None;
    }
    // W-NUMLOOP: a `RetNil` exit is as good a superloop exit as a `Ret` --
    // better, in fact, since it has no op list for `exec::run_super_impl` to
    // hand back to the interpreted machine. Exactly one branch may `Recur`
    // (a two-`Recur` variant never exists; `feasible_worlds` refuses it).
    let (recur_then, ops, next) = match (&lv.then, &lv.els) {
        (LaneBranch::Recur { ops, next }, LaneBranch::Ret { .. } | LaneBranch::RetNil) => (true, ops, next),
        (LaneBranch::Ret { .. } | LaneBranch::RetNil, LaneBranch::Recur { ops, next }) => (false, ops, next),
        _ => return None,
    };
    if next.len() != n {
        return None;
    }
    // Loop-invariant registers: everything this iteration never writes and
    // that is not a binding. (`lv.test.ops` is empty, checked above, so the
    // `Recur` list is the only writer; a `Ret` branch's writes happen after
    // the superloop has already handed off.)
    let mut inv = [true; REGS];
    for slot in inv.iter_mut().take(n) {
        *slot = false;
    }
    for op in ops.iter() {
        inv[op_parts(op).0 as usize] = false;
    }
    // Test: binding 0 against a loop invariant, no ops of its own.
    if lv.test.a.idx() != 0 || lv.test.a.is_f() != (lv.world.binds[0] == Tag::F) {
        return None;
    }
    if !inv[lv.test.b.idx() as usize] {
        return None;
    }
    for (p, _) in ops.iter().enumerate() {
        if !single_use(ops, p, next) {
            return None;
        }
    }
    let mut consumed = vec![false; ops.len()];
    let f0 = lv.world.binds[0] == Tag::F;
    let f1 = n == 2 && lv.world.binds[1] == Tag::F;
    let c0 = build_chain(0, n, ops, next, f0, &inv, &mut consumed)?;
    let c1 = if n == 2 {
        build_chain(1, n, ops, next, f1, &inv, &mut consumed)?
    } else {
        Chain::default()
    };
    if !consumed.iter().all(|c| *c) {
        return None;
    }
    Some(SuperLoop {
        recur_then,
        cmp: lv.test.cmp,
        test_inv: lv.test.b,
        c: [c0, c1],
        n_binds: n,
        f0,
        f1,
    })
}

/// Attaches a superloop shape to every variant that admits one. Called from
/// `resolve::specialize_num_loop` at RESOLVE time (never per call), and
/// gated there by `MOVA_NO_SUPERLOOP=1` / `Interp::superloop_enabled` --
/// a pure EMISSION gate, exactly like `MOVA_NO_LANES`: with it off no
/// variant carries a shape, so there is nothing left in the hot path to be
/// wrong about.
pub fn attach_superloops(vs: &mut [LaneVariant]) {
    for lv in vs.iter_mut() {
        lv.sup = build_superloop(lv);
    }
}
