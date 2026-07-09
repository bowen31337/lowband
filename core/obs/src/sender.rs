//! Telemetry sender — Feature 137.
//!
//! [`send`] assembles the JSON payload from a [`QosTelemetryBatch`] and
//! POSTs it to the configured endpoint over a plain TCP connection using
//! HTTP/1.1.  No media content is ever included in the request body.
//!
//! The function is a no-op (returns [`TelemetryError::Disabled`]) when
//! [`QosTelemetryConfig::enabled`] is `false`, so no network connection
//! is opened until the user explicitly opts in.

use std::io::{Read, Write};
use std::net::TcpStream;

use crate::telemetry::{QosTelemetryBatch, QosTelemetryConfig};

/// Errors returned by [`send`].
#[derive(Debug)]
pub enum TelemetryError {
    /// User has not opted in — `config.enabled` is `false`.
    Disabled,
    /// The endpoint URL could not be parsed as `http://host[:port]/path`.
    InvalidEndpoint(String),
    /// HTTP response status line could not be parsed.
    ParseError,
    /// A TCP I/O error occurred while connecting or transferring data.
    Io(std::io::Error),
    /// The server returned a non-2xx HTTP status code.
    HttpError(u16),
}

impl std::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryError::Disabled =>
                write!(f, "telemetry is disabled (user has not opted in)"),
            TelemetryError::InvalidEndpoint(s) =>
                write!(f, "invalid endpoint: {s}"),
            TelemetryError::ParseError =>
                write!(f, "malformed HTTP response status line"),
            TelemetryError::Io(e) =>
                write!(f, "I/O error: {e}"),
            TelemetryError::HttpError(code) =>
                write!(f, "HTTP error: {code}"),
        }
    }
}

impl From<std::io::Error> for TelemetryError {
    fn from(e: std::io::Error) -> Self {
        TelemetryError::Io(e)
    }
}

/// POST `batch` to `config.endpoint` as a JSON body.
///
/// Returns `Err(TelemetryError::Disabled)` immediately when
/// `config.enabled` is `false` — no network connection is attempted.
///
/// The POST body is the JSON serialisation of `batch`, which contains
/// only aggregate numeric statistics and no media content.
pub fn send(config: &QosTelemetryConfig, batch: &QosTelemetryBatch) -> Result<(), TelemetryError> {
    if !config.enabled {
        return Err(TelemetryError::Disabled);
    }
    let (host, port, path) = parse_http_endpoint(&config.endpoint)?;
    let body = batch.to_json();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        len = body.len(),
    );
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)?;
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let status = parse_status_code(&response)?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(TelemetryError::HttpError(status))
    }
}

/// Parse `http://host[:port]/path` into its components.
///
/// Port defaults to 80 when omitted.  Only the `http://` scheme is
/// accepted; `https://` and other schemes return
/// [`TelemetryError::InvalidEndpoint`].
fn parse_http_endpoint(endpoint: &str) -> Result<(String, u16, String), TelemetryError> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| TelemetryError::InvalidEndpoint(endpoint.to_owned()))?;
    let (authority, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    let (host, port) = if let Some(colon) = authority.rfind(':') {
        let port: u16 = authority[colon + 1..]
            .parse()
            .map_err(|_| TelemetryError::InvalidEndpoint(endpoint.to_owned()))?;
        (&authority[..colon], port)
    } else {
        (authority, 80u16)
    };
    Ok((host.to_owned(), port, path.to_owned()))
}

/// Extract the numeric HTTP status code from the first response line.
fn parse_status_code(response: &str) -> Result<u16, TelemetryError> {
    response
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(TelemetryError::ParseError)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{QosTelemetryBatch, QosTelemetryConfig, TierCounts};

    // ── Opt-in gate ───────────────────────────────────────────────────────────

    #[test]
    fn send_returns_disabled_when_not_opted_in() {
        let config = QosTelemetryConfig::new("http://example.com/qos");
        assert!(!config.enabled, "config must be disabled by default");

        let batch = QosTelemetryBatch {
            session_count: 1,
            total_bytes_sum: 1_000,
            duration_sum_ms: 60_000,
            peak_tier_counts: TierCounts::default(),
        };
        let result = send(&config, &batch);
        assert!(
            matches!(result, Err(TelemetryError::Disabled)),
            "send must return Disabled — not attempt a network connection — when opted out"
        );
    }

    // ── parse_http_endpoint ───────────────────────────────────────────────────

    #[test]
    fn parse_http_endpoint_host_port_path() {
        let (host, port, path) =
            parse_http_endpoint("http://telemetry.example.com:9000/v1/qos").unwrap();
        assert_eq!(host, "telemetry.example.com");
        assert_eq!(port, 9000);
        assert_eq!(path, "/v1/qos");
    }

    #[test]
    fn parse_http_endpoint_default_port_80() {
        let (host, port, path) =
            parse_http_endpoint("http://telemetry.example.com/qos").unwrap();
        assert_eq!(host, "telemetry.example.com");
        assert_eq!(port, 80);
        assert_eq!(path, "/qos");
    }

    #[test]
    fn parse_http_endpoint_no_path_defaults_to_slash() {
        let (host, port, path) =
            parse_http_endpoint("http://example.com").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        assert_eq!(path, "/");
    }

    #[test]
    fn parse_http_endpoint_rejects_non_http_scheme() {
        assert!(
            matches!(
                parse_http_endpoint("https://example.com/qos"),
                Err(TelemetryError::InvalidEndpoint(_))
            ),
            "https:// scheme must be rejected"
        );
    }

    #[test]
    fn parse_http_endpoint_rejects_invalid_port() {
        assert!(
            matches!(
                parse_http_endpoint("http://example.com:notaport/qos"),
                Err(TelemetryError::InvalidEndpoint(_))
            ),
            "non-numeric port must be rejected"
        );
    }

    // ── parse_status_code ─────────────────────────────────────────────────────

    #[test]
    fn parse_status_code_200_ok() {
        assert_eq!(parse_status_code("HTTP/1.1 200 OK\r\n\r\n").unwrap(), 200);
    }

    #[test]
    fn parse_status_code_204_no_content() {
        assert_eq!(parse_status_code("HTTP/1.1 204 No Content\r\n").unwrap(), 204);
    }

    #[test]
    fn parse_status_code_500_server_error() {
        assert_eq!(parse_status_code("HTTP/1.1 500 Internal Server Error\r\n").unwrap(), 500);
    }

    #[test]
    fn parse_status_code_empty_response_returns_parse_error() {
        assert!(matches!(parse_status_code(""), Err(TelemetryError::ParseError)));
    }
}
