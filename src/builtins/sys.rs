//! System natives: `slurp spit getenv exit sh(=shell-out) read-line
//! file-exists? delete-file list-dir cwd time-ms` -- libc-backed where it
//! buys authentic errno reporting (ARCHITECTURE.md's showcase feature).
//! This is the one module allowed `unsafe`; every `unsafe` block carries a
//! one-line invariant comment.

use std::ffi::CString;

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{PMap, Symbol, Value};

pub fn register(i: &mut Interp) {
    reg(i, "slurp", ArityHint::Exact(1), slurp);
    reg(i, "spit", ArityHint::Min(2), spit);
    // mova campaign (clojure-lsp): `classpath.mova`/`shared.mova` call
    // the 0-arg `System/getenv()` overload (the WHOLE environment map),
    // not just the 1-arg lookup -- see `getenv`'s doc.
    reg(i, "getenv", ArityHint::Range(0, 1), getenv);
    reg(i, "exit", ArityHint::Range(0, 1), exit);
    reg(i, "sh", ArityHint::Min(1), sh);
    reg(i, "read-line", ArityHint::Exact(0), read_line);
    // `(--read-form-text)`: the text of one form from `*in*`, the rest stays buffered (nil at EOF)
    reg(i, "--read-form-text", ArityHint::Exact(0), |interp, _args| {
        match interp.globals.get(&Symbol::simple("*in*")) {
            Some(Value::HostInst(h)) if h.kind == crate::hostclass::HostKind::InputStream => {
                Ok(crate::hostclass::stream_read_form_text(&h)?.map(|t| Value::Str(t.into())).unwrap_or(Value::Nil))
            }
            // not a host stream (a `with-in-str` reader): the caller reads by lines
            _ => Ok(Value::Keyword("fallback".into())),
        }
    });
    reg(i, "file-exists?", ArityHint::Exact(1), file_exists);
    reg(i, "delete-file", ArityHint::Exact(1), delete_file);
    // mova/PLAN.md interop-census batch: `db.clj`'s `atomic-move!` (cache
    // file swap) -- POSIX `rename(2)` IS the atomic move (replaces an
    // existing target, same-filesystem guarantee real Java's
    // `StandardCopyOption/ATOMIC_MOVE` also relies on), so no
    // `Files/move`/`CopyOption`/`AtomicMoveNotSupportedException` veneer
    // is needed -- `db.mova` overlay calls this directly instead.
    reg(i, "move-file", ArityHint::Exact(2), move_file);
    reg(i, "list-dir", ArityHint::Exact(1), list_dir);
    reg(i, "cwd", ArityHint::Exact(0), cwd);
    reg(i, "time-ms", ArityHint::Exact(0), time_ms);

    // ns: restrict qualified->bare fallback to clojure.core spellings
    // (DESIGN-flow-namespace.md item 5): `(System/getenv "HOME")` and
    // `(System/exit 0)` used to reach these bare fns purely through the
    // old unconditional trailing bare-name probe -- this module never
    // registered anything under "System"/"java.lang.System" at all. Real
    // entries under both class spellings, matching each fn's exact Java
    // name (`System.getenv`, `System.exit`); same fix shape as
    // `builtins::numbers`'s `Math/abs` alias. The rest of this module's
    // bare surface (`slurp`/`spit`/`sh`/`read-line`/`file-exists?`/
    // `delete-file`/`list-dir`/`cwd`) has no canonical `System/x` (or any
    // other JDK class) spelling to restore -- they're mova-native names,
    // not orphaned interop aliases. `time-ms` likewise isn't `System/
    // currentTimeMillis` under a different name: that static is already
    // registered for real, separately, by `builtins::statics`
    // (`system_current_time_millis`).
    crate::builtins::strings::alias(i, "System", "getenv");
    crate::builtins::strings::alias(i, "java.lang.System", "getenv");
    crate::builtins::strings::alias(i, "System", "exit");
    crate::builtins::strings::alias(i, "java.lang.System", "exit");
}

