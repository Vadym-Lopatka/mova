//! Heap image: compiled-tier IR codec. A closure whose `CompileSlot` holds IR
//! at write time comes back already compiled (no re-expansion, no re-lowering).
//! Runtime caches (field ICs, JIT/threaded slots, lens sites) restart empty.

use std::sync::Arc;

use super::{R, W};
use crate::compile::ir::*;
use crate::compile::{CompiledArity, CompiledClosure, CompiledFn};
use crate::env::VarCell;
use crate::reader::Span;
use crate::value::{Symbol, Value};

const INTRIN: [IntrinOp; 13] = [
    IntrinOp::Add,
    IntrinOp::Sub2,
    IntrinOp::Mul,
    IntrinOp::Div2,
    IntrinOp::Inc,
    IntrinOp::Dec,
    IntrinOp::Lt2,
    IntrinOp::Le2,
    IntrinOp::Gt2,
    IntrinOp::Ge2,
    IntrinOp::Eq2,
    IntrinOp::Zero,
    IntrinOp::Not,
];

/// `MOVA_IMAGE_NO_IR=1`: write every fn as pending (A/B switch).
pub(super) fn disabled() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var("MOVA_IMAGE_NO_IR").is_ok_and(|v| v == "1"))
}

/// True iff `cc` can be written: plain top-level instance (no captures, no
/// rec group) and no IR node the codec skips (NumLoop).
pub(super) fn persistable(cc: &CompiledClosure) -> bool {
    cc.captures.is_empty() && cc.group.is_none() && fn_ok(&cc.code)
}

/// S3: a boot (pre-index) fn whose IR + threaded code go in a FIX record.
pub(super) fn pre_fn_with_code(v: &Value) -> bool {
    let Value::Fn(c) = v else { return false };
    crate::jit::enabled()
        && !disabled()
        && c.compiled.compiled().is_some_and(|cc| persistable(cc) && cc.code.arities.iter().any(|a| a.threaded.has_entry()))
}

fn fn_ok(f: &CompiledFn) -> bool {
    f.arities.iter().all(|a| a.body.iter().all(ir_ok))
}

fn pat_ok(p: &CompiledPattern) -> bool {
    match p {
        CompiledPattern::Slot(_) => true,
        CompiledPattern::Seq(s) => s.iter().all(|x| match x {
            SeqStep::Elem(p) | SeqStep::Rest(p) | SeqStep::As(p) => pat_ok(p),
        }),
        CompiledPattern::Map(m) => {
            m.as_pat.as_ref().is_none_or(pat_ok)
                && m.entries.iter().all(|e| pat_ok(&e.target) && e.default.as_ref().is_none_or(ir_ok))
        }
    }
}

fn ir_ok(x: &Ir) -> bool {
    let all = |v: &[Ir]| v.iter().all(ir_ok);
    match x {
        Ir::NumLoop(_) => false,
        Ir::Const(_) | Ir::LoadSlot(_) | Ir::LoadSlotTake(_) | Ir::LoadCapture(_) | Ir::SelfRef => true,
        Ir::GlobalRef { .. } | Ir::CreationEnvLookup { .. } | Ir::SiblingRef(_) | Ir::Escape(_) => true,
        Ir::SetMutField { value, .. } => ir_ok(value),
        Ir::If { test, then, els } => ir_ok(test) && ir_ok(then) && els.as_deref().is_none_or(ir_ok),
        Ir::Do(v) | Ir::VectorLit(v) | Ir::SetLit(v) => all(v),
        Ir::Let { binds, body } | Ir::Loop { binds, body, .. } => binds.iter().all(|(p, i)| pat_ok(p) && ir_ok(i)) && all(body),
        Ir::Recur { args, .. } => all(args),
        Ir::Call { callee, args, .. } => ir_ok(callee) && all(args),
        Ir::CallGlobal { args, .. } | Ir::CallCreationEnv { args, .. } | Ir::Intrinsic { args, .. } => all(args),
        Ir::MapLit(v) => v.iter().all(|(a, b)| ir_ok(a) && ir_ok(b)),
        Ir::Throw { value, .. } => ir_ok(value),
        Ir::MakeClosure { template, .. } => fn_ok(&template.code),
        Ir::MakeRecGroup { members, .. } => members.iter().all(|m| fn_ok(&m.template.code)),
        Ir::Try { body, catches, finally } => all(body) && catches.iter().all(|c| all(&c.body)) && finally.as_deref().is_none_or(all),
        Ir::DynBind(d) => d.pairs.iter().all(|(_, _, i)| ir_ok(i)) && all(&d.body),
        Ir::Def { value, .. } => value.as_deref().is_none_or(ir_ok),
        Ir::FieldGet(g) => ir_ok(&g.fallback),
        Ir::New(n) => n.args.iter().all(ir_ok) && ir_ok(&n.fallback),
    }
}

