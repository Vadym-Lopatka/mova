//! `nx`: one-shot CLI for coding agents (contract: nx/CLI-DESIGN.md). Exit 0 ok / found, 1 not found or findings, 2 usage or project error.
use nx_core::cli::{check, def, doc, find, hook, ns, outline, refs, Opts, Reply};

const USAGE: &str = "usage: nx [--root <dir>] [--json] [--all] <command>
  check [file...] [--new] [--errors] [--info]   diagnostics
  def <sym> [--in file]              where + signature + doc + source
  refs <sym> [--in file]             uses, grouped by file, with the enclosing function
  doc <sym> [--in file]              origin (project, Mova native, Mova stdlib, jar), arglists, doc
  outline <file|ns>                  vars of a namespace: line, kind, name, arglists, first doc line
  ns [prefix]                        project namespaces: file, public var count, dependents
  find <text>                        var definitions whose name contains the text (project first)
  hook                               Claude Code PostToolUse / Stop hook (stdin JSON)";

fn parse(args: Vec<String>) -> Result<(String, Opts), String> {
    let mut o = Opts::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => o.json = true,
            "--all" => o.all = true,
            "--new" => o.new = true,
            "--info" => o.info = true,
            "--errors" => o.errors = true,
            "--root" => o.root = Some(it.next().ok_or("--root needs a directory")?),
            "--in" => o.in_file = Some(it.next().ok_or("--in needs a file")?),
            f if f.starts_with("--") => return Err(format!("unknown flag {f}")),
            _ => o.args.push(a),
        }
    }
    if o.args.is_empty() {
        return Err("no command".to_string());
    }
    let cmd = o.args.remove(0);
    Ok((cmd, o))
}

fn run(args: Vec<String>) -> Result<Reply, String> {
    let (cmd, o) = parse(args).map_err(|e| format!("{e}\n{USAGE}"))?;
    let r = match cmd.as_str() {
        "check" => check::run(&o),
        "def" => def::run(&o),
        "refs" => refs::run(&o),
        "doc" => doc::run(&o),
        "outline" => outline::run(&o),
        "ns" => ns::run(&o),
        "find" => find::run(&o),
        "hook" => Ok(hook::run()),
        _ => Err(format!("unknown command {cmd}\n{USAGE}")),
    };
    r.map_err(|e| if e.starts_with("usage: nx ") { format!("{e}\n{USAGE}") } else { e })
}

fn main() {
    let code = match run(std::env::args().skip(1).collect()) {
        Ok(r) => {
            print!("{}", r.out);
            eprint!("{}", r.err);
            r.code
        }
        Err(e) => {
            eprintln!("nx: {e}");
            2
        }
    };
    std::process::exit(code);
}