fn expect_str<'a>(v: &'a Value, who: &str) -> Result<&'a str, RjError> {
    match v {
        Value::Str(s) => Ok(s.as_ref()),
        other => Err(RjError::type_err(format!(
            "{who}: expected a string, got {}",
            other.type_name()
        ))),
    }
}

/// lsp/io (review round 2): `slurp`'s path argument, widened to accept
/// what `clojure.java.io/resource` returns -- a plain path string, a
/// `java.io.File` (`path_str_of`, same as `builtins::io`'s file natives
/// use), or a `"file://"`-prefixed resource URL (the scheme is stripped
/// before opening; see `builtins::io::resolve_resource`'s doc for why
/// `resource` returns that shape). A bare path with no scheme is
/// completely unaffected -- `strip_prefix` only fires on a literal
/// `file://` lead-in.
fn slurp_path_arg(v: &Value, who: &str) -> Result<String, RjError> {
    let raw = crate::hostclass::path_str_of(v).ok_or_else(|| {
        RjError::type_err(format!(
            "{who}: expected a string or java.io.File, got {}",
            v.type_name()
        ))
    })?;
    let raw = raw.as_ref().to_string();
    Ok(raw.strip_prefix("file://").map(str::to_string).unwrap_or(raw))
}

/// Turns a path string into a NUL-terminated `CString` for libc calls; a
/// path containing an interior NUL can never name a real file, so this is
/// reported the same way libc itself would (`EINVAL`) rather than panicking.
fn c_path(path: &str, syscall: &str) -> Result<CString, RjError> {
    CString::new(path).map_err(|_| {
        RjError::sys(
            format!("invalid path {path:?} (contains a NUL byte)"),
            libc::EINVAL,
            syscall,
        )
    })
}

