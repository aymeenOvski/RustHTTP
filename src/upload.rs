//! Owner: Person B — multipart/form-data uploads.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};

pub fn handle_upload(req: &Request, route: &RouteConfig, server: &ServerConfig) -> Response {
    let _ = (req, route); // TODO(B): check if upload folder is set, parse the file data from the request body, and save it to disk
    error_response(500, server)
}
