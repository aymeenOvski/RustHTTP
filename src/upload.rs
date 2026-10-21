//! Owner: Person B — multipart file upload handling.
//!
//! An upload is saved by an `UploadJob`: the event loop calls `step` once per
//! turn, and each call saves at most about one TURN_BUDGET of the body. So a
//! 1 GB upload never stops the other clients, and only a small window of
//! the body is in RAM at any time.
use crate::config::{RouteConfig, ServerConfig};
use crate::error::error_response;
use crate::http::{Request, Response};

use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bytes read from the body at a time.
const READ_PIECE: usize = 64 * 1024;
/// Longest accepted header block of one part (Content-Disposition, ...).
const MAX_PART_HEADERS: usize = 8 * 1024;

/// Checks the request and prepares the job that will save its files.
/// `Err` is the response to send right away (400, 500).
pub fn start_upload(
    req: &Request,
    route: &RouteConfig,
    server: &ServerConfig,
) -> Result<UploadJob, Response> {
    // Ensure upload_dir is configured for this route
    let upload_dir = match &route.upload_dir {
        Some(dir) => dir,
        None => return Err(error_response(500, server)),
    };
    let upload_path = Path::new(upload_dir);

    // Ensure the upload directory exists on disk
    if !upload_path.exists() && fs::create_dir_all(upload_path).is_err() {
        return Err(error_response(500, server));
    }

    // Inspect Content-Type header to ensure it's present
    let content_type = match req.header("content-type") {
        Some(val) => val,
        None => return Err(error_response(400, server)), // Bad Request: Missing Content-Type
    };

    // Extract the boundary string from Content-Type
    let boundary = match extract_boundary(content_type) {
        Some(b) if !b.is_empty() => b,
        _ => return Err(error_response(400, server)), // Bad Request: Invalid boundary
    };

    // A big body arrived in a temporary file, a small one in memory: the job
    // reads both the same way, piece by piece.
    let body = match &req.body_file {
        Some(body_file) => match body_file.take_file() {
            Ok(file) => Source::File(file),
            Err(_) => return Err(error_response(500, server)),
        },
        None => Source::Memory(Cursor::new(req.body.clone())), // at most SPOOL_ABOVE bytes
    };
    Ok(UploadJob::new(body, &boundary, upload_path))
}

/// Where the job reads the body from.
pub enum Source {
    /// A small body, copied from the request.
    Memory(Cursor<Vec<u8>>),
    /// A big body: the temporary file (its name is already deleted).
    File(File),
    /// Anything else (tests).
    Other(Box<dyn Read>),
}

impl Read for Source {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self {
            Source::Memory(c) => c.read(out),
            Source::File(f) => f.read(out),
            Source::Other(r) => r.read(out),
        }
    }
}

/// The response once the job is finished.
pub fn upload_response(result: Result<usize, u16>, server: &ServerConfig) -> Response {
    match result {
        Ok(0) => error_response(400, server), // the form contained no file
        Ok(_) => Response::new(201) // Created
            .with_header("Content-Type", "text/plain")
            .with_body(b"File(s) uploaded successfully".to_vec()),
        Err(code) => error_response(code, server),
    }
}

/// Where the job is in the multipart body.
enum Part {
    /// Before the first boundary: anything there is ignored.
    Preamble,
    /// Just after a boundary: "--" ends the body, "\r\n" starts a part.
    AfterBoundary,
    /// Reading the headers of a part, up to the empty line.
    Headers,
    /// Copying the data of a part into its file. `None`: a plain form field
    /// (no filename), read and thrown away.
    Data(Option<(PathBuf, File)>),
}

