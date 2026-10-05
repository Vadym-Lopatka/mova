//! `time_one <file>`: parse vs analyze timing for one file.
use nx_core::analyzer::*;
use std::time::Instant;
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let src = std::fs::read_to_string(&p).unwrap();
    let kind = FileKind::from_path(&p).unwrap();
    let t = Instant::now();
    let cst = nx_core::reader::parse(&src);
    eprintln!("parse {:?} ({} bytes)", t.elapsed(), src.len());
    let defs = DefsIndex::default();
    let t = Instant::now();
    let fa = analyze_cst(cst, kind, &Config::default(), &defs, Options::external());
    eprintln!("analyze {:?}", t.elapsed());
    drop(fa);
}
