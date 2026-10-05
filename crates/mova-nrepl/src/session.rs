//! Sessions and the session table (the only shared map in the server).
//!
//! * Ids are UUID v4 text (36 bytes), made without a lock.
//! * A persistent session lives in the table from `clone` until `close`.
//! * An *ephemeral* session is a message with no `session` key. It is not in
//!   the table and not listed by `ls-sessions`. The router gives it a fresh id
//!   for every reply of that message.
//!
//! P2 seam: `Session::slot` holds backend-owned state (the session thread, its
//! job FIFO, bindings) created on first use; `interrupt` is the per-session
//! flag the interpreter polls. The wire layer never looks inside either.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub const ID_LEN: usize = 36;

/// A session id: 36 ASCII bytes, `8-4-4-4-12` lowercase hex, version 4.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId([u8; ID_LEN]);

impl SessionId {
    /// A fresh random id.
    pub fn new() -> SessionId {
        let (a, b) = random_pair();
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&a.to_be_bytes());
        bytes[8..].copy_from_slice(&b.to_be_bytes());
        bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
        bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10xx
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [b'-'; ID_LEN];
        let mut o = 0;
        for (i, byte) in bytes.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                o += 1; // skip a dash
            }
            out[o] = HEX[(byte >> 4) as usize];
            out[o + 1] = HEX[(byte & 15) as usize];
            o += 2;
        }
        SessionId(out)
    }

    /// Parses exactly what `new` makes. Anything else is `None`.
    pub fn parse(b: &[u8]) -> Option<SessionId> {
        let a: [u8; ID_LEN] = b.try_into().ok()?;
        Some(SessionId(a))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        // ASCII by construction
        std::str::from_utf8(&self.0).unwrap_or("")
    }
}

impl Default for SessionId {
    fn default() -> Self {
        SessionId::new()
    }
}

impl std::fmt::Debug for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Two random u64. Seeded once from the OS; then a lock-free counter through
/// the splitmix64 finalizer (a bijection, so ids never repeat within 2^64
/// draws). Unique, not secret: nREPL has no authentication anyway.
fn random_pair() -> (u64, u64) {
    static SEED: OnceLock<u64> = OnceLock::new();
    static CTR: AtomicU64 = AtomicU64::new(0);
    let seed = *SEED.get_or_init(|| {
        use std::io::Read;
        let mut b = [0u8; 8];
        let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok();
        if ok {
            u64::from_le_bytes(b)
        } else {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1);
            t ^ ((std::process::id() as u64) << 32)
        }
    });
    fn mix(mut z: u64) -> u64 {
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    let n = CTR.fetch_add(2, Ordering::Relaxed);
    (mix(seed.wrapping_add(n)), mix(seed.wrapping_add(n + 1) ^ 0xA5A5_A5A5_A5A5_A5A5))
}

/// One session.
pub struct Session {
    pub id: SessionId,
    /// Name of the session's current namespace; what `describe` reports as
    /// `aux.current-ns`. The backend updates it after each form.
    ns: Mutex<String>,
    /// Interrupt state for the interpreter: 0 none, 1 soft, 2 hard (P3).
    pub interrupt: AtomicU8,
    closed: AtomicBool,
    /// Backend-owned state, created on first use (P2: the session thread).
    pub slot: OnceLock<Box<dyn Any + Send + Sync>>,
}

impl Session {
    fn new(id: SessionId) -> Session {
        Session {
            id,
            ns: Mutex::new("user".to_string()),
            interrupt: AtomicU8::new(0),
            closed: AtomicBool::new(false),
            slot: OnceLock::new(),
        }
    }

    /// A session that is not in any table (for ephemeral requests).
    pub fn ephemeral() -> Arc<Session> {
        Arc::new(Session::new(SessionId::new()))
    }

    pub fn ns(&self) -> String {
        self.ns.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Runs `f` on the namespace name without copying it.
    pub fn with_ns<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        match self.ns.lock() {
            Ok(g) => f(&g),
            Err(_) => f("user"),
        }
    }

    pub fn set_ns(&self, ns: &str) {
        if let Ok(mut g) = self.ns.lock() {
            g.clear();
            g.push_str(ns);
        }
    }

    /// True after `close`. A session thread should stop when it sees this.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("id", &self.id).field("ns", &self.ns()).finish()
    }
}

/// All persistent sessions.
#[derive(Default)]
pub struct SessionTable {
    map: Mutex<HashMap<SessionId, Arc<Session>>>,
}