impl<'a> W<'a> {
    /// A value in IR: collected (lazy-IR prelude pass) or written.
    fn ival(&mut self, v: &Value) {
        match &mut self.collect {
            Some(c) => c.push(super::Pre::V(v.clone())),
            None => self.val(v),
        }
    }
    fn iarities(&mut self, a: &Arc<Vec<crate::value::Arity>>) {
        match &mut self.collect {
            Some(c) => c.push(super::Pre::Ar(a.clone())),
            None => self.arities(a),
        }
    }
    fn span(&mut self, s: &Span) {
        self.u(s.start as u64);
        self.u(s.end as u64);
    }
    fn cell(&mut self, c: &Arc<VarCell>) {
        self.ival(&Value::Var(c.clone()))
    }
    fn chain(&mut self, c: &GlobalChain) {
        match c {
            GlobalChain::One(a) => {
                self.u(1);
                self.cell(a)
            }
            GlobalChain::Two(a, b) => {
                self.u(2);
                self.cell(a);
                self.cell(b)
            }
            GlobalChain::Many(cs) => {
                self.u(cs.len() as u64);
                cs.iter().for_each(|x| self.cell(x))
            }
        }
    }
    fn cap(&mut self, c: &CaptureSrc) {
        let (t, n) = match *c {
            CaptureSrc::Slot(n) => (0, n),
            CaptureSrc::Capture(n) => (1, n),
            CaptureSrc::SelfRef => (2, 0),
            CaptureSrc::Sibling(n) => (3, n),
        };
        self.b(t);
        self.u(n as u64)
    }
    fn caps(&mut self, v: &[CaptureSrc]) {
        self.u(v.len() as u64);
        v.iter().for_each(|c| self.cap(c))
    }
    fn irs(&mut self, v: &[Ir]) {
        self.u(v.len() as u64);
        v.iter().for_each(|x| self.ir(x))
    }
    fn opt_ir(&mut self, x: Option<&Ir>) {
        match x {
            Some(x) => {
                self.b(1);
                self.ir(x)
            }
            None => self.b(0),
        }
    }
    fn opt_sym(&mut self, s: &Option<Symbol>) {
        match s {
            Some(s) => {
                self.b(1);
                self.sym(s)
            }
            None => self.b(0),
        }
    }
    fn pat(&mut self, p: &CompiledPattern) {
        match p {
            CompiledPattern::Slot(n) => {
                self.b(0);
                self.u(*n as u64)
            }
            CompiledPattern::Seq(s) => {
                self.b(1);
                self.u(s.len() as u64);
                for x in s {
                    let (t, p) = match x {
                        SeqStep::Elem(p) => (0, p),
                        SeqStep::Rest(p) => (1, p),
                        SeqStep::As(p) => (2, p),
                    };
                    self.b(t);
                    self.pat(p)
                }
            }
            CompiledPattern::Map(m) => {
                self.b(2);
                self.u(m.entries.len() as u64);
                for e in &m.entries {
                    self.ival(&e.key);
                    self.pat(&e.target);
                    self.opt_ir(e.default.as_ref());
                    self.b(e.required as u8);
                    self.span(&e.span)
                }
                match &m.as_pat {
                    Some(p) => {
                        self.b(1);
                        self.pat(p)
                    }
                    None => self.b(0),
                }
            }
        }
    }
    fn binds(&mut self, b: &[(CompiledPattern, Ir)]) {
        self.u(b.len() as u64);
        for (p, i) in b {
            self.pat(p);
            self.ir(i)
        }
    }
    /// Shared `Arc<CompiledFn>`: 0 = new (id = next), 1 = ref.
    pub(super) fn cfn(&mut self, f: &Arc<CompiledFn>) {
        let p = Arc::as_ptr(f) as usize;
        if let Some(&id) = self.cfn_seen.get(&p) {
            self.b(1);
            return self.u(id as u64);
        }
        self.b(0);
        match &f.name {
            Some(n) => {
                self.b(1);
                self.str_(n)
            }
            None => self.b(0),
        }
        self.u(f.capture_syms.len() as u64);
        f.capture_syms.iter().for_each(|s| self.sym(s));
        self.u(f.arities.len() as u64);
        for (i, a) in f.arities.iter().enumerate() {
            self.u(a.n_params as u64);
            self.b(a.variadic as u8);
            self.u(a.n_recur as u64);
            self.u(a.scratch_base as u64);
            self.u(a.n_slots as u64);
            self.irs(&a.body);
            // S3: relocatable threaded-tier code for arities hot at write time
            match crate::jit::enabled().then(|| a.threaded.image_code(f, i)).flatten() {
                Some(c) => {
                    *self.census.entry("arity:native-code").or_default() += 1;
                    *self.census.entry("arity:native-code-bytes").or_default() += c.len() as u64;
                    self.b(1);
                    self.u(self.code.len() as u64);
                    self.u(c.len() as u64);
                    self.code.extend_from_slice(&c)
                }
                None => self.b(0),
            }
        }
        let id = self.cfn_seen.len() as u32;
        self.cfn_seen.insert(p, id);
    }
    fn tpl(&mut self, t: &Arc<FnTemplate>) {
        let p = Arc::as_ptr(t) as usize;
        if let Some(&id) = self.tpl_seen.get(&p) {
            self.b(1);
            return self.u(id as u64);
        }
        self.b(0);
        self.cfn(&t.code);
        self.iarities(&t.arities);
        let id = self.tpl_seen.len() as u32;
        self.tpl_seen.insert(p, id);
    }
    fn ir(&mut self, x: &Ir) {
        match x {
            Ir::Const(v) => {
                self.b(0);
                self.ival(v)
            }
            Ir::LoadSlot(n) => {
                self.b(1);
                self.u(*n as u64)
            }
            Ir::LoadSlotTake(n) => {
                self.b(2);
                self.u(*n as u64)
            }
            Ir::LoadCapture(n) => {
                self.b(3);
                self.u(*n as u64)
            }
            Ir::SelfRef => self.b(4),
            Ir::GlobalRef { chain, sym, span } => {
                self.b(5);
                self.chain(chain);
                self.sym(sym);
                self.span(span)
            }
            Ir::CreationEnvLookup { sym, chain, span } => {
                self.b(6);
                self.chain(chain);
                self.sym(sym);
                self.span(span)
            }
            Ir::SetMutField { owner_slot, field_slot, field_name, ic: _, value, span } => {
                self.b(7);
                self.u(*owner_slot as u64);
                self.u(*field_slot as u64);
                self.str_(field_name);
                self.ir(value);
                self.span(span)
            }
            Ir::If { test, then, els } => {
                self.b(8);
                self.ir(test);
                self.ir(then);
                self.opt_ir(els.as_deref())
            }
            Ir::Do(v) => {
                self.b(9);
                self.irs(v)
            }
            Ir::Let { binds, body } => {
                self.b(10);
                self.binds(binds);
                self.irs(body)
            }
            Ir::Loop { binds, scratch_base, body } => {
                self.b(11);
                self.binds(binds);
                self.u(*scratch_base as u64);
                self.irs(body)
            }
            Ir::Recur { args, scratch_base } => {
                self.b(12);
                self.irs(args);
                self.u(*scratch_base as u64)
            }
            Ir::NumLoop(_) => unreachable!("persistable() rejects NumLoop"),
            Ir::Call { callee, args, span } => {
                self.b(13);
                self.ir(callee);
                self.irs(args);
                self.span(span)
            }
            Ir::CallGlobal { chain, sym, sym_span, args, span } => {
                self.b(14);
                self.chain(chain);
                self.sym(sym);
                self.span(sym_span);
                self.irs(args);
                self.span(span)
            }
            Ir::CallCreationEnv { sym, chain, sym_span, args, span } => {
                self.b(15);
                self.chain(chain);
                self.sym(sym);
                self.span(sym_span);
                self.irs(args);
                self.span(span)
            }
            Ir::Intrinsic { op, chain, sym, sym_span, args, span } => {
                self.b(16);
                self.b(INTRIN.iter().position(|o| *o as u8 == *op as u8).unwrap() as u8);
                self.chain(chain);
                self.sym(sym);
                self.span(sym_span);
                self.irs(args);
                self.span(span)
            }
            Ir::VectorLit(v) => {
                self.b(17);
                self.irs(v)
            }
            Ir::MapLit(v) => {
                self.b(18);
                self.u(v.len() as u64);
                for (a, b) in v {
                    self.ir(a);
                    self.ir(b)
                }
            }
            Ir::SetLit(v) => {
                self.b(19);
                self.irs(v)
            }
            Ir::Throw { value, span } => {
                self.b(20);
                self.ir(value);
                self.span(span)
            }
            Ir::MakeClosure { template, caps } => {
                self.b(21);
                self.tpl(template);
                self.caps(caps)
            }
            Ir::MakeRecGroup { members, slots } => {
                self.b(22);
                self.u(members.len() as u64);
                for m in members {
                    self.tpl(&m.template);
                    self.caps(&m.caps)
                }
                self.u(slots.len() as u64);
                slots.iter().for_each(|s| self.u(*s as u64))
            }
            Ir::SiblingRef(n) => {
                self.b(23);
                self.u(*n as u64)
            }
            Ir::Try { body, catches, finally } => {
                self.b(24);
                self.irs(body);
                self.u(catches.len() as u64);
                for c in catches {
                    self.opt_sym(&c.class);
                    self.u(c.slot as u64);
                    self.irs(&c.body)
                }
                match finally {
                    Some(f) => {
                        self.b(1);
                        self.irs(f)
                    }
                    None => self.b(0),
                }
            }
            Ir::DynBind(d) => {
                self.b(25);
                self.b(d.redefs as u8);
                self.u(d.pairs.len() as u64);
                for (s, sp, i) in &d.pairs {
                    self.sym(s);
                    self.span(sp);
                    self.ir(i)
                }
                self.irs(&d.body);
                self.span(&d.span)
            }
            Ir::Def { cell, value } => {
                self.b(26);
                self.cell(cell);
                self.opt_ir(value.as_deref())
            }
            Ir::Escape(e) => {
                self.b(27);
                self.form(&e.form);
                self.u(e.binds.len() as u64);
                for (s, c) in &e.binds {
                    self.sym(s);
                    self.cap(c)
                }
                self.span(&e.span)
            }
            Ir::FieldGet(g) => {
                self.b(28);
                self.str_(&g.field);
                self.ival(&g.kw);
                let (t, n) = match g.recv {
                    FieldRecv::Slot(n) => (0, n),
                    FieldRecv::Capture(n) => (1, n),
                };
                self.b(t);
                self.u(n as u64);
                self.ir(&g.fallback)
            }
            Ir::New(n) => {
                self.b(29);
                self.form(&n.class);
                self.irs(&n.args);
                self.ir(&n.fallback);
                self.span(&n.span)
            }
        }
    }
}

