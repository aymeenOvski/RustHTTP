//! Owner: Person A — raw bytes -> request head.
//! The body is handled in 2c, once we know the server's body limit.
use std::collections::HashMap;

use super::Method;

/// A head bigger than this is refused (protects memory).
pub const MAX_HEAD_SIZE: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Broken request → 400
    BadRequest,
    /// A real method we don't support (PUT, PATCH...) → 405
    MethodNotAllowed,
    /// Body over the limit → 413 (used in 2c)
    PayloadTooLarge,
}

impl ParseError {
    pub fn status(self) -> u16 {
        match self {
            ParseError::BadRequest => 400,
            ParseError::MethodNotAllowed => 405,
            ParseError::PayloadTooLarge => 413,
        }
    }
}

/// Everything before the body.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestHead {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub version: String,
    /// Names are lowercase; a repeated header has its values joined with ", ".
    pub headers: HashMap<String, String>,
    /// Bytes used by the head, including the final "\r\n\r\n".
    /// The body (if any) starts at this index.
    pub head_len: usize,
}

impl RequestHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }
}

/// Ok(None)        → the head is not complete yet, read more.
/// Ok(Some(head))  → a complete, valid head.
/// Err(e)          → a broken request, answer e.status() and close.
pub fn parse_head(buf: &[u8]) -> Result<Option<RequestHead>, ParseError> {
    // 1. Find the empty line that ends the head.
    let end = match find(buf, b"\r\n\r\n") {
        Some(i) => i,
        None => {
            if buf.len() > MAX_HEAD_SIZE {
                return Err(ParseError::BadRequest);
            }
            return Ok(None);
        }
    };
    if end > MAX_HEAD_SIZE {
        return Err(ParseError::BadRequest);
    }

    // 2. The head must be valid text.
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| ParseError::BadRequest)?;

    // 3. First line = request line, the rest = headers.
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(ParseError::BadRequest)?;
    let (method, path, query, version) = parse_request_line(request_line)?;

    // 4. Headers.
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(ParseError::BadRequest)?;
        if name.is_empty() || name.contains(|c: char| c.is_whitespace()) {
            return Err(ParseError::BadRequest);
        }
        let value = value.trim();
        headers
            .entry(name.to_ascii_lowercase())
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(value);
            })
            .or_insert_with(|| value.to_string());
    }

    // 5. HTTP/1.1 requires Host.
    if version == "HTTP/1.1" && !headers.contains_key("host") {
        return Err(ParseError::BadRequest);
    }

    Ok(Some(RequestHead {
        method,
        path,
        query,
        version,
        headers,
        head_len: end + 4,
    }))
}

type RequestLine = (Method, String, Option<String>, String);

/// "GET /index.html?x=1 HTTP/1.1" -> (Get, "/index.html", Some("x=1"), "HTTP/1.1")
fn parse_request_line(line: &str) -> Result<RequestLine, ParseError> {
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(ParseError::BadRequest);
    }
    let (m, target, version) = (parts[0], parts[1], parts[2]);

    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(ParseError::BadRequest);
    }

    let method = match Method::parse(m) {
        Some(method) => method,
        // Looks like a real method name, we just don't support it.
        None if m.chars().all(|c| c.is_ascii_uppercase()) => {
            return Err(ParseError::MethodNotAllowed);
        }
        None => return Err(ParseError::BadRequest),
    };

    if !target.starts_with('/') {
        return Err(ParseError::BadRequest);
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target.to_string(), None),
    };

    Ok((method, path, query, version.to_string()))
}

/// Position of `needle` inside `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(raw: &[u8]) -> RequestHead {
        match parse_head(raw) {
            Ok(Some(h)) => h,
            other => panic!("expected a complete head, got {other:?}"),
        }
    }

    #[test]
    fn parses_get_with_query_and_headers() {
        let raw = b"GET /hello?name=you HTTP/1.1\r\nHost: localhost:8080\r\nUser-Agent: curl/8.5.0\r\n\r\n";
        let h = head(raw);
        assert_eq!(h.method, Method::Get);
        assert_eq!(h.path, "/hello");
        assert_eq!(h.query.as_deref(), Some("name=you"));
        assert_eq!(h.version, "HTTP/1.1");
        assert_eq!(h.header("host"), Some("localhost:8080"));
        assert_eq!(h.header("USER-AGENT"), Some("curl/8.5.0"));
        assert_eq!(h.head_len, raw.len());
    }

    #[test]
    fn head_len_points_at_the_body() {
        let raw = b"POST /up HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\nhello";
        let h = head(raw);
        assert_eq!(h.method, Method::Post);
        assert_eq!(&raw[h.head_len..], b"hello");
    }

    #[test]
    fn parses_delete() {
        let h = head(b"DELETE /files/a.txt HTTP/1.1\r\nHost: a\r\n\r\n");
        assert_eq!(h.method, Method::Delete);
        assert_eq!(h.path, "/files/a.txt");
        assert_eq!(h.query, None);
    }

    #[test]
    fn incomplete_head_asks_for_more() {
        assert_eq!(parse_head(b""), Ok(None));
        assert_eq!(parse_head(b"GET / HTTP/1.1\r\nHost: a\r\n"), Ok(None));
    }

    #[test]
    fn header_names_lowercased_and_repeats_joined() {
        let h = head(b"GET / HTTP/1.1\r\nHOST: a\r\nAccept: text/html\r\naccept: */*\r\n\r\n");
        assert!(h.headers.contains_key("host"));
        assert_eq!(h.header("accept"), Some("text/html, */*"));
    }

    #[test]
    fn unsupported_method_is_405() {
        assert_eq!(
            parse_head(b"PUT / HTTP/1.1\r\nHost: a\r\n\r\n"),
            Err(ParseError::MethodNotAllowed)
        );
        assert_eq!(ParseError::MethodNotAllowed.status(), 405);
    }

    #[test]
    fn broken_requests_are_400() {
        let cases: Vec<&[u8]> = vec![
            &b"GET /\r\nHost: a\r\n\r\n"[..],                     // only 2 parts
            &b"GET  / HTTP/1.1\r\nHost: a\r\n\r\n"[..],           // double space
            &b"GET / HTTP/2.0\r\nHost: a\r\n\r\n"[..],            // bad version
            &b"GET / HTTP/1.1\r\n\r\n"[..],                       // no Host
            &b"GET index.html HTTP/1.1\r\nHost: a\r\n\r\n"[..],   // no leading /
            &b"GET / HTTP/1.1\r\nHost: a\r\nNoColon\r\n\r\n"[..], // header without ':'
            &b"GET / HTTP/1.1\r\nHost: a\r\nBad Name: x\r\n\r\n"[..], // space in name
            &b"G(T / HTTP/1.1\r\nHost: a\r\n\r\n"[..],            // garbage method
            &b"GET /\xff HTTP/1.1\r\nHost: a\r\n\r\n"[..],        // not UTF-8
        ];
        for raw in cases {
            assert_eq!(
                parse_head(raw),
                Err(ParseError::BadRequest),
                "expected 400 for {:?}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[test]
    fn http10_does_not_need_host() {
        let h = head(b"GET / HTTP/1.0\r\n\r\n");
        assert_eq!(h.version, "HTTP/1.0");
    }

    #[test]
    fn oversized_head_is_400() {
        let mut raw = b"GET / HTTP/1.1\r\nHost: a\r\nX-Big: ".to_vec();
        raw.extend(std::iter::repeat(b'a').take(MAX_HEAD_SIZE + 10));
        assert_eq!(parse_head(&raw), Err(ParseError::BadRequest));
    }
}
