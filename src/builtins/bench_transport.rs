//! V05-PERF-PLAN.md E2 probe: does a dedicated 1:1 transport (SPSC ring +
//! spin-then-park, or a single-slot direct handoff) beat the general
//! `Chan` (`Mutex` + `Condvar`, see `super::chan_put`/`chan_take`) by
//! enough to justify building it into the flow engine? **Measurement
//! only** -- nothing here is wired into the engine; these are `#[ignore]`d
//! tracking probes, following the exact precedent of `src/compile/bench.rs`
//! (see its header for the pattern this file reuses, incl. `report`'s
//! layout). They live inside `builtins` (rather than a crate-root module)
//! because `chan_put`/`chan_take` are `pub(crate)` to `r#async`, and
//! `r#async` itself is a private sibling module -- only a descendant of
//! `builtins` can name that path.
//!
//! ```text
//! cargo test --release --lib bench_transport -- --ignored --nocapture
//! ```
//!
//! Run in `--release`: a debug build measures `rustc -O0` spin loops, not
//! the transport.
//!
//! ## The transports
//!
//! Baselines and probe prototypes:
//!
//! 1. Current `Chan`, `BufferPolicy::Unbuffered` -- the general engine
//!    transport, rendezvous mode.
//! 2. Current `Chan`, `BufferPolicy::Fixed(1024)` -- the flow engine's
//!    common conn shape, and the one-way throughput bar to beat.
//! 3. `proto spsc ring-1024` -- this file's prototype: a bounded
//!    single-producer/single-consumer ring, lock-free `AtomicUsize`
//!    head/tail, spin ~4000 iterations then `thread::park`, with the
//!    always-lock-the-mutex `Waiter` below.
//! 4. `proto handoff` -- this file's prototype single-slot rendezvous,
//!    modeled on jank-v3's direct handoff.
//!
//! The production kernel (`src/transport.rs`), added after the probe:
//!
//! 5. `kernel spsc ring-1024` / `kernel spsc ring-1` -- the shipped
//!    `SpscRing` at the common conn capacity and at the degenerate
//!    capacity-1 shape.
//! 6. `kernel handoff` -- the shipped `Handoff`.
//!
//! The kernel rows exist to answer one question the probe left open: the
//! prototypes' `Waiter` takes a mutex on EVERY push and pop, which cost them
//! 3x on one-way throughput against buffered `Chan`. The kernel replaces
//! that with a single-word RMW wake protocol (see `crate::transport`'s
//! module doc). Keeping the prototypes in the same run is what makes the
//! difference attributable to the protocol rather than to the machine.
//!
//! ## Waiter accounting (read the SCAR TISSUE note before changing this)
//!
//! `bench/optimization-log.md` records a real regression: a "skip the wake
//! when no one's waiting" optimization built on `Chan`'s
//! `waiting_takers`/`waiting_putters` *counters* 2x-regressed fanout5,
//! because a counter can go stale between "peer decided to park" and
//! "waker checked the counter" -- it answers "how many, historically" not
//! "is someone about to block right now". `Waiter` below is not a counter:
//! it is a `Mutex<Option<Thread>>` holding the actual parked `Thread`
//! handle, and the protocol is register-THEN-recheck-THEN-park (see
//! `Waiter::register`'s doc). Combined with `thread::park`'s own per-thread
//! wake token (an `unpark()` that lands before the matching `park()` is
//! never lost -- it latches, so `park()` just returns immediately), there
//! is no interleaving that drops a wakeup: either the recheck (after
//! registering) already observes the peer's progress and skips the park
//! entirely, or the peer's `wake()` observes the registered `Thread` and
//! unparks it, or -- if `wake()` ran and grabbed the slot before `park()`
//! was called -- the token it set is still there waiting for us. This is
//! complete by construction, not by auditing every call site.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use super::r#async::{chan_put, chan_take};
use crate::sync::lock_mutex;
use crate::transport::{
    Handoff as KHandoff, HandoffRx, HandoffTx, SpscRing as KSpscRing, SpscRx, SpscTx,
};
use crate::value::{BufferPolicy, Chan, Value};

/// Spin budget before a producer/consumer gives up and parks. Chosen per
/// the plan (~4000 `spin_loop` hints) to cover a cross-core cache-line
/// exchange (tens to low hundreds of ns) many times over without ever
/// reaching the park/unpark syscall in the steady-state common case.
const SPIN_LIMIT: u32 = 4000;

