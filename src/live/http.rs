//! The small slice of HTTP/1.1 the live viewer needs: parse a GET request head, check its
//! `Host` header and token, route it, and build responses and server-sent events.
//!
//! Everything here is pure (no I/O), so it's unit-tested directly.

use std::fmt::Write as _;

/// Largest request head accepted; the viewer's requests are a few hundred bytes.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Headers sent with every response. The token travels in the URL, so nothing may leak it
/// through a referrer, a cache or an embedding page; the page loads nothing external.
const SECURITY_HEADERS: &str = "Cache-Control: no-store\r\n\
X-Content-Type-Options: nosniff\r\n\
Referrer-Policy: no-referrer\r\n\
X-Frame-Options: DENY\r\n\
Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src blob: data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n";

/// The parts of a request the viewer looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// Path of the target, still percent-encoded.
    pub path: String,
    /// Query string without the `?`, still percent-encoded.
    pub query: String,
    pub host: Option<String>,
}

/// Why a request head couldn't be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("malformed request")]
    Malformed,
    #[error("request head too large")]
    TooLarge,
}

/// Returns the length of the request head (through the blank line) once `buf` holds all of it.
#[must_use]
pub fn head_len(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Parses a request head: the request line and the headers, up to the blank line.
pub fn parse_head(head: &[u8]) -> Result<Request, ParseError> {
    if head.len() > MAX_HEAD_BYTES {
        return Err(ParseError::TooLarge);
    }
    let text = std::str::from_utf8(head).map_err(|_| ParseError::Malformed)?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version), None) = (
        request_line.next(),
        request_line.next(),
        request_line.next(),
        request_line.next(),
    ) else {
        return Err(ParseError::Malformed);
    };
    if !version.starts_with("HTTP/1.") || !target.starts_with('/') || method.is_empty() {
        return Err(ParseError::Malformed);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut host = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(ParseError::Malformed)?;
        if name.trim().eq_ignore_ascii_case("host") {
            // Two Host headers are a classic request-smuggling trick; refuse them
            if host.is_some() {
                return Err(ParseError::Malformed);
            }
            host = Some(value.trim().to_string());
        }
    }
    Ok(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        host,
    })
}

/// Whether the `Host` header names this server as a browser would reach it. Anything else,
/// such as a DNS-rebound domain that resolves to 127.0.0.1, is refused.
#[must_use]
pub fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Some((name, host_port)) = host.rsplit_once(':') else {
        return false;
    };
    host_port.parse::<u16>().is_ok_and(|p| p == port)
        && (name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost"))
}

/// Compares two tokens in time that doesn't depend on where they differ.
#[must_use]
pub fn tokens_match(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// The decoded value of `name` in a query string.
#[must_use]
pub fn query_param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| percent_decode(value))
}

/// Decodes `%XX` escapes. `None` if an escape is malformed or the result isn't UTF-8.
#[must_use]
pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Encodes `text` for use as one URL path segment or query value.
#[must_use]
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// What a request path asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `/`: the list of sessions.
    Index,
    /// `/s/<session_id>`: the viewer page.
    Page(String),
    /// `/s/<session_id>/events`: the event stream.
    Events(String),
    NotFound,
}

/// Routes a (still encoded) request path.
#[must_use]
pub fn route(path: &str) -> Route {
    if path == "/" {
        return Route::Index;
    }
    let Some(rest) = path.strip_prefix("/s/") else {
        return Route::NotFound;
    };
    let (segment, events) = match rest.split_once('/') {
        None => (rest, false),
        Some((segment, "events")) => (segment, true),
        Some(_) => return Route::NotFound,
    };
    match percent_decode(segment) {
        Some(id) if !id.is_empty() && events => Route::Events(id),
        Some(id) if !id.is_empty() => Route::Page(id),
        _ => Route::NotFound,
    }
}

/// An HTTP status line's code and reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status(pub u16, pub &'static str);

pub const OK: Status = Status(200, "OK");
pub const BAD_REQUEST: Status = Status(400, "Bad Request");
pub const FORBIDDEN: Status = Status(403, "Forbidden");
pub const NOT_FOUND: Status = Status(404, "Not Found");
pub const METHOD_NOT_ALLOWED: Status = Status(405, "Method Not Allowed");
pub const MISDIRECTED: Status = Status(421, "Misdirected Request");
pub const HEAD_TOO_LARGE: Status = Status(431, "Request Header Fields Too Large");

/// A complete response that closes the connection.
#[must_use]
pub fn response(status: Status, content_type: &str, body: &[u8]) -> Vec<u8> {
    let Status(code, reason) = status;
    let allow = if status == METHOD_NOT_ALLOWED {
        "Allow: GET\r\n"
    } else {
        ""
    };
    let mut out = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{allow}{SECURITY_HEADERS}Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// A short plain-text response, for errors.
#[must_use]
pub fn text_response(status: Status, message: &str) -> Vec<u8> {
    response(status, "text/plain; charset=utf-8", message.as_bytes())
}

/// Response head that starts a server-sent event stream. `retry` makes a dropped browser
/// reconnect after a second.
#[must_use]
pub fn event_stream_head() -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n{SECURITY_HEADERS}Connection: keep-alive\r\n\r\nretry: 1000\n\n"
    )
}

/// One server-sent event. Each line of `data` becomes its own `data:` field, so the browser
/// gets `data` back unchanged.
#[must_use]
pub fn sse_event(event: &str, data: &str) -> String {
    let mut out = String::with_capacity(data.len() + event.len() + 16);
    let _ = writeln!(out, "event: {event}");
    for line in data.split('\n') {
        let _ = writeln!(out, "data: {}", line.strip_suffix('\r').unwrap_or(line));
    }
    out.push('\n');
    out
}