impl<'a> R<'a> {
    fn span(&mut self) -> Span {
        Span { start: self.u() as usize, end: self.u() as usize }
    }
    fn cell(&mut self) -> Arc<VarCell> {
        match self.val() {
            Value::Var(c) => c,
            _ => panic!("image: IR var ref is not a var"),
        }
    }
    fn chain(&mut self) -> GlobalChain {
        let n = self.u() as usize;
        GlobalChain::new((0..n).map(|_| self.cell()).collect())
    }
    fn cap(&mut self) -> CaptureSrc {
        let t = self.byte();
        let n = self.u() as u16;
        match t {
            0 => CaptureSrc::Slot(n),
            1 => CaptureSrc::Capture(n),
            2 => CaptureSrc::SelfRef,
            _ => CaptureSrc::Sibling(n),
        }
    }
    fn caps(&mut self) -> Vec<CaptureSrc> {
        let n = self.u() as usize;
        (0..n).map(|_| self.cap()).collect()
    }
    fn irs(&mut self) -> Vec<Ir> {
        let n = self.u() as usize;
        (0..n).map(|_| self.ir()).collect()
    }
    fn opt_ir(&mut self) -> Option<Ir> {
        (self.byte() == 1).then(|| self.ir())
    }
    fn pat(&mut self) -> CompiledPattern {
        match self.byte() {
            0 => CompiledPattern::Slot(self.u() as u16),
            1 => {
                let n = self.u() as usize;
                CompiledPattern::Seq(
                    (0..n)
                        .map(|_| match self.byte() {
                            0 => SeqStep::Elem(self.pat()),
                            1 => SeqStep::Rest(self.pat()),
                            _ => SeqStep::As(self.pat()),
                        })
                        .collect(),
                )
            }
            _ => {
                let n = self.u() as usize;
                let entries = (0..n)
                    .map(|_| MapEntry {
                        key: self.val(),
                        target: self.pat(),
                        default: self.opt_ir(),
                        required: self.byte() == 1,
                        span: self.span(),
                    })
                    .collect();
                let as_pat = (self.byte() == 1).then(|| self.pat());
                CompiledPattern::Map(Box::new(MapPattern { entries, as_pat }))
            }
        }
    }
    fn binds(&mut self) -> Vec<(CompiledPattern, Ir)> {
        let n = self.u() as usize;
        (0..n).map(|_| (self.pat(), self.ir())).collect()
    }
    pub(super) fn cfn(&mut self) -> Arc<CompiledFn> {
        if self.byte() == 1 {
            let id = self.u() as usize;
            return self.cfns[id].clone();
        }
        let name = (self.byte() == 1).then(|| self.str_());
        let n = self.u() as usize;
        let capture_syms = (0..n).map(|_| self.sym()).collect();
        let n = self.u() as usize;
        let arities = (0..n)
            .map(|_| {
                let a = CompiledArity {
                    n_params: self.u() as usize,
                    variadic: self.byte() == 1,
                    n_recur: self.u() as usize,
                    scratch_base: self.u() as u16,
                    n_slots: self.u() as usize,
                    body: self.irs(),
                    jit: Default::default(),
                    threaded: Default::default(),
                };
                if self.byte() == 1 {
                    let (off, len) = (self.u() as usize, self.u() as usize);
                    if let Some((f, base)) = self.code.as_ref() {
                        a.threaded.attach_image_code(crate::jit::AotBlob { file: f.clone(), off: (base + off) as u64, len });
                    }
                }
                a
            })
            .collect();
        let f = Arc::new(CompiledFn { name, arities, capture_syms });
        self.cfns.push(f.clone());
        f
    }
    fn tpl(&mut self) -> Arc<FnTemplate> {
        if self.byte() == 1 {
            let id = self.u() as usize;
            return self.tpls[id].clone();
        }
        let code = self.cfn();
        let arities = self.arities();
        let t = Arc::new(FnTemplate { code, arities });
        self.tpls.push(t.clone());
        t
    }
    fn sym_span_args(&mut self) -> (Symbol, Span, Vec<Ir>, Span) {
        (self.sym(), self.span(), self.irs(), self.span())
    }
    fn ir(&mut self) -> Ir {
        match self.byte() {
            0 => Ir::Const(self.val()),
            1 => Ir::LoadSlot(self.u() as u16),
            2 => Ir::LoadSlotTake(self.u() as u16),
            3 => Ir::LoadCapture(self.u() as u16),
            4 => Ir::SelfRef,
            5 => Ir::GlobalRef { chain: self.chain(), sym: self.sym(), span: self.span() },
            6 => {
                let chain = self.chain();
                Ir::CreationEnvLookup { chain, sym: self.sym(), span: self.span() }
            }
            7 => Ir::SetMutField {
                owner_slot: self.u() as u16,
                field_slot: self.u() as u16,
                field_name: self.str_(),
                ic: FieldIc::new(),
                value: Box::new(self.ir()),
                span: self.span(),
            },
            8 => Ir::If { test: Box::new(self.ir()), then: Box::new(self.ir()), els: self.opt_ir().map(Box::new) },
            9 => Ir::Do(self.irs()),
            10 => Ir::Let { binds: self.binds(), body: self.irs() },
            11 => Ir::Loop { binds: self.binds(), scratch_base: self.u() as u16, body: self.irs() },
            12 => Ir::Recur { args: self.irs(), scratch_base: self.u() as u16 },
            13 => Ir::Call { callee: Box::new(self.ir()), args: self.irs(), span: self.span() },
            14 => {
                let chain = self.chain();
                let (sym, sym_span, args, span) = self.sym_span_args();
                Ir::CallGlobal { chain, sym, sym_span, args, span }
            }
            15 => {
                let chain = self.chain();
                let (sym, sym_span, args, span) = self.sym_span_args();
                Ir::CallCreationEnv { sym, chain, sym_span, args, span }
            }
            16 => {
                let op = INTRIN[self.byte() as usize];
                let chain = self.chain();
                let (sym, sym_span, args, span) = self.sym_span_args();
                Ir::Intrinsic { op, chain, sym, sym_span, args, span }
            }
            17 => Ir::VectorLit(self.irs()),
            18 => {
                let n = self.u() as usize;
                Ir::MapLit((0..n).map(|_| (self.ir(), self.ir())).collect())
            }
            19 => Ir::SetLit(self.irs()),
            20 => Ir::Throw { value: Box::new(self.ir()), span: self.span() },
            21 => Ir::MakeClosure { template: self.tpl(), caps: self.caps() },
            22 => {
                let n = self.u() as usize;
                let members = (0..n).map(|_| RecMember { template: self.tpl(), caps: self.caps() }).collect();
                let k = self.u() as usize;
                Ir::MakeRecGroup { members, slots: (0..k).map(|_| self.u() as u16).collect() }
            }
            23 => Ir::SiblingRef(self.u() as u16),
            24 => {
                let body = self.irs();
                let n = self.u() as usize;
                let catches = (0..n)
                    .map(|_| CatchArm { class: (self.byte() == 1).then(|| self.sym()), slot: self.u() as u16, body: self.irs() })
                    .collect();
                let finally = (self.byte() == 1).then(|| self.irs());
                Ir::Try { body, catches, finally }
            }
            25 => {
                let redefs = self.byte() == 1;
                let n = self.u() as usize;
                let pairs = (0..n).map(|_| (self.sym(), self.span(), self.ir())).collect();
                Ir::DynBind(Box::new(DynBind { redefs, pairs, body: self.irs(), span: self.span() }))
            }
            26 => Ir::Def { cell: self.cell(), value: self.opt_ir().map(Box::new) },
            27 => {
                let form = self.form();
                let n = self.u() as usize;
                let binds = (0..n).map(|_| (self.sym(), self.cap())).collect();
                Ir::Escape(Box::new(Escape { form, binds, span: self.span(), lens_site: crate::lens::NO_SITE }))
            }
            28 => {
                let field = self.str_();
                let kw = self.val();
                let recv = if self.byte() == 0 { FieldRecv::Slot(self.u() as u16) } else { FieldRecv::Capture(self.u() as u16) };
                Ir::FieldGet(Box::new(FieldGet { field, kw, recv, ic: FieldIc::new(), fallback: self.ir(), lens_site: crate::lens::NO_SITE }))
            }
            29 => {
                let class = self.form();
                let args = self.irs();
                let fallback = self.ir();
                Ir::New(Box::new(NewInst { class, args, fallback, span: self.span() }))
            }
            t => panic!("image: bad IR tag {t}"),
        }
    }
}
