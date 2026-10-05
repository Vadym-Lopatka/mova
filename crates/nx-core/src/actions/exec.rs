//! workspace/executeCommand (feature/command.clj): runs a refactoring and returns the WorkspaceEdit / showDocument request.
use super::preds::file_text;
use super::rz::{from_tree, is_printable_only, Loc};
use super::transform as t;
use super::tree::{Meta, Tag, Tree};
use super::zops::find_at_pos;
use crate::analyzer::json::{parse as jparse, Json};
use crate::query::{json_str, Q};

pub struct Edit {
    pub range: Option<Meta>,
    pub text: String,
}

pub struct ResourceChange {
    pub uri: String,
}

pub struct Show {
    pub uri: String,
    pub range: Option<Meta>,
}

/// Command outcome (mirrors the result shapes `call-command` dispatches on).
pub enum Out {
    /// A seq of edits on the command's own file.
    Seq(Vec<Edit>),
    /// `:changes-by-uri` / `:resource-changes` / `:show-document-after-edit`.
    Map { changes: Vec<(String, Vec<Edit>)>, resources: Vec<ResourceChange>, show: Option<Show> },
    NoOp,
    Err(String, i64),
    /// nil result: "Could not apply command."
    Nil,
    /// Write a file (clj-kondo config) and answer null.
    Write { path: String, content: String },
    /// nil result plus an error `window/showMessage` for the user.
    NilMsg(String),
    /// nil result plus an info `window/showMessage` (server-info / cursor-info).
    Info(String),
    /// A question for the user (`window/showMessageRequest`); the command is re-run with the answer appended.
    Ask { message: String, actions: Vec<String> },
}

pub struct Caps {
    pub doc_changes: bool,
    pub resource_ops: bool,
    pub annotations: bool,
}

pub struct Settings {
    pub use_metadata_for_privacy: bool,
    pub keep_parens: bool,
}

pub struct Ctx<'a> {
    pub q: &'a Q<'a>,
    pub uri: String,
    pub line: i64,
    pub ch: i64,
    pub row: u32,
    pub col: u32,
    pub end_row: u32,
    pub end_col: u32,
    pub args: Vec<Json>,
    pub settings: Settings,
    pub version: i64,
    pub answers: Vec<Option<String>>,
    pub init: Option<Json>,
    pub text: Option<std::sync::Arc<str>>,
    pub root: Option<Loc>,
    pub loc: Option<Loc>,
    pub loc_end: Option<Loc>,
}

fn range_json(m: Meta) -> String {
    format!(
        "{{\"start\":{{\"line\":{},\"character\":{}}},\"end\":{{\"line\":{},\"character\":{}}}}}",
        m.row.saturating_sub(1).max(0),
        m.col.saturating_sub(1).max(0),
        m.end_row.saturating_sub(1).max(0),
        m.end_col.saturating_sub(1).max(0)
    )
}

fn edits_json(es: &[Edit]) -> String {
    let mut s = String::from("[");
    for (i, e) in es.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let r = e.range.map(range_json).unwrap_or_else(|| "null".to_string());
        s.push_str(&format!("{{\"range\":{},\"newText\":{}}}", r, json_str(&e.text)));
    }
    s.push(']');
    s
}

fn from_ze(v: Vec<t::ZE>) -> Vec<Edit> {
    v.into_iter().map(|z| Edit { range: z.range, text: z.text }).collect()
}

fn seq(v: Vec<t::ZE>) -> Out {
    Out::Seq(from_ze(v))
}

fn arg_str(a: &[Json], i: usize) -> Option<String> {
    a.get(i).and_then(|j| j.as_str()).map(|s| s.to_string())
}

/// The user's answer to question number `n` of the command, or the question itself.
pub fn ask(c: &Ctx, n: usize, message: &str, actions: &[&str]) -> Result<Option<String>, Out> {
    match c.answers.get(n) {
        Some(a) => Ok(a.clone()),
        None => Err(Out::Ask { message: message.to_string(), actions: actions.iter().map(|s| s.to_string()).collect() }),
    }
}

fn an<'a>(c: &'a Ctx<'a>) -> Option<super::refactors::An<'a>> {
    Some(super::refactors::An { q: c.q, f: c.q.s.id(&c.uri)? })
}

