//! Owner: Person B — multipart/form-data uploads.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};

pub fn handle_upload(req: &Request, route: &RouteConfig, server: &ServerConfig) -> Response {
    let _ = (req, route); // TODO(B)
    error_response(500, server)
}
