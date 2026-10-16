//! Owner: Person A — raw bytes -> request head.
//! The body is handled in 2c, once we know the server's body limit.
use std::collections::HashMap;

use super::{Method, Request};

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

    /// Head + body -> the shared `Request` type (consumes the head).
    pub fn into_request(self, body: Vec<u8>) -> Request {
        Request {
            method: self.method,
            path: self.path,
            query: self.query,
            version: self.version,
            headers: self.headers,
            body,
        }
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

/// How the body is framed, decided from the headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// No body (a normal GET).
    None,
    /// Exactly this many bytes follow the head.
    Length(usize),
    /// Transfer-Encoding: chunked (Step 4).
    Chunked,
}

pub fn body_kind(head: &RequestHead) -> Result<BodyKind, ParseError> {
    match (
        head.header("transfer-encoding"),
        head.header("content-length"),
    ) {
        // Both at once = classic request-smuggling trick: refuse.
        (Some(_), Some(_)) => Err(ParseError::BadRequest),
        (Some(te), None) => {
            if te.trim().eq_ignore_ascii_case("chunked") {
                Ok(BodyKind::Chunked)
            } else {
                Err(ParseError::BadRequest)
            }
        }
        (None, Some(cl)) => parse_content_length(cl).map(BodyKind::Length),
        (None, None) => Ok(BodyKind::None),
    }
}

/// "5" -> 5. A repeated header arrives joined ("5, 5"): all copies must agree.
fn parse_content_length(value: &str) -> Result<usize, ParseError> {
    let mut result: Option<usize> = None;
    for part in value.split(',') {
        let p = part.trim();
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseError::BadRequest);
        }
        // parse() also fails on numbers too big for usize.
        let n: usize = p.parse().map_err(|_| ParseError::BadRequest)?;
        match result {
            Some(previous) if previous != n => return Err(ParseError::BadRequest),
            _ => result = Some(n),
        }
    }
    result.ok_or(ParseError::BadRequest)
}

// ───────────────────────────── chunked bodies ───────────────────────────

const MAX_CHUNK_LINE: usize = 1024;

const MAX_TRAILERS: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    /// Waiting for a size line like "1a\r\n".
    Size,
    /// This many data bytes of the current chunk are still missing.
    Data(usize),
    /// Waiting for the "\r\n" that ends a chunk's data.
    DataEnd,
    /// After the "0" chunk: header lines until an empty line.
    Trailers,
    /// The whole body has arrived.
    Done,
}

/// Decodes a chunked body piece by piece, remembering where it stopped.
#[derive(Debug)]
pub struct ChunkedDecoder {
    /// Position in `raw` (the bytes after the head) where we stopped.
    pos: usize,
    state: ChunkState,
    /// The real body: chunk data only, no sizes, no "\r\n".
    body: Vec<u8>,
    trailer_bytes: usize,
}

impl ChunkedDecoder {
    pub fn new() -> Self {
        ChunkedDecoder {
            pos: 0,
            state: ChunkState::Size,
            body: Vec::new(),
            trailer_bytes: 0,
        }
    }

    /// `raw` = every byte received after the head so far (it only grows).
    /// Ok(true)  → the body is complete.
    /// Ok(false) → need more bytes; call again when they arrive.
    /// Err(e)    → broken (400) or too big (413).
    pub fn advance(&mut self, raw: &[u8], max_body: usize) -> Result<bool, ParseError> {
        loop {
            match self.state {
                ChunkState::Size => {
                    let line_end = match find_newline(raw, self.pos) {
                        Some(i) => i,
                        None => {
                            if raw.len() - self.pos > MAX_CHUNK_LINE {
                                return Err(ParseError::BadRequest);
                            }
                            return Ok(false);
                        }
                    };
                    if line_end - self.pos > MAX_CHUNK_LINE {
                        return Err(ParseError::BadRequest);
                    }
                    let size = parse_chunk_size(&raw[self.pos..line_end])?;
                    self.pos = line_end + 1;
                    if size == 0 {
                        self.state = ChunkState::Trailers;
                    } else {
                        // Check the limit BEFORE storing the chunk.
                        if self.body.len().saturating_add(size) > max_body {
                            return Err(ParseError::PayloadTooLarge);
                        }
                        self.state = ChunkState::Data(size);
                    }
                }
                ChunkState::Data(missing) => {
                    let available = raw.len() - self.pos;
                    if available == 0 {
                        return Ok(false);
                    }
                    let take = missing.min(available);
                    self.body.extend_from_slice(&raw[self.pos..self.pos + take]);
                    self.pos += take;
                    if take < missing {
                        self.state = ChunkState::Data(missing - take);
                        return Ok(false);
                    }
                    self.state = ChunkState::DataEnd;
                }
                ChunkState::DataEnd => match raw.get(self.pos) {
                    None => return Ok(false),
                    Some(b'\n') => {
                        self.pos += 1;
                        self.state = ChunkState::Size;
                    }
                    Some(b'\r') => match raw.get(self.pos + 1) {
                        None => return Ok(false),
                        Some(b'\n') => {
                            self.pos += 2;
                            self.state = ChunkState::Size;
                        }
                        Some(_) => return Err(ParseError::BadRequest),
                    },
                    Some(_) => return Err(ParseError::BadRequest),
                },
                ChunkState::Trailers => {
                    let line_end = match find_newline(raw, self.pos) {
                        Some(i) => i,
                        None => {
                            if self.trailer_bytes + (raw.len() - self.pos) > MAX_TRAILERS {
                                return Err(ParseError::BadRequest);
                            }
                            return Ok(false);
                        }
                    };
                    let line = &raw[self.pos..line_end];
                    self.pos = line_end + 1;
                    if line.is_empty() || line == b"\r" {
                        self.state = ChunkState::Done; // empty line = end of body
                    } else {
                        // A trailer header: accepted and ignored.
                        self.trailer_bytes += line.len() + 1;
                        if self.trailer_bytes > MAX_TRAILERS {
                            return Err(ParseError::BadRequest);
                        }
                    }
                }
                ChunkState::Done => return Ok(true),
            }
        }
    }

