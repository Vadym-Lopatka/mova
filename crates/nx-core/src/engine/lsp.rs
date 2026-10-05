//! Findings -> LSP diagnostics exactly as JVM clojure-lsp (`kondo-finding->diagnostic`, default range-type :full).
use super::types::{Finding, Level};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
    /// 1 error, 2 warning, 3 info.
    pub severity: u8,
    pub code: String,
    pub source: &'static str,
    pub message: String,
    /// 1 = Unnecessary, 2 = Deprecated.
    pub tags: Vec<u8>,
}

const UNNECESSARY: [&str; 11] = [
    "clojure-lsp/unused-public-var",
    "redefined-var",
    "redundant-do",
    "redundant-expression",
    "redundant-let",
    "unused-binding",
    "unreachable-code",
    "unused-import",
    "unused-namespace",
    "unused-private-var",
    "unused-referred-var",
];

pub fn severity(l: Level) -> u8 {
    match l {
        Level::Error => 1,
        Level::Warning => 2,
        Level::Info => 3,
    }
}

/// One finding -> diagnostic. Range: 1-based row/col -> 0-based (min 0); multi-line finding collapses to its start.
/// (The `:range-type` != :full bracket-start collapse is not modelled: default is :full.)
pub fn to_diagnostic(f: &Finding) -> Diagnostic {
    let (er, ec) = if f.row != f.end_row { (f.row, f.col) } else { (f.end_row, f.end_col) };
    let z = |v: u32| v.saturating_sub(1);
    let mut tags = Vec::new();
    if UNNECESSARY.contains(&f.ty.as_str()) {
        tags.push(1);
    }
    if f.ty == "deprecated-var" {
        tags.push(2);
    }
    Diagnostic {
        line: z(f.row),
        character: z(f.col),
        end_line: z(er),
        end_character: z(ec),
        severity: severity(f.level),
        code: f.ty.clone(),
        source: if f.ty == "clojure-lsp/unused-public-var" { "clojure-lsp" } else { "clj-kondo" },
        message: f.message.clone(),
        tags,
    }
}

pub fn to_diagnostics(fs: &[Finding]) -> Vec<Diagnostic> {
    fs.iter().map(to_diagnostic).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiline_collapses() {
        let f = Finding { level: Level::Warning, ty: "unused-binding".into(), row: 3, col: 5, end_row: 4, end_col: 9, message: "m".into() };
        let d = to_diagnostic(&f);
        assert_eq!((d.line, d.character, d.end_line, d.end_character, d.severity, d.tags.clone()), (2, 4, 2, 4, 2, vec![1]));
    }
}
