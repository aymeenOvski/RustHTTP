//! Owner: Person C — CGI execution.
use std::path::Path;

use crate::config::RouteConfig;
use crate::http::{Request, Response};

pub fn run(req: &Request, route: &RouteConfig, script: &Path) -> Response {
    let _ = (req, route, script); // TODO(C)
    Response::text(501, "CGI not implemented yet\n")
}
