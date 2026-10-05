//! Does the JavaParser used by clj-kondo (default language level, ~Java 11) fail on this source?
//! On any parse problem kondo's AST carries no comments, so no member has a `:doc`.

#[derive(PartialEq, Clone, Copy)]
enum T<'a> {
    Id(&'a str),
    P(u8),
    Arrow,
    Str,
}

fn lex(src: &str) -> Option<Vec<T<'_>>> {
    let b = src.as_bytes();
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
        } else if c == b'"' {
            if b[i..].starts_with(b"\"\"\"") {
                return None; // text block
            }
            i += 1;
            while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
            v.push(T::Str);
        } else if c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != b'\'' && b[i] != b'\n' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
            v.push(T::Str);
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            v.push(T::Id(&src[s..i]));
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            v.push(T::Str);
        } else if c == b'-' && b.get(i + 1) == Some(&b'>') {
            v.push(T::Arrow);
            i += 2;
        } else {
            v.push(T::P(c));
            i += 1;
        }
    }
    Some(v)
}

/// True when JavaParser (language level < 14) hits a real syntax error that drops all comments from the AST.
/// Validator-only problems (switch expressions, `instanceof` patterns, records) keep the AST and its comments;
/// among JDK sources only `yield` statements (an identifier for the old grammar) cause one.
pub fn fails(src: &str) -> bool {
    if !src.contains("yield") {
        return false;
    }
    let Some(t) = lex(src) else { return false };
    (1..t.len()).any(|i| {
        matches!(t[i], T::Id("yield"))
            && matches!(t[i - 1], T::P(b';') | T::P(b'{') | T::P(b'}') | T::P(b':') | T::Arrow)
            && !matches!(t.get(i + 1), Some(T::P(b'=')) | Some(T::P(b'.')))
            // `yield name;`, `yield -1;`, `yield new X()` still parse (declaration / expression statement)
            && match t.get(i + 1) {
                Some(T::Str) => true,
                Some(T::Id(x)) => match *x {
                    "null" | "true" | "false" | "this" | "super" => true,
                    "switch" => false,
                    "new" => true,
                    _ => !matches!(t.get(i + 2), Some(T::P(b';')) | Some(T::P(b'=')) | Some(T::P(b',')) | Some(T::P(b'['))),
                },
                Some(T::P(b'(')) => true,
                _ => false,
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets() {
        assert!(fails("class A { int f(int x){ return switch(x){ case 1: yield 2; default: yield 3; }; } }"));
        assert!(!fails("class A { int f(int x){ return switch(x){ case 1 -> 2; default -> 3; }; } }"));
        assert!(!fails("class A { boolean f(Object o){ return o instanceof String s; } }"));
    }

    /// `JP_TRUTH=truth.tsv cargo test jp_truth -- --ignored --nocapture` (path\tok|lost|problem-but-comments)
    #[test]
    #[ignore]
    fn jp_truth() {
        let Ok(p) = std::env::var("JP_TRUTH") else { return };
        let (mut bad, mut n) = (0, 0);
        for l in std::fs::read_to_string(p).unwrap().lines() {
            let (f, r) = l.split_once('\t').unwrap();
            let Ok(s) = std::fs::read_to_string(f) else { continue };
            n += 1;
            if fails(&s) != (r == "lost") {
                bad += 1;
                if bad <= 40 {
                    println!("MISMATCH {r} {f}");
                }
            }
        }
        println!("{bad} mismatches / {n}");
    }
}
