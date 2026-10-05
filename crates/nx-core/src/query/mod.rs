//! Native LSP feature answers over an immutable `Snapshot`, each returning the JVM clojure-lsp wire JSON.
//! Element model: `El {file, bucket, idx}`; accessors mirror the kondo element maps after clojure-lsp normalization
//! (see nx/FEATURES.md section 0). New buckets (keywords, symbols, protocol-impls, java-*) plug into `B` + the
//! accessors below.
pub mod alias_edit;
pub mod callh;
pub mod complete;
pub mod def;
pub mod diag;
pub mod format;
pub mod hover;
pub mod impls;
pub mod jdk;
pub mod decompile;
pub mod jorder;
pub mod lens;
pub mod ranges;
pub mod refs;
pub mod rename;
pub mod semtok;
pub mod sighelp;
pub mod snippets;
pub mod special_forms;
pub mod symbols;
pub mod text;
pub mod wsym;

use crate::analyzer::*;
use crate::cst::Pos;
use crate::engine::index::{PosIdx, B};
use crate::engine::store::{FileId, Snapshot};
use crate::engine::types::FileEntry;
use crate::intern::SymId;
use std::fmt::Write;

pub const CLJ: u8 = 1;
pub const CLJS: u8 = 2;

/// One analysis element.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct El {
    pub f: FileId,
    pub b: B,
    pub i: u32,
}

/// Request position, 0-based LSP.
#[derive(Clone, Copy, Debug)]
pub struct At<'a> {
    pub uri: &'a str,
    pub line: u32,
    pub ch: u32,
}

impl At<'_> {
    pub fn row(&self) -> u32 {
        self.line + 1
    }
    pub fn col(&self) -> u32 {
        self.ch + 1
    }
}

pub struct Q<'a> {
    pub s: &'a Snapshot,
}

pub(crate) fn json_str(s: &str) -> String {
    crate::analyzer::expr::json_str(s)
}

pub fn pos_json(out: &mut String, row: u32, col: u32) {
    let _ = write!(out, "{{\"line\":{},\"character\":{}}}", row.saturating_sub(1), col.saturating_sub(1));
}

pub fn range_json(p: Pos) -> String {
    let mut s = String::from("{\"start\":");
    pos_json(&mut s, p.row, p.col);
    s.push_str(",\"end\":");
    pos_json(&mut s, p.end_row, p.end_col);
    s.push('}');
    s
}

