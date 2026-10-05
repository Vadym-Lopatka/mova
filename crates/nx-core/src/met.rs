//! Native metrics (nx METRICS.md, M2): worker-job stage-stats and project-pass records. No Mova deps.
//! Off (the default): one atomic flag check per submit, no clock read, nothing stored.
//! On: a job costs four clock reads and ~20 relaxed atomic adds; nothing allocates, nothing blocks, nothing is lost.
//! A stage-stats cell is nx.core.latency's `S` (count, sum-us, max-us, 24 log2 buckets); the reader takes a snapshot
//! and resets it, and merges the deltas (`merge-stage`). Only nx's main thread reads, when traffic is quiet.
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

static ON: AtomicBool = AtomicBool::new(false);

/// Turns the native metrics on (the Mova binding calls it from `tap-init!`). Never turned off again.
pub fn enable() {
    ON.store(true, Relaxed);
}

#[inline]
pub fn on() -> bool {
    ON.load(Relaxed)
}

/// A clock stamp when metrics are on.
#[inline]
pub fn now() -> Option<Instant> {
    if on() {
        Some(Instant::now())
    } else {
        None
    }
}

/// Split timer: `lap` = us since the last lap (0 when metrics are off: no clock read).
pub struct Lap(Option<Instant>);

impl Lap {
    pub fn start() -> Lap {
        Lap(now())
    }

    pub fn lap(&mut self) -> u64 {
        match self.0 {
            Some(t) => {
                let n = Instant::now();
                self.0 = Some(n);
                n.duration_since(t).as_micros() as u64
            }
            None => 0,
        }
    }
}

pub const BUCKETS: usize = 24;

/// nx.core.latency/log2-bucket: index of the highest set bit of `max(us, 1)`, capped at 23.
#[inline]
pub fn log2_bucket(us: u64) -> usize {
    ((63 - us.max(1).leading_zeros()) as usize).min(BUCKETS - 1)
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);

/// One stage-stats cell, updated with relaxed atomics.
pub struct Stage {
    count: AtomicU64,
    sum_us: AtomicU64,
    max_us: AtomicU64,
    buckets: [AtomicU64; BUCKETS],
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct StageSnap {
    pub count: u64,
    pub sum_us: u64,
    pub max_us: u64,
    pub buckets: [u64; BUCKETS],
}

impl StageSnap {
    /// Nothing recorded (also no half of a sample that was being written during the take).
    pub fn is_zero(&self) -> bool {
        self.count == 0 && self.sum_us == 0 && self.max_us == 0 && self.buckets.iter().all(|b| *b == 0)
    }
}

impl Stage {
    pub const fn new() -> Stage {
        Stage { count: ZERO, sum_us: ZERO, max_us: ZERO, buckets: [ZERO; BUCKETS] }
    }

    /// nx.core.latency/note-sample.
    #[inline]
    pub fn note(&self, us: u64) {
        self.count.fetch_add(1, Relaxed);
        self.sum_us.fetch_add(us, Relaxed);
        self.max_us.fetch_max(us, Relaxed);
        self.buckets[log2_bucket(us)].fetch_add(1, Relaxed);
    }

