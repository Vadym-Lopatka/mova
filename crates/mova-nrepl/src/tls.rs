//! TLS, behind the `tls` cargo feature (`--tls-keys-file`, `--tls-keys-str`).
//!
//! Same rules as `nrepl.tls`:
//!
//! * The key material is PEM text with the CA certificate first, the own
//!   certificate second, and an unencrypted PKCS#8 private key.
//! * TLS 1.3 only. Mutual authentication: the server requires a client
//!   certificate signed by the CA; the client checks the server's the same way
//!   (not the host name, as the JVM client does not).
//! * A client that presents the server's own certificate is refused
//!   ("Cannot use same keys as server").
//!
//! Not done: the 30 s handshake timeout of the JVM (a server with no timers
//! cannot have one; a stalled handshake holds one connection slot and no CPU),
//! and the swapped-order detection of the two certificates.
//!
//! `TlsStream` is a non-blocking `rustls` server connection over a `TcpStream`,
//! driven by the IO thread: `read` and `write` give plain bytes and never
//! block; `wants_write` and `flush_out` move the encrypted bytes.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

/// Client and server configuration made from one key file.
pub struct TlsConfig {
    server: Arc<ServerConfig>,
    client: Arc<ClientConfig>,
    own: Vec<u8>,
}

fn b64(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return Err("bad base64 in PEM".into()),
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// The base64 payloads of every `-----BEGIN <label>-----` block.
fn blocks(text: &str, label: &str) -> Vec<String> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut res = Vec::new();
    let mut cur: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t == begin {
            cur = Some(String::new());
        } else if t == end {
            if let Some(c) = cur.take() {
                res.push(c);
            }
        } else if let Some(c) = cur.as_mut() {
            c.push_str(t);
        }
    }
    res
}

const KEY_HELP: &str = "The TLS keys material must contain the CA certificate, your own certificate and an unencrypted PKCS#8 private key (a `-----BEGIN PRIVATE KEY-----` block) in PEM format.";

impl TlsConfig {
    /// Builds the contexts from key material (the contents of a keys file).
    pub fn from_keys(text: &str) -> Result<TlsConfig, String> {
        let certs = blocks(text, "CERTIFICATE");
        match certs.len() {
            0 => return Err(format!("No certificates found. {KEY_HELP}")),
            1 => {
                return Err("Only one certificate found, but the TLS keys material must contain both the CA certificate and your own certificate.".into())
            }
            _ => {}
        }
        let ca = b64(&certs[0])?;
        let own = b64(&certs[1])?;
        let key = match blocks(text, "PRIVATE KEY").first() {
            Some(k) => ec::complete_pkcs8(b64(k)?),
            None if text.contains("ENCRYPTED PRIVATE KEY") => {
                return Err("The private key is password-protected, which is not supported. Decrypt it first, e.g. `openssl pkey -in encrypted-key.pem -out key.pem`.".into())
            }
            None if text.contains("PRIVATE KEY") => {
                return Err("The private key is not an unencrypted PKCS#8 key. Convert it with `openssl pkcs8 -topk8 -nocrypt -in key.pem -out key-pkcs8.pem`.".into())
            }
            None => return Err(format!("No private key found. {KEY_HELP}")),
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(ca)).map_err(|e| format!("bad CA certificate: {e}"))?;
        let roots = Arc::new(roots);
        let key = || PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.clone()));
        let own_der = CertificateDer::from(own.clone());

        let client_verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(roots.clone(), provider.clone())
            .build()
            .map_err(|e| e.to_string())?;
        let server = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| e.to_string())?
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(vec![own_der.clone()], key())
            .map_err(|e| format!("bad key or certificate: {e}"))?;

        let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(roots, provider.clone())
            .build()
            .map_err(|e| e.to_string())?;
        let client = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoHostName(inner)))
            .with_client_auth_cert(vec![own_der], key())
            .map_err(|e| format!("bad key or certificate: {e}"))?;
        Ok(TlsConfig { server: Arc::new(server), client: Arc::new(client), own })
    }

    /// Reads a keys file, or uses the string. The JVM error texts are kept.
    pub fn load(file: Option<&str>, string: Option<&str>) -> Result<TlsConfig, String> {
        match (file, string) {
            (Some(f), _) => {
                let text = std::fs::read_to_string(f).map_err(|_| format!(":tls-keys-file specified as {f}, but the file was not found."))?;
                TlsConfig::from_keys(&text).map_err(|e| format!("Could not create TLS context from file {f}. Error message: {e}"))
            }
            (None, Some(s)) => TlsConfig::from_keys(s).map_err(|e| format!("Could not create TLS context from string. Error message: {e}")),
            _ => Err("Could not create TLS context. Neither :tls-keys-str nor :tls-keys-file given.".into()),
        }
    }

    /// Client side, blocking: connects and completes the handshake.
    pub fn connect(&self, host: &str, port: u16) -> io::Result<Box<dyn crate::client::Stream>> {
        let sock = TcpStream::connect((host, port))?;
        let name = ServerName::try_from(host.to_string()).map_err(|e| io::Error::other(e.to_string()))?;
        let conn = ClientConnection::new(self.client.clone(), name).map_err(|e| io::Error::other(e.to_string()))?;
        let mut s = rustls::StreamOwned::new(conn, sock);
        // finish the handshake now so errors show up at connect time
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock).map_err(|e| io::Error::other(format!("TLS handshake failed: {e}")))?;
        }
        Ok(Box::new(ClientTls(s)))
    }
}

