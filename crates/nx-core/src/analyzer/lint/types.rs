//! clj-kondo's basic type checking (`types.clj`): expression tags of literals, locals with hints and calls of
//! core fns, matched against the built-in arg specs (`specs.txt`, dumped by `tools/dump_type_specs.clj`).
//! Tags that kondo infers through user fn return types / param inference are unknown here (no finding).
use super::*;
use crate::analyzer::defs::{fast_map, FastMap};
use crate::cst::{Cst, Kind, NodeId};
use crate::intern::{intern, SymId};
use std::sync::OnceLock;

pub const KNOWN: &[&str] = &[
    "string", "char-sequence", "seqable", "int", "long", "short", "number", "pos-int", "nat-int", "neg-int", "double", "byte", "ratio", "vector", "sequential", "associative", "coll", "ideref", "ifn", "stack", "map", "nil", "set",
    "sorted-set", "fn", "keyword", "symbol", "transducer", "list", "seq", "sorted-map", "boolean", "true", "false", "truthy", "atom", "future", "regex", "char", "seqable-or-transducer", "throwable", "any", "float", "var", "ilookup",
    "array", "inst", "class",
];
pub const OTHER: u8 = 255;
/// Tag ids from here on index `LintState::utab` (union tags of locals).
pub const UNION_BASE: u8 = 128;
pub const K_ANY: u8 = 41;
pub const K_NIL: u8 = 21;
pub const K_SEQABLE: u8 = 2;
pub const K_BOOLEAN: u8 = 31;

/// A keyword tag: known type index (or `OTHER`) and the `:nilable/` flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Kw(pub u8, pub bool);

fn kid(name: &str) -> u8 {
    KNOWN.iter().position(|k| *k == name).map_or(OTHER, |p| p as u8)
}
pub fn kw_named(name: &str) -> Kw {
    k(name)
}

fn k(name: &str) -> Kw {
    Kw(kid(name), false)
}

/// `is-a-relations` / `could-be-relations` (kondo `types.clj`), as (type, related...) rows.
const IS_A: &[(&str, &[&str])] = &[
    ("string", &["char-sequence", "seqable"]),
    ("char-sequence", &["seqable"]),
    ("int", &["number"]),
    ("long", &["number"]),
    ("short", &["number"]),
    ("pos-int", &["int", "nat-int", "number"]),
    ("nat-int", &["int", "number"]),
    ("neg-int", &["int", "number"]),
    ("double", &["number"]),
    ("float", &["number"]),
    ("byte", &["number"]),
    ("ratio", &["number"]),
    ("vector", &["seqable", "sequential", "associative", "coll", "ifn", "stack", "ilookup"]),
    ("map", &["seqable", "associative", "coll", "ifn", "ilookup"]),
    ("nil", &["seqable"]),
    ("coll", &["seqable"]),
    ("set", &["seqable", "coll", "ifn", "ilookup"]),
    ("sorted-set", &["set", "seqable", "coll", "ifn", "ilookup"]),
    ("fn", &["ifn"]),
    ("keyword", &["ifn"]),
    ("symbol", &["ifn"]),
    ("associative", &["seqable", "coll", "ifn", "ilookup"]),
    ("transducer", &["ifn", "fn"]),
    ("list", &["seq", "sequential", "seqable", "coll", "stack"]),
    ("seq", &["seqable", "sequential", "coll"]),
    ("sequential", &["coll", "seqable"]),
    ("sorted-map", &["map", "seqable", "associative", "coll", "ifn", "ilookup"]),
    ("true", &["boolean"]),
    ("false", &["boolean"]),
    ("atom", &["ideref"]),
    ("future", &["ideref"]),
    ("var", &["ideref", "ifn"]),
    ("array", &["seqable", "ilookup"]),
];
const COULD_BE: &[(&str, &[&str])] = &[
    ("boolean", &["true", "false"]),
    ("char-sequence", &["string"]),
    ("int", &["neg-int", "nat-int", "pos-int", "long", "short", "float", "double", "number"]),
    ("long", &["int", "neg-int", "nat-int", "pos-int", "short", "float", "double", "number"]),
    ("short", &["int", "long", "neg-int", "nat-int", "pos-int", "float", "double", "number"]),
    ("pos-int", &["long", "short", "float", "double", "number"]),
    ("nat-int", &["pos-int", "long", "short", "float", "double", "number"]),
    ("neg-int", &["long", "short", "float", "double", "number"]),
    ("byte", &["int", "long", "short", "float", "double", "number"]),
    ("float", &["double", "number"]),
    ("double", &["float", "number"]),
    ("number", &["neg-int", "pos-int", "nat-int", "int", "long", "short", "double", "byte", "ratio", "float"]),
    ("coll", &["map", "sorted-map", "vector", "set", "sorted-set", "list", "associative", "seq", "sequential", "ifn", "stack", "ilookup"]),
    ("seqable", &["coll", "vector", "set", "sorted-set", "map", "associative", "char-sequence", "string", "nil", "list", "seq", "sequential", "ifn", "stack", "sorted-map", "ilookup", "array"]),
    ("associative", &["map", "vector", "sequential", "stack", "sorted-map"]),
    ("ifn", &["fn", "transducer", "symbol", "keyword", "map", "set", "sorted-set", "vector", "associative", "seqable", "coll", "sequential", "stack", "sorted-map", "var", "ideref", "ilookup"]),
    ("fn", &["transducer"]),
    ("seq", &["list", "stack"]),
    ("stack", &["list", "vector", "seq", "sequential", "seqable", "coll", "ifn", "associative", "ilookup"]),
    ("sequential", &["seq", "list", "vector", "ifn", "associative", "stack", "ilookup"]),
    ("map", &["sorted-map"]),
    ("set", &["sorted-set"]),
    ("ideref", &["atom", "future", "var", "ifn"]),
    ("ilookup", &["map", "set", "sorted-set", "sorted-map", "coll", "seqable", "ifn", "associative", "vector", "sequential", "stack", "array"]),
];

