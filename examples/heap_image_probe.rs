//! Heap-image KILL-PROBE (docs/HEAP-IMAGE-DESIGN.md). Throwaway measurement
//! harness, not a feature. Loads clojure-lsp.main exactly like
//! `mova/bin/clojure-lsp` does (minus `-main`), then prices the three
//! persisted-image variants:
//!
//!   census   -- live allocations/bytes after load, var/closure/form counts,
//!               top-level form head histogram (defn/defprotocol/...).
//!   floor(a) -- replay the post-load LIVE allocation histogram (malloc +
//!               write every byte): the lower bound of ANY full deserialize.
//!   codec    -- a compact binary Form codec over every loaded source file:
//!               encode size, decode time vs text-read time (prices (a)'s per
//!               node rate and (c)'s form cache).
//!   mmap(b)  -- map a file the size of the live heap MAP_PRIVATE, fault it
//!               read-only and then CoW-write one word per page.
//!
//! Usage (from the mova worktree):
//!   cargo run --release --example heap_image_probe -- \
//!       "$(cat "$MOVA_KONDO_DIR/module-path.txt")"
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use mova::internal::reader::{read_all_allow_cond_user, Form, FormValue, Span};
use mova::internal::{Interp, Keyword, Str, Symbol, Value};

// ---------------------------------------------------------------- allocator
const NB: usize = 64 + 24; // 64 x 16-byte linear buckets (<=1024), then log2
static TOTAL: AtomicU64 = AtomicU64::new(0);
static LIVE_N: AtomicU64 = AtomicU64::new(0);
static LIVE_B: AtomicU64 = AtomicU64::new(0);
static HIST: [AtomicU64; NB] = [const { AtomicU64::new(0) }; NB];