fn with_tree<R>(c: &Ctx, f: impl FnOnce(&Tree, super::tree::Z) -> Option<R>) -> Option<R> {
    let text = c.text.clone()?;
    let tree = Tree::parse(&text);
    if tree.err {
        return None;
    }
    let z = super::tree::find_at_pos(&tree, c.row, c.col)?;
    f(&tree, z)
}

fn drag_out(c: &Ctx, d: super::clauses::DragOut) -> Out {
    Out::Map {
        changes: vec![(c.uri.clone(), d.edits.into_iter().map(|e| Edit { range: Some(e.range), text: e.text }).collect())],
        resources: vec![],
        show: Some(Show { uri: c.uri.clone(), range: Some(d.show) }),
    }
}

pub fn clean_ns_findings_pub(c: &Ctx) -> Option<(super::clean_ns::Findings, Vec<String>)> {
    clean_ns_findings(c)
}

impl<'a> Ctx<'a> {
    /// A string setting from the initialization options (`path` of keys).
    pub fn init_setting(&self, path: &[&str]) -> Option<String> {
        let mut cur = self.init.as_ref()?;
        for p in path {
            cur = cur.get(p)?;
        }
        cur.as_str().map(|s| s.to_string())
    }
}

fn clean_ns_findings(c: &Ctx) -> Option<(super::clean_ns::Findings, Vec<String>)> {
    let f = c.q.s.id(&c.uri)?;
    let fa = c.q.fa(f);
    let entry = c.q.entry(f);
    let mut fnd = super::clean_ns::Findings { unused_aliases: Default::default(), unused_refers: Default::default(), unused_imports: Default::default(), duplicate_requires: Default::default() };
    for x in &entry.findings {
        match x.ty.as_str() {
            "unused-namespace" => {
                if let Some(ns) = x.message.strip_prefix("namespace ").and_then(|m| m.strip_suffix(" is required but never used")) {
                    if !fa.var_usages.iter().any(|u| !u.refer && u.to.as_str() == ns) {
                        fnd.unused_aliases.insert(ns.to_string());
                    }
                }
            }
            "unused-referred-var" => {
                if let Some(q) = x.message.strip_prefix("#'").and_then(|m| m.strip_suffix(" is referred but never used")) {
                    if let Some((ns, name)) = q.split_once('/') {
                        let mut seen: Vec<(u32, u32, u32, u32)> = Vec::new();
                        for u in fa.var_usages.iter().filter(|u| u.name.as_str() == name && u.to.as_str() == ns) {
                            let k = (u.pos.row, u.pos.col, u.pos.end_row, u.pos.end_col);
                            if !seen.contains(&k) {
                                seen.push(k);
                            }
                        }
                        if seen.len() <= 1 {
                            fnd.unused_refers.insert(q.to_string());
                        }
                    }
                }
            }
            "unused-import" => {
                // kondo `:class` (full name): the import usage at the finding position
                let cl = fa.java_class_usages.iter().find(|u| u.flags & crate::analyzer::JU_IMPORT != 0 && u.pos.row == x.row && u.pos.col == x.col).map(|u| u.class.as_str().to_string());
                if let Some(cl) = cl {
                    let used_var = fa.var_usages.iter().any(|u| format!("{}.{}", u.to.as_str(), u.name.as_str()) == cl);
                    let used_java = fa.java_class_usages.iter().any(|u| u.class.as_str() == cl && u.flags & crate::analyzer::JU_IMPORT == 0);
                    if !(used_var || used_java) {
                        fnd.unused_imports.insert(cl);
                    }
                }
            }
            "duplicate-require" => {
                if let Some(ns) = x.message.strip_prefix("duplicate require of ") {
                    fnd.duplicate_requires.insert(ns.to_string());
                }
            }
            _ => {}
        }
    }
    let aliases: Vec<String> = fa.var_usages.iter().filter(|u| !u.alias.is_none()).map(|u| u.alias.as_str().to_string()).collect();
    Some((fnd, aliases))
}