/// Saves the files of one multipart body, a piece at a time.
///
/// Dropped before the end (client gone, broken body, server stopping): the
/// file being written is deleted, so the upload folder never keeps half a
/// file.
pub struct UploadJob {
    /// `None` once the job is dropped.
    body: Option<Source>,
    /// Bytes read from the body and not used yet (a small window).
    buf: Vec<u8>,
    /// The body has no more bytes.
    ended: bool,
    /// "--boundary": the first boundary.
    first: Vec<u8>,
    /// "\r\n--boundary": inside the body, a boundary starts on a new line.
    delimiter: Vec<u8>,
    dir: PathBuf,
    part: Part,
    saved: usize,
}

impl UploadJob {
    pub fn new(body: Source, boundary: &str, dir: &Path) -> UploadJob {
        UploadJob {
            body: Some(body),
            buf: Vec::new(),
            ended: false,
            first: format!("--{boundary}").into_bytes(),
            delimiter: format!("\r\n--{boundary}").into_bytes(),
            dir: dir.to_path_buf(),
            part: Part::Preamble,
            saved: 0,
        }
    }

    /// Saves the next piece: reads at most about `budget` bytes of the body.
    ///
    /// - `None`: not finished, call again on the next turn.
    /// - `Some(Ok(n))`: done, `n` files saved.
    /// - `Some(Err(status))`: broken body (400) or disk error (500).
    pub fn step(&mut self, budget: usize) -> Option<Result<usize, u16>> {
        let mut read_now = 0;
        loop {
            match self.advance() {
                Ok(Some(saved)) => return Some(Ok(saved)),
                Ok(None) => {} // needs more bytes
                Err(code) => return Some(Err(code)),
            }
            if self.ended {
                return Some(Err(400)); // the body ends in the middle of a part
            }
            if read_now >= budget {
                return None; // the other clients' turn
            }
            match self.read_piece() {
                Ok(n) => read_now += n,
                Err(code) => return Some(Err(code)),
            }
        }
    }

    /// Uses the bytes already in `buf` as far as they go.
    /// `Ok(None)`: more bytes are needed. `Ok(Some(n))`: finished.
    fn advance(&mut self) -> Result<Option<usize>, u16> {
        loop {
            match &mut self.part {
                Part::Preamble => match find(&self.buf, &self.first) {
                    Some(i) => {
                        self.buf.drain(..i + self.first.len());
                        self.part = Part::AfterBoundary;
                    }
                    None => {
                        // Keep the end: the boundary may be cut between two pieces.
                        let keep = self.first.len() - 1;
                        if self.buf.len() > keep {
                            let drop = self.buf.len() - keep;
                            self.buf.drain(..drop);
                        }
                        return Ok(None);
                    }
                },
                Part::AfterBoundary => {
                    if self.buf.len() < 2 {
                        return Ok(None);
                    }
                    if self.buf.starts_with(b"--") {
                        return Ok(Some(self.saved)); // the closing boundary
                    }
                    if !self.buf.starts_with(b"\r\n") {
                        return Err(400);
                    }
                    self.buf.drain(..2);
                    self.part = Part::Headers;
                }
                Part::Headers => match find(&self.buf, b"\r\n\r\n") {
                    Some(i) => {
                        let name = extract_filename_from_headers(&self.buf[..i])
                            .and_then(|raw| sanitize_filename(&raw))
                            .filter(|name| !name.is_empty());
                        self.buf.drain(..i + 4);
                        let out = match name {
                            Some(name) => Some(create_unique(&self.dir, &name)?),
                            None => None,
                        };
                        self.part = Part::Data(out);
                    }
                    None if self.buf.len() > MAX_PART_HEADERS => return Err(400),
                    None => return Ok(None),
                },
                Part::Data(out) => match find(&self.buf, &self.delimiter) {
                    Some(i) => {
                        if let Some((_, file)) = out {
                            file.write_all(&self.buf[..i]).map_err(|_| 500u16)?;
                            self.saved += 1;
                        }
                        self.buf.drain(..i + self.delimiter.len());
                        self.part = Part::AfterBoundary; // the finished file is closed here
                    }
                    None => {
                        // Write all but the last bytes: they may be the start
                        // of the boundary.
                        let safe = self.buf.len().saturating_sub(self.delimiter.len() - 1);
                        if safe > 0 {
                            if let Some((_, file)) = out {
                                file.write_all(&self.buf[..safe]).map_err(|_| 500u16)?;
                            }
                            self.buf.drain(..safe);
                        }
                        return Ok(None);
                    }
                },
            }
        }
    }