/// SPSC ring capacity; must be a power of two (index masking below relies
/// on it). Matches the flow engine's common `Fixed(1024)` conn shape so
/// transport 2 and transport 3 are apples-to-apples.
const RING_CAP: usize = 1024;
const RING_MASK: usize = RING_CAP - 1;

const ONE_WAY_N: u64 = 2_000_000;
const PINGPONG_N: u64 = 1_000_000;

/// 1 discarded warmup round + 5 measured rounds, per the v0.4 campaign
/// discipline (bench/optimization-log.md): report `[min, max]`, claim a
/// winner only on non-overlapping ranges.
const ROUNDS: usize = 6;

// ---------------------------------------------------------------------------
// Waiter: the register/recheck/park building block shared by SpscRing and
// Handoff. See the module doc's "Waiter accounting" section for the
// completeness argument.
// ---------------------------------------------------------------------------

struct Waiter(Mutex<Option<Thread>>);

impl Waiter {
    fn new() -> Self {
        Waiter(Mutex::new(None))
    }

    /// Registers the CURRENT thread as the one to wake. Callers must call
    /// this, then re-check the wait condition, and only `thread::park()`
    /// if the recheck still says "not ready" -- see the module doc.
    fn register(&self) {
        *lock_mutex(&self.0) = Some(thread::current());
    }

    /// Takes and unparks whoever is registered, if anyone. A `wake()` with
    /// no registered waiter is a (harmless) no-op -- the peer hasn't
    /// reached `register()` yet, which per the protocol means it will
    /// observe our progress on its own post-registration recheck instead.
    ///
    /// ATTEMPTED AND REVERTED: an earlier version added an `AtomicBool`
    /// "is anyone parked" hint, checked (lock-free) before bothering to
    /// take `0`'s mutex, to avoid a lock acquisition on every single
    /// push/pop in the common case where nobody is parked. It looked
    /// race-free by the same register-then-recheck argument used
    /// elsewhere in this file -- and is exactly the kind of "safe-looking
    /// wake elision" the SCAR TISSUE note warns about. Under the
    /// ping-pong stress bench it deadlocked (a real hang, confirmed via
    /// `sample` showing both threads parked with 0% CPU, not merely a
    /// slow run) after passing the one-way throughput bench cleanly first
    /// -- proof the failure mode needs sustained register/wake churn to
    /// surface, not a one-shot smoke test. Root cause not fully isolated
    /// before the revert (leading theory: `wake()` can observe a stale
    /// registration belonging to an EARLIER wait cycle that the waiter's
    /// own recheck had already resolved without parking, taking that
    /// stale slot -- and its `unpark()` token -- out from under a BRAND
    /// NEW registration for the waiter's NEXT wait cycle). Reverted to
    /// this always-lock-the-mutex form, which is simple enough to be
    /// obviously correct and was the form all four transports' numbers
    /// below were actually measured with. See bench/optimization-log.md's
    /// E2 entry.
    ///
    /// **That theory was wrong, and the real cause is now known** -- see
    /// `crate::transport`'s module doc. Putting "is anyone parked" in its
    /// own location makes (fill -> check waiter) and (check empty -> park)
    /// a store-buffer (Dekker) pattern across two locations, which
    /// Acquire/Release does not close on AArch64: both loads may return the
    /// stale value, the producer skips the wake, and the waiter parks on
    /// data that is already there. The shipped kernel removes the race
    /// rather than ordering around it, by packing `PARKED` into the same
    /// word the peer already updates with an RMW. This prototype is kept
    /// AS IS, mutex and all, purely as an in-run bench control: it is the
    /// thing the kernel is measured against.
    fn wake(&self) {
        if let Some(t) = lock_mutex(&self.0).take() {
            t.unpark();
        }
    }
}

// ---------------------------------------------------------------------------
// `CachePadded`: pins the wrapped value to its own 64-byte (Apple Silicon
// and x86_64 both use 64B lines; some ARM cores use 128B, but 64B is the
// standard conservative choice) cache line. `head`/`tail` (ring) and
// `full` (handoff) are each written by exactly ONE side and read by the
// OTHER on every single operation -- if two independently-written hot
// atomics share a line, every write forces the other core to refetch
// BOTH, even the field it didn't touch (classic false sharing). Measured
// impact on this exact bench: padding `head`/`tail` roughly DOUBLED SPSC
// one-way throughput (see bench/optimization-log.md's E2 entry).
// ---------------------------------------------------------------------------

