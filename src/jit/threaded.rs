//! G1 (docs/JIT.md "Threaded tier"): Cranelift codegen for the threaded
//! tier. Every block here does nothing but call one of `jit::jit_t_*` and
//! branch on its `u32` status -- no `Value` is ever read/written except
//! inside those Rust helpers. See `jit::TCtx`/`jit::call_threaded` for the
//! runtime half; this module only ever runs at lowering time (once per
//! arity, cached by `ThreadedSlot`).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use cranelift_codegen::ir::{condcodes::IntCC, types, AbiParam, InstBuilder, MemFlagsData, Signature};
use cranelift_codegen::ir::{FuncRef, Value as CVal};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use super::calls::{jit_t_call, leak_site_addr};
use super::{
    explain_enabled, jit_t_exec, jit_t_move, jit_t_nil, jit_t_tick_fuel, jit_t_truthy,
    ThreadedEntry, THREADED_LOWER_COUNT, THREADED_LOWER_NS, THREADED_OUT,
};
use crate::compile::ir::{CompiledPattern, Ir};
use crate::compile::CompiledFn;
use crate::value::Value;

#[path = "leaf.rs"]
mod leaf;

/// K6 kill switch: `MOVA_JIT_BORROW=0` clones every call arg (no aliased locals).
fn borrow_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("MOVA_JIT_BORROW").map(|v| v != "0").unwrap_or(true))
}

/// L1 kill switch: `MOVA_JIT_LEAF=0` keeps every leaf on `jit_t_exec`.
fn leaf_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("MOVA_JIT_LEAF").map(|v| v != "0").unwrap_or(true))
}

fn module() -> &'static Mutex<JITModule> {
    static MODULE: OnceLock<Mutex<JITModule>> = OnceLock::new();
    MODULE.get_or_init(|| {
        // L1: the IR verifier is ~a third of compile cost; `MOVA_JIT_VERIFY=1` turns it back on.
        let verify = if std::env::var("MOVA_JIT_VERIFY").as_deref() == Ok("1") { "true" } else { "false" };
        // K3: single-pass regalloc (compile cost); `MOVA_JIT_REGALLOC=backtracking` restores the default.
        let ra = if std::env::var("MOVA_JIT_REGALLOC").as_deref() == Ok("backtracking") { "backtracking" } else { "single_pass" };
        let mut jb = JITBuilder::with_flags(&[("enable_verifier", verify), ("regalloc_algorithm", ra)], cranelift_module::default_libcall_names())
            .expect("threaded tier: host ISA/JITBuilder setup failed");
        jb.symbol("jit_t_exec", jit_t_exec as *const u8);
        jb.symbol("jit_t_nil", jit_t_nil as *const u8);
        jb.symbol("jit_t_truthy", jit_t_truthy as *const u8);
        jb.symbol("jit_t_move", jit_t_move as *const u8);
        jb.symbol("jit_t_tick_fuel", jit_t_tick_fuel as *const u8);
        jb.symbol("jit_t_call", jit_t_call as *const u8);
        jb.symbol(super::aot::LIT_SYM, super::aot::lit_base() as *const u8);
        leaf::register(&mut jb);
        Mutex::new(JITModule::new(jb))
    })
}

/// One active native `Loop` frame: a `Recur`/status-1 targeting
/// `scratch_base` jumps straight to `rebind_block` (mirrors `exec_loop`).
struct LoopFrame {
    scratch_base: u16,
    rebind_block: cranelift_codegen::ir::Block,
    /// K3: shared non-zero-status block (param: status), filled when the loop closes.
    bad: Option<cranelift_codegen::ir::Block>,
    /// K7b: a `try` region (`bad` = its handler), not a recur target.
    is_try: bool,
}

struct Helpers {
    exec_fn: FuncRef,
    nil_fn: FuncRef,
    truthy_fn: FuncRef,
    move_fn: FuncRef,
    fuel_fn: FuncRef,
    call_fn: FuncRef,
}

