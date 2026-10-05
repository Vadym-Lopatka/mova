//! `mem_attr <project-root>`: logical heap attribution (counting allocator) of the settled store.
use nx_core::engine::Engine;
use std::alloc::{GlobalAlloc, Layout};
static M: mimalloc::MiMalloc = mimalloc::MiMalloc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct C;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static USABLE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for C {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let n = LIVE.fetch_add(l.size(), Relaxed) + l.size();
        PEAK.fetch_max(n, Relaxed);
        let p = M.alloc(l);
        USABLE.fetch_add(libmimalloc_sys::mi_usable_size(p as *const _), Relaxed);
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        USABLE.fetch_sub(libmimalloc_sys::mi_usable_size(p as *const _), Relaxed);
        M.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if n >= l.size() {
            let v = LIVE.fetch_add(n - l.size(), Relaxed) + n - l.size();
            PEAK.fetch_max(v, Relaxed);
        } else {
            LIVE.fetch_sub(l.size() - n, Relaxed);
        }
        USABLE.fetch_sub(libmimalloc_sys::mi_usable_size(p as *const _), Relaxed);
        let q = M.realloc(p, l, n);
        USABLE.fetch_add(libmimalloc_sys::mi_usable_size(q as *const _), Relaxed);
        q
    }
}
#[global_allocator]
static G: C = C;
fn live() -> usize {
    LIVE.load(Relaxed)
}
fn mb(x: usize) -> f64 {
    x as f64 / 1e6
}

fn fp() -> f64 {
    let o = std::process::Command::new("footprint").args(["--noCategories", "-f", "bytes", &std::process::id().to_string()]).output().unwrap();
    let t = String::from_utf8_lossy(&o.stdout);
    t.split("phys_footprint: ").nth(1).and_then(|x| x.split(' ').next()).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) / 1e6
}

