//! Owner: Person A — Response -> bytes on the wire.
use super::{reason_phrase, Response};

pub fn to_bytes(resp: &Response) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {} {}\r\n", resp.status, reason_phrase(resp.status));
    let mut has_len = false;
    for (name, value) in &resp.headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_len = true;
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_len {
        out.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&resp.body);
    bytes
}
