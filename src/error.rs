//! Owner: Person B — used by EVERYONE to build error responses.
use crate::config::ServerConfig;
use crate::http::{reason_phrase, Response};

pub fn error_response(status: u16, server: &ServerConfig) -> Response {
    let _ = server; // TODO(B): check if a custom error html page exists for this status code, load it if found, or use the default text.
    let body = format!(
        "<!DOCTYPE html><html><body><h1>{} {}</h1></body></html>",
        status,
        reason_phrase(status)
    );
    Response::new(status)
        .with_header("Content-Type", "text/html; charset=utf-8")
        .with_body(body.into_bytes())
}