struct Rel {
    is_a: Vec<u64>,
    could: Vec<u64>,
}

fn rel() -> &'static Rel {
    static R: OnceLock<Rel> = OnceLock::new();
    R.get_or_init(|| {
        let n = KNOWN.len();
        let mut r = Rel { is_a: vec![0; n], could: vec![0; n] };
        for (a, bs) in IS_A {
            for b in *bs {
                r.is_a[kid(a) as usize] |= 1u64 << kid(b);
            }
        }
        for (a, bs) in COULD_BE {
            for b in *bs {
                r.could[kid(a) as usize] |= 1u64 << kid(b);
            }
        }
        r
    })
}

fn rel_has(table: &[u64], a: u8, b: u8) -> bool {
    a != OTHER && b != OTHER && table[a as usize] & (1u64 << b) != 0
}

/// `types/match?` for keyword tags.
pub fn match_kw(actual: Kw, expected: Kw) -> bool {
    let r = rel();
    let (any, nil, seqable) = (K_ANY, K_NIL, K_SEQABLE);
    if actual == expected || actual.0 == any || expected.0 == any {
        return true;
    }
    // :truthy (some truthy value of unknown type) matches every type except a falsy one
    if !actual.1 && actual.0 == kid("truthy") && !(expected.0 == K_NIL || expected.0 == kid("false")) {
        return true;
    }
    // non-nilable keys only appear in the relation tables
    if !actual.1 && !expected.1 && (rel_has(&r.is_a, actual.0, expected.0) || rel_has(&r.could, actual.0, expected.0)) {
        return true;
    }
    // `(contains? (get is-a-relations actual) expected)` with a nilable keyword never hits; the unnil'd comparison does
    let (kk, target) = (actual.0, expected.0);
    if kk == target || target == any || rel_has(&r.is_a, kk, target) || rel_has(&r.could, kk, target) || kk == OTHER {
        return true;
    }
    if actual.0 == nil && !actual.1 && (expected.1 || expected.0 == seqable) {
        return true;
    }
    false
}

/// Spec value (argument spec of a fn).
#[derive(Clone, Debug)]
pub enum S {
    Kw(Kw),
    Set(Vec<Kw>),
    Rest { spec: Box<S>, last: Option<Box<S>> },
    Vec(Vec<S>),
}

#[derive(Clone, Debug)]
pub enum Ret {
    Kw(Kw),
    Set(Vec<Kw>),
}

#[derive(Clone, Debug, Default)]
pub struct ArSpec {
    pub args: Vec<S>,
    pub ret: Option<Ret>,
    pub min_arity: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct FnSpec {
    pub fixed: Vec<(u32, ArSpec)>,
    pub varargs: Option<ArSpec>,
}

impl FnSpec {
    /// kondo `called-arity`.
    pub fn arity(&self, n: u32) -> Option<&ArSpec> {
        if let Some((_, a)) = self.fixed.iter().find(|(k, _)| *k == n) {
            return Some(a);
        }
        let v = self.varargs.as_ref()?;
        match v.min_arity {
            Some(m) => (n >= m).then_some(v),
            None => Some(v),
        }
    }
}

fn kw_of(c: &Cst, n: NodeId) -> Option<Kw> {
    if c.kind(n) != Kind::Keyword {
        return None;
    }
    let name = c.name(n).as_str();
    Some(if c.ns(n).as_str() == "nilable" { Kw(kid(name), true) } else { Kw(kid(name), false) })
}

fn parse_s(c: &Cst, n: NodeId) -> Option<S> {
    match c.kind(n) {
        Kind::Keyword => kw_of(c, n).map(S::Kw),
        Kind::Set => Some(S::Set(c.sig_children(n).filter_map(|x| kw_of(c, x)).collect())),
        Kind::Vector => Some(S::Vec(c.sig_children(n).filter_map(|x| parse_s(c, x)).collect())),
        Kind::Map => {
            let kids: Vec<NodeId> = c.sig_children(n).collect();
            let (mut spec, mut last) = (None, None);
            let mut i = 0;
            while i + 1 < kids.len() {
                match c.name(kids[i]).as_str() {
                    "spec" => spec = parse_s(c, kids[i + 1]),
                    "last" => last = parse_s(c, kids[i + 1]),
                    _ => {}
                }
                i += 2;
            }
            Some(S::Rest { spec: Box::new(spec?), last: last.map(Box::new) })
        }
        _ => None,
    }
}

fn parse_arity(c: &Cst, n: NodeId) -> ArSpec {
    let mut a = ArSpec::default();
    let kids: Vec<NodeId> = c.sig_children(n).collect();
    let mut i = 0;
    while i + 1 < kids.len() {
        let v = kids[i + 1];
        match c.name(kids[i]).as_str() {
            "args" => a.args = c.sig_children(v).filter_map(|x| parse_s(c, x)).collect(),
            "ret" => {
                a.ret = match c.kind(v) {
                    Kind::Keyword => kw_of(c, v).map(Ret::Kw),
                    Kind::Set => Some(Ret::Set(c.sig_children(v).filter_map(|x| kw_of(c, x)).collect())),
                    _ => None,
                }
            }
            "min-arity" => a.min_arity = c.text(v).parse().ok(),
            _ => {}
        }
        i += 2;
    }
    a
}

/// Names that have a spec in some namespace (cheap pre-filter for call heads).
pub fn spec_names() -> &'static crate::analyzer::defs::FastSet<SymId> {
    static N: OnceLock<crate::analyzer::defs::FastSet<SymId>> = OnceLock::new();
    N.get_or_init(|| specs(false).keys().chain(specs(true).keys()).map(|k| k.1).chain(std::iter::once(intern("throw"))).collect())
}

