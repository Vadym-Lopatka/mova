//! Per-file position index (cursor lookup by binary search) and bucket order (clojure-lsp trap T1).
use crate::analyzer::*;
use crate::intern::SymId;

/// Analysis buckets as clojure-lsp sees them (after kondo normalization).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum B {
    NsDef,
    NsUsage,
    NsAlias,
    VarDef,
    VarUsage,
    Local,
    LocalUsage,
    KwDef,
    KwUsage,
    Symbols,
    ProtoImpl,
    JavaClassUsage,
    JavaClassDef,
    JavaMemberDef,
    InstInv,
}
pub const NB: usize = 15;
pub const ALL_B: [B; NB] = [
    B::NsDef, B::NsUsage, B::NsAlias, B::VarDef, B::VarUsage, B::Local, B::LocalUsage, B::KwDef, B::KwUsage, B::Symbols,
    B::ProtoImpl, B::JavaClassUsage, B::JavaClassDef, B::JavaMemberDef, B::InstInv,
];

impl B {
    pub fn keyword(self) -> &'static str {
        match self {
            B::NsDef => "namespace-definitions",
            B::NsUsage => "namespace-usages",
            B::NsAlias => "namespace-alias",
            B::VarDef => "var-definitions",
            B::VarUsage => "var-usages",
            B::Local => "locals",
            B::LocalUsage => "local-usages",
            B::KwDef => "keyword-definitions",
            B::KwUsage => "keyword-usages",
            B::Symbols => "symbols",
            B::ProtoImpl => "protocol-impls",
            B::JavaClassUsage => "java-class-usages",
            B::JavaClassDef => "java-class-definitions",
            B::JavaMemberDef => "java-member-definitions",
            B::InstInv => "instance-invocations",
        }
    }
    /// kondo's own analysis key (a bucket can come from a different kondo key).
    fn kondo_key(self) -> &'static str {
        match self {
            B::NsAlias => "namespace-usages",
            B::KwDef | B::KwUsage => "keywords",
            b => b.keyword(),
        }
    }
}

// ---- Clojure `hash` of a keyword (Murmur3 + hashCombine) and PersistentHashMap trie order ----
fn mix_k1(k: u32) -> u32 {
    k.wrapping_mul(0xcc9e2d51).rotate_left(15).wrapping_mul(0x1b873593)
}
fn mix_h1(h: u32, k: u32) -> u32 {
    (h ^ k).rotate_left(13).wrapping_mul(5).wrapping_add(0xe6546b64)
}
fn fmix(mut h: u32, len: u32) -> u32 {
    h ^= len;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85ebca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2ae35);
    h ^ (h >> 16)
}
fn hash_chars(s: &str) -> u32 {
    let u: Vec<u16> = s.encode_utf16().collect();
    let mut h = 0u32;
    let mut i = 1;
    while i < u.len() {
        h = mix_h1(h, mix_k1(u[i - 1] as u32 | ((u[i] as u32) << 16)));
        i += 2;
    }
    if u.len() & 1 == 1 {
        h ^= mix_k1(u[u.len() - 1] as u32);
    }
    fmix(h, 2 * u.len() as u32)
}
/// `(hash :name)` for an unqualified keyword.
pub fn keyword_hash(name: &str) -> u32 {
    // Symbol.hasheq = hashCombine(hash(name), hash(ns = nil) = 0); Keyword adds 0x9e3779b9
    let seed = hash_chars(name);
    let sym = seed ^ 0u32.wrapping_add(0x9e3779b9).wrapping_add(seed << 6).wrapping_add(((seed as i32) >> 2) as u32);
    sym.wrapping_add(0x9e3779b9)
}
/// Iteration position in a PersistentHashMap: 5-bit chunks from the low end, ascending.
fn trie_key(h: u32) -> [u8; 7] {
    let mut k = [0u8; 7];
    for (i, x) in k.iter_mut().enumerate() {
        *x = ((h >> (5 * i)) & 31) as u8;
    }
    k
}
fn order_by(keys: impl Fn(B) -> &'static str) -> [u8; NB] {
    let mut v: Vec<(usize, [u8; 7])> = ALL_B.iter().enumerate().map(|(i, &b)| (i, trie_key(keyword_hash(keys(b))))).collect();
    v.sort_by_key(|x| x.1);
    let mut rank = [0u8; NB];
    for (r, (i, _)) in v.iter().enumerate() {
        rank[*i] = r as u8;
    }
    rank
}
/// Rank of each bucket when the per-uri analysis map is a hash-map (> 8 buckets).
pub fn hash_rank() -> &'static [u8; NB] {
    static R: std::sync::OnceLock<[u8; NB]> = std::sync::OnceLock::new();
    R.get_or_init(|| order_by(|b| b.keyword()))
}
/// Rank for maps with <= 8 buckets (array-map, insertion order = kondo key order; alias follows its usage).
fn kondo_rank() -> &'static [u8; NB] {
    static R: std::sync::OnceLock<[u8; NB]> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        let mut r = order_by(|b| b.kondo_key());
        // same kondo key: keep definitions/usages/alias in a stable order (kondo element order is unknown here)
        for b in [B::NsAlias] {
            r[b as usize] = r[B::NsUsage as usize];
        }
        r
    })
}

