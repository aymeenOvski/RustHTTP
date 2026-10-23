//! Owner: Person B — files, index, directory listing, DELETE.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};
use std::fs::{self, File};
use std::io;
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
            Ok(path) => path,
            Err(code) => return error_response(code, server),
        };

        match handle_delete(&target_path) {
            Ok(code) => return Response::new(code),
            Err(code) => return error_response(code, server),
        }
    }

    // Only allow GET requests for static files
    if req.method.to_string() != "GET" {
        return error_response(405, server); // Method Not Allowed
    }

    // Resolve and secure the path
    let target_path = match resolve_safe_path(&req.path, route_path_str, route_root_str) {
        Ok(path) => path,
        Err(code) => return error_response(code, server),
    };

    if !target_path.exists() {
        return error_response(404, server);
    }

    // Handle Directory vs File
    let result = if target_path.is_dir() {
        handle_directory(
            &target_path,
            &req.path,
            route.index.as_deref(),
            route.autoindex,
        )
    } else {
        serve_file(&target_path)
    };

    // Convert Result to Response
    match result {
        Ok(response) => response,
        Err(code) => error_response(code, server),
    }
}

// Safely resolves a URI path against a route root directory.
// Err(404): the file does not exist. Err(403): trying to leave the root.
// Err(500): the route's root folder itself is missing (config problem).
pub fn resolve_safe_path(uri_path: &str, route_path: &str, root_dir: &str) -> Result<PathBuf, u16> {
    let root_path = Path::new(root_dir);

    // Strip the route prefix from the requested URI
    let stripped = uri_path.strip_prefix(route_path).unwrap_or(uri_path);
    let clean_uri = stripped.trim_start_matches('/');

    // ".." means trying to climb out of the root: refuse before touching the disk.
    if clean_uri.split(['/', '\\']).any(|part| part == "..") {
        return Err(403);
    }

    let canonical_root = root_path.canonicalize().map_err(|_| 500u16)?;
    let canonical_target = match root_path.join(clean_uri).canonicalize() {
        Ok(path) => path,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(404),
        Err(_) => return Err(403), // e.g. permission denied
    };

    // A symbolic link could still point outside the root.
    if canonical_target.starts_with(&canonical_root) {
        Ok(canonical_target)
    } else {
        Err(403)
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

// Opens a file and builds a 200 OK HTTP response that sends it.
// The file is NOT read here: the network layer reads it piece by piece while
// it sends it, so a 1 GB file never has to fit in RAM. Content-Length is
// added from the file size.
pub fn serve_file(path: &Path) -> Result<Response, u16> {
    // Open the file (500 Internal Server Error if it cannot be opened)
    let file = File::open(path).map_err(|_| 500u16)?;
    let len = file.metadata().map_err(|_| 500u16)?.len();

    let mime_type = get_mime_type(path);

    Ok(Response::new(200)
        .with_header("Content-Type", mime_type)
        .with_file(file, len))
}

// Handles directory requests (checks for index.html or generates autoindex)
pub fn handle_directory(
    dir_path: &Path,
    request_uri: &str,
    index: Option<&str>, // NEW: the route's `index` from the config
    autoindex: bool,
) -> Result<Response, u16> {
    // The route's default file (config `index`), or index.html if not set.
    let index_path = dir_path.join(index.unwrap_or("index.html"));
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

    Ok(Response::new(200)
        .with_header("Content-Type", "text/html; charset=utf-8")
        .with_body(html.into_bytes()))
}

// Safely deletes a file or empty directory from disk
pub fn handle_delete(target_path: &Path) -> Result<u16, u16> {
    //  Check if the target exists on disk
    if !target_path.exists() {
        return Err(404); // Not Found
    }

    // Check if the target is writable
    let can_write = std::fs::OpenOptions::new().write(true).open(&target_path).is_ok();
    if !can_write {
        return Err(403); // Forbidden: cannot write/delete
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
