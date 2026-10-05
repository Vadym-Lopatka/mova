//! textDocument/codeAction (feature/code_actions.clj): the action list at a position, byte-identical to clojure-lsp.
pub mod clauses;
pub mod create_function;
pub mod create_test;
pub mod clean_ns;
pub mod exec;
pub mod extract;
pub mod inline;
pub mod info;
pub mod listx;
pub mod move_form;
pub mod paredit;
pub mod libspec;
pub mod preds;
pub mod promote;
pub mod refactors;
pub mod resolve_macro;
pub mod rz;
pub mod sugg;
pub mod settings;
pub mod thread_get;
pub mod transform;
pub mod zops;
pub mod tree;

use crate::analyzer::json::{parse as jparse, Json};
use crate::query::{json_str, Q};
use tree::*;

/// Text of a document / dependency entry / disk file by uri.
pub fn read_uri_text(uri: &str) -> Option<String> {
    let dec = |s: &str| {
        let b = s.as_bytes();
        let mut o = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
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
        let (j, e) = r.split_once("!/")?;
        (dec(j), dec(e))
    } else if let Some(r) = uri.strip_prefix("zipfile://") {
        let (j, e) = r.split_once("::")?;
        (dec(j), dec(e))
    } else {
        let p = crate::engine::scan::uri_to_path(uri)?;
        return std::fs::read_to_string(p).ok();
    };
    let mut j = crate::io::jar::Jar::open(&jar).ok()?;
    let mut buf = Vec::new();
    j.read(&entry, &mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// One JSON argument of a command.
#[derive(Clone, Debug)]
pub enum A {
    S(String),
    N(i64),
    Nil,
    Arr(Vec<A>),
}

pub struct Act {
    pub title: String,
    pub kind: &'static str,
    pub preferred: bool,
    pub ctitle: String,
    pub cmd: &'static str,
    pub args: Vec<A>,
}

impl Act {
    pub fn new(title: &str, kind: &'static str, cmd: &'static str, args: Vec<A>) -> Act {
        Act { title: title.to_string(), kind, preferred: false, ctitle: title.to_string(), cmd, args }
    }
    fn json(&self) -> String {
        let mut s = format!("{{\"title\":{},\"kind\":\"{}\"", json_str(&self.title), self.kind);
        if self.preferred {
            s.push_str(",\"isPreferred\":true");
        }
        s.push_str(&format!(",\"command\":{{\"title\":{},\"command\":\"{}\",\"arguments\":[", json_str(&self.ctitle), self.cmd));
        for (i, a) in self.args.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            match a {
                A::S(v) => s.push_str(&json_str(v)),
                A::N(n) => s.push_str(&n.to_string()),
                A::Nil => s.push_str("null"),
                A::Arr(v) => {
                    s.push('[');
                    for (j, x) in v.iter().enumerate() {
                        if j > 0 {
                            s.push(',');
                        }
                        if let A::S(t) = x {
                            s.push_str(&json_str(t));
                        }
                    }
                    s.push(']');
                }
            }
        }
        s.push_str("]}}");
        s
    }
}

pub struct Diag {
    pub code: String,
    pub message: String,
    pub line: u32,
    pub ch: u32,
    pub refers: Vec<String>,
}

pub struct Req {
    pub uri: String,
    pub line: u32,
    pub ch: u32,
    pub end: Option<(u32, u32)>,
    pub diags: Vec<Diag>,
    pub ws_edit: bool,
}

fn num(j: Option<&Json>) -> Option<u32> {
    j.and_then(|x| x.as_f64()).map(|n| n as u32)
}

pub fn parse_req(params: &str) -> Option<Req> {
    let j = jparse(params)?;
    let uri = j.get("uri")?.as_str()?.to_string();
    let s = j.get("start")?.as_arr()?;
    let end = j.get("end").and_then(|e| e.as_arr()).and_then(|e| Some((num(e.first())?, num(e.get(1))?)));
    let mut diags = Vec::new();
    for d in j.get("diags").and_then(|d| d.as_arr()).unwrap_or(&[]) {
        let refers = d
            .get("refers")
            .and_then(|r| r.as_arr())
            .map(|r| r.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        diags.push(Diag {
            code: d.get("code").and_then(|c| c.as_str()).unwrap_or("").to_string(),
            message: d.get("message").and_then(|c| c.as_str()).unwrap_or("").to_string(),
            line: num(d.get("line")).unwrap_or(0),
            ch: num(d.get("ch")).unwrap_or(0),
            refers,
        });
    }
    let ws_edit = matches!(j.get("wsEdit"), Some(Json::Bool(true)));
    Some(Req { uri, line: num(s.first())?, ch: num(s.get(1))?, end, diags, ws_edit })
}

/// `textDocument/codeAction` answer (JSON array text). `params` = `{uri,start:[l,c],end:[l,c],diags:[{code,message,line,ch,refers}],wsEdit}`.
pub fn code_action(q: &Q, params: &str) -> String {
    let Some(req) = parse_req(params) else { return "[]".into() };
    let acts = actions(q, &req);
    let mut s = String::from("[");
    for (i, a) in acts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&a.json());
    }
    s.push(']');
    s
}

