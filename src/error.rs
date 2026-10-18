//! Owner: Person B — used by EVERYONE to build error responses.
use std::fs;
use crate::config::ServerConfig;
use crate::http::{reason_phrase, Response};

pub fn error_response(status: u16, server: &ServerConfig) -> Response {
    // Check if the server configuration defines a custom error page for this status code
    if let Some(path) = server.error_pages.get(&status) {
        // Try reading the custom error page file from disk
        if let Ok(body_bytes) = fs::read(path) {
            return Response::new(status)
                .with_header("Content-Type", "text/html; charset=utf-8")
                .with_body(body_bytes);
        }
    }

    // Fallback: Generate a clean default HTML error page if no custom file exists or reading failed
    let body = format!(
        "<!DOCTYPE html><html><head><title>Error {status}</title></head><body><h1>{} {}</h1></body></html>",
        status,
        reason_phrase(status)
    );

    Response::new(status)
        .with_header("Content-Type", "text/html; charset=utf-8")
        .with_body(body.into_bytes())
}