struct LowerCtx<'a> {
    b: FunctionBuilder<'a>,
    ptr_ty: types::Type,
    ctx_val: CVal,
    slots_ptr: CVal,
    n_slots: u32,
    /// K7b: temps are reused once their node is done; the frame needs the high-water mark.
    hw: u32,
    loops: Vec<LoopFrame>,
    /// K3: shared "return status" block for checks outside any loop frame.
    ret_blk: Option<cranelift_codegen::ir::Block>,
    h: Helpers,
    lh: leaf::LeafRefs,
    n_native: u32,
    n_exec: u32,
    n_call: u32,
}

const VALUE_SIZE: i64 = std::mem::size_of::<crate::value::Value>() as i64;

impl<'a> LowerCtx<'a> {
    fn alloc_temp(&mut self) -> u32 {
        let t = self.n_slots;
        self.n_slots += 1;
        self.hw = self.hw.max(self.n_slots);
        t
    }

    fn slot_addr(&mut self, i: u32) -> CVal {
        self.b.ins().iadd_imm_s(self.slots_ptr, (i as i64) * VALUE_SIZE)
    }

    /// After a helper call returning a status in `{0,1,2}`, leaves `self.b`
    /// positioned at the "ok" continuation block. `1` jumps to the nearest
    /// enclosing native loop's rebind block, or returns `1` (fn-level
    /// `Flow::Recur`) when there is none; `2` returns `2` immediately --
    /// there is no partial state to unwind, `slots` is a plain `Vec`.
    fn check_status(&mut self, status: CVal) {
        // K3: one shared exit block per loop / per fn instead of 4 blocks per call site.
        let ok = self.b.create_block();
        let bad = self.bad_block();
        self.b.ins().brif(status, bad, &[status.into()], ok, &[]);
        self.b.seal_block(ok);
        self.b.switch_to_block(ok);
    }

    fn bad_block(&mut self) -> cranelift_codegen::ir::Block {
        let existing = match self.loops.last() {
            Some(f) => f.bad,
            None => self.ret_blk,
        };
        if let Some(b) = existing {
            return b;
        }
        let b = self.b.create_block();
        self.b.append_block_param(b, types::I32);
        match self.loops.last_mut() {
            Some(f) => f.bad = Some(b),
            None => self.ret_blk = Some(b),
        }
        b
    }

    /// Fills a loop's shared bad block: status 2 returns, 1 (recur) jumps to its rebind.
    fn fill_loop_bad(&mut self, f: &LoopFrame) {
        let Some(bad) = f.bad else { return };
        let cur = self.b.current_block();
        self.b.switch_to_block(bad);
        let st = self.b.block_params(bad)[0];
        let is_err = self.b.ins().icmp_imm_s(IntCC::Equal, st, 2);
        let err_blk = self.b.create_block();
        self.b.ins().brif(is_err, err_blk, &[], f.rebind_block, &[]);
        self.b.seal_block(err_blk);
        self.b.switch_to_block(err_blk);
        let two = self.b.ins().iconst(types::I32, 2);
        if self.loops.iter().any(|f| f.is_try) {
            // K7b: an enclosing native `try` sees the error.
            let outer = self.bad_block();
            self.b.ins().jump(outer, &[two.into()]);
        } else {
            self.b.ins().return_(&[two]);
        }
        self.b.seal_block(bad);
        if let Some(c) = cur {
            self.b.switch_to_block(c);
        }
    }

    fn call_exec(&mut self, node: *const Ir, d: u32) {
        self.n_exec += 1;
        let node_c = self.b.ins().iconst(self.ptr_ty, node as i64);
        let d_c = self.b.ins().iconst(types::I32, d as i64);
        let call = self.b.ins().call(self.h.exec_fn, &[self.ctx_val, node_c, d_c]);
        let status = self.b.inst_results(call)[0];
        self.check_status(status);
    }

    fn call_nil(&mut self, d: u32) {
        let d_c = self.b.ins().iconst(types::I32, d as i64);
        self.b.ins().call(self.h.nil_fn, &[self.ctx_val, d_c]);
    }

    fn call_truthy(&mut self, t: u32) -> CVal {
        let addr = self.slot_addr(t);
        let call = self.b.ins().call(self.h.truthy_fn, &[addr]);
        self.b.inst_results(call)[0]
    }

