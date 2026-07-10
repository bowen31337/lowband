//! Signaling client — the peer side of the rendezvous (FR-1).
//!
//! [`SignalingClient`] speaks the same `/signal/*` HTTP API the server in this
//! crate exposes, so a technician or assisted peer can turn a 9-digit code
//! into an offer/answer/ICE exchange and TURN credentials.  It is a small
//! blocking HTTP/1.1 client over [`std::net::TcpStream`] — no async runtime,
//! no TLS, no extra dependencies — which keeps it usable from inside the
//! `lowbandd` daemon on every target (including musl).
//!
//! Signaling is plaintext SDP/ICE brokering only; the media path is secured
//! separately by the Noise-IK handshake
//! ([`lowband_crypto`](../../lowband_crypto/index.html)) once the peers hold
//! each other's transport address.
//!
//! # Flow
//!
//! ```text
//! technician                         assisted user
//! ──────────                         ─────────────
//! create_session()  ── code ─────►   (out of band: phone/SMS)
//! post_offer(code, sdp)
//! post_candidate(code, cand)*
//!                                     join(code) → offer + candidates
//!                                     post_answer(code, sdp)
//!                                     post_candidate(code, cand)*
//! poll_answer(code) → sdp
//! (both) mark_connected(code)  ── evicts the session ──►
//! ```

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// TURN credential returned by [`SignalingClient::turn_credential`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCredential {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
    pub ttl_secs: u64,
}

/// What the joining peer receives from [`SignalingClient::join`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoinInfo {
    /// The offerer's SDP, if it has been posted yet.
    pub offer: Option<String>,
    /// ICE candidates the offerer has published so far.
    pub candidates: Vec<String>,
}

/// Errors from a signaling exchange.
#[derive(Debug)]
pub enum ClientError {
    /// Transport-level failure (connect/read/write).
    Io(io::Error),
    /// The server returned a non-success status (e.g. 404 for a dead code).
    Status(u16),
    /// The response body could not be parsed as expected.
    Malformed(&'static str),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(e) => write!(f, "signaling io error: {e}"),
            ClientError::Status(s) => write!(f, "signaling server returned status {s}"),
            ClientError::Malformed(what) => write!(f, "malformed signaling response: {what}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

type Result<T> = std::result::Result<T, ClientError>;

/// Blocking signaling client bound to one rendezvous server.
pub struct SignalingClient {
    host_header: String,
    addr: std::net::SocketAddr,
    timeout: Duration,
}

impl SignalingClient {
    /// Connect-on-demand client for the server reachable at `addr`
    /// (e.g. `"signal.lowband.dev:443"` or `"127.0.0.1:8080"`).
    ///
    /// The address is resolved once here; each request opens a fresh
    /// connection (the signaling exchange is a handful of short requests).
    pub fn connect(addr: impl ToSocketAddrs, host_header: impl Into<String>) -> Result<Self> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or(ClientError::Malformed("address resolved to nothing"))?;
        Ok(Self {
            host_header: host_header.into(),
            addr,
            timeout: Duration::from_secs(10),
        })
    }

    /// Override the per-request connect/read timeout (default 10s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Create a new session; returns the 9-digit join code (technician side).
    pub fn create_session(&self) -> Result<String> {
        let (status, body) = self.request("POST", "/signal/session", None)?;
        expect(status, 201)?;
        json_string_field(&body, "session_code")
            .ok_or(ClientError::Malformed("missing session_code"))
    }

    /// Publish the offerer's SDP for `code`.
    pub fn post_offer(&self, code: &str, sdp: &str) -> Result<()> {
        let body = json_obj(&[("session_code", code), ("sdp", sdp)]);
        let (status, _) = self.request("POST", "/signal/offer", Some(&body))?;
        expect(status, 200)
    }

    /// Publish the joiner's answer SDP for `code`.
    pub fn post_answer(&self, code: &str, sdp: &str) -> Result<()> {
        let body = json_obj(&[("session_code", code), ("sdp", sdp)]);
        let (status, _) = self.request("POST", "/signal/answer", Some(&body))?;
        expect(status, 200)
    }

    /// Publish one ICE candidate for `code`.
    pub fn post_candidate(&self, code: &str, candidate: &str) -> Result<()> {
        let body = json_obj(&[("session_code", code), ("candidate", candidate)]);
        let (status, _) = self.request("POST", "/signal/candidate", Some(&body))?;
        expect(status, 202)
    }

    /// Fetch the offer and candidates for `code` (joiner side).
    ///
    /// Returns `Err(Status(404))` when the code is unknown or expired.
    pub fn join(&self, code: &str) -> Result<JoinInfo> {
        let path = format!("/signal/join/{code}");
        let (status, body) = self.request("GET", &path, None)?;
        expect(status, 200)?;
        Ok(JoinInfo {
            offer: json_string_field(&body, "offer"),
            candidates: json_string_array_field(&body, "candidates"),
        })
    }

    /// Poll for the joiner's answer (offerer side).
    ///
    /// `Ok(None)` means the answer has not been posted yet — poll again.
    pub fn poll_answer(&self, code: &str) -> Result<Option<String>> {
        let path = format!("/signal/answer/{code}");
        let (status, body) = self.request("GET", &path, None)?;
        expect(status, 200)?;
        Ok(json_string_field(&body, "answer"))
    }

    /// Request a short-lived TURN credential.
    pub fn turn_credential(&self) -> Result<TurnCredential> {
        let (status, body) = self.request("POST", "/signal/turn", None)?;
        expect(status, 200)?;
        Ok(TurnCredential {
            urls: json_string_array_field(&body, "urls"),
            username: json_string_field(&body, "username")
                .ok_or(ClientError::Malformed("missing username"))?,
            credential: json_string_field(&body, "credential")
                .ok_or(ClientError::Malformed("missing credential"))?,
            ttl_secs: json_u64_field(&body, "ttl_secs").unwrap_or(0),
        })
    }

    /// Signal that a direct connection was established; the server evicts the
    /// session and leaves the media path.
    pub fn mark_connected(&self, code: &str) -> Result<()> {
        let body = json_obj(&[("session_code", code)]);
        let (status, _) = self.request("POST", "/signal/connected", Some(&body))?;
        expect(status, 200)
    }

    // ── HTTP/1.1 over a fresh TCP connection ──────────────────────────────

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
        let mut stream = TcpStream::connect_timeout(&self.addr, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.host_header
        );
        if let Some(b) = body {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");
        if let Some(b) = body {
            req.push_str(b);
        }
        stream.write_all(req.as_bytes())?;
        stream.flush()?;

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        parse_response(&raw)
    }
}

/// Parse a `Connection: close` HTTP/1.1 response into (status, body).
fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let split = find_header_end(raw).ok_or(ClientError::Malformed("no header terminator"))?;
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| ClientError::Malformed("non-utf8 headers"))?;
    let body = &raw[split + 4..];

