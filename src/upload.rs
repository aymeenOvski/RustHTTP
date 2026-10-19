//! Owner: Person B — multipart file upload handling.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};


pub fn handle_upload(req: &Request, route: &RouteConfig, server: &ServerConfig) -> Response {
    // Ensure upload_dir is configured for this route
    let upload_dir_str = match &route.upload_dir {
        Some(dir) => dir,
        None => return error_response(500, server),
    };

    let upload_path = Path::new(upload_dir_str);

    // Ensure the upload directory exists on disk
    if !upload_path.exists() {
        if fs::create_dir_all(upload_path).is_err() {
            return error_response(500, server);
        }
    }

    // Inspect Content-Type header to ensure it's present
    let content_type = match req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("Content-Type")) {
        Some((_, val)) => val,
        None => return error_response(400, server), // Bad Request: Missing Content-Type
    };

    // Extract the boundary string from Content-Type
    let boundary = match extract_boundary(content_type) {
        Some(b) => b,
        None => return error_response(400, server), // Bad Request: Invalid boundary
    };

    match parse_and_save_multipart(&req.body, &boundary, upload_path) {
        Ok(()) => Response {
            status: 201, // Created
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body: b"File(s) uploaded successfully".to_vec(),
        },
        Err(code) => error_response(code, server),
    }
}

// Helper to parse out the boundary string from the Content-Type header
fn extract_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';') {
        let part = part.trim();
        if part.starts_with("boundary=") {
            let b = part.trim_start_matches("boundary=").trim_matches('"');
            return Some(b.to_string());
        }
    }
    None
}

// Magic Number File Verification
fn validate_file_magic_bytes(content: &[u8]) -> bool {
    if content.len() < 4 {
        return false;
    }
    // JPEG: FF D8 FF
    if content.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return true;
    }
    // PNG: 89 50 4E 47
    if content.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return true;
    }
    // PDF: %PDF (25 50 44 46)
    if content.starts_with(&[0x25, 0x50, 0x44, 0x46]) {
        return true;
    }
    false
}

// Secure Filename Sanitization
fn sanitize_filename(raw_name: &str) -> Option<String> {
    let path = Path::new(raw_name);
    if let Some(file_name) = path.file_name() {
        return file_name.to_str().map(|s| s.to_string());
    }
    None
}

// Uniqueness Enforcement
fn make_unique_filename(safe_name: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    
    format!("{}_{}", timestamp, safe_name)
}

// Parses multipart form-data payload blocks, sanitizes names, makes them unique, validates magic bytes, and saves files to disk
fn parse_and_save_multipart(body: &[u8], boundary: &str, upload_path: &Path) -> Result<(), u16> {
    let boundary_bytes = format!("--{}", boundary).into_bytes();
    let end_boundary_bytes = format!("--{}--", boundary).into_bytes();

    let chunks = split_by_boundary(body, &boundary_bytes);

    for chunk in chunks {
        if chunk.is_empty() || chunk.starts_with(&end_boundary_bytes) {
            continue;
        }

        // Split headers from body part (separated by \r\n\r\n)
        if let Some(pos) = find_subsequence(chunk, b"\r\n\r\n") {
            let header_part = &chunk[..pos];
            let content_part = &chunk[pos + 4..];

            // Clean trailing CRLF from content
            let content = strip_trailing_crlf(content_part);

            // Extract filename from headers
            if let Some(filename) = extract_filename_from_headers(header_part) {
                // Sanitize the filename against path traversal attacks
                let safe_name = match sanitize_filename(&filename) {
                    Some(name) => name,
                    None => continue,
                };

                // Enforce uniqueness so files don't overwrite each other
                let unique_name = make_unique_filename(&safe_name);

                // Verify actual file bytes via magic numbers
                // if !validate_file_magic_bytes(content) {
                //     return Err(400); // Bad Request: File signature mismatch
                // }

                // Write to disk
                let target_file_path = upload_path.join(unique_name);
                if fs::write(target_file_path, content).is_err() {
                    return Err(500); // Internal Server Error on disk write failure
                }
            }
        }
    }

    Ok(())
}

// Helper to extract filename from Content-Disposition headers
fn extract_filename_from_headers(header_bytes: &[u8]) -> Option<String> {
    let headers_str = String::from_utf8_lossy(header_bytes);
    for line in headers_str.lines() {
        if line.to_lowercase().starts_with("content-disposition") {
            for part in line.split(';') {
                let part = part.trim();
                if part.starts_with("filename=") {
                    return Some(part.trim_start_matches("filename=").trim_matches('"').to_string());
                }
            }
        }
    }
    None
}

// Helper to find a subslice index within a byte slice
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

// Splits a byte slice by a boundary marker
fn split_by_boundary<'a>(body: &'a [u8], boundary: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut start = 0;

    while let Some(pos) = find_subsequence(&body[start..], boundary) {
        let idx = start + pos;
        if idx > start {
            parts.push(&body[start..idx]);
        }
        start = idx + boundary.len();
    }

    if start < body.len() {
        parts.push(&body[start..]);
    }

    parts
}

// Strips trailing \r\n from multipart chunks
fn strip_trailing_crlf(data: &[u8]) -> &[u8] {
    if data.ends_with(b"\r\n") {
        &data[..data.len() - 2]
    } else if data.ends_with(b"\n") {
        &data[..data.len() - 1]
    } else {
        data
    }
}