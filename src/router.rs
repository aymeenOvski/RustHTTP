//! Owner: Person B — the bridge between network and application.
use std::sync::{LazyLock, Mutex};

use crate::utils::cookie::parse_cookies;
use crate::utils::session::SessionStore;

use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};
use crate::{cgi, static_files, upload};

static SESSION_STORE: LazyLock<Mutex<SessionStore>> =
    LazyLock::new(|| Mutex::new(SessionStore::new()));

pub fn handle(req: &Request, server: &ServerConfig) -> Response {
    // Parse incoming cookies and check for an active session ID
    let mut store = SESSION_STORE.lock().unwrap();
    let mut session_id = None;

    if let Some(cookie_header) = req.header("cookie") {
        let cookies = parse_cookies(cookie_header);
        if let Some(id) = cookies.get("session_id") {
            session_id = Some(id.clone());
        }
    }

    // Validate session or create a new one if missing/expired
    let mut is_new_session = false;
    let _session = match session_id {
        Some(ref id) => store.get_session(id),
        None => None,
    };

    let actual_session_id = match session_id {
        Some(id) if store.get_session(&id).is_some() => id,
        _ => {
            is_new_session = true;
            store.create_session()
        }
    };

    drop(store);

    // Find the best matching route using prefix matching
    let matched_route = find_matching_route(&req.path, &server.routes);

    let route = match matched_route {
        Some(r) => r,
        None => return error_response(404, server), // No route matches -> 404 Not Found
    };
    
    // Handle Redirections
    if let Some(ref target) = route.redirect {
        return Response::new(301)
            .with_header("Location", target)
            .with_body(b"Moved Permanently".to_vec());
    }
    
    // Validate if the HTTP method is allowed for this route
    if !route.methods.contains(&req.method) {
        return error_response(405, server); // Method not allowed -> 405 Method Not Allowed
    }


    // Dispatch to CGI if the file extension matches a CGI rule
    for (ext, _interpreter) in &route.cgi {
        if req.path.ends_with(ext) {
            if let Some(ref root) = route.root {
                let relative_path = req.path.strip_prefix(&route.path).unwrap_or(&req.path);
                let script_path = root.join(relative_path.trim_start_matches('/'));
                let mut resp = cgi::run(req, route, &script_path);
                if is_new_session {
                    resp = resp.with_header(
                        "Set-Cookie",
                        &format!("session_id={actual_session_id}; Path=/; HttpOnly"),
                    );
                }
                return resp;
            }
        }
    }

    // Dispatch to Upload Handler if it's a POST request and an upload directory is set
    if req.method == crate::http::Method::Post && route.upload_dir.is_some() {
        let mut resp = upload::handle_upload(req, route, server);
        if is_new_session {
            resp = resp.with_header(
                "Set-Cookie",
                &format!("session_id={actual_session_id}; Path=/; HttpOnly"),
            );
        }
        return resp;
    }

    // Default Fallback: Hand off to Static Files (Handles GET, file downloads, autoindex, and DELETE)
    let mut resp = static_files::serve(req, route, server);

    if is_new_session {
        resp = resp.with_header(
            "Set-Cookie",
            &format!("session_id={actual_session_id}; Path=/; HttpOnly"),
        );
    }

    resp
}

// Helper function to find the longest matching route prefix
fn find_matching_route<'a>(path: &str, routes: &'a [RouteConfig]) -> Option<&'a RouteConfig> {
    let mut best_match: Option<&RouteConfig> = None;
    let mut longest_len = 0;

    for route in routes {
        // The route "/docs" matches "/docs" and "/docs/...", but not "/docsx".
        // The route "/" (prefix "") matches every path.
        let prefix = route.path.trim_end_matches('/');
        let is_match = path == prefix || path.starts_with(&format!("{prefix}/"));

        if is_match {
            if route.path.len() > longest_len {
                longest_len = route.path.len();
                best_match = Some(route);
            }
        }
    }

    best_match
}
