//! The write queue seen from outside the IO thread.
//!
//! An `Outbox` is a cheap, cloneable handle to one connection. Any thread may
//! call `send` with an already encoded reply.
//!
//! * From another thread: the bytes are appended to one byte buffer per
//!   connection, under its lock (a frame is copied whole, so frames never
//!   interleave). No allocation per message. The IO thread is rung only when
//!   the buffer goes from empty to non-empty *and* the IO thread is about to
//!   sleep (see `Shared`): a burst of replies costs at most one syscall, and
//!   a busy IO thread costs none.
//! * From the IO thread itself (a backend answering inside `dispatch`): the
//!   bytes go into a thread-local list. No lock, no wake-up. The IO loop
//!   drains it before it blocks again.
//!
//! Replies for one connection are written in the order they were sent from
//! one thread. A connection that is closed drops whatever is queued for it.
//!
//! # Backpressure
//!
//! A backend thread that sends faster than the client reads must not make the
//! server grow without limit. Each connection has a [`Gate`] that counts the
//! bytes sent from other threads and not yet written to the socket. When that
//! count passes `HIGH_WATER` the *sending thread* blocks (a condvar, no
//! polling) until the IO thread has written it down to `LOW_WATER`, or the
//! connection closes. The message that crossed the line is already queued, so
//! the bound is `HIGH_WATER` plus one message per blocked sender. The IO
//! thread itself never blocks: its own replies are not counted.

use crate::poll::Waker;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Senders block when this many bytes (sent from other threads) are unwritten.
pub const HIGH_WATER: usize = 1 << 20;
/// ... and wake when the IO thread has written that down to this.
pub const LOW_WATER: usize = 1 << 19;

/// Per-connection byte counter that blocks fast senders. See the module docs.
pub(crate) struct Gate {
    inflight: AtomicUsize,
    waiters: AtomicUsize,
    /// Bumped by `kick`: a blocked sender that sees it change stops waiting once.
    kicks: AtomicUsize,
    closed: AtomicBool,
    mu: Mutex<()>,
    cv: Condvar,
}

