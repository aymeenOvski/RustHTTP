//! Owner: Person B — the bridge between network and application.
use std::sync::{LazyLock, Mutex};

use crate::utils::cookie::parse_cookies;
use crate::utils::session::SessionStore;

use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};
use crate::{static_files, upload};

use crate::cgi::CgiJob;
use crate::upload::UploadJob;
use std::io;

pub enum Outcome {
    Response(Response),
    Cgi {
        job: CgiJob,
        set_cookie: Option<String>,
    },
    /// An upload to save: the event loop runs the job a piece per turn,
    /// then sends `upload::upload_response`.
    Upload {
        job: UploadJob,
        set_cookie: Option<String>,
    },
}

static SESSION_STORE: LazyLock<Mutex<SessionStore>> =
    LazyLock::new(|| Mutex::new(SessionStore::new()));

pub fn handle(req: &Request, server: &ServerConfig) -> Response {
    match dispatch(req, server) {
        Outcome::Response(r) => r,
        Outcome::Cgi { .. } => Response::text(501, "CGI needs the event loop\n"),
        // Without the event loop (tests): save it all at once.
        Outcome::Upload { mut job, .. } => loop {
            if let Some(result) = job.step(usize::MAX) {
                break upload::upload_response(result, server);
            }
        },
    }
}

pub fn dispatch(req: &Request, server: &ServerConfig) -> Outcome {
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
        None => return Outcome::Response(error_response(404, server)), // No route matches -> 404 Not Found
    };

    // Handle Redirections
    if let Some(ref target) = route.redirect {
        return Outcome::Response(
            Response::new(301)
                .with_header("Location", target)
                .with_body(b"Moved Permanently".to_vec()),
        );
    }

    // Validate if the HTTP method is allowed for this route
    if !route.methods.contains(&req.method) {
        return Outcome::Response(error_response(405, server)); // Method not allowed -> 405 Method Not Allowed
    }

    // Dispatch to CGI if the file extension matches a CGI rule
    for (ext, _interpreter) in &route.cgi {
        if req.path.ends_with(ext) {
            if let Some(ref root) = route.root {
                let relative_path = req.path.strip_prefix(&route.path).unwrap_or(&req.path);
                let script_path = root.join(relative_path.trim_start_matches('/'));
                return match CgiJob::start(req, route, &script_path) {
                    Ok(job) => Outcome::Cgi {
                        job,
                        set_cookie: is_new_session
                            .then(|| format!("session_id={actual_session_id}; Path=/; HttpOnly")),
                    },
                    Err(e) => {
                        let status = match e.kind() {
                            io::ErrorKind::NotFound => 404,
                            io::ErrorKind::PermissionDenied => 403,
                            _ => 500,
                        };
                        Outcome::Response(error_response(status, server))
                    }
                };
            }
        }
    }

    // Dispatch to Upload Handler if it's a POST request and an upload directory is set
    if req.method == crate::http::Method::Post && route.upload_dir.is_some() {
        let set_cookie =
            is_new_session.then(|| format!("session_id={actual_session_id}; Path=/; HttpOnly"));
        return match upload::start_upload(req, route, server) {
            Ok(job) => Outcome::Upload { job, set_cookie },
            Err(mut resp) => {
                if let Some(cookie) = &set_cookie {
                    resp = resp.with_header("Set-Cookie", cookie);
                }
                Outcome::Response(resp)
            }
        };
    }

    // Default Fallback: Hand off to Static Files (Handles GET, file downloads, autoindex, and DELETE)
    let mut resp = static_files::serve(req, route, server);

    if is_new_session {
        resp = resp.with_header(
            "Set-Cookie",
            &format!("session_id={actual_session_id}; Path=/; HttpOnly"),
        );
    }

    Outcome::Response(resp)
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

// ───────────────────────── route matching tests ─────────────────────────

#[cfg(test)]
mod route_matching_tests {
    use super::*;
    use crate::http::Method;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn route(path: &str, root: Option<&str>) -> RouteConfig {
        RouteConfig {
            path: path.to_string(),
            methods: vec![Method::Get],
            root: root.map(PathBuf::from),
            ..Default::default()
        }
    }

