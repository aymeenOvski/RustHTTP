//! Owner: Person B — files, index, directory listing, DELETE.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};

use std::fs;
use std::path::{Path, PathBuf};

pub fn serve(req: &Request, route: &RouteConfig, server: &ServerConfig) -> Response {
    let route_path_str = route.path.as_str();
    
    let route_root_str = match route.root.as_ref().and_then(|p| p.to_str()) {
        Some(s) => s,
        None => "",
    };

    //  Handle DELETE method
    if req.method.to_string() == "DELETE" {
        let target_path = match resolve_safe_path(&req.path, route_path_str, route_root_str) {
            Some(path) => path,
            None => return error_response(403, server),
        };

        match handle_delete(&target_path) {
            Ok(code) => {
                return Response {
                    status: code,
                    headers: vec![],
                    body: vec![],
                };
            }
            Err(code) => return error_response(code, server),
        }
    }

    // Only allow GET requests for static files
    if req.method.to_string() != "GET" {
        return error_response(405, server); // Method Not Allowed
    }

    // Resolve and secure the path
    let target_path = match resolve_safe_path(&req.path, route_path_str, route_root_str) {
        Some(path) => path,
        None => return error_response(403, server), // Forbidden / Path traversal
    };

    if !target_path.exists() {
        return error_response(404, server);
    }

    // Handle Directory vs File
    let result = if target_path.is_dir() {
        handle_directory(&target_path, &req.path, route.autoindex)
    } else {
        serve_file(&target_path)
    };

    // Convert Result to Response
    match result {
        Ok((headers, body)) => Response {
            status: 200,
            headers,
            body,
        },
        Err(code) => error_response(code, server),
    }
}

// Safely resolves a URI path against a route root directory.
// Prevents directory traversal attacks.
pub fn resolve_safe_path(uri_path: &str, route_path: &str, root_dir: &str) -> Option<PathBuf> {
    let root_path = Path::new(root_dir);

    // Strip the route prefix from the requested URI
    let stripped = if uri_path.starts_with(route_path) {
        &uri_path[route_path.len()..]
    } else {
        uri_path
    };

    // Clean leading slashes and join with the root directory
    let clean_uri = stripped.trim_start_matches('/');
    let full_path = root_path.join(clean_uri);

    // Security check: Ensure the canonicalized path stays within the root
    let canonical_root = root_path.canonicalize().ok()?;

    let canonical_target = full_path.canonicalize().ok()?;

    if canonical_target.starts_with(canonical_root) {
        Some(canonical_target)
    } else {
        None // Traversal attempt detected!
    }
}

// A lightweight helper to map file extensions to MIME types
pub fn get_mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain",
        _ => "application/octet-stream", // Default binary fallback
    }
}

// Reads a file from disk and builds a 200 OK HTTP response
pub fn serve_file(path: &Path) -> Result<(Vec<(String, String)>, Vec<u8>), u16> {
    // Read file bytes from disk (returns 500 Internal Server Error if read fails)
    let contents = match fs::read(path) {
        Ok(data) => data,
        Err(_) => return Err(500),
    };

    let mime_type = get_mime_type(path);

    // Build headers
    let headers = vec![
        ("Content-Type".to_string(), mime_type.to_string()),
        ("Content-Length".to_string(), contents.len().to_string()),
    ];

    Ok((headers, contents))
}

// Handles directory requests (checks for index.html or generates autoindex)
pub fn handle_directory(
    dir_path: &Path,
    request_uri: &str,
    autoindex: bool,
) -> Result<(Vec<(String, String)>, Vec<u8>), u16> {
    // Check for a default index file
    let index_path = dir_path.join("index.html");
    if index_path.is_file() {
        return serve_file(&index_path);
    }

    // If no index file, check if autoindex is enabled
    if !autoindex {
        return Err(403); // Forbidden if autoindex is off
    }

    // Generate HTML directory listing
    let mut html = String::from("<html><head><title>Index of ");
    html.push_str(request_uri);
    html.push_str("</title></head><body><h1>Index of ");
    html.push_str(request_uri);
    html.push_str("</h1><hr><ul>");

    // Read directory contents
    // let entries = fs::read_dir(dir_path).map_err(|_| 500)?;
    let entries = match fs::read_dir(dir_path) {
        Ok(entries) => entries,
        Err(_) => return Err(500),
    };

    for entry in entries.flatten() {
        if let Ok(file_name) = entry.file_name().into_string() {
            let mut href = format!("{}/{}", request_uri.trim_end_matches('/'), file_name);
            if entry.path().is_dir() {
                href.push('/'); // important for the browser to recognize it as a directory
            }
            html.push_str(&format!("<li><a href=\"{}\">{}</a></li>", href, file_name));
        }
    }

    html.push_str("</ul><hr></body></html>");
    let body = html.into_bytes();

    let headers = vec![
        (
            "Content-Type".to_string(),
            "text/html; charset=utf-8".to_string(),
        ),
        ("Content-Length".to_string(), body.len().to_string()),
    ];

    Ok((headers, body))
}

// Safely deletes a file or empty directory from disk
pub fn handle_delete(target_path: &Path) -> Result<u16, u16> {
    //  Check if the target exists on disk
    if !target_path.exists() {
        return Err(404); // Not Found
    }

    // Delete file vs empty directory
    let result = if target_path.is_dir() {
        // ! fs::remove_dir only deletes *empty* directories safely
        fs::remove_dir(target_path)
    } else {
        fs::remove_file(target_path)
    };

    // Return success code 204 (No Content) or 500 on failure
    match result {
        Ok(_) => Ok(204),   // HTTP 204 No Content
        Err(_) => Err(500), // Internal Server Error (e.g., permission denied)
    }
}
