//! Append-only per-jar blob container, mmapped on read.
//!
//! Layout: header (16 B) = `b"NXC1"`, u32 format version, u32 user schema, u32 0.
//! Then records, each 8-byte aligned: u32 klen, u32 vlen, u64 checksum(key,val), key, val, pad.
//! Reading scans records and stops at the first bad/truncated one (valid prefix is kept;
//! a bad header makes the file empty). Last record for a key wins.
use memmap2::Mmap;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"NXC1";
const FORMAT: u32 = 1;
const HDR: usize = 16;

/// Serialise a value into a blob (the analyzer's FileAnalysis encoding plugs in here).
pub trait Encode {
    fn encode(&self, out: &mut Vec<u8>);
}
/// Parse a blob; `None` = corrupt/unknown (caller recomputes).
pub trait Decode: Sized {
    fn decode(b: &[u8]) -> Option<Self>;
}

impl Encode for Vec<u8> {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self)
    }
}
impl Decode for Vec<u8> {
    fn decode(b: &[u8]) -> Option<Self> {
        Some(b.to_vec())
    }
}
impl Encode for String {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.as_bytes())
    }
}
impl Decode for String {
    fn decode(b: &[u8]) -> Option<Self> {
        String::from_utf8(b.to_vec()).ok()
    }
}
impl Encode for u64 {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes())
    }
}
impl Decode for u64 {
    fn decode(b: &[u8]) -> Option<Self> {
        Some(u64::from_le_bytes(b.try_into().ok()?))
    }
}

fn checksum(k: &[u8], v: &[u8]) -> u64 {
    // 64-bit multiply-fold over 8-byte words; detects truncation and bit rot, not adversaries.
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15 ^ (k.len() as u64) << 32 ^ v.len() as u64;
    for s in [k, v] {
        let mut ch = s.chunks_exact(8);
        for w in &mut ch {
            h = (h ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(0x100_0000_01B3_u64 | 1 << 40).rotate_left(29);
        }
        let mut t = [0u8; 8];
        t[..ch.remainder().len()].copy_from_slice(ch.remainder());
        h = (h ^ u64::from_le_bytes(t)).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(31);
    }
    h ^ (h >> 32)
}

fn pad(n: usize) -> usize {
    (8 - n % 8) % 8
}

pub struct Cache {
    path: PathBuf,
    schema: u32,
    mmap: Option<Mmap>,
    /// key -> (value range in mmap)
    index: HashMap<Vec<u8>, Range<usize>>,
    /// Puts made through this handle since open (visible to `get` without remap).
    pending: HashMap<Vec<u8>, Vec<u8>>,
    /// Valid length of the file (bytes); appends start here.
    valid_len: usize,
    file: Option<File>,
    /// True if open found a corrupt/truncated tail or bad header.
    pub recovered: bool,
}

impl Cache {
    /// Open (or lazily create on first `put`). Never fails on corrupt content.
    pub fn open(path: impl AsRef<Path>, schema: u32) -> Cache {
        let path = path.as_ref().to_path_buf();
        let mut c = Cache { path, schema, mmap: None, index: HashMap::new(), pending: HashMap::new(), valid_len: 0, file: None, recovered: false };
        c.load();
        c
    }

    fn load(&mut self) {
        self.mmap = None;
        self.index.clear();
        self.valid_len = 0;
        let Ok(f) = File::open(&self.path) else { return };
        let Ok(m) = (unsafe { Mmap::map(&f) }) else { return };
        let b = &m[..];
        let hdr_ok = b.len() >= HDR
            && &b[..4] == MAGIC
            && u32::from_le_bytes(b[4..8].try_into().unwrap()) == FORMAT
            && u32::from_le_bytes(b[8..12].try_into().unwrap()) == self.schema;
        if !hdr_ok {
            self.recovered = !b.is_empty();
            return;
        }
        let mut off = HDR;
        while off + 16 <= b.len() {
            let kl = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
            let vl = u32::from_le_bytes(b[off + 4..off + 8].try_into().unwrap()) as usize;
            let sum = u64::from_le_bytes(b[off + 8..off + 16].try_into().unwrap());
            let ks = off + 16;
            let Some(ve) = ks.checked_add(kl).and_then(|x| x.checked_add(vl)) else { break };
            let next = ve + pad(ve);
            if next > b.len() {
                break;
            }
            if checksum(&b[ks..ks + kl], &b[ks + kl..ve]) != sum {
                break;
            }
            self.index.insert(b[ks..ks + kl].to_vec(), ks + kl..ve);
            off = next;
        }
        self.recovered = off != b.len();
        self.valid_len = off;
        self.mmap = Some(m);
    }

    pub fn len(&self) -> usize {
        self.index.len() + self.pending.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Borrowed blob; `None` if absent.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        if let Some(v) = self.pending.get(key) {
            return Some(v);
        }
        let r = self.index.get(key)?;
        self.mmap.as_ref().map(|m| &m[r.clone()])
    }

    pub fn get_value<T: Decode>(&self, key: &[u8]) -> Option<T> {
        T::decode(self.get(key)?)
    }

    pub fn put_value<T: Encode>(&mut self, key: &[u8], v: &T) -> std::io::Result<()> {
        let mut b = Vec::new();
        v.encode(&mut b);
        self.put(key, &b)
    }

    /// Append a record (one `write` call). Truncates a corrupt tail first.
    pub fn put(&mut self, key: &[u8], val: &[u8]) -> std::io::Result<()> {
        if self.file.is_none() {
            if let Some(d) = self.path.parent() {
                std::fs::create_dir_all(d)?;
            }
            let mut f = OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&self.path)?;
            if self.valid_len == 0 {
                f.set_len(0)?;
                let mut h = Vec::with_capacity(HDR);
                h.extend_from_slice(MAGIC);
                h.extend_from_slice(&FORMAT.to_le_bytes());
                h.extend_from_slice(&self.schema.to_le_bytes());
                h.extend_from_slice(&0u32.to_le_bytes());
                f.write_all(&h)?;
                self.valid_len = HDR;
            } else {
                f.set_len(self.valid_len as u64)?;
            }
            // reopen for append semantics
            drop(f);
            self.file = Some(OpenOptions::new().append(true).open(&self.path)?);
        }
        let mut rec = Vec::with_capacity(16 + key.len() + val.len() + 8);
        rec.extend_from_slice(&(key.len() as u32).to_le_bytes());
        rec.extend_from_slice(&(val.len() as u32).to_le_bytes());
        rec.extend_from_slice(&checksum(key, val).to_le_bytes());
        rec.extend_from_slice(key);
        rec.extend_from_slice(val);
        rec.resize(rec.len() + pad(rec.len()), 0);
        self.file.as_mut().unwrap().write_all(&rec)?;
        self.valid_len += rec.len();
        self.index.remove(key);
        self.pending.insert(key.to_vec(), val.to_vec());
        Ok(())
    }

    /// Re-map the file so pending puts become zero-copy mmap reads.
    pub fn remap(&mut self) {
        self.pending.clear();
        self.file = None;
        self.load();
    }
}