    /// Reads one more piece of the body into `buf`. Ok(0) at the end.
    fn read_piece(&mut self) -> Result<usize, u16> {
        let Some(body) = self.body.as_mut() else {
            return Err(500);
        };
        let old = self.buf.len();
        self.buf.resize(old + READ_PIECE, 0);
        loop {
            match body.read(&mut self.buf[old..]) {
                Ok(n) => {
                    self.buf.truncate(old + n);
                    if n == 0 {
                        self.ended = true;
                    }
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.buf.truncate(old);
                    return Err(500);
                }
            }
        }
    }
}

impl Drop for UploadJob {
    fn drop(&mut self) {
        // Stopped in the middle of a file: never keep half of it.
        if let Part::Data(Some((path, _))) = &self.part {
            let _ = fs::remove_file(path);
        }
        // The temporary body file: its space is freed a piece per turn.
        if let Some(Source::File(file)) = self.body.take() {
            crate::spool::throw_away(file);
        }
    }
}

/// Creates a new file for an upload. Two files with the same name in the
/// same millisecond get different names instead of overwriting each other.
fn create_unique(dir: &Path, safe_name: &str) -> Result<(PathBuf, File), u16> {
    let base = make_unique_filename(safe_name);
    for n in 0..1000 {
        let name = if n == 0 { base.clone() } else { format!("{n}_{base}") };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(500),
        }
    }
    Err(500)
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

