//! S3 (E4a, docs/E4-AOT-DESIGN.md): threaded-tier native code persisted in the
//! heap image. At image write, each arity that holds a threaded entry is
//! lowered again with every pointer-range `iconst` turned into an `Abs8`
//! relocation; each absolute target is then classified WITHOUT looking at
//! node kinds: a fresh per-site cache (re-allocated by its factory), a byte
//! inside one of the arity's own `Ir` nodes (pre-order index + offset, found
//! again by the same walk over the restored IR), or an address inside this
//! executable (delta from its load base; the image key covers the binary).
//! Anything else makes the arity non-persistable (it just lowers lazily).
//! On first call after restore the record is copied into a MAP_JIT arena,
//! patched and bound -- no Cranelift. Redefinition semantics are the
//! runtime tier's own: every global goes through a fresh epoch-checked site.

use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::ir::{
    immediates::Imm64, types, ExternalName, Function, GlobalValueData, InstBuilder, InstructionData, Opcode,
};
use cranelift_codegen::{Context, FinalizedRelocTarget};
use cranelift_jit::JITModule;
use cranelift_module::{Linkage, Module, ModuleRelocTarget};

use super::{ThreadedEntry, AOT_BOUND_COUNT, AOT_BOUND_NS};
use crate::compile::ir::Ir;
use crate::compile::CompiledFn;

pub(super) const LIT_SYM: &str = "mova_aot_lit_base";
const VERSION: u8 = 1;
const LO: u64 = 1 << 32;
const HI: u64 = 1 << 48;

static LIT_ANCHOR: u8 = 0;
/// Absolute refs `encode` could not classify (each one skips its arity).
pub static UNCLASSIFIED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Address every rewritten constant is expressed against (symbol + addend).
pub(super) fn lit_base() -> usize {
    &LIT_ANCHOR as *const u8 as usize
}

thread_local! {
    static FRESH: RefCell<Option<Vec<(usize, usize)>>> = const { RefCell::new(None) };
}

/// Lowering hook: `addr` is a freshly leaked per-site cache; `make` leaks another.
pub(super) fn note_fresh(addr: usize, make: fn() -> usize) {
    FRESH.with(|f| {
        if let Some(v) = f.borrow_mut().as_mut() {
            v.push((addr, make as usize));
        }
    });
}

pub(super) fn begin() {
    FRESH.with(|f| *f.borrow_mut() = Some(Vec::new()));
}

pub(super) fn end() -> Vec<(usize, usize)> {
    FRESH.with(|f| f.borrow_mut().take().unwrap_or_default())
}

fn dl_base(a: usize) -> Option<usize> {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    (unsafe { libc::dladdr(a as *const libc::c_void, &mut info) } != 0).then_some(info.dli_fbase as usize)
}

#[cfg(target_os = "macos")]
extern "C" {
    fn _dyld_get_image_header(i: u32) -> *const libc::c_void;
}

// Main executable's load base; on macOS without `dladdr` (its symbol lookup pages in ~3 MB of LINKEDIT).
fn exe_base() -> usize {
    static B: OnceLock<usize> = OnceLock::new();
    #[cfg(target_os = "macos")]
    let b = || unsafe { _dyld_get_image_header(0) } as usize;
    #[cfg(not(target_os = "macos"))]
    let b = || dl_base(lit_base()).unwrap_or(0);
    *B.get_or_init(b)
}

/// Every I64 `iconst` in pointer range becomes `symbol_value(LIT_SYM + delta)`.
pub(super) fn relocatable_consts(m: &mut JITModule, func: &mut Function) {
    let Ok(did) = m.declare_data(LIT_SYM, Linkage::Import, false, false) else { return };
    let gv0 = m.declare_data_in_func(did, func);
    let GlobalValueData::Symbol { name, .. } = func.global_values[gv0].clone() else { return };
    let base = lit_base() as i64;
    let mut insts = Vec::new();
    for blk in func.layout.blocks() {
        insts.extend(func.layout.block_insts(blk));
    }
    for inst in insts {
        let InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } = func.dfg.insts[inst] else { continue };
        let v = imm.bits() as u64;
        if !(LO..HI).contains(&v) || func.dfg.value_type(func.dfg.first_result(inst)) != types::I64 {
            continue;
        }
        let gv = func.create_global_value(GlobalValueData::Symbol {
            name: name.clone(),
            offset: Imm64::new(v as i64 - base),
            colocated: false,
            tls: false,
        });
        func.stencil.replace(inst).symbol_value(types::I64, gv);
    }
}

