//! SHARED CONTRACT — announce any change to the whole team first.
pub mod parser;
pub mod serialize;

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

impl Method {
    pub fn parse(s: &str) -> Option<Method> {
        match s {
            "GET" => Some(Method::Get),
            "POST" => Some(Method::Post),
            "DELETE" => Some(Method::Delete),
            _ => None,
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub version: String,
    /// Header names are stored lowercase.
    pub headers: HashMap<String, String>,
    /// The body when it is small. Already de-chunked.
    pub body: Vec<u8>,
    /// The body when it is big: it was written to a temporary file while it
    /// arrived, so it never had to fit in RAM. `body` is then empty.
    pub body_file: Option<BodyFile>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }

    /// Size of the body, wherever it is (memory or file).
    pub fn body_len(&self) -> u64 {
        match &self.body_file {
            Some(file) => file.len(),
            None => self.body.len() as u64,
        }
    }
}

/// A request body stored in a temporary file. The file is deleted when this
/// value is dropped (request finished, client gone, error).
///
/// On Linux, deleting a file that is still open only removes its name: a
/// job that took it (upload, CGI) keeps reading it to the end.
pub struct BodyFile {
    path: PathBuf,
    len: u64,
    /// The file, open and at its start, until a job takes it.
    file: Cell<Option<File>>,
}

impl BodyFile {
    /// Made by the network layer once the whole body is in the file.
    pub fn new(path: PathBuf, len: u64, file: File) -> BodyFile {
        BodyFile {
            path,
            len,
            file: Cell::new(Some(file)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The body file, open at its start, for the job that will read it.
    /// That job becomes its last user: it closes it (or gives it to
    /// `spool::throw_away`) when done.
    pub fn take_file(&self) -> io::Result<File> {
        match self.file.take() {
            Some(file) => Ok(file),
            None => File::open(&self.path), // taken before: open it again
        }
    }
}

impl fmt::Debug for BodyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BodyFile({}, {} bytes)", self.path.display(), self.len)
    }
}

impl Drop for BodyFile {
    fn drop(&mut self) {
        // The name goes at once. The disk space is freed a piece per turn,
        // because freeing 1 GB at once can stop the event loop for a second.
        let _ = fs::remove_file(&self.path);
        if let Some(file) = self.file.take() {
            crate::spool::throw_away(file);
        }
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// A body built in memory (pages, error pages, CGI output).
    pub body: Vec<u8>,
    /// A body sent straight from a file (downloads). It is read piece by
    /// piece while it is sent, so the file never has to fit in RAM.
    /// When set, `body` is empty.
    pub file: Option<FileBody>,
}

/// An open file and how many bytes of it to send.
#[derive(Debug)]
pub struct FileBody {
    pub file: File,
    pub len: u64,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            file: None,
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Response::new(status)
            .with_header("Content-Type", "text/plain; charset=utf-8")
            .with_body(body.as_bytes().to_vec())
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self.file = None;
        self
    }

    /// The body is the first `len` bytes of `file`, sent from disk.
    pub fn with_file(mut self, file: File, len: u64) -> Self {
        self.body = Vec::new();
        self.file = Some(FileBody { file, len });
        self
    }

    /// Size of the body, wherever it is (memory or file).
    pub fn body_len(&self) -> u64 {
        match &self.file {
            Some(f) => f.len,
            None => self.body.len() as u64,
        }
    }
}

pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_parse() {
        assert_eq!(Method::parse("GET"), Some(Method::Get));
        assert_eq!(Method::parse("PUT"), None);
    }
}
