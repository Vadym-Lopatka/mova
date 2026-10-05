//! JDK index numbers: cold build ms, warm load ms, size, phys_footprint deltas (macOS `footprint`).
use nx_core::jdk;
use std::time::Instant;

fn footprint_kb() -> Option<f64> {
    let out = std::process::Command::new("footprint").arg("-p").arg(std::process::id().to_string()).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let l = s.lines().find(|l| l.contains("Footprint:"))?;
    let v: Vec<&str> = l.split_whitespace().collect();
    let p = v.iter().position(|x| *x == "Footprint:")?;
    let n: f64 = v.get(p + 1)?.parse().ok()?;
    Some(match v.get(p + 2).map(|x| x.trim_end_matches(',')) {
        Some("MB") => n * 1024.0,
        Some("GB") => n * 1024.0 * 1024.0,
        Some("KB") => n,
        Some("bytes") => n / 1024.0,
        _ => n,
    })
}

fn main() {
    let zip = jdk::resolve_zip(None).expect("jdk zip");
    let warm_only = std::env::args().any(|a| a == "--warm");
    let f0 = footprint_kb();
    if !warm_only {
        let t = Instant::now();
        let j = jdk::open_zip(&zip, std::thread::available_parallelism().map_or(2, |n| n.get()).saturating_sub(2).clamp(1, 4)).expect("build");
        println!("cold build+write+map: {:.0} ms ({} files, {} classes, {} members, {:.1} MB)", t.elapsed().as_secs_f64() * 1e3, j.file_count(), j.class_count(), j.member_count(), j.size_bytes() as f64 / 1048576.0);
        println!("footprint after cold build: {:?} KB (before {:?})", footprint_kb(), f0);
        drop(j);
    }
    let f1 = footprint_kb();
    let t = Instant::now();
    let j = jdk::open_zip(&zip, 1).expect("warm");
    println!("warm load: {:.3} ms", t.elapsed().as_secs_f64() * 1e3);
    let f2 = footprint_kb();
    let t = Instant::now();
    let c = j.class("java.util.UUID").unwrap();
    let m = j.find_member(c, "randomUUID").unwrap();
    let _ = j.member_pos(m);
    let d = j.member_doc(c, m);
    println!("first lookup + doc: {:.3} ms (doc {} bytes)", t.elapsed().as_secs_f64() * 1e3, d.map_or(0, |d| d.len()));
    let t = Instant::now();
    let n = j.classes_with_prefix("java.util.").count();
    println!("prefix java.util.: {} classes in {:.3} ms", n, t.elapsed().as_secs_f64() * 1e3);
    let f3 = footprint_kb();
    println!("footprint KB: before-load {:?} after-load {:?} after-queries {:?} (before {:?})", f1, f2, f3, f0);
}