    fn call_move(&mut self, dst: u32, src: u32) {
        if leaf_on() && super::layout::probe().is_some() {
            // K3: inline raw move (was a `jit_t_move` call per binding per iteration).
            self.lower_load_take(src as u16, dst);
            return;
        }
        let d = self.slot_addr(dst);
        let s = self.slot_addr(src);
        self.b.ins().call(self.h.move_fn, &[d, s]);
    }

    fn call_fuel(&mut self) {
        let call = self.b.ins().call(self.h.fuel_fn, &[self.ctx_val]);
        let status = self.b.inst_results(call)[0];
        self.check_status(status);
    }

    /// G2a (docs/JIT.md): `CallGlobal`/simple-callee `Call` -- `callee`
    /// (when `Some`, an `Ir::Call` whose callee is a `LoadSlot`/
    /// `LoadSlotTake`/`Const`) is lowered into a temp slot FIRST, then each
    /// arg into its own consecutive temp, so `jit_t_call` finds the callee
    /// at `arg_base - 1` and the args at `arg_base..arg_base+argc` -- all in
    /// this same invocation's `slots`, never a separate `Vec` (see
    /// `jit::calls`'s module doc for why that is the whole point).
    fn lower_call(&mut self, node: &Ir, callee: Option<&Ir>, args: &[Ir], d: u32) {
        self.n_call += 1;
        if let Some(c) = callee {
            let cs = self.alloc_temp();
            self.lower_into(c, cs);
        }
        // Every arg's temp is RESERVED before any of them is lowered: an
        // arg that is itself a call (this same fn, recursively) allocates
        // its OWN temps past this range, and if a later sibling's temp were
        // picked only after that happened, it would land past those
        // (non-consecutive with its neighbors) -- breaking `jit_t_call`'s
        // `slots[arg_base..arg_base+argc]` contract silently (no verifier
        // catches it: the slot is real, just the wrong one).
        let arg_base = self.n_slots;
        for _ in args {
            self.alloc_temp();
        }
        let mut mask: u32 = 0;
        for (i, a) in args.iter().enumerate() {
            // K6: a local arg is aliased (no clone) when no later arg can fail or move it.
            if let Ir::LoadSlot(j) = a {
                let rest_safe = args[i + 1..].iter().all(|b| match b {
                    Ir::Const(_) | Ir::LoadSlot(_) => true,
                    Ir::LoadSlotTake(k) => k != j,
                    _ => false,
                });
                if i < 32 && borrow_on() && leaf_on() && rest_safe && self.lower_borrow_arg(*j, arg_base + i as u32) {
                    mask |= 1 << i;
                    continue;
                }
            }
            self.lower_into(a, arg_base + i as u32);
        }
        let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
        let site_c = self.b.ins().iconst(self.ptr_ty, leak_site_addr());
        let base_c = self.b.ins().iconst(types::I32, arg_base as i64);
        let argc_c = self.b.ins().iconst(types::I32, args.len() as i64);
        let d_c = self.b.ins().iconst(types::I32, d as i64);
        let m_c = self.b.ins().iconst(types::I32, mask as i64);
        let call = self.b.ins().call(self.h.call_fn, &[self.ctx_val, site_c, node_c, base_c, argc_c, d_c, m_c]);
        let status = self.b.inst_results(call)[0];
        self.check_status(status);
    }

    /// K3: literal items into consecutive temps (reserved first, like call args), then `jit_t_coll`.
    fn lower_coll(&mut self, kind: u32, items: &[&Ir], d: u32) {
        self.n_native += 1;
        let base = self.n_slots;
        for _ in items {
            self.alloc_temp();
        }
        for (i, it) in items.iter().enumerate() {
            self.lower_into(it, base + i as u32);
        }
        let n = if kind == 0 { items.len() } else { items.len() / 2 };
        let args = [
            self.ctx_val,
            self.b.ins().iconst(types::I32, kind as i64),
            self.b.ins().iconst(types::I32, base as i64),
            self.b.ins().iconst(types::I32, n as i64),
            self.b.ins().iconst(types::I32, d as i64),
        ];
        self.b.ins().call(self.lh.coll, &args);
    }

