//! Replies: status lists and the `Responder` a backend uses to answer.

use crate::bencode::{write_reply, Val, V};
use crate::outbox::Outbox;
use crate::session::SessionId;

/// Status lists, in the exact order the JVM sends them. The JVM builds these
/// from Clojure hash sets, so the order is not alphabetical and differs per
/// set; the wire goldens fix it. Copy these, never sort them.
pub mod status {
    pub const DONE: &[&str] = &["done"];
    pub const UNKNOWN_OP: &[&str] = &["done", "unknown-op", "error"];
    pub const UNKNOWN_SESSION: &[&str] = &["done", "unknown-session", "error"];
    pub const SESSION_CLOSED: &[&str] = &["done", "session-closed"];
    pub const NO_CODE: &[&str] = &["done", "no-code", "error"];
    pub const UNKNOWN_CODE_TYPE: &[&str] = &["done", "unknown-code-type", "error"];
    pub const NAMESPACE_NOT_FOUND: &[&str] = &["namespace-not-found", "done", "error"];
    pub const EVAL_ERROR: &[&str] = &["eval-error"];
    // interrupt (P3)
    pub const INTERRUPTED: &[&str] = &["done", "interrupted"];
    pub const SESSION_IDLE: &[&str] = &["done", "session-idle"];
    pub const INTERRUPT_ID_MISMATCH: &[&str] = &["done", "interrupt-id-mismatch", "error"];
    pub const SESSION_EPHEMERAL: &[&str] = &["session-ephemeral", "done", "error"];
    // stdin (P3)
    pub const NEED_INPUT: &[&str] = &["need-input"];
}

/// The fixed bytes around the text of a one-field reply. See `Responder::text_frame`.
#[derive(Clone, Debug)]
pub struct TextFrame {
    prefix: Vec<u8>,
    suffix: Vec<u8>,
}

/// Answers one request, from any thread, as many times as needed.
///
/// It owns what every reply of that request shares: the connection's
/// `Outbox`, the request `id` (echoed as it came on the wire; none if the
/// request had none) and the session id (the request's session, or the fresh
/// ephemeral id the router made for it). Clone it freely; it is `Send`.
#[derive(Clone, Debug)]
pub struct Responder {
    out: Outbox,
    id: Option<Box<[u8]>>,
    session: SessionId,
}

impl Responder {
    pub(crate) fn new(out: Outbox, id: Option<Val<'_>>, session: SessionId) -> Responder {
        Responder { out, id: id.map(|v| v.raw().into()), session }
    }

