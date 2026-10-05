//! `nx hook`: Claude Code hook. stdin JSON, by `hook_event_name`: PostToolUse (the edited file) or Stop (files changed in the session); new errors on stderr, exit 2.
use super::*;
use crate::analyzer::json::Json;
use std::io::Read;

const FIX_NOW: &str = "(fix them now; look things up with `nx def|refs|doc <sym>`; re-check with `nx check <file>`)\n";

pub fn run() -> Reply {
    // never fail an edit for our own errors: exit 0
    dispatch().unwrap_or_else(|| Reply::out(String::new(), 0))
}

fn dispatch() -> Option<Reply> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok()?;
    let json = crate::analyzer::json::parse(&input)?;
    match json.get("hook_event_name").and_then(Json::as_str).unwrap_or("PostToolUse") {
        "PostToolUse" => check_edited(&json),
        "Stop" => check_session(&json),
        _ => None,
    }
}

/// `check --new --errors` of `files` (none: the files git reports as changed) in the project at `root`.
fn errors_of(root: &Path, files: &[PathBuf]) -> Option<Reply> {
    let o = Opts { new: true, errors: true, ..Opts::default() };
    check::check(&Project::load(root.to_path_buf()), files, &o).ok()
}

/// The check lines under a header with their error count (the lines that are not source lines or `broken elsewhere:`); clean -> exit 0.
fn blocked(r: Reply, what: &str, footer: &str) -> Reply {
    let n = r.out.lines().filter(|l| !l.starts_with(' ') && *l != "broken elsewhere:").count();
    if n == 0 {
        return Reply::out(String::new(), 0);
    }
    Reply::err(format!("nx: {n} new error(s) {what}\n{}{footer}", r.out), 2)
}

fn check_edited(json: &Json) -> Option<Reply> {
    let file = PathBuf::from(json.get("tool_input")?.get("file_path")?.as_str()?).canonicalize().ok()?;
    if !scan::is_source(&file) {
        return None;
    }
    let root = nearest_root(file.parent()?)?.canonicalize().ok()?;
    let mut r = errors_of(&root, &[file.clone()])?;
    // a formatter hook may be rewriting the file right now: look once more
    if r.out.contains(" error syntax ") {
        std::thread::sleep(std::time::Duration::from_millis(250));
        r = errors_of(&root, &[file])?;
    }
    Some(blocked(r, "after this edit", FIX_NOW))
}

fn check_session(json: &Json) -> Option<Reply> {
    if json.get("stop_hook_active") == Some(&Json::Bool(true)) {
        return None;
    }
    let root = nearest_root(Path::new(json.get("cwd")?.as_str()?))?.canonicalize().ok()?;
    Some(blocked(errors_of(&root, &[])?, "in files changed in this session. Fix them before you finish.", ""))
}
