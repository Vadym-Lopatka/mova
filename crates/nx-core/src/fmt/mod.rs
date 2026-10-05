//! Native cljfmt 0.16.4 (default pipeline) with clojure-lsp's formatting / range-formatting shapes.
//!
//! Supported options: indentation, remove-consecutive-blank-lines, remove-surrounding-whitespace,
//! insert-missing-whitespace, remove-multiple-non-indenting-spaces, remove-trailing-whitespace,
//! indent-line-comments, function-arguments-indentation, normalize-newlines-at-file-end, indents (+ extra).
//! Not implemented (off by default in cljfmt): alignment, sort-ns-references, split-keypairs,
//! remove-blank-lines-in-forms.

pub mod config;
#[cfg(test)]
mod cases;
mod parse;
mod passes;
pub mod tree;

pub use config::{FmtConfig, FnArgIndent, Key, Part, Rule, Spec};
pub use parse::ParseErr;

use passes::Ctx;
use tree::{Tag, Tree};

/// LSP-style text edit (0-based line / UTF-16 character).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub new_text: String,
}

/// cljfmt `reformat-form` on the tree rooted at `root`.
fn reformat(t: &mut Tree, root: u32, cfg: &FmtConfig) -> Result<(), ParseErr> {
    let cr = cfg.compiled();
    let mut ctx = Ctx::default();
    if cfg.indentation && cr.needs_ctx {
        let (mut a, mut r) = passes::alias_refer_maps(t, root);
        a.extend(cfg.alias_map.iter().map(|(k, v)| (k.clone(), v.clone())));
        r.extend(cfg.refer_map.iter().map(|(k, v)| (k.clone(), v.clone())));
        ctx.alias = a;
        ctx.refer = r;
        ctx.ns_name = passes::find_namespace(t, root);
    }
    // rewrite-clj `z/of-node`: every pass starts at the first non-whitespace/comment top-level node
    let start = t.down_sig(root).unwrap_or(root);
    if cfg.remove_consecutive_blank_lines {
        passes::remove_consecutive_blank_lines(t, start);
    }
    if cfg.remove_surrounding_whitespace {
        passes::remove_surrounding_whitespace(t, start);
    }
    if cfg.insert_missing_whitespace {
        passes::insert_missing_whitespace(t, start);
    }
    if cfg.remove_multiple_non_indenting_spaces {
        passes::remove_multiple_non_indenting_spaces(t, start);
    }
    if cfg.indentation {
        passes::unindent(t, start, cfg);
        let ind = passes::Indenter { cfg, cr, ctx: &ctx, failed: Default::default() };
        passes::indent(t, root, start, &ind);
        if ind.failed.get() {
            return Err(ParseErr("regex indent key applied to a symbol without namespace".into()));
        }
    }
    if cfg.remove_trailing_whitespace {
        passes::remove_trailing_whitespace(t, start);
    }
    Ok(())
}

fn trimr_java(s: &str) -> &str {
    s.trim_end_matches(|c: char| parse::java_ws(c))
}

/// `reformat-string` on LF-only text.
fn reformat_string(src: &str, cfg: &FmtConfig) -> Result<String, ParseErr> {
    let src = &*parse::normalize_reader_newlines(src);
    let mut t = parse::parse(src)?;
    reformat(&mut t, 0, cfg)?;
    let mut out = String::with_capacity(src.len() + 16);
    t.render(0, &mut out);
    if cfg.normalize_newlines_at_file_end {
        let blank = out.chars().all(parse::java_ws);
        let mut s = trimr_java(&out).to_string();
        if !blank {
            s.push('\n');
        }
        return Ok(s);
    }
    Ok(out)
}