impl<'a> Q<'a> {
    pub fn new(s: &'a Snapshot) -> Q<'a> {
        Q { s }
    }
    pub fn entry(&self, f: FileId) -> &'a FileEntry {
        self.s.file(f).expect("file id")
    }
    pub fn fa(&self, f: FileId) -> &'a FileAnalysis {
        self.entry(f).fa().expect("analysis")
    }
    /// Indices (ascending) of the var definitions `ns/name` of file `f`: a scan of the bucket, no position index.
    pub fn var_def_idx(&self, f: FileId, ns: SymId, name: SymId) -> Vec<u32> {
        let fa = self.fa(f);
        fa.var_definitions.iter().enumerate().filter(|(_, d)| !d.name.is_none() && d.ns == ns && d.name == name).map(|(i, _)| i as u32).collect()
    }
    /// Indices (ascending) of the keyword elements `ns/name` of file `f` that are definitions, or usages in internal files.
    pub fn kw_idx(&self, f: FileId, ns: SymId, name: SymId) -> Vec<u32> {
        let fa = self.fa(f);
        let internal = fa.has_callstack;
        fa.keywords.iter().enumerate().filter(|(_, k)| k.ns == ns && k.name == name && (!k.reg.is_none() || internal)).map(|(i, _)| i as u32).collect()
    }
    pub fn pos_idx(&self, f: FileId) -> &'a PosIdx {
        self.entry(f).pos_idx().expect("pos idx")
    }
    pub fn uri(&self, f: FileId) -> &'a str {
        &self.entry(f).uri
    }
    pub fn internal(&self, f: FileId) -> bool {
        self.entry(f).internal
    }

    /// Name range (`name-row..name-end-col`) of the element.
    pub fn name_pos(&self, e: El) -> Pos {
        let fa = self.fa(e.f);
        let i = e.i as usize;
        match e.b {
            B::NsDef => fa.namespace_definitions[i].name_pos,
            B::NsUsage => fa.namespace_usages[i].name_pos,
            B::NsAlias => fa.namespace_usages[i].alias_pos,
            B::VarDef => fa.var_definitions[i].name_pos,
            B::VarUsage => fa.var_usages[i].name_pos,
            B::Local => fa.locals[i].pos,
            B::LocalUsage => {
                let l = &fa.local_usages[i];
                if l.name_pos.row != 0 { l.name_pos } else { l.pos }
            }
            B::KwDef | B::KwUsage => fa.keywords[i].pos,
            B::Symbols => fa.symbols[i].pos,
            B::ProtoImpl => fa.protocol_impls[i].name_pos,
            B::JavaClassUsage => {
                let u = &fa.java_class_usages[i];
                if u.flags & JU_HAS_NAME != 0 && u.name_pos.row != 0 { u.name_pos } else { u.pos }
            }
            B::InstInv => fa.instance_invocations[i].name_pos,
            B::JavaClassDef | B::JavaMemberDef => Pos { row: 0, col: 0, end_row: 0, end_col: 0 },
        }
    }

    /// `row/col/end-row/end-col` of the element (whole form for definitions/usages).
    pub fn form_pos(&self, e: El) -> Pos {
        let fa = self.fa(e.f);
        let i = e.i as usize;
        match e.b {
            B::NsDef => fa.namespace_definitions[i].pos,
            B::NsUsage | B::NsAlias => {
                let p = fa.namespace_usages[i].name_pos;
                Pos { row: p.row, col: p.col, end_row: 0, end_col: 0 }
            }
            B::VarDef => fa.var_definitions[i].pos,
            B::VarUsage => fa.var_usages[i].pos,
            B::Local => fa.locals[i].pos,
            B::LocalUsage => fa.local_usages[i].pos,
            B::ProtoImpl => fa.protocol_impls[i].pos,
            B::JavaClassUsage => fa.java_class_usages[i].pos,
            _ => self.name_pos(e),
        }
    }

    /// `:name` of the element.
    pub fn name(&self, e: El) -> SymId {
        let fa = self.fa(e.f);
        let i = e.i as usize;
        match e.b {
            B::NsDef => fa.namespace_definitions[i].name,
            B::NsUsage => fa.namespace_usages[i].to,
            // the alias copy of a namespace usage has no `:name` (clojure-lsp renames `:to` on the original only)
            B::NsAlias => SymId::NONE,
            B::VarDef => fa.var_definitions[i].name,
            B::VarUsage => fa.var_usages[i].name,
            B::Local => fa.locals[i].name,
            B::LocalUsage => fa.local_usages[i].name,
            B::KwDef | B::KwUsage => fa.keywords[i].name,
            B::Symbols => fa.symbols[i].name,
            _ => SymId::NONE,
        }
    }

    /// Element `:lang` tag (0 when absent).
    pub fn el_lang(&self, e: El) -> u8 {
        let fa = self.fa(e.f);
        let i = e.i as usize;
        match e.b {
            B::NsDef => fa.namespace_definitions[i].lang,
            B::NsUsage | B::NsAlias => fa.namespace_usages[i].lang,
            B::VarDef => fa.var_definitions[i].lang,
            B::VarUsage => fa.var_usages[i].lang,
            B::Local => fa.locals[i].lang,
            B::LocalUsage => fa.local_usages[i].lang,
            B::KwDef | B::KwUsage => fa.keywords[i].lang,
            B::Symbols => match fa.symbols[i].lang {
                3 => L_CLJ, // edn symbols are looked up as clj
                l => l,
            },
            B::InstInv => fa.instance_invocations[i].lang,
            B::JavaClassUsage => if fa.java_class_usages[i].flags & JU_CLJS != 0 { L_CLJS } else { L_CLJ },
            _ => 0,
        }
    }

    /// clojure-lsp `elem-langs` as a bit mask.
    pub fn langs(&self, e: El) -> u8 {
        match self.el_lang(e) {
            L_CLJ => CLJ,
            L_CLJS => CLJS,
            _ => file_langs(self.uri(e.f)),
        }
    }

    /// Elements under the cursor in bucket-iteration order.
    pub fn under_cursor(&self, uri: &str, row: u32, col: u32) -> Vec<El> {
        let Some(f) = self.s.id(uri) else { return Vec::new() };
        let Some(pi) = self.entry(f).pos_idx() else { return Vec::new() };
        let mut out: Vec<El> = pi.at(row, col).into_iter().map(|e| El { f, b: e.b, i: e.i }).collect();
        // usages made from `unresolved-namespace` findings sit at the tail of `var_usages` (not in the position index)
        if let Some(fa) = self.entry(f).fa() {
            for (i, u) in fa.var_usages.iter().enumerate().rev().take_while(|(_, u)| u.synth) {
                if u.pos.row == row && u.pos.col <= col && col <= u.pos.end_col {
                    out.push(El { f, b: B::VarUsage, i: i as u32 });
                }
            }
        }
        out
    }

    pub fn first_under_cursor(&self, uri: &str, row: u32, col: u32) -> Option<El> {
        self.under_cursor(uri, row, col).into_iter().next()
    }

    /// LSP Location of an element (`->range` of its name).
    pub fn location(&self, e: El) -> String {
        format!("{{\"uri\":{},\"range\":{}}}", json_str(self.uri(e.f)), range_json(self.name_pos(e)))
    }
}

/// `uri->available-langs` as a mask.
pub fn file_langs(uri: &str) -> u8 {
    if uri.ends_with(".cljs") {
        CLJS
    } else if uri.ends_with(".cljc") {
        CLJ | CLJS
    } else {
        CLJ
    }
}