    /// K5: gate (class into a temp) -> args into consecutive temps -> make; a gate miss runs the Escape.
    fn lower_new(&mut self, node: &Ir, n: &crate::compile::ir::NewInst, d: u32) {
        self.n_native += 1;
        let t = self.alloc_temp();
        let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
        let t_c = self.b.ins().iconst(types::I32, t as i64);
        let call = self.b.ins().call(self.lh.new_chk, &[self.ctx_val, node_c, t_c]);
        let ok = self.b.inst_results(call)[0];
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let merge = self.b.create_block();
        self.b.ins().brif(ok, fast, &[], slow, &[]);
        self.b.seal_block(fast);
        self.b.seal_block(slow);
        self.b.switch_to_block(fast);
        let base = self.n_slots;
        for _ in &n.args {
            self.alloc_temp();
        }
        for (i, a) in n.args.iter().enumerate() {
            self.lower_into(a, base + i as u32);
        }
        let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
        let args = [
            self.ctx_val,
            node_c,
            self.b.ins().iconst(types::I32, t as i64),
            self.b.ins().iconst(types::I32, base as i64),
            self.b.ins().iconst(types::I32, d as i64),
        ];
        let call = self.b.ins().call(self.lh.new_make, &args);
        let st = self.b.inst_results(call)[0];
        self.check_status(st);
        self.b.ins().jump(merge, &[]);
        self.b.switch_to_block(slow);
        self.call_exec(&n.fallback as *const Ir, d);
        self.b.ins().jump(merge, &[]);
        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
    }

    fn dead_block(&mut self) {
        let d = self.b.create_block();
        self.b.seal_block(d);
        self.b.switch_to_block(d);
    }

    fn lower_seq(&mut self, body: &[Ir], d: u32) {
        match body.split_last() {
            None => self.call_nil(d),
            Some((last, init)) => {
                for stmt in init {
                    self.lower_into(stmt, d);
                }
                self.lower_into(last, d);
            }
        }
    }

    fn lower_if(&mut self, test: &Ir, then: &Ir, els: &Option<Box<Ir>>, d: u32) {
        self.n_native += 1;
        // K3: a `LoadSlot` test is read in place (borrowed), no clone.
        let t = match test {
            Ir::LoadSlot(i) if leaf_on() => *i as u32,
            _ => {
                let t = self.alloc_temp();
                self.lower_into(test, t);
                t
            }
        };
        let truthy = match leaf_on().then(|| self.truthy_inline(t)).flatten() {
            Some(v) => v,
            None => self.call_truthy(t),
        };
        let then_blk = self.b.create_block();
        let else_blk = self.b.create_block();
        let merge = self.b.create_block();
        self.b.ins().brif(truthy, then_blk, &[], else_blk, &[]);
        self.b.seal_block(then_blk);
        self.b.seal_block(else_blk);

        self.b.switch_to_block(then_blk);
        self.lower_into(then, d);
        self.b.ins().jump(merge, &[]);

        self.b.switch_to_block(else_blk);
        match els {
            Some(e) => self.lower_into(e, d),
            None => self.call_nil(d),
        }
        self.b.ins().jump(merge, &[]);

        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
    }

    fn lower_loop(&mut self, node: &Ir, binds: &[(CompiledPattern, Ir)], scratch_base: u16, body: &[Ir], d: u32) {
        self.n_native += 1;
        for (bi, (pat, init)) in binds.iter().enumerate() {
            if plain(pat) {
                self.lower_into(init, plain_slot(pat));
            } else {
                // K7b: destructuring loop bind via `jit_t_bind` (the init's `recur` escapes, as in `exec_loop`).
                let save = self.n_slots;
                let t = self.alloc_temp();
                self.lower_into(init, t);
                self.call_bind(node, bi, t);
                self.n_slots = save;
            }
        }
        let header = self.b.create_block();
        let rebind = self.b.create_block();
        let exit = self.b.create_block();
        self.b.ins().jump(header, &[]);
        self.b.switch_to_block(header);

        self.loops.push(LoopFrame { scratch_base, rebind_block: rebind, bad: None, is_try: false });
        self.lower_seq(body, d);
        let f = self.loops.pop().expect("pushed above");
        self.fill_loop_bad(&f);
        // Normal (non-recur) completion: `d` holds the loop's value, exactly
        // like `exec_loop` returning `Flow::Val` -- jump to `exit` so this
        // (now-dangling) block gets a terminator, and the caller's own
        // continuation lands in a real, still-open block.
        self.b.ins().jump(exit, &[]);

        self.b.switch_to_block(rebind);
        self.call_fuel();
        for (i, (pat, _)) in binds.iter().enumerate() {
            if plain(pat) {
                self.call_move(plain_slot(pat), scratch_base as u32 + i as u32);
            } else {
                self.call_bind(node, i, scratch_base as u32 + i as u32);
            }
        }
        self.b.ins().jump(header, &[]);
        self.b.seal_block(header);
        self.b.seal_block(rebind);

        self.b.seal_block(exit);
        self.b.switch_to_block(exit);
    }

