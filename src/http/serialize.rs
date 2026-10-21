//! turns a `Response` into the bytes sent to the client.
use super::{FileBody, Response, reason_phrase};

/// A response ready to send, kept in parts so the body is never copied.
pub struct Wire {
    /// Status line, headers and the empty line that ends them.
    pub head: Vec<u8>,
    /// The body, moved out of the `Response`. Empty for 1xx, 204 and 304.
    pub body: Vec<u8>,
    /// Or the body is a file, sent piece by piece after the head.
    pub file: Option<FileBody>,
}

/// Builds the bytes of a response.
///
/// - Adds `Content-Length` when the router did not set one (for a file:
///   its length, known before the first byte is read).
/// - 1xx, 204 and 304 get no body and no `Content-Length` (RFC 9110).
/// - CR and LF are removed from header names and values.
///
/// Takes the `Response` by value so its body is moved, not copied.
pub fn to_wire(resp: Response) -> Wire {
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
        head.push_str(&format!("Content-Length: {}\r\n", resp.body_len()));
    }
    head.push_str("\r\n");

    if no_body {
        return Wire {
            head: head.into_bytes(),
            body: Vec::new(),
            file: None, // dropped here: the file is closed
        };
    }
    Wire {
        head: head.into_bytes(),
        body: resp.body, // moved, not copied
        file: resp.file,
    }
}

/// Removes CR and LF, so a header name or value can never add extra headers.
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
    fn a_file_body_gets_its_length_and_stays_on_disk() {
        let path = std::env::temp_dir().join(format!("wire_{}.bin", std::process::id()));
        std::fs::write(&path, vec![b'z'; 3000]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let wire = to_wire(Response::new(200).with_file(file, 3000));
        let head = String::from_utf8(wire.head).unwrap();
        assert!(head.contains("Content-Length: 3000\r\n"));
        assert!(wire.body.is_empty()); // nothing was read into memory
        assert_eq!(wire.file.map(|f| f.len), Some(3000));
        let _ = std::fs::remove_file(&path);
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