fn run(c: &Ctx, cmd: &str) -> Out {
    if cmd == "clean-ns" {
        let Some(root) = c.root.as_ref() else { return Out::NoOp };
        let loc = c.loc.clone().unwrap_or_else(|| root.clone());
        let Some((fnd, aliases)) = clean_ns_findings(c) else { return Out::NoOp };
        let st = super::clean_ns::CleanSettings::from_json(c.init.as_ref());
        return match super::clean_ns::clean_ns_edits(&loc, &st, fnd, &|a| aliases.iter().any(|x| x == a)) {
            Some(v) => seq(v),
            None => Out::NoOp,
        };
    }
    if cmd == "server-info" || cmd == "cursor-info" {
        return match cmd {
            "server-info" => Out::Info(super::info::server_info_text(c.q, c.init.as_ref())),
            // JVM: `(apply cursor-info-log uri components args)` needs exactly [row col] after the 3 standard arguments
            "cursor-info" => match (c.args.len(), c.args.first().and_then(|a| a.as_f64()), c.args.get(1).and_then(|a| a.as_f64())) {
                (2, Some(r), Some(cl)) => Out::Info(super::info::cursor_info_text(c.q, &c.uri, r as u32, cl as u32)),
                _ => Out::Err("Internal error".into(), -32603),
            },
            _ => Out::NoOp,
        };
    }
    let loc = c.loc.as_ref();
    if loc.is_none() && cmd == "resolve-macro-as" {
        return super::resolve_macro::resolve_macro_as(c, None);
    }
    let Some(z) = loc else { return Out::Nil };
    match cmd {
        "change-coll" => seq(t::change_coll(z, &arg_str(&c.args, 0).unwrap_or_default())),
        "cycle-coll" => seq(t::cycle_coll(z)),
        "cycle-privacy" => seq(t::cycle_privacy(z, c.settings.use_metadata_for_privacy)),
        "thread-first" => seq(t::thread_one(z, "->", c.settings.keep_parens)),
        "thread-last" => seq(t::thread_one(z, "->>", c.settings.keep_parens)),
        "thread-first-all" => seq(t::thread_all(z, "->", c.settings.keep_parens)),
        "thread-last-all" => seq(t::thread_all(z, "->>", c.settings.keep_parens)),
        "unwind-thread" => seq(t::unwind_thread(z)),
        "unwind-all" => seq(t::unwind_all(z)),
        "introduce-let" => seq(super::refactors::introduce_let(z, &arg_str(&c.args, 0).unwrap_or_else(|| "new-binding".into()))),
        "move-to-let" => match an(c) {
            Some(a) => super::refactors::move_to_let(&a, z, &arg_str(&c.args, 0).unwrap_or_else(|| "new-binding".into())).map_or(Out::Nil, seq),
            None => Out::Nil,
        },
        "expand-let" => match an(c) {
            Some(a) => super::refactors::expand_let(&a, z, true).map_or(Out::Nil, |(r, l)| seq(vec![t::ZE { range: r, text: l.string() }])),
            None => Out::Nil,
        },
        "extract-to-def" => super::refactors::extract_to_def(z, arg_str(&c.args, 0).as_deref(), true).map_or(Out::Nil, seq),
        "suppress-diagnostic" => super::refactors::suppress_diagnostic(z, &arg_str(&c.args, 0).unwrap_or_default()).map_or(Out::Nil, seq),
        "sort-clauses" | "sort-map" => with_tree(c, |tree, tz| super::clauses::sort_edits(c.q, &c.uri, tree, tz).map(|v| Out::Seq(v.into_iter().map(|e| Edit { range: Some(e.range), text: e.text }).collect()))).unwrap_or(Out::Nil),
        "drag-backward" | "move-coll-entry-up" | "drag-forward" | "move-coll-entry-down" => {
            let fwd = matches!(cmd, "drag-forward" | "move-coll-entry-down");
            with_tree(c, |tree, tz| super::clauses::drag(c.q, &c.uri, tree, tz, fwd, (c.row as i64, c.col as i64)).map(|d| drag_out(c, d))).unwrap_or(Out::Nil)
        }
        "if->cond-refactor" => super::refactors::if_to_cond(z),
        "cond->if-refactor" => super::refactors::cond_to_if(z),
        "extract-function" => super::extract::extract_function(c, z, c.loc_end.as_ref(), &arg_str(&c.args, 0).unwrap_or_else(|| "new-function".into())),
        "add-require-suggestion" => {
            let ns = arg_str(&c.args, 0).unwrap_or_default();
            let alias = arg_str(&c.args, 1);
            let refer = arg_str(&c.args, 2);
            let js = matches!(c.args.get(3), Some(Json::Bool(true)));
            super::libspec::add_require_suggestion(c, z, &ns, alias.as_deref(), refer.as_deref(), js)
        }
        "add-missing-import" | "add-import-to-namespace" => super::libspec::add_missing_import(c, z, &arg_str(&c.args, 0).unwrap_or_default()),
        "swap-namespace-with-alias" => {
            super::libspec::swap_namespace_with_alias(z, &arg_str(&c.args, 0).unwrap_or_default(), &arg_str(&c.args, 1).unwrap_or_default()).map_or(Out::Nil, |v| Out::Seq(v))
        }
        "demote-fn" => super::promote::demote_fn(z).map_or(Out::Nil, seq),
        "promote-fn" => super::promote::promote_fn(c, z, arg_str(&c.args, 0).as_deref()),
        "move-to-for-let" => super::refactors::move_to_for_let(z, &arg_str(&c.args, 0).unwrap_or_else(|| "new-binding".into())).map_or(Out::Seq(vec![]), seq),
        "create-function" => super::create_function::create_function(c, z),
        "inline-function" => match c.root.as_ref() {
            Some(r) => super::inline::inline_function(c.q, &c.uri, r, z),
            None => Out::Nil,
        },
        "drag-param-backward" => drag_param(c, false),
        "drag-param-forward" => drag_param(c, true),
        "move-form" => super::move_form::move_form(c),
        "create-test" => super::create_test::create_test(c, z),
        "resolve-macro-as" => {
            let ms = c.text.clone().and_then(|t| {
                let tree = Tree::parse(&t);
                if tree.err {
                    return None;
                }
                let zz = super::tree::find_at_pos(&tree, c.row, c.col)?;
                super::macro_sym(c.q, &c.uri, zz)
            });
            super::resolve_macro::resolve_macro_as(c, ms)
        }
        "forward" | "forward-select" | "backward" | "backward-select" => {
            // paredit movement: only a showDocument range
            let sexpr = if !is_printable_only(z.tag()) { Some(z.clone()) } else if cmd.starts_with("forward") { z.right() } else { z.left() };
            let Some(m) = sexpr.and_then(|s| s.meta()) else { return Out::Nil };
            let (r, cl) = (c.row, c.col);
            let range = match cmd {
                "forward" => Meta { row: m.end_row, col: m.end_col, end_row: m.end_row, end_col: m.end_col },
                "forward-select" => Meta { row: r, col: cl, end_row: m.end_row, end_col: m.end_col },
                "backward" => Meta { row: m.row, col: m.col, end_row: m.row, end_col: m.col },
                _ => Meta { row: m.row, col: m.col, end_row: r, end_col: cl },
            };
            Out::Map { changes: vec![], resources: vec![], show: Some(Show { uri: c.uri.clone(), range: Some(range) }) }
        }
        "forward-slurp" | "forward-barf" | "backward-slurp" | "backward-barf" | "raise-sexp" | "kill-sexp" => super::paredit::paredit_op(c, cmd),
        "restructure-keys" => super::refactors::restructure_keys(c.q, &c.uri, z).map_or(Out::Nil, seq),
        "add-missing-libspec" => {
            let Some(sym) = super::libspec::safe_sym(z) else { return Out::Nil };
            match super::sugg::require_suggestions(c.q, &c.uri, &sym).into_iter().next() {
                Some(s) => super::libspec::add_require_suggestion(c, z, &s.ns, s.alias.as_deref(), s.refer.as_deref(), false),
                None => Out::Nil,
            }
        }
        "refer-to-as" => super::refactors::refer_to_as(c.q, &c.uri, z).map_or(Out::Nil, seq),
        "as-to-refer" => super::refactors::as_to_refer(c.q, &c.uri, z).map_or(Out::Nil, seq),
        "replace-refer-all-with-refer" => {
            let refers: Vec<String> = match c.args.first() {
                Some(Json::Arr(a)) => a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
                _ => vec![],
            };
            seq(super::refactors::replace_refer_all_with_refer(z, &refers))
        }
        "replace-refer-all-with-alias" => super::refactors::replace_refer_all_with_alias(c.q, &c.uri, z).map_or(Out::Nil, seq),
        "cycle-keyword-auto-resolve" => super::refactors::cycle_keyword(c, z),
        "cycle-namespaced-map" => super::refactors::cycle_namespaced_map(z).map_or(Out::Nil, seq),
        "destructure-keys" => super::refactors::destructure_keys(c.q, &c.uri, z).map_or(Out::Nil, seq),
        "inline-symbol" => super::inline::inline_symbol(c.q, &c.uri, c.row, c.col),
        "get-in-more" => seq(super::thread_get::get_in_more(z)),
        "get-in-all" => seq(super::thread_get::get_in_all(z)),
        "get-in-less" => seq(super::thread_get::get_in_less(z)),
        "get-in-none" => seq(super::thread_get::get_in_none(z)),
        _ => Out::Err(format!("Command {cmd} is not supported natively"), -32601),
    }
}

