//! S4 (everyday3): `rand rand-int rand-nth shuffle`. Non-deterministic by
//! nature -- there is no shared-seed contract with real Clojure to match
//! bit-for-bit (Clojure itself doesn't guarantee one either: `rand` goes
//! through `Math/random()`, an unseeded, JVM-global generator), so the
//! measured conformance surface here is PROPERTIES (range, type, "throws
//! on empty"), never exact values. See tests/conformance/corpus/
//! everyday3.corpus's `;; rand family` section.
//!
//! Deliberately not a real `rand` crate dependency (none is in
//! Cargo.toml, same call `builtins::async::next_rand` already made for
//! `alts!!`'s randomized op order) -- a tiny thread-local xorshift64 is
//! plenty for "give me a number nobody can predict", the entire
//! requirement here.
//!
//! L1/W4: `go` bodies are tasks now, multiplexed onto a handful of shard
//! threads (`crate::runtime`), and this seed is thread-local, not
//! task-local -- every task that lands on the same shard shares that
//! shard's PRNG stream instead of getting one of its own. Harmless (the
//! conformance surface above is properties, not a reproducible sequence)
//! and deliberately not fixed: a per-task seed would cost a fresh
//! `SystemTime` read (or a scheduler-plumbed seed) on every task spawn for
//! a guarantee `rand` doesn't make in real Clojure either.

use crate::builtins::collections::materialize;
use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::Value;

/// Thread-local xorshift64, seeded from the wall clock on first use.
/// Deliberately independent of `builtins::async::next_rand` (that one's
/// `pub(crate)`-invisible to this module, and sharing state across two
/// unrelated randomness consumers buys nothing).
///
/// L5/W5 kernel fix: the cell also carries the `clock::sim_call_gen()` this
/// state was last seeded at. `sim_call_begin` re-seeds the shared USER
/// stream per `simulate` call but cannot reach into this thread's TLS to
/// reset it directly, so in SIM MODE ONLY the generation is checked on every
/// draw and the state is re-seeded whenever it has moved on -- see
/// `clock::SIM_CALL_GEN` for the full rationale. Real mode is unchanged: the
/// gen check is behind the same `sim_enabled()` branch the old lazy-seed
/// check already used, so it costs nothing new there.
fn next_u64() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    }
    STATE.with(|s| {
        let (mut gen, mut x) = s.get();
        if crate::clock::sim_enabled() {
            let cur_gen = crate::clock::sim_call_gen();
            if x == 0 || gen != cur_gen {
                // L5/W3 fence #8 (design §4): in sim this thread-local
                // starts from a draw on the seeded USER stream, so `(rand)`
                // and friends replay exactly. Deliberately a DRAW and not a
                // constant derived from the seed: two thread-locals (or two
                // shard threads under `MOVA_SHARDS>1`) must not be handed
                // the same sequence. See `clock::user_next`.
                x = crate::clock::user_next_nonzero();
                gen = cur_gen;
            }
        } else if x == 0 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x2545_F491_4F6C_DD1D);
            // Mix in the thread id's hash too -- two threads racing to
            // seed at "the same" nanosecond (a realistic pmap/future
            // scenario) would otherwise start from identical state and
            // produce identical sequences.
            let tid_mix = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                std::thread::current().id().hash(&mut h);
                h.finish()
            };
            x = (nanos ^ tid_mix) | 1; // must be nonzero for xorshift
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set((gen, x));
        x
    })
}

/// A uniform `f64` in `[0, 1)` with full 53-bit mantissa precision, same
/// shape as `java.util.Random#nextDouble`/`Math.random()`.
fn next_f64() -> f64 {
    let bits = next_u64() >> 11; // top 53 bits
    (bits as f64) * (1.0 / (1u64 << 53) as f64)
}

fn expect_positive_len(n: usize, op: &str) -> Result<(), RjError> {
    if n == 0 {
        Err(RjError::other(format!("{op}: index out of bounds for empty collection")))
    } else {
        Ok(())
    }
}

pub fn register(i: &mut Interp) {
    reg(i, "rand", ArityHint::Range(0, 1), |_i, args| {
        let r = next_f64();
        Ok(match args.first() {
            None => Value::Float(r),
            Some(Value::Int(n)) => Value::Float(*n as f64 * r),
            Some(Value::Float(n)) => Value::Float(*n * r),
            Some(other) => {
                return Err(RjError::type_err(format!("rand: expected a number, got {}", other.type_name())))
            }
        })
    });

    reg(i, "rand-int", ArityHint::Exact(1), |_i, args| {
        let n = match &args[0] {
            Value::Int(n) => *n as f64,
            Value::Float(n) => *n,
            other => {
                return Err(RjError::type_err(format!(
                    "rand-int: expected a number, got {}",
                    other.type_name()
                )))
            }
        };
        Ok(Value::Int((n * next_f64()) as i64))
    });

    reg(i, "rand-nth", ArityHint::Exact(1), |interp, args| {
        let items = materialize(interp, &args[0])?;
        expect_positive_len(items.len(), "rand-nth")?;
        let idx = (next_u64() as usize) % items.len();
        Ok(items[idx].clone())
    });

    reg(i, "shuffle", ArityHint::Exact(1), |interp, args| {
        let mut items = materialize(interp, &args[0])?;
        for idx in (1..items.len()).rev() {
            let j = (next_u64() as usize) % (idx + 1);
            items.swap(idx, j);
        }
        Ok(Value::Vector(items.into_iter().collect()))
    });
}