fn nodes(body: &[Ir]) -> Vec<usize> {
    let mut v = Vec::new();
    crate::compile::lastuse::walk_all(body, &mut |n| v.push(n as *const Ir as usize));
    v
}

fn put32(o: &mut Vec<u8>, x: u32) {
    o.extend_from_slice(&x.to_le_bytes());
}
fn put64(o: &mut Vec<u8>, x: u64) {
    o.extend_from_slice(&x.to_le_bytes());
}

/// Record: ver u8, recur u8, extra u32, code_len u32, code, n u32,
/// n x (off u32, tag u8, v u64); tag 0 = node (idx<<32|off), 1 = fresh
/// (factory exe delta), 2 = exe delta.
pub(super) fn encode(
    m: &JITModule,
    ctx: &Context,
    body: &[Ir],
    fresh: &[(usize, usize)],
    extra: u32,
    recur: bool,
) -> Option<Vec<u8>> {
    stats_once();
    let cc = ctx.compiled_code()?;
    let exe = exe_base();
    let in_exe = |a: usize| exe != 0 && dl_base(a) == Some(exe);
    let mut ns: Vec<(usize, u32)> = nodes(body).into_iter().enumerate().map(|(i, a)| (a, i as u32)).collect();
    ns.sort_unstable();
    let sz = std::mem::size_of::<Ir>();
    let code = cc.code_buffer();
    let mut o = vec![VERSION, recur as u8];
    put32(&mut o, extra);
    put32(&mut o, code.len() as u32);
    o.extend_from_slice(code);
    let relocs = cc.buffer.relocs();
    put32(&mut o, relocs.len() as u32);
    for r in relocs {
        let (Reloc::Abs8, FinalizedRelocTarget::ExternalName(ExternalName::User(u))) = (r.kind, &r.target) else {
            return None;
        };
        let un = &ctx.func.params.user_named_funcs()[*u];
        let t = ModuleRelocTarget::User { namespace: un.namespace, index: un.index };
        let a = (m.get_address(&t) as i64 + r.addend) as usize;
        put32(&mut o, r.offset);
        if let Some(&(_, make)) = fresh.iter().find(|(f, _)| *f == a) {
            if !in_exe(make) {
                return None;
            }
            o.push(1);
            put64(&mut o, (make - exe) as u64);
            continue;
        }
        let i = ns.partition_point(|&(s, _)| s <= a);
        if i > 0 && a < ns[i - 1].0 + sz {
            let (s, idx) = ns[i - 1];
            o.push(0);
            put64(&mut o, ((idx as u64) << 32) | (a - s) as u64);
            continue;
        }
        if in_exe(a) {
            o.push(2);
            put64(&mut o, (a - exe) as u64);
            continue;
        }
        // Unknown absolute ref: never persist it; count it and trip debug builds.
        UNCLASSIFIED.fetch_add(1, Ordering::Relaxed);
        if super::explain_enabled() {
            eprintln!("threaded: image-code: unclassified constant {a:#x}");
        }
        debug_assert!(false, "image-code: unclassified absolute ref {a:#x} (K3 site cache missing note_fresh?)");
        return None;
    }
    Some(o)
}

struct Arena {
    p: usize,
    left: usize,
}

const CHUNK: usize = 1 << 20;

fn alloc(n: usize) -> Option<*mut u8> {
    static A: Mutex<Arena> = Mutex::new(Arena { p: 0, left: 0 });
    let n = (n + 15) & !15;
    let mut a = A.lock().ok()?;
    if a.left < n {
        let len = n.max(CHUNK);
        #[cfg(target_os = "macos")]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT;
        #[cfg(not(target_os = "macos"))]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON;
        let prot = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, -1, 0) };
        if p == libc::MAP_FAILED {
            return None;
        }
        a.p = p as usize;
        a.left = len;
    }
    let p = a.p;
    a.p += n;
    a.left -= n;
    Some(p as *mut u8)
}

