//! nREPL server for Mova: the wire layer.
//!
//! This crate is std + `libc` only. It does **not** depend on the `mova`
//! crate; the interpreter reaches it through the [`Backend`] trait, which the
//! `mova` binary implements (`mova` depends on `mova-nrepl`, never the
//! reverse).
//!
//! # Threads and ownership
//!
//! ```text
//!  sockets ─▶ [IO thread: kqueue/epoll] ─▶ bencode parse ─▶ router
//!                  ▲                                          │ native ops answered here
//!                  │ inbox + wake-up (other threads)          │ (describe clone close ls-sessions,
//!                  │ thread-local list (IO thread)            │  unknown op/session, eval arg checks)
//!                  │                                          ▼
//!             [write buffers] ◀── Responder/Outbox ◀── Backend::dispatch(Call)
//!                                                       (any thread replies, any time)
//! ```
//!
//! * **One IO thread** (`Server::run`, normally the process main thread) owns
//!   every socket, every read/write buffer and the connection table. There is
//!   no thread per connection or per request, and no timer: an idle server
//!   blocks in `kevent`/`epoll_wait` and uses no CPU.
//! * **The session table** ([`SessionTable`]) is the only shared map. The IO
//!   thread looks sessions up on every message; backends hold `Arc<Session>`.
//! * **Per-session state** belongs to the backend. A session carries a
//!   backend-owned `slot` (P2: the session thread, its FIFO of jobs) and an
//!   `interrupt` flag. The wire layer never looks inside either.
//!
//! # Backend contract
//!
//! The router answers `describe`, `clone`, `close`, `ls-sessions`, unknown
//! ops and unknown sessions itself. Everything else (`eval`, `load-file`,
//! `interrupt`, `stdin`, `completions`, `lookup`, `forward-system-output`) is
//! handed to [`Backend::dispatch`] as a [`Call`]:
//!
//! * `dispatch` runs **on the IO thread**. It must not block and must not
//!   run the interpreter. Do the minimum: copy what you need
//!   (`call.req.to_owned_request()`), queue a job for a session thread, return.
//!   A panic is caught and logged; the request then gets no reply.
//! * A request can have **any number of replies, over time, from any
//!   thread**. `call.reply` is a [`Responder`]: `Send + Clone`, it already
//!   knows the request `id` and the session id, and `reply.send(&[...])`
//!   encodes and queues one message (`out`, `value`, `err`, ... then
//!   `status: [done]`). Hold on to it as long as you like.
//! * Sending from another thread costs one lock and at most one wake-up
//!   syscall (wake-ups are coalesced). Sending from inside `dispatch` (IO
//!   thread) costs neither.
//! * [`Responder::is_open`] turns false when the client disconnects; queued
//!   replies for a closed connection are dropped.
//! * `call.session` is `None` for an *ephemeral* request (no `session` key);
//!   `reply.session_id()` is then the fresh id every reply of it must carry
//!   (the responder adds it for you).
//!
//! ```ignore
//! struct MyBackend;
//! impl Backend for MyBackend {
//!     fn dispatch(&self, call: Call<'_>) {
//!         let job = call.req.to_owned_request();      // owned copy
//!         let reply = call.reply;                      // Send + Clone
//!         std::thread::spawn(move || {
//!             let req = job.request();
//!             reply.send(&[("out", V::Str("hi\n"))]);
//!             reply.send(&[("ns", V::Str("user")), ("value", V::Str("3"))]);
//!             reply.send_status(status::DONE);
//!         });
//!     }
//! }
//! ```
//!
//! # Transports (design 5.9)
//!
//! The router, `Responder` and `Outbox` speak bencode only. A per-connection
//! [`Codec`] (`Server::with_codec`) translates at the edge, on the IO thread:
//! `Bencode` is the untouched fast path (one `match` on a `Copy` enum per
//! append); `Edn` converts each EDN request map to a bencode request and prints
//! each bencode reply as an EDN map; `Tty` reads one form at a time from text
//! lines and turns replies into text and a prompt (see [`transport`]).
//! Backend threads still push one encoded `Vec` per reply through the shared
//! inbox with at most one wake-up per burst; translation happens when the IO
//! thread appends to the write buffer, which is the one place a later
//! "shared per-connection byte buffer" change has to touch. TLS (`tls` cargo
//! feature) is a `Stream` variant of the same loop (`tls::TlsStream`).
//!
//! Other modules: [`cmdline`] (all flags, help text, `.nrepl.edn` config),
//! [`client`] (the small blocking client behind `--ack`, `--connect`,
//! `--interactive`), [`ack`], [`edn`].
//!
//! # Startup order (design 2.1)
//!
//! `Listeners::bind` (bind + listen) → print `Listeners::banner` → write
//! `.nrepl-port` → start the boot thread → `Server::new` → `Server::run`.
//! The interpreter is not touched before the banner. Connections that arrive
//! earlier wait in the kernel backlog.