fn parse_fn_spec(edn: &str) -> Option<FnSpec> {
    let c = crate::reader::parse(edn);
    let root = c.root();
    let map = c.children(root).iter().copied().find(|&n| c.kind(n) == Kind::Map)?;
    let kids: Vec<NodeId> = c.sig_children(map).collect();
    let mut spec = FnSpec::default();
    let mut i = 0;
    while i + 1 < kids.len() {
        let (kk, v) = (kids[i], kids[i + 1]);
        let a = parse_arity(&c, v);
        if c.kind(kk) == Kind::Number {
            if let Ok(n) = c.text(kk).parse::<u32>() {
                spec.fixed.push((n, a));
            }
        } else {
            spec.varargs = Some(a);
        }
        i += 2;
    }
    Some(spec)
}

/// Built-in specs by (namespace, name) for clj / cljs: kondo `built-in-specs`, plus the per-arity tags of the
/// built-in cache (derived from type hints) for arities without a spec (`lint-arg-types` falls back on the
/// called var's `:arities`); `:ret` of cache arities only counts for vars that have no spec.
pub fn specs(cljs: bool) -> &'static FastMap<(SymId, SymId), FnSpec> {
    static S: [OnceLock<FastMap<(SymId, SymId), FnSpec>>; 2] = [OnceLock::new(), OnceLock::new()];
    S[cljs as usize].get_or_init(|| {
        let mut m: FastMap<(SymId, SymId), FnSpec> = fast_map();
        for line in include_str!("specs.txt").lines() {
            let mut it = line.splitn(3, ' ');
            let (Some(ns), Some(name), Some(edn)) = (it.next(), it.next(), it.next()) else { continue };
            if let Some(spec) = parse_fn_spec(edn) {
                m.insert((intern(ns), intern(name)), spec);
            }
        }
        let want = if cljs { "cljs" } else { "clj" };
        for line in include_str!("cachespecs.txt").lines() {
            let mut it = line.splitn(4, ' ');
            let (Some(lang), Some(ns), Some(name), Some(edn)) = (it.next(), it.next(), it.next(), it.next()) else { continue };
            if lang != want {
                continue;
            }
            let Some(mut cs) = parse_fn_spec(edn) else { continue };
            let key = (intern(ns), intern(name));
            match m.get_mut(&key) {
                None => {
                    m.insert(key, cs);
                }
                Some(base) => {
                    for (n, mut a) in cs.fixed.drain(..) {
                        if base.arity(n).is_none() {
                            a.ret = None;
                            base.fixed.push((n, a));
                        }
                    }
                    if base.varargs.is_none() {
                        if let Some(mut v) = cs.varargs.take() {
                            v.ret = None;
                            base.varargs = Some(v);
                        }
                    }
                }
            }
        }
        m
    })
}

pub fn label(k: Kw) -> String {
    let base = match KNOWN.get(k.0 as usize) {
        Some(n) => match *n {
            "nil" => "nil",
            "string" => "string",
            "number" => "number",
            "int" => "integer",
            "long" => "long",
            "short" => "short",
            "double" => "double",
            "float" => "float",
            "pos-int" => "positive integer",
            "nat-int" => "natural integer",
            "neg-int" => "negative integer",
            "byte" => "byte",
            "ratio" => "ratio",
            "seqable" => "seqable collection",
            "seq" => "seq",
            "vector" => "vector",
            "stack" => "stack (list, vector, etc.)",
            "associative" => "associative collection",
            "map" => "map",
            "coll" => "collection",
            "list" => "list",
            "regex" => "regular expression",
            "char" => "character",
            "boolean" | "true" | "false" => "boolean",
            "truthy" => "truthy value",
            "atom" => "atom",
            "future" => "future",
            "ideref" => "deref",
            "fn" | "ifn" => "function",
            "keyword" => "keyword",
            "symbol" => "symbol",
            "transducer" => "transducer",
            "seqable-or-transducer" => "seqable or transducer",
            "set" => "set",
            "sorted-set" => "sorted set",
            "char-sequence" => "char sequence",
            "sequential" => "sequential collection",
            "throwable" => "throwable",
            "sorted-map" => "sorted map",
            "var" => "var",
            "ilookup" => "ILookup",
            "array" => "array",
            "class" => "class",
            "inst" => "instant",
            other => other,
        },
        None => "any",
    };
    if k.1 {
        format!("{} or nil", base)
    } else {
        base.to_owned()
    }
}

// ---- Clojure hash order of keyword sets (PersistentHashSet iteration) ----

fn murmur3_chars(s: &str) -> i32 {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut h1: i32 = 0;
    let mut i = 1;
    while i < units.len() {
        let mut k1: i32 = (units[i - 1] as i32) | ((units[i] as i32) << 16);
        k1 = k1.wrapping_mul(0xcc9e2d51u32 as i32);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(0x1b873593);
        h1 ^= k1;
        h1 = h1.rotate_left(13);
        h1 = h1.wrapping_mul(5).wrapping_add(0xe6546b64u32 as i32);
        i += 2;
    }
    if units.len() % 2 == 1 {
        let mut k1: i32 = units[units.len() - 1] as i32;
        k1 = k1.wrapping_mul(0xcc9e2d51u32 as i32);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(0x1b873593);
        h1 ^= k1;
    }
    h1 ^= (units.len() as i32) * 2;
    // fmix
    h1 ^= ((h1 as u32) >> 16) as i32;
    h1 = h1.wrapping_mul(0x85ebca6bu32 as i32);
    h1 ^= ((h1 as u32) >> 13) as i32;
    h1 = h1.wrapping_mul(0xc2b2ae35u32 as i32);
    h1 ^= ((h1 as u32) >> 16) as i32;
    h1
}

fn hash_combine(seed: i32, h: i32) -> i32 {
    seed ^ h.wrapping_add(0x9e3779b9u32 as i32).wrapping_add(seed << 6).wrapping_add(((seed as u32) >> 2) as i32)
}

/// `Keyword.hasheq` of `:ns/name`.
fn keyword_hash(ns: Option<&str>, name: &str) -> i32 {
    let sym = hash_combine(murmur3_chars(name), ns.map_or(0, |n| java_string_hash(n)));
    sym.wrapping_add(0x9e3779b9u32 as i32)
}