impl SessionTable {
    pub fn new() -> SessionTable {
        SessionTable::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Arc<Session>>> {
        // A poisoned lock still holds a valid map: no operation here can leave it half-updated.
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Creates and registers a session (namespace `user`).
    pub fn create(&self) -> Arc<Session> {
        let s = Arc::new(Session::new(SessionId::new()));
        self.lock().insert(s.id, s.clone());
        s
    }

    /// Looks up by the id bytes from the wire. No allocation.
    pub fn get(&self, id: &[u8]) -> Option<Arc<Session>> {
        let id = SessionId::parse(id)?;
        self.lock().get(&id).cloned()
    }

    /// Removes and marks closed. `None` if there was no such session.
    pub fn remove(&self, id: &[u8]) -> Option<Arc<Session>> {
        let id = SessionId::parse(id)?;
        let s = self.lock().remove(&id)?;
        s.closed.store(true, Ordering::Release);
        Some(s)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Ids of all sessions, in no particular order.
    pub fn ids(&self) -> Vec<SessionId> {
        self.lock().keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn id_is_uuid_v4() {
        for _ in 0..1000 {
            let id = SessionId::new();
            let s = id.as_str();
            assert_eq!(s.len(), 36);
            let b = s.as_bytes();
            assert!(b[8] == b'-' && b[13] == b'-' && b[18] == b'-' && b[23] == b'-', "{s}");
            assert_eq!(b[14], b'4', "{s}");
            assert!(matches!(b[19], b'8' | b'9' | b'a' | b'b'), "{s}");
            assert!(s.bytes().all(|c| c == b'-' || c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
        }
    }

    #[test]
    fn ids_are_unique() {
        let set: HashSet<SessionId> = (0..50_000).map(|_| SessionId::new()).collect();
        assert_eq!(set.len(), 50_000);
    }

    #[test]
    fn ids_are_unique_across_threads() {
        let hs: Vec<_> = (0..4).map(|_| std::thread::spawn(|| (0..10_000).map(|_| SessionId::new()).collect::<Vec<_>>())).collect();
        let mut all = HashSet::new();
        for h in hs {
            for id in h.join().unwrap() {
                assert!(all.insert(id));
            }
        }
    }

    #[test]
    fn create_get_remove() {
        let t = SessionTable::new();
        assert!(t.is_empty());
        let a = t.create();
        let b = t.create();
        assert_eq!(t.len(), 2);
        assert!(Arc::ptr_eq(&t.get(a.id.as_bytes()).unwrap(), &a));
        assert!(t.get(b"not-a-uuid").is_none());
        assert!(t.get(b"00000000-0000-0000-0000-000000000000").is_none());
        assert!(!a.is_closed());
        let gone = t.remove(a.id.as_bytes()).unwrap();
        assert!(gone.is_closed());
        assert!(t.get(a.id.as_bytes()).is_none());
        assert!(t.remove(a.id.as_bytes()).is_none());
        assert_eq!(t.ids(), vec![b.id]);
    }

    #[test]
    fn ephemeral_is_not_listed() {
        let t = SessionTable::new();
        let e = Session::ephemeral();
        assert!(t.get(e.id.as_bytes()).is_none());
        assert!(t.ids().is_empty());
    }

    #[test]
    fn ns_defaults_to_user_and_can_change() {
        let t = SessionTable::new();
        let s = t.create();
        assert_eq!(s.ns(), "user");
        s.set_ns("foo.bar");
        assert_eq!(s.with_ns(|n| n.len()), 7);
        // a clone starts in `user`
        assert_eq!(t.create().ns(), "user");
    }

    #[test]
    fn slot_is_set_once() {
        let s = Session::ephemeral();
        assert!(s.slot.get().is_none());
        assert!(s.slot.set(Box::new(5u32)).is_ok());
        assert!(s.slot.set(Box::new(6u32)).is_err());
        assert_eq!(s.slot.get().unwrap().downcast_ref::<u32>(), Some(&5));
    }

    #[test]
    fn concurrent_create_and_remove() {
        let t = Arc::new(SessionTable::new());
        let hs: Vec<_> = (0..4)
            .map(|_| {
                let t = t.clone();
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        let s = t.create();
                        assert!(t.get(s.id.as_bytes()).is_some());
                        assert!(t.remove(s.id.as_bytes()).is_some());
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert!(t.is_empty());
    }
}