fn uri_arg(r: &Req) -> A {
    A::S(r.uri.clone())
}

pub fn actions(q: &Q, r: &Req) -> Vec<Act> {
    let Some(f) = q.s.id(&r.uri) else { return vec![] };
    let Some(text) = preds::file_text(q, f) else { return vec![] };
    let tree = Tree::parse(&text);
    if tree.err {
        // clojure-lsp: no zipper (nil root) -> only the workspace-edit actions that need no form
        let mut out = Vec::new();
        if r.ws_edit {
            let mut seen: Vec<String> = Vec::new();
            for d in &r.diags {
                let title = format!("Suppress '{}' diagnostic", d.code);
                if !seen.contains(&title) {
                    seen.push(title.clone());
                    out.push(Act::new(&title, "quickfix", "suppress-diagnostic", vec![A::S(r.uri.clone()), A::N(d.line as i64), A::N(d.ch as i64), A::S(d.code.clone())]).ct("Suppress diagnostic"));
                }
            }
            out.push(Act::new("Clean namespace", "source.organizeImports", "clean-ns", vec![A::S(r.uri.clone()), A::N(r.line as i64), A::N(r.ch as i64)]));
        }
        return out;
    }
    let (row, col) = (r.line + 1, r.ch + 1);
    let (line, ch) = (r.line as i64, r.ch as i64);
    let zo = find_at_pos(&tree, row, col);
    let u = || uri_arg(r);
    let pos3 = || vec![u(), A::N(line), A::N(ch)];
    let with = |extra: A| vec![u(), A::N(line), A::N(ch), extra];
    let mut out: Vec<Act> = Vec::new();

    // --- predicates (order of code_actions.clj/all) ---
    let on = |f: &dyn Fn(Z) -> bool| zo.map_or(false, |z| f(z));
    let inline_symbol = zo.is_some() && preds::inline_symbol(q, &r.uri, row, col);
    let other_colls = zo.map_or(vec![], preds::find_other_colls);
    let can_add_let = on(&preds::can_add_let);
    let can_move_to_for_let = on(&preds::can_move_to_let_kw);
    let inside_fn = on(&|z| preds::find_function_form(z).is_some());
    let promote = zo.and_then(preds::can_promote_fn);
    let demote = on(&preds::can_demote_fn);
    let inline_fn = on(&|z| preds::can_inline_fn(q, &r.uri, &tree, z));
    let destructure = on(&|z| preds::can_destructure_keys(q, &r.uri, &tree, z));
    let restructure = {
        let root = rz::Loc::of_node(rz::from_tree(&tree));
        zops::find_at_pos(&root, row, col).map_or(false, |l| refactors::can_restructure_loc(q, &r.uri, &l))
    };
    let extract_def = on(&preds::can_extract_to_def);
    let thread = on(&preds::can_thread);
    let unwind = on(&preds::can_unwind_thread);
    let get_more = on(&preds::can_get_in_more);
    let get_less = on(&preds::can_get_in_less);
    let near_if = on(&|z| preds::near(z, "if"));
    let near_cond = on(&|z| preds::near(z, "cond"));
    let refer_as = r.ws_edit && on(&preds::can_refer_to_as);
    let as_refer = r.ws_edit && on(&preds::can_as_to_refer);
    let sort_ctx = if r.ws_edit { zo.and_then(|z| clauses::can_sort(q, &r.uri, z)) } else { None };
    let drag_b = r.ws_edit && on(&|z| clauses::can_drag(q, &r.uri, &tree, z, false));
    let drag_f = r.ws_edit && on(&|z| clauses::can_drag(q, &r.uri, &tree, z, true));
    let dragp_b = r.ws_edit && on(&|z| clauses::can_drag_param(q, &r.uri, &tree, z, false));
    let dragp_f = r.ws_edit && on(&|z| clauses::can_drag_param(q, &r.uri, &tree, z, true));

    listx::diag_actions(q, r, &tree, zo, &mut out);
    if let Some(m) = zo.and_then(|z| macro_sym(q, &r.uri, z)) {
        out.push(Act::new(&format!("Resolve macro '{m}' as..."), "quickfix", "resolve-macro-as", pos3()).ct("Resolve macro as..."));
    }
    if inline_symbol {
        out.push(Act::new("Inline symbol", "refactor.inline", "inline-symbol", pos3()));
    }
    for c in &other_colls {
        out.push(Act::new(&format!("Change coll to {c}"), "refactor", "change-coll", with(A::S(c.to_string()))).ct("Change coll"));
    }
    if let Some(st) = zo.and_then(|z| preds::cycle_kw_status(&tree, z)) {
        let t = if st == preds::KwStatus::AutoToNs { "Change auto-resolved keyword to namespaced" } else { "Change namespaced keyword to auto-resolved" };
        out.push(Act::new(t, "refactor.rewrite", "cycle-keyword-auto-resolve", pos3()));
    }
    if let Some(st) = zo.and_then(preds::cycle_nsmap_status) {
        let t = if st == preds::NsMapStatus::MapToNs { "Change map to namespaced map" } else { "Change namespaced map to map" };
        out.push(Act::new(t, "refactor.rewrite", "cycle-namespaced-map", pos3()));
    }
    if can_add_let {
        out.push(Act::new("Move to let", "refactor.extract", "move-to-let", with(A::S("new-binding".into()))));
        let (el, ec) = match r.end {
            Some((l, c)) => (A::N(l as i64), A::N(c as i64)),
            None => (A::Nil, A::Nil),
        };
        out.push(Act::new(
            "Extract function",
            "refactor.extract",
            "extract-function",
            vec![u(), A::N(line), A::N(ch), A::S("new-function".into()), el, ec],
        ));
    }
    if can_move_to_for_let {
        out.push(Act::new("Move to :let", "refactor.extract", "move-to-for-let", with(A::S("new-binding".into()))));
    }
    if inside_fn {
        out.push(Act::new("Cycle privacy", "refactor.rewrite", "cycle-privacy", pos3()));
    }
    if let Some(p) = promote {
        out.push(Act::new(&format!("Promote {p}"), "refactor.rewrite", "promote-fn", with(A::Nil)));
    }
    if demote {
        out.push(Act::new("Demote fn to #()", "refactor.rewrite", "demote-fn", pos3()));
    }
    if inline_fn {
        out.push(Act::new("Inline function", "refactor.inline", "inline-function", pos3()));
    }
    if destructure {
        out.push(Act::new("Destructure keys", "refactor.rewrite", "destructure-keys", pos3()));
    }
    if restructure {
        out.push(Act::new("Restructure keys", "refactor.rewrite", "restructure-keys", pos3()));
    }
    if extract_def {
        out.push(Act::new("Extract to def", "refactor.extract", "extract-to-def", with(A::Nil)));
    }
    if thread {
        out.push(Act::new("Thread first all", "refactor.rewrite", "thread-first-all", pos3()));
        out.push(Act::new("Thread last all", "refactor.rewrite", "thread-last-all", pos3()));
    }
    if unwind {
        out.push(Act::new("Unwind thread once", "refactor.rewrite", "unwind-thread", pos3()));
        out.push(Act::new("Unwind whole thread", "refactor.rewrite", "unwind-all", pos3()));
    }
    if get_more {
        out.push(Act::new("Move another expression to get/get-in", "refactor.rewrite", "get-in-more", pos3()));
        out.push(Act::new("Move all expressions to get/get-in", "refactor.rewrite", "get-in-all", pos3()));
    }
    if get_less {
        out.push(Act::new("Remove one element from get/get-in", "refactor.rewrite", "get-in-less", pos3()));
        out.push(Act::new("Unwind whole get/get-in", "refactor.rewrite", "get-in-none", pos3()));
    }
    if r.ws_edit {
        if let Some(c) = sort_ctx {
            let t = match c {
                "map" => "Sort map",
                "vector" => "Sort vector",
                "set" => "Sort set",
                "list" => "Sort list",
                _ => "Sort clauses",
            };
            out.push(Act::new(t, "refactor.rewrite", "sort-clauses", pos3()));
        }
        if drag_b {
            out.push(Act::new("Drag backward", "refactor.rewrite", "drag-backward", pos3()));
        }
        if drag_f {
            out.push(Act::new("Drag forward", "refactor.rewrite", "drag-forward", pos3()));
        }
        if dragp_b {
            out.push(Act::new("Drag param backward", "refactor.rewrite", "drag-param-backward", pos3()));
        }
        if dragp_f {
            out.push(Act::new("Drag param forward", "refactor.rewrite", "drag-param-forward", pos3()));
        }
    }
    if can_add_let {
        out.push(Act::new("Introduce let", "refactor.extract", "introduce-let", with(A::S("new-binding".into()))));
    }
    if r.ws_edit {
        // (suppress and create-test come before clean-ns; create-test needs source paths: not native yet)
        let mut seen: Vec<String> = Vec::new();
        for d in &r.diags {
            let title = format!("Suppress '{}' diagnostic", d.code);
            if seen.contains(&title) {
                continue;
            }
            seen.push(title.clone());
            out.push(Act::new(&title, "quickfix", "suppress-diagnostic", vec![u(), A::N(d.line as i64), A::N(d.ch as i64), A::S(d.code.clone())]).ct("Suppress diagnostic"));
        }
    }
    if near_if {
        out.push(Act::new("Change nested if to cond", "refactor.extract", "if->cond-refactor", pos3()));
    }
    if near_cond {
        out.push(Act::new("Change cond to nested if", "refactor.extract", "cond->if-refactor", pos3()));
    }
    if r.ws_edit {
        if let Some(n) = zo.and_then(|z| create_test::can_create_test(q, &r.uri, z)) {
            out.push(Act::new(&format!("Create test for '{n}'"), "refactor.rewrite", "create-test", pos3()).ct("Create test"));
        }
    }
    if r.ws_edit {
        out.push(Act::new("Clean namespace", "source.organizeImports", "clean-ns", pos3()));
        if refer_as {
            out.push(Act::new("Replace refer with as", "refactor.rewrite", "refer-to-as", pos3()));
        }
        if as_refer {
            out.push(Act::new("Replace as with refer", "refactor.rewrite", "as-to-refer", pos3()));
        }
    }
    out
}

impl Act {
    pub fn ct(mut self, t: &str) -> Act {
        self.ctitle = t.to_string();
        self
    }
}

const EXCLUDED_ALL: [&str; 4] = ["clojure.core", "clojure.core.async", "cljs.core.async", "cljs.core.async.macros"];

/// `find-full-macro-symbol-to-resolve`.
pub fn macro_sym(q: &Q, uri: &str, z: Z) -> Option<String> {
    let name = preds::find_function_usage_name_loc(z)?;
    let m = name.meta();
    let e = q.first_under_cursor(uri, m.row, m.col)?;
    if e.b != crate::engine::index::B::VarUsage {
        return None;
    }
    let u = &q.fa(e.f).var_usages[e.i as usize];
    if !u.macro_ {
        return None;
    }
    let to = u.to.as_str();
    let nm = u.name.as_str();
    if EXCLUDED_ALL.contains(&to) {
        return None;
    }
    // other entries of `excluded-macros` are vectors: `(contains? [..] sym)` tests indexes, so they never exclude
    Some(format!("{to}/{nm}"))
}