#[repr(align(64))]
struct CachePadded<T>(T);

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> CachePadded<T> {
    fn new(v: T) -> Self {
        CachePadded(v)
    }
}

// ---------------------------------------------------------------------------
// Transport 3: bounded SPSC ring.
// ---------------------------------------------------------------------------

/// A single producer, single consumer, fixed-capacity ring of `Value`.
/// `head`/`tail` are monotonically increasing counters (never wrapped
/// themselves -- only their `& RING_MASK` projection indexes `buf`), so
/// there is no ABA hazard across wraparounds.
///
/// # Safety invariant (for the `unsafe` slot accesses below)
///
/// Exactly one thread ever calls `push` (writes `tail`, calls
/// `.write()` on a slot) and exactly one thread ever calls `pop` (writes
/// `head`, calls `.assume_init_read()` on a slot) for a given ring. `push`
/// only writes slot `tail & MASK` when `tail - head < RING_CAP`, i.e. only
/// into a slot the consumer has already vacated (its `pop` of the PREVIOUS
/// occupant of that slot index happened, provably, `RING_CAP` pushes ago).
/// `pop` only reads slot `head & MASK` when `head != tail`, i.e. only a
/// slot the producer has already initialized. The producer's `tail.store`
/// uses `Release` and the consumer's matching `tail.load` uses `Acquire`
/// (and symmetrically for `head`), so each write of a slot
/// happens-before the read that observes it, and each read
/// happens-before the write that reuses that slot index. That is the
/// standard SPSC ring correctness argument; no other synchronization is
/// needed for the `UnsafeCell` accesses to be data-race-free.
struct SpscRing {
    buf: Box<[UnsafeCell<MaybeUninit<Value>>]>,
    // Each on its own cache line (see `CachePadded`'s doc): the producer
    // writes `tail`+reads `head`, the consumer writes `head`+reads `tail`,
    // every single push/pop -- sharing a line between them would force
    // every op to bounce the whole line cross-core.
    head: CachePadded<AtomicUsize>, // next index to read; only the consumer writes it
    tail: CachePadded<AtomicUsize>, // next index to write; only the producer writes it
    consumer_waiter: Waiter,
    producer_waiter: Waiter,
}

// SAFETY: `UnsafeCell<MaybeUninit<Value>>` slots are only ever touched
// through `push`/`pop`, which the struct-level doc comment proves is
// data-race-free for a single producer + single consumer.
unsafe impl Sync for SpscRing {}

impl SpscRing {
    fn new() -> Self {
        let mut v = Vec::with_capacity(RING_CAP);
        v.resize_with(RING_CAP, || UnsafeCell::new(MaybeUninit::uninit()));
        SpscRing {
            buf: v.into_boxed_slice(),
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
            consumer_waiter: Waiter::new(),
            producer_waiter: Waiter::new(),
        }
    }

    fn push(&self, v: Value) {
        let mut spins = 0u32;
        let mut v = v;
        loop {
            let tail = self.tail.load(Ordering::Relaxed);
            let head = self.head.load(Ordering::Acquire);
            if tail - head < RING_CAP {
                // SAFETY: see struct doc -- this slot was last read (or
                // never written) by the consumer, whose `head` advance we
                // just observed via Acquire, so it is safe to overwrite.
                unsafe { (*self.buf[tail & RING_MASK].get()).write(v) };
                self.tail.store(tail + 1, Ordering::Release);
                self.consumer_waiter.wake();
                return;
            }
            v = self.wait_for_room(v, tail, &mut spins);
        }
    }

    /// Extracted so `push`'s hot loop stays small; returns `v` back to the
    /// caller once room MIGHT be available (caller re-checks).
    fn wait_for_room(&self, v: Value, tail: usize, spins: &mut u32) -> Value {
        if *spins < SPIN_LIMIT {
            *spins += 1;
            std::hint::spin_loop();
            return v;
        }
        self.producer_waiter.register();
        // Recheck AFTER registering (see module doc): if the consumer
        // already made room, skip the park -- we'd otherwise risk racing
        // a `wake()` that happened before we registered.
        if tail - self.head.load(Ordering::Acquire) < RING_CAP {
            return v;
        }
        thread::park();
        *spins = 0;
        v
    }

