//! A small EDN reader and printer: just what the EDN transport and the
//! `.nrepl.edn` config files need.
//!
//! The reader is incremental: `parse` says `Need` when the buffer ends inside a
//! value, so the transport can wait for more bytes. Tags (`#inst "..."`) are
//! dropped and the tagged value is returned. Floats are kept as text.

#[derive(Debug, Clone, PartialEq)]
pub enum Edn {
    Nil,
    Bool(bool),
    Int(i64),
    /// A number that is not an integer, as written.
    Float(String),
    Str(String),
    /// Without the leading colon.
    Kw(String),
    Sym(String),
    List(Vec<Edn>),
    Vec(Vec<Edn>),
    Map(Vec<(Edn, Edn)>),
    Set(Vec<Edn>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The buffer ends inside a value.
    Need,
    Bad(&'static str),
}

type R<T> = Result<T, Stop>;

const MAX_DEPTH: usize = 256;

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b',' | 0x0c)
}

fn is_delim(b: u8) -> bool {
    is_ws(b) || matches!(b, b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'"' | b';')
}

/// Skips white space, commas, comments and `#_` discards. Returns the new position.
fn skip(buf: &[u8], mut pos: usize, eof: bool, depth: usize) -> R<usize> {
    loop {
        match buf.get(pos) {
            Some(&b) if is_ws(b) => pos += 1,
            Some(b';') => {
                while buf.get(pos).is_some_and(|&b| b != b'\n') {
                    pos += 1;
                }
            }
            Some(b'#') if buf.get(pos + 1) == Some(&b'_') => {
                let (_, p) = value(buf, pos + 2, eof, depth + 1)?;
                pos = p;
            }
            Some(b'#') if buf.get(pos + 1).is_none() && !eof => return Err(Stop::Need),
            _ => return Ok(pos),
        }
    }
}

/// Parses one value at `pos`. With `eof` set a token that touches the end of the
/// buffer is complete (config files); otherwise it may continue (`Need`).
pub fn parse(buf: &[u8], pos: usize, eof: bool) -> R<(Edn, usize)> {
    value(buf, pos, eof, 0)
}