    /// K3/K7b: destructure `slots[t]` (moved out) through bind `bi` of Let/Loop `node` (image-relocatable).
    fn call_bind(&mut self, node: &Ir, bi: usize, t: u32) {
        let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
        let bi_c = self.b.ins().iconst(types::I32, bi as i64);
        let t_c = self.b.ins().iconst(types::I32, t as i64);
        let call = self.b.ins().call(self.lh.bind, &[self.ctx_val, node_c, bi_c, t_c]);
        let st = self.b.inst_results(call)[0];
        self.check_status(st);
    }

    /// K7b: `try` without `finally`: body native under a handler frame; status 2 -> `jit_t_catch`
    /// picks the arm (slot written) or re-raises; status 1 (recur) passes through, as in `exec_try`.
    fn lower_try(&mut self, node: &Ir, body: &[Ir], catches: &[crate::compile::ir::CatchArm], d: u32) {
        self.n_native += 1;
        let handler = self.b.create_block();
        self.b.append_block_param(handler, types::I32);
        let merge = self.b.create_block();
        self.loops.push(LoopFrame { scratch_base: 0, rebind_block: handler, bad: Some(handler), is_try: true });
        self.lower_seq(body, d);
        self.loops.pop();
        self.b.ins().jump(merge, &[]);
        self.b.seal_block(handler);
        self.b.switch_to_block(handler);
        let st = self.b.block_params(handler)[0];
        let is_err = self.b.ins().icmp_imm_s(IntCC::Equal, st, 2);
        let catch_blk = self.b.create_block();
        let fwd = self.b.create_block();
        self.b.ins().brif(is_err, catch_blk, &[], fwd, &[]);
        self.b.seal_block(catch_blk);
        self.b.seal_block(fwd);
        self.b.switch_to_block(fwd);
        let outer = self.bad_block();
        self.b.ins().jump(outer, &[st.into()]);
        self.b.switch_to_block(catch_blk);
        let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
        let call = self.b.ins().call(self.lh.catch, &[self.ctx_val, node_c]);
        let r = self.b.inst_results(call)[0];
        for (k, arm) in catches.iter().enumerate() {
            let arm_blk = self.b.create_block();
            let next = self.b.create_block();
            let hit = self.b.ins().icmp_imm_s(IntCC::Equal, r, k as i64 + 3);
            self.b.ins().brif(hit, arm_blk, &[], next, &[]);
            self.b.seal_block(arm_blk);
            self.b.seal_block(next);
            self.b.switch_to_block(arm_blk);
            self.lower_seq(&arm.body, d);
            self.b.ins().jump(merge, &[]);
            self.b.switch_to_block(next);
        }
        // No arm matched (or recur/fuel error): re-raise outward.
        let outer = self.bad_block();
        let two = self.b.ins().iconst(types::I32, 2);
        self.b.ins().jump(outer, &[two.into()]);
        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
    }

    fn lower_recur(&mut self, args: &[Ir], scratch_base: u16) {
        self.n_native += 1;
        for (j, arg) in args.iter().enumerate() {
            self.lower_into(arg, scratch_base as u32 + j as u32);
        }
        match self.loops.iter().rposition(|f| !f.is_try && f.scratch_base == scratch_base) {
            Some(pos) => {
                let t = self.loops[pos].rebind_block;
                self.b.ins().jump(t, &[]);
            }
            None => {
                let one = self.b.ins().iconst(types::I32, 1);
                self.b.ins().return_(&[one]);
            }
        }
        self.dead_block();
    }

