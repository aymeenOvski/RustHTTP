//! SHARED CONTRACT — announce any change to the whole team first.
pub mod parser;
pub mod serialize;

use std::collections::HashMap;
use std::fmt;

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

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub version: String,
    /// Header names are stored lowercase.
    pub headers: HashMap<String, String>,
    /// Already de-chunked.
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
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
        self
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