/// `Util.hash(String)` = `String.hashCode` (used for the namespace part).
fn java_string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    h
}

/// Order of keywords in a Clojure hash set (HAMT iteration: 5-bit chunks of the hash from the low end).
pub fn hash_set_order(ks: &[Kw]) -> Vec<Kw> {
    let mut v: Vec<(Vec<u32>, Kw)> = ks
        .iter()
        .map(|&kw| {
            let name = KNOWN.get(kw.0 as usize).copied().unwrap_or("any");
            let h = if kw.1 { keyword_hash(Some("nilable"), name) } else { keyword_hash(None, name) } as u32;
            ((0..7).map(|i| (h >> (5 * i)) & 31).collect(), kw)
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v.into_iter().map(|x| x.1).collect()
}

pub fn label_set(ks: &[Kw]) -> String {
    hash_set_order(ks).iter().map(|&k| label(k)).collect::<Vec<_>>().join(" or ")
}

/// Actual tag of an argument.
#[derive(Clone, Debug)]
pub enum Ty {
    K(Kw),
    U(Vec<Kw>),
}

pub fn ty_kws_pub(t: Ty) -> Vec<Kw> {
    ty_kws(t)
}

fn ty_kws(t: Ty) -> Vec<Kw> {
    match t {
        Ty::K(k) => vec![k],
        Ty::U(v) => v,
    }
}

fn norm_ty(mut ks: Vec<Kw>) -> Ty {
    let mut out: Vec<Kw> = Vec::new();
    for k in ks.drain(..) {
        if !out.contains(&k) {
            out.push(k);
        }
    }
    if out.len() == 1 {
        Ty::K(out[0])
    } else {
        Ty::U(out)
    }
}

/// `tu/union-type` over normalized sets; an unknown side makes the result unknown (`:any`).
fn union_ks(a: Option<Vec<Kw>>, b: Option<Vec<Kw>>) -> Option<Vec<Kw>> {
    let (mut a, b) = (a?, b?);
    if a.iter().chain(b.iter()).any(|k| k.0 == K_ANY) {
        return None;
    }
    for k in b {
        if !a.contains(&k) {
            a.push(k);
        }
    }
    Some(a)
}

/// `tu/truthiness-tag`: members reduced to their truthiness (nilable / seqable include nil); None when unknown.
fn truthiness(t: &Option<Ty>) -> Option<Vec<Kw>> {
    let ks = ty_kws(t.clone()?);
    let mut out: Vec<Kw> = Vec::new();
    for k in ks {
        if k.0 == OTHER {
            return None;
        }
        if k.1 {
            out.push(Kw(K_NIL, false));
            out.push(Kw(k.0, false));
        } else if k.0 == K_SEQABLE {
            out.push(Kw(K_NIL, false));
            out.push(Kw(K_SEQABLE, false));
        } else {
            out.push(k);
        }
    }
    Some(out)
}

fn is_falsy_kw(k: Kw) -> bool {
    !k.1 && (k.0 == K_NIL || k.0 == kid("false"))
}

fn is_truthy_kw(k: Kw) -> bool {
    !k.1 && !matches!(k.0, x if x == K_ANY || x == K_NIL || x == kid("false") || x == K_BOOLEAN || x == K_SEQABLE)
}

fn always_falsy(t: &Option<Ty>) -> bool {
    truthiness(t).map_or(false, |v| !v.is_empty() && v.iter().all(|&k| is_falsy_kw(k)))
}

fn never_falsy(t: &Option<Ty>) -> bool {
    truthiness(t).map_or(false, |v| !v.is_empty() && v.iter().all(|&k| is_truthy_kw(k)))
}

/// `tu/truthy-part`; `Some(None)` = ::nothing.
fn truthy_part(t: &Option<Ty>) -> Option<Option<Vec<Kw>>> {
    let Some(ks) = truthiness(t) else { return Some(Some(vec![k("truthy")])) };
    let mut out = Vec::new();
    for x in ks {
        if is_falsy_kw(x) {
            continue;
        }
        out.push(if !x.1 && x.0 == K_BOOLEAN { k("true") } else if !x.1 && x.0 == K_ANY { k("truthy") } else { x });
    }
    Some(if out.is_empty() { None } else { Some(out) })
}

/// `tu/falsy-part`.
fn falsy_part(t: &Option<Ty>) -> Option<Option<Vec<Kw>>> {
    let Some(ks) = truthiness(t) else { return Some(Some(vec![k("nil"), k("false")])) };
    let mut out = Vec::new();
    for x in ks {
        if is_falsy_kw(x) {
            out.push(x);
        } else if !x.1 && x.0 == K_BOOLEAN {
            out.push(k("false"));
        } else if !x.1 && x.0 == K_ANY {
            out.push(k("nil"));
            out.push(k("false"));
        }
    }
    Some(if out.is_empty() { None } else { Some(out) })
}

/// `tu/absorb` of a normalized set.
fn absorb(ks: Vec<Kw>) -> Ty {
    if ks.iter().any(|k| k.0 == K_ANY) {
        return Ty::K(Kw(K_ANY, false));
    }
    let mut v = ks;
    if v.iter().any(|k| !k.1 && k.0 == K_BOOLEAN) {
        v.retain(|k| !(k.0 == kid("true") || k.0 == kid("false")));
    }
    if v.iter().any(|k| !k.1 && k.0 == kid("truthy")) {
        v.retain(|k| !(is_truthy_kw(*k) && k.0 != kid("truthy")));
    }
    norm_ty(v)
}

/// `tu/fold-logic`; None = unknown (`:any`).
fn fold_logic(args: &[Option<Ty>], stop: fn(&Option<Ty>) -> bool, part: fn(&Option<Ty>) -> Option<Option<Vec<Kw>>>) -> Option<Ty> {
    let mut acc: Option<Vec<Kw>> = Some(Vec::new());
    for (i, a) in args.iter().enumerate() {
        let last = i + 1 == args.len();
        if last || stop(a) {
            acc = union_ks(acc, a.clone().map(ty_kws));
            break;
        }
        match part(a) {
            Some(None) => {}
            Some(Some(p)) => acc = union_ks(acc, Some(p)),
            None => return None,
        }
    }
    acc.map(absorb)
}

fn match_ty(t: &Ty, expected: Kw) -> bool {
    match t {
        Ty::K(k) => match_kw(*k, expected),
        Ty::U(ks) => ks.iter().any(|&k| match_kw(k, expected)),
    }
}

fn ty_label(t: &Ty) -> String {
    match t {
        Ty::K(k) => label(*k),
        Ty::U(ks) => label_set(ks),
    }
}

/// `tag-from-meta` of a type hint symbol.
pub fn tag_from_hint(name: &str) -> Option<Kw> {
    Some(match name {
        "void" => k("nil"),
        "boolean" => k("boolean"),
        "Boolean" | "java.lang.Boolean" => Kw(kid("boolean"), true),
        "byte" => k("byte"),
        "Byte" | "java.lang.Byte" => Kw(kid("byte"), true),
        "Number" | "java.lang.Number" => Kw(kid("number"), true),
        "int" => k("int"),
        "Integer" | "java.lang.Integer" => Kw(kid("int"), true),
        "long" => k("long"),
        "Long" | "java.lang.Long" => Kw(kid("long"), true),
        "short" => k("short"),
        "Short" | "java.lang.Short" => Kw(kid("short"), true),
        "double" => k("double"),
        "float" => k("float"),
        "Double" | "java.lang.Double" => Kw(kid("double"), true),
        "Float" | "java.lang.Float" => Kw(kid("float"), true),
        "ratio?" => k("ratio"),
        "CharSequence" | "java.lang.CharSequence" => Kw(kid("char-sequence"), true),
        "String" | "java.lang.String" => Kw(kid("string"), true),
        "char" => k("char"),
        "Character" | "java.lang.Character" => Kw(kid("char"), true),
        "Seqable" | "clojure.lang.Seqable" => k("seqable"),
        "java.util.List" => Kw(kid("list"), true),
        "class" => k("class"),
        "Class" | "java.lang.Class" => Kw(kid("class"), true),
        "Date" | "java.util.Date" => Kw(kid("inst"), true),
        "Future" | "java.util.concurrent.Future" => Kw(kid("future"), true),
        "future" => k("future"),
        _ => return None,
    })
}

impl<'a> Analyzer<'a> {
    /// kondo `types/number->tag` of a number token.
    fn number_tag(&self, n: NodeId) -> Kw {
        let t = self.c.text(n);
        let neg = t.starts_with('-');
        let body = t.trim_start_matches(['-', '+']);
        if body.contains('/') {
            return k("ratio");
        }
        if body.ends_with('N') || body.ends_with('M') {
            return k("number");
        }
        let hex = body.starts_with("0x") || body.starts_with("0X");
        let radix = body.contains(['r', 'R']);
        if !hex && !radix && (body.contains('.') || body.contains(['e', 'E'])) {
            return k("double");
        }
        let digits = body.len();
        if digits > 18 && !hex {
            return k("number");
        }
        let zero = body.chars().all(|c| c == '0');
        if neg && !zero {
            k("neg-int")
        } else if zero {
            k("nat-int")
        } else {
            k("pos-int")
        }
    }

    /// Spec key for a call head that resolves to a namespace with built-in specs, without side effects.
    fn spec_fn_of(&self, head: NodeId) -> Option<&'static FnSpec> {
        let (ns, name) = (self.c.ns(head), self.c.name(head));
        if !spec_names().contains(&name) {
            return None;
        }
        let cljs = self.is_cljs();
        let cur = self.cur_ns();
        let rns = if ns.is_none() {
            if self.find_binding(name).is_some() || cur.vars.contains(&name) || cur.referred.contains_key(&name) || cur.clojure_excluded.contains(&name) || !(crate::analyzer::defs::core_sym(cljs, name) || crate::analyzer::resolve::is_special_symbol(name.as_str())) {
                return None;
            }
            self.core_ns()
        } else {
            let q = cur.qualify.get(&ns).copied().or(if cur.name == ns { Some(ns) } else { None })?;
            q
        };
        specs(cljs).get(&(rns, name))
    }

    /// Tags of core forms with a `:fn` spec in kondo (`if`, `do`, `let`, `when`, ...): `Some(tag)` when `head` is
    /// such a form (the tag may be unknown), `None` to continue with the call specs.
    fn ty_of_special(&self, n: NodeId, head: NodeId, nargs: u32) -> Option<Option<Ty>> {
        let name = self.c.name(head).as_str();
        if !matches!(name, "if" | "if-let" | "when" | "when-not" | "when-let" | "do" | "let" | "atom" | "identity" | "with-meta" | "doto" | "cond" | "and" | "or") {
            return None;
        }
        let ns = self.c.ns(head);
        let cur = self.cur_ns();
        if ns.is_none() {
            let nm = self.c.name(head);
            if self.find_binding(nm).is_some() || cur.vars.contains(&nm) || cur.referred.contains_key(&nm) || cur.clojure_excluded.contains(&nm) {
                return None;
            }
        } else if cur.qualify.get(&ns).copied() != Some(self.core_ns()) {
            return None;
        }
        let kids = self.c.children(n);
        let arg = |i: usize| -> Option<Ty> { kids.get(i + 1).and_then(|&a| self.ty_of(a)) };
        let union = |a: Option<Ty>, b: Option<Ty>| -> Option<Ty> {
            let (a, b) = (a?, b?);
            let mut ks: Vec<Kw> = Vec::new();
            for t in [a, b] {
                match t {
                    Ty::K(k) => ks.push(k),
                    Ty::U(v) => ks.extend(v),
                }
            }
            ks.dedup();
            Some(Ty::U(ks))
        };
        Some(match name {
            "atom" => Some(Ty::K(k("atom"))),
            "if" if nargs >= 2 => union(arg(1), if nargs < 3 { Some(Ty::K(k("nil"))) } else { arg(2) }),
            "if-let" if nargs >= 2 => union(arg(1), arg(2)),
            "when" | "when-not" | "when-let" if nargs >= 1 => union(Some(Ty::K(k("nil"))), arg(nargs as usize - 1)),
            "do" if nargs >= 1 => arg(nargs as usize - 1),
            "let" if nargs >= 2 => arg(nargs as usize - 1),
            // a->a / doto: the tag of the first arg
            "identity" | "with-meta" | "doto" if nargs >= 1 => arg(0),
            "cond" => {
                let tags: Vec<Option<Ty>> = (0..nargs as usize).map(arg).collect();
                // pairs: (cond, ret); a missing last ret is an unknown tag
                let n = tags.len();
                let last_cond_kw = if n == 0 { false } else { let li = if n % 2 == 1 { n - 1 } else { n - 2 }; matches!(kids.get(li + 1).map(|&x| self.kind(self.c.unwrap_meta(x))), Some(Kind::Keyword)) };
                let mut rets: Vec<Option<Ty>> = Vec::new();
                let mut i = 0;
                while i < n {
                    rets.push(if i + 1 < n { tags[i + 1].clone() } else { None });
                    i += 2;
                }
                let mut acc: Option<Vec<Kw>> = if last_cond_kw { Some(Vec::new()) } else { Some(vec![k("nil")]) };
                for r in rets {
                    acc = union_ks(acc, r.map(ty_kws));
                }
                acc.map(norm_ty)
            }
            "and" => {
                let tags: Vec<Option<Ty>> = (0..nargs as usize).map(arg).collect();
                if tags.is_empty() {
                    Some(Ty::K(k("true")))
                } else {
                    fold_logic(&tags, always_falsy, falsy_part)
                }
            }
            "or" => {
                let tags: Vec<Option<Ty>> = (0..nargs as usize).map(arg).collect();
                if tags.is_empty() {
                    Some(Ty::K(k("nil")))
                } else {
                    fold_logic(&tags, never_falsy, truthy_part)
                }
            }
            _ => None,
        })
    }

    /// Tag of an expression: `None` = unknown (`:any`).
    pub fn ty_of(&self, n: NodeId) -> Option<Ty> {
        let n = self.c.unwrap_meta(n);
        Some(Ty::K(match self.kind(n) {
            Kind::Nil => k("nil"),
            Kind::True | Kind::False => k("boolean"),
            Kind::String => k("string"),
            Kind::Number => self.number_tag(n),
            Kind::Symbolic => k("double"),
            Kind::Keyword => k("keyword"),
            Kind::Char => k("char"),
            Kind::Regex => k("regex"),
            Kind::Vector => k("vector"),
            Kind::Map | Kind::NsMap => k("map"),
            Kind::Set => k("set"),
            Kind::AnonFn => k("fn"),
            Kind::Var => k("var"),
            Kind::Quote => {
                let x = self.c.nth(n, 0)?;
                match self.kind(self.c.unwrap_meta(x)) {
                    Kind::Symbol => k("symbol"),
                    Kind::List => k("list"),
                    Kind::Vector => k("vector"),
                    Kind::Map => k("map"),
                    Kind::Set => k("set"),
                    Kind::Keyword => k("keyword"),
                    Kind::String => k("string"),
                    Kind::Number => self.number_tag(self.c.unwrap_meta(x)),
                    Kind::Char => k("char"),
                    Kind::Nil => k("nil"),
                    Kind::True | Kind::False => k("boolean"),
                    _ => return None,
                }
            }
            Kind::Symbol => {
                if self.c.ns(n).is_none() {
                    if !self.lt.tagged_any {
                        return None;
                    }
                    let b = self.find_binding(self.c.name(n))?;
                    if b.tag == 0 {
                        return None;
                    }
                    let id16 = (b.tag >> 1) - 1;
                    if id16 >= super::rets::RT_BASE {
                        return None;
                    }
                    let id = id16 as u8;
                    if id >= UNION_BASE {
                        return self.lt.utab.get((id - UNION_BASE) as usize).map(|u| Ty::U(u.clone()));
                    }
                    return Some(Ty::K(Kw(id, b.tag & 1 == 1)));
                }
                return None;
            }
            Kind::List => {
                let head = self.c.nth(n, 0)?;
                if self.kind(head) != Kind::Symbol {
                    return None;
                }
                let nargs = self.c.children(n).len() as u32 - 1;
                if let Some(t) = self.ty_of_special(n, head, nargs) {
                    return t;
                }
                let spec = self.spec_fn_of(head)?;
                match &spec.arity(nargs)?.ret {
                    Some(Ret::Kw(kw)) if kw.0 != kid("any") => return Some(Ty::K(*kw)),
                    Some(Ret::Set(ks)) => return Some(Ty::U(ks.clone())),
                    _ => return None,
                }
            }
            _ => return None,
        }))
    }

    /// Tag stored on a local binding: `(+ tag-id 1)` with the nilable flag in bit 0; 0 = none.
    pub fn tag_code(kw: Kw) -> u16 {
        if kw.0 == OTHER {
            0
        } else {
            (((kw.0 as u16) + 1) << 1) | kw.1 as u16
        }
    }

    /// kondo `lint-arg-types!` for a call of a core fn; findings are queued with the usage index.
    pub fn lint_arg_types(&mut self, expr: NodeId, head: NodeId, nargs: u32) {
        if !self.lon || self.ctx.off & (uses::OFF_TYPE | uses::OFF_ARITY) != 0 || self.lc().level(FType::TypeMismatch) == OFF {
            return;
        }
        let Some(spec) = self.spec_fn_of(head) else { return };
        if self.opts.mova && self.kind(head) == Kind::Symbol && self.c.name(head).as_str() == "throw" {
            return; // Mova throws any value
        }
        let Some(ar) = spec.arity(nargs) else { return };
        if ar.args.is_empty() {
            return;
        }
        // cheap exit: nothing is checkable when no argument has a known tag
        let needed = ar.args.iter().take_while(|s| !matches!(s, S::Rest { .. })).count();
        if needed <= nargs as usize && !self.c.children(expr)[1..].iter().any(|&a| self.ty_of(a).is_some()) {
            return;
        }
        let args: Vec<NodeId> = self.c.children(expr)[1..].to_vec();
        let tags: Vec<Option<Ty>> = args.iter().map(|&a| self.ty_of(a)).collect();
        let mut finds: Vec<Finding> = Vec::new();
        let mut specs: std::collections::VecDeque<S> = ar.args.iter().cloned().collect();
        let mut rest: Option<S> = None;
        let mut last: Option<S> = None;
        let mut i = 0usize;
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 10_000 {
                break;
            }
            if i >= args.len() && specs.is_empty() {
                break;
            }
            match specs.pop_front() {
                Some(S::Rest { spec, last: l }) => {
                    rest = Some(*spec);
                    last = l.map(|b| *b);
                    specs.clear();
                }
                Some(S::Vec(v)) => {
                    for s in v.into_iter().rev() {
                        specs.push_front(s);
                    }
                }
                Some(S::Set(ks)) => {
                    let t = tags.get(i).cloned().flatten();
                    if let (Some(t), Some(&a)) = (&t, args.get(i)) {
                        if !ks.iter().any(|&e| match_ty(t, e)) {
                            let p = self.pos(self.c.unwrap_meta(a));
                            let mut f = Finding::new(FType::TypeMismatch, p, format!("Expected: {}, received: {}.", label_set(&ks), ty_label(t)));
                            f.lang = self.ltag;
                            finds.push(f);
                        }
                    }
                    i += 1;
                }
                Some(S::Kw(e)) => {
                    if i >= args.len() {
                        let p = self.pos(expr);
                        let p = args.last().map_or(p, |&a| self.pos(self.c.unwrap_meta(a)));
                        let mut f = Finding::new(FType::TypeMismatch, p, "Insufficient input.");
                        f.lang = self.ltag;
                        finds.push(f);
                        break;
                    }
                    if let Some(t) = tags[i].as_ref() {
                        if !match_ty(t, e) {
                            let p = self.pos(self.c.unwrap_meta(args[i]));
                            let mut f = Finding::new(FType::TypeMismatch, p, format!("Expected: {}, received: {}.", label(e), ty_label(t)));
                            f.lang = self.ltag;
                            finds.push(f);
                        }
                    }
                    i += 1;
                }
                None => match &rest {
                    Some(r) => {
                        let more = i + 1 < args.len();
                        let next = if more { r.clone() } else { last.clone().unwrap_or_else(|| r.clone()) };
                        if i >= args.len() {
                            break;
                        }
                        specs.push_front(next);
                    }
                    None => break,
                },
            }
        }
        if !finds.is_empty() {
            let idx = self.out.var_usages.len() as u32;
            for f in finds {
                self.out.lint_tfind.push((idx, f));
            }
        }
    }
}