    fn lower_into(&mut self, node: &Ir, d: u32) {
        // K7b: a node's temps are dead once its value is in `d` (every slot write drops the old value).
        let save = self.n_slots;
        self.lower_node(node, d);
        self.n_slots = save;
    }

    fn lower_node(&mut self, node: &Ir, d: u32) {
        match node {
            Ir::Do(body) => self.lower_seq(body, d),
            Ir::If { test, then, els } => self.lower_if(test, then, els, d),
            Ir::Let { binds, body } if leaf_on() || binds.iter().all(|(p, _)| plain(p)) => {
                self.n_native += 1;
                for (bi, (pat, init)) in binds.iter().enumerate() {
                    if plain(pat) {
                        self.lower_into(init, plain_slot(pat));
                    } else {
                        // K3: destructuring bind via `exec_pattern`; body stays native.
                        let t = self.alloc_temp();
                        self.lower_into(init, t);
                        self.call_bind(node, bi, t);
                    }
                }
                self.lower_seq(body, d);
            }
            Ir::Loop { binds, scratch_base, body } if leaf_on() || binds.iter().all(|(p, _)| plain(p)) => {
                self.lower_loop(node, binds, *scratch_base, body, d)
            }
            Ir::Recur { args, scratch_base } => self.lower_recur(args, *scratch_base),
            Ir::Try { body, catches, finally: None } if leaf_on() && try_on() => self.lower_try(node, body, catches, d),
            Ir::VectorLit(items) if leaf_on() => {
                let items: Vec<&Ir> = items.iter().collect();
                self.lower_coll(0, &items, d)
            }
            Ir::MapLit(pairs) if leaf_on() && !pairs.is_empty() => {
                let items: Vec<&Ir> = pairs.iter().flat_map(|(k, v)| [k, v]).collect();
                self.lower_coll(1, &items, d)
            }
            Ir::New(n) if leaf_on() => self.lower_new(node, n, d),
            Ir::MakeClosure { .. } if leaf_on() => self.lower_mk_fn(node, d),
            Ir::CallGlobal { args, .. } => self.lower_call(node, None, args, d),
            Ir::Call { callee, args, .. } if leaf_on() && args.len() == 1 && matches!(callee.as_ref(), Ir::Const(Value::Keyword(_))) => {
                let Ir::Const(kw) = callee.as_ref() else { unreachable!() };
                self.lower_kw_call(node, kw, &args[0], d)
            }
            Ir::Call { callee, args, .. }
                if matches!(callee.as_ref(), Ir::LoadSlot(_) | Ir::LoadSlotTake(_) | Ir::Const(_))
                    || (leaf_on() && matches!(callee.as_ref(), Ir::LoadCapture(_) | Ir::SelfRef)) =>
            {
                self.lower_call(node, Some(callee.as_ref()), args, d)
            }
            Ir::Const(v) if leaf_on() => self.lower_const(v, d),
            Ir::LoadSlot(i) if leaf_on() => self.lower_load_slot(*i, d),
            Ir::LoadSlotTake(i) if leaf_on() => self.lower_load_take(*i, d),
            Ir::LoadCapture(i) if leaf_on() => self.lower_load_cap(*i, d),
            Ir::SelfRef if leaf_on() => self.lower_self_ref(d),
            Ir::GlobalRef { .. } if leaf_on() => self.lower_global(node, d),
            Ir::Intrinsic { op, chain, args, .. } if leaf_on() => self.lower_intrinsic(node, *op, chain, args, d),
            other => self.call_exec(other as *const Ir, d),
        }
    }
}

/// K7b: `MOVA_JIT_TRY=0` keeps `try` on the exec fallback.
fn try_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("MOVA_JIT_TRY").as_deref() != Ok("0"))
}

fn plain(p: &CompiledPattern) -> bool {
    matches!(p, CompiledPattern::Slot(_))
}
fn plain_slot(p: &CompiledPattern) -> u32 {
    match p {
        CompiledPattern::Slot(i) => *i as u32,
        _ => unreachable!("plain() already filtered non-Slot patterns"),
    }
}