/// clojure-lsp `formatting`: `wrap-normalize-newlines` + `reformat-string`. Err on parse errors.
pub fn try_format(src: &str, cfg: &FmtConfig) -> Result<String, ParseErr> {
    let crlf = match src.find('\n') {
        Some(i) => i > 0 && src.as_bytes()[i - 1] == b'\r',
        None => false,
    };
    if src.contains("\r\n") {
        let norm = src.replace("\r\n", "\n");
        let out = reformat_string(&norm, cfg)?;
        return Ok(if crlf { out.replace('\n', "\r\n") } else { out });
    }
    reformat_string(src, cfg)
}

/// Formatted text; the source unchanged when it does not parse.
pub fn format(src: &str, cfg: &FmtConfig) -> String {
    try_format(src, cfg).unwrap_or_else(|_| src.to_string())
}

/// `textDocument/formatting` result: empty when unchanged, else one whole-document edit.
pub fn format_edits(src: &str, cfg: &FmtConfig) -> Vec<TextEdit> {
    match try_format(src, cfg) {
        Ok(new) if new != src => vec![TextEdit { start_line: 0, start_col: 0, end_line: 999_999, end_col: 999_999, new_text: new }],
        _ => vec![],
    }
}

/// Top-level node spans with 1-based (row, utf16 col) start / exclusive end.
struct Span {
    id: u32,
    sr: u32,
    sc: u32,
    er: u32,
    ec: u32,
}

fn top_spans(t: &Tree) -> Vec<Span> {
    let mut out = Vec::new();
    let (mut row, mut col) = (1u32, 1u32);
    let mut buf = String::new();
    let mut x = t.down(0);
    while let Some(id) = x {
        buf.clear();
        t.render(id, &mut buf);
        let (sr, sc) = (row, col);
        for ch in buf.chars() {
            if ch == '\n' {
                row += 1;
                col = 1;
            } else {
                col += ch.len_utf16() as u32;
            }
        }
        out.push(Span { id, sr, sc, er: row, ec: col });
        x = t.right(id);
    }
    out
}

fn find_at(spans: &[Span], from: usize, row: u32, col: u32) -> Option<usize> {
    (from..spans.len()).find(|&i| {
        let s = &spans[i];
        (row, col) >= (s.sr, s.sc) && (row, col) < (s.er, s.ec)
    })
}

/// clojure-lsp `range-formatting` with 1-based row/col positions (as in the handler's `format-pos`).
pub fn format_range_pos(src: &str, row: u32, col: u32, end_row: u32, end_col: u32, cfg: &FmtConfig) -> Vec<TextEdit> {
    let src = &*parse::normalize_reader_newlines(src);
    let Ok(mut t) = parse::parse(src) else { return vec![] };
    let spans = top_spans(&t);
    let Some(si) = find_at(&spans, 0, row, col) else { return vec![] };
    let ei = find_at(&spans, si, end_row, end_col).unwrap_or(spans.len() - 1);
    let kids: Vec<u32> = spans[si..=ei].iter().map(|s| s.id).collect();
    let (s, e) = (&spans[si], &spans[ei]);
    let root2 = t.add(Tag::Forms, 0, 0);
    t.set_children(root2, &kids);
    if reformat(&mut t, root2, cfg).is_err() {
        return vec![];
    }
    let mut out = String::new();
    t.render(root2, &mut out);
    vec![TextEdit { start_line: s.sr - 1, start_col: s.sc - 1, end_line: e.er - 1, end_col: e.ec - 1, new_text: out }]
}

/// Range formatting for whole lines `start_line..=end_line` (0-based): from column 1 of the first line to the
/// last character of the last line, so exactly the top-level forms touching those lines are reformatted.
pub fn format_range(src: &str, start_line: u32, end_line: u32, cfg: &FmtConfig) -> Vec<TextEdit> {
    let end_len = src
        .split('\n')
        .nth(end_line as usize)
        .map(|l| l.encode_utf16().count() as u32)
        .unwrap_or(0);
    format_range_pos(src, start_line + 1, 1, end_line + 1, end_len.max(1), cfg)
}

#[cfg(test)]
mod tests;
