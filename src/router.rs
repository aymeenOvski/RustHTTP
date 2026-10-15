//! Owner: Person B — the bridge between network and application.
use crate::config::ServerConfig;
use crate::http::{Request, Response};

pub fn handle(req: &Request, server: &ServerConfig) -> Response {
    let _ = server; // TODO(B): match the request to a route in the server config, checks methods, redirects, CGI, files and delegate to the right handler (return the appropriate response).
    Response::text(200, &format!("stub: {} {}\n", req.method, req.path))
}
