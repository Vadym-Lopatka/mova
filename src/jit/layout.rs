//! E3a (docs/NATIVE-TIER-DESIGN.md §1): `Value` has no explicit `repr` --
//! adding `#[repr(C, u8)]` grew `size_of::<Value>()` from 32 to 40 (measured,
//! rejected, see docs/JIT.md). Instead we discover the tag/payload byte
//! offsets ONCE at startup by constructing known-variant samples and diffing
//! their raw bytes; if the discovered layout isn't self-consistent, every
//! inline tag-test fast path in the generic JIT stays off (helpers only).

use crate::keyword::Keyword;
use crate::value::Value;
use std::sync::OnceLock;

/// `tag_off`/`payload_off` are byte offsets into a 32-byte `Value`; both
/// read as little-endian u64s (see `compute`). `payload_off` holds the
/// `i64`/`f64` bit pattern for `Int`/`Float`, and `0`/`1` for `Bool`.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub tag_off: usize,
    pub payload_off: usize,
    pub nil: u64,
    pub bool_tag: u64,
    pub int_tag: u64,
    pub float_tag: u64,
    pub keyword_tag: u64,
    /// K3: byte offset + value of `Keyword`'s inner discriminant for `Interned` (bit-copyable).
    pub kw_disc_off: usize,
    pub kw_interned: u8,
    /// K3: byte offset of an Interned keyword's u32 id.
    pub kw_id_off: usize,
}

fn bytes(v: &Value) -> [u8; 32] {
    let mut b = [0u8; 32];
    // SAFETY: `Value` is exactly 32 bytes (pinned by
    // `value::tests::size_of_value_unchanged_by_bignum_variants`). This is a
    // raw byte copy, never a typed read of padding -- the result is only
    // ever compared/diffed as bytes below, never interpreted as any other
    // type.
    unsafe { std::ptr::copy_nonoverlapping(v as *const Value as *const u8, b.as_mut_ptr(), 32) };
    b
}

