//! Owner: Person B — the bridge between network and application.
use crate::config::ServerConfig;
use crate::http::{Request, Response};

pub fn handle(req: &Request, server: &ServerConfig) -> Response {
    let _ = server; // TODO(B): route matching, methods, redirects, CGI, files
    Response::text(200, &format!("stub: {} {}\n", req.method, req.path))
}
