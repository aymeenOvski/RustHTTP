//! Owner: Person B — the bridge between network and application.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};
use crate::{static_files, upload, cgi};

pub fn handle(req: &Request, server: &ServerConfig) -> Response {
    // Find the best matching route using prefix matching
    let matched_route = find_matching_route(&req.path, &server.routes);

    let route = match matched_route {
        Some(r) => r,
        None => return error_response(404, server), // No route matches -> 404 Not Found
    };

    // Validate if the HTTP method is allowed for this route
    if !route.methods.contains(&req.method) {
        return error_response(405, server); // Method not allowed -> 405 Method Not Allowed
    }

    // Handle Redirections
    if let Some(ref target) = route.redirect {
        return Response::new(301)
            .with_header("Location", target)
            .with_body(b"Moved Permanently".to_vec());
    }

    // Dispatch to CGI if the file extension matches a CGI rule
    // We check if the request path ends with any registered CGI extension (e.g., ".py")
    for (ext, _interpreter) in &route.cgi {
        if req.path.ends_with(ext) {
            if let Some(ref root) = route.root {
                // Strip the route prefix (e.g., "/cgi") so we get just "/hello.py"
                let relative_path = req.path.strip_prefix(&route.path).unwrap_or(&req.path);
                
                // Now root ("cgi-bin") + relative_path ("/hello.py") becomes "cgi-bin/hello.py"
                let script_path = root.join(relative_path.trim_start_matches('/'));
                
                return cgi::run(req, route, &script_path);
            }
        }
    }

    // Dispatch to Upload Handler if it's a POST request and an upload directory is set
    if req.method == crate::http::Method::Post && route.upload_dir.is_some() {
        return upload::handle_upload(req, route, server);
    }

    // Default Fallback: Hand off to Static Files (Handles GET, file downloads, autoindex, and DELETE)
    static_files::serve(req, route, server)
}

// Helper function to find the longest matching route prefix
fn find_matching_route<'a>(path: &str, routes: &'a [RouteConfig]) -> Option<&'a RouteConfig> {
    let mut best_match: Option<&RouteConfig> = None;
    let mut longest_len = 0;

    for route in routes {
        // Check if the request path matches the route exactly or starts with it as a subdirectory
        let is_match = if route.path == "/" {
            path == "/"
        } else {
            path == route.path || path.starts_with(&format!("{}/", route.path))
        };

        if is_match {
            if route.path.len() > longest_len {
                longest_len = route.path.len();
                best_match = Some(route);
            }
        }
    }

    best_match
}