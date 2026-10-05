//! `forward-system-output`: copies what the process writes to `System/out` and
//! `System/err` to the clients that asked for it (JVM: `nrepl.middleware.io` and
//! `nrepl.util.out`).
//!
//! * The op registers the requesting session: its connection gets every
//!   chunk as `{id <the op's id>, session <the session>, out|err <text>,
//!   source "system"}`. Registering again replaces; closing the session or the
//!   connection removes it. Without a session the op only answers `done`.
//! * `System/out` and `System/err` are `Tee` writers (see `builtins/io.rs`):
//!   they write to the real stream and hand the same bytes to [`tap`]. While
//!   nobody is registered, that costs one relaxed atomic load.
//! * Chunking is `CallbackBufferedOutputStream`'s: a buffer of 1024 bytes; it
//!   is sent when full (never inside a character), and up to the last `\n`
//!   after each write; `flush` sends the rest.
//! * `Mova` has no `*out*` root rebinding: `println` inside an eval goes to the
//!   eval's own `out` as before, and is not forwarded.

use mova_nrepl::{Responder, SessionId, V};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

const CHUNK: usize = 1024;

struct Target {
    session: SessionId,
    reply: Responder,
}

#[derive(Default)]
struct Chan {
    targets: Vec<Target>,
    buf: Vec<u8>,
}

#[derive(Default)]
struct State {
    out: Chan,
    err: Chan,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<State> = Mutex::new(State { out: Chan { targets: Vec::new(), buf: Vec::new() }, err: Chan { targets: Vec::new(), buf: Vec::new() } });

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts forwarding both streams to `reply` (the `forward-system-output`
/// request's responder), for `session`.
pub(crate) fn register(session: SessionId, reply: Responder) {
    let mut guard = lock();
    let st = &mut *guard;
    for ch in [&mut st.out, &mut st.err] {
        ch.targets.retain(|t| t.session != session);
        ch.targets.push(Target { session, reply: reply.clone() });
    }
    ACTIVE.store(true, Ordering::Release);
}

/// Stops forwarding for a session that was closed.
pub(crate) fn unregister(session: &SessionId) {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let mut guard = lock();
    let st = &mut *guard;
    for ch in [&mut st.out, &mut st.err] {
        ch.targets.retain(|t| &t.session != session);
    }
}

fn send(ch: &mut Chan, key: &'static str, text: &[u8]) {
    ch.targets.retain(|t| {
        t.reply.send(&[(key, V::Bytes(text)), ("source", V::Str("system"))]);
        t.reply.is_open()
    });
}

/// Called with bytes written to `System/out` or `System/err`.
fn tap(stream: Stream, mut bytes: &[u8]) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut st = lock();
    let (ch, key) = match stream {
        Stream::Out => (&mut st.out, "out"),
        Stream::Err => (&mut st.err, "err"),
    };
    if ch.targets.is_empty() {
        ch.buf.clear();
        return;
    }
    while !bytes.is_empty() {
        let room = CHUNK.saturating_sub(ch.buf.len()).max(1);
        let mut cut = room.min(bytes.len());
        // finish the character that crosses the line
        while cut < bytes.len() && (bytes[cut] & 0xC0) == 0x80 {
            cut += 1;
        }
        ch.buf.extend_from_slice(&bytes[..cut]);
        bytes = &bytes[cut..];
        let flush_to = if ch.buf.len() >= CHUNK { ch.buf.len() } else { ch.buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) };
        if flush_to > 0 {
            let rest = ch.buf.split_off(flush_to);
            let chunk = std::mem::replace(&mut ch.buf, rest);
            send(ch, key, &chunk);
        }
    }
}

fn tap_flush(stream: Stream) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut st = lock();
    let (ch, key) = match stream {
        Stream::Out => (&mut st.out, "out"),
        Stream::Err => (&mut st.err, "err"),
    };
    if !ch.buf.is_empty() {
        let chunk = std::mem::take(&mut ch.buf);
        send(ch, key, &chunk);
    }
}

/// A writer for `System/out` / `System/err`: writes to `inner`, and feeds the tap.
pub struct Tee<W: Write> {
    stream: Stream,
    inner: W,
}

impl<W: Write> Tee<W> {
    pub fn new(stream: Stream, inner: W) -> Tee<W> {
        Tee { stream, inner }
    }
}

impl<W: Write> Write for Tee<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        tap(self.stream, &buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()?;
        tap_flush(self.stream);
        Ok(())
    }
}
