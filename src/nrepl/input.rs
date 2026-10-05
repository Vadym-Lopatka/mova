//! `*in*` for a session (design 5.5).
//!
//! The `stdin` op appends to a queue; the evaluated code reads from it
//! through a `HostKind::InputStream` (`read-line`, `(.read *in*)`, `slurp`,
//! `read`). When a read finds the queue empty it sends `{status [need-input]}`
//! for the eval that runs now and sleeps on a condvar until data (or EOF)
//! arrives or the eval is interrupted. Every time a read has to block again,
//! for example in the middle of a line, it sends `need-input` again.
//!
//! An EOF (`stdin` with no or empty data) is a marker in the queue: exactly one
//! read call gets it (end of input), then the queue goes on as before, so a
//! later `stdin "x\n"` can still be read.

use crate::hostclass::{BoxedReader, HostInstVal, HostKind, HostState};
use crate::interrupt::Interrupt;
use crate::value::Value;
use mova_nrepl::{status, Responder};
use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Condvar, Mutex};

enum Item {
    Data(Vec<u8>),
    Eof,
}

struct State {
    items: VecDeque<Item>,
    /// The reply of the eval that runs now: `need-input` goes to its id.
    reply: Option<Responder>,
}

pub(crate) struct StdinQueue {
    st: Mutex<State>,
    cv: Condvar,
}

impl StdinQueue {
    pub(crate) fn new() -> Arc<StdinQueue> {
        Arc::new(StdinQueue { st: Mutex::new(State { items: VecDeque::new(), reply: None }), cv: Condvar::new() })
    }

    /// Appends text. Never blocks.
    pub(crate) fn add(&self, data: &[u8]) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.items.push_back(Item::Data(data.to_vec()));
        self.cv.notify_all();
    }

    /// `skip-stdin-newline`: drops a newline that starts the queued text.
    pub(crate) fn skip_newline(&self) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(Item::Data(d)) = st.items.front_mut() {
            if d.first() == Some(&b'\n') {
                d.remove(0);
                if d.is_empty() {
                    st.items.pop_front();
                }
            }
        }
    }

    pub(crate) fn add_eof(&self) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.items.push_back(Item::Eof);
        self.cv.notify_all();
    }

    pub(crate) fn set_reply(&self, r: Option<Responder>) {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).reply = r;
    }

    /// The `*in*` value for a session thread. `intr` is that thread's flag.
    pub(crate) fn stream_value(self: &Arc<Self>, intr: Arc<Interrupt>) -> Value {
        let r: Box<dyn Read + Send> = Box::new(QueueReader { q: self.clone(), intr });
        Value::HostInst(Arc::new(HostInstVal {
            kind: HostKind::InputStream,
            state: std::sync::Mutex::new(HostState::InputStream(BoxedReader(std::io::BufReader::with_capacity(256, r)))),
        }))
    }
}

struct QueueReader {
    q: Arc<StdinQueue>,
    intr: Arc<Interrupt>,
}

impl Read for QueueReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut st = self.q.st.lock().unwrap_or_else(|e| e.into_inner());
        let mut asked = false;
        loop {
            match st.items.front_mut() {
                Some(Item::Data(d)) => {
                    let n = d.len().min(buf.len());
                    buf[..n].copy_from_slice(&d[..n]);
                    d.drain(..n);
                    if d.is_empty() {
                        st.items.pop_front();
                    }
                    return Ok(n);
                }
                Some(Item::Eof) => {
                    st.items.pop_front();
                    return Ok(0);
                }
                None => {
                    if !asked {
                        asked = true;
                        if let Some(r) = &st.reply {
                            r.send_status(status::NEED_INPUT);
                        }
                    }
                    let (g, aborted) = crate::interrupt::wait_on(&self.intr, &self.q.cv, st);
                    st = g;
                    if aborted {
                        // `Other`, not `Interrupted`: callers retry on `Interrupted`.
                        return Err(std::io::Error::other("interrupted while waiting for input"));
                    }
                }
            }
        }
    }
}

/// An `*in*` that is at end of input (ephemeral requests have nobody to ask).
pub(crate) fn empty_stream() -> Value {
    let r: Box<dyn Read + Send> = Box::new(std::io::empty());
    Value::HostInst(Arc::new(HostInstVal {
        kind: HostKind::InputStream,
        state: std::sync::Mutex::new(HostState::InputStream(BoxedReader(std::io::BufReader::with_capacity(16, r)))),
    }))
}
