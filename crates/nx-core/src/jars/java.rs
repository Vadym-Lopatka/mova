//! kondo-like `java-class-definitions`: class name + flags from `.class` headers and `.java` sources.

/// Flag bits of kondo's class flags (sorted alphabetically when emitted).
pub const FL_ABSTRACT: u8 = 1;
pub const FL_FINAL: u8 = 2;
pub const FL_INTERFACE: u8 = 4;
pub const FL_PRIVATE: u8 = 8;
pub const FL_PROTECTED: u8 = 16;
pub const FL_PUBLIC: u8 = 32;
pub const FL_STATIC: u8 = 64;

const NAMES: [(u8, &str); 7] = [(FL_ABSTRACT, "abstract"), (FL_FINAL, "final"), (FL_INTERFACE, "interface"), (FL_PRIVATE, "private"), (FL_PROTECTED, "protected"), (FL_PUBLIC, "public"), (FL_STATIC, "static")];

/// Flag names in emit (alphabetical) order.
pub fn flag_names(f: u8) -> impl Iterator<Item = &'static str> {
    NAMES.iter().filter(move |(b, _)| f & b != 0).map(|x| x.1)
}

/// What kondo-like class filtering needs from a `.class` file.
#[derive(Clone, Debug)]
pub struct ClassInfo {
    /// Binary name with dots.
    pub name: String,
    /// Reported flags (`FL_*`): header access bits public, final, interface (abstract is not reported for .class).
    pub flags: u8,
    /// `SourceFile` attribute.
    pub source_file: Option<String>,
    /// Binary name (dots) of the `EnclosingMethod` class: set for anonymous and local classes.
    pub enclosing: Option<String>,
}

/// Parse a whole `.class` file (constant pool, skipping members, then class attributes).
pub fn class_info(b: &[u8]) -> Option<ClassInfo> {
    if b.len() < 10 || b[..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }
    let u16_at = |p: usize| -> Option<usize> { Some(u16::from_be_bytes([*b.get(p)?, *b.get(p + 1)?]) as usize) };
    let n = u16_at(8)?;
    let mut off = vec![0u32; n.max(1)];
    let mut p = 10usize;
    let mut i = 1;
    while i < n {
        off[i] = p as u32;
        p += match *b.get(p)? {
            1 => 3 + u16_at(p + 1)?,
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => 5,
            5 | 6 => {
                i += 1;
                9
            }
            7 | 8 | 16 | 19 | 20 => 3,
            15 => 4,
            _ => return None,
        };
        i += 1;
    }
    let utf = |i: usize| -> Option<String> {
        let u = *off.get(i)? as usize;
        let len = u16_at(u + 1)?;
        Some(String::from_utf8_lossy(b.get(u + 3..u + 3 + len)?).into_owned())
    };
    let class_name = |i: usize| -> Option<String> { Some(utf(u16_at(*off.get(i)? as usize + 1)?)?.replace('/', ".")) };
    let acc = u16_at(p)?;
    let name = class_name(u16_at(p + 2)?)?;
    p += 6;
    p += 2 + 2 * u16_at(p)?; // interfaces
    for _ in 0..2 {
        // fields, methods
        let cnt = u16_at(p)?;
        p += 2;
        for _ in 0..cnt {
            p += 6;
            let ac = u16_at(p)?;
            p += 2;
            for _ in 0..ac {
                let len = u32::from_be_bytes(b.get(p + 2..p + 6)?.try_into().ok()?) as usize;
                p += 6 + len;
            }
        }
    }
    let ac = u16_at(p)?;
    p += 2;
    let (mut source_file, mut enclosing) = (None, None);
    for _ in 0..ac {
        let an = utf(u16_at(p)?)?;
        let len = u32::from_be_bytes(b.get(p + 2..p + 6)?.try_into().ok()?) as usize;
        match an.as_str() {
            "SourceFile" => source_file = utf(u16_at(p + 6)?),
            "EnclosingMethod" => enclosing = class_name(u16_at(p + 6)?),
            _ => {}
        }
        p += 6 + len;
    }
    let mut flags = 0;
    if acc & 0x1 != 0 {
        flags |= FL_PUBLIC;
    }
    if acc & 0x10 != 0 {
        flags |= FL_FINAL;
    }
    if acc & 0x200 != 0 {
        flags |= FL_INTERFACE;
    }
    Some(ClassInfo { name, flags, source_file, enclosing })
}

