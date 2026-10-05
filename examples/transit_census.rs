// Census bench for the clojure-lsp campaign: decode a transit-json cache
// file and tally Value categories + string dedup opportunity. Usage:
//   transit_census <path-to-db.transit.json>
use std::collections::HashMap;
use std::time::Instant;

use mova::internal::{read_json_str, PMap, Value};

#[derive(Default)]
struct Census {
    counts: HashMap<&'static str, usize>,
    bytes: HashMap<&'static str, usize>,
    small_map_sizes: Vec<usize>,
    big_maps: usize,
    str_total: usize,
    str_bytes: usize,
    str_content: HashMap<String, usize>,
    kw_total: usize,
    kw_content: HashMap<String, usize>,
}

fn bump(c: &mut Census, k: &'static str, b: usize) {
    *c.counts.entry(k).or_insert(0) += 1;
    *c.bytes.entry(k).or_insert(0) += b;
}

fn walk(v: &Value, c: &mut Census) {
    match v {
        Value::Nil => bump(c, "nil", 0),
        Value::Bool(_) => bump(c, "bool", 1),
        Value::Int(_) => bump(c, "int", 8),
        Value::Float(_) => bump(c, "float", 8),
        Value::Char(_) => bump(c, "char", 4),
        Value::Str(s) => {
            let s = s.as_ref();
            c.str_total += 1;
            c.str_bytes += s.len();
            *c.str_content.entry(s.to_string()).or_insert(0) += 1;
            bump(c, "str", s.len());
        }
        Value::Sym(s) => bump(c, "sym", s.name.len()),
        Value::Keyword(k) => {
            let s = k.text();
            c.kw_total += 1;
            *c.kw_content.entry(s.to_string()).or_insert(0) += 1;
            bump(c, "keyword", s.len());
        }
        Value::Vector(pv) => {
            bump(c, "vector", pv.len() * 32);
            for x in pv.iter() {
                walk(x, c);
            }
        }
        Value::List(pv) => {
            bump(c, "list", pv.len() * 32);
            for x in pv.iter() {
                walk(x, c);
            }
        }
        Value::Map(m) => {
            match m {
                PMap::Small(a) => {
                    c.small_map_sizes.push(a.len());
                    bump(c, "map_small", a.len() * 64);
                }
                PMap::Big(_) => {
                    c.big_maps += 1;
                    bump(c, "map_big", m.len() * 96);
                }
                PMap::Shaped(a) => {
                    c.small_map_sizes.push(a.len());
                    bump(c, "map_shaped", a.len() * 64);
                }
            }
            for (k, val) in m.iter() {
                walk(k, c);
                walk(val, c);
            }
        }
        Value::Set(s) => {
            bump(c, "set", s.len() * 32);
            for x in s.iter() {
                walk(x, c);
            }
        }
        _ => bump(c, "other", 32),
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: transit_census <file>");
    let raw = std::fs::read_to_string(&path).expect("read");
    println!("input bytes: {}", raw.len());
    let t0 = Instant::now();
    let v = read_json_str(&raw).expect("decode");
    let decode_ms = t0.elapsed().as_millis();
    println!("decode: {decode_ms} ms");

    let mut c = Census::default();
    let t1 = Instant::now();
    walk(&v, &mut c);
    println!("walk: {} ms", t1.elapsed().as_millis());

    let mut keys: Vec<_> = c.counts.keys().copied().collect();
    keys.sort();
    println!("\n{:<12} {:>10} {:>14}", "category", "count", "est.bytes");
    for k in keys {
        println!("{:<12} {:>10} {:>14}", k, c.counts[k], c.bytes[k]);
    }
    println!(
        "\nsmall maps: {} (avg size {:.1}), big maps: {}",
        c.small_map_sizes.len(),
        c.small_map_sizes.iter().sum::<usize>() as f64 / c.small_map_sizes.len().max(1) as f64,
        c.big_maps
    );
    println!(
        "strings: {} total, {} distinct content ({:.1}x dup), {} bytes raw",
        c.str_total,
        c.str_content.len(),
        c.str_total as f64 / c.str_content.len().max(1) as f64,
        c.str_bytes
    );
    println!(
        "keywords: {} total, {} distinct content ({:.1}x dup)",
        c.kw_total,
        c.kw_content.len(),
        c.kw_total as f64 / c.kw_content.len().max(1) as f64
    );
}