/// Captures `errno` right after a failing libc call returns a sentinel
/// value; must be called before any other libc/syscall-adjacent code runs
/// so the thread-local `errno` hasn't been clobbered.
fn last_errno() -> i32 {
    // SAFETY: `__error()`/`errno` access is a plain thread-local read with
    // no aliasing or lifetime requirements; `std::io::Error::last_os_error`
    // wraps the same primitive portably, so we use it instead of poking
    // libc's errno directly.
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

// ---------------------------------------------------------------------
// slurp / spit
// ---------------------------------------------------------------------

/// Reads `path` fully via open(2)/fstat(2)/read(2)/close(2), UTF-8 decoding
/// losslessly (documented: invalid UTF-8 bytes become U+FFFD rather than
/// erroring, since mova strings are always valid `str`).
fn slurp(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    // lsp/kondo: `(slurp (.getInputStream jar entry))` -- an already-
    // open `HostKind::InputStream` (e.g. a jar entry's bytes) reads
    // straight through `stream_read_all`, no path/open(2) involved.
    if let Value::HostInst(h) = &args[0] {
        if h.kind == crate::hostclass::HostKind::InputStream {
            // real `slurp` closes what it reads (`with-open`)
            let all = crate::hostclass::stream_read_all(h)?;
            let _ = crate::hostclass::stream_close(h);
            return Ok(Value::Str(all));
        }
        // lsp/io (clj-kondo stdin campaign): `(slurp *in*)` when `*in*` is
        // bound (by `with-in-str`) to a `java.io.StringReader`/
        // `PushbackReader`/`LineNumberingPushbackReader` -- all three
        // share `HostKind::StringReader` (the wrapper ctors are identity
        // passthroughs, see `hostclass::construct`). Drains the reader's
        // remaining raw content in one shot, byte-exact.
        if h.kind == crate::hostclass::HostKind::StringReader {
            return crate::hostclass::slurp_string_reader(h);
        }
    }
    // accepts a `java.io.File`, bare string, or `file://`-prefixed
    // resource URL -- see `slurp_path_arg`'s doc (kept over the
    // kondo-wave `fileio::expect_path` variant: superset behavior).
    // e2: `(slurp "https://...")` / `(slurp (URL. ..))` -- the JVM's io/reader tries a URL first.
    if let Some(u) = crate::hostclass::url_str_of(&args[0])
        .or_else(|| match &args[0] { Value::Str(s) if crate::http::is_http_url(s) => Some(s.clone()), _ => None })
    {
        let stream = crate::hostclass::http_open_stream(&u)?;
        let Value::HostInst(h) = &stream else { unreachable!() };
        return Ok(Value::Str(crate::hostclass::stream_read_all(h)?));
    }
    let path = slurp_path_arg(&args[0], "slurp")?;
    let cpath = c_path(&path, "open")?;

    // SAFETY: `cpath` is a valid NUL-terminated C string owned by this
    // call; `open` either returns a valid fd (>= 0) or -1 with errno set.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        let errno = last_errno();
        return Err(RjError::sys(
            format!("couldn't slurp {path:?}"),
            errno,
            "open",
        ));
    }

    let size = {
        // SAFETY: `fd` was just returned as a valid, open descriptor and is
        // not touched by any other thread; `stat_buf` is fully overwritten
        // by a successful `fstat` before being read.
        let mut stat_buf: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd, &mut stat_buf) };
        if rc < 0 {
            let errno = last_errno();
            // SAFETY: `fd` is still the valid descriptor opened above.
            unsafe { libc::close(fd) };
            return Err(RjError::sys(
                format!("couldn't slurp {path:?}"),
                errno,
                "fstat",
            ));
        }
        stat_buf.st_size.max(0) as usize
    };

    let mut buf: Vec<u8> = Vec::with_capacity(size);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        // SAFETY: `fd` is a valid open descriptor for the lifetime of this
        // loop; `chunk` is a plain stack buffer sized exactly `chunk.len()`.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n < 0 {
            let errno = last_errno();
            // SAFETY: `fd` is still the valid descriptor opened above.
            unsafe { libc::close(fd) };
            return Err(RjError::sys(
                format!("couldn't slurp {path:?}"),
                errno,
                "read",
            ));
        }
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    // SAFETY: `fd` is still the valid descriptor opened above; closing it
    // exactly once here is the normal successful-path close.
    let rc = unsafe { libc::close(fd) };
    if rc < 0 {
        let errno = last_errno();
        return Err(RjError::sys(
            format!("couldn't slurp {path:?}"),
            errno,
            "close",
        ));
    }

    Ok(Value::Str(String::from_utf8_lossy(&buf).into_owned().into()))
}

