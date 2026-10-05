//! Long-lived worker pool with two priorities (High = open/changed docs, Low = background project pass).
use super::types::is_external_uri;
use super::scan::path_to_uri;
use super::store::Store;
use super::types::{FileEntry, Lang};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prio {
    High,
    Low,
}

/// One finished job. `entry` None = skipped/unreadable (still counts for batch progress).
pub struct Done {
    pub entry: Option<Arc<FileEntry>>,
    /// Background batch id (0 = none).
    pub batch: u64,
    /// Disk read that must replace an open-doc entry (after didClose).
    pub disk_override: bool,
    /// Position in an `analyze_paths` call.
    pub idx: usize,
}

enum Kind {
    Text { uri: String, version: i64, text: String },
    Path { path: PathBuf, disk_override: bool },
}

struct Job {
    kind: Kind,
    batch: u64,
    reply: Option<Sender<Done>>,
    /// Index for `analyze_paths` ordering.
    idx: usize,
    /// Submit stamp; Some only when metrics are on (`crate::met`).
    sub: Option<std::time::Instant>,
}

#[derive(Default)]
struct Queues {
    high: VecDeque<Job>,
    low: VecDeque<Job>,
    shutdown: bool,
    idle: usize,
}

struct Batch {
    total: usize,
    done: usize,
}

struct Inner {
    q: Mutex<Queues>,
    cv: Condvar,
    tx: Mutex<Sender<Done>>,
    rx: Mutex<Receiver<Done>>,
    closed: AtomicBool,
    next_batch: AtomicU64,
    batches: Mutex<Vec<(u64, Batch)>>,
    store: Store,
    /// Latest text of every open doc (also those still in flight: a snapshot misses them).
    open: Mutex<std::collections::HashMap<String, (i64, Arc<str>)>>,
    spawned: AtomicUsize,
    max_workers: usize,
}

pub struct Pool {
    inner: Arc<Inner>,
    pub workers: usize,
}

