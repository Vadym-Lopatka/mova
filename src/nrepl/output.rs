//! `*out*` / `*err*` capture for one eval (design 5.3, "out chunk rule").
//!
//! A [`Sink`] collects what the evaluated code writes, in a UTF-8 byte buffer
//! of 1024. Rule (checked against the JVM goldens `c01`, `c02`):
//!
//! * chars are appended whole; as soon as the buffer holds **at least** 1024
//!   bytes it is sent (so a chunk of 3-byte chars is 1026 bytes, never a split
//!   char);
//! * `println` / `prn` / `newline` and `(flush)` send what is buffered;
//!   `print` / `pr` do not (they wait for a flush, the end of the form, or a
//!   full buffer);
//! * the evaluator flushes `*err*` then `*out*` before each form's `value`
//!   and once more when the eval ends.
//!
//! A sink keeps the [`Responder`] of the eval that made it, so output from a
//! `future` that outlives the eval still carries that eval's `id`.
//!
//! The `*out*` value is a `HostKind::OutputStream` whose writer is
//! [`SinkWriter`]. The interpreter's `print` flushes the stream itself after
//! each write; `SinkWriter::flush` tells that apart from a real flush with
//! `builtins::strings::in_auto_flush`.

use crate::hostclass::{BoxedWriter, HostInstVal, HostKind, HostState};
use crate::value::Value;
use mova_nrepl::{Responder, TextFrame, V};
use std::io::Write;
use std::sync::{Arc, Mutex};

/// Bytes after which a chunk is sent.
const CHUNK: usize = 1024;

struct State {
    buf: Vec<u8>,
    reply: Responder,
    /// The constant bytes of an `out`/`err` frame (none when `keys` adds fields).
    frame: Option<TextFrame>,
    /// Bytes after which a chunk is sent (`CHUNK`, or the request's `out-limit`).
    limit: usize,
}

pub(crate) struct Sink {
    key: &'static str,
    /// Fields added to every message (the `keys` request option).
    extra: Vec<(String, String)>,
    st: Mutex<State>,
}

impl Sink {
    /// `key` is the reply field: `"out"` or `"err"`.
    pub(crate) fn new(key: &'static str, reply: Responder, extra: Vec<(String, String)>) -> Arc<Sink> {
        let frame = extra.is_empty().then(|| reply.text_frame(key));
        Arc::new(Sink { key, extra, st: Mutex::new(State { buf: Vec::new(), reply, frame, limit: CHUNK }) })
    }

    /// The request's `out-limit`: the buffer size in bytes (at least 1).
    pub(crate) fn set_limit(&self, n: usize) {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).limit = n.max(1);
    }

    /// The `*out*` / `*err*` value that writes into this sink.
    ///
    /// `BufWriter` with capacity 0 passes every write straight through: no
    /// 8 KiB buffer per eval, and `.write` + `(flush)` behave as on the JVM.
    pub(crate) fn stream_value(self: &Arc<Self>) -> Value {
        let w: Box<dyn Write + Send> = Box::new(SinkWriter(self.clone()));
        Value::HostInst(Arc::new(HostInstVal {
            kind: HostKind::OutputStream,
            state: Mutex::new(HostState::OutputStream(BoxedWriter(std::io::BufWriter::with_capacity(0, w)))),
        }))
    }

    /// Appends text under the chunk rule.
    pub(crate) fn write_bytes(&self, mut s: &[u8]) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        while !s.is_empty() {
            let room = st.limit.saturating_sub(st.buf.len()).max(1);
            if s.len() < room {
                st.buf.extend_from_slice(s);
                return;
            }
            // Fill to 1024 and finish the char that crosses the line.
            let mut cut = room;
            while cut < s.len() && (s[cut] & 0xC0) == 0x80 {
                cut += 1;
            }
            st.buf.extend_from_slice(&s[..cut]);
            s = &s[cut..];
            self.send_locked(&mut st);
        }
    }

    /// `write_bytes`, then `flush` when asked: one lock for both (what
    /// `println` to this sink does).
    pub(crate) fn write_and_flush(&self, s: &str, flush: bool) {
        let mut s = s.as_bytes();
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        while !s.is_empty() {
            let room = st.limit.saturating_sub(st.buf.len()).max(1);
            if s.len() < room {
                st.buf.extend_from_slice(s);
                break;
            }
            let mut cut = room;
            while cut < s.len() && (s[cut] & 0xC0) == 0x80 {
                cut += 1;
            }
            st.buf.extend_from_slice(&s[..cut]);
            s = &s[cut..];
            self.send_locked(&mut st);
        }
        if flush && !st.buf.is_empty() {
            self.send_locked(&mut st);
        }
    }

    pub(crate) fn write_text(&self, s: &str) {
        self.write_bytes(s.as_bytes());
    }

    /// Sends what is buffered, if anything.
    pub(crate) fn flush(&self) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if !st.buf.is_empty() {
            self.send_locked(&mut st);
        }
    }

    fn send_locked(&self, st: &mut State) {
        // `buf` is whole UTF-8 (chars are never split); bytes go out as they are.
        if let Some(frame) = &st.frame {
            st.reply.send_text(frame, &st.buf);
        } else {
            let mut f: Vec<(&str, V<'_>)> = vec![(self.key, V::Bytes(&st.buf))];
            for (k, v) in &self.extra {
                f.retain(|(n, _)| n != k);
                f.push((k.as_str(), V::Str(v)));
            }
            st.reply.send(&f);
        }
        st.buf.clear();
    }
}

impl crate::builtins::strings::FastOut for Sink {
    fn write_flush(&self, s: &str, flush: bool) {
        self.write_and_flush(s, flush);
    }
}

struct SinkWriter(Arc<Sink>);

impl Write for SinkWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.write_bytes(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !crate::builtins::strings::in_auto_flush() {
            self.0.flush();
        }
        Ok(())
    }
}
