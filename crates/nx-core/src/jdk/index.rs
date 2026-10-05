//! Flat, mmap-able JDK index: class table (sorted by name), per-file member runs, string blob.
//! Layout (little endian): header, files[u32 str], classes[4 x u32: name file lo hi], by_simple[u32 class], members[10 x u32], strings.
use super::parse::{self, FileParse, CAT_SHIFT, F_FIELD, F_FINAL, F_METHOD};
use memmap2::Mmap;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;

const MAGIC: &[u8; 8] = b"NXJD0002";
/// Bump when the parser output changes.
pub const PARSE_VERSION: u64 = 13;
const NONE: u32 = u32::MAX;
const HDR: usize = 8 + 3 * 8 + 9 * 4;
const M_WORDS: usize = 10;

struct Interner {
    blob: Vec<u8>,
    map: HashMap<String, u32>,
}

impl Interner {
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(o) = self.map.get(s) {
            return *o;
        }
        let mut s2 = s;
        if s2.len() > 60000 {
            let mut e = 60000;
            while !s2.is_char_boundary(e) {
                e -= 1;
            }
            s2 = &s2[..e];
        }
        let off = self.blob.len() as u32;
        self.blob.extend_from_slice(&(s2.len() as u16).to_le_bytes());
        self.blob.extend_from_slice(s2.as_bytes());
        self.map.insert(s.to_string(), off);
        off
    }
}

fn w32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// Is this entry a JDK source clojure-lsp analyzes: `java*/**.java` (top dir starts with "java").
fn wanted(name: &str) -> bool {
    name.ends_with(".java") && name.starts_with("java") && !name.ends_with("module-info.java") && name.contains('/')
}

/// Parse `src.zip` with `threads` workers and serialize the index.
pub fn build(zip: &Path, threads: usize) -> std::io::Result<Vec<u8>> {
    let f = std::fs::File::open(zip)?;
    let map = unsafe { Mmap::map(&f)? };
    let bytes: &[u8] = &map[..];
    let mut ar0 = zip_open(bytes)?;
    let mut names: Vec<(usize, String, [u32; 3])> = Vec::new();
    for i in 0..ar0.len() {
        if let Ok(e) = ar0.by_index_raw(i) {
            if wanted(e.name()) {
                names.push((i, e.name().to_string(), [e.data_start() as u32, e.compressed_size() as u32, matches!(e.compression(), zip::CompressionMethod::Deflated) as u32]));
            }
        }
    }
    drop(ar0);
    let threads = threads.max(1).min(names.len().max(1));
    let names_ref = &names;
    // workers parse and send; the collector (this thread) interns and appends, so a parse result lives briefly
    let mut st = Interner { blob: Vec::new(), map: HashMap::new() };
    let mut files: Vec<(u32, [u32; 3])> = Vec::with_capacity(names.len());
    let mut classes: Vec<(String, u32, u32, u32)> = Vec::new();
    let mut members: Vec<u8> = Vec::new();
    let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, FileParse)>(threads * 2);
    std::thread::scope(|s| {
        for t in 0..threads {
            let tx = tx.clone();
            s.spawn(move || {
                let mut ar = zip_open(bytes).expect("zip");
                let mut buf = Vec::new();
                let mut k = t;
                while k < names_ref.len() {
                    let idx = names_ref[k].0;
                    if let Ok(mut e) = ar.by_index(idx) {
                        buf.clear();
                        if e.read_to_end(&mut buf).is_ok() {
                            let text = String::from_utf8_lossy(&buf);
                            if tx.send((k, parse::parse(&text))).is_err() {
                                return;
                            }
                        }
                    }
                    k += threads;
                }
            });
        }
        drop(tx);
        for (k, mut fp) in rx {
            if fp.has_enum {
                fp.classes.clear();
                fp.members.clear();
            }
            let file = files.len() as u32;
            files.push((st.intern(&names[k].1), names[k].2));
            let base = (members.len() / (4 * M_WORDS)) as u32;
            for m in &fp.members {
                w32(&mut members, st.intern(&m.name));
                w32(&mut members, m.ty.as_deref().map_or(NONE, |t| st.intern(t)));
                w32(&mut members, m.params.as_deref().map_or(NONE, |t| st.intern(t)));
                w32(&mut members, m.flags as u32);
                for x in [m.row, m.col, m.end_row, m.end_col, m.doc.0, m.doc.1] {
                    w32(&mut members, x);
                }
            }
            for (name, lo, hi) in fp.classes {
                classes.push((name, file, base + lo, base + hi));
            }
        }
    });
    classes.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let cls_names: Vec<u32> = classes.iter().map(|c| st.intern(&c.0)).collect();
    let simple = |n: &str| -> usize { n.rfind('.').map_or(0, |i| i + 1) };
    let mut by_simple: Vec<u32> = (0..classes.len() as u32).collect();
    by_simple.sort_by(|a, b| {
        let (x, y) = (&classes[*a as usize].0, &classes[*b as usize].0);
        x[simple(x)..].as_bytes().cmp(y[simple(y)..].as_bytes()).then(x.as_bytes().cmp(y.as_bytes()))
    });
    // serialize
    let (n_files, n_classes, n_members) = (files.len() as u32, classes.len() as u32, (members.len() / (4 * M_WORDS)) as u32);
    let off_files = HDR as u32;
    let off_classes = off_files + 16 * n_files;
    let off_simple = off_classes + 16 * n_classes;
    let off_members = off_simple + 4 * n_classes;
    let off_strs = off_members + 4 * M_WORDS as u32 * n_members;
    let (size, mtime) = zip_stat(zip);
    let mut out: Vec<u8> = Vec::with_capacity(off_strs as usize + st.blob.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&mtime.to_le_bytes());
    out.extend_from_slice(&PARSE_VERSION.to_le_bytes());
    for x in [n_files, n_classes, n_members, st.blob.len() as u32, off_files, off_classes, off_simple, off_members, off_strs] {
        w32(&mut out, x);
    }
    for f in &files {
        w32(&mut out, f.0);
        for x in f.1 {
            w32(&mut out, x);
        }
    }
    for (i, c) in classes.iter().enumerate() {
        w32(&mut out, cls_names[i]);
        w32(&mut out, c.1);
        w32(&mut out, c.2);
        w32(&mut out, c.3);
    }
    for x in &by_simple {
        w32(&mut out, *x);
    }
    out.extend_from_slice(&members);
    out.extend_from_slice(&st.blob);
    Ok(out)
}