    let status_line = head.lines().next().ok_or(ClientError::Malformed("no status line"))?;
    // "HTTP/1.1 200 OK"
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(ClientError::Malformed("no status code"))?;

    let body = if header_is_chunked(head) {
        dechunk(body)?
    } else {
        String::from_utf8(body.to_vec()).map_err(|_| ClientError::Malformed("non-utf8 body"))?
    };
    Ok((code, body))
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

fn header_is_chunked(head: &str) -> bool {
    head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    })
}

/// Decode HTTP/1.1 chunked transfer encoding into the concatenated body.
fn dechunk(mut body: &[u8]) -> Result<String> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or(ClientError::Malformed("chunk size line"))?;
        let size_str = std::str::from_utf8(&body[..line_end])
            .map_err(|_| ClientError::Malformed("chunk size utf8"))?
            .trim();
        // Chunk extensions (after ';') are ignored.
        let size_hex = size_str.split(';').next().unwrap_or("");
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| ClientError::Malformed("chunk size hex"))?;
        body = &body[line_end + 2..];
        if size == 0 {
            break;
        }
        if body.len() < size {
            return Err(ClientError::Malformed("truncated chunk"));
        }
        out.extend_from_slice(&body[..size]);
        // Skip the chunk's trailing CRLF.
        body = &body[size..];
        if body.starts_with(b"\r\n") {
            body = &body[2..];
        }
    }
    String::from_utf8(out).map_err(|_| ClientError::Malformed("non-utf8 body"))
}

fn expect(status: u16, want: u16) -> Result<()> {
    if status == want {
        Ok(())
    } else {
        Err(ClientError::Status(status))
    }
}

// ── Minimal JSON helpers ──────────────────────────────────────────────────
//
// The signaling responses are tiny, flat objects; a targeted extractor is
// smaller and dependency-free versus pulling serde into the client path.