fn bucket(sz: usize) -> usize {
    if sz <= 1024 {
        (sz.max(1) - 1) / 16
    } else {
        (64 + (usize::BITS - (sz - 1).leading_zeros()) as usize - 10).min(NB - 1)
    }
}
fn bucket_size(b: usize) -> usize {
    if b < 64 {
        (b + 1) * 16
    } else {
        1usize << (b - 64 + 10)
    }
}
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        TOTAL.fetch_add(1, Relaxed);
        LIVE_N.fetch_add(1, Relaxed);
        LIVE_B.fetch_add(l.size() as u64, Relaxed);
        HIST[bucket(l.size())].fetch_add(1, Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE_N.fetch_sub(1, Relaxed);
        LIVE_B.fetch_sub(l.size() as u64, Relaxed);
        HIST[bucket(l.size())].fetch_sub(1, Relaxed);
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        LIVE_B.fetch_add(n as u64, Relaxed);
        LIVE_B.fetch_sub(l.size() as u64, Relaxed);
        HIST[bucket(l.size())].fetch_sub(1, Relaxed);
        HIST[bucket(n)].fetch_add(1, Relaxed);
        TOTAL.fetch_add(1, Relaxed);
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static A: Counting = Counting;

fn snap() -> (u64, u64, u64, Vec<u64>) {
    (
        TOTAL.load(Relaxed),
        LIVE_N.load(Relaxed),
        LIVE_B.load(Relaxed),
        HIST.iter().map(|h| h.load(Relaxed)).collect(),
    )
}
fn rss_mb() -> f64 {
    mova::internal::load_trace::maxrss_kb() as f64 / 1024.0
}

// ---------------------------------------------------------------- form codec
fn uv(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}
fn s(out: &mut Vec<u8>, t: &str) {
    uv(out, t.len() as u64);
    out.extend_from_slice(t.as_bytes());
}
#[derive(Default)]
struct Stats {
    nodes: u64,
    atoms_other: u64,
}
fn enc(out: &mut Vec<u8>, f: &Form, st: &mut Stats) {
    st.nodes += 1;
    let (tag, n) = match &f.value {
        FormValue::Atom(_) => (0u8, 0),
        FormValue::List(v) => (1, v.len()),
        FormValue::Vector(v) => (2, v.len()),
        FormValue::Map(v) => (3, v.len()),
        FormValue::Set(v) => (4, v.len()),
    };
    out.push(tag | if f.meta.is_some() { 0x80 } else { 0 });
    uv(out, f.span.start as u64);
    uv(out, f.span.end as u64);
    if let Some(m) = &f.meta {
        enc(out, m, st);
    }
    match &f.value {
        FormValue::Atom(v) => match v {
            Value::Nil => out.push(0),
            Value::Bool(b) => out.push(1 + *b as u8),
            Value::Int(i) => {
                out.push(3);
                uv(out, ((*i << 1) ^ (*i >> 63)) as u64)
            }
            Value::Float(x) => {
                out.push(4);
                out.extend_from_slice(&x.to_le_bytes())
            }
            Value::Str(t) => {
                out.push(5);
                s(out, t)
            }
            Value::Sym(sy) => {
                out.push(6);
                match &sy.ns {
                    Some(ns) => {
                        out.push(1);
                        s(out, ns)
                    }
                    None => out.push(0),
                }
                s(out, &sy.name)
            }
            Value::Keyword(k) => {
                out.push(7);
                s(out, k)
            }
            Value::Char(c) => {
                out.push(8);
                uv(out, *c as u64)
            }
            other => {
                st.atoms_other += 1;
                out.push(5);
                s(out, &mova::internal::pr_str(other))
            }
        },
        FormValue::List(v) | FormValue::Vector(v) | FormValue::Set(v) => {
            uv(out, n as u64);
            for c in v {
                enc(out, c, st);
            }
        }
        FormValue::Map(v) => {
            uv(out, n as u64);
            for (k, x) in v {
                enc(out, k, st);
                enc(out, x, st);
            }
        }
    }
}
struct Dec<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Dec<'a> {
    fn u(&mut self) -> u64 {
        let mut v = 0u64;
        let mut sh = 0;
        loop {
            let x = self.b[self.i];
            self.i += 1;
            v |= ((x & 0x7f) as u64) << sh;
            if x & 0x80 == 0 {
                return v;
            }
            sh += 7;
        }
    }
    fn s(&mut self) -> &'a str {
        let n = self.u() as usize;
        let t = unsafe { std::str::from_utf8_unchecked(&self.b[self.i..self.i + n]) };
        self.i += n;
        t
    }
    fn form(&mut self) -> Form {
        let t = self.b[self.i];
        self.i += 1;
        let span = Span { start: self.u() as usize, end: self.u() as usize };
        let meta = if t & 0x80 != 0 { Some(Box::new(self.form())) } else { None };
        let value = match t & 0x7f {
            0 => {
                let k = self.b[self.i];
                self.i += 1;
                FormValue::Atom(match k {
                    0 => Value::Nil,
                    1 => Value::Bool(false),
                    2 => Value::Bool(true),
                    3 => {
                        let z = self.u();
                        Value::Int(((z >> 1) as i64) ^ -((z & 1) as i64))
                    }
                    4 => {
                        let mut a = [0u8; 8];
                        a.copy_from_slice(&self.b[self.i..self.i + 8]);
                        self.i += 8;
                        Value::Float(f64::from_le_bytes(a))
                    }
                    5 => Value::Str(Str::from(self.s())),
                    6 => {
                        let has = self.b[self.i];
                        self.i += 1;
                        let ns = if has == 1 { Some(Str::from(self.s())) } else { None };
                        Value::Sym(Symbol { ns, name: Str::from(self.s()) })
                    }
                    7 => Value::Keyword(Keyword::construct(self.s())),
                    _ => Value::Char(char::from_u32(self.u() as u32).unwrap_or('?')),
                })
            }
            3 => {
                let n = self.u() as usize;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    let k = self.form();
                    let x = self.form();
                    v.push((k, x));
                }
                FormValue::Map(v)
            }
            tag => {
                let n = self.u() as usize;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(self.form());
                }
                match tag {
                    1 => FormValue::List(v),
                    2 => FormValue::Vector(v),
                    _ => FormValue::Set(v),
                }
            }
        };
        Form { value, span, meta }
    }
}
fn count_nodes(f: &Form) -> u64 {
    1 + f.meta.as_ref().map_or(0, |m| count_nodes(m))
        + match &f.value {
            FormValue::Atom(_) => 0,
            FormValue::List(v) | FormValue::Vector(v) | FormValue::Set(v) => v.iter().map(count_nodes).sum(),
            FormValue::Map(v) => v.iter().map(|(k, x)| count_nodes(k) + count_nodes(x)).sum(),
        }
}

// ---------------------------------------------------------------- main
fn main() {
    let mp = std::env::args().nth(1).expect("arg1: colon-separated module path");
    std::thread::Builder::new()
        .stack_size(512 << 20)
        .spawn(move || run(mp))
        .unwrap()
        .join()
        .unwrap();
}

