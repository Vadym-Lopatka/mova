//! The command line of nREPL 1.8.0 (`nrepl.cmdline`), and its config files.
//!
//! Rules copied from the JVM, quirks included:
//!
//! * Shorthands (`-p` for `--port`, ...) are expanded in **every** argument,
//!   also in values. `-C` is not a shorthand there (a bug: the help lists it);
//!   here it means `--color`. `-r` is `--repl`, which nothing reads.
//! * Options come first. Parsing stops at the first argument that does not
//!   start with `-`. `--interactive --connect --color --help --version
//!   --verbose` take no value; every other option, known or not, takes the
//!   next argument (even if that is a flag). So unknown flags are ignored, but
//!   they eat the next word.
//! * `--handler`, `--transport` and `--middleware` values are EDN symbols (or a
//!   vector of symbols for middleware).
//! * Config: `~/.nrepl/nrepl.edn` (or `$NREPL_CONFIG_DIR/nrepl.edn`,
//!   `$XDG_CONFIG_HOME/nrepl/nrepl.edn`, `~/.config/nrepl/nrepl.edn`: the first
//!   that exists) is merged under `./.nrepl.edn`, and both under the command
//!   line. Keys are the long option names as keywords (`:port`, `:bind`, ...).
//!
//! Beyond the JVM: `--flag=value` works, and three Mova switches
//! (`--errors=rich|jvm`, `--no-core-image`, `--boot-first`).
//!
//! Parsing allocates a few small strings and reads at most four small files
//! (ENOENT is the usual answer); it does not touch the interpreter.

use crate::edn::{self, Edn};
use std::path::PathBuf;

/// `--help` output, byte for byte what 1.8.0 prints (with its newline).
pub const HELP: &str = include_str!("help.txt");

/// The parsed command line merged over the config files. Strings are raw;
/// `port()` and `ack()` parse them.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Options {
    pub help: bool,
    pub version: bool,
    pub connect: bool,
    pub interactive: bool,
    pub color: bool,
    pub verbose: bool,
    pub host: Option<String>,
    pub bind: Option<String>,
    pub port: Option<String>,
    pub socket: Option<String>,
    pub transport: Option<String>,
    pub handler: Option<String>,
    pub middleware: Vec<String>,
    pub repl_fn: Option<String>,
    pub tls_keys_file: Option<String>,
    pub tls_keys_str: Option<String>,
    pub ack: Option<String>,
    // Mova only
    pub errors: Option<String>,
    pub no_core_image: bool,
    pub boot_first: bool,
}

const SHORTHANDS: &[(&str, &str)] = &[
    ("-i", "--interactive"),
    ("-r", "--repl"),
    ("-f", "--repl-fn"),
    ("-c", "--connect"),
    ("-C", "--color"),
    ("-b", "--bind"),
    ("-h", "--host"),
    ("-p", "--port"),
    ("-s", "--socket"),
    ("-m", "--middleware"),
    ("-t", "--transport"),
    ("-n", "--handler"),
    ("-v", "--version"),
];

const UNARY: &[&str] =
    &["--interactive", "--connect", "--color", "--help", "--version", "--verbose", "--no-core-image", "--boot-first"];

fn expand(arg: &str) -> &str {
    SHORTHANDS.iter().find(|(s, _)| *s == arg).map(|(_, l)| *l).unwrap_or(arg)
}

/// A symbol written on the command line or in a config file, as text.
fn symbol(v: &str) -> Result<String, String> {
    match edn::parse(v.as_bytes(), 0, true) {
        Ok((Edn::Sym(s), _)) => Ok(s),
        Ok((other, _)) => Err(format!("{other:?} is not a symbol")),
        Err(_) => Err(format!("cannot read {v:?}")),
    }
}