/// Serialize a flat object of string fields, escaping values.
fn json_obj(fields: &[(&str, &str)]) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("\"{k}\":\"{}\"", json_escape(v)));
    }
    s.push('}');
    s
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Extract a string field's value, returning `None` for a missing key or a
/// JSON `null`.  Handles the escape sequences `json_escape` produces.
fn json_string_field(body: &str, key: &str) -> Option<String> {
    let at = field_value_start(body, key)?;
    let rest = &body[at..];
    if rest.starts_with("null") {
        return None;
    }
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '/' => out.push('/'),
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

fn json_u64_field(body: &str, key: &str) -> Option<u64> {
    let at = field_value_start(body, key)?;
    let end = body[at..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| at + i)
        .unwrap_or(body.len());
    body[at..end].parse().ok()
}

/// Extract a `["a","b"]` string array field (used for candidates and urls).
fn json_string_array_field(body: &str, key: &str) -> Vec<String> {
    let Some(at) = field_value_start(body, key) else {
        return Vec::new();
    };
    let rest = &body[at..];
    if !rest.starts_with('[') {
        return Vec::new();
    }
    let end = match rest.find(']') {
        Some(e) => e,
        None => return Vec::new(),
    };
    let inner = &rest[1..end];
    let mut out = Vec::new();
    let mut chars = inner.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '"' {
            // Reuse the scalar extractor from this opening quote.
            if let Some(s) = json_string_field(&format!("\"x\":{}", &inner[i..]), "x") {
                let consumed = s.len();
                out.push(s);
                // Advance past the closing quote of this element.
                for (j, cc) in inner[i + 1..].char_indices() {
                    if cc == '"' && j >= consumed.saturating_sub(1) {
                        while let Some(&(k, _)) = chars.peek() {
                            if k <= i + 1 + j {
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
    out
}

/// Byte offset just after `"key":` (and any following whitespace).
fn field_value_start(body: &str, key: &str) -> Option<usize> {
    let needle = format!("\"{key}\"");
    let mut from = 0;
    while let Some(rel) = body[from..].find(&needle) {
        let after_key = from + rel + needle.len();
        let after = body[after_key..].trim_start();
        if let Some(colon) = after.strip_prefix(':') {
            let val = colon.trim_start();
            let off = body.len() - val.len();
            return Some(off);
        }
        from = after_key;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_obj_escapes_values() {
        assert_eq!(
            json_obj(&[("session_code", "100000001"), ("sdp", "a\"b")]),
            "{\"session_code\":\"100000001\",\"sdp\":\"a\\\"b\"}"
        );
    }

    #[test]
    fn string_field_extraction_and_null() {
        let body = r#"{"session_code":"100000001","offer":"v=0","answer":null}"#;
        assert_eq!(json_string_field(body, "session_code").as_deref(), Some("100000001"));
        assert_eq!(json_string_field(body, "offer").as_deref(), Some("v=0"));
        assert_eq!(json_string_field(body, "answer"), None);
        assert_eq!(json_string_field(body, "missing"), None);
    }

    #[test]
    fn string_field_handles_escapes() {
        let body = r#"{"sdp":"line1\nline2\t\"q\""}"#;
        assert_eq!(json_string_field(body, "sdp").as_deref(), Some("line1\nline2\t\"q\""));
    }

    #[test]
    fn string_array_extraction() {
        let body = r#"{"candidates":["cand:a","cand:b","cand:c"]}"#;
        assert_eq!(
            json_string_array_field(body, "candidates"),
            vec!["cand:a", "cand:b", "cand:c"]
        );
        assert!(json_string_array_field(body, "candidates").len() == 3);
        let empty = r#"{"candidates":[]}"#;
        assert!(json_string_array_field(empty, "candidates").is_empty());
    }

    #[test]
    fn u64_field_extraction() {
        let body = r#"{"ttl_secs":86400,"x":1}"#;
        assert_eq!(json_u64_field(body, "ttl_secs"), Some(86400));
    }

    #[test]
    fn parse_response_plain_and_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_response(plain).unwrap(), (200, "{}".to_string()));

        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), (200, "{\"a\":1}".to_string()));
    }

    #[test]
    fn status_error_surfaces_code() {
        let notfound = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let (status, _) = parse_response(notfound).unwrap();
        assert_eq!(status, 404);
        assert!(matches!(expect(status, 200), Err(ClientError::Status(404))));
    }
}