fn declare_helper(m: &mut JITModule, name: &str, params: &[types::Type], ret: Option<types::Type>) -> FuncId {
    let mut sig = Signature::new(CallConv::triple_default(m.isa().triple()));
    for p in params {
        sig.params.push(AbiParam::new(*p));
    }
    if let Some(r) = ret {
        sig.returns.push(AbiParam::new(r));
    }
    m.declare_function(name, Linkage::Import, &sig)
        .expect("threaded tier: declaring a helper symbol cannot fail")
}

/// Lowers arity `idx` of `code`, or returns `None` on a genuine Cranelift
/// error (there is no "unsupported shape" failure: every node not natively
/// structured falls back to `jit_t_exec`).
pub(super) fn lower_arity(code: &Arc<CompiledFn>, idx: usize, native_recur: bool) -> Option<(ThreadedEntry, u32)> {
    lower_arity_x(code, idx, native_recur, None)
}

/// S3: lowers again with every pointer-range constant as a relocation and
/// returns the relocatable code record (`jit::aot`), or `None` if any
/// constant cannot be re-derived in another process.
pub(super) fn lower_for_image(code: &Arc<CompiledFn>, idx: usize, native_recur: bool) -> Option<Vec<u8>> {
    let mut out = None;
    lower_arity_x(code, idx, native_recur, Some(&mut out))?;
    if out.is_none() && explain_enabled() {
        eprintln!("threaded: {} arity {idx} image-code: not relocatable", code.name.as_deref().unwrap_or("<anonymous>"));
    }
    out
}

