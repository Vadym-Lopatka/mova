//! The IO thread: one loop for every connection.
//!
//! The loop blocks in `Poller::wait` with no timeout. It wakes for: a new
//! connection, bytes to read, room to write, or a wake-up from another thread
//! that queued a reply (see `outbox`). Nothing else runs on it. Idle cost is
//! zero: no timers, no polling.
//!
//! Reading: bytes go into one 64 KiB scratch buffer shared by all
//! connections. If a connection has no partial message, whole messages are
//! parsed straight from the scratch buffer; only a partial tail is copied into
//! the connection's own buffer. An idle connection owns no read buffer at all.
//!
//! Writing: replies to native ops are encoded straight into the connection's
//! write buffer and written with one `write` per batch of requests. Replies
//! queued by other threads arrive through the inbox. `EVFILT_WRITE`/`EPOLLOUT`
//! interest is on only while the write buffer is not empty. If a client does
//! not read and the buffer passes `HIGH_WATER`, reading from it pauses until it
//! drains. Replies pushed by backend threads are bounded by the per-connection
//! `Gate` (see `outbox`): the sending thread blocks, the IO thread does not.

use crate::bencode::{parse_request, DecodeError, Parsed, Request};
use crate::listen::Listeners;
use crate::outbox::{mark_io_thread, ConnKey, Gate, Outbox, Pending, Shared};
use crate::poll::{Event, Interest, Poller, BATCH, WAKE_TOKEN};
use crate::router::Router;
use crate::session::SessionTable;
use crate::transport::{bencode_replies_to_edn, edn_request_to_bencode, Codec, Tty};
use crate::Backend;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const TOKEN_TCP: u64 = 1 << 63 | 1;
const TOKEN_UNIX: u64 = 1 << 63 | 2;
const READ_CHUNK: usize = 64 * 1024;
const HIGH_WATER: usize = 1 << 20;
/// Buffers bigger than this are freed when they go empty.
const KEEP_CAPACITY: usize = 256 * 1024;

enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
    #[cfg(feature = "tls")]
    Tls(Box<crate::tls::TlsStream>),
}

impl Stream {
    fn fd(&self) -> RawFd {
        match self {
            Stream::Tcp(s) => s.as_raw_fd(),
            Stream::Unix(s) => s.as_raw_fd(),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.fd(),
        }
    }
    /// TLS only: ciphertext is waiting for socket room.
    fn wants_write(&self) -> bool {
        match self {
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.wants_write(),
            _ => false,
        }
    }
    /// TLS only: pushes ciphertext (handshake, records) to the socket.
    fn flush_out(&mut self) -> io::Result<()> {
        match self {
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.flush_out(),
            _ => Ok(()),
        }
    }
    /// A read may leave data inside the stream that the poller cannot see.
    fn read_until_blocked(&self) -> bool {
        match self {
            #[cfg(feature = "tls")]
            Stream::Tls(_) => true,
            _ => false,
        }
    }
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(buf),
            Stream::Unix(s) => s.read(buf),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.read(buf),
        }
    }
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.write(buf),
            Stream::Unix(s) => s.write(buf),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.write(buf),
        }
    }
}

/// Everything on the output side of a connection: the write buffer and the
/// translation of bencode replies for the connection's codec.
struct Egress {
    codec: Codec,
    /// Encoded replies not yet written; `wpos` bytes are already out.
    wbuf: Vec<u8>,
    wpos: usize,
    /// TTY state (only for `Codec::Tty`).
    tty: Option<Box<Tty>>,
    /// Scratch for native replies that need translating.
    tmp: Vec<u8>,
    /// Non-bencode codecs: bytes ever appended / ever written, and the gate
    /// credit owed once the bytes of a remote reply have reached the socket.
    queued: u64,
    written: u64,
    credits: VecDeque<(u64, usize)>,
    /// Empty buffer swapped with the outbox's on each drain, so its capacity is reused.
    spare: Vec<u8>,
}

impl Egress {
    fn new(codec: Codec) -> Egress {
        Egress {
            codec,
            wbuf: Vec::new(),
            wpos: 0,
            tty: (codec == Codec::Tty).then(|| Box::new(Tty::new())),
            tmp: Vec::new(),
            queued: 0,
            written: 0,
            credits: VecDeque::new(),
            spare: Vec::new(),
        }
    }

