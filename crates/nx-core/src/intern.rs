//! Global append-only string interner. `resolve` is lock-free; `intern` takes a shard read lock
//! (write lock only on first insert of a string). Strings live forever (`&'static str`).

use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::RwLock;

/// Interned string id. `SymId::NONE` means "absent" (e.g. no namespace part).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct SymId(pub u32);

impl SymId {
    pub const NONE: SymId = SymId(u32::MAX);
    #[inline]
    pub fn is_none(self) -> bool {
        self.0 == u32::MAX
    }
    #[inline]
    pub fn as_str(self) -> &'static str {
        resolve(self)
    }
}

const SHARD_BITS: u32 = 6;
const SHARDS: usize = 1 << SHARD_BITS;
const CHUNK_BITS: u32 = 14;
const CHUNK: usize = 1 << CHUNK_BITS;
const MAX_CHUNKS: usize = 1 << 16;
const ARENA_BLOCK: usize = 16 * 1024; // one macOS page: a shard only touches what it stores

#[derive(Clone, Copy)]
struct Slot {
    ptr: *const u8,
    len: u32,
}

static CHUNKS: [AtomicPtr<Slot>; MAX_CHUNKS] = [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_CHUNKS];
static NEXT: AtomicU32 = AtomicU32::new(0);

struct Shard {
    table: Vec<u64>, // (tag32 << 32) | (id + 1); 0 = empty
    count: usize,
    arena: &'static mut [u8],
}

static TABLES: [RwLock<Shard>; SHARDS] = [const {
    RwLock::new(Shard {
        table: Vec::new(),
        count: 0,
        arena: &mut [],
    })
}; SHARDS];

const K: u64 = 0x9E37_79B9_7F4A_7C15;

#[inline]
fn hash(b: &[u8]) -> u64 {
    let mut h: u64 = b.len() as u64;
    let mut c = b.chunks_exact(8);
    for w in &mut c {
        h = (h.rotate_left(5) ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(K);
    }
    let r = c.remainder();
    if !r.is_empty() {
        let mut t = [0u8; 8];
        t[..r.len()].copy_from_slice(r);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(t)).wrapping_mul(K);
    }
    h ^= h >> 32;
    h = h.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    h ^ (h >> 29)
}

#[inline]
fn slot_of(id: u32) -> Option<Slot> {
    let c = CHUNKS.get((id >> CHUNK_BITS) as usize)?.load(Ordering::Acquire);
    if c.is_null() {
        return None;
    }
    // SAFETY: chunk has CHUNK slots; slot was written before `id` was published.
    Some(unsafe { *c.add(id as usize & (CHUNK - 1)) })
}

/// Resolve an id to its string. Invalid ids (incl. `NONE`) give "".
#[inline]
pub fn resolve(id: SymId) -> &'static str {
    match slot_of(id.0) {
        // SAFETY: slot points at leaked valid UTF-8 bytes.
        Some(s) if !s.ptr.is_null() => unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(s.ptr, s.len as usize)) },
        _ => "",
    }
}

#[inline]
fn probe(table: &[u64], h: u64, s: &str) -> Result<SymId, usize> {
    let mask = table.len() - 1;
    let tag = (h as u32) as u64;
    let mut i = (h >> 20) as usize & mask;
    loop {
        let e = table[i];
        if e == 0 {
            return Err(i);
        }
        if e >> 32 == tag {
            let id = (e as u32) - 1;
            if resolve(SymId(id)) == s {
                return Ok(SymId(id));
            }
        }
        i = (i + 1) & mask;
    }
}