impl<'a> Analyzer<'a> {
    /// Type hint (`^String x`) of a binding form as a tag code.
    pub fn hint_code(&self, orig: NodeId) -> Option<u16> {
        let mut cur = orig;
        let mut found = None;
        while let Some((m, t)) = self.c.meta(cur) {
            if self.kind(m) == Kind::Symbol {
                let nm = if self.c.ns(m).is_none() { self.c.name(m).as_str().to_owned() } else { self.node_str(m) };
                if let Some(kw) = tag_from_hint(&nm) {
                    found = Some(Self::tag_code(kw));
                }
            }
            cur = t;
        }
        found
    }

    /// Tag code of a `let` binding value.
    pub fn init_tag_code(&mut self, value: NodeId) -> u16 {
        match self.ty_of(value) {
            Some(Ty::K(kw)) => Self::tag_code(kw),
            Some(Ty::U(ks)) => {
                let i = match self.lt.utab.iter().position(|u| *u == ks) {
                    Some(i) => i,
                    None => {
                        self.lt.utab.push(ks);
                        self.lt.utab.len() - 1
                    }
                };
                if i + UNION_BASE as usize >= 250 {
                    return 0;
                }
                (((UNION_BASE as u16 + i as u16) + 1) << 1) as u16
            }
            _ => match self.rt_of(value) {
                Some(rt @ (super::rets::Rt::Call { .. } | super::rets::Rt::Map { .. })) => self.rt_tag_code(rt),
                _ => 0,
            },
        }
    }
}

