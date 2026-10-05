//! The interpreter side of the nREPL server (design phase P2: `eval`).
//!
//! The wire layer (`mova-nrepl` crate) knows nothing about Mova. This module
//! implements its [`Backend`](mova_nrepl::Backend) trait:
//!
//! ```text
//!  IO thread ──dispatch(eval)──▶ session FIFO ──▶ session thread (own Interp fork,
//!                │                                 own dynamic bindings)
//!                └─ no session ─▶ ephemeral pool ─▶ worker thread (fresh bindings per eval)
//!  boot thread ── builds the base Interp ──▶ latch ──▶ every thread forks from it
//! ```
//!
//! # How the interpreter is shared (this is how `future` works too)
//!
//! * `Interp::fork()` makes a cheap sibling that **shares** the global state:
//!   `globals` (every var's root value), namespaces, protocols, multimethods,
//!   keywords. A `def` on one thread is visible on all of them.
//! * Dynamic bindings are **per thread**: every `VarCell` keeps a binding stack
//!   per execution context (`ctx::current_ctx`, one per OS thread). A session
//!   thread pushes one frame per session var (`*1 *2 *3 *e`, `*print-length*`,
//!   `*ns*`, `*out*`, ...) when it starts and pops them when it ends. `set!`
//!   changes that thread's top frame, so it sticks in the session and does not
//!   leak into other sessions or into the root.
//! * So one session = one OS thread + one forked `Interp` + one binding frame.
//!   Nothing else is per session.
//!
//! # Files
//!
//! * `backend.rs`: the `Backend` impl, request parsing, boot latch.
//! * `bindings.rs`: the session var set; push / pop / snapshot of frames.
//! * `session_thread.rs`: session FIFO + thread; the ephemeral worker pool.
//! * `eval.rs`: the eval loop (forms, `*1`/`*e`, errors, value reply). It has
//!   one function, `reply_value`, that turns a value into reply messages:
//!   the seam for the P3 print/caught options.
//! * `output.rs`: the `out` / `err` writer (1024-byte chunk rule).
//! * `tooling.rs`, `completion.rs`, `lookup.rs`: the `completions` and `lookup`
//!   ops (design 5.6): one worker thread, native code over the var tables.

mod backend;
mod bindings;
mod completion;
mod eval;
pub mod forward;
mod input;
mod lookup;
mod middleware;
mod output;
mod print;
mod session_thread;
mod toplevel;
mod tooling;

pub use backend::{Config, ErrorMode, MovaBackend};
