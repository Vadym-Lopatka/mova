//! JVM map iteration order of uri keys: clojure-lsp picks "the last" candidate over a hash-map of uris, and a
//! persistent hash map iterates by the 5-bit chunks (least significant first) of `(hash uri)`.

fn mix_k1(k: u32) -> u32 {
    k.wrapping_mul(0xcc9e2d51).rotate_left(15).wrapping_mul(0x1b873593)
}

/// `Murmur3.hashInt`.
fn hash_int(x: i32) -> u32 {
    if x == 0 {
        return 0;
    }
    let mut h = 0u32 ^ mix_k1(x as u32);
    h = h.rotate_left(13).wrapping_mul(5).wrapping_add(0xe6546b64);
    h ^= 4;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85ebca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2ae35);
    h ^ (h >> 16)
}

/// `(hash s)` of a Clojure string (`Util.hasheq`: murmur3 of `String.hashCode`).
pub fn clj_hash(s: &str) -> u32 {
    let mut h: i32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    hash_int(h)
}

/// Sort key reproducing persistent-hash-map iteration order of string keys.
pub fn order_key(uri: &str) -> u64 {
    let h = clj_hash(uri);
    let mut k = 0u64;
    for i in 0..7 {
        k = (k << 5) | ((h >> (5 * i)) & 31) as u64;
    }
    k
}

/// Mova `(hash <string>)` as used by its CHAMP sets (champ `DefaultBuildHasher` over `Value::Str`: tag byte 4, the bytes,
/// 0xff; FxHash-style accumulate + murmur fmix64, folded to 32 bits).
pub fn champ_hash(s: &str) -> u32 {
    const G: u64 = 0x517c_c1b7_2722_0a95;
    let acc = |st: u64, w: u64| (st.rotate_left(5) ^ w).wrapping_mul(G);
    let mut st = acc(0, 4);
    for c in s.as_bytes().chunks(8) {
        let mut b = [0u8; 8];
        b[..c.len()].copy_from_slice(c);
        st = acc(st, u64::from_ne_bytes(b));
    }
    st = acc(st, 0xff);
    st ^= st >> 33;
    st = st.wrapping_mul(0xff51_afd7_ed55_8ccd);
    st ^= st >> 33;
    st = st.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    st ^= st >> 33;
    (st ^ (st >> 32)) as u32
}

fn champ_walk(items: &mut Vec<(u32, usize)>, depth: u32, out: &mut Vec<usize>) {
    if depth > 6 || items.len() <= 1 {
        out.extend(items.iter().map(|x| x.1)); // single entry or a hash collision (insertion order)
        return;
    }
    let mut groups: Vec<Vec<(u32, usize)>> = vec![Vec::new(); 32];
    for &(h, i) in items.iter() {
        groups[((h >> (5 * depth)) & 31) as usize].push((h, i));
    }
    // CHAMP: a node's own entries (chunks holding one key) first, then its sub-nodes, each in chunk order
    for g in groups.iter().filter(|g| g.len() == 1) {
        out.push(g[0].1);
    }
    for g in groups.iter_mut().filter(|g| g.len() > 1) {
        champ_walk(g, depth + 1, out);
    }
}

/// Iteration order (indexes into `uris`) of a Mova hash set holding `uris`: clojure-lsp `find-last` picks the last.
pub fn champ_order(uris: &[&str]) -> Vec<usize> {
    let mut items: Vec<(u32, usize)> = uris.iter().enumerate().map(|(i, u)| (champ_hash(u), i)).collect();
    let mut out = Vec::with_capacity(uris.len());
    champ_walk(&mut items, 0, &mut out);
    out
}

/// Iteration order (indexes into `uris`) of a Clojure persistent hash set holding `uris` (the real JVM: `:uris` of dep-graph).
pub fn hamt_order(uris: &[&str]) -> Vec<usize> {
    let mut v: Vec<(u64, usize)> = uris.iter().enumerate().map(|(i, u)| (order_key(u), i)).collect();
    v.sort(); // equal keys = hash collision (insertion order)
    v.into_iter().map(|x| x.1).collect()
}

