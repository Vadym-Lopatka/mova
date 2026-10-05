//! Tiny JSON reader (used to load jar goldens / oracle files). Not a general-purpose parser:
//! numbers are f64, objects keep insertion order, no surrogate-pair handling beyond `\uXXXX`.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(v) => v.iter().find(|(a, _)| a == k).map(|(_, b)| b),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
}

impl Json {
    /// Compact serialization (numbers that are integral print without a fraction).
    pub fn write(&self, out: &mut String) {
        use std::fmt::Write;
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => {
                let _ = write!(out, "{}", *n as i64);
            }
            Json::Num(n) => {
                let _ = write!(out, "{n}");
            }
            Json::Str(s) => out.push_str(&super::expr::json_str(s)),
            Json::Arr(v) => {
                out.push('[');
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    x.write(out);
                }
                out.push(']');
            }
            Json::Obj(v) => {
                out.push('{');
                for (i, (k, x)) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&super::expr::json_str(k));
                    out.push(':');
                    x.write(out);
                }
                out.push('}');
            }
        }
    }
}

pub fn parse(s: &str) -> Option<Json> {
    let b = s.as_bytes();
    let mut i = 0;
    let v = val(b, &mut i)?;
    Some(v)
}

fn ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\n' | b'\r' | b'\t') {
        *i += 1;
    }
}

fn val(b: &[u8], i: &mut usize) -> Option<Json> {
    ws(b, i);
    match *b.get(*i)? {
        b'{' => {
            *i += 1;
            let mut v = Vec::new();
            ws(b, i);
            if b[*i] == b'}' {
                *i += 1;
                return Some(Json::Obj(v));
            }
            loop {
                ws(b, i);
                let k = string(b, i)?;
                ws(b, i);
                if b[*i] != b':' {
                    return None;
                }
                *i += 1;
                let x = val(b, i)?;
                v.push((k, x));
                ws(b, i);
                match b[*i] {
                    b',' => *i += 1,
                    b'}' => {
                        *i += 1;
                        return Some(Json::Obj(v));
                    }
                    _ => return None,
                }
            }
        }
        b'[' => {
            *i += 1;
            let mut v = Vec::new();
            ws(b, i);
            if b[*i] == b']' {
                *i += 1;
                return Some(Json::Arr(v));
            }
            loop {
                v.push(val(b, i)?);
                ws(b, i);
                match b[*i] {
                    b',' => *i += 1,
                    b']' => {
                        *i += 1;
                        return Some(Json::Arr(v));
                    }
                    _ => return None,
                }
            }
        }
        b'"' => Some(Json::Str(string(b, i)?)),
        b't' => {
            *i += 4;
            Some(Json::Bool(true))
        }
        b'f' => {
            *i += 5;
            Some(Json::Bool(false))
        }
        b'n' => {
            *i += 4;
            Some(Json::Null)
        }
        _ => {
            let st = *i;
            while *i < b.len() && matches!(b[*i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                *i += 1;
            }
            std::str::from_utf8(&b[st..*i]).ok()?.parse().ok().map(Json::Num)
        }
    }
}

fn string(b: &[u8], i: &mut usize) -> Option<String> {
    if b[*i] != b'"' {
        return None;
    }
    *i += 1;
    let mut out: Vec<u8> = Vec::new();
    while *i < b.len() {
        match b[*i] {
            b'"' => {
                *i += 1;
                return String::from_utf8(out).ok();
            }
            b'\\' => {
                *i += 1;
                match b[*i] {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'u' => {
                        let h = std::str::from_utf8(&b[*i + 1..*i + 5]).ok()?;
                        let c = char::from_u32(u32::from_str_radix(h, 16).ok()?).unwrap_or('\u{fffd}');
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        *i += 4;
                    }
                    c => out.push(c),
                }
                *i += 1;
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
    None
}