impl<'a> Analyzer<'a> {
    /// kondo `analyze-clojure-string-replace`: the replacement must fit the match arg.
    pub fn lint_string_replace(&mut self, args: &[NodeId]) {
        if !self.lon || self.ctx.off & uses::OFF_TYPE != 0 || self.lc().level(FType::TypeMismatch) == OFF || args.len() < 3 {
            return;
        }
        let Some(Ty::K(m)) = self.ty_of(args[1]) else { return };
        let Some(Ty::K(r)) = self.ty_of(args[2]) else { return };
        let (mname, mnil) = (KNOWN.get(m.0 as usize).copied().unwrap_or(""), m.1);
        let _ = mnil;
        let nilable_string = Kw(kid("string"), true);
        let msg = match mname {
            "string" if !match_kw(r, nilable_string) => Some("String match arg requires string replacement arg"),
            "char" if !match_kw(r, k("char")) => Some("Char match arg requires char replacement arg"),
            "regex" if !(match_kw(r, nilable_string) || (match_kw(r, k("ifn")) && r.0 != kid("keyword"))) => Some("Regex match arg requires string or function replacement arg"),
            _ => None,
        };
        if let Some(m) = msg {
            let last = *args.last().unwrap();
            let p = self.pos(self.c.unwrap_meta(last));
            self.lint(FType::TypeMismatch, p, format!("{}, received: {}", m, label(r)));
        }
    }
}