/// Set iteration order of the reference implementation: Clojure's hash set (default; true JVM clojure-lsp) or the
/// Mova CHAMP set (`NX_SET_ORDER=champ`: the Mova port that produced the gate goldens).
pub fn set_order(uris: &[&str]) -> Vec<usize> {
    static CHAMP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *CHAMP.get_or_init(|| std::env::var("NX_SET_ORDER").is_ok_and(|v| v == "champ")) {
        champ_order(uris)
    } else {
        hamt_order(uris)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_hashes() {
        // values from Clojure 1.12.1 `(hash ...)`
        assert_eq!(clj_hash("a") as i32, 1455541201);
        assert_eq!(clj_hash("app.core") as i32, 1435577696);
        assert_eq!(clj_hash("jar:file:///x/y.jar!/cljs/core.cljc") as i32, -538407358);
    }
    #[test]
    fn champ_order_matches_mova_set_iteration() {
        // `(seq (set ["u0/x.clj" .. "u39/x.clj"]))` as printed by mova
        let want = ["u8/x.clj", "u37/x.clj", "u9/x.clj", "u20/x.clj", "u19/x.clj", "u29/x.clj", "u39/x.clj", "u11/x.clj", "u34/x.clj", "u12/x.clj", "u14/x.clj", "u27/x.clj", "u23/x.clj", "u35/x.clj", "u6/x.clj", "u13/x.clj", "u4/x.clj", "u10/x.clj", "u1/x.clj", "u26/x.clj", "u5/x.clj", "u0/x.clj", "u21/x.clj", "u32/x.clj", "u30/x.clj", "u22/x.clj", "u38/x.clj", "u15/x.clj", "u2/x.clj", "u18/x.clj", "u17/x.clj", "u33/x.clj", "u25/x.clj", "u36/x.clj", "u16/x.clj", "u24/x.clj", "u7/x.clj", "u3/x.clj", "u28/x.clj", "u31/x.clj"];
        let names: Vec<String> = (0..40).map(|i| format!("u{i}/x.clj")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let got: Vec<&str> = champ_order(&refs).into_iter().map(|i| refs[i]).collect();
        assert_eq!(got, want);
    }
    #[test]
    fn champ_order_two_jars() {
        // mova `(seq (set [a b]))` prints 1.12.4 then 1.12.6 (e2test: definition of clojure.core vars lands in 1.12.6)
        let us = ["zipfile:///Users/me/.m2/repository/org/clojure/clojure/1.12.4/clojure-1.12.4.jar::clojure/core.clj", "zipfile:///Users/me/.m2/repository/org/clojure/clojure/1.12.6/clojure-1.12.6.jar::clojure/core.clj"];
        assert_eq!(champ_order(&us), vec![0, 1]);
    }
    #[test]
    fn hamt_order_two_jars_like_real_jvm() {
        // real clojure-lsp (jar scheme): the last uri of the ns set wins (b_jarver.py: clojure.string -> 1.12.6, cheshire.core -> 6.1.0)
        let j = |p: &str, e: &str| format!("jar:file:///Users/me/.m2/repository/{p}!/{e}");
        let us = [j("org/clojure/clojure/1.12.4/clojure-1.12.4.jar", "clojure/string.clj"), j("org/clojure/clojure/1.12.6/clojure-1.12.6.jar", "clojure/string.clj")];
        let r: Vec<&str> = us.iter().map(|s| s.as_str()).collect();
        assert_eq!(hamt_order(&r).last().copied(), Some(1));
        let us = [j("cheshire/cheshire/6.1.0/cheshire-6.1.0.jar", "cheshire/core.clj"), j("cheshire/cheshire/6.2.0/cheshire-6.2.0.jar", "cheshire/core.clj")];
        let r: Vec<&str> = us.iter().map(|s| s.as_str()).collect();
        assert_eq!(hamt_order(&r).last().copied(), Some(0));
    }
}
