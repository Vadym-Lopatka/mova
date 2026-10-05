//! Jar (zip) listing and reading over one mmap + one central-directory parse per open.
use memmap2::Mmap;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;
use zip::ZipArchive;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    /// .clj .cljc .cljs .edn
    Source,
    /// anything under `clj-kondo.exports/`
    KondoConfig,
    Class,
    /// .java sources (class definitions only)
    Java,
}

pub fn classify(name: &str) -> Option<EntryKind> {
    if name.ends_with('/') {
        return None;
    }
    if name.starts_with("clj-kondo.exports/") {
        return Some(EntryKind::KondoConfig);
    }
    if name.ends_with(".class") {
        return Some(EntryKind::Class);
    }
    let ext = name.rsplit_once('.')?.1;
    if ext == "java" {
        return Some(EntryKind::Java);
    }
    matches!(ext, "clj" | "cljc" | "cljs" | "edn").then_some(EntryKind::Source)
}

pub struct Jar {
    // SAFETY: `ar` borrows the mmap's bytes; the mmap is heap-stable and outlives `ar` (field order).
    ar: ZipArchive<Cursor<&'static [u8]>>,
    _map: Mmap,
}

impl Jar {
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Jar> {
        let f = File::open(path)?;
        let map = unsafe { Mmap::map(&f)? };
        let bytes: &'static [u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(&map[..]) };
        let ar = ZipArchive::new(Cursor::new(bytes)).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Jar { ar, _map: map })
    }

    /// Entry names of the given kinds, in central-directory order. No decompression.
    pub fn names(&mut self, kinds: &[EntryKind]) -> Vec<(String, EntryKind)> {
        let mut v = Vec::new();
        for i in 0..self.ar.len() {
            if let Ok(e) = self.ar.by_index_raw(i) {
                if let Some(k) = classify(e.name()).filter(|k| kinds.contains(k)) {
                    v.push((e.name().to_string(), k));
                }
            }
        }
        v
    }

    /// First entry (central-directory order) whose name satisfies `f`.
    pub fn find_entry(&mut self, f: impl Fn(&str) -> bool) -> Option<String> {
        (0..self.ar.len()).find_map(|i| self.ar.by_index_raw(i).ok().and_then(|e| f(e.name()).then(|| e.name().to_string())))
    }

    pub fn source_names(&mut self) -> Vec<String> {
        self.names(&[EntryKind::Source]).into_iter().map(|x| x.0).collect()
    }
    pub fn class_names(&mut self) -> Vec<String> {
        self.names(&[EntryKind::Class]).into_iter().map(|x| x.0).collect()
    }

    /// Read every Source + KondoConfig entry with one shared buffer; `f(name, kind, bytes)`.
    pub fn for_each_source(&mut self, buf: &mut Vec<u8>, mut f: impl FnMut(&str, EntryKind, &[u8])) -> std::io::Result<()> {
        for i in 0..self.ar.len() {
            let mut e = self.ar.by_index(i).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let Some(k) = classify(e.name()).filter(|k| matches!(k, EntryKind::Source | EntryKind::KondoConfig)) else { continue };
            buf.clear();
            buf.reserve(e.size() as usize);
            e.read_to_end(buf)?;
            f(e.name(), k, buf);
        }
        Ok(())
    }

    /// Read every entry of the given kinds (central-directory order) with one shared buffer; `f(name, kind, bytes)`.
    pub fn for_each(&mut self, kinds: &[EntryKind], buf: &mut Vec<u8>, mut f: impl FnMut(&str, EntryKind, &[u8])) -> std::io::Result<()> {
        for i in 0..self.ar.len() {
            let mut e = self.ar.by_index(i).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let Some(k) = classify(e.name()).filter(|k| kinds.contains(k)) else { continue };
            buf.clear();
            buf.reserve(e.size() as usize);
            e.read_to_end(buf)?;
            f(e.name(), k, buf);
        }
        Ok(())
    }

    /// Like `for_each`, but first hands `f` only a prefix of each entry (`first` bytes); while `f` returns false and the
    /// entry has more bytes, the prefix doubles (up to the whole entry). Cheap header reads of `.class` files.
    pub fn for_each_head(&mut self, kinds: &[EntryKind], first: usize, buf: &mut Vec<u8>, mut f: impl FnMut(&str, EntryKind, &[u8]) -> bool) -> std::io::Result<()> {
        for i in 0..self.ar.len() {
            let mut e = self.ar.by_index(i).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let Some(k) = classify(e.name()).filter(|k| kinds.contains(k)) else { continue };
            let size = e.size() as usize;
            let mut want = first.min(size);
            buf.clear();
            loop {
                let have = buf.len();
                buf.resize(want, 0);
                e.read_exact(&mut buf[have..])?;
                if f(e.name(), k, buf) || want >= size {
                    break;
                }
                want = (want * 2).min(size);
            }
        }
        Ok(())
    }

    /// Read one entry by name.
    pub fn read(&mut self, name: &str, buf: &mut Vec<u8>) -> std::io::Result<()> {
        let mut e = self.ar.by_name(name).map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?;
        buf.clear();
        e.read_to_end(buf)?;
        Ok(())
    }
}

/// Cheap jar identity (32 hex): sha256 of canonical-ish path + size + mtime(ns). One `stat`, no file read.
/// Choice: maven jars are immutable per path; a rebuilt local/SNAPSHOT jar changes size or mtime.
/// Use `content_hash` when a stronger guarantee is needed.
pub fn stat_key(path: &Path) -> std::io::Result<String> {
    let m = std::fs::metadata(path)?;
    let mt = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    Ok(super::hash_parts([path.as_os_str().as_encoded_bytes(), &m.len().to_le_bytes(), &mt.to_le_bytes()]))
}

/// Full sha256 (32 hex) of the jar bytes (mmap); only when the stat key is not trusted.
pub fn content_hash(path: &Path) -> std::io::Result<String> {
    let f = File::open(path)?;
    let map = unsafe { Mmap::map(&f)? };
    Ok(super::hex(&Sha256::digest(&map[..])[..16]))
}
