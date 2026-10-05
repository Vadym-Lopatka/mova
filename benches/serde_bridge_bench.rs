//! Manual (no `criterion` dependency) release-mode timing harness for the
//! `serde` embedding bridge (`mova::serde_bridge::{to_value, from_value}`),
//! measured against a `serde_json` baseline over the SAME fixtures as a
//! familiar reference point. `required-features = ["serde"]` in
//! `Cargo.toml` keeps this target out of a plain `cargo build`/`cargo bench`
//! entirely.
//!
//! Run with: `cargo bench --features serde --bench serde_bridge_bench`
//! (implies `--release` via the `bench` profile; see `Cargo.toml`'s
//! `[profile.release]` for the `lto = true` / `codegen-units = 1` settings
//! that also apply here).
//!
//! Methodology: for every (fixture, op) pair, 1 warmup batch + 5 measured
//! batches, each batch running `iters` repetitions back-to-back and
//! dividing total elapsed time by `iters`; the report is the MEDIAN of the
//! 5 batch ns/op numbers (matches this repo's own `bench/run.sh`
//! methodology for the flow benches -- see `bench/RESULTS.md`).
//! `std::hint::black_box` pins both the input and the freshly-built output
//! at each call site so the optimizer can't hoist the "same fixture every
//! iteration" call out of the loop or elide the unread result.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use mova::serde_bridge::{from_value, to_value};
use mova::internal::Value;

// ------------------------------- fixtures ---------------------------------

#[derive(Clone, Serialize, Deserialize)]
struct FlatStruct {
    id: u64,
    name: String,
    score: f64,
    active: bool,
    count: i32,
    ratio: f32,
    tag: char,
    weight: i64,
    note: String,
    verified: bool,
}