/// Writes `contents` to `path` via open(O_WRONLY|O_CREAT|O_TRUNC,0644)/
/// write(2)/close(2), overwriting any existing file (or O_APPEND when a
/// trailing `:append true` option is given -- lsp/host: taoensso.timbre's
/// spit-appender shim calls `(spit fname s :append true)`). Returns `nil`.
fn spit(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    // kondo-wave: accepts a `java.io.File` too (not just a bare string) --
    // see `fileio::expect_path`'s doc.
    let path = crate::builtins::fileio::expect_path(&args[0], "spit")?;
    let path = path.as_ref();
    let contents = expect_str(&args[1], "spit")?;
    let cpath = c_path(path, "open")?;

    let mut append = false;
    let mut opts = args[2..].iter();
    while let (Some(k), Some(v)) = (opts.next(), opts.next()) {
        if matches!(k, Value::Keyword(kw) if kw.text().as_ref() == "append") {
            append = matches!(v, Value::Bool(true));
        }
    }
    let flags = libc::O_WRONLY | libc::O_CREAT | if append { libc::O_APPEND } else { libc::O_TRUNC };

    // SAFETY: `cpath` is a valid NUL-terminated C string owned by this
    // call; mode 0o644 only applies when O_CREAT actually creates the file.
    let fd = unsafe { libc::open(cpath.as_ptr(), flags, 0o644) };
    if fd < 0 {
        let errno = last_errno();
        return Err(RjError::sys(
            format!("couldn't spit to {path:?}"),
            errno,
            "open",
        ));
    }

    let bytes = contents.as_bytes();
    let mut written = 0usize;
    while written < bytes.len() {
        // SAFETY: `fd` is a valid open descriptor; the pointer+len slice
        // `bytes[written..]` stays within `contents`'s live allocation.
        let n = unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr().cast(),
                bytes.len() - written,
            )
        };
        if n < 0 {
            let errno = last_errno();
            // SAFETY: `fd` is still the valid descriptor opened above.
            unsafe { libc::close(fd) };
            return Err(RjError::sys(
                format!("couldn't spit to {path:?}"),
                errno,
                "write",
            ));
        }
        written += n as usize;
    }
    // SAFETY: `fd` is still the valid descriptor opened above; closing it
    // exactly once here is the normal successful-path close.
    let rc = unsafe { libc::close(fd) };
    if rc < 0 {
        let errno = last_errno();
        return Err(RjError::sys(
            format!("couldn't spit to {path:?}"),
            errno,
            "close",
        ));
    }

    Ok(Value::Nil)
}

// ---------------------------------------------------------------------
// env / process
// ---------------------------------------------------------------------

/// `(getenv)` (0-arg, real `System.getenv()`) returns the WHOLE
/// environment as a `Map<String,String>`; `(getenv name)` (1-arg, real
/// `System.getenv(String)`) looks up one var, `nil` if unset -- same
/// two overloads real `java.lang.System` has.
fn getenv(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match args.first() {
        None => {
            let mut m = PMap::new();
            for (k, v) in std::env::vars() {
                m.insert(Value::Str(k.into()), Value::Str(v.into()));
            }
            Ok(Value::Map(m))
        }
        Some(v) => {
            let name = expect_str(v, "getenv")?;
            match std::env::var(name) {
                Ok(v) => Ok(Value::Str(v.into())),
                Err(_) => Ok(Value::Nil),
            }
        }
    }
}

fn exit(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let code = match args.first() {
        None => 0,
        Some(Value::Int(n)) => *n as i32,
        Some(other) => {
            return Err(RjError::type_err(format!(
                "exit: expected an int exit code, got {}",
                other.type_name()
            )))
        }
    };
    std::process::exit(code);
}