    fn pending(&self) -> usize {
        self.wbuf.len() - self.wpos
    }

    /// Adds bytes to the write buffer and leaves `bytes` empty. Into an empty
    /// buffer the bytes are adopted without a copy (the two vectors swap).
    fn queue(&mut self, bytes: &mut Vec<u8>) {
        if self.pending() == 0 {
            std::mem::swap(&mut self.wbuf, bytes);
            self.wpos = 0;
        } else {
            if self.wpos > 0 && self.wpos >= self.wbuf.len() / 2 {
                self.wbuf.drain(..self.wpos);
                self.wpos = 0;
            }
            self.wbuf.extend_from_slice(bytes);
        }
        bytes.clear();
    }

    /// Appends bencode replies translated for the codec.
    fn translate(&mut self, bencode: &[u8]) {
        let before = self.wbuf.len();
        match self.codec {
            Codec::Bencode => self.wbuf.extend_from_slice(bencode),
            Codec::Edn => {
                bencode_replies_to_edn(bencode, &mut self.wbuf);
            }
            Codec::Tty => {
                if let Some(t) = self.tty.as_mut() {
                    t.replies(bencode, &mut self.wbuf);
                }
            }
        }
        self.queued += (self.wbuf.len() - before) as u64;
    }

    /// A reply from the backend (any thread). Returns the gate credit to give
    /// back at once (`0` if it is owed until the bytes are written).
    /// `bytes` is left empty (its capacity is for the caller to reuse).
    fn queue_reply(&mut self, bytes: &mut Vec<u8>, remote: bool, remote_unwritten: &mut usize) -> usize {
        let len = bytes.len();
        if self.codec == Codec::Bencode {
            if remote {
                *remote_unwritten += len;
            }
            self.queue(bytes);
            return 0;
        }
        let before = self.queued;
        self.translate(bytes);
        bytes.clear();
        if !remote {
            return 0;
        }
        if self.queued == before {
            return len; // nothing to write: credit now
        }
        self.credits.push_back((self.queued, len));
        0
    }

    /// Routes one request; the replies it makes at once land in the write buffer.
    fn route(&mut self, req: &Request<'_>, router: &Router, outbox: &Outbox) {
        if self.codec == Codec::Bencode {
            router.route(req, &mut self.wbuf, outbox);
        } else {
            let mut tmp = std::mem::take(&mut self.tmp);
            tmp.clear();
            router.route(req, &mut tmp, outbox);
            self.translate(&tmp);
            self.tmp = tmp;
        }
    }

    /// TTY: issues the next `eval` while one is ready.
    fn pump_tty(&mut self, router: &Router, outbox: &Outbox) {
        while self.tty.as_ref().is_some_and(|t| t.need_pump) {
            let Some(bytes) = self.tty.as_mut().and_then(|t| t.next_request()) else { continue };
            if let Ok(Parsed::Message(req, _)) = parse_request(&bytes) {
                self.route(&req, router, outbox);
            }
        }
    }

    /// TTY: greeting and the implicit `clone`.
    fn start_tty(&mut self, router: &Router, outbox: &Outbox) {
        let g = Tty::greeting(&router.backend.versions().clojure.version_string);
        self.wbuf.extend_from_slice(&g);
        self.queued += g.len() as u64;
        let clone = Tty::clone_request();
        if let Ok(Parsed::Message(req, _)) = parse_request(&clone) {
            self.route(&req, router, outbox);
        }
        self.pump_tty(router, outbox);
    }
}

struct Conn {
    stream: Stream,
    token: u64,
    open: Arc<AtomicBool>,
    outbox: Outbox,
    /// Partial message, kept between reads. Empty when idle.
    rbuf: Vec<u8>,
    /// `rbuf` must hold at least this many bytes before parsing is retried.
    want: usize,
    eg: Egress,
    /// EDN requests are converted to bencode here before routing.
    conv: Vec<u8>,
    interest: Interest,
    /// Queued for a flush in `deliver`.
    dirty: bool,
    /// Bytes of `wbuf[wpos..]` that came from other threads and are counted
    /// in the `Gate` (credited back as they reach the socket). Bencode only.
    remote_unwritten: usize,
}

#[derive(Debug)]
enum Close {
    Eof,
    Io(io::Error),
    Malformed(DecodeError),
}

