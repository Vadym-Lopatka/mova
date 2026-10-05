//! Poisoned-lock handling, centralized. A panicked thread that was holding
//! one of our `Mutex`/`RwLock`/`Condvar` locks (e.g. inside a `future*`
//! thread, or a native mid-mutation) marks that lock "poisoned"; the stdlib
//! default is to propagate that poisoning as an `Err` from every subsequent
//! `.lock()`/`.read()`/`.write()`, which would otherwise force every single
//! call site in the interpreter to decide what to do about it.
//!
//! mova's policy (documented once, here, instead of `unwrap()` spreading
//! across the codebase): recover the guard anyway via
//! `.unwrap_or_else(|e| e.into_inner())`. A panic inside one closure/thread
//! shouldn't permanently wedge every other value that happens to share a
//! lock-protected cell -- the data behind the lock might be left in a
//! half-updated state, but that's true of any panic-mid-mutation regardless
//! of poisoning, and refusing to ever touch the cell again is strictly
//! worse for a long-lived REPL/interpreter process than pressing on.
//!
//! Every lock acquisition in the interpreter (`Env`, `Atom`, `LazySeq`,
//! `Future`/`Promise`/`Delay` cells) goes through these three helpers so the
//! policy lives in exactly one place.

use std::sync::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

pub fn lock_mutex<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn lock_read<T>(m: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

pub fn lock_write<T>(m: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

/// `Condvar::wait`, poison-recovered the same way the lock helpers above are.
pub fn cv_wait<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(guard).unwrap_or_else(|e| e.into_inner())
}

/// `Condvar::wait_timeout`, poison-recovered; returns the reacquired guard
/// plus whether the wait timed out (as opposed to being woken by a notify).
pub fn cv_wait_timeout<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>, dur: Duration) -> (MutexGuard<'a, T>, bool) {
    let (guard, result) = cv.wait_timeout(guard, dur).unwrap_or_else(|e| e.into_inner());
    (guard, result.timed_out())
}