/// Escapes text for HTML element content and attribute values.
#[must_use]
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Request, ParseError> {
        parse_head(text.as_bytes())
    }

    #[test]
    fn test_head_len_finds_blank_line() {
        assert_eq!(head_len(b"GET / HTTP/1.1\r\nHost: x\r\n"), None);
        assert_eq!(head_len(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
    }

    #[test]
    fn test_parse_request_line_and_host() {
        let request = parse(
            "GET /s/a%20b?t=abc&x=1 HTTP/1.1\r\nUser-Agent: t\r\nhOsT:  127.0.0.1:80 \r\n\r\n",
        )
        .unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/s/a%20b");
        assert_eq!(request.query, "t=abc&x=1");
        assert_eq!(request.host.as_deref(), Some("127.0.0.1:80"));

        let bare = parse("GET / HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!((bare.path.as_str(), bare.query.as_str()), ("/", ""));
        assert_eq!(bare.host, None);
    }

    #[test]
    fn test_parse_rejects_malformed_heads() {
        assert_eq!(parse("GET /\r\n\r\n"), Err(ParseError::Malformed));
        assert_eq!(parse("GET / HTTP/2\r\n\r\n"), Err(ParseError::Malformed));
        assert_eq!(
            parse("GET http://evil/ HTTP/1.1\r\n\r\n"),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            parse("GET / HTTP/1.1 extra\r\n\r\n"),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            parse("GET / HTTP/1.1\r\nno-colon\r\n\r\n"),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            parse("GET / HTTP/1.1\r\nHost: a:1\r\nHost: b:1\r\n\r\n"),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            parse_head(&[0xff, b'\r', b'\n']),
            Err(ParseError::Malformed)
        );
        let huge = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_HEAD_BYTES));
        assert_eq!(parse(&huge), Err(ParseError::TooLarge));
    }

    #[test]
    fn test_host_must_be_loopback_on_our_port() {
        assert!(host_allowed(Some("127.0.0.1:4000"), 4000));
        assert!(host_allowed(Some("localhost:4000"), 4000));
        assert!(host_allowed(Some("LOCALHOST:4000"), 4000));
        assert!(!host_allowed(Some("127.0.0.1:4001"), 4000));
        assert!(!host_allowed(Some("127.0.0.1"), 4000));
        assert!(!host_allowed(Some("evil.example:4000"), 4000));
        assert!(!host_allowed(Some("localhost.evil.example:4000"), 4000));
        assert!(!host_allowed(Some("127.0.0.1:4000@evil:4000"), 4000));
        assert!(!host_allowed(None, 4000));
    }

    #[test]
    fn test_tokens_match_only_exactly() {
        assert!(tokens_match("abcd", "abcd"));
        assert!(!tokens_match("abce", "abcd"));
        assert!(!tokens_match("abc", "abcd"));
        assert!(!tokens_match("", "abcd"));
    }

    #[test]
    fn test_query_param_decodes_value() {
        assert_eq!(query_param("t=ab%2Fc&x=1", "t").as_deref(), Some("ab/c"));
        assert_eq!(query_param("x=1&t=", "t").as_deref(), Some(""));
        assert_eq!(query_param("t", "t").as_deref(), Some(""));
        assert_eq!(query_param("tt=1", "t"), None);
        assert_eq!(query_param("t=%zz", "t"), None);
    }

    #[test]
    fn test_percent_round_trip() {
        let id = "my session/1 é";
        let encoded = percent_encode(id);
        assert_eq!(encoded, "my%20session%2F1%20%C3%A9");
        assert_eq!(percent_decode(&encoded).as_deref(), Some(id));
        assert_eq!(percent_decode("%4"), None);
        assert_eq!(percent_decode("%ff"), None, "not UTF-8");
    }

    #[test]
    fn test_routes() {
        assert_eq!(route("/"), Route::Index);
        assert_eq!(route("/s/default"), Route::Page("default".into()));
        assert_eq!(route("/s/a%2Fb"), Route::Page("a/b".into()));
        assert_eq!(route("/s/default/events"), Route::Events("default".into()));
        assert_eq!(route("/s/"), Route::NotFound);
        assert_eq!(route("/s/default/other"), Route::NotFound);
        assert_eq!(route("/favicon.ico"), Route::NotFound);
    }

    #[test]
    fn test_response_has_length_and_security_headers() {
        let bytes = response(OK, "text/html", b"<p>hi</p>");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Content-Length: 9\r\n"), "{text}");
        assert!(text.contains("Referrer-Policy: no-referrer"), "{text}");
        assert!(text.contains("Content-Security-Policy: default-src 'none'"));
        assert!(text.ends_with("\r\n\r\n<p>hi</p>"), "{text}");
        let refused = String::from_utf8(text_response(METHOD_NOT_ALLOWED, "no")).unwrap();
        assert!(refused.contains("Allow: GET\r\n"), "{refused}");
    }

    #[test]
    fn test_sse_event_splits_lines() {
        assert_eq!(
            sse_event("frame", "{\"a\":1}"),
            "event: frame\ndata: {\"a\":1}\n\n"
        );
        assert_eq!(
            sse_event("x", "one\r\ntwo"),
            "event: x\ndata: one\ndata: two\n\n"
        );
    }

    #[test]
    fn test_escape_html() {
        assert_eq!(
            escape_html("<a href=\"x\">&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
    }
}
