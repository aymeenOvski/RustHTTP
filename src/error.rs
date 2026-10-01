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