struct ClientTls(rustls::StreamOwned<ClientConnection, TcpStream>);

impl Read for ClientTls {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}
impl Write for ClientTls {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
impl crate::client::Stream for ClientTls {
    fn set_read_timeout(&mut self, d: Option<std::time::Duration>) -> io::Result<()> {
        self.0.sock.set_read_timeout(d)
    }
}

/// The JVM client checks the chain against the CA but not the host name.
#[derive(Debug)]
struct NoHostName(Arc<rustls::client::WebPkiServerVerifier>);

impl rustls::client::danger::ServerCertVerifier for NoHostName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName | rustls::CertificateError::NotValidForNameContext { .. },
            )) => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            r => r,
        }
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(m, c, d)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// A server-side connection driven by the IO thread.
pub struct TlsStream {
    sock: TcpStream,
    conn: ServerConnection,
    own: Vec<u8>,
    checked: bool,
}

impl TlsStream {
    pub fn accept(sock: TcpStream, cfg: &Arc<TlsConfig>) -> io::Result<TlsStream> {
        let conn = ServerConnection::new(cfg.server.clone()).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(TlsStream { sock, conn, own: cfg.own.clone(), checked: false })
    }

    pub fn set_nonblocking(&self) -> bool {
        self.sock.set_nonblocking(true).is_ok() && self.sock.set_nodelay(true).is_ok()
    }

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn wants_write(&self) -> bool {
        self.conn.wants_write()
    }

