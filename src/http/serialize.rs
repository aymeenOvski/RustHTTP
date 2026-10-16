//! Owner: Person A — Response -> bytes on the wire.
use super::{Response, reason_phrase};

pub struct Wire {
    /// Status line + headers + the empty line.
    pub head: Vec<u8>,
    /// The body (empty for 1xx, 204 and 304).
    pub body: Vec<u8>,
}

pub fn to_wire(resp: Response) -> Wire {
    // 1xx, 204 and 304 never carry a body (RFC 9110).
    let no_body = (100..200).contains(&resp.status) || resp.status == 204 || resp.status == 304;

    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        resp.status,
        reason_phrase(resp.status)
    );
    let mut has_length = false;
    for (name, value) in &resp.headers {
        if name.eq_ignore_ascii_case("content-length") {
            if no_body {
                continue; // not allowed on bodiless responses
            }
            has_length = true;
        }
        head.push_str(&clean(name));
        head.push_str(": ");
        head.push_str(&clean(value));
        head.push_str("\r\n");
    }
    if !has_length && !no_body {
        head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    head.push_str("\r\n");

    Wire {
        head: head.into_bytes(),
        body: if no_body { Vec::new() } else { resp.body }, // moved, not copied
    }
}

/// Removes CR and LF so a header value can never inject extra headers.
fn clean(s: &str) -> String {
    s.chars().filter(|&c| c != '\r' && c != '\n').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Head + body glued together: exactly what the client receives.
    fn on_the_wire(resp: Response) -> Vec<u8> {
        let wire = to_wire(resp);
        let mut bytes = wire.head;
        bytes.extend_from_slice(&wire.body);
        bytes
    }

    #[test]
    fn adds_status_line_and_content_length() {
        let text = String::from_utf8(on_the_wire(Response::text(200, "hi"))).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.ends_with("\r\n\r\nhi"));
    }

    #[test]
    fn keeps_an_existing_content_length() {
        let resp = Response::new(200)
            .with_header("Content-Length", "2")
            .with_body(b"hi".to_vec());
        let text = String::from_utf8(on_the_wire(resp)).unwrap();
        assert_eq!(text.matches("Content-Length").count(), 1);
    }

    #[test]
    fn no_body_and_no_length_for_204() {
        let resp = Response::new(204).with_body(b"ignored".to_vec());
        assert_eq!(on_the_wire(resp), b"HTTP/1.1 204 No Content\r\n\r\n");
    }

    #[test]
    fn header_injection_is_removed() {
        let resp = Response::new(301).with_header("Location", "/x\r\nSet-Cookie: evil=1");
        let text = String::from_utf8(on_the_wire(resp)).unwrap();
        assert!(text.contains("Location: /xSet-Cookie: evil=1\r\n"));
        assert!(!text.contains("\r\nSet-Cookie"));
    }

    #[test]
    fn the_body_is_moved_not_copied() {
        let body = vec![b'x'; 1_000_000];
        let address = body.as_ptr(); // where the body's bytes live in memory
        let wire = to_wire(Response::new(200).with_body(body));
        assert_eq!(wire.body.as_ptr(), address); // same memory: no copy was made
        assert_eq!(wire.body.len(), 1_000_000);
    }
}