/// One addressable element: name range + (bucket, index).
#[derive(Clone, Copy, Debug)]
pub struct Ent {
    pub row: u32,
    pub col: u32,
    pub end_row: u32,
    pub end_col: u32,
    pub b: B,
    pub i: u32,
}

/// Single-line element (16 B): `bi` = bucket << 27 | index.
#[derive(Clone, Copy, Debug)]
pub struct SEnt {
    pub row: u32,
    pub col: u32,
    pub end_col: u32,
    bi: u32,
}
impl SEnt {
    fn new(e: Ent) -> SEnt {
        debug_assert!(e.i < (1 << 27));
        SEnt { row: e.row, col: e.col, end_col: e.end_col, bi: (e.b as u32) << 27 | e.i }
    }
    pub fn ent(&self) -> Ent {
        Ent { row: self.row, col: self.col, end_row: self.row, end_col: self.end_col, b: ALL_B[(self.bi >> 27) as usize], i: self.bi & ((1 << 27) - 1) }
    }
}

/// Position data of one file (independent of usage target resolution).
#[derive(Debug, Default)]
pub struct PosIdx {
    /// Single-line name ranges sorted by (row, col).
    pub ents: Vec<SEnt>,
    /// Multi-line name ranges.
    pub multi: Vec<Ent>,
    /// Bucket rank (iteration order) for this file.
    pub rank: [u8; NB],
    /// local id -> index in `locals` (u32::MAX = none).
    pub local_by_id: Vec<u32>,
}

fn add(ents: &mut Vec<SEnt>, multi: &mut Vec<Ent>, p: crate::cst::Pos, b: B, i: usize) {
    if p.row == 0 || p.col == 0 {
        return;
    }
    let e = Ent { row: p.row, col: p.col, end_row: p.end_row, end_col: p.end_col, b, i: i as u32 };
    if p.end_row == p.row { ents.push(SEnt::new(e)) } else { multi.push(e) }
}

/// Element validity + name position exactly as kondo.clj `valid-element?` leaves them.
use crate::analyzer::shrink_vec;
impl PosIdx {
    pub fn shrink(&mut self) {
        shrink_vec(&mut self.ents);
        shrink_vec(&mut self.multi);
        shrink_vec(&mut self.local_by_id);
    }
    pub fn keep(&mut self) {
        crate::analyzer::shrink_vec(&mut self.ents);
        crate::analyzer::shrink_vec(&mut self.multi);
        crate::analyzer::shrink_vec(&mut self.local_by_id);
    }
}