/// Request entry point: method name without the `textDocument/` prefix. None = not answered natively.
/// `completionItem/resolve` of the client's item JSON.
pub fn resolve_completion(s: &Snapshot, item: &str) -> String {
    complete::resolve_item(s, item)
}

pub fn answer(s: &Snapshot, method: &str, at: At, include_declaration: bool) -> Option<String> {
    answer_x(s, method, at, include_declaration, "")
}

/// `answer` with the method's extra string argument (rename `newName`, workspace/symbol `query`).
pub fn answer_x(s: &Snapshot, method: &str, at: At, include_declaration: bool, extra: &str) -> Option<String> {
    let q = Q::new(s);
    match method {
        "definition" => Some(def::definition(&q, at)),
        "declaration" => Some(def::declaration(&q, at)),
        "references" => Some(refs::references(&q, at, include_declaration)),
        "documentHighlight" => Some(refs::highlight(&q, at)),
        "hover" => Some(hover::hover(&q, at)),
        "documentSymbol" => Some(symbols::document_symbol(&q, at.uri)),
        "completion" => Some(complete::completion(&q, at)),
        "implementation" => Some(impls::implementation(&q, at)),
        "prepareCallHierarchy" => Some(callh::prepare(&q, at)),
        "callHierarchyIncoming" => Some(callh::incoming(&q, at)),
        "callHierarchyOutgoing" => Some(callh::outgoing(&q, at)),
        "workspaceSymbol" => Some(wsym::workspace_symbol(&q, extra)),
        "signatureHelp" => Some(sighelp::signature_help(&q, at)),
        "prepareRename" => Some(rename::prepare_rename(&q, at)),
        "rename" => Some(rename::rename(&q, at, extra)),
        "dependencyContents" => Some(dependency_contents(at.uri)),
        "semanticTokensFull" => Some(semtok::full(&q, at.uri)),
        "foldingRange" => Some(ranges::folding(&q, at.uri)),
        "selectionRange" => Some(ranges::selection(&q, at)),
        "linkedEditingRange" => Some(ranges::linked_editing(&q, at)),
        "codeLens" => Some(lens::code_lens(&q, at.uri)),
        "cursorInfo" => Some(crate::actions::info::cursor_info_json(&q, at.uri, at.row(), at.col())),
        _ => None,
    }
}

/// Requests with extra integer arguments (`nums`): semanticTokensRange [start line, end line], codeLensResolve [row col sl sc el ec].
pub fn answer_n(s: &Snapshot, method: &str, uri: &str, nums: &[i64]) -> Option<String> {
    let q = Q::new(s);
    let n = |i: usize| nums.get(i).copied().unwrap_or(0).max(0) as u32;
    match method {
        "semanticTokensRange" => Some(semtok::range(&q, uri, n(0), n(1))),
        "codeLensResolve" => Some(lens::resolve(&q, uri, nums)),
        "formatting" => Some(format::formatting(&q, uri)),
        "rangeFormatting" => Some(format::range_formatting(&q, uri, nums)),
        _ => None,
    }
}

/// Requests carrying a JSON parameter string (codeAction): method, params JSON.
pub fn answer_j(s: &Snapshot, method: &str, json: &str) -> Option<String> {
    let q = Q::new(s);
    match method {
        "codeAction" => Some(crate::actions::code_action(&q, json)),
        "executeCommand" => Some(crate::actions::exec::exec_command(&q, json)),
        "serverInfo" => {
            let init = crate::analyzer::json::parse(json);
            Some(crate::actions::info::server_info_json(&q, init.as_ref()))
        }
        "clojuredocs" => Some(crate::actions::info::clojuredocs_raw(json)),
        "projectTree" => Some(crate::actions::info::project_tree(&q, json)),
        _ => None,
    }
}

/// `clojure/dependencyContents`: text of a dependency entry (`jar:file:///x.jar!/a/b.clj` or `zipfile:///x.jar::a/b.clj`).
pub fn dependency_contents(uri: &str) -> String {
    let dec = |s: &str| {
        let b = s.as_bytes();
        let mut o = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() + 0 {
                if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    o.push(v);
                    i += 3;
                    continue;
                }
            }
            o.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&o).into_owned()
    };
    let (jar, entry) = if let Some(r) = uri.strip_prefix("jar:file://") {
        match r.split_once("!/") {
            Some((j, e)) => (dec(j), dec(e)),
            None => return "null".into(),
        }
    } else if let Some(r) = uri.strip_prefix("zipfile://") {
        match r.split_once("::") {
            Some((j, e)) => (dec(j), dec(e)),
            None => return "null".into(),
        }
    } else if let Some(p) = crate::engine::scan::uri_to_path(uri) {
        return std::fs::read_to_string(p).map(|t| json_str(&t)).unwrap_or_else(|_| "null".into());
    } else {
        return "null".into();
    };
    let Ok(mut j) = crate::io::jar::Jar::open(&jar) else { return "null".into() };
    let mut buf = Vec::new();
    if j.read(&entry, &mut buf).is_err() {
        return "null".into();
    }
    json_str(&String::from_utf8_lossy(&buf))
}