    /// Snapshot and reset. A sample written during the take may land half in this delta and half in the next one;
    /// every field only adds (max: maxes), so the merged deltas are exact.
    pub fn take(&self) -> StageSnap {
        let mut s = StageSnap { count: self.count.swap(0, Relaxed), sum_us: self.sum_us.swap(0, Relaxed), max_us: self.max_us.swap(0, Relaxed), buckets: [0; BUCKETS] };
        for (o, b) in s.buckets.iter_mut().zip(&self.buckets) {
            *o = b.swap(0, Relaxed);
        }
        s
    }
}

impl Default for Stage {
    fn default() -> Stage {
        Stage::new()
    }
}

// ---- worker jobs ----

/// Job kinds: a document text sent by the client | a file read from disk (project pass, watched file, didClose).
pub const JOB_OPEN: usize = 0;
pub const JOB_BG: usize = 1;
pub const JOB_KINDS: [&str; 2] = ["job/open", "job/bg"];
/// Stages of a job. queue = submit -> a worker starts it; analyze = the rest of the job after read and parse.
pub const JOB_STAGES: [&str; 4] = ["queue", "read", "parse", "analyze"];

struct Jobs {
    count: AtomicU64,
    bytes: AtomicU64,
    stages: [Stage; 4],
}

#[allow(clippy::declare_interior_mutable_const)]
const JOBS0: Jobs = Jobs { count: ZERO, bytes: ZERO, stages: [Stage::new(), Stage::new(), Stage::new(), Stage::new()] };
static JOBS: [Jobs; 2] = [JOBS0, JOBS0];

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct JobsSnap {
    pub count: u64,
    pub bytes: u64,
    pub stages: [StageSnap; 4],
}

impl JobsSnap {
    pub fn is_zero(&self) -> bool {
        self.count == 0 && self.bytes == 0 && self.stages.iter().all(|s| s.is_zero())
    }
}

/// One finished job: `us` in `JOB_STAGES` order; None = the job has no such stage (a text job reads nothing).
#[inline]
pub fn job(kind: usize, bytes: u64, us: [Option<u64>; 4]) {
    let j = &JOBS[kind];
    j.count.fetch_add(1, Relaxed);
    j.bytes.fetch_add(bytes, Relaxed);
    for (s, u) in j.stages.iter().zip(us) {
        if let Some(u) = u {
            s.note(u);
        }
    }
}

/// Snapshot and reset of the job stats, in `JOB_KINDS` order.
pub fn take_jobs() -> [JobsSnap; 2] {
    let take = |j: &Jobs| JobsSnap { count: j.count.swap(0, Relaxed), bytes: j.bytes.swap(0, Relaxed), stages: [j.stages[0].take(), j.stages[1].take(), j.stages[2].take(), j.stages[3].take()] };
    [take(&JOBS[0]), take(&JOBS[1])]
}

const IDLE: u64 = u64::MAX;
thread_local! {
    /// Parse ns of the job this worker thread is running; IDLE = not in a timed job (no clock read then).
    static PARSE_NS: Cell<u64> = const { Cell::new(IDLE) };
}

/// A timed job starts on this thread.
#[inline]
pub fn job_begin() {
    PARSE_NS.with(|c| c.set(0));
}

/// The timed job ended: its parse time in us.
#[inline]
pub fn job_end() -> u64 {
    PARSE_NS.with(|c| c.replace(IDLE)) / 1000
}

/// Clock stamp before a parse, only inside a timed job.
#[inline]
pub fn parse_start() -> Option<Instant> {
    if PARSE_NS.with(|c| c.get()) == IDLE {
        None
    } else {
        Some(Instant::now())
    }
}

#[inline]
pub fn parse_end(t: Option<Instant>) {
    if let Some(t) = t {
        let ns = t.elapsed().as_nanos() as u64;
        PARSE_NS.with(|c| {
            if c.get() != IDLE {
                c.set(c.get() + ns)
            }
        });
    }
}

// ---- project pass ----

/// Spans of a pass, us. index = jar layer into the store + jar indexes; files = batch queued -> last file committed;
/// pass = `analyze_project` entered -> last file committed.
pub const PASS_SPANS: [&str; 8] = ["discover", "config", "classpath", "jars", "ext-dirs", "index", "files", "pass"];
pub const P_DISCOVER: usize = 0;
pub const P_CONFIG: usize = 1;
pub const P_CLASSPATH: usize = 2;
pub const P_JARS: usize = 3;
pub const P_EXT_DIRS: usize = 4;
pub const P_INDEX: usize = 5;
pub const P_FILES: usize = 6;
pub const P_PASS: usize = 7;

/// Counters of a pass. cp-cached = 1 when the classpath came from the cache (no `clojure -Spath`).
pub const PASS_COUNTS: [&str; 12] =
    ["files", "cp-cached", "cp-errors", "jars", "jars-warm", "jars-cold", "jars-failed", "jar-files", "jar-defs", "jar-cache-b", "ext-dirs", "ext-files"];
pub const N_FILES: usize = 0;
pub const N_CP_CACHED: usize = 1;
pub const N_CP_ERRORS: usize = 2;
pub const N_JARS: usize = 3;
pub const N_JARS_WARM: usize = 4;
pub const N_JARS_COLD: usize = 5;
pub const N_JARS_FAILED: usize = 6;
pub const N_JAR_FILES: usize = 7;
pub const N_JAR_DEFS: usize = 8;
pub const N_JAR_CACHE_B: usize = 9;
pub const N_EXT_DIRS: usize = 10;
pub const N_EXT_FILES: usize = 11;

/// One project pass.
#[derive(Clone, Debug, Default)]
pub struct Pass {
    pub batch: u64,
    /// `analyze_project` entered | the batch was queued | the last file of the batch was committed.
    pub start: Option<Instant>,
    pub queued: Option<Instant>,
    pub end: Option<Instant>,
    pub us: [u64; 8],
    pub n: [u64; 12],
}

#[derive(Default)]
struct Passes {
    open: Vec<Pass>,
    /// Batches that completed before their pass record was opened.
    early: Vec<(u64, Instant)>,
    done: Vec<Pass>,
}

static PASSES: Mutex<Passes> = Mutex::new(Passes { open: Vec::new(), early: Vec::new(), done: Vec::new() });
const KEEP: usize = 16; // records kept when nobody reads them

fn finish(mut p: Pass, end: Instant) -> Pass {
    p.end = Some(end);
    if let Some(q) = p.queued {
        p.us[P_FILES] = end.saturating_duration_since(q).as_micros() as u64;
    }
    if let Some(s) = p.start {
        p.us[P_PASS] = end.saturating_duration_since(s).as_micros() as u64;
    }
    p
}

fn keep(v: &mut Vec<Pass>, p: Pass) {
    if v.len() >= KEEP {
        v.remove(0);
    }
    v.push(p);
}

/// The synchronous part of a pass is over and its batch is queued. A pass with no file ends here.
pub fn pass_open(p: Pass) {
    let mut g = PASSES.lock().unwrap_or_else(|e| e.into_inner());
    let early = g.early.iter().position(|(b, _)| *b == p.batch).map(|i| g.early.remove(i).1);
    let end = if p.n[N_FILES] == 0 { p.queued } else { early };
    match end {
        Some(end) => keep(&mut g.done, finish(p, end)),
        None => keep(&mut g.open, p),
    }
}

/// The last file of batch `batch` was committed.
pub fn pass_done(batch: u64) {
    let end = Instant::now();
    let mut g = PASSES.lock().unwrap_or_else(|e| e.into_inner());
    match g.open.iter().position(|p| p.batch == batch) {
        Some(i) => {
            let p = g.open.remove(i);
            keep(&mut g.done, finish(p, end));
        }
        None => {
            if g.early.len() >= KEEP {
                g.early.remove(0);
            }
            g.early.push((batch, end));
        }
    }
}

/// The finished passes not yet read.
pub fn take_passes() -> Vec<Pass> {
    std::mem::take(&mut PASSES.lock().unwrap_or_else(|e| e.into_inner()).done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// nx.core.latency/log2-bucket, line by line.
    fn mova_bucket(us: u64) -> usize {
        let us = us.max(1) as u128;
        let (mut n, mut b) = (1u128, 0usize);
        loop {
            if b >= BUCKETS - 1 || n * 2 > us {
                return b;
            }
            n *= 2;
            b += 1;
        }
    }

    /// nx.core.latency/note-sample over `samples`.
    fn mova_note(samples: &[u64]) -> StageSnap {
        let mut s = StageSnap::default();
        for &us in samples {
            s.count += 1;
            s.sum_us += us;
            s.max_us = s.max_us.max(us);
            s.buckets[mova_bucket(us)] += 1;
        }
        s
    }

    #[test]
    fn bucket_math_equals_mova() {
        // nx/test/latency_test.mova "bucket"
        let got: Vec<usize> = [0u64, 1, 2, 3, 4, 1023, 1024, 100_000_000_000].iter().map(|u| log2_bucket(*u)).collect();
        assert_eq!(got, vec![0, 0, 1, 1, 2, 9, 10, 23]);
        for i in 0..40u32 {
            for d in [-1i64, 0, 1] {
                let us = ((1u64 << i) as i64 + d).max(0) as u64;
                assert_eq!(log2_bucket(us), mova_bucket(us), "us={us}");
            }
        }
        assert_eq!(log2_bucket(u64::MAX), 23);
    }

    #[test]
    fn stage_equals_note_sample() {
        // nx/test/latency_test.mova "note" / "buckets": the same samples give the same stage-stats
        let samples = [1u64, 2, 3, 100, 5000];
        let st = Stage::new();
        for s in samples {
            st.note(s);
        }
        let snap = st.take();
        assert_eq!(snap, mova_note(&samples));
        assert_eq!((snap.count, snap.sum_us, snap.max_us, snap.buckets[12]), (5, 5106, 5000, 1));
        assert!(st.take().is_zero(), "take resets");
        // a wide spread, incl. 0 and the catch-all bucket
        let mut x = 88172645463325252u64;
        let wide: Vec<u64> = (0..5000)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> (i % 50)) % 20_000_000_000
            })
            .chain([0, 1, 8_388_607, 8_388_608, 1 << 40])
            .collect();
        for s in &wide {
            st.note(*s);
        }
        assert_eq!(st.take(), mova_note(&wide));
    }

    #[test]
    fn deltas_merge_exactly_under_writers() {
        static ST: Stage = Stage::new();
        let total = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..4u64).map(|t| sc.spawn(move || (0..20_000u64).for_each(|i| ST.note((i * 7 + t) % 3000)))).collect();
            let mut sum = StageSnap::default();
            let mut add = |d: StageSnap| {
                sum.count += d.count;
                sum.sum_us += d.sum_us;
                sum.max_us = sum.max_us.max(d.max_us);
                for (a, b) in sum.buckets.iter_mut().zip(d.buckets) {
                    *a += b;
                }
            };
            while !hs.iter().all(|h| h.is_finished()) {
                add(ST.take());
            }
            hs.into_iter().for_each(|h| h.join().unwrap());
            add(ST.take());
            sum
        });
        let all: Vec<u64> = (0..4u64).flat_map(|t| (0..20_000u64).map(move |i| (i * 7 + t) % 3000)).collect();
        assert_eq!(total, mova_note(&all));
    }

    #[test]
    fn pass_records() {
        // off: no clock
        assert!(Lap(None).lap() == 0);
        let t = Instant::now();
        let mk = |batch, files| Pass { batch, start: Some(t), queued: Some(t), n: { let mut n = [0; 12]; n[N_FILES] = files; n }, ..Default::default() };
        pass_open(mk(9001, 3));
        assert!(take_passes().iter().all(|p| p.batch != 9001), "not done yet");
        pass_done(9001);
        pass_done(9002); // completes before its record opens
        pass_open(mk(9002, 5));
        pass_open(mk(9003, 0)); // no file: done at once
        let got = take_passes();
        let ids: Vec<u64> = got.iter().map(|p| p.batch).filter(|b| (9001..=9003).contains(b)).collect();
        assert_eq!(ids, vec![9001, 9002, 9003]);
        assert!(got.iter().all(|p| p.end.is_some() && p.us[P_PASS] >= p.us[P_FILES]));
    }
}