pub fn default_workers() -> usize {
    if let Some(n) = std::env::var("NX_WORKERS").ok().and_then(|v| v.parse::<usize>().ok()) {
        return n.max(1);
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    // each worker keeps an allocator heap + stack pages (~1.4 MB each settled): 3 workers, see mem_attr example
    cores.saturating_sub(2).clamp(1, 3)
}

pub(crate) fn read_lossy(p: &std::path::Path) -> Option<String> {
    let b = std::fs::read(p).ok()?;
    Some(match String::from_utf8(b) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

fn entry_of(uri: String, version: i64, lang: Lang, internal: bool, text: String, ctx: &super::ctx::Ctx) -> Arc<FileEntry> {
    let r = super::analyze::analyze_file_ctx_pos(&uri, text, lang, internal, version >= 0 || !internal, ctx);
    Arc::new(FileEntry {
        uri: uri.into(),
        version,
        lang,
        hash: r.hash,
        findings: r.findings,
        text: r.text,
        analysis: r.analysis.map(Arc::new),
        pos: r.pos.map(Arc::new),
        lazy_pos: Default::default(),
        tgt: Arc::new(Vec::new()),
        internal,
        lazy: None,
    })
}

fn run(job: Job, store: &Store) -> (Done, Option<Sender<Done>>) {
    let Job { kind, batch, reply, idx, sub } = job;
    // metrics (off: `sub` is None, no clock read): queue wait, read, parse, the rest; recorded natively below
    let t0 = sub.map(|_| {
        crate::met::job_begin();
        std::time::Instant::now()
    });
    let (mut mkind, mut bytes, mut read_us) = (crate::met::JOB_BG, 0u64, None);
    let ctx = store.ctx().clone();
    let (entry, ov) = match kind {
        Kind::Text { uri, version, text } => {
            (mkind, bytes) = (crate::met::JOB_OPEN, text.len() as u64);
            let lang = Lang::from_path(&uri);
            let internal = !is_external_uri(&uri);
            // a config swap during analysis (project config loads after the first didOpen): redo with the new one
            let mut cfg0 = ctx.cfg.load_full();
            let mut defs0 = ctx.defs.load_full();
            let mut e = entry_of(uri.clone(), version, lang, internal, text, &ctx);
            for _ in 0..3 {
                // same for the jar/dir defs layer (classpath load): a result built on the old layer must not win the commit
                if Arc::ptr_eq(&cfg0, &ctx.cfg.load_full()) && Arc::ptr_eq(&defs0, &ctx.defs.load_full()) {
                    break;
                }
                let Some(t) = e.text.as_ref().map(|t| t.to_string()) else { break };
                cfg0 = ctx.cfg.load_full();
                defs0 = ctx.defs.load_full();
                e = entry_of(uri.clone(), version, lang, internal, t, &ctx);
            }
            (Some(e), false)
        }
        Kind::Path { path, disk_override } => {
            let uri = path_to_uri(&path);
            // an open doc wins over a background disk read: skip the work
            let open = !disk_override && store.snapshot().get_exact(&uri).map(|e| e.version >= 0).unwrap_or(false);
            if open {
                (None, false)
            } else {
                let tr = t0.map(|_| std::time::Instant::now());
                let text = read_lossy(&path);
                read_us = tr.map(|t| t.elapsed().as_micros() as u64);
                match text {
                    Some(text) => {
                        bytes = text.len() as u64;
                        let lang = Lang::from_path(&uri);
                        (Some(entry_of(uri, -1, lang, true, text, &ctx)), disk_override)
                    }
                    None => (None, false),
                }
            }
        }
    };
    if let (Some(sub), Some(t0)) = (sub, t0) {
        let parse = crate::met::job_end();
        if entry.is_some() {
            let run = t0.elapsed().as_micros() as u64;
            let queue = t0.saturating_duration_since(sub).as_micros() as u64;
            let rest = run.saturating_sub(read_us.unwrap_or(0)).saturating_sub(parse);
            crate::met::job(mkind, bytes, [Some(queue), read_us, Some(parse), Some(rest)]);
        }
    }
    (Done { entry, batch, disk_override: ov, idx }, reply)
}

impl Pool {
    /// `workers` 0 = default.
    pub fn new(workers: usize, store: Store) -> Pool {
        let workers = if workers == 0 { default_workers() } else { workers };
        let (tx, rx) = channel();
        let inner = Arc::new(Inner {
            q: Mutex::new(Queues::default()),
            cv: Condvar::new(),
            tx: Mutex::new(tx),
            rx: Mutex::new(rx),
            closed: AtomicBool::new(false),
            next_batch: AtomicU64::new(1),
            batches: Mutex::new(Vec::new()),
            store,
            open: Mutex::new(Default::default()),
            spawned: AtomicUsize::new(0),
            max_workers: workers,
        });
        Pool { inner, workers }
    }

    /// Workers start lazily (idle process = zero extra threads): grow to `want` (capped at the pool size).
    fn ensure_workers(&self, want: usize) {
        let inn = &self.inner;
        while inn.spawned.load(Ordering::Relaxed) < want.min(inn.max_workers) {
            let i = inn.spawned.fetch_add(1, Ordering::Relaxed);
            if i >= inn.max_workers {
                inn.spawned.fetch_sub(1, Ordering::Relaxed);
                return;
            }
            let (inn2, tx) = (inn.clone(), inn.tx.lock().unwrap().clone());
            std::thread::Builder::new()
                .name(format!("nx-worker-{i}"))
                .stack_size(8 << 20)
                .spawn(move || worker(inn2, tx))
                .expect("spawn nx worker");
        }
    }

    fn push(&self, job: Job, prio: Prio) {
        let mut q = self.inner.q.lock().unwrap();
        match prio {
            Prio::High => {
                // coalesce: a newer text for the same uri replaces a queued older one
                if let Kind::Text { uri, version, .. } = &job.kind {
                    q.high.retain(|j| !matches!(&j.kind, Kind::Text { uri: u, version: v, .. } if u == uri && v <= version));
                }
                q.high.push_back(job)
            }
            Prio::Low => q.low.push_back(job),
        }
        let need = q.idle == 0;
        drop(q);
        if need {
            self.ensure_workers(self.inner.spawned.load(Ordering::Relaxed) + 1);
        }
        self.inner.cv.notify_one();
    }

    /// Open/changed document. Never blocks.
    pub fn submit_text(&self, uri: &str, version: i64, text: String) {
        {
            let mut o = self.inner.open.lock().unwrap();
            if o.get(uri).map_or(true, |(v, _)| *v <= version) {
                o.insert(uri.to_string(), (version, Arc::from(text.as_str())));
            }
        }
        self.push(Job { kind: Kind::Text { uri: uri.to_string(), version, text }, batch: 0, reply: None, idx: 0, sub: crate::met::now() }, Prio::High);
    }

    /// Open docs (uri, version, text), including ones whose analysis is not committed yet.
    pub fn open_docs(&self) -> Vec<(String, i64, Arc<str>)> {
        self.inner.open.lock().unwrap().iter().map(|(u, (v, t))| (u.clone(), *v, t.clone())).collect()
    }

    /// The doc was closed: it is no longer re-analyzed on config / classpath changes.
    pub fn forget_open(&self, uri: &str) {
        self.inner.open.lock().unwrap().remove(uri);
    }

    pub fn submit_paths(&self, paths: Vec<PathBuf>, disk_override: bool, prio: Prio) {
        let sub = crate::met::now();
        for path in paths {
            self.push(Job { kind: Kind::Path { path, disk_override }, batch: 0, reply: None, idx: 0, sub }, prio);
        }
    }

    /// Background project pass: all files low priority; returns the batch id (progress via `batch_progress`).
    pub fn submit_batch(&self, files: Vec<PathBuf>) -> u64 {
        let id = self.inner.next_batch.fetch_add(1, Ordering::Relaxed);
        self.inner.batches.lock().unwrap().push((id, Batch { total: files.len(), done: 0 }));
        let total = files.len();
        let sub = crate::met::now();
        let mut q = self.inner.q.lock().unwrap();
        for path in files {
            q.low.push_back(Job { kind: Kind::Path { path, disk_override: false }, batch: id, reply: None, idx: 0, sub });
        }
        drop(q);
        self.ensure_workers(total);
        self.inner.cv.notify_all();
        id
    }

    /// Blocking parallel batch (private reply channel); results in input order.
    pub fn analyze_paths(&self, paths: Vec<PathBuf>) -> Vec<Option<Arc<FileEntry>>> {
        let n = paths.len();
        let (tx, rx) = channel();
        self.ensure_workers(n);
        let sub = crate::met::now();
        {
            let mut q = self.inner.q.lock().unwrap();
            for (idx, path) in paths.into_iter().enumerate() {
                q.low.push_back(Job { kind: Kind::Path { path, disk_override: true }, batch: 0, reply: Some(tx.clone()), idx, sub });
            }
        }
        self.inner.cv.notify_all();
        drop(tx);
        let mut out: Vec<Option<Arc<FileEntry>>> = vec![None; n];
        for _ in 0..n {
            match rx.recv() {
                Ok(d) => {
                    let i = d.idx;
                    out[i] = d.entry;
                }
                Err(_) => break,
            }
        }
        out
    }

    /// (done, total) of a background batch, counted as results are received.
    pub fn batch_progress(&self, id: u64) -> Option<(usize, usize)> {
        self.inner.batches.lock().unwrap().iter().find(|(b, _)| *b == id).map(|(_, b)| (b.done, b.total))
    }

    /// Block for one result, then drain up to `max` ready ones. None once closed.
    pub fn recv_batch(&self, max: usize) -> Option<Vec<Done>> {
        let rx = self.inner.rx.lock().unwrap();
        let first = loop {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(d) => break d,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if self.inner.closed.load(Ordering::Relaxed) {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        };
        let mut v = vec![first];
        while v.len() < max {
            match rx.try_recv() {
                Ok(d) => v.push(d),
                Err(_) => break,
            }
        }
        drop(rx);
        let mut bs = self.inner.batches.lock().unwrap();
        for d in &v {
            if d.batch != 0 {
                if let Some((_, b)) = bs.iter_mut().find(|(id, _)| *id == d.batch) {
                    b.done += 1;
                }
            }
        }
        Some(v)
    }

    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        self.inner.q.lock().unwrap().shutdown = true;
        self.inner.cv.notify_all();
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.close();
    }
}

fn worker(inn: Arc<Inner>, tx: Sender<Done>) {
    let mut dirty = false; // worked since the last allocator trim
    loop {
        let job = {
            let mut q = inn.q.lock().unwrap();
            loop {
                if q.shutdown {
                    return;
                }
                if let Some(j) = q.high.pop_front().or_else(|| q.low.pop_front()) {
                    break j;
                }
                q.idle += 1;
                if dirty {
                    // idle after work: hand this thread's freed pages back once it stays idle
                    let (g, t) = inn.cv.wait_timeout(q, std::time::Duration::from_millis(150)).unwrap();
                    q = g;
                    q.idle -= 1;
                    if t.timed_out() && q.high.is_empty() && q.low.is_empty() {
                        drop(q);
                        super::trim();
                        dirty = false;
                        q = inn.q.lock().unwrap();
                    }
                } else {
                    q = inn.cv.wait(q).unwrap();
                    q.idle -= 1;
                }
            }
        };
        dirty = true;
        let (done, reply) = run(job, &inn.store);
        match reply {
            Some(r) => {
                let _ = r.send(done);
            }
            None => {
                let _ = tx.send(done);
            }
        }
    }
}