/// `params`: `{command, arguments, caps:{docChanges,resourceOps,annotations}, settings:{usesMetadataForPrivacy,keepParens}}`.
pub fn exec_command(q: &Q, params: &str) -> String {
    let Some(j) = jparse(params) else { return "null".into() };
    let cmd = j.get("command").and_then(|c| c.as_str()).unwrap_or("").to_string();
    let args: Vec<Json> = j.get("arguments").and_then(|a| a.as_arr()).map(|a| a.to_vec()).unwrap_or_default();
    let caps = {
        let c = j.get("caps");
        let b = |k: &str| matches!(c.and_then(|c| c.get(k)), Some(Json::Bool(true)));
        Caps { doc_changes: b("docChanges"), resource_ops: b("resourceOps"), annotations: b("annotations") }
    };
    let settings = {
        let c = j.get("settings");
        let b = |k: &str| matches!(c.and_then(|c| c.get(k)), Some(Json::Bool(true)));
        Settings { use_metadata_for_privacy: b("usesMetadataForPrivacy"), keep_parens: b("keepParens") }
    };
    let answers: Vec<Option<String>> = j.get("answers").and_then(|a| a.as_arr()).map(|a| a.iter().map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
    // JVM `(int line)` / `(int character)` throw on missing arguments: internal error
    if !matches!(args.get(1), Some(Json::Num(_))) || !matches!(args.get(2), Some(Json::Num(_))) {
        return err_json(-32603, "Internal error");
    }
    let uri = args.first().and_then(|a| a.as_str()).unwrap_or("").to_string();
    let num = |i: usize| args.get(i).and_then(|a| a.as_f64()).map(|n| n as i64);
    let (line, ch) = (num(1).unwrap_or(0), num(2).unwrap_or(0));
    let rest: Vec<Json> = args.iter().skip(3).cloned().collect();
    let selection = cmd == "extract-function" && rest.len() > 2;
    let (line_end, ch_end) = if selection { (rest[rest.len() - 2].as_f64().map(|n| n as i64).unwrap_or(line), rest[rest.len() - 1].as_f64().map(|n| n as i64).unwrap_or(ch)) } else { (line, ch) };
    let fn_args: Vec<Json> = if selection { rest[..rest.len() - 2].to_vec() } else { rest };
    let (row, col) = ((line + 1) as u32, (ch + 1) as u32);
    let (end_row, end_col) = ((line_end + 1) as u32, (ch_end + 1) as u32);
    let file = q.s.id(&uri);
    let text = file.and_then(|f| file_text(q, f));
    let version = j.get("version").and_then(|v| v.as_f64()).map(|v| v as i64).unwrap_or(0);
    let mut root = None;
    let (mut loc, mut loc_end) = (None, None);
    if let Some(text) = text.clone() {
        let tree = Tree::parse(&text);
        if !tree.err {
            let r = Loc::of_node(from_tree(&tree));
            loc = find_at_pos(&r, row, col);
            loc_end = find_at_pos(&r, end_row, end_col);
            root = Some(r);
        }
    }
    let ctx = Ctx { q, uri: uri.clone(), line, ch, row, col, end_row, end_col, args: fn_args, settings, version, answers, init: super::settings::effective(q.s.project.as_ref().map(|p| p.root.as_path()), j.get("init").cloned()), text, root, loc, loc_end };
    let out = if ctx.loc.is_none() && !runs_without_loc(&cmd) { Out::Nil } else { run(&ctx, &cmd) };
    let no_loc = ctx.loc.is_none() && !runs_without_loc(&cmd);
    render(&ctx, out, no_loc, &caps)
}

/// Commands the JVM runs (and answers) before its "no form at this location" check.
fn runs_without_loc(cmd: &str) -> bool {
    matches!(cmd, "clean-ns" | "cursor-info" | "server-info" | "resolve-macro-as")
}

fn err_json(code: i64, msg: &str) -> String {
    format!("{{\"error\":{{\"code\":{code},\"message\":{}}}}}", json_str(msg))
}

fn client_changes(docs: &[(String, i64, Vec<Edit>)], resources: &[ResourceChange], caps: &Caps) -> String {
    let doc_json = |d: &(String, i64, Vec<Edit>)| format!("{{\"textDocument\":{{\"uri\":{},\"version\":{}}},\"edits\":{}}}", json_str(&d.0), d.1, edits_json(&d.2));
    if caps.doc_changes || caps.resource_ops {
        let mut parts: Vec<String> = resources
            .iter()
            .map(|r| format!("{{\"kind\":\"create\",\"uri\":{},\"options\":{{\"overwrite\":false,\"ignoreIfExists\":true}}}}", json_str(&r.uri)))
            .collect();
        parts.extend(docs.iter().map(doc_json));
        let n = parts.len();
        if caps.annotations && n > 1 {
            // refactor review: confirm annotations on every change (see shared/inject-confirm-annotations)
            let mut anns: Vec<String> = Vec::new();
            for (i, p) in parts.iter().enumerate() {
                if i < resources.len() {
                    anns.push(p.replacen("{\"kind\":\"create\"", "{\"kind\":\"create\",\"annotationId\":\"confirmClojureLspRefactor\"", 1));
                } else {
                    anns.push(p.clone());
                }
            }
            return format!(
                "{{\"documentChanges\":[{}],\"changeAnnotations\":{{\"confirmClojureLspRefactor\":{{\"label\":\"Confirm clojure-lsp refactor\",\"needsConfirmation\":true}}}}}}",
                anns.join(",")
            );
        }
        format!("{{\"documentChanges\":[{}]}}", parts.join(","))
    } else {
        let mut s = String::from("{\"changes\":{");
        for (i, d) in docs.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("{}:{}", json_str(&d.0), edits_json(&d.2)));
        }
        s.push_str("}}");
        s
    }
}