fn lower_arity_x(
    code: &Arc<CompiledFn>,
    idx: usize,
    native_recur: bool,
    mut img: Option<&mut Option<Vec<u8>>>,
) -> Option<(ThreadedEntry, u32)> {
    let start = Instant::now();
    if img.is_some() {
        super::aot::begin();
    }
    let arity = &code.arities[idx];
    let name = code.name.as_deref().unwrap_or("<anonymous>");
    let mut m = module().lock().expect("threaded tier module mutex poisoned");
    let target_config = m.target_config();
    let ptr_ty = target_config.pointer_type();

    let exec_id = declare_helper(&mut m, "jit_t_exec", &[ptr_ty, ptr_ty, types::I32], Some(types::I32));
    let nil_id = declare_helper(&mut m, "jit_t_nil", &[ptr_ty, types::I32], None);
    let truthy_id = declare_helper(&mut m, "jit_t_truthy", &[ptr_ty], Some(types::I8));
    let move_id = declare_helper(&mut m, "jit_t_move", &[ptr_ty, ptr_ty], None);
    let fuel_id = declare_helper(&mut m, "jit_t_tick_fuel", &[ptr_ty], Some(types::I32));
    let call_id = declare_helper(
        &mut m,
        "jit_t_call",
        &[ptr_ty, ptr_ty, ptr_ty, types::I32, types::I32, types::I32, types::I32],
        Some(types::I32),
    );

    let leaf_ids = leaf::declare(&mut m, ptr_ty);
    let mut sig = Signature::new(CallConv::triple_default(m.isa().triple()));
    sig.params.push(AbiParam::new(ptr_ty));
    sig.returns.push(AbiParam::new(types::I32));
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let fname = format!("mova_threaded_{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let func_id = match m.declare_function(&fname, Linkage::Export, &sig) {
        Ok(id) => id,
        Err(e) => {
            if explain_enabled() {
                eprintln!("threaded: {name} arity {idx} error: {e}");
            }
            return None;
        }
    };

    let mut fctx = m.make_context();
    fctx.func.signature = sig;
    let mut fbctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut fctx.func, &mut fbctx);
    let entry_block = b.create_block();
    b.append_block_params_for_function_params(entry_block);
    b.switch_to_block(entry_block);
    b.seal_block(entry_block);
    let ctx_val = b.block_params(entry_block)[0];
    let slots_off = std::mem::offset_of!(super::TCtx, slots) as i32;
    let slots_ptr = b.ins().load(ptr_ty, MemFlagsData::trusted(), ctx_val, slots_off);

    let h = Helpers {
        exec_fn: m.declare_func_in_func(exec_id, b.func),
        nil_fn: m.declare_func_in_func(nil_id, b.func),
        truthy_fn: m.declare_func_in_func(truthy_id, b.func),
        move_fn: m.declare_func_in_func(move_id, b.func),
        fuel_fn: m.declare_func_in_func(fuel_id, b.func),
        call_fn: m.declare_func_in_func(call_id, b.func),
    };
    let lh = leaf::refs(&mut m, &leaf_ids, b.func);

    let mut lc = LowerCtx {
        b,
        ptr_ty,
        ctx_val,
        slots_ptr,
        n_slots: arity.n_slots as u32,
        hw: arity.n_slots as u32,
        loops: Vec::new(),
        ret_blk: None,
        h,
        lh,
        n_native: 0,
        n_exec: 0,
        n_call: 0,
    };
    // K1: the fn body is a loop -- a fn-level `recur` (status 1 included)
    // jumps to `fn_rebind` (fuel tick, move scratch -> params), never out.
    let fn_header = lc.b.create_block();
    let fn_rebind = lc.b.create_block();
    lc.b.ins().jump(fn_header, &[]);
    lc.b.switch_to_block(fn_header);
    if native_recur {
        lc.loops.push(LoopFrame { scratch_base: arity.scratch_base, rebind_block: fn_rebind, bad: None, is_try: false });
    }
    lc.lower_seq(&arity.body, THREADED_OUT);
    let zero = lc.b.ins().iconst(types::I32, 0);
    lc.b.ins().return_(&[zero]);
    if let Some(f) = lc.loops.pop() {
        lc.fill_loop_bad(&f);
    }
    lc.loops.clear();
    lc.b.switch_to_block(fn_rebind);
    if native_recur {
        lc.call_fuel();
        for i in 0..arity.n_recur as u32 {
            lc.call_move(i, arity.scratch_base as u32 + i);
        }
        lc.b.ins().jump(fn_header, &[]);
    } else {
        let one = lc.b.ins().iconst(types::I32, 1);
        lc.b.ins().return_(&[one]);
    }
    lc.b.seal_block(fn_header);
    lc.b.seal_block(fn_rebind);
    if let Some(r) = lc.ret_blk {
        lc.b.switch_to_block(r);
        let st = lc.b.block_params(r)[0];
        lc.b.ins().return_(&[st]);
        lc.b.seal_block(r);
    }

    let LowerCtx { b, hw: n_slots, n_native, n_exec, n_call, .. } = lc;
    b.finalize(target_config);
    // S3: an entry that is only `jit_t_exec` calls is the interpreter plus a
    // hop -- skip the Cranelift compile (keeps cold first calls cheap).
    if n_native == 0 && n_call == 0 {
        super::aot::end();
        return None;
    }
    let fresh = if img.is_some() { super::aot::end() } else { Vec::new() };
    if img.is_some() {
        super::aot::relocatable_consts(&mut *m, &mut fctx.func);
    }

    let result = (|| -> Result<*const u8, String> {
        m.define_function(func_id, &mut fctx).map_err(|e| e.to_string())?;
        if let Some(out) = img.as_deref_mut() {
            let extra = n_slots - arity.n_slots as u32;
            *out = super::aot::encode(&*m, &fctx, &arity.body, &fresh, extra, native_recur);
        }
        m.clear_context(&mut fctx);
        m.finalize_definitions().map_err(|e| e.to_string())?;
        Ok(m.get_finalized_function(func_id))
    })();

    let elapsed = start.elapsed().as_nanos() as u64;
    THREADED_LOWER_NS.fetch_add(elapsed, Ordering::Relaxed);
    let count = THREADED_LOWER_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    match result {
        Ok(code_ptr) => {
            if explain_enabled() {
                let extra = n_slots - arity.n_slots as u32;
                let total = THREADED_LOWER_NS.load(Ordering::Relaxed);
                eprintln!(
                    "threaded: {name} arity {idx} lowered ({n_native} native nodes, {n_exec} exec nodes, {n_call} light calls, {extra} temps); compile-cost total={total}ns arities={count}"
                );
            }
            let entry: extern "C" fn(*mut super::TCtx) -> u32 = unsafe { std::mem::transmute(code_ptr) };
            Some((ThreadedEntry(entry), n_slots - arity.n_slots as u32))
        }
        Err(e) => {
            if explain_enabled() {
                eprintln!("threaded: {name} arity {idx} error: {e}");
            }
            None
        }
    }
}
