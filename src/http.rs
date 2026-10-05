//! e2: the one HTTP(S) request shape clojure-lsp needs (`clojure-lsp.http`, `(slurp "https://...")`):
//! a blocking GET that follows redirects, with JVM-style connect/read timeouts. Backed by `ureq` (rustls).

use std::io::Read;
use std::time::Duration;

/// A fetched response: status line, `Content-Type`, and the still-unread body stream.
pub struct HttpResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Box<dyn Read + Send>,
}

/// GET `url`. `Err` is the message of the `java.io.IOException` the JVM would throw (bad host, refused, timeout, TLS).
pub fn get(url: &str, connect_ms: Option<u64>, read_ms: Option<u64>) -> Result<HttpResponse, String> {
    let ms = |v: Option<u64>| v.filter(|n| *n > 0).map(Duration::from_millis);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(ms(connect_ms))
        .timeout_recv_response(ms(read_ms))
        .timeout_recv_body(ms(read_ms))
        .build()
        .into();
    let resp = agent.get(url).call().map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
    Ok(HttpResponse { status, content_type, body })
}

/// True for the strings `slurp`/`io/reader` must treat as URLs rather than file paths.
pub fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}