impl Conn {
    fn pending(&self) -> usize {
        self.eg.pending()
    }

    /// Writes as much as the socket takes.
    fn flush(&mut self) -> io::Result<()> {
        let eg = &mut self.eg;
        while eg.wpos < eg.wbuf.len() {
            match self.stream.write(&eg.wbuf[eg.wpos..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    eg.wpos += n;
                    if eg.codec == Codec::Bencode {
                        let credit = n.min(self.remote_unwritten);
                        self.remote_unwritten -= credit;
                        self.outbox.gate().release(credit);
                    } else {
                        eg.written += n as u64;
                        while let Some(&(end, len)) = eg.credits.front() {
                            if end > eg.written {
                                break;
                            }
                            eg.credits.pop_front();
                            self.outbox.gate().release(len);
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if eg.wpos == eg.wbuf.len() {
            eg.wpos = 0;
            if eg.wbuf.capacity() > KEEP_CAPACITY {
                eg.wbuf = Vec::new();
            } else {
                eg.wbuf.clear();
            }
        }
        self.stream.flush_out()
    }

    /// Brings the poller's interest in line with the buffers.
    fn sync(&mut self, poller: &Poller) -> io::Result<()> {
        let pending = self.pending();
        let want = Interest { read: pending <= HIGH_WATER, write: pending > 0 || self.stream.wants_write() };
        if want != self.interest {
            poller.modify(self.stream.fd(), self.token, self.interest, want)?;
            self.interest = want;
        }
        Ok(())
    }

    /// Reads what is there and answers it.
    fn on_readable(&mut self, router: &Router, scratch: &mut [u8]) -> Result<(), Close> {
        loop {
            let n = match self.stream.read(scratch) {
                Ok(0) => {
                    let _ = self.flush(); // best effort: answers already made
                    return Err(Close::Eof);
                }
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Close::Io(e)),
            };
            if let Err(e) = self.feed(&scratch[..n], router) {
                let _ = self.flush(); // answers to the good messages before the junk
                return Err(Close::Malformed(e));
            }
            if n < scratch.len() && !self.stream.read_until_blocked() {
                break; // drained; level-triggered polling tells us if more comes
            }
        }
        self.flush().map_err(Close::Io)
    }

    fn feed(&mut self, data: &[u8], router: &Router) -> Result<(), DecodeError> {
        match self.eg.codec {
            Codec::Bencode => self.feed_bencode(data, router),
            Codec::Edn => self.feed_edn(data, router),
            Codec::Tty => {
                if let Some(t) = self.eg.tty.as_mut() {
                    t.inbuf.extend_from_slice(data);
                    t.need_pump = true;
                }
                self.eg.pump_tty(router, &self.outbox);
                Ok(())
            }
        }
    }

    fn feed_bencode(&mut self, data: &[u8], router: &Router) -> Result<(), DecodeError> {
        if self.rbuf.is_empty() {
            let (used, need) = run_messages(data, &mut self.eg.wbuf, &self.outbox, router)?;
            if used < data.len() {
                self.rbuf.reserve(need.max(data.len() - used));
                self.rbuf.extend_from_slice(&data[used..]);
                self.want = need;
            }
        } else {
            self.rbuf.extend_from_slice(data);
            if self.rbuf.len() < self.want {
                return Ok(());
            }
            let (used, need) = run_messages(&self.rbuf, &mut self.eg.wbuf, &self.outbox, router)?;
            self.rbuf.drain(..used);
            self.want = need;
            if self.rbuf.is_empty() && self.rbuf.capacity() > KEEP_CAPACITY {
                self.rbuf = Vec::new();
            }
        }
        Ok(())
    }

    fn feed_edn(&mut self, data: &[u8], router: &Router) -> Result<(), DecodeError> {
        self.rbuf.extend_from_slice(data);
        let mut pos = 0;
        loop {
            self.conv.clear();
            match edn_request_to_bencode(&self.rbuf[pos..], &mut self.conv).map_err(DecodeError)? {
                None => break,
                Some(used) => {
                    pos += used;
                    if let Parsed::Message(req, _) = parse_request(&self.conv)? {
                        self.eg.route(&req, router, &self.outbox);
                    }
                }
            }
        }
        self.rbuf.drain(..pos);
        if self.rbuf.is_empty() && self.rbuf.capacity() > KEEP_CAPACITY {
            self.rbuf = Vec::new();
        }
        Ok(())
    }
}

/// Parses and routes every complete message in `buf`. Returns the bytes used
/// and, for a partial tail, how many bytes that tail needs before it is worth
/// parsing again (0 if there is no tail).
fn run_messages(buf: &[u8], out: &mut Vec<u8>, outbox: &Outbox, router: &Router) -> Result<(usize, usize), DecodeError> {
    let mut pos = 0;
    while pos < buf.len() {
        match parse_request(&buf[pos..])? {
            Parsed::Message(req, used) => {
                router.route(&req, out, outbox);
                pos += used;
            }
            Parsed::Need(n) => return Ok((pos, n)),
        }
    }
    Ok((pos, 0))
}

struct Slot {
    gen: u32,
    conn: Option<Conn>,
}

/// Handle to stop a running server from another thread (tests, embedding).
#[derive(Clone)]
pub struct ServerHandle {
    shared: Arc<Shared>,
}

impl ServerHandle {
    /// Makes `Server::run` return. Open connections are closed.
    pub fn shutdown(&self) {
        self.shared.request_stop();
    }
}

/// The server: listeners, the router, and the connection table. `run` is the
/// IO thread.
pub struct Server {
    poller: Poller,
    listeners: Listeners,
    router: Router,
    shared: Arc<Shared>,
    slots: Vec<Slot>,
    free: Vec<u32>,
    dirty: Vec<usize>,
    codec: Codec,
    #[cfg(feature = "tls")]
    tls: Option<Arc<crate::tls::TlsConfig>>,
}

impl Server {
    /// Creates the poller. Cheap; call it after the banner and port file.
    pub fn new(listeners: Listeners, backend: Arc<dyn Backend>, verbose: bool) -> io::Result<Server> {
        let poller = Poller::new()?;
        let shared = Arc::new(Shared::new(poller.waker()));
        let sessions = Arc::new(SessionTable::new());
        let slow = backend.middleware_lane();
        if slow {
            backend.attach_lane(crate::NativeLane::new(sessions.clone(), backend.clone()));
        }
        let router = Router { sessions, backend, verbose, slow };
        Ok(Server {
            poller,
            listeners,
            router,
            shared,
            slots: Vec::new(),
            free: Vec::new(),
            dirty: Vec::new(),
            codec: Codec::Bencode,
            #[cfg(feature = "tls")]
            tls: None,
        })
    }

    /// The transport of every connection (`-t/--transport`). Default bencode.
    pub fn with_codec(mut self, codec: Codec) -> Server {
        self.codec = codec;
        self
    }

    /// Serve TLS (TCP only): every connection needs a client certificate.
    #[cfg(feature = "tls")]
    pub fn with_tls(mut self, tls: Arc<crate::tls::TlsConfig>) -> Server {
        self.tls = Some(tls);
        self
    }

    pub fn handle(&self) -> ServerHandle {
        ServerHandle { shared: self.shared.clone() }
    }

    pub fn sessions(&self) -> Arc<SessionTable> {
        self.router.sessions.clone()
    }

    /// Runs the IO loop on the calling thread until `shutdown` is called
    /// (or the poller fails).
    pub fn run(mut self) -> io::Result<()> {
        mark_io_thread();
        if let Some(l) = &self.listeners.tcp {
            self.poller.add(l.as_raw_fd(), TOKEN_TCP, Interest::READ)?;
        }
        if let Some(l) = &self.listeners.unix {
            self.poller.add(l.as_raw_fd(), TOKEN_UNIX, Interest::READ)?;
        }
        let mut events: Vec<Event> = Vec::with_capacity(BATCH);
        let mut scratch = vec![0u8; READ_CHUNK].into_boxed_slice();
        let mut batch: Pending = Vec::new();
        let mut keys: Vec<ConnKey> = Vec::new();
        loop {
            // Block only when no sender has queued anything; otherwise just
            // look at the sockets and go on (a sender that comes in while we
            // work rings no bell).
            if self.shared.prepare_sleep() {
                self.poller.wait(&mut events)?;
                self.shared.awake();
            } else {
                self.poller.poll_now(&mut events)?;
            }
            for ev in &events {
                match ev.token {
                    WAKE_TOKEN => {}
                    TOKEN_TCP => self.accept(true),
                    TOKEN_UNIX => self.accept(false),
                    t => self.conn_event(t, ev, &mut scratch),
                }
            }
            // Replies the backend queued on this thread while handling requests
            // go first. A handler may send a reply and then wake another thread
            // (an `interrupt` answers `interrupted`, then wakes the eval, which
            // prints): what the handler sent happened before whatever the woken
            // thread sends, so it must reach the wire first.
            self.shared.take_local(&mut batch);
            self.deliver(&mut batch, false);
            // replies other threads queued
            self.shared.take_dirty(&mut keys);
            self.deliver_remote(&mut keys);
            if self.shared.stopping() {
                for i in 0..self.slots.len() {
                    self.close(i, None);
                }
                return Ok(());
            }
        }
    }

    fn log(&self, msg: std::fmt::Arguments<'_>) {
        if self.router.verbose {
            eprintln!("nrepl: {msg}");
        }
    }

    fn accept(&mut self, tcp: bool) {
        // Level-triggered: if we stop early, the poller reports it again.
        for _ in 0..64 {
            let r = if tcp {
                self.listeners.tcp.as_ref().map(|l| l.accept().and_then(|(s, _)| self.wrap_tcp(s)))
            } else {
                self.listeners.unix.as_ref().map(|l| l.accept().map(|(s, _)| Stream::Unix(s)))
            };
            match r {
                Some(Ok(stream)) => self.add_conn(stream),
                Some(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Some(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => return,
                Some(Err(e)) => {
                    self.log(format_args!("accept failed: {e}"));
                    return;
                }
                None => return,
            }
        }
    }

    #[cfg(feature = "tls")]
    fn wrap_tcp(&self, s: TcpStream) -> io::Result<Stream> {
        match &self.tls {
            Some(cfg) => Ok(Stream::Tls(Box::new(crate::tls::TlsStream::accept(s, cfg)?))),
            None => Ok(Stream::Tcp(s)),
        }
    }

    #[cfg(not(feature = "tls"))]
    fn wrap_tcp(&self, s: TcpStream) -> io::Result<Stream> {
        Ok(Stream::Tcp(s))
    }

    fn add_conn(&mut self, stream: Stream) {
        let ok = match &stream {
            Stream::Tcp(s) => s.set_nonblocking(true).is_ok() && s.set_nodelay(true).is_ok(),
            Stream::Unix(s) => s.set_nonblocking(true).is_ok(),
            #[cfg(feature = "tls")]
            Stream::Tls(s) => s.set_nonblocking(),
        };
        if !ok {
            return;
        }
        let idx = match self.free.pop() {
            Some(i) => i as usize,
            None => {
                self.slots.push(Slot { gen: 0, conn: None });
                self.slots.len() - 1
            }
        };
        let key = ConnKey(((self.slots[idx].gen as u64) << 32) | idx as u64);
        let open = Arc::new(AtomicBool::new(true));
        let gate = Arc::new(Gate::new());
        if self.poller.add(stream.fd(), key.0, Interest::READ).is_err() {
            self.free.push(idx as u32);
            return;
        }
        let mut conn = Conn {
            outbox: Outbox::new(self.shared.clone(), key, open.clone(), gate),
            stream,
            token: key.0,
            open,
            rbuf: Vec::new(),
            want: 0,
            eg: Egress::new(self.codec),
            conv: Vec::new(),
            interest: Interest::READ,
            dirty: false,
            remote_unwritten: 0,
        };
        if self.codec == Codec::Tty {
            conn.eg.start_tty(&self.router, &conn.outbox);
            let res = conn.flush().and_then(|_| conn.sync(&self.poller));
            if res.is_err() {
                self.poller.remove(conn.stream.fd(), conn.interest);
                self.free.push(idx as u32);
                return;
            }
        }
        self.slots[idx].conn = Some(conn);
        self.log(format_args!("connection {idx} opened"));
    }

    fn conn_event(&mut self, token: u64, ev: &Event, scratch: &mut [u8]) {
        let idx = (token & 0xffff_ffff) as usize;
        let gen = (token >> 32) as u32;
        let Some(slot) = self.slots.get_mut(idx) else { return };
        if slot.gen != gen {
            return; // stale event for a connection that was closed in this batch
        }
        let Some(conn) = slot.conn.as_mut() else { return };
        let mut res: Result<(), Close> = Ok(());
        if ev.readable {
            res = conn.on_readable(&self.router, scratch);
        }
        if res.is_ok() && ev.writable {
            res = conn.flush().map_err(Close::Io);
        }
        if res.is_ok() {
            res = conn.sync(&self.poller).map_err(Close::Io);
        }
        if let Err(why) = res {
            self.close(idx, Some(why));
        }
    }

    /// Writes what other threads queued for the connections in `keys`.
    fn deliver_remote(&mut self, keys: &mut Vec<ConnKey>) {
        if keys.is_empty() {
            return;
        }
        for key in keys.drain(..) {
            let idx = (key.0 & 0xffff_ffff) as usize;
            let gen = (key.0 >> 32) as u32;
            if let Some(slot) = self.slots.get_mut(idx) {
                if let (true, Some(conn)) = (slot.gen == gen, slot.conn.as_mut()) {
                    let mut bytes = std::mem::take(&mut conn.eg.spare);
                    conn.outbox.take(&mut bytes);
                    if !bytes.is_empty() {
                        let credit = conn.eg.queue_reply(&mut bytes, true, &mut conn.remote_unwritten);
                        if credit > 0 {
                            conn.outbox.gate().release(credit);
                        }
                        conn.eg.pump_tty(&self.router, &conn.outbox);
                    }
                    if bytes.capacity() <= KEEP_CAPACITY {
                        conn.eg.spare = bytes;
                    }
                    if !conn.dirty {
                        conn.dirty = true;
                        self.dirty.push(idx);
                    }
                }
            }
        }
        self.flush_dirty();
    }

    /// Writes replies the backend queued on the IO thread.
    fn deliver(&mut self, batch: &mut Pending, remote: bool) {
        if batch.is_empty() {
            return;
        }
        for (key, mut bytes) in batch.drain(..) {
            let idx = (key.0 & 0xffff_ffff) as usize;
            let gen = (key.0 >> 32) as u32;
            if let Some(slot) = self.slots.get_mut(idx) {
                if let (true, Some(conn)) = (slot.gen == gen, slot.conn.as_mut()) {
                    let credit = conn.eg.queue_reply(&mut bytes, remote, &mut conn.remote_unwritten);
                    if credit > 0 {
                        conn.outbox.gate().release(credit);
                    }
                    conn.eg.pump_tty(&self.router, &conn.outbox);
                    if !conn.dirty {
                        conn.dirty = true;
                        self.dirty.push(idx);
                    }
                }
                // else: connection closed; the reply is dropped
            }
        }
        self.flush_dirty();
    }

    fn flush_dirty(&mut self) {
        let mut dirty = std::mem::take(&mut self.dirty);
        for idx in dirty.drain(..) {
            let Some(conn) = self.slots[idx].conn.as_mut() else { continue };
            conn.dirty = false;
            let res = conn.flush().and_then(|_| conn.sync(&self.poller));
            if let Err(e) = res {
                self.close(idx, Some(Close::Io(e)));
            }
        }
        self.dirty = dirty;
    }

    fn close(&mut self, idx: usize, why: Option<Close>) {
        let Some(slot) = self.slots.get_mut(idx) else { return };
        let Some(conn) = slot.conn.take() else { return };
        conn.open.store(false, std::sync::atomic::Ordering::Release);
        conn.outbox.gate().close();
        // TTY: the session it made goes with the connection (the JVM sends `close`)
        if let Some(bytes) = conn.eg.tty.as_ref().and_then(|t| t.close_request()) {
            if let Ok(Parsed::Message(req, _)) = parse_request(&bytes) {
                let mut sink = Vec::new();
                self.router.route(&req, &mut sink, &conn.outbox);
            }
        }
        self.poller.remove(conn.stream.fd(), conn.interest);
        slot.gen = (slot.gen + 1) & 0x7fff_ffff;
        self.free.push(idx as u32);
        match why {
            Some(Close::Malformed(e)) => self.log(format_args!("connection {idx} closed: {e}")),
            Some(Close::Io(e)) => self.log(format_args!("connection {idx} closed: {e}")),
            Some(Close::Eof) | None => self.log(format_args!("connection {idx} closed")),
        }
    }
}