impl<'a> Analyzer<'a> {
    fn queue_tfind(&mut self, f: Finding) {
        let idx = self.out.var_usages.len() as u32;
        self.out.lint_tfind.push((idx, f));
    }

    /// The other checks of kondo `lint-arg-types!` / `lint-specific-calls!` that depend on arg tags:
    /// redundant-primitive-coercion, not-empty?, is-message-not-string, missing-test-assertion.
    pub fn lint_arg_extras(&mut self, expr: NodeId, r: &crate::analyzer::resolve::Resolved, nargs: u32) {
        if !self.lon {
            return;
        }
        let s = crate::analyzer::syms();
        let core = r.ns == s.clojure_core || r.ns == s.cljs_core;
        let nm = names();
        let is_test_is = nargs == 2 && r.name == nm.is;
        let coercion = nargs == 1 && core && (r.name == nm.double || r.name == nm.long || r.name == nm.short || r.name == nm.byte || r.name == nm.char_ || r.name == nm.boolean);
        let empty_q = nargs == 1 && core && r.name == nm.empty_p;
        // missing-test-assertion needs a test frame directly above the call
        let test_ctx = core && matches!(self.cs.last(), Some(&(ns, n)) if matches!((ns.as_str(), n.as_str()), ("clojure.core" | "cljs.core", "let") | ("clojure.test" | "cljs.test", "testing" | "deftest")));
        if !(is_test_is || coercion || empty_q || test_ctx) {
            return;
        }
        let name = r.name.as_str();
        let p = self.pos(expr);
        let gen = self.lint_is_gen(expr);
        let _ = nm.not;
        if core && nargs == 1 {
            let expected = match name {
                "double" => Some("double"),
                "long" => Some("long"),
                "short" => Some("short"),
                "byte" => Some("byte"),
                "char" => Some("char"),
                "boolean" => Some("boolean"),
                _ => None,
            };
            if let (Some(e), false) = (expected, gen) {
                if self.lc().level(FType::RedundantPrimitiveCoercion) != OFF {
                    if let Some(a) = self.c.children(expr).get(1).copied() {
                        if let Some(Ty::K(t)) = self.ty_of(a) {
                            if t == k(e) {
                                let mut f = Finding::new(FType::RedundantPrimitiveCoercion, p, format!("Redundant {} coercion: expression already has type {}", name, e));
                                f.lang = self.ltag;
                                self.queue_tfind(f);
                            }
                        }
                    }
                }
            }
            if name == "empty?" && matches!(self.cs.last(), Some(&(ns, n)) if (ns == crate::analyzer::syms().clojure_core || ns == crate::analyzer::syms().cljs_core) && n.as_str() == "not") && self.lc().level(FType::NotEmpty) != OFF {
                if let Some(a) = self.c.children(expr).get(1).copied() {
                    if let Some(Ty::K(t)) = self.ty_of(a) {
                        let seq = k("seq");
                        if t == seq || (!t.1 && rel_has(&rel().is_a, t.0, seq.0)) {
                            let mut f = Finding::new(FType::NotEmpty, p, "Use (seq x) instead of (not (empty? x)) when x is a seq");
                            f.lang = self.ltag;
                            self.queue_tfind(f);
                        }
                    }
                }
            }
        }
        if nargs == 2 && name == "is" && matches!(r.ns.as_str(), "clojure.test" | "cljs.test") && !gen && self.lc().level(FType::IsMessageNotString) != OFF {
            if let Some(a) = self.c.children(expr).get(2).copied() {
                let a0 = self.c.unwrap_meta(a);
                let multi = self.kind(a0) == Kind::String && self.pos(a0).row != self.pos(a0).end_row;
                if !multi {
                    if let Some(t) = self.ty_of(a) {
                        if !match_ty(&t, k("string")) {
                            let mut f = Finding::new(FType::IsMessageNotString, self.pos(a0), "Test assertion message should be a string");
                            f.lang = self.ltag;
                            self.queue_tfind(f);
                        }
                    }
                }
            }
        }
        // missing-test-assertion: a pure core call used as a statement in a deftest body
        if core && self.lc().level(FType::MissingTestAssertion) != OFF && super::uval::unused_values().contains(&(if r.ns == crate::analyzer::syms().cljs_core { crate::analyzer::syms().clojure_core } else { r.ns }, r.name)) {
            let mut idx = if self.ctx.idx == u32::MAX { None } else { Some(self.ctx.idx) };
            let mut expected = false;
            for &(ns, nm) in self.cs.iter().rev() {
                let n = nm.as_str();
                match (ns.as_str(), n) {
                    ("clojure.core" | "cljs.core", "let") => idx = None,
                    ("clojure.test" | "cljs.test", "testing") => {
                        if idx == Some(0) {
                            break;
                        }
                        idx = None;
                    }
                    ("clojure.test" | "cljs.test", "deftest") => {
                        expected = true;
                        break;
                    }
                    _ => break,
                }
            }
            if expected {
                self.lint(FType::MissingTestAssertion, p, "missing test assertion");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_indices() {
        assert_eq!(kid("any"), K_ANY);
        assert_eq!(kid("nil"), K_NIL);
        assert_eq!(kid("seqable"), K_SEQABLE);
        assert_eq!(kid("boolean"), K_BOOLEAN);
    }
    #[test]
    fn set_order_matches_clojure() {
        // `(keyword 1)` message of clj-kondo: "symbol or string or keyword"
        let o = hash_set_order(&[k("symbol"), k("string"), k("keyword")]);
        assert_eq!(o.iter().map(|&x| label(x)).collect::<Vec<_>>().join(" or "), "symbol or string or keyword");
    }
}

struct Names {
    double: SymId,
    long: SymId,
    short: SymId,
    byte: SymId,
    char_: SymId,
    boolean: SymId,
    empty_p: SymId,
    not: SymId,
    is: SymId,
}

fn names() -> &'static Names {
    static N: OnceLock<Names> = OnceLock::new();
    N.get_or_init(|| Names { double: intern("double"), long: intern("long"), short: intern("short"), byte: intern("byte"), char_: intern("char"), boolean: intern("boolean"), empty_p: intern("empty?"), not: intern("not"), is: intern("is") })
}