    /// Sends one reply dict: `fields` plus `id` and `session`. Keys are
    /// sorted by the encoder, so field order does not matter. Returns `false`
    /// if the connection is closed.
    pub fn send(&self, fields: &[(&str, V<'_>)]) -> bool {
        thread_local! {
            static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        SCRATCH.with(|s| match s.try_borrow_mut() {
            Ok(mut buf) => {
                buf.clear();
                write_reply(&mut buf, self.id.as_deref(), V::Bytes(self.session.as_bytes()), fields);
                let ok = self.out.append(&[&buf]);
                if buf.capacity() > 64 << 10 {
                    *buf = Vec::new();
                }
                ok
            }
            Err(_) => {
                let mut buf = Vec::with_capacity(96);
                write_reply(&mut buf, self.id.as_deref(), V::Bytes(self.session.as_bytes()), fields);
                self.out.send(buf)
            }
        })
    }

    /// The constant parts of a one-text-field reply (`out`, `err`) for this
    /// request: build once, then `send_text` per message.
    pub fn text_frame(&self, key: &str) -> TextFrame {
        let hdr = |k: &str| format!("{}:{}", k.len(), k).into_bytes();
        let mut id_part = Vec::new();
        if let Some(id) = &self.id {
            id_part.extend_from_slice(b"2:id");
            id_part.extend_from_slice(id);
        }
        let mut session_part = b"7:session".to_vec();
        session_part.extend_from_slice(format!("{}:", self.session.as_bytes().len()).as_bytes());
        session_part.extend_from_slice(self.session.as_bytes());
        // keys go out sorted: `id` < `session`, and `key` sits before or after `id`
        let (mut prefix, mut suffix) = (vec![b'd'], Vec::new());
        if key < "id" {
            prefix.extend_from_slice(&hdr(key));
            suffix.extend_from_slice(&id_part);
        } else {
            prefix.extend_from_slice(&id_part);
            prefix.extend_from_slice(&hdr(key));
        }
        suffix.extend_from_slice(&session_part);
        suffix.push(b'e');
        TextFrame { prefix, suffix }
    }

    /// Sends `{key: text, id, session}` using a frame from `text_frame`.
    /// Same bytes as `send(&[(key, V::Bytes(text))])`, with no sorting, no
    /// encoder and no allocation.
    pub fn send_text(&self, frame: &TextFrame, text: &[u8]) -> bool {
        let mut digits = [0u8; 21];
        let mut n = text.len();
        let mut i = digits.len();
        i -= 1;
        digits[i] = b':';
        loop {
            i -= 1;
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.out.append(&[&frame.prefix, &digits[i..], text, &frame.suffix])
    }

    /// Sends `{status: [...]}`. Use the lists in `status`.
    pub fn send_status(&self, status: &[&str]) -> bool {
        self.send(&[("status", V::Strs(status))])
    }

    /// The same responder with another request `id` (raw bencode of the id, as
    /// `id_raw` gives it, or none). `interrupt` answers for the interrupted
    /// eval's id on the interrupter's own connection.
    pub fn with_id(&self, id: Option<&[u8]>) -> Responder {
        Responder { out: self.out.clone(), id: id.map(|v| v.into()), session: self.session }
    }

    /// A responder for the same connection and session that has this `id`
    /// (raw bencode) and `session`.
    pub fn with_id_and_session(&self, id: Option<&[u8]>, session: SessionId) -> Responder {
        Responder { out: self.out.clone(), id: id.map(|v| v.into()), session }
    }

    pub fn outbox(&self) -> &Outbox {
        &self.out
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session
    }

    /// The request id as it came on the wire (encoded), if any.
    pub fn id_raw(&self) -> Option<&[u8]> {
        self.id.as_deref()
    }

    pub fn is_open(&self) -> bool {
        self.out.is_open()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::{ConnKey, Gate, Shared};
    use crate::poll::Poller;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn outbox() -> Outbox {
        let p = Poller::new().unwrap();
        let shared = Arc::new(Shared::new(p.waker()));
        Outbox::new(shared, ConnKey(7), Arc::new(AtomicBool::new(true)), Arc::new(Gate::new()))
    }

    fn drain(ob: &Outbox) -> Vec<u8> {
        let mut v = Vec::new();
        ob.take(&mut v);
        v
    }

    #[test]
    fn text_frame_is_byte_equal_to_the_encoder() {
        let sid = SessionId::new();
        for id in [Some(&b"3:abc"[..]), Some(&b"i12e"[..]), None] {
            for key in ["out", "err"] {
                for text in [&b""[..], b"x", b"hello\n", &[b'a'; 1500][..]] {
                    let ob = outbox();
                    let r = Responder { out: ob.clone(), id: id.map(|v| v.into()), session: sid };
                    r.send(&[(key, V::Bytes(text))]);
                    let a = drain(&ob);
                    let f = r.text_frame(key);
                    r.send_text(&f, text);
                    let b = drain(&ob);
                    assert_eq!(String::from_utf8_lossy(&a), String::from_utf8_lossy(&b), "key {key} id {id:?}");
                }
            }
        }
    }

    #[test]
    fn frames_from_many_threads_never_interleave() {
        let ob = outbox();
        let sid = SessionId::new();
        let mut hs = Vec::new();
        for t in 0..4u8 {
            let r = Responder { out: ob.clone(), id: Some(b"1:a".as_slice().into()), session: sid };
            hs.push(std::thread::spawn(move || {
                let f = r.text_frame("out");
                let text = vec![b'a' + t; 50 + t as usize];
                for _ in 0..500 {
                    r.send_text(&f, &text);
                    r.send(&[("out", V::Bytes(&text))]);
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        let all = drain(&ob);
        let mut pos = 0;
        let mut n = 0;
        while pos < all.len() {
            let (v, used) = crate::bencode::decode(&all[pos..]).unwrap().expect("whole frame");
            let t = v.get("out").and_then(|o| o.as_str()).unwrap().to_owned();
            assert!(t.bytes().all(|b| b == t.as_bytes()[0]), "frame {n} mixed: {t}");
            pos += used;
            n += 1;
        }
        assert_eq!(n, 4 * 1000);
    }
}