pub mod ack;
pub mod bencode;
pub mod client;
pub mod cmdline;
pub mod describe;
pub mod edn;
pub mod io;
pub mod listen;
mod outbox;
pub mod poll;
mod reply;
mod router;
pub mod session;
#[cfg(feature = "tls")]
pub mod tls;
pub mod transport;

use std::sync::{Arc, OnceLock};

pub use bencode::{OwnedRequest, Request, Val, V};
pub use describe::Versions;
pub use io::{Server, ServerHandle};
pub use listen::{Endpoint, Listeners};
pub use outbox::{Outbox, Tap};
pub use reply::{status, Responder, TextFrame};
pub use transport::Codec;
pub use session::{Session, SessionId, SessionTable};

/// One request handed to the backend.
pub struct Call<'a> {
    /// The request, borrowed from the read buffer. Valid only inside `dispatch`.
    pub req: &'a Request<'a>,
    /// The session, or `None` for an ephemeral request.
    pub session: Option<Arc<Session>>,
    /// Sends replies for this request, from any thread.
    pub reply: Responder,
}

/// Handle to the router's native ops (`describe`, `clone`, `close`, ...) and to
/// `Backend::dispatch`, for a worker thread of the middleware lane.
#[derive(Clone)]
pub struct NativeLane {
    sessions: Arc<SessionTable>,
    backend: Arc<dyn Backend>,
}

impl NativeLane {
    pub(crate) fn new(sessions: Arc<SessionTable>, backend: Arc<dyn Backend>) -> NativeLane {
        NativeLane { sessions, backend }
    }

    /// Runs `req` as the router would for a request whose session was already
    /// resolved (`session`, `sid`). Native replies are appended to `out`; the
    /// replies of backend ops go to `outbox` (a [`Outbox::tap`]).
    pub fn route(&self, req: &Request<'_>, session: Option<Arc<Session>>, sid: SessionId, out: &mut Vec<u8>, outbox: &Outbox) {
        let r = router::Router { sessions: self.sessions.clone(), backend: self.backend.clone(), verbose: false, slow: false };
        r.route_resolved(req, session, sid, out, outbox);
    }

    /// The session table (for ids of sessions that exist).
    pub fn sessions(&self) -> &Arc<SessionTable> {
        &self.sessions
    }
}

/// The interpreter side. See the crate docs for the contract.
pub trait Backend: Send + Sync + 'static {
    /// Handles `eval`, `load-file`, `interrupt`, `stdin`, `completions`,
    /// `lookup` and `forward-system-output`. Runs on the IO thread: do not block.
    fn dispatch(&self, call: Call<'_>);

    /// What `describe` reports under `versions` (`clojure` and `java`).
    fn versions(&self) -> &Versions {
        static DEFAULT: OnceLock<Versions> = OnceLock::new();
        DEFAULT.get_or_init(Versions::default)
    }

    /// True when Mova-level middleware or a custom handler is configured. The
    /// router then hands **every** request (after the unknown-session check) to
    /// [`Backend::dispatch_slow`] instead of answering native ops itself. Read
    /// once, in `Server::new`. See the `middleware` module of the backend.
    fn middleware_lane(&self) -> bool {
        false
    }

    /// Middleware lane only: one request, any op. Runs on the IO thread: queue
    /// it for a worker. The worker answers natively through the [`NativeLane`]
    /// (given by `attach_lane`) once the handler stack lets the message through.
    fn dispatch_slow(&self, _call: Call<'_>) {}

    /// Middleware lane only: called once, before the first request.
    fn attach_lane(&self, _lane: NativeLane) {}

    /// A session was cloned. `parent` is `None` for a plain `clone` (fresh
    /// default bindings). Copy the parent's bindings, but not its `*ns*`:
    /// the child starts in `user`.
    fn session_cloned(&self, _parent: Option<&Arc<Session>>, _child: &Arc<Session>) {}

    /// A session was closed (already removed from the table). Stop its thread.
    fn session_closed(&self, _session: &Arc<Session>) {}
}
