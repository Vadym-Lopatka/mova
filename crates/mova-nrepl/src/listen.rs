//! Binding the listening socket and the startup banner (design 5.8).

use std::io;
use std::net::TcpListener;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

/// Where to listen.
#[derive(Clone, Debug)]
pub enum Endpoint {
    /// `--bind` and `--port`. Port 0 picks a free one.
    Tcp { host: String, port: u16 },
    /// `--socket`: a Unix domain socket at this path.
    Unix(PathBuf),
}

/// A bound, listening, non-blocking socket. Creating it is the first thing
/// startup does; connections wait in the kernel backlog until the IO loop runs.
pub struct Listeners {
    pub(crate) tcp: Option<TcpListener>,
    pub(crate) unix: Option<UnixListener>,
    host: String,
    port: u16,
    path: Option<PathBuf>,
}

impl Listeners {
    pub fn bind(ep: &Endpoint) -> io::Result<Listeners> {
        match ep {
            Endpoint::Tcp { host, port } => {
                let l = TcpListener::bind((host.as_str(), *port))?;
                l.set_nonblocking(true)?;
                let port = l.local_addr()?.port();
                Ok(Listeners { tcp: Some(l), unix: None, host: host.clone(), port, path: None })
            }
            Endpoint::Unix(path) => {
                // A stale socket file from a dead server would make bind fail.
                if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
                    let _ = std::fs::remove_file(path);
                }
                let l = UnixListener::bind(path)?;
                l.set_nonblocking(true)?;
                Ok(Listeners { tcp: None, unix: Some(l), host: String::new(), port: 0, path: Some(path.clone()) })
            }
        }
    }

    /// The port actually bound (TCP only).
    pub fn port(&self) -> Option<u16> {
        self.tcp.as_ref().map(|_| self.port)
    }

    pub fn socket_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The line the JVM prints for the default transport, byte for byte
    /// (without the newline).
    pub fn banner(&self) -> String {
        self.banner_with("nrepl", false)
    }

    /// The same for another transport: `scheme` is `Codec::uri_scheme`; `tls`
    /// appends the `s` (`nrepls://`) as `nrepl.socket/as-nrepl-uri` does.
    pub fn banner_with(&self, scheme: &str, tls: bool) -> String {
        match &self.path {
            Some(p) => {
                // The JVM prints the absolute path (cwd + "/" + path as given, not normalized).
                let abs = if p.is_absolute() {
                    p.clone()
                } else {
                    std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.clone())
                };
                format!("nREPL server started on socket {scheme}+unix:{}", uri_path(&abs.to_string_lossy()))
            }
            None => {
                let uri_host = if self.host.contains(':') && !self.host.starts_with('[') {
                    format!("[{}]", self.host)
                } else {
                    self.host.clone()
                };
                let s = if tls { "s" } else { "" };
                format!(
                    "nREPL server started on port {} on host {} - {scheme}{s}://{}:{}",
                    self.port, self.host, uri_host, self.port
                )
            }
        }
    }
}

impl Drop for Listeners {
    fn drop(&mut self) {
        if let Some(p) = &self.path {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Percent-encodes what `java.net.URI` would quote in a path.
fn uri_path(p: &str) -> String {
    let mut s = String::with_capacity(p.len());
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => s.push(b as char),
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' | b'/' | b':' | b'@' | b'&' | b'='
            | b'+' | b'$' | b',' | b';' => s.push(b as char),
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_banner() {
        let l = Listeners::bind(&Endpoint::Tcp { host: "127.0.0.1".into(), port: 0 }).unwrap();
        let p = l.port().unwrap();
        assert_ne!(p, 0);
        assert_eq!(
            l.banner(),
            format!("nREPL server started on port {p} on host 127.0.0.1 - nrepl://127.0.0.1:{p}")
        );
    }

    #[test]
    fn unix_banner_and_stale_socket() {
        let dir = std::env::temp_dir().join(format!("mova-nrepl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a b.sock");
        let l = Listeners::bind(&Endpoint::Unix(path.clone())).unwrap();
        assert_eq!(l.banner(), format!("nREPL server started on socket nrepl+unix:{}/a%20b.sock", dir.display()));
        // a crash leaves the file behind: make one by hand
        drop(l);
        std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(path.exists());
        Listeners::bind(&Endpoint::Unix(path)).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
