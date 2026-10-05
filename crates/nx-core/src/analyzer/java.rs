//! kondo `java/reg-class-def!`: class definitions of `.class` files (constant pool + access flags, what ASM reports)
//! and `.java` sources (type declarations and their modifiers, what JavaParser reports). Members are not produced
//! (`java-member-definitions` is off in clojure-lsp's analysis config).
use super::types::*;
use crate::intern::intern;

/// `.class` bytes -> class name and flags (public/static/final/interface only, as kondo's `opcode->flags`).
pub fn class_def_from_class(b: &[u8]) -> Option<JavaClassDef> {
    let u16_at = |i: usize| -> Option<usize> { Some(((*b.get(i)? as usize) << 8) | *b.get(i + 1)? as usize) };
    if b.get(0..4)? != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }
    let n = u16_at(8)?;
    let mut off = 10;
    // (tag, start) per constant index
    let mut pool: Vec<(u8, usize)> = vec![(0, 0); n.max(1)];
    let mut i = 1;
    while i < n {
        let tag = *b.get(off)?;
        pool[i] = (tag, off + 1);
        let adv = match tag {
            1 => 3 + u16_at(off + 1)?,
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => 5,
            5 | 6 => 9,
            7 | 8 | 16 | 19 | 20 => 3,
            15 => 4,
            _ => return None,
        };
        off += adv;
        i += if tag == 5 || tag == 6 { 2 } else { 1 };
    }
    let access = u16_at(off)?;
    let this = u16_at(off + 2)?;
    let (t, at) = *pool.get(this)?;
    if t != 7 {
        return None;
    }
    let name_idx = u16_at(at)?;
    let (t, at) = *pool.get(name_idx)?;
    if t != 1 {
        return None;
    }
    let len = u16_at(at)?;
    let raw = b.get(at + 2..at + 2 + len)?;
    let name = String::from_utf8_lossy(raw).replace('/', ".");
    let mut flags = 0u16;
    if access & 0x1 != 0 {
        flags |= jf_bit("public");
    }
    if access & 0x8 != 0 {
        flags |= jf_bit("static");
    }
    if access & 0x10 != 0 {
        flags |= jf_bit("final");
    }
    if access & 0x200 != 0 {
        flags |= jf_bit("interface");
    }
    Some(JavaClassDef { class: intern(&name), flags })
}

#[derive(Clone, Copy, PartialEq)]
enum T<'a> {
    Id(&'a str),
    P(u8),
}

fn tokenize(s: &str) -> Vec<T<'_>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'"' => {
                if b.get(i + 1) == Some(&b'"') && b.get(i + 2) == Some(&b'"') {
                    i += 3;
                    while i + 2 < b.len() && !(b[i] == b'"' && b[i + 1] == b'"' && b[i + 2] == b'"') {
                        if b[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    i += 3;
                } else {
                    i += 1;
                    while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                        if b[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 => {
                let st = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                    i += 1;
                }
                out.push(T::Id(&s[st..i]));
            }
            _ if c.is_ascii_digit() => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                    i += 1;
                }
            }
            _ => {
                out.push(T::P(c));
                i += 1;
            }
        }
    }
    out
}

fn modifier_bit(s: &str) -> u16 {
    match s {
        "public" | "protected" | "private" | "abstract" | "static" | "final" | "transient" | "volatile" | "synchronized" | "native" | "strictfp" | "default" | "sealed" => jf_bit(s),
        _ => 0,
    }
}

