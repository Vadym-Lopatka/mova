//! fmt_range_cmp <in-root> <range-oracle-root>: compare fmt::format_range_pos with tools/fmt_oracle/range.clj outputs.
use nx_core::fmt::{self, FmtConfig};
use std::path::PathBuf;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (inr, outr) = (PathBuf::from(&a[1]), PathBuf::from(&a[2]));
    let cfg = FmtConfig::default();
    let tsv = std::fs::read_to_string(outr.join("ranges.tsv")).unwrap();
    let (mut ok, mut bad, mut both_err) = (0, 0, 0);
    for line in tsv.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let (rel, i) = (f[0], f[1]);
        let n: Vec<u32> = f[2..6].iter().map(|x| x.parse().unwrap()).collect();
        let Ok(src) = std::fs::read_to_string(inr.join(rel)) else { continue };
        let got = fmt::format_range_pos(&src, n[0], n[1], n[2], n[3], &cfg);
        let exp_path = outr.join(format!("{}.r{}", rel, i));
        if !exp_path.exists() {
            if got.is_empty() { both_err += 1 } else { bad += 1; println!("MISMATCH (oracle error) {} r{} {:?}", rel, i, n) }
            continue;
        }
        let exp = std::fs::read_to_string(&exp_path).unwrap();
        let (hdr, text) = exp.split_once('\n').unwrap();
        let r: Vec<u32> = hdr.split(' ').map(|x| x.parse().unwrap()).collect();
        let good = got.len() == 1
            && [got[0].start_line, got[0].start_col, got[0].end_line, got[0].end_col] == [r[0], r[1], r[2], r[3]]
            && got[0].new_text == text;
        if good { ok += 1 } else {
            bad += 1;
            if bad <= 5 {
                println!("MISMATCH {} r{} {:?}: got {:?} exp {:?}", rel, i, n, got.first().map(|e| (e.start_line, e.start_col, e.end_line, e.end_col, e.new_text.len())), (hdr, text.len()));
            }
        }
    }
    println!("ranges ok={} bad={} both_err={}", ok, bad, both_err);
}
