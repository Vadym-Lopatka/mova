//! EDN split probe (competitor review 2026-08-23, docs/EDN-BENCH-VS-EDNC):
//! where does `read-string`'s time go? edn.c is 5-8x faster end-to-end; the
//! double materialization (source -> `Form` tree -> `Value` tree) is the
//! suspected structural chunk. Phases per fast-edn corpus file:
//!
//!   A  source -> Form           (`read_one`: lexer+parser, span tree; Form
//!                                dropped inside the timed loop)
//!   B  &Form -> Value           (`form_to_value` on a pre-parsed Form;
//!                                Value dropped inside the timed loop)
//!   C  source -> Form -> Value  (the `read-string` core path, both drops
//!                                in-loop -- comparable to the script-level
//!                                numbers minus builtin/interp dispatch)
//!   D  source -> Value directly (`edn_fast::try_read_edn`, the edn/fast
//!                                branch's direct-to-Value byte-level
//!                                reader -- `None` (bail) is reported as a
//!                                dash rather than measured, since a bail
//!                                falls through to the SAME cost as phase C
//!                                plus one wasted scan, not a number worth
//!                                dressing up as this file's own result)
//!
//! Batches calibrated to >= 200ms (Instant is ns-precision but batching
//! amortizes loop overhead), median of 7, black_box fencing both ways.

use mova::internal::reader::{form_to_value, read_one_user as read_one, try_read_edn};
use std::hint::black_box;
use std::time::Instant;

/// Wave-3 mimalloc measurement: same `--features bench-mimalloc` switch as
/// `src/main.rs`'s identical block (see that file's doc for the rationale
/// -- pricing allocator front-end cost against the allocation-count diet).
/// This probe is the allocation-heaviest path in the crate per byte of
/// input (every symbol/keyword/string/collection is a fresh allocation),
/// so it's the natural place to measure mimalloc's effect on phases C/D
/// specifically, attributed against the SAME corpus and protocol as the
/// default-allocator numbers. Never enabled by default.
#[cfg(feature = "bench-mimalloc")]
#[global_allocator]
static BENCH_MIMALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;


const FILES: &[&str] = &[
    "basic_10.edn",
    "basic_100.edn",
    "basic_1000.edn",
    "basic_10000.edn",
    "basic_100000.edn",
    "keywords_10.edn",
    "keywords_100.edn",
    "keywords_1000.edn",
    "keywords_10000.edn",
    "ints_1400.edn",
    "strings_1000.edn",
    "strings_uni_250.edn",
    "nested_100000.edn",
];

fn batch<F: FnMut()>(f: &mut F, reps: u64) -> f64 {
    let t0 = Instant::now();
    for _ in 0..reps {
        f();
    }
    t0.elapsed().as_secs_f64() * 1e3
}

/// Calibrate reps to a >=200ms batch, then return the median of 7 batches
/// as microseconds per op.
fn measure<F: FnMut()>(mut f: F) -> f64 {
    let mut reps = 1u64;
    loop {
        let ms = batch(&mut f, reps);
        if ms >= 200.0 {
            break;
        }
        reps *= 2;
    }
    let mut runs: Vec<f64> = (0..7).map(|_| batch(&mut f, reps)).collect();
    runs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    runs[3] * 1000.0 / reps as f64
}

/// edn.c's roundtrip targets (µs/op, from the owner spec), matched by file
/// name -- `None` for the two files edn.c's own suite doesn't report a
/// target for (`basic_10000`, `keywords_10000`, `strings_uni_250`).
fn edn_c_target(name: &str) -> Option<f64> {
    match name {
        "basic_10.edn" => Some(0.096),
        "basic_100.edn" => Some(0.285),
        "basic_1000.edn" => Some(1.37),
        "basic_10000.edn" => None,
        "basic_100000.edn" => Some(147.6),
        "keywords_10.edn" => Some(0.211),
        "keywords_100.edn" => Some(1.45),
        "keywords_1000.edn" => Some(15.5),
        "keywords_10000.edn" => Some(197.8),
        "ints_1400.edn" => Some(22.0),
        "strings_1000.edn" => Some(14.2),
        "strings_uni_250.edn" => None,
        "nested_100000.edn" => Some(181.4),
        _ => None,
    }
}

fn main() {
    let Ok(dir) = std::env::var("MOVA_EDN_CORPUS_DIR") else {
        println!("skipped: set MOVA_EDN_CORPUS_DIR");
        return;
    };
    println!(
        "{:<22} {:>12} {:>12} {:>12} {:>8} {:>8} {:>14} {:>12} {:>10}",
        "file", "A:read us", "B:to_val us", "C:full us", "A/C %", "B/C %", "D:fast us", "edn.c us", "D/target"
    );
    for name in FILES {
        let src = std::fs::read_to_string(format!("{dir}/{name}")).expect("corpus file");
        let form = read_one(&src).ok().flatten().expect("parses");

        let a = measure(|| {
            black_box(read_one(black_box(&src)).ok().flatten());
        });
        let b = measure(|| {
            black_box(form_to_value(black_box(&form)));
        });
        let c = measure(|| {
            let fm = read_one(black_box(&src)).ok().flatten().expect("parses");
            black_box(form_to_value(&fm));
        });
        // Phase D: only measured when this file actually takes the fast
        // path (bails are a structural "not this file" result, not a
        // number -- see `edn_fast`'s doc for which corpus files bail and
        // why, mainly the string-escape files).
        let takes_fast_path = try_read_edn(&src).is_some();
        let d = if takes_fast_path {
            Some(measure(|| {
                black_box(try_read_edn(black_box(&src)));
            }))
        } else {
            None
        };

        let target = edn_c_target(name);
        let d_str = d.map(|v| format!("{v:.3}")).unwrap_or_else(|| "bail".to_string());
        let target_str = target.map(|v| format!("{v:.3}")).unwrap_or_else(|| "-".to_string());
        let ratio_str = match (d, target) {
            (Some(d), Some(t)) => format!("{:.2}x", d / t),
            _ => "-".to_string(),
        };

        println!(
            "{:<22} {:>12.3} {:>12.3} {:>12.3} {:>7.1}% {:>7.1}% {:>14} {:>12} {:>10}",
            name,
            a,
            b,
            c,
            100.0 * a / c,
            100.0 * b / c,
            d_str,
            target_str,
            ratio_str,
        );
    }
}