pub fn build_pos(fa: &FileAnalysis) -> PosIdx {
    let mut ents = Vec::new();
    let mut multi = Vec::new();
    let mut present = [false; NB];
    for (i, n) in fa.namespace_definitions.iter().enumerate() {
        add(&mut ents, &mut multi, n.name_pos, B::NsDef, i);
    }
    for (i, n) in fa.namespace_usages.iter().enumerate() {
        add(&mut ents, &mut multi, n.name_pos, B::NsUsage, i);
        if n.alias_pos.row != 0 && !n.alias.is_none() {
            add(&mut ents, &mut multi, n.alias_pos, B::NsAlias, i);
        }
    }
    for (i, d) in fa.var_definitions.iter().enumerate() {
        if d.name.is_none() {
            continue;
        }
        add(&mut ents, &mut multi, d.name_pos, B::VarDef, i);
    }
    for (i, u) in fa.var_usages.iter().enumerate() {
        if u.derived || u.derived_name || u.synth {
            continue;
        }
        add(&mut ents, &mut multi, u.name_pos, B::VarUsage, i);
    }
    let mut local_by_id = vec![u32::MAX; fa.next_local_id as usize + 2];
    for (i, l) in fa.locals.iter().enumerate() {
        add(&mut ents, &mut multi, l.pos, B::Local, i);
        if let Some(s) = local_by_id.get_mut(l.id as usize) {
            *s = i as u32;
        }
    }
    for (i, l) in fa.local_usages.iter().enumerate() {
        let p = if l.name_pos.row != 0 { l.name_pos } else { l.pos };
        add(&mut ents, &mut multi, p, B::LocalUsage, i);
    }
    let internal = fa.has_callstack;
    for (i, k) in fa.keywords.iter().enumerate() {
        let b = if !k.reg.is_none() {
            B::KwDef
        } else if internal {
            B::KwUsage
        } else {
            continue;
        };
        add(&mut ents, &mut multi, k.pos, b, i);
    }
    for (i, s) in fa.symbols.iter().enumerate() {
        add(&mut ents, &mut multi, s.pos, B::Symbols, i);
    }
    for (i, p) in fa.protocol_impls.iter().enumerate() {
        if !p.derived {
            add(&mut ents, &mut multi, p.name_pos, B::ProtoImpl, i);
        }
    }
    for (i, u) in fa.java_class_usages.iter().enumerate() {
        let p = if u.flags & JU_HAS_NAME != 0 && u.name_pos.row != 0 { u.name_pos } else { u.pos };
        add(&mut ents, &mut multi, p, B::JavaClassUsage, i);
    }
    for (i, u) in fa.instance_invocations.iter().enumerate() {
        if !u.derived {
            add(&mut ents, &mut multi, u.name_pos, B::InstInv, i);
        }
    }
    for e in multi.iter() {
        present[e.b as usize] = true;
    }
    for e in ents.iter() {
        present[(e.bi >> 27) as usize] = true;
    }
    let nb = present.iter().filter(|x| **x).count();
    let rank = if nb > 8 { *hash_rank() } else { *kondo_rank() };
    ents.sort_by_key(|e| (e.row, e.col));
    PosIdx { ents, multi, rank, local_by_id }
}

impl PosIdx {
    /// All elements whose name range contains (row, col) (each bound checked separately, like
    /// clojure-lsp `xf-under-cursor`), in bucket-iteration order.
    pub fn at(&self, row: u32, col: u32) -> Vec<Ent> {
        let mut out: Vec<Ent> = Vec::new();
        let start = self.ents.partition_point(|e| e.row < row);
        for e in &self.ents[start..] {
            if e.row != row || e.col > col {
                break;
            }
            if col <= e.end_col {
                out.push(e.ent());
            }
        }
        for e in &self.multi {
            if e.row <= row && row <= e.end_row && e.col <= col && col <= e.end_col {
                out.push(*e);
            }
        }
        out.sort_by_key(|e| (self.rank[e.b as usize], e.i));
        out
    }
}

/// (to, name, usage idx) of var-usages sorted by target (after `finish_usages`).
pub fn build_targets(fa: &FileAnalysis) -> Vec<(u32, u32, u32)> {
    let mut v: Vec<(u32, u32, u32)> = fa
        .var_usages
        .iter()
        .enumerate()
        .filter(|(_, u)| !(u.derived || u.derived_name))
        .map(|(i, u)| (u.to.0, u.name.0, i as u32))
        .collect();
    v.sort();
    v
}

/// Range of `v` with the first two components equal to (a, b).
pub fn equal_range(v: &[(u32, u32, u32)], a: SymId, b: SymId) -> &[(u32, u32, u32)] {
    let lo = v.partition_point(|x| (x.0, x.1) < (a.0, b.0));
    let hi = v.partition_point(|x| (x.0, x.1) <= (a.0, b.0));
    &v[lo..hi]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyword_hashes_match_clojure() {
        assert_eq!(keyword_hash("var-usages") as i32, 835416600);
        assert_eq!(keyword_hash("namespace-usages") as i32, -1646052986);
        assert_eq!(keyword_hash("symbols") as i32, 1211743);
    }
    #[test]
    fn hash_order_matches_clojure() {
        let r = hash_rank();
        let mut names: Vec<(u8, &str)> = ALL_B.iter().map(|b| (r[*b as usize], b.keyword())).collect();
        names.sort();
        let got: Vec<&str> = names.iter().map(|x| x.1).collect();
        assert_eq!(
            got,
            [
                "java-class-usages", "namespace-usages", "locals", "local-usages", "instance-invocations", "namespace-definitions",
                "keyword-definitions", "java-member-definitions", "namespace-alias", "var-definitions", "java-class-definitions",
                "keyword-usages", "protocol-impls", "var-usages", "symbols"
            ]
        );
    }
}