fn zip_open(bytes: &[u8]) -> std::io::Result<zip::ZipArchive<Cursor<&[u8]>>> {
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// (size, mtime ns) of the zip.
pub fn zip_stat(p: &Path) -> (u64, u64) {
    match std::fs::metadata(p) {
        Ok(m) => (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64)),
        Err(_) => (0, 0),
    }
}

/// A member of a JDK class.
#[derive(Clone, Copy)]
pub struct MemberRef(u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClassRef(pub u32);

/// The loaded (mmapped) index.
pub struct Jdk {
    map: Mmap,
    pub zip: String,
    n_files: u32,
    n_classes: u32,
    n_members: u32,
    off_files: usize,
    off_classes: usize,
    off_simple: usize,
    off_members: usize,
    off_strs: usize,
}

impl Jdk {
    /// Map `idx` if it matches `zip` (size, mtime, parse version).
    pub fn open(idx: &Path, zip: &Path) -> Option<Jdk> {
        let f = std::fs::File::open(idx).ok()?;
        let map = unsafe { Mmap::map(&f).ok()? };
        if map.len() < HDR || &map[..8] != MAGIC {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(map[o..o + 8].try_into().unwrap());
        let (size, mtime) = zip_stat(zip);
        if u64_at(8) != size || u64_at(16) != mtime || u64_at(24) != PARSE_VERSION {
            return None;
        }
        let h: Vec<u32> = (0..9).map(|i| u32::from_le_bytes(map[32 + 4 * i..36 + 4 * i].try_into().unwrap())).collect();
        let u = |i: usize| h[i];
        let j = Jdk {
            n_files: u(0),
            n_classes: u(1),
            n_members: u(2),
            off_files: u(4) as usize,
            off_classes: u(5) as usize,
            off_simple: u(6) as usize,
            off_members: u(7) as usize,
            off_strs: u(8) as usize,
            zip: zip.to_string_lossy().into_owned(),
            map,
        };
        let end = j.off_strs + u(3) as usize;
        (j.map.len() >= end && j.off_strs >= j.off_members).then_some(j)
    }

    fn u32_at(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.map[o..o + 4].try_into().unwrap())
    }
    fn s(&self, off: u32) -> &str {
        let o = self.off_strs + off as usize;
        let len = u16::from_le_bytes([self.map[o], self.map[o + 1]]) as usize;
        // SAFETY: strings were written from `&str` (truncated on a char boundary).
        unsafe { std::str::from_utf8_unchecked(&self.map[o + 2..o + 2 + len]) }
    }

    pub fn class_count(&self) -> usize {
        self.n_classes as usize
    }
    pub fn member_count(&self) -> usize {
        self.n_members as usize
    }
    pub fn file_count(&self) -> usize {
        self.n_files as usize
    }
    pub fn size_bytes(&self) -> usize {
        self.map.len()
    }

    fn cw(&self, c: ClassRef, w: usize) -> u32 {
        self.u32_at(self.off_classes + 16 * c.0 as usize + 4 * w)
    }
    pub fn class_name(&self, c: ClassRef) -> &str {
        self.s(self.cw(c, 0))
    }
    /// Zip entry of the source file, e.g. `java.base/java/util/Date.java`.
    pub fn class_entry(&self, c: ClassRef) -> &str {
        self.s(self.u32_at(self.off_files + 16 * self.cw(c, 1) as usize))
    }

    /// Exact class lookup (binary name, dots + `$`).
    pub fn class(&self, name: &str) -> Option<ClassRef> {
        let i = self.lower_bound(name);
        (i < self.class_count() && self.class_name(ClassRef(i as u32)) == name).then_some(ClassRef(i as u32))
    }

    fn lower_bound(&self, prefix: &str) -> usize {
        let (mut lo, mut hi) = (0usize, self.class_count());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.class_name(ClassRef(mid as u32)).as_bytes() < prefix.as_bytes() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Classes whose full name starts with `prefix`, in name order.
    pub fn classes_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = ClassRef> + 'a {
        let start = self.lower_bound(prefix);
        (start..self.class_count()).map(|i| ClassRef(i as u32)).take_while(move |c| self.class_name(*c).starts_with(prefix))
    }

    /// Classes whose simple name (after the last `.`) equals `simple`.
    pub fn classes_simple(&self, simple: &str) -> Vec<ClassRef> {
        let key = |i: usize| -> &str {
            let c = ClassRef(self.u32_at(self.off_simple + 4 * i));
            let n = self.class_name(c);
            &n[n.rfind('.').map_or(0, |d| d + 1)..]
        };
        let (mut lo, mut hi) = (0usize, self.class_count());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if key(mid).as_bytes() < simple.as_bytes() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut out = Vec::new();
        while lo < self.class_count() && key(lo) == simple {
            out.push(ClassRef(self.u32_at(self.off_simple + 4 * lo)));
            lo += 1;
        }
        out
    }

    /// Members in kondo order: fields, constructors, methods, enum constants (source order inside each).
    pub fn members(&self, c: ClassRef) -> Vec<MemberRef> {
        let (lo, hi) = (self.cw(c, 2), self.cw(c, 3));
        let mut v: Vec<MemberRef> = (lo..hi).map(MemberRef).collect();
        v.sort_by_key(|m| self.member_flags(*m) >> CAT_SHIFT);
        v
    }

    pub fn find_member(&self, c: ClassRef, name: &str) -> Option<MemberRef> {
        self.members(c).into_iter().find(|m| self.member_name(*m) == name)
    }

    fn mw(&self, m: MemberRef, w: usize) -> u32 {
        self.u32_at(self.off_members + 4 * M_WORDS * m.0 as usize + 4 * w)
    }
    pub fn member_name(&self, m: MemberRef) -> &str {
        self.s(self.mw(m, 0))
    }
    /// Field type / method return type.
    pub fn member_type(&self, m: MemberRef) -> Option<&str> {
        let o = self.mw(m, 1);
        (o != NONE).then(|| self.s(o))
    }
    /// `Parameter.toString` of each parameter (methods and constructors only).
    pub fn member_params(&self, m: MemberRef) -> Option<Vec<&str>> {
        let o = self.mw(m, 2);
        (o != NONE).then(|| {
            let s = self.s(o);
            if s.is_empty() { Vec::new() } else { s.split('\u{1f}').collect() }
        })
    }
    pub fn member_flags(&self, m: MemberRef) -> u16 {
        self.mw(m, 3) as u16
    }
    pub fn is_field(&self, m: MemberRef) -> bool {
        self.member_flags(m) & F_FIELD != 0
    }
    pub fn is_method(&self, m: MemberRef) -> bool {
        self.member_flags(m) & F_METHOD != 0
    }
    pub fn is_final(&self, m: MemberRef) -> bool {
        self.member_flags(m) & F_FINAL != 0
    }
    /// (row, col, end-row, end-col), 1-based as kondo reports.
    pub fn member_pos(&self, m: MemberRef) -> (u32, u32, u32, u32) {
        (self.mw(m, 4), self.mw(m, 5), self.mw(m, 6), self.mw(m, 7))
    }

    /// Byte span of the attached comment in the source (empty when none).
    pub fn member_doc_span(&self, m: MemberRef) -> (usize, usize) {
        (self.mw(m, 8) as usize, self.mw(m, 9) as usize)
    }

    /// Raw attached comment (`/** ... */`) of the member, read from the zip on demand.
    pub fn member_doc(&self, c: ClassRef, m: MemberRef) -> Option<String> {
        let (lo, hi) = self.member_doc_span(m);
        if hi <= lo {
            return None;
        }
        let text = self.class_source(c)?;
        // kondo's JavaParser attaches no comments when the file has a parse problem (Math, Long: switch expressions)
        if super::jp::fails(&text) {
            return None;
        }
        text.get(lo..hi).map(|s| s.to_string())
    }

    /// Source text of the class's file: one read at the stored data offset + inflate (no central directory parse).
    pub fn class_source(&self, c: ClassRef) -> Option<String> {
        self.class_bytes(c).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Raw bytes of the class's file.
    pub fn class_bytes(&self, c: ClassRef) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let base = self.off_files + 16 * self.cw(c, 1) as usize;
        let (off, csize, deflated) = (self.u32_at(base + 4) as u64, self.u32_at(base + 8) as usize, self.u32_at(base + 12) != 0);
        let mut f = std::fs::File::open(&self.zip).ok()?;
        f.seek(SeekFrom::Start(off)).ok()?;
        let mut raw = vec![0u8; csize];
        f.read_exact(&mut raw).ok()?;
        let mut buf = Vec::new();
        if deflated {
            flate2::read::DeflateDecoder::new(&raw[..]).read_to_end(&mut buf).ok()?;
        } else {
            buf = raw;
        }
        Some(buf)
    }
}