fn symbols(v: &str) -> Result<Vec<String>, String> {
    match edn::parse(v.as_bytes(), 0, true) {
        Ok((Edn::Sym(s), _)) => Ok(vec![s]),
        Ok((Edn::Vec(l) | Edn::List(l), _)) => l
            .into_iter()
            .map(|e| match e {
                Edn::Sym(s) => Ok(s),
                other => Err(format!("{other:?} is not a symbol")),
            })
            .collect(),
        Ok((other, _)) => Err(format!("{other:?} is not a symbol")),
        Err(_) => Err(format!("cannot read {v:?}")),
    }
}

impl Options {
    /// Applies one option by its long name (without `--`). Unknown names are ignored.
    fn set(&mut self, name: &str, v: Val<'_>) -> Result<(), String> {
        let text = || v.text();
        match name {
            "help" => self.help = v.truthy(),
            "version" => self.version = v.truthy(),
            "connect" => self.connect = v.truthy(),
            "interactive" => self.interactive = v.truthy(),
            "color" => self.color = v.truthy(),
            "verbose" => self.verbose = v.truthy(),
            "no-core-image" => self.no_core_image = v.truthy(),
            "boot-first" => self.boot_first = v.truthy(),
            "host" => self.host = text(),
            "bind" => self.bind = text(),
            "port" => self.port = text(),
            "socket" => self.socket = text(),
            "repl-fn" => self.repl_fn = text(),
            "tls-keys-file" => self.tls_keys_file = text(),
            "tls-keys-str" => self.tls_keys_str = text(),
            "ack" => self.ack = text(),
            "errors" => self.errors = text(),
            "transport" => self.transport = text().map(|t| symbol(&t)).transpose()?,
            "handler" => self.handler = text().map(|t| symbol(&t)).transpose()?,
            "middleware" => {
                if let Some(t) = text() {
                    self.middleware = symbols(&t)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Reads `args` (the words after `nrepl`) over `base` (the config).
    pub fn parse_over(mut self, args: &[String]) -> Result<Options, String> {
        let mut i = 0;
        while i < args.len() {
            let (flag, inline) = match args[i].split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v)),
                _ => (args[i].as_str(), None),
            };
            let flag = expand(flag);
            if !flag.starts_with('-') {
                break; // first non-option word ends the options
            }
            if UNARY.contains(&flag) {
                self.set(flag.trim_start_matches("--"), Val::Bool(true))?;
            } else {
                let v = match inline {
                    Some(v) => Some(v),
                    None => {
                        i += 1;
                        args.get(i).map(|a| expand(a))
                    }
                };
                // `-C` etc. that is not `--name` is unknown: ignored
                if let (Some(name), Some(v)) = (flag.strip_prefix("--"), v) {
                    self.set(name, Val::Str(v))?;
                }
            }
            i += 1;
        }
        Ok(self)
    }

    /// Options from the config files only.
    pub fn from_config(files: &[Vec<u8>]) -> Result<Options, String> {
        let mut o = Options::default();
        for content in files {
            let Ok((Edn::Map(m), _)) = edn::parse(content, 0, true) else {
                return Err("config file is not an EDN map".into());
            };
            for (k, v) in m {
                let Edn::Kw(k) = k else { continue };
                let val = match &v {
                    Edn::Nil | Edn::Bool(false) => continue,
                    Edn::Bool(true) => Val::Bool(true),
                    Edn::Int(n) => Val::Owned(n.to_string()),
                    Edn::Str(s) | Edn::Sym(s) | Edn::Kw(s) => Val::Owned(s.clone()),
                    Edn::Vec(l) | Edn::List(l) => Val::Owned(format!(
                        "[{}]",
                        l.iter()
                            .filter_map(|e| match e {
                                Edn::Sym(s) => Some(s.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    )),
                    _ => continue,
                };
                o.set(&k, val)?;
            }
        }
        Ok(o)
    }

    /// `--port` as the JVM reads it (`Integer/parseInt`, then a bind that must fit 0..=65535).
    pub fn port_number(&self) -> Result<Option<u16>, String> {
        self.number(self.port.as_deref(), "port")
    }

    pub fn ack_number(&self) -> Result<Option<u16>, String> {
        self.number(self.ack.as_deref(), "ack")
    }

    fn number(&self, v: Option<&str>, what: &str) -> Result<Option<u16>, String> {
        let Some(v) = v else { return Ok(None) };
        let n: i64 = v.parse().map_err(|_| format!("java.lang.NumberFormatException: For input string: \"{v}\""))?;
        u16::try_from(n).map(Some).map_err(|_| format!("{what} value out of range: {n}"))
    }

    pub fn transport_symbol(&self) -> &str {
        self.transport.as_deref().unwrap_or("nrepl.transport/bencode")
    }
}

enum Val<'a> {
    Bool(bool),
    Str(&'a str),
    Owned(String),
}

impl Val<'_> {
    fn truthy(&self) -> bool {
        match self {
            Val::Bool(b) => *b,
            Val::Str(s) => !s.is_empty(),
            Val::Owned(_) => true,
        }
    }
    fn text(&self) -> Option<String> {
        match self {
            Val::Bool(_) => None,
            Val::Str(s) => Some((*s).to_string()),
            Val::Owned(s) => Some(s.clone()),
        }
    }
}

/// The config file paths in order of precedence: global first, local last.
/// Only paths; the caller reads them (a missing file is fine).
pub fn config_paths() -> (Vec<PathBuf>, PathBuf) {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = env("HOME");
    let mut c = Vec::new();
    if let Some(d) = env("NREPL_CONFIG_DIR") {
        c.push(d.join("nrepl.edn"));
    }
    if let Some(d) = env("XDG_CONFIG_HOME") {
        c.push(d.join("nrepl").join("nrepl.edn"));
    }
    if let Some(h) = &home {
        c.push(h.join(".config").join("nrepl").join("nrepl.edn"));
        c.push(h.join(".nrepl").join("nrepl.edn"));
    }
    (c, PathBuf::from(".nrepl.edn"))
}

/// True if any config file might exist. Start-up calls this first: it costs a
/// few `access` calls on stack buffers (no allocation, no `Path`), so a start
/// with no config files stays as fast as one that never looked.
fn config_might_exist() -> bool {
    fn present(parts: &[&[u8]]) -> bool {
        let mut buf = [0u8; 1024];
        let mut n = 0;
        for p in parts {
            if n + p.len() + 1 > buf.len() {
                return true; // too long to check here: let the slow path decide
            }
            buf[n..n + p.len()].copy_from_slice(p);
            n += p.len();
        }
        buf[n] = 0;
        // SAFETY: `buf` holds a NUL-terminated path.
        let r = unsafe { libc::access(buf.as_ptr().cast(), libc::F_OK) };
        // anything but "no such file" (permission, loops, ...) goes to the slow path
        r == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound
    }
    fn env(name: &'static [u8]) -> Option<&'static [u8]> {
        // SAFETY: `name` is NUL-terminated; getenv returns a NUL-terminated string or null.
        let p = unsafe { libc::getenv(name.as_ptr().cast()) };
        if p.is_null() {
            return None;
        }
        // SAFETY: `p` points to a NUL-terminated C string that lives as long as the environment.
        let b = unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes();
        (!b.is_empty()).then_some(b)
    }
    if present(&[b".nrepl.edn"]) {
        return true;
    }
    if env(b"NREPL_CONFIG_DIR\0").is_some_and(|d| present(&[d, b"/nrepl.edn"])) {
        return true;
    }
    if env(b"XDG_CONFIG_HOME\0").is_some_and(|d| present(&[d, b"/nrepl/nrepl.edn"])) {
        return true;
    }
    match env(b"HOME\0") {
        Some(h) => present(&[h, b"/.config/nrepl/nrepl.edn"]) || present(&[h, b"/.nrepl/nrepl.edn"]),
        None => false,
    }
}

/// Reads the config: the first global file that exists, then the local one.
pub fn read_config() -> Vec<Vec<u8>> {
    if !config_might_exist() {
        return Vec::new();
    }
    let (global, local) = config_paths();
    let mut files = Vec::new();
    if let Some(b) = global.iter().find_map(|p| std::fs::read(p).ok()) {
        files.push(b);
    }
    if let Ok(b) = std::fs::read(local) {
        files.push(b);
    }
    files
}

/// The whole thing: config files under the command line.
pub fn parse(args: &[String]) -> Result<Options, String> {
    Options::from_config(&read_config())?.parse_over(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Options {
        let a: Vec<String> = a.iter().map(|s| s.to_string()).collect();
        Options::default().parse_over(&a).unwrap()
    }

    #[test]
    fn flags() {
        let o = p(&["-p", "7888", "--bind", "0.0.0.0", "-s", "/tmp/x", "--verbose", "-t", "nrepl.transport/edn", "--ack", "5"]);
        assert_eq!(o.port.as_deref(), Some("7888"));
        assert_eq!(o.bind.as_deref(), Some("0.0.0.0"));
        assert_eq!(o.socket.as_deref(), Some("/tmp/x"));
        assert!(o.verbose);
        assert_eq!(o.transport_symbol(), "nrepl.transport/edn");
        assert_eq!(o.ack_number().unwrap(), Some(5));
    }

    #[test]
    fn host_is_not_help_and_unary() {
        let o = p(&["-h", "example.org", "-c", "-C", "-i", "-v"]);
        assert_eq!(o.host.as_deref(), Some("example.org"));
        assert!(o.connect && o.color && o.interactive && o.version && !o.help);
    }

    #[test]
    fn unknown_flags_eat_the_next_word() {
        let o = p(&["--nosuch", "--verbose", "--port", "9"]);
        assert!(!o.verbose, "the value of --nosuch");
        assert_eq!(o.port.as_deref(), Some("9"));
        // parsing stops at the first plain word
        let o = p(&["stop", "--verbose"]);
        assert!(!o.verbose);
    }

    #[test]
    fn middleware_and_equals() {
        let o = p(&["-m", "[a/b c/d]", "--handler=x.y/z"]);
        assert_eq!(o.middleware, ["a/b", "c/d"]);
        assert_eq!(o.handler.as_deref(), Some("x.y/z"));
        assert_eq!(p(&["-m", "a/b"]).middleware, ["a/b"]);
        assert!(Options::default().parse_over(&["-m".into(), "\"s\"".into()]).is_err());
    }

    #[test]
    fn config_under_cli() {
        let o = Options::from_config(&[b"{:port 7000 :bind \"0.0.0.0\" :transport nrepl.transport/edn :verbose true :middleware [a/b]}".to_vec()])
            .unwrap();
        assert_eq!(o.port.as_deref(), Some("7000"));
        assert_eq!(o.middleware, ["a/b"]);
        let o = o.parse_over(&["-p".into(), "7001".into()]).unwrap();
        assert_eq!(o.port.as_deref(), Some("7001"));
        assert_eq!(o.bind.as_deref(), Some("0.0.0.0"));
        assert!(o.verbose);
    }

    #[test]
    fn numbers() {
        let mut o = Options::default();
        o.port = Some("x".into());
        assert!(o.port_number().is_err());
        o.port = Some("70000".into());
        assert!(o.port_number().is_err());
        o.port = Some("7".into());
        assert_eq!(o.port_number().unwrap(), Some(7));
    }

    #[test]
    fn help_text_is_the_jvm_text() {
        assert!(HELP.starts_with("Usage:\n\n  -i/--interactive"));
        assert!(HELP.ends_with("--verbose                   Show verbose output.\n"));
    }
}