    /// Writes pending encrypted bytes until the socket is full.
    pub fn flush_out(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            match self.conn.write_tls(&mut self.sock) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Plain bytes in. `WouldBlock` when none are ready; `Ok(0)` at the end.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.conn.reader().read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                // peer closed without close_notify: treat as the end
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) => return Err(e),
            }
            match self.conn.read_tls(&mut self.sock) {
                Ok(0) => return Ok(0),
                Ok(_) => {
                    let st = self.conn.process_new_packets();
                    // handshake answers (and alerts on failure) must go out even on error
                    let _ = self.flush_out();
                    st.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("TLS: {e}")))?;
                    if !self.checked && !self.conn.is_handshaking() {
                        self.checked = true;
                        let same = self.conn.peer_certificates().and_then(|c| c.first()).is_some_and(|c| c.as_ref() == self.own.as_slice());
                        if same {
                            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Cannot use same keys as server"));
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Plain bytes out: always accepted (encrypted into the connection's buffer).
    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.conn.writer().write_all(buf)?;
        self.flush_out()?;
        Ok(buf.len())
    }
}

/// Java writes EC private keys as PKCS#8 without the public key; `ring` (the
/// crypto of rustls here) insists on it. This adds it (P-256 only), so keys
/// made by `keytool`, BouncyCastle and `nrepl.tls`'s own test tools load.
/// Other keys are returned unchanged.
mod ec {
    // ---- DER ----
    fn tlv(buf: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let tag = *buf.first()?;
        let l = *buf.get(1)? as usize;
        let (len, hdr) = if l < 0x80 {
            (l, 2)
        } else {
            let n = l & 0x7f;
            if n == 0 || n > 3 {
                return None;
            }
            let mut v = 0usize;
            for i in 0..n {
                v = (v << 8) | *buf.get(2 + i)? as usize;
            }
            (v, 2 + n)
        };
        let body = buf.get(hdr..hdr + len)?;
        Some((tag, body, &buf[hdr + len..]))
    }

    fn put(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut o = vec![tag];
        if body.len() < 0x80 {
            o.push(body.len() as u8);
        } else if body.len() < 0x100 {
            o.extend_from_slice(&[0x81, body.len() as u8]); // DER: shortest form
        } else {
            o.extend_from_slice(&[0x82, (body.len() >> 8) as u8, body.len() as u8]);
        }
        o.extend_from_slice(body);
        o
    }

    const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];

    pub fn complete_pkcs8(der: Vec<u8>) -> Vec<u8> {
        rebuild(&der).unwrap_or(der)
    }

    fn rebuild(der: &[u8]) -> Option<Vec<u8>> {
        let (0x30, info, _) = tlv(der)? else { return None };
        let (0x02, _ver, rest) = tlv(info)? else { return None };
        let (0x30, alg, rest) = tlv(rest)? else { return None };
        let (0x06, oid, alg_rest) = tlv(alg)? else { return None };
        let (0x06, curve, _) = tlv(alg_rest)? else { return None };
        if oid != OID_EC || curve != OID_P256 {
            return None;
        }
        let (0x04, inner, _) = tlv(rest)? else { return None };
        let (0x30, ecpk, _) = tlv(inner)? else { return None };
        let (0x02, _v, rest) = tlv(ecpk)? else { return None };
        let (0x04, d, rest) = tlv(rest)? else { return None };
        if !rest.is_empty() || d.len() > 32 {
            return None; // already has parameters or a public key
        }
        let mut dd = [0u8; 32];
        dd[32 - d.len()..].copy_from_slice(d);
        let q = p256_public(&dd)?;
        let mut point = vec![0u8, 4];
        point.extend_from_slice(&q);
        let mut ecpk = put(0x02, &[1]);
        ecpk.extend(put(0x04, &dd));
        ecpk.extend(put(0xa1, &put(0x03, &point)));
        let mut alg = put(0x06, OID_EC);
        alg.extend(put(0x06, OID_P256));
        let mut out = put(0x02, &[0]);
        out.extend(put(0x30, &alg));
        out.extend(put(0x04, &put(0x30, &ecpk)));
        Some(put(0x30, &out))
    }

    // ---- P-256: Q = d * G (once, at start-up; not constant time, not needed here) ----
    type U = [u64; 4]; // little endian limbs

    const P: U = [0xffff_ffff_ffff_ffff, 0x0000_0000_ffff_ffff, 0, 0xffff_ffff_0000_0001];
    const GX: U = [0xf4a1_3945_d898_c296, 0x7703_7d81_2deb_33a0, 0xf8bc_e6e5_63a4_40f2, 0x6b17_d1f2_e12c_4247];
    const GY: U = [0xcbb6_4068_37bf_51f5, 0x2bce_3357_6b31_5ece, 0x8ee7_eb4a_7c0f_9e16, 0x4fe3_42e2_fe1a_7f9b];

    fn geq(a: &U, b: &U) -> bool {
        for i in (0..4).rev() {
            if a[i] != b[i] {
                return a[i] > b[i];
            }
        }
        true
    }
    fn sub_raw(a: &U, b: &U) -> (U, bool) {
        let mut r = [0u64; 4];
        let mut borrow = false;
        for i in 0..4 {
            let (x, b1) = a[i].overflowing_sub(b[i]);
            let (y, b2) = x.overflowing_sub(borrow as u64);
            r[i] = y;
            borrow = b1 || b2;
        }
        (r, borrow)
    }
    fn add_mod(a: &U, b: &U) -> U {
        let mut r = [0u64; 4];
        let mut carry = false;
        for i in 0..4 {
            let (x, c1) = a[i].overflowing_add(b[i]);
            let (y, c2) = x.overflowing_add(carry as u64);
            r[i] = y;
            carry = c1 || c2;
        }
        if carry || geq(&r, &P) {
            r = sub_raw(&r, &P).0;
        }
        r
    }
    fn sub_mod(a: &U, b: &U) -> U {
        let (r, borrow) = sub_raw(a, b);
        if borrow {
            // add P back (wraps correctly in 256 bits)
            let mut o = [0u64; 4];
            let mut carry = false;
            for i in 0..4 {
                let (x, c1) = r[i].overflowing_add(P[i]);
                let (y, c2) = x.overflowing_add(carry as u64);
                o[i] = y;
                carry = c1 || c2;
            }
            o
        } else {
            r
        }
    }
    fn mul_mod(a: &U, b: &U) -> U {
        let mut r = [0u64; 4];
        for i in (0..4).rev() {
            for bit in (0..64).rev() {
                r = add_mod(&r, &r);
                if (b[i] >> bit) & 1 == 1 {
                    r = add_mod(&r, a);
                }
            }
        }
        r
    }
    fn is_zero(a: &U) -> bool {
        a.iter().all(|&x| x == 0)
    }
    fn inv(a: &U) -> U {
        // a^(p-2)
        let e = sub_raw(&P, &[2, 0, 0, 0]).0;
        let mut r = [1u64, 0, 0, 0];
        for i in (0..4).rev() {
            for bit in (0..64).rev() {
                r = mul_mod(&r, &r);
                if (e[i] >> bit) & 1 == 1 {
                    r = mul_mod(&r, a);
                }
            }
        }
        r
    }

    /// Jacobian point; `z == 0` is the point at infinity.
    #[derive(Clone, Copy)]
    struct Pt {
        x: U,
        y: U,
        z: U,
    }

    fn double(p: &Pt) -> Pt {
        if is_zero(&p.z) || is_zero(&p.y) {
            return Pt { x: [1, 0, 0, 0], y: [1, 0, 0, 0], z: [0; 4] };
        }
        let zz = mul_mod(&p.z, &p.z);
        let m0 = mul_mod(&sub_mod(&p.x, &zz), &add_mod(&p.x, &zz));
        let m = add_mod(&add_mod(&m0, &m0), &m0);
        let yy = mul_mod(&p.y, &p.y);
        let s0 = mul_mod(&p.x, &yy);
        let s = add_mod(&add_mod(&s0, &s0), &add_mod(&s0, &s0));
        let x3 = sub_mod(&mul_mod(&m, &m), &add_mod(&s, &s));
        let y4 = mul_mod(&yy, &yy);
        let y4_2 = add_mod(&y4, &y4);
        let y4_4 = add_mod(&y4_2, &y4_2);
        let y4_8 = add_mod(&y4_4, &y4_4);
        let y3 = sub_mod(&mul_mod(&m, &sub_mod(&s, &x3)), &y4_8);
        let yz = mul_mod(&p.y, &p.z);
        Pt { x: x3, y: y3, z: add_mod(&yz, &yz) }
    }

    /// `p + (GX, GY)`.
    fn add_g(p: &Pt) -> Pt {
        if is_zero(&p.z) {
            return Pt { x: GX, y: GY, z: [1, 0, 0, 0] };
        }
        let z1z1 = mul_mod(&p.z, &p.z);
        let u2 = mul_mod(&GX, &z1z1);
        let s2 = mul_mod(&GY, &mul_mod(&p.z, &z1z1));
        let h = sub_mod(&u2, &p.x);
        let r = sub_mod(&s2, &p.y);
        if is_zero(&h) {
            return if is_zero(&r) { double(p) } else { Pt { x: [1, 0, 0, 0], y: [1, 0, 0, 0], z: [0; 4] } };
        }
        let hh = mul_mod(&h, &h);
        let hhh = mul_mod(&h, &hh);
        let v = mul_mod(&p.x, &hh);
        let x3 = sub_mod(&sub_mod(&mul_mod(&r, &r), &hhh), &add_mod(&v, &v));
        let y3 = sub_mod(&mul_mod(&r, &sub_mod(&v, &x3)), &mul_mod(&p.y, &hhh));
        Pt { x: x3, y: y3, z: mul_mod(&p.z, &h) }
    }

    fn be(b: &[u8; 32]) -> U {
        let mut u = [0u64; 4];
        for i in 0..4 {
            let mut w = [0u8; 8];
            w.copy_from_slice(&b[32 - 8 * (i + 1)..32 - 8 * i]);
            u[i] = u64::from_be_bytes(w);
        }
        u
    }
    fn to_be(u: &U) -> [u8; 32] {
        let mut b = [0u8; 32];
        for i in 0..4 {
            b[32 - 8 * (i + 1)..32 - 8 * i].copy_from_slice(&u[i].to_be_bytes());
        }
        b
    }

    /// `X || Y` of `d * G`, or `None` if `d` is zero.
    pub fn p256_public(d: &[u8; 32]) -> Option<[u8; 64]> {
        let k = be(d);
        if is_zero(&k) {
            return None;
        }
        let mut r = Pt { x: [1, 0, 0, 0], y: [1, 0, 0, 0], z: [0; 4] };
        for i in (0..4).rev() {
            for bit in (0..64).rev() {
                r = double(&r);
                if (k[i] >> bit) & 1 == 1 {
                    r = add_g(&r);
                }
            }
        }
        if is_zero(&r.z) {
            return None;
        }
        let zi = inv(&r.z);
        let zi2 = mul_mod(&zi, &zi);
        let x = mul_mod(&r.x, &zi2);
        let y = mul_mod(&r.y, &mul_mod(&zi2, &zi));
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&to_be(&x));
        out[32..].copy_from_slice(&to_be(&y));
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn hex(s: &str) -> Vec<u8> {
            (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
        }

        #[test]
        fn java_style_key_is_completed_and_loads() {
            // a key made by Java (PKCS#8 EC without the public key), from `locksmith/gen-certs`
            let java = hex("3041020100301306072a8648ce3d020106082a8648ce3d030107042730250201010420ce1997af5fd09078c8c300f535ce07d4940e6895ee52d4047ea0efaece2c17c1");
            let full = complete_pkcs8(java.clone());
            assert_ne!(full, java);
            let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(full));
            rustls::crypto::ring::default_provider().key_provider.load_private_key(key).expect("ring accepts it");
        }

        #[test]
        fn public_key_matches_openssl() {
            // from `openssl ecparam -name prime256v1 -genkey`
            let d: [u8; 32] = hex("796204ff829310e3c11ce9bcbc7de3b3a9fb5d2768027f1a16af5571ef21e8c0").try_into().unwrap();
            let q = p256_public(&d).unwrap();
            assert_eq!(
                q.to_vec(),
                hex("40ba17463aa85edfb228c86a06c2bbe44746a9c23fe28ea1cd6a77119f9df4674b0e109e18003bbdeda4c3fc1a91ab588e723eda6597a9ea2dd38d13198500de")
            );
            // 1 * G
            let mut one = [0u8; 32];
            one[31] = 1;
            assert_eq!(p256_public(&one).unwrap()[..32], to_be(&GX));
            // 2 * G
            let mut two = [0u8; 32];
            two[31] = 2;
            assert_eq!(
                p256_public(&two).unwrap()[..32],
                hex("7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc47669978")[..]
            );
        }
    }
}
