//! `nx_query <root> <method> <rel-file> <line> <char> [--bench N] [--jar-scheme]`: settle the project, answer one query, print it.
use nx_core::engine::{ClientOpts, Engine};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: nx_query <root> <method> <rel-file> <line> <char> [--bench N]");
        return;
    }
    let root = std::path::PathBuf::from(&a[1]).canonicalize().unwrap();
    let e = Engine::new(0);
    e.set_client_opts(ClientOpts { jar_scheme: true, arity_on_same_line: true, ..Default::default() });
    let t0 = Instant::now();
    let (batch, total, _) = e.analyze_project(&root);
    let t1 = t0.elapsed();
    let mut done = 0;
    while done < total {
        let r = e.await_results(64).unwrap();
        done += r.len();
        e.commit(&r);
    }
    eprintln!("jars+discover {:.1} ms, settled {:.1} ms ({} files, batch {})", t1.as_secs_f64() * 1e3, t0.elapsed().as_secs_f64() * 1e3, total, batch);
    {
        let sn = e.store.snapshot();
        let ext_dirs = sn.uris().filter(|u| u.starts_with("file:")).count();
        eprintln!("entries {} (file: {} , jars {})", sn.file_count, ext_dirs, sn.jars.as_ref().map_or(0, |j| j.files.len()));
    }
    let path = root.join(if a.len() > 3 { &a[3] } else { "" });
    let mut uri = nx_core::engine::scan::path_to_uri(&path);
    if let Ok(alias) = std::env::var("NXQ_ALIAS") {
        uri = uri.replacen(&alias.split('=').next().unwrap().to_string(), alias.split('=').nth(1).unwrap(), 1);
    }
    let snap = e.store.snapshot();
    let at = nx_core::query::At { uri: &uri, line: a.get(4).and_then(|x| x.parse().ok()).unwrap_or(0), ch: a.get(5).and_then(|x| x.parse().ok()).unwrap_or(0) };
    let inc = true;
    if a[2] == "debug-def" {
        // nx_query <root> debug-def <ns> <name>
        let q = nx_core::query::Q::new(&snap);
        let (ns, name) = (nx_core::intern::intern(&a[3]), nx_core::intern::intern(&a[4]));
        for l in [1u8, 2] {
            if let Some(d) = q.last_var_def(ns, name, l, false) {
                let v = &q.fa(d.f).var_definitions[d.i as usize];
                println!("lang {l}: {} {:?} has_arglists {} macro {} lang {} fixed {:?}", q.uri(d.f), v.name_pos, v.has_arglists, v.macro_, v.lang, v.fixed);
                let f = q.fa(d.f);
                for x in f.var_definitions.iter().filter(|x| x.ns == ns && x.name == name) {
                    println!("  def lang {} has_arglists {} arglists {:?} macro {}", x.lang, x.has_arglists, x.arglists, x.macro_);
                }
            }
        }
        return;
    }
    let t = Instant::now();
    let out = nx_core::query::answer(&snap, &a[2], at, inc);
    eprintln!("first {:.3} ms", t.elapsed().as_secs_f64() * 1e3);
    println!("{}", out.unwrap_or_else(|| "<none>".into()));
    if let Some(p) = a.iter().position(|x| x == "--bench") {
        let n: usize = a[p + 1].parse().unwrap();
        let mut v = Vec::new();
        for _ in 0..n {
            let t = Instant::now();
            std::hint::black_box(nx_core::query::answer(&snap, &a[2], at, inc));
            v.push(t.elapsed().as_secs_f64() * 1e6);
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!("p50 {:.1} us  p99 {:.1} us", v[n / 2], v[n * 99 / 100]);
    }
}
