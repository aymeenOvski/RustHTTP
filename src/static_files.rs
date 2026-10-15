//! Owner: Person B — files, index, directory listing, DELETE.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};

pub fn serve(req: &Request, route: &RouteConfig, server: &ServerConfig) -> Response {
    let _ = (req, route); // TODO(B): use the config route rules to find the file in the root folder, check for index/autoindex, or delete it.
    error_response(404, server)
}
