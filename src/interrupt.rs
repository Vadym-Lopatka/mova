//! P0c spike: per-interpreter interrupt flag (nREPL `interrupt` op).
//!
//! `state`: 0 = none, 1 = soft (script-catchable `InterruptedException`),
//! 2 = hard (unwinds past `catch` like `FuelExhausted`, `finally` runs).
//! The evaluating thread CONSUMES the state when it raises the error, so a
//! `finally` body that runs during the unwind is not interrupted again.
use crate::error::RjError;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct Interrupt {
    state: AtomicU8,
    mu: Mutex<()>,
    cv: Condvar,
    /// The condvar this interpreter's thread is blocked on right now (a
    /// channel, a promise, a monitor), registered by [`wait_on`] so that
    /// `soft`/`hard` can wake it. No polling: the waiter sleeps until woken.
    waker: Mutex<Option<WakePtr>>,
}

struct WakePtr(*const Condvar);
// SAFETY: the pointer is only dereferenced under the `waker` lock, and the
// waiter clears it (under the same lock) before the condvar can be dropped.
unsafe impl Send for WakePtr {}

/// Message of the soft error a loop back-edge raises. A host (nREPL) treats it
/// like the JVM's `ThreadDeath`: the interrupted eval ends without an error report.
pub const LOOP_INTERRUPT_MSG: &str = "interrupted (loop)";

impl Interrupt {
    pub fn new() -> Arc<Self> {
        Arc::new(Interrupt { state: AtomicU8::new(0), mu: Mutex::new(()), cv: Condvar::new(), waker: Mutex::new(None) })
    }
    #[inline(always)]
    pub fn pending(&self) -> bool {
        self.state.load(Ordering::Relaxed) != 0
    }
    /// Stage 1. Never downgrades a pending hard interrupt.
    pub fn soft(&self) {
        self.state.fetch_max(1, Ordering::SeqCst);
        self.wake();
    }
    /// Stage 2.
    pub fn hard(&self) {
        self.state.store(2, Ordering::SeqCst);
        self.wake();
    }
    pub fn clear(&self) {
        self.state.store(0, Ordering::SeqCst);
    }
    /// Wakes the thread if it sleeps in `sleep` or in a [`wait_on`] wait. Safe
    /// to call again and again (a host re-wakes while an interrupt is pending,
    /// which closes the window between a waiter's flag check and its park).
    pub fn wake(&self) {
        {
            let _g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
            self.cv.notify_all();
        }
        let w = self.waker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(WakePtr(p)) = &*w {
            // SAFETY: see `WakePtr`.
            unsafe { &**p }.notify_all();
        }
    }
    /// Like `take_err`, for a loop back-edge: the soft error carries
    /// [`LOOP_INTERRUPT_MSG`].
    #[cold]
    #[inline(never)]
    pub fn take_err_loop(&self) -> Option<RjError> {
        match self.state.swap(0, Ordering::SeqCst) {
            0 => None,
            2 => Some(RjError::interrupted_hard("interrupted (hard)")),
            _ => Some(RjError::interrupted(LOOP_INTERRUPT_MSG)),
        }
    }
    /// Consumes the pending state into an error. `None` if nothing pending.
    #[cold]
    #[inline(never)]
    pub fn take_err(&self) -> Option<RjError> {
        match self.state.swap(0, Ordering::SeqCst) {
            0 => None,
            2 => Some(RjError::interrupted_hard("interrupted (hard)")),
            _ => Some(RjError::interrupted("sleep interrupted")),
        }
    }
    /// Interruptible sleep: wakes immediately on `soft`/`hard`.
    pub fn sleep(&self, dur: Duration) -> Result<(), RjError> {
        let deadline = Instant::now() + dur;
        let mut g = self.mu.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.pending() {
                drop(g);
                return Err(self.take_err().unwrap_or_else(|| RjError::interrupted("interrupted")));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            g = self.cv.wait_timeout(g, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

thread_local! {
    /// Set by interruptible natives that call into code without `&Interp`
    /// (channel waits). Slice-polled, see `builtins::async::chan_wait`.
    pub static WAIT_INTR: std::cell::Cell<*const Interrupt> = const { std::cell::Cell::new(std::ptr::null()) };
}

thread_local! {
    pub static WAIT_ABORT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
// (WAIT_INTR above is replaced by a raw pointer cell.)

/// Arms the interruptible condvar wait on this thread for one native call.
pub struct WaitGuard;
impl WaitGuard {
    pub fn arm(i: &Arc<Interrupt>) -> WaitGuard {
        WAIT_INTR.with(|c| c.set(Arc::as_ptr(i)));
        WAIT_ABORT.with(|c| c.set(false));
        WaitGuard
    }
}
impl Drop for WaitGuard {
    fn drop(&mut self) {
        WAIT_INTR.with(|c| c.set(std::ptr::null()));
    }
}

/// One interruptible wait on `cv`: returns `(guard, true)` at once if an
/// interrupt is pending, else parks until `cv` is notified (by its owner or by
/// `Interrupt::wake`) and returns `(guard, false)`. The caller re-checks its
/// own condition and calls again. No timeout, no polling.
pub fn wait_on<'a, T>(i: &Interrupt, cv: &Condvar, g: std::sync::MutexGuard<'a, T>) -> (std::sync::MutexGuard<'a, T>, bool) {
    if i.pending() {
        return (g, true);
    }
    *i.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(WakePtr(cv));
    if i.pending() {
        *i.waker.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return (g, true);
    }
    let g = crate::sync::cv_wait(cv, g);
    *i.waker.lock().unwrap_or_else(|e| e.into_inner()) = None;
    (g, false)
}

/// Interruptible `Condvar` wait for a thread that armed `WaitGuard`. Returns
/// `(guard, aborted)`.
pub fn cv_wait_intr<'a, T>(cv: &Condvar, g: std::sync::MutexGuard<'a, T>) -> (std::sync::MutexGuard<'a, T>, bool) {
    let p = WAIT_INTR.with(|c| c.get());
    if p.is_null() {
        return (crate::sync::cv_wait(cv, g), false);
    }
    // SAFETY: see `WaitGuard`.
    wait_on(unsafe { &*p }, cv, g)
}
