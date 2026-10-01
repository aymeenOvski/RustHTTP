#!/usr/bin/env bash
# LocalServer skeleton setup.
# Run it inside your (cloned, empty) repo folder:  bash setup.sh
set -euo pipefail

if [ ! -f Cargo.toml ]; then
  cargo init --name localserver
fi

grep -q '^mio' Cargo.toml || cat >> Cargo.toml <<'EOF'
mio = { version = "1", features = ["os-poll", "net"] }
EOF

mkdir -p src/http src/utils www/uploads error_pages cgi-bin

# ───────────────────────── main.rs (shared, tiny) ─────────────────────────
cat > src/main.rs <<'EOF'
// Remove this once the stubs are replaced by real code.
#![allow(dead_code)]

mod cgi;
mod config;
mod connection;
mod error;
mod http;
mod router;
mod server;
mod static_files;
mod upload;
mod utils;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.yaml".to_string());

    let config = match config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = server::run(config) {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
EOF

# ─────────────────── http/mod.rs  (SHARED CONTRACT) ───────────────────
cat > src/http/mod.rs <<'EOF'
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
EOF

# ───────────────────────────── Person A ─────────────────────────────
cat > src/http/parser.rs <<'EOF'
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
EOF

cat > src/http/serialize.rs <<'EOF'
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
EOF

cat > src/server.rs <<'EOF'
//! Owner: Person A — mio event loop, listeners, timeouts.
use crate::config::Config;

pub fn run(config: Config) -> std::io::Result<()> {
    // TODO(A): register listeners with mio::Poll and run the event loop.
    for s in &config.servers {
        println!("[stub] would listen on {}:{:?}", s.host, s.ports);
    }
    Ok(())
}
EOF

cat > src/connection.rs <<'EOF'
//! Owner: Person A — per-connection state machine.
use std::time::Instant;

#[derive(Debug, PartialEq)]
pub enum State {
    ReadingHead,
    ReadingBody,
    Writing,
    Closed,
}

pub struct Connection {
    pub state: State,
    pub read_buf: Vec<u8>,
    pub write_buf: Vec<u8>,
    pub written: usize,
    pub last_activity: Instant,
}

impl Connection {
    pub fn new() -> Self {
        Connection {
            state: State::ReadingHead,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            written: 0,
            last_activity: Instant::now(),
        }
    }
}
EOF

# ───────────────────────────── Person B ─────────────────────────────
cat > src/config.rs <<'EOF'
//! Owner: Person B — parsing + validation.
//! The STRUCTS below are SHARED CONTRACT.
use std::collections::HashMap;
use std::path::PathBuf;

use crate::http::Method;

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    pub host: String,
    pub ports: Vec<u16>,
    pub server_names: Vec<String>,
    pub is_default: bool,
    pub client_max_body_size: usize,
    pub error_pages: HashMap<u16, PathBuf>,
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct RouteConfig {
    pub path: String,
    pub methods: Vec<Method>,
    pub root: Option<PathBuf>,
    pub redirect: Option<String>,
    pub index: Option<String>,
    pub autoindex: bool,
    /// extension (e.g. ".py") -> interpreter path
    pub cgi: HashMap<String, PathBuf>,
    pub upload_dir: Option<PathBuf>,
}

pub fn load(path: &str) -> Result<Config, String> {
    let _ = path; // TODO(B): read file, parse, validate
    Ok(dev_config())
}

/// Hardcoded config so everyone can work before the parser exists.
pub fn dev_config() -> Config {
    Config {
        servers: vec![ServerConfig {
            host: "127.0.0.1".into(),
            ports: vec![8080],
            is_default: true,
            client_max_body_size: 1_000_000,
            routes: vec![RouteConfig {
                path: "/".into(),
                methods: vec![Method::Get, Method::Post, Method::Delete],
                root: Some("./www".into()),
                index: Some("index.html".into()),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}
EOF

cat > src/router.rs <<'EOF'
//! Owner: Person B — the bridge between network and application.
use crate::config::ServerConfig;
use crate::http::{Request, Response};

pub fn handle(req: &Request, server: &ServerConfig) -> Response {
    let _ = server; // TODO(B): route matching, methods, redirects, CGI, files
    Response::text(200, &format!("stub: {} {}\n", req.method, req.path))
}
EOF

cat > src/error.rs <<'EOF'
//! Owner: Person B — used by EVERYONE to build error responses.
use crate::config::ServerConfig;
use crate::http::{reason_phrase, Response};

pub fn error_response(status: u16, server: &ServerConfig) -> Response {
    let _ = server; // TODO(B): load custom page from server.error_pages
    let body = format!(
        "<!DOCTYPE html><html><body><h1>{} {}</h1></body></html>",
        status,
        reason_phrase(status)
    );
    Response::new(status)
        .with_header("Content-Type", "text/html; charset=utf-8")
        .with_body(body.into_bytes())
}
EOF

cat > src/static_files.rs <<'EOF'
//! Owner: Person B — files, index, directory listing, DELETE.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};

pub fn serve(req: &Request, route: &RouteConfig, server: &ServerConfig) -> Response {
    let _ = (req, route); // TODO(B)
    error_response(404, server)
}
EOF

cat > src/upload.rs <<'EOF'
//! Owner: Person B — multipart/form-data uploads.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};

pub fn handle_upload(req: &Request, route: &RouteConfig, server: &ServerConfig) -> Response {
    let _ = (req, route); // TODO(B)
    error_response(500, server)
}
EOF

cat > src/utils/mod.rs <<'EOF'
pub mod cookie;
pub mod session;
EOF

cat > src/utils/cookie.rs <<'EOF'
//! Owner: Person B
use std::collections::HashMap;

pub fn parse_cookies(header: &str) -> HashMap<String, String> {
    let _ = header; // TODO(B)
    HashMap::new()
}
EOF

cat > src/utils/session.rs <<'EOF'
//! Owner: Person B
use std::collections::HashMap;
use std::time::Instant;

pub struct Session {
    pub data: HashMap<String, String>,
    pub last_seen: Instant,
}

#[derive(Default)]
pub struct SessionStore {
    sessions: HashMap<String, Session>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}
EOF

# ───────────────────────────── Person C ─────────────────────────────
cat > src/cgi.rs <<'EOF'
//! Owner: Person C — CGI execution.
use std::path::Path;

use crate::config::RouteConfig;
use crate::http::{Request, Response};

pub fn run(req: &Request, route: &RouteConfig, script: &Path) -> Response {
    let _ = (req, route, script); // TODO(C)
    Response::text(501, "CGI not implemented yet\n")
}
EOF

cat > cgi-bin/hello.py <<'EOF'
#!/usr/bin/env python3
print("Content-Type: text/html")
print()
print("<h1>Hello from CGI</h1>")
EOF
chmod +x cgi-bin/hello.py

# ─────────────────────────── static assets ───────────────────────────
cat > www/index.html <<'EOF'
<!DOCTYPE html>
<html><head><title>LocalServer</title></head>
<body><h1>LocalServer is running</h1></body></html>
EOF
touch www/uploads/.gitkeep

for code in 400 403 404 405 413 500; do
  cat > "error_pages/$code.html" <<EOF
<!DOCTYPE html>
<html><head><title>Error $code</title></head>
<body><h1>Error $code</h1></body></html>
EOF
done

cat > config.yaml <<'EOF'
# Placeholder — Person B decides the final format and writes the parser.
servers:
  - host: 127.0.0.1
    ports: [8080]
    default: true
    client_max_body_size: 1000000
    error_pages:
      404: ./error_pages/404.html
    routes:
      - path: /
        root: ./www
        methods: [GET, POST, DELETE]
        index: index.html
        autoindex: false
EOF

cat > README.md <<'EOF'
# LocalServer

HTTP/1.1 server in Rust (single thread, mio event loop).

    cargo run -- config.yaml
    cargo test

Owners: A = network (server, connection, http/parser, http/serialize)
        B = app (config, router, static_files, upload, error, utils)
        C = cgi
EOF

echo "Done. Now run: cargo build && cargo test"