    fn pop(&self) -> Value {
        let mut spins = 0u32;
        loop {
            let head = self.head.load(Ordering::Relaxed);
            let tail = self.tail.load(Ordering::Acquire);
            if head != tail {
                // SAFETY: see struct doc -- this slot was written by the
                // producer, whose `tail` advance we just observed via
                // Acquire, so it is safe to read (and it is initialized).
                let val = unsafe { (*self.buf[head & RING_MASK].get()).assume_init_read() };
                self.head.store(head + 1, Ordering::Release);
                self.producer_waiter.wake();
                return val;
            }
            if spins < SPIN_LIMIT {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            self.consumer_waiter.register();
            if head != self.tail.load(Ordering::Acquire) {
                continue;
            }
            thread::park();
            spins = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Transport 4: direct handoff (jank-v3-style single-slot rendezvous).
// ---------------------------------------------------------------------------

/// A single-slot rendezvous: `put` blocks until the slot is empty, writes,
/// wakes the consumer, THEN blocks again until the consumer has drained it
/// (so the producer never overwrites an unread value and a `put` return
/// means "durably handed off", matching `chan_put`'s `Unbuffered`
/// contract). `full` is the single source of truth for slot occupancy.
///
/// # Safety invariant
///
/// Only the producer thread writes the slot (in `put`, guarded by
/// `!full`) and only the consumer thread reads it (in `take`, guarded by
/// `full`); `full`'s `Release` store after the write / `Acquire` load
/// before the read (and symmetrically for the drain) makes the write
/// happen-before the read and the read happen-before the next write, the
/// same single-slot argument as `Chan`'s `Unbuffered` mode already relies
/// on under its `Mutex`, just without the `Mutex`.
struct Handoff {
    slot: UnsafeCell<MaybeUninit<Value>>,
    full: CachePadded<AtomicBool>,
    consumer_waiter: Waiter,
    producer_waiter: Waiter,
}

// SAFETY: see struct doc -- single producer, single consumer, `full`
// gates every access to `slot` with Acquire/Release.
unsafe impl Sync for Handoff {}

impl Handoff {
    fn new() -> Self {
        Handoff {
            slot: UnsafeCell::new(MaybeUninit::uninit()),
            full: CachePadded::new(AtomicBool::new(false)),
            consumer_waiter: Waiter::new(),
            producer_waiter: Waiter::new(),
        }
    }

    fn spin_then_park_until(&self, waiter: &Waiter, mut ready: impl FnMut() -> bool) {
        let mut spins = 0u32;
        loop {
            if ready() {
                return;
            }
            if spins < SPIN_LIMIT {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            waiter.register();
            if ready() {
                return;
            }
            thread::park();
            spins = 0;
        }
    }

    fn put(&self, v: Value) {
        // Wait for the slot to be empty (previous value fully consumed).
        self.spin_then_park_until(&self.producer_waiter, || !self.full.load(Ordering::Acquire));
        // SAFETY: `full` was observed false with Acquire, and only the
        // producer writes the slot, so this write cannot race a read.
        unsafe { (*self.slot.get()).write(v) };
        self.full.store(true, Ordering::Release);
        self.consumer_waiter.wake();
        // Wait for the consumer to drain it before returning, matching
        // `chan_put`'s `Unbuffered` "durably handed off" contract.
        self.spin_then_park_until(&self.producer_waiter, || !self.full.load(Ordering::Acquire));
    }

    fn take(&self) -> Value {
        self.spin_then_park_until(&self.consumer_waiter, || self.full.load(Ordering::Acquire));
        // SAFETY: `full` was observed true with Acquire, and only the
        // consumer reads the slot, so this read observes an initialized
        // value written by the producer's happens-before `put`.
        let val = unsafe { (*self.slot.get()).assume_init_read() };
        self.full.store(false, Ordering::Release);
        self.producer_waiter.wake();
        val
    }
}

// ---------------------------------------------------------------------------
// Transport abstraction: lets the bench harness below be written once and run
// against every transport.
//
// The two production transports (`crate::transport`) hand back non-`Clone`,
// `!Sync` `Tx`/`Rx` halves on purpose (that is what makes their SPSC
// precondition a compile-time fact), so the harness cannot share one `Arc`
// between both threads the way it can for the prototypes and `Chan`. It
// therefore works in terms of a *pair* of one-directional endpoints: a
// `Sender` moved into the producer thread and a `Receiver` moved into the
// consumer thread. `Chan` and the prototypes simply hand out two clones of
// the same `Arc`.
// ---------------------------------------------------------------------------

trait Sender: Send {
    fn send(&self, v: Value);
}

trait Receiver: Send {
    fn recv(&self) -> Value;
}

/// One transport under test: a name plus a factory producing a fresh
/// producer/consumer endpoint pair per round.
type Endpoints = (Box<dyn Sender>, Box<dyn Receiver>);
type Factory = Box<dyn Fn() -> Endpoints>;

/// `Arc<T>` endpoint pair for the transports whose type does not enforce
/// SPSC itself (`Chan`, and the two probe prototypes).
struct Shared<T>(Arc<T>);

impl Sender for Shared<Chan> {
    fn send(&self, v: Value) {
        assert!(chan_put(&self.0, v), "chan_put failed: channel closed unexpectedly");
    }
}
impl Receiver for Shared<Chan> {
    fn recv(&self) -> Value {
        chan_take(&self.0).expect("chan_take: channel closed unexpectedly")
    }
}
impl Sender for Shared<SpscRing> {
    fn send(&self, v: Value) {
        self.0.push(v);
    }
}
impl Receiver for Shared<SpscRing> {
    fn recv(&self) -> Value {
        self.0.pop()
    }
}
impl Sender for Shared<Handoff> {
    fn send(&self, v: Value) {
        self.0.put(v);
    }
}
impl Receiver for Shared<Handoff> {
    fn recv(&self) -> Value {
        self.0.take()
    }
}

fn shared<T: Send + Sync + 'static>(make: impl Fn() -> T + 'static) -> Factory
where
    Shared<T>: Sender + Receiver,
{
    Box::new(move || {
        let a = Arc::new(make());
        (Box::new(Shared(Arc::clone(&a))), Box::new(Shared(a)))
    })
}

// -- the production kernel -------------------------------------------------

impl Sender for SpscTx {
    fn send(&self, v: Value) {
        assert!(self.put(v), "SpscTx::put failed: closed unexpectedly");
    }
}
impl Receiver for SpscRx {
    fn recv(&self) -> Value {
        self.take().expect("SpscRx::take: closed unexpectedly")
    }
}
impl Sender for HandoffTx {
    fn send(&self, v: Value) {
        assert!(self.put(v), "HandoffTx::put failed: closed unexpectedly");
    }
}
impl Receiver for HandoffRx {
    fn recv(&self) -> Value {
        self.take().expect("HandoffRx::take: closed unexpectedly")
    }
}

fn kernel_ring(cap: usize) -> Factory {
    Box::new(move || {
        let (tx, rx) = KSpscRing::channel(cap);
        (Box::new(tx), Box::new(rx))
    })
}

fn kernel_ring_spin(cap: usize, spin: u32) -> Factory {
    Box::new(move || {
        let (tx, rx) = KSpscRing::channel_with_spin(cap, spin);
        (Box::new(tx), Box::new(rx))
    })
}

fn kernel_handoff() -> Factory {
    Box::new(|| {
        let (tx, rx) = KHandoff::channel();
        (Box::new(tx), Box::new(rx))
    })
}

// ---------------------------------------------------------------------------
// Measurement harness: 1 discarded warmup + 5 measured rounds, report
// [min, max], following bench/optimization-log.md's discipline.
// ---------------------------------------------------------------------------

fn measured_rounds(mut round: impl FnMut() -> Duration) -> Vec<Duration> {
    let mut v = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        v.push(round());
    }
    v.remove(0); // discard warmup
    v
}

fn report_one_way(label: &str, n: u64, rounds: &[Duration]) {
    let mut msg_per_s: Vec<f64> = rounds.iter().map(|d| n as f64 / d.as_secs_f64()).collect();
    let mut ns_per_msg: Vec<f64> = rounds.iter().map(|d| d.as_nanos() as f64 / n as f64).collect();
    msg_per_s.sort_by(|a, b| a.total_cmp(b));
    ns_per_msg.sort_by(|a, b| a.total_cmp(b));
    println!(
        "{label:24} one-way   msg/s [{:>13.0}, {:>13.0}]   ns/msg [{:>7.1}, {:>7.1}]",
        msg_per_s[0],
        msg_per_s[msg_per_s.len() - 1],
        ns_per_msg[0],
        ns_per_msg[ns_per_msg.len() - 1],
    );
}

fn report_pingpong(label: &str, round_trips: u64, rounds: &[Duration]) {
    // RTT/2 as ns/hop: each round trip is two hops (there and back).
    let mut ns_per_hop: Vec<f64> =
        rounds.iter().map(|d| d.as_nanos() as f64 / (round_trips as f64 * 2.0)).collect();
    ns_per_hop.sort_by(|a, b| a.total_cmp(b));
    println!(
        "{label:24} ping-pong ns/hop (RTT/2) [{:>7.1}, {:>7.1}]",
        ns_per_hop[0],
        ns_per_hop[ns_per_hop.len() - 1],
    );
}

fn bench_one_way(make: &Factory, label: &str) {
    let rounds = measured_rounds(|| {
        let (tx, rx) = make();
        let consumer = thread::spawn(move || {
            let mut acc: i64 = 0;
            let mut count: u64 = 0;
            for _ in 0..ONE_WAY_N {
                if let Value::Int(i) = rx.recv() {
                    acc = acc.wrapping_add(i);
                }
                count += 1;
            }
            std::hint::black_box(acc);
            count
        });
        let t0 = Instant::now();
        for i in 0..ONE_WAY_N {
            tx.send(Value::Int(std::hint::black_box(i as i64)));
        }
        let received = consumer.join().expect("consumer thread panicked");
        let elapsed = t0.elapsed();
        assert_eq!(received, ONE_WAY_N, "{label}: message count mismatch");
        elapsed
    });
    report_one_way(label, ONE_WAY_N, &rounds);
}

fn bench_pingpong(make: &Factory, label: &str) {
    let rounds = measured_rounds(|| {
        let (ping_tx, ping_rx) = make(); // ping -> pong
        let (pong_tx, pong_rx) = make(); // pong -> ping
        let pong = thread::spawn(move || {
            for _ in 0..PINGPONG_N {
                let v = ping_rx.recv();
                pong_tx.send(v);
            }
        });
        let t0 = Instant::now();
        let mut v = Value::Int(0);
        for _ in 0..PINGPONG_N {
            ping_tx.send(v);
            v = pong_rx.recv();
        }
        let elapsed = t0.elapsed();
        pong.join().expect("pong thread panicked");
        std::hint::black_box(&v);
        elapsed
    });
    report_pingpong(label, PINGPONG_N, &rounds);
}

/// Every row, in one process, in a fixed order -- no interleaving across
/// binaries (bench/optimization-log.md's discipline). The two `kernel *` rows
/// are `src/transport.rs`; the two bare rows above them are this file's
/// probe prototypes, kept so the kernel's cost is measured against the thing
/// it replaced rather than against a memory of it.
fn all_transports() -> Vec<(&'static str, Factory)> {
    vec![
        ("chan unbuffered", shared(|| Chan::new(BufferPolicy::Unbuffered))),
        ("chan buffered-1024", shared(|| Chan::new(BufferPolicy::Fixed(1024)))),
        ("proto spsc ring-1024", shared(SpscRing::new)),
        ("proto handoff", shared(Handoff::new)),
        ("kernel spsc ring-1024", kernel_ring(1024)),
        ("kernel spsc ring-1", kernel_ring(1)),
        ("kernel handoff", kernel_handoff()),
    ]
}

#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib bench_transport -- --ignored --nocapture"]
fn bench_transport_one_way() {
    for (label, make) in all_transports() {
        bench_one_way(&make, label);
    }
}

#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib bench_transport -- --ignored --nocapture"]
fn bench_transport_pingpong() {
    for (label, make) in all_transports() {
        bench_pingpong(&make, label);
    }
}

/// Spin-budget sweep for the production kernel: the spin count is the
/// wake protocol's main tuning knob, and "the probe used 4000" is not a
/// measurement. Reports both metrics at each budget so the latency/throughput
/// tradeoff is visible rather than assumed.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib bench_transport -- --ignored --nocapture"]
fn bench_transport_spin_sweep() {
    for spin in [0u32, 50, 500, 2000, 4000, 16000] {
        let label = format!("kernel ring-1024 spin={spin}");
        let make = kernel_ring_spin(1024, spin);
        bench_one_way(&make, &label);
        bench_pingpong(&make, &label);
    }
}