fn store(sh: &mut Shard, s: &str) -> u32 {
    let n = s.len();
    let ptr: *const u8 = if n == 0 {
        std::ptr::NonNull::<u8>::dangling().as_ptr()
    } else {
        if sh.arena.len() < n {
            let blk = n.max(ARENA_BLOCK);
            sh.arena = Vec::leak(vec![0u8; blk]);
        }
        let a = std::mem::take(&mut sh.arena);
        let (head, tail) = a.split_at_mut(n);
        head.copy_from_slice(s.as_bytes());
        sh.arena = tail;
        head.as_ptr()
    };
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let ci = (id >> CHUNK_BITS) as usize;
    assert!(ci < MAX_CHUNKS, "interner full");
    let mut c = CHUNKS[ci].load(Ordering::Acquire);
    if c.is_null() {
        let fresh: *mut Slot = Vec::leak(vec![Slot { ptr: std::ptr::null(), len: 0 }; CHUNK]).as_mut_ptr();
        match CHUNKS[ci].compare_exchange(std::ptr::null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => c = fresh,
            Err(cur) => {
                // SAFETY: fresh was leaked from a Vec of CHUNK slots and never shared.
                drop(unsafe { Vec::from_raw_parts(fresh, CHUNK, CHUNK) });
                c = cur;
            }
        }
    }
    // SAFETY: slot index < CHUNK; only the creator writes this slot, before the id is shared.
    unsafe { *c.add(id as usize & (CHUNK - 1)) = Slot { ptr, len: n as u32 } };
    id
}

/// Intern a string; equal strings always give equal ids. Thread-safe.
pub fn intern(s: &str) -> SymId {
    let h = hash(s.as_bytes());
    let shard = &TABLES[(h >> (64 - SHARD_BITS)) as usize];
    {
        let g = shard.read().unwrap_or_else(|e| e.into_inner());
        if !g.table.is_empty() {
            if let Ok(id) = probe(&g.table, h, s) {
                return id;
            }
        }
    }
    let mut g = shard.write().unwrap_or_else(|e| e.into_inner());
    if g.table.is_empty() {
        g.table = vec![0u64; 256];
    }
    let pos = match probe(&g.table, h, s) {
        Ok(id) => return id,
        Err(p) => p,
    };
    let id = store(&mut g, s);
    g.table[pos] = ((h as u32 as u64) << 32) | (id as u64 + 1);
    g.count += 1;
    if g.count * 2 > g.table.len() {
        grow(&mut g);
    }
    SymId(id)
}

fn grow(g: &mut Shard) {
    let old = std::mem::take(&mut g.table);
    let mut t = vec![0u64; old.len() * 2];
    let mask = t.len() - 1;
    for e in old.into_iter().filter(|&e| e != 0) {
        // re-hash from the string (tag only has 32 bits of the probe hash)
        let h = hash(resolve(SymId((e as u32) - 1)).as_bytes());
        let mut i = (h >> 20) as usize & mask;
        while t[i] != 0 {
            i = (i + 1) & mask;
        }
        t[i] = e;
    }
    g.table = t;
}

/// Number of interned strings so far.
pub fn len() -> usize {
    NEXT.load(Ordering::Relaxed) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_dedupe() {
        let a = intern("defn");
        assert_eq!(a, intern("defn"));
        assert_ne!(a, intern("defn-"));
        assert_eq!(resolve(a), "defn");
        assert_eq!(resolve(intern("")), "");
        assert_eq!(resolve(intern("ሴ𝄞")), "ሴ𝄞");
        assert_eq!(resolve(SymId::NONE), "");
        assert_eq!(resolve(SymId(123_456_789)), "");
    }

    #[test]
    fn many_and_growth() {
        let ids: Vec<_> = (0..50_000).map(|i| intern(&format!("grow-{i}"))).collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(resolve(*id), format!("grow-{i}"));
            assert_eq!(intern(&format!("grow-{i}")), *id);
        }
    }

    #[test]
    fn long_string() {
        let s = "x".repeat(200_000);
        assert_eq!(resolve(intern(&s)).len(), 200_000);
    }

    #[test]
    fn threads() {
        let hs: Vec<_> = (0..8)
            .map(|t| {
                std::thread::spawn(move || {
                    let mut v = Vec::new();
                    for i in 0..20_000 {
                        let s = format!("thr-{}", (i * 7 + t) % 5000);
                        let id = intern(&s);
                        assert_eq!(resolve(id), s);
                        v.push((s, id));
                    }
                    v
                })
            })
            .collect();
        let mut all = std::collections::HashMap::new();
        for h in hs {
            for (s, id) in h.join().unwrap() {
                assert_eq!(*all.entry(s).or_insert(id), id);
            }
        }
    }
}