/// Runs `(sh "cmd" "arg1" "arg2" ...)` via `std::process::Command`, returning
/// `{:exit n :out "..." :err "..."}`. stdout/stderr are decoded lossily
/// (same rationale as `slurp`). A spawn failure (e.g. command not found)
/// becomes a `Sys` error carrying the OS error code as errno.
/// mova campaign (clojure-lsp): a trailing `Value::Map` is the real
/// `clojure.java.shell/sh`'s options map (`:dir`/`:env`/...) --
/// `clojure_lsp.classpath/lookup-classpath!` passes `:dir root-path` so
/// the classpath-discovery subprocess runs IN THE PROJECT, not wherever
/// mova's own process happened to start (measured bug: silently running
/// in the wrong cwd made `clojure -A:dev:test -Spath` see a different/no
/// `deps.edn`, which cascaded into "Classpath lookup failed" and 0
/// analyzed files). Only `:dir` is implemented (the one option this
/// corpus's call sites use); `:env`/`:in`/others are accepted but
/// ignored, same "measured subset" scoping as `format`'s old doc.
fn sh(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let (opts, str_args) = match args.last() {
        Some(Value::Map(m)) => (Some(m.clone()), &args[..args.len() - 1]),
        _ => (None, args),
    };
    let cmd = expect_str(&str_args[0], "sh")?;
    let mut command = std::process::Command::new(cmd);
    for a in &str_args[1..] {
        command.arg(expect_str(a, "sh")?);
    }
    if let Some(m) = &opts {
        if let Some(Value::Str(dir)) = m.get(&Value::Keyword("dir".into())) {
            command.current_dir(dir.as_ref());
        }
    }
    // P0c: interruptible run: spawn, drain pipes on helper threads, poll
    // `try_wait` in 5 ms interruptible sleeps, kill the child on interrupt.
    use std::io::Read;
    let sys_err = |e: std::io::Error| {
        RjError::sys(
            format!("couldn't run {cmd:?}"),
            e.raw_os_error().unwrap_or(libc::ENOENT),
            "exec",
        )
    };
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(sys_err)?;
    let mut so = child.stdout.take().expect("piped");
    let mut se = child.stderr.take().expect("piped");
    let t_out = std::thread::spawn(move || { let mut b = Vec::new(); let _ = so.read_to_end(&mut b); b });
    let t_err = std::thread::spawn(move || { let mut b = Vec::new(); let _ = se.read_to_end(&mut b); b });
    let status = loop {
        match child.try_wait().map_err(sys_err)? {
            Some(st) => break st,
            None => {
                if let Err(e) = interp.intr.sleep(std::time::Duration::from_millis(5)) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
            }
        }
    };
    let output = std::process::Output {
        status,
        stdout: t_out.join().unwrap_or_default(),
        stderr: t_err.join().unwrap_or_default(),
    };

    let mut m = PMap::new();
    m.insert(
        Value::Keyword("exit".into()),
        Value::Int(output.status.code().unwrap_or(-1) as i64),
    );
    m.insert(
        Value::Keyword("out".into()),
        Value::Str(String::from_utf8_lossy(&output.stdout).into_owned().into()),
    );
    m.insert(
        Value::Keyword("err".into()),
        Value::Str(String::from_utf8_lossy(&output.stderr).into_owned().into()),
    );
    Ok(Value::Map(m))
}

/// Reads one line from stdin, stripping the trailing newline. Returns `nil`
/// on EOF (no bytes read at all).
///
/// lsp/io (clj-kondo stdin campaign): first checks whether `*in*` is
/// currently bound (by `with-in-str`) to a `HostKind::StringReader` --
/// same "read `*out*`'s current value" shape `builtins::strings::
/// out_write` uses for output, mirrored here for input. Any other `*in*`
/// value (nil, unbound, a non-StringReader) falls through to real stdin,
/// unchanged from pre-existing behavior.
fn read_line(interp: &mut Interp, _args: &[Value]) -> Result<Value, RjError> {
    if let Some(Value::HostInst(h)) = interp.globals.get(&Symbol::simple("*in*")) {
        if h.kind == crate::hostclass::HostKind::StringReader {
            return crate::hostclass::read_line_from_string_reader(&h);
        }
        // nREPL: the session's `*in*` is a host input stream (see `nrepl::input`).
        if h.kind == crate::hostclass::HostKind::InputStream {
            return Ok(match crate::hostclass::stream_read_line(&h)? {
                Some(l) => Value::Str(l),
                None => Value::Nil,
            });
        }
    }
    let mut line = String::new();
    let n = std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| RjError::sys("couldn't read from stdin", e.raw_os_error().unwrap_or(libc::EIO), "read"))?;
    if n == 0 {
        return Ok(Value::Nil);
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Value::Str(line.into()))
}

// ---------------------------------------------------------------------
// filesystem
// ---------------------------------------------------------------------

/// `access(2)` with `F_OK`: existence only, no error path -- ENOENT simply
/// means `false`.
fn file_exists(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    // kondo-wave: accepts a `java.io.File` too -- see `fileio::expect_path`.
    let path = crate::builtins::fileio::expect_path(&args[0], "file-exists?")?;
    let path = path.as_ref();
    let cpath = c_path(path, "access")?;
    // SAFETY: `cpath` is a valid NUL-terminated C string owned by this call;
    // `access` never dereferences beyond it.
    let rc = unsafe { libc::access(cpath.as_ptr(), libc::F_OK) };
    Ok(Value::Bool(rc == 0))
}