fn render(c: &Ctx, out: Out, no_loc: bool, caps: &Caps) -> String {
    match out {
        Out::NoOp => "null".into(),
        Out::Err(m, code) => err_json(code, &m),
        _ if no_loc => err_json(-32602, "Could not find a form at this location."),
        Out::Map { changes, resources, show } => {
            let docs: Vec<(String, i64, Vec<Edit>)> = changes.into_iter().map(|(u, e)| { let v = if u == c.uri { c.version } else { -1 }; (u, v, e) }).collect();
            let edit = if docs.is_empty() && resources.is_empty() { "null".to_string() } else { client_changes(&docs, &resources, caps) };
            let show_json = match show {
                Some(s) => {
                    let r = s.range.map(range_json).unwrap_or_else(|| "{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":999999,\"character\":999999}}".to_string());
                    format!("{{\"uri\":{},\"takeFocus\":true,\"selection\":{}}}", json_str(&s.uri), r)
                }
                None => "null".to_string(),
            };
            format!("{{\"edit\":{edit},\"show\":{show_json}}}")
        }
        Out::Seq(es) => {
            if es.is_empty() {
                err_json(-32600, "Nothing to change.")
            } else {
                let docs = vec![(c.uri.clone(), c.version, es)];
                format!("{{\"edit\":{},\"show\":null}}", client_changes(&docs, &[], caps))
            }
        }
        Out::Nil => err_json(-32600, "Nothing to change."),
        Out::Write { path, content } => format!("{{\"write\":{{\"path\":{},\"content\":{}}}}}", json_str(&path), json_str(&content)),
        Out::Info(t) => format!("{{\"info\":{}}}", json_str(&t)),
        Out::NilMsg(m) => format!("{{\"error\":{{\"code\":-32600,\"message\":\"Nothing to change.\"}},\"message\":{}}}", json_str(&m)),
        Out::Ask { message, actions } => {
            let acts: Vec<String> = actions.iter().map(|a| json_str(a)).collect();
            format!("{{\"ask\":{{\"message\":{},\"actions\":[{}]}}}}", json_str(&message), acts.join(","))
        }
    }
}