    /// How many bytes of `raw` the chunked body used (sizes and "\r\n" included).
    pub fn consumed(&self) -> usize {
        self.pos
    }

    /// The decoded body. Call once `advance` returned Ok(true).
    pub fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.body)
    }
}

/// "1a" / "1a;name=value" / "1a\r" -> 26
fn parse_chunk_size(line: &[u8]) -> Result<usize, ParseError> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    // Chunk extensions (";name=value") are allowed and ignored.
    let size_part = match line.iter().position(|&b| b == b';') {
        Some(i) => &line[..i],
        None => line,
    };
    let text = std::str::from_utf8(size_part)
        .map_err(|_| ParseError::BadRequest)?
        .trim_matches(|c| c == ' ' || c == '\t');
    // At most 15 hex digits, so the number always fits in a usize.
    if text.is_empty() || text.len() > 15 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ParseError::BadRequest);
    }
    usize::from_str_radix(text, 16).map_err(|_| ParseError::BadRequest)
}

/// Index of the next '\n' at or after `from`.
fn find_newline(buf: &[u8], from: usize) -> Option<usize> {
    buf[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| from + i)
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

    fn head_with(extra_headers: &str) -> RequestHead {
        let raw = format!("POST /up HTTP/1.1\r\nHost: a\r\n{extra_headers}\r\n");
        head(raw.as_bytes())
    }

    #[test]
    fn body_kind_from_headers() {
        assert_eq!(
            body_kind(&head(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n")),
            Ok(BodyKind::None)
        );
        assert_eq!(
            body_kind(&head_with("Content-Length: 5\r\n")),
            Ok(BodyKind::Length(5))
        );
        assert_eq!(
            body_kind(&head_with("Content-Length: 0\r\n")),
            Ok(BodyKind::Length(0))
        );
        assert_eq!(
            body_kind(&head_with("Transfer-Encoding: chunked\r\n")),
            Ok(BodyKind::Chunked)
        );
    }

    #[test]
    fn bad_body_framing_is_400() {
        let cases = [
            "Content-Length: abc\r\n",
            "Content-Length: -5\r\n",
            "Content-Length: 5\r\nContent-Length: 6\r\n",
            "Content-Length: 99999999999999999999999\r\n",
            "Transfer-Encoding: gzip\r\n",
            "Content-Length: 5\r\nTransfer-Encoding: chunked\r\n",
        ];
        for extra in cases {
            assert_eq!(
                body_kind(&head_with(extra)),
                Err(ParseError::BadRequest),
                "for {extra:?}"
            );
        }
    }

    #[test]
    fn same_length_twice_is_fine() {
        let h = head_with("Content-Length: 5\r\nContent-Length: 5\r\n");
        assert_eq!(body_kind(&h), Ok(BodyKind::Length(5)));
    }

    #[test]
    fn into_request_keeps_everything() {
        let h = head_with("Content-Length: 5\r\n");
        let req = h.into_request(b"hello".to_vec());
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.path, "/up");
        assert_eq!(req.body, b"hello");
        assert_eq!(req.header("content-length"), Some("5"));
    }

    // ── chunked decoder ─────────────────────────────────────────────

    /// Decodes a whole chunked body in one call.
    fn decode(raw: &[u8], max: usize) -> Result<(Vec<u8>, usize), ParseError> {
        let mut d = ChunkedDecoder::new();
        match d.advance(raw, max)? {
            true => Ok((d.take_body(), d.consumed())),
            false => panic!("body not complete: {:?}", String::from_utf8_lossy(raw)),
        }
    }

    #[test]
    fn decodes_chunked_body() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(decode(raw, 1024), Ok((b"Wikipedia".to_vec(), raw.len())));
    }

    #[test]
    fn hex_sizes() {
        let mut raw = b"a\r\n0123456789\r\n10\r\n".to_vec();
        raw.extend_from_slice(&[b'x'; 16]); // 0x10 = 16 bytes
        raw.extend_from_slice(b"\r\n0\r\n\r\n");
        let (body, _) = decode(&raw, 1024).unwrap();
        assert_eq!(body.len(), 10 + 16);
    }

    #[test]
    fn empty_chunked_body() {
        assert_eq!(decode(b"0\r\n\r\n", 1024), Ok((Vec::new(), 5)));
    }

    #[test]
    fn extensions_and_trailers_are_ignored() {
        let raw = b"4;name=value\r\nWiki\r\n0\r\nX-Checksum: abc\r\nX-Other: 1\r\n\r\n";
        assert_eq!(decode(raw, 1024), Ok((b"Wiki".to_vec(), raw.len())));
    }

    #[test]
    fn works_when_bytes_arrive_one_by_one() {
        let raw = b"3\r\nabc\r\na;x=1\r\n0123456789\r\n0\r\nT: 1\r\n\r\n";
        let mut d = ChunkedDecoder::new();
        for end in 1..raw.len() {
            assert_eq!(
                d.advance(&raw[..end], 1024),
                Ok(false),
                "finished too early at {end}"
            );
        }
        assert_eq!(d.advance(raw, 1024), Ok(true));
        assert_eq!(d.consumed(), raw.len());
        assert_eq!(d.take_body(), b"abc0123456789");
    }

    #[test]
    fn stops_at_the_end_of_the_body() {
        // A pipelined request follows: it must NOT be eaten.
        let body = b"2\r\nhi\r\n0\r\n\r\n";
        let mut raw = body.to_vec();
        raw.extend_from_slice(b"GET /next HTTP/1.1\r\n");
        let (decoded, used) = decode(&raw, 1024).unwrap();
        assert_eq!(decoded, b"hi");
        assert_eq!(used, body.len());
        assert_eq!(&raw[used..], b"GET /next HTTP/1.1\r\n");
    }

    #[test]
    fn chunked_body_over_the_limit_is_413() {
        let raw = b"6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n"; // 11 bytes
        assert_eq!(decode(raw, 11).map(|(b, _)| b), Ok(b"hello world".to_vec()));
        assert_eq!(decode(raw, 10), Err(ParseError::PayloadTooLarge));
        // Refused from the size line alone, before the 1 GB "arrives".
        let mut d = ChunkedDecoder::new();
        assert_eq!(
            d.advance(b"40000000\r\n", 1024),
            Err(ParseError::PayloadTooLarge)
        );
    }

    #[test]
    fn malformed_chunks_are_400() {
        let cases: Vec<&[u8]> = vec![
            &b"zz\r\nhi\r\n0\r\n\r\n"[..], // not hex
            &b"\r\nhi\r\n0\r\n\r\n"[..],   // empty size
            &b"-2\r\nhi\r\n0\r\n\r\n"[..], // negative
            &b"fffffffffffffffff\r\n"[..], // 17 digits: too long
            &b"3\r\nabcXX0\r\n\r\n"[..],   // data not followed by \r\n
            &b"3\r\nabc\rX"[..],           // \r not followed by \n
        ];
        for raw in cases {
            let mut d = ChunkedDecoder::new();
            assert_eq!(
                d.advance(raw, 1024),
                Err(ParseError::BadRequest),
                "expected 400 for {:?}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[test]
    fn endless_lines_are_400() {
        let mut d = ChunkedDecoder::new();
        let endless_size = vec![b'1'; MAX_CHUNK_LINE + 1];
        assert_eq!(
            d.advance(&endless_size, usize::MAX),
            Err(ParseError::BadRequest)
        );

        let mut d = ChunkedDecoder::new();
        let mut endless_trailer = b"0\r\nX-Big: ".to_vec();
        endless_trailer.extend(std::iter::repeat(b'a').take(MAX_TRAILERS + 1));
        assert_eq!(
            d.advance(&endless_trailer, 1024),
            Err(ParseError::BadRequest)
        );
    }
}