/// Position of `needle` in `haystack`. Jumps from one possible first byte to
/// the next, so it stays fast on big binary files.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let first = *needle.first()?;
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        let rest = &haystack[i..=haystack.len() - needle.len()];
        i += rest.iter().position(|&b| b == first)?;
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that gives one byte at a time: the worst case, every
    /// boundary is cut between two pieces.
    struct OneByte(Vec<u8>, usize);
    impl Read for OneByte {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.1 >= self.0.len() || out.is_empty() {
                return Ok(0);
            }
            out[0] = self.0[self.1];
            self.1 += 1;
            Ok(1)
        }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("upload_test_{}_{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn form(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (field, filename, data) in parts {
            body.extend_from_slice(b"--XyZ\r\nContent-Disposition: form-data; name=\"");
            body.extend_from_slice(field.as_bytes());
            body.push(b'"');
            if let Some(f) = filename {
                body.extend_from_slice(format!("; filename=\"{f}\"").as_bytes());
            }
            body.extend_from_slice(b"\r\n\r\n");
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--XyZ--\r\n");
        body
    }

    /// Runs a job to the end, like the event loop does (one step per turn).
    /// Returns the result and how many steps it took.
    fn run(body: Source, d: &Path, budget: usize) -> (Result<usize, u16>, usize) {
        let mut job = UploadJob::new(body, "XyZ", d);
        let mut steps = 1;
        loop {
            if let Some(result) = job.step(budget) {
                return (result, steps);
            }
            steps += 1;
        }
    }

    fn saved_files(d: &Path) -> Vec<Vec<u8>> {
        let mut names: Vec<_> = fs::read_dir(d).unwrap().flatten().map(|e| e.path()).collect();
        names.sort();
        names.iter().map(|p| fs::read(p).unwrap()).collect()
    }

    #[test]
    fn saves_a_file_with_the_exact_bytes() {
        let d = dir("exact");
        let data: Vec<u8> = (0..=255u8).cycle().take(300_000).collect(); // binary, has \r and \n
        let body = form(&[("file", Some("a.bin"), &data)]);
        assert_eq!(run(Source::Memory(Cursor::new(body)), &d, 1 << 20).0, Ok(1));
        assert_eq!(saved_files(&d), vec![data]);
    }

    #[test]
    fn a_big_body_is_saved_in_many_small_steps() {
        let d = dir("steps");
        let data = vec![b'q'; 3_000_000];
        let body = form(&[("file", Some("big.bin"), &data)]);
        // 64 KB per step: the event loop gets back control ~46 times.
        let (result, steps) = run(Source::Memory(Cursor::new(body)), &d, 64 * 1024);
        assert_eq!(result, Ok(1));
        assert!(steps > 40, "only {steps} steps");
        assert_eq!(saved_files(&d), vec![data]);
    }

    #[test]
    fn boundaries_cut_between_pieces_are_found() {
        // One byte per read, and data that looks almost like a boundary.
        let d = dir("onebyte");
        let data = b"line1\r\n--Xy not a boundary\r\n-- still data".to_vec();
        let body = form(&[("note", None, b"ignored field"), ("file", Some("t.txt"), &data)]);
        assert_eq!(run(Source::Other(Box::new(OneByte(body, 0))), &d, 1 << 20).0, Ok(1));
        assert_eq!(saved_files(&d), vec![data]);
    }

    #[test]
    fn two_files_in_one_form() {
        let d = dir("two");
        let body = form(&[("a", Some("a.txt"), b"first"), ("b", Some("b.txt"), b"second")]);
        assert_eq!(run(Source::Memory(Cursor::new(body)), &d, 1 << 20).0, Ok(2));
        let mut got = saved_files(&d);
        got.sort();
        assert_eq!(got, vec![b"first".to_vec(), b"second".to_vec()]);
    }

    #[test]
    fn a_form_without_file_saves_nothing() {
        let d = dir("nofile");
        let body = form(&[("name", None, b"ali")]);
        assert_eq!(run(Source::Memory(Cursor::new(body)), &d, 1 << 20).0, Ok(0));
        assert!(saved_files(&d).is_empty());
    }

    #[test]
    fn a_cut_body_is_400_and_leaves_no_half_file() {
        let d = dir("cut");
        let mut body = form(&[("file", Some("a.bin"), &[7u8; 5000])]);
        body.truncate(3000); // the client stopped in the middle
        let mut job = UploadJob::new(Source::Memory(Cursor::new(body)), "XyZ", &d);
        assert_eq!(job.step(1 << 20), Some(Err(400)));
        drop(job); // the server drops the job after an error
        assert!(saved_files(&d).is_empty());
    }

    #[test]
    fn a_job_dropped_in_the_middle_leaves_no_half_file() {
        let d = dir("dropped");
        let body = form(&[("file", Some("a.bin"), &vec![1u8; 2_000_000])]);
        let mut job = UploadJob::new(Source::Memory(Cursor::new(body)), "XyZ", &d);
        assert_eq!(job.step(64 * 1024), None); // started...
        assert_eq!(fs::read_dir(&d).unwrap().count(), 1); // ...half a file on disk
        drop(job); // the client left: the connection is closed
        assert!(saved_files(&d).is_empty());
    }

    #[test]
    fn a_path_in_the_filename_is_removed() {
        let d = dir("traversal");
        let body = form(&[("file", Some("../../evil.txt"), b"x")]);
        assert_eq!(run(Source::Memory(Cursor::new(body)), &d, 1 << 20).0, Ok(1));
        let name = fs::read_dir(&d).unwrap().next().unwrap().unwrap().file_name();
        assert!(name.to_string_lossy().ends_with("_evil.txt"));
    }

    #[test]
    fn find_works_on_binary_data() {
        assert_eq!(find(b"aaa\r\n--XyZ", b"\r\n--XyZ"), Some(3));
        assert_eq!(find(b"\r\r\n--X", b"\r\n--X"), Some(1));
        assert_eq!(find(b"abc", b"abcd"), None);
        assert_eq!(find(b"", b"x"), None);
    }
}