    /// The routes of test/test.conf (server "main").
    fn routes() -> Vec<RouteConfig> {
        let mut old = route("/old", None);
        old.redirect = Some("/".to_string());
        let mut cgi = route("/cgi-bin", Some("test/www/cgi-bin"));
        cgi.cgi
            .insert(".py".to_string(), PathBuf::from("/usr/bin/python3"));
        vec![
            route("/", Some("test/www")),
            route("/docs", Some("test/www/docs")),
            route("/upload", Some("test/www/uploads")),
            old,
            cgi,
        ]
    }

    /// The path of the route chosen for `request_path`.
    fn matched(request_path: &str, routes: &[RouteConfig]) -> Option<String> {
        find_matching_route(request_path, routes).map(|r| r.path.clone())
    }

    fn request(method: Method, path: &str) -> Request {
        Request {
            method,
            path: path.to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
            body_file: None,
        }
    }

    #[test]
    fn files_in_the_site_root_use_the_root_route() {
        let routes = routes();
        for path in ["/", "/about.html", "/css/style.css"] {
            assert_eq!(matched(path, &routes).as_deref(), Some("/"), "{path}");
        }
    }

    #[test]
    fn a_route_matches_itself_and_everything_under_it() {
        let routes = routes();
        for path in ["/docs", "/docs/", "/docs/one.txt", "/docs/sub/two.txt"] {
            assert_eq!(matched(path, &routes).as_deref(), Some("/docs"), "{path}");
        }
    }

    #[test]
    fn a_similar_name_is_not_under_the_route() {
        let routes = routes();
        assert_eq!(matched("/docsx", &routes).as_deref(), Some("/"));
        assert_eq!(matched("/documents/a.txt", &routes).as_deref(), Some("/"));
    }

    #[test]
    fn the_longest_matching_route_wins() {
        let mut routes = routes();
        routes.push(route("/docs/private", Some("test/www/private")));
        assert_eq!(
            matched("/docs/private/a.txt", &routes).as_deref(),
            Some("/docs/private")
        );
        assert_eq!(matched("/docs/a.txt", &routes).as_deref(), Some("/docs"));
    }

    #[test]
    fn a_trailing_slash_in_the_config_changes_nothing() {
        let routes = vec![route("/docs/", Some("test/www/docs"))];
        assert_eq!(matched("/docs", &routes).as_deref(), Some("/docs/"));
        assert_eq!(matched("/docs/one.txt", &routes).as_deref(), Some("/docs/"));
    }

    #[test]
    fn without_a_root_route_unknown_paths_match_nothing() {
        let routes = vec![route("/docs", Some("test/www/docs"))];
        assert_eq!(matched("/nothing", &routes), None);
    }

    #[test]
    fn requests_reach_the_right_directory_root() {
        let routes = routes();
        let found = find_matching_route("/docs/one.txt", &routes).unwrap();
        assert_eq!(found.root, Some(PathBuf::from("test/www/docs")));
    }

    #[test]
    fn redirections_and_cgi_routes_are_found() {
        let routes = routes();
        let old = find_matching_route("/old", &routes).unwrap();
        assert_eq!(old.redirect.as_deref(), Some("/"));
        let cgi = find_matching_route("/cgi-bin/hello.py", &routes).unwrap();
        assert!(cgi.cgi.contains_key(".py"));
    }

    // The router's status codes, for cases that never touch the disk.

    #[test]
    fn redirect_gives_301_with_location() {
        let server = ServerConfig {
            routes: routes(),
            ..Default::default()
        };
        let response = handle(&request(Method::Get, "/old"), &server);
        assert_eq!(response.status, 301);
        assert!(
            response
                .headers
                .iter()
                .any(|(n, v)| n == "Location" && v == "/")
        );
    }

    #[test]
    fn method_not_allowed_gives_405() {
        let server = ServerConfig {
            routes: routes(),
            ..Default::default()
        };
        let response = handle(&request(Method::Delete, "/docs"), &server);
        assert_eq!(response.status, 405);
    }

    #[test]
    fn no_matching_route_gives_404() {
        let server = ServerConfig {
            routes: vec![route("/docs", Some("test/www/docs"))],
            ..Default::default()
        };
        let response = handle(&request(Method::Get, "/nothing"), &server);
        assert_eq!(response.status, 404);
    }
}