fn main() {
    let f0 = fp();
    let root = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    if std::env::var("SEQ").is_ok() {
        let info = nx_core::engine::scan::discover(&root);
        let ctx = nx_core::engine::ctx::Ctx::new();
        let l0 = live();
        let mut keep = Vec::new();
        for f in &info.files {
            let t = std::fs::read_to_string(f).unwrap();
            let lang = nx_core::engine::Lang::from_path(&f.to_string_lossy());
            keep.push(nx_core::engine::analyze::analyze_file_ctx_pos(&f.to_string_lossy(), t, lang, true, false, &ctx));
        }
        let f1 = fp();
        unsafe { libmimalloc_sys::mi_collect(true) };
        println!("SEQ pass1-only: logical {:.1} MB, footprint {f1:.1} -> {:.1} after collect", mb(live() - l0), fp());
        drop(keep);
        return;
    }
    nx_core::engine::set_trim_hook(|| unsafe { libmimalloc_sys::mi_collect(true) });
    let l0 = live();
    let e = Engine::new(0);
    let l1 = live();
    let n = if std::env::var("JARS").is_ok() {
        let (_b, total, _sp) = if std::env::var("STAGES").is_ok() {
            let mut info = nx_core::engine::scan::discover(&root);
            println!("stage discover: fp {:.1}", fp());
            nx_core::jdk::set_root(&root);
            e.store.ctx().cfg.store(std::sync::Arc::new(nx_core::analyzer::Config::load(&root)));
            println!("stage config: fp {:.1}", fp());
            e.load_classpath_jars(&root);
            println!("stage classpath+jars: fp {:.1} logical {:.1} peak-logical {:.1}", fp(), mb(live() - l1), mb(PEAK.load(Relaxed) - l0));
            if std::env::var("FPCAT").is_ok() { let o = std::process::Command::new("footprint").args(["-f", "bytes", &std::process::id().to_string()]).output().unwrap(); println!("{}", String::from_utf8_lossy(&o.stdout)); }
            let files = std::mem::take(&mut info.files);
            let total = files.len();
            e.store.set_project(std::sync::Arc::new(info));
            e.pool.submit_batch(files);
            if let Some(j) = &e.store.snapshot().jars {
                j.warm();
            }
            println!("stage warm: fp {:.1} logical {:.1}", fp(), mb(live() - l1));
            (0, total, Vec::<String>::new())
        } else {
            e.analyze_project(&root)
        };
        {
            let sn = e.store.snapshot();
            let b = live();
            let mut d = nx_core::analyzer::DefsIndex::new();
            sn.jars.as_ref().unwrap().layer.feed(&mut d);
            println!("one jar DefsIndex copy: {:.2} MB (jar files {}, cache bytes mmapped {:.1} MB)", mb(live() - b), sn.jars.as_ref().unwrap().files.len(), sn.jars.as_ref().unwrap().layer.jars.iter().map(|j| j.cache_bytes).sum::<usize>() as f64 / 1e6);
            drop(d);
        }
        if std::env::var("DECODE_ALL").is_ok() {
            let sn = e.store.snapshot();
            let b = live();
            let t = std::time::Instant::now();
            let mut n = 0;
            let mut top: Vec<(usize, String)> = Vec::new();
            for f in &sn.jars.as_ref().unwrap().files {
                let b1 = live();
                if f.fa().is_some() {
                    n += 1;
                    top.push((live() - b1, f.uri.to_string()));
                }
            }
            top.sort();
            println!("decode all {n} jar files: {:.1} MB logical in {:?}; largest:", mb(live() - b), t.elapsed());
            for (sz, u) in top.iter().rev().take(5) {
                println!("   {:.2} MB {u}", mb(*sz));
            }
        }
        println!("jar layer (classpath) loaded: logical {:.1} MB footprint {:.1}", mb(live() - l1), fp());
        total
    } else {
        let mut info = nx_core::engine::scan::discover(&root);
        let files = std::mem::take(&mut info.files);
        let n = files.len();
        e.store.set_project(std::sync::Arc::new(info));
        e.pool.submit_batch(files);
        n
    };
    let mut done = 0;
    while done < n {
        let r = e.await_results(64).unwrap();
        done += r.len();
        e.commit_with(r);
    }
    std::thread::sleep(std::time::Duration::from_millis(if std::env::var("JARS").is_ok() { 1500 } else { 500 }));
    let f1 = fp();
    unsafe { libmimalloc_sys::mi_collect(true) };
    std::thread::sleep(std::time::Duration::from_millis(200));
    let f2 = fp();
    if std::env::var("MI_STATS").is_ok() { unsafe { libmimalloc_sys::mi_stats_print(std::ptr::null_mut()) }; }
    println!("footprint: start {f0:.1}  settled {f1:.1}  after mi_collect {f2:.1} MB (logical heap {:.1}, size-class usable {:.1})", mb(live()), mb(USABLE.load(Relaxed)));
    if std::env::var("OPEN").is_ok() {
     let uris0: Vec<String> = e.store.snapshot().uris().map(|u| u.to_string()).collect();
     for gen in 0..3 {
        let s = e.store.snapshot();
        let uris: Vec<String> = s.uris().map(|u| u.to_string()).collect();
        for u in &uris {
            let p = nx_core::engine::scan::uri_to_path(u).unwrap();
            e.analyze_text(u, 1 + gen, std::fs::read_to_string(p).unwrap());
        }
        drop(s);
        let mut d = 0;
        while d < uris.len() {
            let r = e.await_results(64).unwrap();
            d += r.len();
            e.commit_with(r);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        let f3 = fp();
        unsafe { libmimalloc_sys::mi_collect(true) };
        println!("open-all: footprint {f3:.1} -> {:.1} after collect; logical {:.1} usable {:.1}", fp(), mb(live()), mb(USABLE.load(Relaxed)));
     }
        for u in &uris0 {
            e.analyze_disk_override(u);
        }
        let mut d = 0;
        while d < uris0.len() {
            let r = e.await_results(64).unwrap();
            d += r.len();
            e.commit_with(r);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        unsafe { libmimalloc_sys::mi_collect(true) };
        if std::env::var("MI_STATS").is_ok() { unsafe { libmimalloc_sys::mi_stats_print(std::ptr::null_mut()) }; }
        if std::env::var("FPCAT").is_ok() { let o = std::process::Command::new("footprint").args(["-f", "bytes", &std::process::id().to_string()]).output().unwrap(); println!("{}", String::from_utf8_lossy(&o.stdout)); }
        println!("closed again: footprint {:.1}; logical {:.1} usable {:.1}", fp(), mb(live()), mb(USABLE.load(Relaxed)));
        return;
    }
    let l2 = live();
    println!("engine+pool       {:7.2} MB", mb(l1 - l0));
    println!("settled store     {:7.2} MB (peak during {:.2} MB) files={n} interned={}", mb(l2 - l1), mb(PEAK.load(Relaxed) - l0), nx_core::intern::len());
    let s = e.store.snapshot();
    println!("  index lens: by_uri {} ns_files {} ns_deps {} defs {} uses {} kws {} kw_ns {}", s.by_uri.len(), s.ns_files.len(), s.ns_deps.len(), s.defs.len(), s.uses.len(), s.kws.len(), s.kw_ns.len());
    let (mut text, mut an, mut pos, mut tgt, mut fnd, mut cst_equiv) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut parts: std::collections::BTreeMap<&str, usize> = Default::default();
    for u in s.uris() {
        let Some(en) = s.get(u) else { continue };
        if let Some(t) = &en.text {
            text += t.len();
            cst_equiv += nx_core::reader::parse(t).heap_bytes();
        }
        if let Some(a) = &en.analysis {
            for (k, n) in [
                ("lint_uses", a.lint_uses.len() * std::mem::size_of::<nx_core::analyzer::lint::uses::LUse>()),
                ("lint_protos", a.lint_protos.len()),
                ("lint_ignores", a.lint_ignores.len()),
                ("lint_ralls", a.lint_ralls.len()),
                ("lint_hofs", a.lint_hofs.len()),
                ("lint_tfind", a.lint_tfind.len()),
                ("lint_levels", a.lint_levels.len()),
                ("findings", a.findings.len()),
                ("var_usages*sz", a.var_usages.len() * std::mem::size_of::<nx_core::analyzer::VarUsage>()),
                ("var_defs*sz", a.var_definitions.len() * std::mem::size_of::<nx_core::analyzer::VarDef>()),
                ("locals*sz", a.locals.len() * std::mem::size_of::<nx_core::analyzer::Local>()),
                ("local_usages*sz", a.local_usages.len() * std::mem::size_of::<nx_core::analyzer::LocalUsage>()),
                ("callstacks*16", a.callstacks.len() * 8),
                ("symbols*sz", a.symbols.len() * std::mem::size_of::<nx_core::analyzer::SymbolUse>()),
                ("keywords*sz", a.keywords.len() * std::mem::size_of::<nx_core::analyzer::Keyword>()),
            ] {
                *parts.entry(k).or_default() += n;
            }
            let b = live();
            let c = (**a).clone();
            an += live() - b;
            drop(c);
        }
        if let Some(p) = &en.pos {
            pos += p.ents.capacity() * 16 + p.multi.capacity() * std::mem::size_of::<nx_core::engine::index::Ent>() + (p.local_by_id.capacity() * 4);
        }
        let b = live();
        let c = en.tgt.to_vec();
        tgt += live() - b;
        drop(c);
        let b = live();
        let c = en.findings.clone();
        fnd += live() - b;
        drop(c);
    }
    {
        use nx_core::analyzer::*;
        let (mut nu, mut nl, mut nlu, mut nd, mut nk, mut nlint) = (0, 0, 0, 0, 0, 0);
        for u in s.uris() {
            if let Some(a) = s.get(u).and_then(|e| e.analysis.clone()) {
                nu += a.var_usages.len();
                nl += a.locals.len();
                nlu += a.local_usages.len();
                nd += a.var_definitions.len();
                nk += a.keywords.len();
                nlint += a.lint_uses.len();
            }
        }
        println!("  counts: var_usages {nu} x{}B  locals {nl} x{}B  local_usages {nlu} x{}B  var_defs {nd} x{}B  keywords {nk} x{}B  lint_uses {nlint} x{}B  Pos {}B",
            std::mem::size_of::<VarUsage>(), std::mem::size_of::<Local>(), std::mem::size_of::<LocalUsage>(), std::mem::size_of::<VarDef>(), std::mem::size_of::<Keyword>(), std::mem::size_of::<nx_core::analyzer::lint::uses::LUse>(), std::mem::size_of::<nx_core::cst::Pos>());
    }
    for (k, v) in &parts {
        println!("    {k:16} {:7.2} MB", mb(*v));
    }
    println!("  text            {:7.2} MB", mb(text));
    println!("  FileAnalysis    {:7.2} MB", mb(an));
    println!("  PosIdx          {:7.2} MB", mb(pos));
    println!("  tgt             {:7.2} MB", mb(tgt));
    println!("  findings        {:7.2} MB", mb(fnd));
    println!("  (CST if kept    {:7.2} MB)", mb(cst_equiv));
    println!("  sum             {:7.2} MB  unaccounted (store maps, snapshot, interner, pool) {:.2} MB", mb(text + an + pos + tgt + fnd), mb(l2 - l1) - mb(text + an + pos + tgt + fnd));
}