fn value(buf: &[u8], pos: usize, eof: bool, depth: usize) -> R<(Edn, usize)> {
    if depth > MAX_DEPTH {
        return Err(Stop::Bad("nested too deep"));
    }
    let pos = skip(buf, pos, eof, depth)?;
    let Some(&b) = buf.get(pos) else { return Err(Stop::Need) };
    match b {
        b'"' => string(buf, pos + 1).map(|(s, p)| (Edn::Str(s), p)),
        b'(' => seq(buf, pos + 1, b')', eof, depth).map(|(v, p)| (Edn::List(v), p)),
        b'[' => seq(buf, pos + 1, b']', eof, depth).map(|(v, p)| (Edn::Vec(v), p)),
        b'{' => {
            let (items, p) = seq(buf, pos + 1, b'}', eof, depth)?;
            if items.len() % 2 != 0 {
                return Err(Stop::Bad("map literal must contain an even number of forms"));
            }
            let mut it = items.into_iter();
            let mut m = Vec::new();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                m.push((k, v));
            }
            Ok((Edn::Map(m), p))
        }
        b')' | b']' | b'}' => Err(Stop::Bad("unmatched delimiter")),
        b'\\' => {
            // character literal: one char, then any letters/digits (\newline, A)
            let mut p = pos + 1;
            if p >= buf.len() {
                return Err(Stop::Need);
            }
            p += utf8_len(buf[p]);
            while buf.get(p).is_some_and(|&c| !is_delim(c)) {
                p += 1;
            }
            if p >= buf.len() && !eof {
                return Err(Stop::Need);
            }
            let s = String::from_utf8_lossy(&buf[pos + 1..p.min(buf.len())]).into_owned();
            Ok((Edn::Str(char_literal(&s)), p))
        }
        b'#' => match buf.get(pos + 1) {
            None => Err(Stop::Need),
            Some(b'{') => seq(buf, pos + 2, b'}', eof, depth).map(|(v, p)| (Edn::Set(v), p)),
            Some(_) => {
                // tagged value: skip the tag symbol, return the value
                let mut p = pos + 1;
                while buf.get(p).is_some_and(|&c| !is_delim(c)) {
                    p += 1;
                }
                if p >= buf.len() {
                    return Err(Stop::Need);
                }
                value(buf, p, eof, depth + 1)
            }
        },
        _ => {
            let mut p = pos;
            while buf.get(p).is_some_and(|&c| !is_delim(c)) {
                p += 1;
            }
            if p >= buf.len() && !eof {
                return Err(Stop::Need);
            }
            Ok((token(&String::from_utf8_lossy(&buf[pos..p])), p))
        }
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn char_literal(s: &str) -> String {
    match s {
        "newline" => "\n".into(),
        "space" => " ".into(),
        "tab" => "\t".into(),
        "return" => "\r".into(),
        "formfeed" => "\u{c}".into(),
        "backspace" => "\u{8}".into(),
        _ => match s.strip_prefix('u').and_then(|h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32) {
            Some(c) if s.len() == 5 => c.to_string(),
            _ => s.to_string(),
        },
    }
}

fn token(t: &str) -> Edn {
    match t {
        "nil" => return Edn::Nil,
        "true" => return Edn::Bool(true),
        "false" => return Edn::Bool(false),
        _ => {}
    }
    if let Some(k) = t.strip_prefix(':') {
        return Edn::Kw(k.to_string());
    }
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.as_bytes().first().is_some_and(|c| c.is_ascii_digit()) {
        let n = t.strip_suffix('N').unwrap_or(t);
        return match n.parse::<i64>() {
            Ok(i) => Edn::Int(i),
            Err(_) => Edn::Float(t.to_string()),
        };
    }
    Edn::Sym(t.to_string())
}

fn seq(buf: &[u8], mut pos: usize, close: u8, eof: bool, depth: usize) -> R<(Vec<Edn>, usize)> {
    let mut items = Vec::new();
    loop {
        pos = skip(buf, pos, eof, depth)?;
        match buf.get(pos) {
            None => return Err(Stop::Need),
            Some(&c) if c == close => return Ok((items, pos + 1)),
            Some(_) => {
                let (v, p) = value(buf, pos, eof, depth + 1)?;
                items.push(v);
                pos = p;
            }
        }
    }
}

fn string(buf: &[u8], mut pos: usize) -> R<(String, usize)> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        match buf.get(pos) {
            None => return Err(Stop::Need),
            Some(b'"') => return Ok((String::from_utf8_lossy(&out).into_owned(), pos + 1)),
            Some(b'\\') => {
                let Some(&e) = buf.get(pos + 1) else { return Err(Stop::Need) };
                pos += 2;
                match e {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'u' => {
                        if buf.len() < pos + 4 {
                            return Err(Stop::Need);
                        }
                        let h = std::str::from_utf8(&buf[pos..pos + 4]).ok();
                        let Some(c) = h.and_then(|h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32) else {
                            return Err(Stop::Bad("bad \\u escape"));
                        };
                        let mut tmp = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                        pos += 4;
                    }
                    _ => return Err(Stop::Bad("unsupported escape character")),
                }
            }
            Some(&b) => {
                out.push(b);
                pos += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// printing (strings only; the transport prints its own maps)
// ---------------------------------------------------------------------------

/// Writes `s` as an EDN string literal, as `pr-str` does.
pub fn write_string(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    // the text is UTF-8 on the wire; replace bad bytes like a JVM decoder would
    let text = String::from_utf8_lossy(s);
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\t' => b"\\t",
            b'\r' => b"\\r",
            8 => b"\\b",
            12 => b"\\f",
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        out.extend_from_slice(esc);
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Edn {
        parse(s.as_bytes(), 0, true).unwrap().0
    }

    #[test]
    fn reads_a_message() {
        let v = p(r#"{:op "eval" :code "(+ 1 2)\n" :id 7, :n nil :x #{:a}}"#);
        let Edn::Map(m) = v else { panic!() };
        assert_eq!(m[0], (Edn::Kw("op".into()), Edn::Str("eval".into())));
        assert_eq!(m[1].1, Edn::Str("(+ 1 2)\n".into()));
        assert_eq!(m[2].1, Edn::Int(7));
        assert_eq!(m[3].1, Edn::Nil);
        assert_eq!(m[4].1, Edn::Set(vec![Edn::Kw("a".into())]));
    }

    #[test]
    fn incremental() {
        assert_eq!(parse(b"{:op \"ev", 0, false), Err(Stop::Need));
        assert_eq!(parse(b"{:op \"eval\"", 0, false), Err(Stop::Need));
        assert_eq!(parse(b"{:op \"eval\"} ", 0, false).unwrap().1, 12);
        assert_eq!(parse(b"{:a 12", 0, false), Err(Stop::Need));
        assert!(matches!(parse(b"}", 0, false), Err(Stop::Bad(_))));
    }

    #[test]
    fn discards_comments_and_tags() {
        assert_eq!(p("; hi\n#_ {:a 1} #inst \"2020\""), Edn::Str("2020".into()));
        assert_eq!(p("[1 #_2 3]"), Edn::Vec(vec![Edn::Int(1), Edn::Int(3)]));
    }

    #[test]
    fn escapes() {
        let mut o = Vec::new();
        write_string(&mut o, b"a\"b\\c\nd\xc3\xa9");
        assert_eq!(String::from_utf8(o).unwrap(), "\"a\\\"b\\\\c\\nd\u{e9}\"");
        assert_eq!(p(r#""A\t""#), Edn::Str("A\t".into()));
    }
}