/// `.java` source -> class definitions (nested types as `pkg.Outer$Inner`).
pub fn class_defs_from_source(src: &str) -> Vec<JavaClassDef> {
    let t = tokenize(src);
    let mut out = Vec::new();
    // package
    let mut pkg = String::new();
    let mut i = 0;
    while i + 1 < t.len() {
        if t[i] == T::Id("package") {
            let mut j = i + 1;
            while j < t.len() && t[j] != T::P(b';') {
                match t[j] {
                    T::Id(s) => pkg.push_str(s),
                    T::P(b'.') => pkg.push('.'),
                    _ => {}
                }
                j += 1;
            }
            break;
        }
        if t[i] == T::Id("import") || matches!(t[i], T::Id(s) if matches!(s, "class" | "interface" | "enum")) {
            break;
        }
        i += 1;
    }
    // frames: Some(name) = type body, None = other braces
    let mut frames: Vec<Option<String>> = Vec::new();
    let mut mods = 0u16;
    let mut i = 0;
    while i < t.len() {
        match t[i] {
            T::P(b';') => mods = 0,
            T::P(b'}') => {
                frames.pop();
                mods = 0;
            }
            T::P(b'{') => {
                frames.push(None);
                mods = 0;
            }
            T::P(b'@') => {
                if t.get(i + 1) == Some(&T::Id("interface")) {
                    // annotation type: a TypeDeclaration without modifiers of ClassOrInterfaceDeclaration flavor
                    if let Some(&T::Id(name)) = t.get(i + 2) {
                        i = open_type(&t, i + 3, name, mods, false, &pkg, &mut frames, &mut out);
                        mods = 0;
                        continue;
                    }
                } else {
                    // annotation: skip name and optional argument list
                    i += 1;
                    while matches!(t.get(i), Some(T::Id(_))) || t.get(i) == Some(&T::P(b'.')) {
                        i += 1;
                    }
                    if t.get(i) == Some(&T::P(b'(')) {
                        let mut d = 0;
                        while i < t.len() {
                            match t[i] {
                                T::P(b'(') => d += 1,
                                T::P(b')') => {
                                    d -= 1;
                                    if d == 0 {
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            i += 1;
                        }
                        i += 1;
                    }
                    continue;
                }
            }
            T::Id(id) => {
                let b = modifier_bit(id);
                if b != 0 {
                    mods |= b;
                } else if id == "non" && t.get(i + 1) == Some(&T::P(b'-')) && t.get(i + 2) == Some(&T::Id("sealed")) {
                    mods |= jf_bit("non-sealed");
                    i += 3;
                    continue;
                } else if i > 0 && t[i - 1] == T::P(b'.') {
                    // `Foo.class` literal
                } else if matches!(id, "class" | "interface" | "enum") || (id == "record" && matches!(t.get(i + 2), Some(T::P(b'(')) | Some(T::P(b'<')))) {
                    if let Some(&T::Id(name)) = t.get(i + 1) {
                        let is_iface = id == "interface";
                        i = open_type(&t, i + 2, name, mods, is_iface, &pkg, &mut frames, &mut out);
                        mods = 0;
                        continue;
                    }
                } else {
                    mods = 0;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Skip a type header from `i` to its `{` and push the frame; registers the type when it is not local.
#[allow(clippy::too_many_arguments)]
fn open_type(t: &[T], mut i: usize, name: &str, mods: u16, iface: bool, pkg: &str, frames: &mut Vec<Option<String>>, out: &mut Vec<JavaClassDef>) -> usize {
    let mut paren = 0;
    while i < t.len() {
        match t[i] {
            T::P(b'(') => paren += 1,
            T::P(b')') => paren -= 1,
            T::P(b'{') if paren <= 0 => break,
            T::P(b';') if paren <= 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    let local = frames.iter().any(|f| f.is_none());
    if local {
        frames.push(None);
        return i + 1;
    }
    let outer: Vec<&str> = frames.iter().flatten().map(|s| s.as_str()).collect();
    let mut full = String::new();
    if !pkg.is_empty() {
        full.push_str(pkg);
        full.push('.');
    }
    for o in &outer {
        full.push_str(o);
        full.push('$');
    }
    full.push_str(name);
    let flags = mods | if iface { jf_bit("interface") } else { 0 };
    out.push(JavaClassDef { class: intern(&full), flags });
    frames.push(Some(name.to_owned()));
    i + 1
}