fn run(mp: String) {
    let dirs: Vec<PathBuf> = mp.split(':').map(PathBuf::from).collect();
    let t0 = Instant::now();
    let mut it = Interp::new();
    it.max_depth = 10_000;
    it.module_paths = dirs.clone();
    it.eval_str("init", "(def *command-line-args* nil) (def *file* \"NO_SOURCE_PATH\")").unwrap();
    let t_boot = t0.elapsed();
    let (tot0, n0, b0, h0) = snap();
    let t1 = Instant::now();
    it.eval_str("entry", "(require 'clojure-lsp.main)").unwrap();
    let t_load = t1.elapsed();
    let (tot1, n1, b1, h1) = snap();
    println!("== load ==");
    println!("boot(Interp::new+core) {:.1} ms | require clojure-lsp.main {:.1} ms | maxrss {:.1} MB", ms(t_boot), ms(t_load), rss_mb());
    println!(
        "core-only live: {} allocs {:.1} MB | load: +{} total allocs, +{} live allocs, +{:.1} MB live bytes (avg {:.0} B/alloc)",
        n0,
        b0 as f64 / 1e6,
        tot1 - tot0,
        n1 as i64 - n0 as i64,
        (b1 - b0) as f64 / 1e6,
        (b1 - b0) as f64 / (n1 - n0) as f64
    );
    let dh: Vec<i64> = h1.iter().zip(&h0).map(|(a, b)| *a as i64 - *b as i64).collect();
    let small: i64 = dh[..4].iter().sum();
    println!("live-delta size mix: <=64B {} ({:.0}%), <=1KB {}, >1KB {}", small, 100.0 * small as f64 / dh.iter().sum::<i64>() as f64, dh[4..64].iter().sum::<i64>(), dh[64..].iter().sum::<i64>());

    // ---- census (vars) ----
    let t = Instant::now();
    let v = it
        .eval_str(
            "census",
            "(vec (for [n (all-ns) [s v] (ns-interns n)] [(str (ns-name n)) (try @v (catch Exception e nil))]))",
        )
        .unwrap();
    let rows: Vec<Value> = match &v {
        Value::Vector(pv) => pv.iter().cloned().collect(),
        _ => vec![],
    };
    let mut kinds: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut nss = HashSet::new();
    let mut seen = HashSet::new();
    let (mut arities, mut body_nodes) = (0u64, 0u64);
    for r in &rows {
        let Value::Vector(p) = r else { continue };
        let pr: Vec<Value> = p.iter().cloned().collect();
        if let Value::Str(n) = &pr[0] {
            nss.insert(n.to_string());
        }
        let k = match &pr[1] {
            Value::Fn(c) | Value::Macro(c) => {
                if seen.insert(std::sync::Arc::as_ptr(c) as usize) {
                    arities += c.arities.len() as u64;
                    body_nodes += c.arities.iter().flat_map(|a| a.body.iter()).map(count_nodes).sum::<u64>();
                }
                if matches!(&pr[1], Value::Fn(_)) { "fn" } else { "macro" }
            }
            Value::Native(_) => "native",
            Value::Atom(_) => "atom",
            Value::Volatile(_) => "volatile",
            Value::Map(_) => "map(protocol/config)",
            Value::Nil => "nil/unbound",
            Value::Class(_) => "class(record/type)",
            Value::Var(_) => "var",
            Value::Delay(_) => "delay",
            Value::Promise(_) | Value::Future(_) | Value::Channel(_) | Value::Timer(_) => "HOST(promise/future/chan/timer)",
            Value::HostInst(_) | Value::HostStruct(_) => "HOST(inst/struct)",
            Value::Regex(_) => "regex",
            Value::Lazy(_) => "lazy",
            Value::Str(_) | Value::Keyword(_) | Value::Int(_) | Value::Bool(_) | Value::Float(_) => "scalar",
            Value::Vector(_) | Value::Set(_) | Value::List(_) => "coll",
            _ => "other",
        };
        *kinds.entry(k).or_default() += 1;
    }
    println!("\n== census ({:.1} ms) ==", ms(t.elapsed()));
    println!("namespaces {} | vars {} | unique closures {} arities {} body Form nodes {}", nss.len(), rows.len(), seen.len(), arities, body_nodes);
    println!("var value kinds: {kinds:?}");
    println!("interned keywords (process-wide table): {}", mova::internal::Keyword::construct("zz-probe").interned_id().map_or(0, |i| i + 1));

    // ---- floor (a): replay live allocation histogram ----
    let t = Instant::now();
    let mut keep: Vec<(*mut u8, Layout)> = Vec::with_capacity((n1 - n0) as usize);
    for (bk, c) in dh.iter().enumerate() {
        let l = Layout::from_size_align(bucket_size(bk), 8).unwrap();
        for _ in 0..(*c).max(0) {
            unsafe {
                let p = System.alloc(l);
                std::ptr::write_bytes(p, 0x5a, l.size());
                keep.push((p, l));
            }
        }
    }
    let t_floor = t.elapsed();
    println!("\n== floor(a): alloc+fill {} blocks ({:.1} MB, bucket-rounded) = {:.1} ms", keep.len(), keep.iter().map(|x| x.1.size()).sum::<usize>() as f64 / 1e6, ms(t_floor));
    for (p, l) in keep {
        unsafe { System.dealloc(p, l) }
    }

    // ---- codec over every loaded source file ----
    let mut files = vec![];
    for n in &nss {
        let rel = n.replace('-', "_").replace('.', "/");
        'f: for d in &dirs {
            for ext in ["mova", "clj", "cljc"] {
                let p = d.join(format!("{rel}.{ext}"));
                if p.exists() {
                    files.push(p);
                    break 'f;
                }
            }
        }
    }
    let srcs: Vec<String> = files.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect();
    let src_bytes: usize = srcs.iter().map(|s| s.len()).sum();
    let t = Instant::now();
    let mut forms = vec![];
    let mut bad = 0;
    for s in &srcs {
        match read_all_allow_cond_user(s) {
            Ok(f) => forms.push(f),
            Err(_) => bad += 1,
        }
    }
    let t_read = t.elapsed();
    let mut heads: BTreeMap<String, u64> = BTreeMap::new();
    for f in forms.iter().flatten() {
        if let FormValue::List(v) = &f.value {
            if let Some(Form { value: FormValue::Atom(Value::Sym(sy)), .. }) = v.first() {
                *heads.entry(sy.name.to_string()).or_default() += 1;
            }
        }
    }
    let mut top: Vec<_> = heads.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    let mut buf = vec![];
    let mut st = Stats::default();
    for f in forms.iter().flatten() {
        enc(&mut buf, f, &mut st);
    }
    let nforms: usize = forms.iter().map(|v| v.len()).sum();
    let (a0, _, _, _) = snap();
    let t = Instant::now();
    let mut d = Dec { b: &buf, i: 0 };
    let mut back = Vec::with_capacity(nforms);
    for _ in 0..nforms {
        back.push(d.form());
    }
    let t_dec = t.elapsed();
    let (a1, _, _, _) = snap();
    println!("\n== codec: {} files ({} unreadable w/o ns ctx), {:.2} MB source, {} top-level forms, {} Form nodes ({} non-scalar atoms) ==", files.len(), bad, src_bytes as f64 / 1e6, nforms, st.nodes, st.atoms_other);
    println!("text read  {:.1} ms ({:.0} MB/s, {:.0} ns/node)", ms(t_read), src_bytes as f64 / 1e6 / t_read.as_secs_f64(), t_read.as_nanos() as f64 / st.nodes as f64);
    println!(
        "bin decode {:.1} ms ({:.2} MB image, {:.0} MB/s, {:.0} ns/node, {} allocs = {:.1}/node)",
        ms(t_dec),
        buf.len() as f64 / 1e6,
        buf.len() as f64 / 1e6 / t_dec.as_secs_f64(),
        t_dec.as_nanos() as f64 / st.nodes as f64,
        a1 - a0,
        (a1 - a0) as f64 / st.nodes as f64
    );
    println!("top-level heads: {:?}", &top[..top.len().min(22)]);
    drop(back);

    // ---- mmap (b) ----
    let img = std::env::temp_dir().join("mova_heap_image_probe.bin");
    let len = (b1 - b0) as usize;
    std::fs::write(&img, vec![0x5au8; len]).unwrap();
    unsafe {
        let fd = libc::open(std::ffi::CString::new(img.to_str().unwrap()).unwrap().as_ptr(), libc::O_RDONLY);
        let t = Instant::now();
        let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE, fd, 0) as *mut u8;
        let t_map = t.elapsed();
        let pg = libc::sysconf(libc::_SC_PAGESIZE) as usize;
        let t = Instant::now();
        let mut acc = 0u64;
        let mut i = 0;
        while i < len {
            acc = acc.wrapping_add(*p.add(i) as u64);
            i += pg;
        }
        let t_rd = t.elapsed();
        let t = Instant::now();
        let mut i = 0;
        while i < len {
            *p.add(i) = 1;
            i += pg;
        }
        let t_wr = t.elapsed();
        println!(
            "\n== mmap(b): {:.1} MB image, page {} B: mmap {:.3} ms, fault-all read {:.1} ms, CoW-write-all {:.1} ms (acc {acc}) | maxrss now {:.1} MB",
            len as f64 / 1e6,
            pg,
            ms(t_map),
            ms(t_rd),
            ms(t_wr),
            rss_mb()
        );
        libc::munmap(p as *mut _, len);
        libc::close(fd);
    }
    let _ = std::fs::remove_file(img);
}
fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1e3
}
