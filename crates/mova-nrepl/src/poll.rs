//! A small readiness poller: kqueue on macOS, epoll on Linux.
//!
//! Level-triggered. `wait` blocks with no timeout, so an idle server uses no
//! CPU. `Waker::wake` can be called from any thread and makes `wait` return
//! an event with `token == WAKE_TOKEN`.
//!
//! The only `unsafe` here is the libc calls; each has a short safety note.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

/// Token reported for a wake-up.
pub const WAKE_TOKEN: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Interest {
    pub read: bool,
    pub write: bool,
}

impl Interest {
    pub const READ: Interest = Interest { read: true, write: false };
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Event {
    pub token: u64,
    pub readable: bool,
    pub writable: bool,
    /// Peer closed or error. Reading will tell which.
    pub hup: bool,
}

/// How many events one `wait` can return.
pub const BATCH: usize = 64;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    pub struct Poller {
        kq: Arc<OwnedFd>,
    }

    pub struct Waker {
        kq: Arc<OwnedFd>,
    }

    fn kev(ident: usize, filter: i16, flags: u16, fflags: u32, token: u64) -> libc::kevent {
        libc::kevent { ident, filter, flags, fflags, data: 0, udata: token as usize as *mut libc::c_void }
    }

    fn apply(kq: RawFd, changes: &[libc::kevent]) -> io::Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        // SAFETY: `changes` is a valid slice for the length given; no event
        // list is requested, so nothing is written.
        let r = unsafe {
            libc::kevent(kq, changes.as_ptr(), changes.len() as libc::c_int, std::ptr::null_mut(), 0, std::ptr::null())
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    impl Poller {
        pub fn new() -> io::Result<Poller> {
            // SAFETY: plain syscall; the result is checked before use.
            let fd = unsafe { libc::kqueue() };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is a fresh, valid descriptor that nothing else owns.
            let kq = unsafe { OwnedFd::from_raw_fd(fd) };
            // close-on-exec, so spawned children do not inherit it
            // SAFETY: fcntl on a valid fd.
            unsafe { libc::fcntl(kq.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
            let p = Poller { kq: Arc::new(kq) };
            apply(
                p.kq.as_raw_fd(),
                &[kev(0, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0, WAKE_TOKEN)],
            )?;
            Ok(p)
        }

        pub fn waker(&self) -> Waker {
            Waker { kq: self.kq.clone() }
        }

        pub fn add(&self, fd: RawFd, token: u64, i: Interest) -> io::Result<()> {
            self.modify(fd, token, Interest::default(), i)
        }

        /// Moves the registration of `fd` from `old` to `new` interest.
        pub fn modify(&self, fd: RawFd, token: u64, old: Interest, new: Interest) -> io::Result<()> {
            let mut ch: [libc::kevent; 2] = [kev(0, 0, 0, 0, 0); 2];
            let mut n = 0;
            let mut put = |filter: i16, was: bool, now: bool| {
                if now && !was {
                    ch[n] = kev(fd as usize, filter, libc::EV_ADD | libc::EV_ENABLE, 0, token);
                    n += 1;
                } else if was && !now {
                    ch[n] = kev(fd as usize, filter, libc::EV_DELETE, 0, token);
                    n += 1;
                }
            };
            put(libc::EVFILT_READ, old.read, new.read);
            put(libc::EVFILT_WRITE, old.write, new.write);
            apply(self.kq.as_raw_fd(), &ch[..n])
        }

        /// Closing the fd removes its filters; nothing to do.
        pub fn remove(&self, _fd: RawFd, _old: Interest) {}

        /// Blocks until something is ready. `events` is cleared first.
        pub fn wait(&self, events: &mut Vec<Event>) -> io::Result<()> {
            self.wait_inner(events, true)
        }

        /// Like `wait`, but returns at once when nothing is ready.
        pub fn poll_now(&self, events: &mut Vec<Event>) -> io::Result<()> {
            self.wait_inner(events, false)
        }

        fn wait_inner(&self, events: &mut Vec<Event>, block: bool) -> io::Result<()> {
            events.clear();
            let mut raw: [libc::kevent; BATCH] = [kev(0, 0, 0, 0, 0); BATCH];
            let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            let timeout: *const libc::timespec = if block { std::ptr::null() } else { &zero };
            // SAFETY: `raw` has room for BATCH events; a null timeout means block.
            let n = unsafe {
                libc::kevent(
                    self.kq.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    raw.as_mut_ptr(),
                    BATCH as libc::c_int,
                    timeout,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                return if e.kind() == io::ErrorKind::Interrupted { Ok(()) } else { Err(e) };
            }
            for k in &raw[..n as usize] {
                let token = k.udata as usize as u64;
                let hup = k.flags & libc::EV_EOF != 0;
                match k.filter {
                    libc::EVFILT_READ => events.push(Event { token, readable: true, writable: false, hup }),
                    libc::EVFILT_WRITE => events.push(Event { token, readable: false, writable: true, hup }),
                    libc::EVFILT_USER => events.push(Event { token: WAKE_TOKEN, ..Event::default() }),
                    _ => {}
                }
            }
            Ok(())
        }
    }

    impl Waker {
        pub fn wake(&self) {
            // SAFETY: valid kqueue fd (kept alive by the Arc); one change, no event list.
            let _ = apply(
                self.kq.as_raw_fd(),
                &[kev(0, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, WAKE_TOKEN)],
            );
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub struct Poller {
        ep: OwnedFd,
        efd: Arc<OwnedFd>,
    }

    pub struct Waker {
        efd: Arc<OwnedFd>,
    }

    fn mask(i: Interest) -> u32 {
        (if i.read { libc::EPOLLIN as u32 } else { 0 }) | (if i.write { libc::EPOLLOUT as u32 } else { 0 })
    }

    fn ctl(ep: RawFd, op: libc::c_int, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        let mut ev = libc::epoll_event { events, u64: token };
        // SAFETY: `ev` is a valid epoll_event for the duration of the call.
        let r = unsafe { libc::epoll_ctl(ep, op, fd, &mut ev) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    impl Poller {
        pub fn new() -> io::Result<Poller> {
            // SAFETY: plain syscalls; results are checked before use.
            let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            if ep < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fresh descriptor, owned here.
            let ep = unsafe { OwnedFd::from_raw_fd(ep) };
            let ef = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            if ef < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fresh descriptor, owned here.
            let efd = Arc::new(unsafe { OwnedFd::from_raw_fd(ef) });
            ctl(ep.as_raw_fd(), libc::EPOLL_CTL_ADD, efd.as_raw_fd(), libc::EPOLLIN as u32, WAKE_TOKEN)?;
            Ok(Poller { ep, efd })
        }

        pub fn waker(&self) -> Waker {
            Waker { efd: self.efd.clone() }
        }

        pub fn add(&self, fd: RawFd, token: u64, i: Interest) -> io::Result<()> {
            ctl(self.ep.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, mask(i), token)
        }

        pub fn modify(&self, fd: RawFd, token: u64, _old: Interest, new: Interest) -> io::Result<()> {
            ctl(self.ep.as_raw_fd(), libc::EPOLL_CTL_MOD, fd, mask(new), token)
        }

        pub fn remove(&self, fd: RawFd, _old: Interest) {
            let _ = ctl(self.ep.as_raw_fd(), libc::EPOLL_CTL_DEL, fd, 0, 0);
        }

        pub fn wait(&self, events: &mut Vec<Event>) -> io::Result<()> {
            self.wait_inner(events, -1)
        }

        /// Like `wait`, but returns at once when nothing is ready.
        pub fn poll_now(&self, events: &mut Vec<Event>) -> io::Result<()> {
            self.wait_inner(events, 0)
        }

        fn wait_inner(&self, events: &mut Vec<Event>, timeout_ms: libc::c_int) -> io::Result<()> {
            events.clear();
            let mut raw = [libc::epoll_event { events: 0, u64: 0 }; BATCH];
            // SAFETY: `raw` has room for BATCH events; timeout -1 means block.
            let n = unsafe { libc::epoll_wait(self.ep.as_raw_fd(), raw.as_mut_ptr(), BATCH as libc::c_int, timeout_ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                return if e.kind() == io::ErrorKind::Interrupted { Ok(()) } else { Err(e) };
            }
            for k in &raw[..n as usize] {
                let (bits, token) = (k.events, k.u64);
                if token == WAKE_TOKEN {
                    let mut buf = [0u8; 8];
                    // SAFETY: reads at most 8 bytes into `buf` (resets the eventfd counter).
                    unsafe { libc::read(self.efd.as_raw_fd(), buf.as_mut_ptr().cast(), 8) };
                    events.push(Event { token, ..Event::default() });
                    continue;
                }
                let hup = bits & (libc::EPOLLHUP | libc::EPOLLERR | libc::EPOLLRDHUP) as u32 != 0;
                events.push(Event {
                    token,
                    readable: bits & libc::EPOLLIN as u32 != 0 || hup,
                    writable: bits & libc::EPOLLOUT as u32 != 0,
                    hup,
                });
            }
            Ok(())
        }
    }

    impl Waker {
        pub fn wake(&self) {
            let one: u64 = 1;
            // SAFETY: writes 8 bytes from a valid local to a valid eventfd.
            unsafe { libc::write(self.efd.as_raw_fd(), (&one as *const u64).cast(), 8) };
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("mova-nrepl supports macOS (kqueue) and Linux (epoll) only");

pub use imp::{Poller, Waker};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn read_ready_and_wake() {
        let p = Poller::new().unwrap();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        p.add(l.as_raw_fd(), 7, Interest::READ).unwrap();
        let mut ev = Vec::new();

        // wake from another thread
        let w = p.waker();
        let t = std::thread::spawn(move || w.wake());
        p.wait(&mut ev).unwrap();
        t.join().unwrap();
        assert!(ev.iter().any(|e| e.token == WAKE_TOKEN));

        // readable listener
        let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        p.wait(&mut ev).unwrap();
        assert!(ev.iter().any(|e| e.token == 7 && e.readable));
        let (s, _) = l.accept().unwrap();
        s.set_nonblocking(true).unwrap();

        // write interest on and off
        p.add(s.as_raw_fd(), 9, Interest::READ).unwrap();
        let both = Interest { read: true, write: true };
        p.modify(s.as_raw_fd(), 9, Interest::READ, both).unwrap();
        p.wait(&mut ev).unwrap();
        assert!(ev.iter().any(|e| e.token == 9 && e.writable));
        p.modify(s.as_raw_fd(), 9, both, Interest::READ).unwrap();
        c.write_all(b"x").unwrap();
        p.wait(&mut ev).unwrap();
        assert!(ev.iter().any(|e| e.token == 9 && e.readable && !e.writable));
    }
}