fn read_u64(b: &[u8; 32], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

/// An 8-byte-aligned offset where every sample (same variant, different
/// payload) agrees -- i.e. a candidate discriminant location -- plus the
/// shared value found there.
fn stable_u64(samples: &[[u8; 32]]) -> Option<(usize, u64)> {
    for off in (0..24).step_by(8) {
        let v0 = read_u64(&samples[0], off);
        if samples.iter().all(|s| read_u64(s, off) == v0) {
            return Some((off, v0));
        }
    }
    None
}

/// Cached result of [`compute`]; probed at most once per process.
pub fn probe() -> Option<Layout> {
    static LAYOUT: OnceLock<Option<Layout>> = OnceLock::new();
    *LAYOUT.get_or_init(compute)
}

fn compute() -> Option<Layout> {
    let nil = [bytes(&Value::Nil), bytes(&Value::Nil)];
    let bools = [bytes(&Value::Bool(true)), bytes(&Value::Bool(false))];
    let ints = [
        bytes(&Value::Int(0)),
        bytes(&Value::Int(1)),
        bytes(&Value::Int(-1)),
        bytes(&Value::Int(1234)),
    ];
    let floats = [bytes(&Value::Float(0.0)), bytes(&Value::Float(2.5)), bytes(&Value::Float(-3.5))];
    let kws = [
        bytes(&Value::Keyword(Keyword::construct("a"))),
        bytes(&Value::Keyword(Keyword::construct("some-longer-probe-kw"))),
    ];

    let (tag_off, nil_tag) = stable_u64(&nil)?;
    let (bt_off, bool_tag) = stable_u64(&bools)?;
    let (it_off, int_tag) = stable_u64(&ints)?;
    let (ft_off, float_tag) = stable_u64(&floats)?;
    let (kt_off, keyword_tag) = stable_u64(&kws)?;
    // A single tag offset must cover every variant we probe, or the
    // "one shared discriminant slot" model is wrong for this build.
    if bt_off != tag_off || it_off != tag_off || ft_off != tag_off || kt_off != tag_off {
        return None;
    }
    let tags = [nil_tag, bool_tag, int_tag, float_tag, keyword_tag];
    for i in 0..tags.len() {
        for j in (i + 1)..tags.len() {
            if tags[i] == tags[j] {
                return None; // two variants alias one tag value: unusable
            }
        }
    }
    // Payload offset: where same-variant Int samples with DIFFERENT values
    // actually differ.
    let payload_off = (0..24).step_by(8).find(|&off| {
        let a = read_u64(&ints[0], off);
        ints.iter().any(|s| read_u64(s, off) != a)
    })?;
    // Cross-check against Bool/Float at that SAME offset, or the "one
    // shared payload slot" model is wrong too.
    if read_u64(&bools[0], payload_off) == read_u64(&bools[1], payload_off) {
        return None;
    }
    if floats.windows(2).all(|w| read_u64(&w[0], payload_off) == read_u64(&w[1], payload_off)) {
        return None;
    }
    // L1: tags are read as ONE byte (Rust may leave bytes 1..7 stale on write), so each must fit in it.
    if tags.iter().any(|&t| t > 0xff) || tag_off != 0 {
        return None;
    }
    // L1: the Bool payload is exactly byte `payload_off` (1/0), so a u64 store of 0/1 writes it.
    if bools[0][payload_off] != 1 || bools[1][payload_off] != 0 {
        return None;
    }
    // K3: an Interned keyword owns nothing; find the inner byte that tells it from Overflow.
    let ints_kw: Vec<Keyword> = ["a", "some-longer-probe-kw", "zz"].iter().map(|t| Keyword::construct(t)).collect();
    if !ints_kw.iter().all(|k| matches!(k, Keyword::Interned(_))) {
        return None;
    }
    let ids: Vec<u32> = ints_kw.iter().map(|k| if let Keyword::Interned(i) = k { *i } else { 0 }).collect();
    let ik: Vec<[u8; 32]> = ints_kw.into_iter().map(|k| bytes(&Value::Keyword(k))).collect();
    // The id's own bytes are never the discriminant.
    let id_off = (payload_off..29).find(|&o| ik.iter().zip(&ids).all(|(s, id)| s[o..o + 4] == id.to_le_bytes()))?;
    let ov = bytes(&Value::Keyword(Keyword::Overflow(std::sync::Arc::new(crate::value::Str::from("probe-overflow")))));
    let (kw_disc_off, kw_interned) = (payload_off..payload_off + 8).filter(|o| !(id_off..id_off + 4).contains(o)).find_map(|off| {
        let x = ik[0][off];
        (ik.iter().all(|s| s[off] == x) && ov[off] != x && ov[0] == ik[0][0]).then_some((off, x))
    })?;
    Some(Layout { tag_off, payload_off, nil: nil_tag, bool_tag, int_tag, float_tag, keyword_tag, kw_disc_off, kw_interned, kw_id_off: id_off })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Documents today's actual layout (tag at byte 0, payload at byte 8)
    /// and pins that the probe finds SOME consistent answer on this build --
    /// generic-tier inline fast paths depend on `probe().is_some()`.
    #[test]
    fn layout_probe_is_consistent() {
        let l = probe().expect("this build's Value layout should be probeable");
        assert_eq!(l.tag_off, 0);
        assert_eq!(l.payload_off, 8);
        assert_eq!(l.nil, 2);
        assert_eq!(l.bool_tag, 3);
        assert_eq!(l.int_tag, 4);
        assert_eq!(l.float_tag, 5);
        assert_eq!(l.keyword_tag, 8);
        assert_eq!((l.kw_disc_off, l.kw_interned), (8, 0));
        // Cached: calling twice must agree (same `OnceLock`).
        let l2 = probe().unwrap();
        assert_eq!(l.tag_off, l2.tag_off);
    }
}