/// `unlink(2)`: errno-aware on failure, `true` on success.
/// `(move-file src dst)`: native atomic rename (`rename(2)`), replacing
/// `dst` if it exists. Both args accept a string or `java.io.File`, same
/// coercion as `delete-file` above.
fn move_file(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let src = crate::builtins::fileio::expect_path(&args[0], "move-file")?;
    let dst = crate::builtins::fileio::expect_path(&args[1], "move-file")?;
    let csrc = c_path(src.as_ref(), "rename")?;
    let cdst = c_path(dst.as_ref(), "rename")?;
    // SAFETY: both C strings are valid NUL-terminated, owned by this call.
    let rc = unsafe { libc::rename(csrc.as_ptr(), cdst.as_ptr()) };
    if rc != 0 {
        let errno = last_errno();
        return Err(RjError::sys(
            format!("couldn't move {src:?} to {dst:?}"),
            errno,
            "rename",
        ));
    }
    Ok(Value::Bool(true))
}

fn delete_file(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    // kondo-wave: accepts a `java.io.File` too -- see `fileio::expect_path`.
    let path = crate::builtins::fileio::expect_path(&args[0], "delete-file")?;
    let path = path.as_ref();
    let cpath = c_path(path, "unlink")?;
    // SAFETY: `cpath` is a valid NUL-terminated C string owned by this call.
    let rc = unsafe { libc::unlink(cpath.as_ptr()) };
    if rc != 0 {
        let errno = last_errno();
        return Err(RjError::sys(
            format!("couldn't delete {path:?}"),
            errno,
            "unlink",
        ));
    }
    Ok(Value::Bool(true))
}

/// Lists directory entry names (portable, via `std::fs::read_dir` rather
/// than raw `opendir`/`readdir` -- no authentic errno win over `std::fs`
/// here since `io::Error::raw_os_error()` already surfaces it), sorted for
/// deterministic output.
fn list_dir(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let path = expect_str(&args[0], "list-dir")?;
    let entries = std::fs::read_dir(path).map_err(|e| {
        RjError::sys(
            format!("couldn't list-dir {path:?}"),
            e.raw_os_error().unwrap_or(libc::ENOENT),
            "opendir",
        )
    })?;

    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            RjError::sys(
                format!("couldn't list-dir {path:?}"),
                e.raw_os_error().unwrap_or(libc::EIO),
                "readdir",
            )
        })?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(Value::Vector(names.into_iter().map(|n| Value::Str(n.into())).collect()))
}

fn cwd(_interp: &mut Interp, _args: &[Value]) -> Result<Value, RjError> {
    let dir = std::env::current_dir().map_err(|e| {
        RjError::sys(
            "couldn't get current working directory",
            e.raw_os_error().unwrap_or(libc::EIO),
            "getcwd",
        )
    })?;
    Ok(Value::Str(dir.to_string_lossy().into_owned().into()))
}

fn time_ms(_interp: &mut Interp, _args: &[Value]) -> Result<Value, RjError> {
    Ok(Value::Int(crate::clock::clock_epoch_ms() as i64))
}

#[cfg(test)]
mod move_file_tests {
    use super::*;

    #[test]
    fn move_file_renames_and_replaces_existing_target() {
        let dir = std::env::temp_dir().join(format!("mova-move-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.txt");
        let dst = dir.join("dst.txt");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        let mut interp = Interp::new();
        let result = move_file(
            &mut interp,
            &[
                Value::Str(src.to_string_lossy().into_owned().into()),
                Value::Str(dst.to_string_lossy().into_owned().into()),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Bool(true));
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new");
        std::fs::remove_dir_all(&dir).ok();
    }
}