fn flat_fixture() -> FlatStruct {
    FlatStruct {
        id: 42,
        name: "widget-9000".to_string(),
        score: 98.6,
        active: true,
        count: -17,
        ratio: 0.5,
        tag: 'Q',
        weight: 123_456_789,
        note: "hello, world".to_string(),
        verified: false,
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct LineItem {
    sku: String,
    qty: i32,
    unit_price: f64,
    gift_wrapped: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct UnitMarker;

#[derive(Clone, Serialize, Deserialize)]
enum Status {
    #[allow(dead_code)]
    Pending,
    #[allow(dead_code)]
    Cancelled(String),
    #[allow(dead_code)]
    Delayed(u32, String),
    Shipped {
        carrier: String,
        tracking: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct OrderEvent {
    order_id: u64,
    customer: String,
    total: f64,
    paid: bool,
    discount_code: Option<String>,
    gift_note: Option<String>,
    tags: Vec<String>,
    items: Vec<LineItem>,
    attributes: HashMap<String, String>,
    status: Status,
    marker: UnitMarker,
    warehouse_coords: (f64, f64, f64),
}

/// ~50 leaf values once flattened (10 flat fields + 3 tags + 3 items * 4
/// fields + 3 attribute entries * 2 + 2 status leaves + 3 tuple leaves +
/// order-level scalars), matching the perf brief's "(b) nested fixture"
/// shape.
fn nested_fixture() -> OrderEvent {
    let mut attributes = HashMap::new();
    attributes.insert("source".to_string(), "web".to_string());
    attributes.insert("campaign".to_string(), "spring-sale".to_string());
    attributes.insert("referrer".to_string(), "newsletter".to_string());
    OrderEvent {
        order_id: 908_123,
        customer: "Ada Lovelace".to_string(),
        total: 249.99,
        paid: true,
        discount_code: Some("SPRING10".to_string()),
        gift_note: None,
        tags: vec!["priority".to_string(), "gift".to_string(), "fragile".to_string()],
        items: vec![
            LineItem {
                sku: "SKU-1".to_string(),
                qty: 2,
                unit_price: 19.99,
                gift_wrapped: true,
            },
            LineItem {
                sku: "SKU-2".to_string(),
                qty: 1,
                unit_price: 209.99,
                gift_wrapped: false,
            },
            LineItem {
                sku: "SKU-3".to_string(),
                qty: 4,
                unit_price: 0.01,
                gift_wrapped: false,
            },
        ],
        attributes,
        status: Status::Shipped {
            carrier: "UPS".to_string(),
            tracking: "1Z999AA10123456784".to_string(),
        },
        marker: UnitMarker,
        warehouse_coords: (37.7749, -122.4194, 16.0),
    }
}

// -------------------------------- harness ----------------------------------

const ROUNDS: usize = 5;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Runs `f` `iters` times back-to-back, returns total ns / `iters`.
fn time_ns_per_iter<F: FnMut()>(mut f: F, iters: u64) -> f64 {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

/// 1 warmup batch + [`ROUNDS`] measured batches of `iters` repetitions
/// each; returns the median ns/iter across the measured batches.
fn bench<F: FnMut()>(mut f: F, iters: u64) -> f64 {
    time_ns_per_iter(&mut f, iters); // warmup, discarded
    let samples: Vec<f64> = (0..ROUNDS).map(|_| time_ns_per_iter(&mut f, iters)).collect();
    median(samples)
}

fn report(label: &str, ns_per_op: f64) {
    println!("{label:<48} {ns_per_op:>12.1} ns");
}

fn main() {
    println!("mova serde_bridge vs serde_json -- release, median of {ROUNDS} rounds\n");

    // ---- (a) flat 10-field struct ----
    let flat = flat_fixture();
    let flat_value = to_value(&flat).unwrap();
    let flat_json = serde_json::to_string(&flat).unwrap();

    report("to_value(flat 10-field struct)", bench(|| { black_box(to_value(black_box(&flat)).unwrap()); }, 200_000));
    report("from_value(flat 10-field struct)", bench(|| { let _: FlatStruct = from_value(black_box(&flat_value)).unwrap(); }, 200_000));
    report(
        "serde_json::to_string(flat)",
        bench(|| { black_box(serde_json::to_string(black_box(&flat)).unwrap()); }, 200_000),
    );
    report(
        "serde_json::from_str(flat)",
        bench(|| { let _: FlatStruct = serde_json::from_str(black_box(&flat_json)).unwrap(); }, 200_000),
    );
    println!();

    // ---- (b) nested fixture (~50 leaves) ----
    let nested = nested_fixture();
    let nested_value = to_value(&nested).unwrap();
    let nested_json = serde_json::to_string(&nested).unwrap();

    report(
        "to_value(nested ~50-leaf fixture)",
        bench(|| { black_box(to_value(black_box(&nested)).unwrap()); }, 50_000),
    );
    report(
        "from_value(nested ~50-leaf fixture)",
        bench(|| { let _: OrderEvent = from_value(black_box(&nested_value)).unwrap(); }, 50_000),
    );
    report(
        "serde_json::to_string(nested)",
        bench(|| { black_box(serde_json::to_string(black_box(&nested)).unwrap()); }, 50_000),
    );
    report(
        "serde_json::from_str(nested)",
        bench(|| { let _: OrderEvent = serde_json::from_str(black_box(&nested_json)).unwrap(); }, 50_000),
    );
    println!();

    // ---- (c) Vec of 1000 flat structs, amortized ns/struct ----
    let vec1000: Vec<FlatStruct> = (0..1000).map(|_| flat_fixture()).collect();
    let vec1000_value: Value = to_value(&vec1000).unwrap();
    let vec1000_json = serde_json::to_string(&vec1000).unwrap();
    const VEC_ITERS: u64 = 500;
    const N: f64 = 1000.0;

    report(
        "to_value(Vec<flat>, 1000) / struct",
        bench(|| { black_box(to_value(black_box(&vec1000)).unwrap()); }, VEC_ITERS) / N,
    );
    report(
        "from_value(Vec<flat>, 1000) / struct",
        bench(|| { let _: Vec<FlatStruct> = from_value(black_box(&vec1000_value)).unwrap(); }, VEC_ITERS) / N,
    );
    report(
        "serde_json::to_string(Vec<flat>,1000) / struct",
        bench(|| { black_box(serde_json::to_string(black_box(&vec1000)).unwrap()); }, VEC_ITERS) / N,
    );
    report(
        "serde_json::from_str(Vec<flat>,1000) / struct",
        bench(|| { let _: Vec<FlatStruct> = serde_json::from_str(black_box(&vec1000_json)).unwrap(); }, VEC_ITERS) / N,
    );
}