/// `drag-param-backward` / `drag-param-forward`.
fn drag_param(c: &Ctx, forward: bool) -> Out {
    use super::clauses as cl;
    let Some(text) = c.text.clone() else { return Out::Nil };
    let tree = Tree::parse(&text);
    if tree.err {
        return Out::Nil;
    }
    let Some(z) = super::tree::find_at_pos(&tree, c.row, c.col) else { return Out::Nil };
    let Some((pd, spec)) = cl::drag_param_defn(c.q, &c.uri, &tree, z, forward, (c.row as i64, c.col as i64)) else { return Out::Nil };
    let mut changes: Vec<(String, Vec<Edit>)> = vec![(c.uri.clone(), pd.defn.edits.into_iter().map(|e| Edit { range: Some(e.range), text: e.text }).collect())];
    // usage edits across files
    let zspec = tree.z(spec.zloc);
    let Some(top) = zspec.to_top() else { return Out::Nil };
    // find-var-definition-name-loc: the name after the op of the top form
    let name = (|| {
        let op = top.down()?;
        let n1 = op.next()?;
        match n1.tag() {
            Tag::Map => n1.right(),
            Tag::Meta if n1.down().map_or(false, |d| d.tag() == Tag::Map) => n1.down().and_then(|d| d.rightmost()),
            Tag::Meta => n1.next().and_then(|x| x.next()),
            _ => Some(n1),
        }
    })();
    let Some(name) = name else { return Out::Nil };
    let nm = name.meta();
    if let Some(el) = c.q.first_under_cursor(&c.uri, nm.row, nm.col) {
        if el.b == crate::engine::index::B::VarDef {
            let refs = c.q.find_references(el, false, None);
            let mut by_file: Vec<(crate::engine::store::FileId, Vec<crate::query::El>)> = Vec::new();
            for r in refs {
                match by_file.iter_mut().find(|(f, _)| *f == r.f) {
                    Some((_, v)) => v.push(r),
                    None => by_file.push((r.f, vec![r])),
                }
            }
            let mut skipped = false;
            let mut usage_changes: Vec<(String, Vec<Edit>)> = Vec::new();
            for (f, els) in by_file {
                let Some(t) = super::preds::file_text(c.q, f) else { continue };
                let tr = Tree::parse(&t);
                if tr.err {
                    continue;
                }
                let uri2 = c.q.uri(f).to_string();
                let mut edits: Vec<Edit> = Vec::new();
                let mut any_skip = false;
                for e in els {
                    let np = c.q.name_pos(e);
                    match cl::usage_edit(c.q, &uri2, &tr, np.row, np.col, pd.origin_idx, forward) {
                        Some(v) => edits.extend(v.into_iter().map(|x| Edit { range: Some(x.range), text: x.text })),
                        None => any_skip = true,
                    }
                }
                skipped |= any_skip;
                if !edits.is_empty() {
                    usage_changes.push((uri2, edits));
                }
            }
            if skipped {
                return Out::NilMsg("Cannot drag. Call sites include ->, ->>, partial, apply, or certain other forms which cannot be safely refactored.".into());
            }
            for (u, e) in usage_changes {
                match changes.iter_mut().find(|(x, _)| *x == u) {
                    Some((_, v)) => v.extend(e),
                    None => changes.push((u, e)),
                }
            }
        }
    }
    Out::Map { changes, resources: vec![], show: Some(Show { uri: c.uri.clone(), range: Some(pd.defn.show) }) }
}
