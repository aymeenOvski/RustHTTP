//! Owner: Person A — raw bytes -> Request (Content-Length + chunked).
use super::Request;

#[derive(Debug, PartialEq)]
pub enum ParseError {
    BadRequest,
    PayloadTooLarge,
}

#[derive(Debug)]
pub enum ParseStatus {
    /// A full request, plus how many bytes of the buffer it used.
    Complete(Request, usize),
    /// Need more bytes from the socket.
    Incomplete,
}

pub fn parse_request(buf: &[u8], max_body: usize) -> Result<ParseStatus, ParseError> {
    let _ = (buf, max_body); // TODO(A): real parser
    Ok(ParseStatus::Incomplete)
}