#[cfg(target_os = "macos")]
extern "C" {
    fn sys_icache_invalidate(start: *mut libc::c_void, len: usize);
}

fn rd32(b: &[u8], i: &mut usize) -> Option<u32> {
    let x = u32::from_le_bytes(b.get(*i..*i + 4)?.try_into().ok()?);
    *i += 4;
    Some(x)
}
fn rd64(b: &[u8], i: &mut usize) -> Option<u64> {
    let x = u64::from_le_bytes(b.get(*i..*i + 8)?.try_into().ok()?);
    *i += 8;
    Some(x)
}

/// `MOVA_AOT_STATS=1`: bound/lowered counts and ns at exit.
fn stats_once() {
    static ON: OnceLock<()> = OnceLock::new();
    ON.get_or_init(|| {
        if std::env::var("MOVA_AOT_STATS").is_ok_and(|v| v == "1") {
            extern "C" fn dump() {
                eprintln!(
                    "aot-stats: bound={} bind_ns={} lowered={} lower_ns={} unclassified={}",
                    AOT_BOUND_COUNT.load(Ordering::Relaxed),
                    AOT_BOUND_NS.load(Ordering::Relaxed),
                    super::THREADED_LOWER_COUNT.load(Ordering::Relaxed),
                    super::THREADED_LOWER_NS.load(Ordering::Relaxed),
                    UNCLASSIFIED.load(Ordering::Relaxed)
                );
            }
            unsafe { libc::atexit(dump) };
        }
    });
}

/// Binds image code for arity `idx` of `code`; `None` = lower as usual.
pub(super) fn bind(code: &Arc<CompiledFn>, idx: usize, recur: bool, b: &[u8]) -> Option<(ThreadedEntry, u32)> {
    let t0 = Instant::now();
    stats_once();
    if b.len() < 10 || b[0] != VERSION || b[1] != recur as u8 {
        return None;
    }
    let exe = exe_base();
    if exe == 0 {
        return None;
    }
    let mut i = 2;
    let extra = rd32(b, &mut i)?;
    let clen = rd32(b, &mut i)? as usize;
    let text = b.get(i..i + clen)?;
    i += clen;
    let n = rd32(b, &mut i)? as usize;
    let mut ns: Option<Vec<usize>> = None;
    let mut patches = Vec::with_capacity(n);
    for _ in 0..n {
        let off = rd32(b, &mut i)? as usize;
        let tag = *b.get(i)?;
        i += 1;
        let v = rd64(b, &mut i)?;
        let a = match tag {
            0 => {
                let ns = ns.get_or_insert_with(|| nodes(&code.arities[idx].body));
                *ns.get((v >> 32) as usize)? + (v & 0xffff_ffff) as usize
            }
            1 => {
                let make: fn() -> usize = unsafe { std::mem::transmute(exe + v as usize) };
                make()
            }
            2 => exe + v as usize,
            _ => return None,
        };
        if off + 8 > clen {
            return None;
        }
        patches.push((off, a as u64));
    }
    let p = alloc(clen)?;
    unsafe {
        #[cfg(target_os = "macos")]
        libc::pthread_jit_write_protect_np(0);
        std::ptr::copy_nonoverlapping(text.as_ptr(), p, clen);
        for (off, a) in patches {
            std::ptr::write_unaligned(p.add(off) as *mut u64, a);
        }
        #[cfg(target_os = "macos")]
        {
            libc::pthread_jit_write_protect_np(1);
            sys_icache_invalidate(p as *mut libc::c_void, clen);
        }
    }
    AOT_BOUND_COUNT.fetch_add(1, Ordering::Relaxed);
    AOT_BOUND_NS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
    if super::explain_enabled() {
        let name = code.name.as_deref().unwrap_or("<anonymous>");
        eprintln!("threaded: {name} arity {idx} image-bound ({clen} B, {n} relocs)");
    }
    let entry: extern "C" fn(*mut super::TCtx) -> u32 = unsafe { std::mem::transmute(p) };
    Some((ThreadedEntry(entry), extra))
}