impl Gate {
    pub(crate) fn new() -> Gate {
        Gate {
            inflight: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
            kicks: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            mu: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    /// Sender side: counts `n` more bytes; blocks while over the limit.
    #[cfg(test)]
    fn add(&self, n: usize) {
        if self.count(n) {
            self.block();
        }
    }

    /// Counts `n` bytes about to be queued. `true` if the count is over
    /// `HIGH_WATER`: the caller queues, then calls `block`. Counting before
    /// queueing keeps the count from ever going below zero.
    pub(crate) fn count(&self, n: usize) -> bool {
        self.inflight.fetch_add(n, Ordering::SeqCst) + n > HIGH_WATER
    }

    /// Blocks while over the limit (see `count`).
    pub(crate) fn block(&self) {
        let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let kicks = self.kicks.load(Ordering::SeqCst);
        while self.inflight.load(Ordering::SeqCst) > LOW_WATER
            && !self.closed.load(Ordering::SeqCst)
            && self.kicks.load(Ordering::SeqCst) == kicks
        {
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        self.waiters.fetch_sub(1, Ordering::SeqCst);
    }

    /// IO thread side: `n` counted bytes reached the socket.
    pub(crate) fn release(&self, n: usize) {
        if n == 0 {
            return;
        }
        let left = self.inflight.fetch_sub(n, Ordering::SeqCst).saturating_sub(n);
        if left <= LOW_WATER && self.waiters.load(Ordering::SeqCst) > 0 {
            let _g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
            self.cv.notify_all();
        }
    }

    /// Lets every sender that is blocked right now go on (once). Used to
    /// free an interrupted eval that waits for a slow client.
    pub(crate) fn kick(&self) {
        self.kicks.fetch_add(1, Ordering::SeqCst);
        let _g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        self.cv.notify_all();
    }

    /// The connection is gone: let every blocked sender go.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        self.cv.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }
}

/// Identifies a connection slot and its generation, so a stale handle never
/// reaches a new connection that reused the slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ConnKey(pub u64);

pub(crate) type Pending = Vec<(ConnKey, Vec<u8>)>;

thread_local! {
    static ON_IO_THREAD: Cell<bool> = const { Cell::new(false) };
    static LOCAL: RefCell<Pending> = const { RefCell::new(Vec::new()) };
}

/// Marks the calling thread as the IO thread (called once by the loop).
pub(crate) fn mark_io_thread() {
    ON_IO_THREAD.with(|c| c.set(true));
}

/// What the IO thread shares with every sender: the list of connections that
/// have bytes waiting, and the "coalesced bell" that wakes it.
///
/// The bell rings (one syscall) only when the IO thread says it is about to
/// sleep (`sleeping`) and a connection goes from empty to non-empty. While the
/// IO thread is busy no sender makes a syscall; before it blocks it re-checks
/// the list (`prepare_sleep`), so a sender that came in between is never lost.
pub(crate) struct Shared {
    dirty: Mutex<Vec<ConnKey>>,
    /// Length of `dirty`, written under its lock; read without it.
    dirty_n: AtomicUsize,
    sleeping: AtomicBool,
    stop: AtomicBool,
    waker: Waker,
}

impl Shared {
    pub(crate) fn new(waker: Waker) -> Shared {
        Shared {
            dirty: Mutex::new(Vec::new()),
            dirty_n: AtomicUsize::new(0),
            sleeping: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            waker,
        }
    }

    /// A connection went from empty to non-empty.
    fn mark_dirty(&self, key: ConnKey) {
        {
            let mut d = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
            d.push(key);
            self.dirty_n.store(d.len(), Ordering::SeqCst);
        }
        if self.sleeping.swap(false, Ordering::SeqCst) {
            self.waker.wake();
        }
    }

    /// IO thread: connections with bytes waiting (moved into `out`, which must be empty).
    pub(crate) fn take_dirty(&self, out: &mut Vec<ConnKey>) {
        if self.dirty_n.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut d = self.dirty.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::swap(&mut *d, out);
        self.dirty_n.store(0, Ordering::SeqCst);
    }

    /// IO thread, just before it blocks: announces sleep, then re-checks.
    /// `true` = go ahead and block; `false` = there is work (or a stop), do not.
    pub(crate) fn prepare_sleep(&self) -> bool {
        self.sleeping.store(true, Ordering::SeqCst);
        if self.dirty_n.load(Ordering::SeqCst) == 0 && !self.stopping() {
            return true;
        }
        self.sleeping.store(false, Ordering::SeqCst);
        false
    }

    /// IO thread: back from the poller.
    pub(crate) fn awake(&self) {
        self.sleeping.store(false, Ordering::SeqCst);
    }

    /// Asks the IO loop to return from `run`.
    pub(crate) fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.waker.wake();
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// IO thread: replies queued by the backend on this thread.
    pub(crate) fn take_local(&self, spare: &mut Pending) {
        LOCAL.with(|l| std::mem::swap(&mut *l.borrow_mut(), spare));
    }
}

/// The bytes other threads have queued for one connection, in order.
struct Buf {
    bytes: Vec<u8>,
    /// The connection is on the `dirty` list (or being delivered): no new
    /// entry, no bell.
    queued: bool,
}

/// Handle for sending encoded replies to one connection.
///
/// All threads append whole frames to one shared byte buffer under its lock,
/// so a frame is never split by another thread's frame and the order is the
/// lock order. The IO thread swaps the buffer out and writes it in one go.
#[derive(Clone)]
pub struct Outbox {
    shared: Arc<Shared>,
    key: ConnKey,
    open: Arc<AtomicBool>,
    gate: Arc<Gate>,
    buf: Arc<Mutex<Buf>>,
    /// `Some` for a tap (see [`Outbox::tap`]): replies go to its queue, not the connection.
    tap: Option<Arc<TapProducer>>,
}

/// What a tap collects: whole encoded frames, in send order.
struct TapQueue {
    st: Mutex<TapState>,
    cv: Condvar,
}

struct TapState {
    bytes: Vec<u8>,
    /// Every sender (clone of the tap `Outbox`) is gone.
    closed: bool,
}

/// Shared by every clone of a tap `Outbox`; dropping the last one closes the queue.
struct TapProducer(Arc<TapQueue>);

impl Drop for TapProducer {
    fn drop(&mut self) {
        let mut st = self.0.st.lock().unwrap_or_else(|e| e.into_inner());
        st.closed = true;
        self.0.cv.notify_all();
    }
}

/// The reading end of a tap. See [`Outbox::tap`].
pub struct Tap(Arc<TapQueue>);

impl Tap {
    /// Blocks until frames are queued (moved into `out`, `true`) or every
    /// sender is gone and nothing is left (`false`).
    pub fn wait_take(&self, out: &mut Vec<u8>) -> bool {
        let mut st = self.0.st.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if !st.bytes.is_empty() {
                out.append(&mut st.bytes);
                return true;
            }
            if st.closed {
                return false;
            }
            st = self.0.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Queues frames as if a sender had sent them (the middleware lane puts
    /// the replies the router wrote inline here, so they keep their order).
    pub fn push(&self, bytes: &[u8]) {
        let mut st = self.0.st.lock().unwrap_or_else(|e| e.into_inner());
        st.bytes.extend_from_slice(bytes);
        self.0.cv.notify_all();
    }
}

impl Outbox {
    pub(crate) fn new(shared: Arc<Shared>, key: ConnKey, open: Arc<AtomicBool>, gate: Arc<Gate>) -> Outbox {
        Outbox { shared, key, open, gate, buf: Arc::new(Mutex::new(Buf { bytes: Vec::new(), queued: false })), tap: None }
    }

    /// A tap: an `Outbox` that is not a connection. Whatever is sent on it (from
    /// any thread, never blocking) can be read from the returned [`Tap`]. It
    /// is open while the connection of `self` is. Used by the middleware lane
    /// to run native ops and hand their replies to Mova-level transports.
    pub fn tap(&self) -> (Outbox, Tap) {
        let q = Arc::new(TapQueue { st: Mutex::new(TapState { bytes: Vec::new(), closed: false }), cv: Condvar::new() });
        let ob = Outbox {
            shared: self.shared.clone(),
            key: self.key,
            open: self.open.clone(),
            gate: Arc::new(Gate::new()),
            buf: Arc::new(Mutex::new(Buf { bytes: Vec::new(), queued: false })),
            tap: Some(Arc::new(TapProducer(q.clone()))),
        };
        (ob, Tap(q))
    }

    pub(crate) fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }

    /// IO thread: takes what other threads queued by swapping it with `spare`
    /// (which must be empty, so its capacity is reused). Clears the "queued" mark.
    pub(crate) fn take(&self, spare: &mut Vec<u8>) {
        let mut b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::swap(&mut b.bytes, spare);
        b.queued = false;
    }

    /// Queues one or more complete encoded messages. Returns `false` (and
    /// drops the bytes) if the connection is already closed.
    ///
    /// From a thread other than the IO thread this **blocks** while more than
    /// `HIGH_WATER` bytes for this connection are waiting to be written (see
    /// the module docs): a slow client slows its own senders instead of
    /// growing memory.
    pub fn send(&self, bytes: Vec<u8>) -> bool {
        self.append(&[&bytes])
    }

    /// Like `send`, from pieces: they are copied one after the other under
    /// one lock, so they form one unbroken frame. No allocation.
    pub fn append(&self, parts: &[&[u8]]) -> bool {
        if !self.is_open() {
            return false;
        }
        if let Some(t) = &self.tap {
            let mut st = t.0.st.lock().unwrap_or_else(|e| e.into_inner());
            for p in parts {
                st.bytes.extend_from_slice(p);
            }
            t.0.cv.notify_all();
            return true;
        }
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if ON_IO_THREAD.with(|c| c.get()) {
            let mut v = Vec::with_capacity(len);
            for p in parts {
                v.extend_from_slice(p);
            }
            LOCAL.with(|l| l.borrow_mut().push((self.key, v)));
            return true;
        }
        let over = self.gate.count(len);
        let first = {
            let mut b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
            for p in parts {
                b.bytes.extend_from_slice(p);
            }
            !std::mem::replace(&mut b.queued, true)
        };
        if first {
            self.shared.mark_dirty(self.key);
        }
        if over {
            self.gate.block();
        }
        self.is_open()
    }

    /// Wakes senders blocked by backpressure on this connection (they go on
    /// past the limit once). Call it when the evals they run were interrupted.
    pub fn wake_blocked_senders(&self) {
        self.gate.kick();
    }

    /// False once the peer is gone. A long-running job can poll this to stop early.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for Outbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Outbox({}, open={})", self.key.0, self.is_open())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn gate_blocks_over_high_water_and_wakes_on_release() {
        let g = Arc::new(Gate::new());
        g.add(HIGH_WATER); // exactly at the limit: no block
        let g2 = g.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            g2.add(10); // crosses: blocks
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err(), "sender must block");
        g.release(HIGH_WATER - LOW_WATER); // still above LOW_WATER: 10 over
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "still above low water");
        g.release(20);
        rx.recv_timeout(Duration::from_secs(2)).expect("sender wakes below low water");
        h.join().unwrap();
        assert!(g.inflight() < HIGH_WATER);
    }

    #[test]
    fn gate_close_frees_blocked_senders() {
        let g = Arc::new(Gate::new());
        g.inflight.store(HIGH_WATER + 1, Ordering::SeqCst);
        let g2 = g.clone();
        let h = std::thread::spawn(move || g2.add(1));
        std::thread::sleep(Duration::from_millis(100));
        g.close();
        h.join().unwrap();
    }

#[cfg(test)]
mod tap_tests {
    use super::*;
    use crate::poll::Poller;

    #[test]
    fn tap_collects_frames_and_closes_when_senders_are_gone() {
        let p = Poller::new().unwrap();
        let ob = Outbox::new(Arc::new(Shared::new(p.waker())), ConnKey(3), Arc::new(AtomicBool::new(true)), Arc::new(Gate::new()));
        let (tob, tap) = ob.tap();
        let t2 = tob.clone();
        assert!(tob.send(b"a".to_vec()));
        assert!(t2.append(&[b"b", b"c"]));
        let mut buf = Vec::new();
        assert!(tap.wait_take(&mut buf));
        assert_eq!(buf, b"abc");
        // nothing reached the connection itself
        let mut conn = Vec::new();
        ob.take(&mut conn);
        assert!(conn.is_empty());
        drop(tob);
        let h = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(t2);
        });
        buf.clear();
        assert!(!tap.wait_take(&mut buf), "closed once every sender is dropped");
        h.join().unwrap();
    }
}
}