/// kondo reports a class unless it is Clojure-compiled fn/init code or an anonymous/local class whose
/// enclosing class is top-level (or itself dropped). Derived empirically from the JVM goldens.
pub fn keep_class(c: &ClassInfo) -> bool {
    let gen = match c.source_file.as_deref() {
        Some(f) => f.ends_with(".clj") || f.ends_with(".cljc") || f.ends_with(".cljs"),
        None => c.name.ends_with("__init") || c.name.contains("proxy$"), // no debug info: only clojure init/proxy classes
    };
    if gen {
        return !c.name.contains('$') && !c.name.ends_with("__init");
    }
    if c.name.split_once('$').map_or(false, |(_, rest)| rest.contains('_') || rest.split('$').any(|seg| seg.starts_with(|ch: char| ch.is_lowercase()))) {
        return false; // nested names with `_` or a lowercase start look clojure-munged: not reported
    }
    // anonymous/local classes (`Outer$1`, `Outer$1Local`) directly inside a top-level class are not reported,
    // nor is anything nested in them
    let mut name = c.name.as_str();
    while let Some((parent, last)) = name.rsplit_once('$') {
        if last.starts_with(|ch: char| ch.is_ascii_digit()) && !parent.contains('$') {
            return false;
        }
        name = parent;
    }
    true
}

#[derive(PartialEq)]
enum Tok<'a> {
    Id(&'a str),
    P(u8),
}

fn tokens(s: &str) -> Vec<Tok<'_>> {
    let b = s.as_bytes();
    let mut v = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c == b'"' && b[i..].starts_with(b"\"\"\"") {
            i += 3;
            while i < b.len() && !b[i..].starts_with(b"\"\"\"") {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 3;
        } else if c == b'"' || c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != c {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 {
            let st = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            v.push(Tok::Id(&s[st..i]));
        } else {
            v.push(Tok::P(c));
            i += 1;
        }
    }
    v
}

/// Type declarations of a Java source: `(binary class name, flags)`; nested types as `Outer$Inner`.
/// Flags are the explicit modifiers (abstract final private protected public static).
pub fn source_classes(src: &str) -> Vec<(String, u8)> {
    let t = tokens(src);
    let mut pkg = String::new();
    let mut out = Vec::new();
    // frames: Some(name) = type body, None = other block
    let mut stack: Vec<Option<String>> = Vec::new();
    let mut mods = 0u8;
    let mut pending: Option<(String, u8)> = None;
    let mut i = 0;
    while i < t.len() {
        match &t[i] {
            Tok::Id("package") if stack.is_empty() && pkg.is_empty() => {
                let mut j = i + 1;
                while j < t.len() && t[j] != Tok::P(b';') {
                    match &t[j] {
                        Tok::Id(x) => pkg.push_str(x),
                        Tok::P(b'.') => pkg.push('.'),
                        _ => {}
                    }
                    j += 1;
                }
                i = j;
            }
            Tok::Id(w @ ("class" | "interface" | "enum" | "record")) if matches!(stack.last(), None | Some(Some(_))) && (i == 0 || t[i - 1] != Tok::P(b'.')) => {
                if let Some(Tok::Id(name)) = t.get(i + 1) {
                    let is_decl = *w != "record" || matches!(t.get(i + 2), Some(Tok::P(b'(')) | Some(Tok::P(b'<')));
                    if is_decl {
                        let outer = stack.iter().rev().find_map(|f| f.clone());
                        let full = match &outer {
                            Some(o) => format!("{o}${name}"),
                            None if pkg.is_empty() => name.to_string(),
                            None => format!("{pkg}.{name}"),
                        };
                        let fl = mods | if *w == "interface" { FL_INTERFACE } else { 0 };
                        out.push((full.clone(), fl));
                        pending = Some((full, fl));
                        mods = 0;
                    }
                }
            }
            Tok::Id("abstract") => mods |= FL_ABSTRACT,
            Tok::Id("final") => mods |= FL_FINAL,
            Tok::Id("private") => mods |= FL_PRIVATE,
            Tok::Id("protected") => mods |= FL_PROTECTED,
            Tok::Id("public") => mods |= FL_PUBLIC,
            Tok::Id("static") => mods |= FL_STATIC,
            Tok::P(b'{') => {
                stack.push(pending.take().map(|p| p.0));
                mods = 0;
            }
            Tok::P(b'}') => {
                stack.pop();
                mods = 0;
            }
            Tok::P(b';') => mods = 0,
            _ => {}
        }
        i += 1;
    }
    out
}
