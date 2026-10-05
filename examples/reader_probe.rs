// PERF-PROBE scratch: loop-read a corpus file many times so `sample`(1)
// has something to attach to. Not part of the product; throwaway.
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let iters: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let src = std::fs::read_to_string(&path).expect("read file");
    let mut total_forms = 0usize;
    let t0 = Instant::now();
    for _ in 0..iters {
        let mut rest: &str = &src;
        let mut forms = Vec::new();
        while let Some(form) = mova::internal::reader::read_one_user(rest).expect("parse") {
            let end = form.span.end;
            forms.push(form);
            rest = &rest[end.min(rest.len())..];
            if rest.is_empty() {
                break;
            }
        }
        total_forms += forms.len();
        std::hint::black_box(&forms);
    }
    let dt = t0.elapsed();
    eprintln!(
        "iters={iters} bytes={} total={:?} per_iter={:?} MB/s={:.1} forms={}",
        src.len(),
        dt,
        dt / iters as u32,
        (src.len() * iters) as f64 / dt.as_secs_f64() / 1e6,
        total_forms
    );
}
