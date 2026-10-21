//! Owner: Person C — CGI execution.
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::config::RouteConfig;
use crate::http::{Request, Response};

use mio::unix::pipe::{Receiver, Sender};
use std::time::Instant;

// Most bytes of output we accept from one script. Protects our memory.
const MAX_OUTPUT: usize = 10 * 1024 * 1024; // 10 MB

#[derive(Debug, PartialEq)]
pub enum CgiRead {
    // Nothing more right now: wait for the next event.
    WouldBlock,
    // The script closed its output: it is finished.
    Eof,
    TooLarge,
    // The pipe failed.
    Failed,
}

// starts a CGI process and returns the Child handle for further interaction
pub fn start_cgi_process(req: &Request, route: &RouteConfig, script: &Path) -> io::Result<Child> {
    // user's script path
    let script = script.canonicalize()?;
    let script_dir = script
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "script has no folder"))?;

    // Find the interpreter for the script's extension.
    let ext = script
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let interpreter = route.cgi.get(&ext).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no interpreter for this extension")
    })?;

    let mut cmd = Command::new(interpreter);
    cmd.arg(&script);
    cmd.current_dir(script_dir);

    cmd.env_clear(); // start from an EMPTY environment: no secrets inherited
    // Python still needs a PATH to work, so we copy only that one.
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    cmd.env("PATH", path);

    // CGI root path
    let root = route
        .root
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "route has no root"))?
        .canonicalize()?;

    if !script.starts_with(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "script is outside the root folder",
        ));
    }

    // Environment variables: set the CGI-required ones.
    cmd.env("GATEWAY_INTERFACE", "CGI/1.1");
    cmd.env("SERVER_PROTOCOL", &req.version);
    cmd.env("REQUEST_METHOD", req.method.to_string());
    cmd.env("QUERY_STRING", req.query.as_deref().unwrap_or(""));
    cmd.env("CONTENT_LENGTH", req.body_len().to_string());
    cmd.env("CONTENT_TYPE", req.header("content-type").unwrap_or(""));
    cmd.env("PATH_INFO", &req.path);
    cmd.env("SCRIPT_NAME", &req.path);
    cmd.env("SCRIPT_FILENAME", &script);

    // Additional headers
    for (name, value) in &req.headers {
        if name == "content-length" || name == "content-type" || name == "proxy" {
            continue;
        }
        let key = format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_"));
        cmd.env(key, value);
    }

    // A big body is already in a temporary file: that file becomes the
    // script's stdin directly, nothing to copy. A small one is written into a
    // pipe by the event loop. The answer is read from stdout (a pipe).
    match &req.body_file {
        Some(body_file) => cmd.stdin(Stdio::from(body_file.take_file()?)),
        None => cmd.stdin(Stdio::piped()),
    };
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());

    cmd.spawn()
}

pub struct CgiJob {
    child: Child,
    // Where the script's answer comes from (non-blocking, watched by mio).
    pub stdout: Option<Receiver>,
    // Where we send the request body (None once everything is sent).
    pub stdin: Option<Sender>,
    // The request body, and how many bytes of it were already sent.
    pub to_send: Vec<u8>,
    pub sent: usize,
    // What the script printed so far.
    pub output: Vec<u8>,
    // When it started: used for the timeout.
    pub started: Instant,
}

impl CgiJob {
    pub fn start(req: &Request, route: &RouteConfig, script: &Path) -> io::Result<CgiJob> {
        let mut child = start_cgi_process(req, route, script)?;

        let stdout = child.stdout.take().map(Receiver::from);
        let mut stdin = child.stdin.take().map(Sender::from);

        if req.body.is_empty() {
            stdin = None;
        }

        let job = CgiJob {
            child,
            stdout,
            stdin,
            to_send: req.body.clone(),
            sent: 0,
            output: Vec::new(),
            started: Instant::now(),
        };

        // Pipes must never block: this is a single-thread server.
        if let Some(r) = &job.stdout {
            r.set_nonblocking(true)?;
        }
        if let Some(s) = &job.stdin {
            s.set_nonblocking(true)?;
        }
        Ok(job)
    }

    // Write as much of the request body as we can to the script's stdin.
    pub fn write_body(&mut self) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let mut close = false;
        while self.sent < self.to_send.len() {
            match stdin.write(&self.to_send[self.sent..]) {
                Ok(0) => {
                    close = true;
                    break;
                }
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    close = true; // e.g. the script exited without reading
                    break;
                }
            }
        }
        if self.sent >= self.to_send.len() {
            close = true;
        }
        if close {
            self.stdin = None; // dropping the pipe closes it
        }
    }

    // Reads whatever the script has printed so far.
    pub fn read_output(&mut self) -> CgiRead {
        let Some(out) = self.stdout.as_mut() else {
            return CgiRead::Eof;
        };
        let mut buf = [0u8; 8192];
        loop {
            match out.read(&mut buf) {
                Ok(0) => return CgiRead::Eof,
                Ok(n) => {
                    self.output.extend_from_slice(&buf[..n]);
                    if self.output.len() > MAX_OUTPUT {
                        return CgiRead::TooLarge;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return CgiRead::WouldBlock,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return CgiRead::Failed,
            }
        }
    }

    // Turns what the script printed into an HTTP response.
    // No blank line between headers and body = broken script = 502.
    pub fn parse_output(raw: &[u8]) -> Response {
        // Find where the headers end: "\r\n\r\n" or "\n\n".
        let (head_end, body_start) = match Self::find_header_end(raw) {
            Some(found) => found,
            None => return Response::text(502, "Bad Gateway: invalid CGI output\n"),
        };

        let head = String::from_utf8_lossy(&raw[..head_end]);
        let mut status = 200u16;
        let mut resp = Response::new(200);

        // Go through the header lines.
        for line in head.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue; // no colon: not a header, skip it
            };
            let (name, value) = (name.trim(), value.trim());

            if name.eq_ignore_ascii_case("status") {
                // "404 Not Found" -> take the number only.
                if let Some(code) = value
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<u16>().ok())
                    .filter(|n| (100..=599).contains(n))
                {
                    status = code;
                }
            } else {
                resp = resp.with_header(name, value);
            }
        }

        // Everything after the blank line is the body.
        resp.status = status;
        resp.with_body(raw[body_start..].to_vec())
    }

    // Position of the blank line that ends the headers:
    // (where the headers stop, where the body starts).
    fn find_header_end(raw: &[u8]) -> Option<(usize, usize)> {
        for i in 0..raw.len() {
            if raw[i..].starts_with(b"\r\n\r\n") {
                return Some((i, i + 4));
            }
            if raw[i..].starts_with(b"\n\n") {
                return Some((i, i + 2));
            }
        }
        None
    }
}

// When a CgiJob is dropped, we must kill the child process and wait for it to exit.
impl Drop for CgiJob {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;
    use std::collections::HashMap;
    use std::io::Read;

    // A route that says: ".py" files are run with python3.
    fn python_route() -> RouteConfig {
        let mut cgi = HashMap::new();
        cgi.insert(".py".to_string(), "python3".into());
        RouteConfig {
            cgi,
            root: Some("cgi-bin".into()),
            ..Default::default()
        }
    }

    // A fake GET request, like the browser would send.
    fn get_request(path: &str) -> Request {
        Request {
            method: Method::Get,
            path: path.into(),
            query: None,
            version: "HTTP/1.1".into(),
            headers: HashMap::new(),
            body: Vec::new(),
            body_file: None,
        }
    }

    #[test]
    fn hello_script_starts_and_prints() {
        let req = get_request("/cgi-bin/hello.py");
        let mut child = start_cgi_process(&req, &python_route(), Path::new("cgi-bin/hello.py"))
            .expect("python3 should start");

        // We have no body to send: closing stdin tells the script "that's all".
        drop(child.stdin.take());

        // Read everything the script printed.
        let mut output = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        child.wait().unwrap(); // always wait: no zombie process

        assert!(output.contains("Content-Type: text/html"));
        assert!(output.contains("Hello from CGI"));
    }

    #[test]
    fn missing_script_is_an_error() {
        let req = get_request("/cgi-bin/nope.py");
        let result = start_cgi_process(&req, &python_route(), Path::new("cgi-bin/nope.py"));
        assert!(result.is_err()); // no crash, just an error we can turn into a 500/404
    }

    #[test]
    fn unknown_extension_is_an_error() {
        // Cargo.toml exists, but ".toml" is not in the route's cgi list.
        let req = get_request("/Cargo.toml");
        let result = start_cgi_process(&req, &python_route(), Path::new("Cargo.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn script_outside_root_is_refused() {
        // A real .py file that is NOT inside cgi-bin.
        let outside = std::env::temp_dir().join("outside_root.py");
        std::fs::write(&outside, "print('hacked')").unwrap();

        let req = get_request("/cgi-bin/../outside_root.py");
        let err = start_cgi_process(&req, &python_route(), &outside).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn secrets_and_proxy_header_do_not_reach_the_script() {
        // A temporary folder with a script that reports what it can see.
        let dir = std::env::temp_dir().join("cgi_env_test");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("env.py");
        std::fs::write(
            &script,
            "import os\n\
             print('Content-Type: text/plain')\n\
             print()\n\
             print('HOME=' + os.environ.get('HOME', 'missing'))\n\
             print('PROXY=' + os.environ.get('HTTP_PROXY', 'missing'))\n\
             print('AGENT=' + os.environ.get('HTTP_USER_AGENT', 'missing'))\n",
        )
        .unwrap();

        let mut route = python_route();
        route.root = Some(dir.clone());

        let mut req = get_request("/env.py");
        req.headers
            .insert("proxy".into(), "evil.example:8080".into());
        req.headers.insert("user-agent".into(), "tester".into());

        let mut child = start_cgi_process(&req, &route, &script).unwrap();
        drop(child.stdin.take());
        let mut output = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        child.wait().unwrap();

        assert!(output.contains("HOME=missing")); // parent's variables are gone
        assert!(output.contains("PROXY=missing")); // dangerous header skipped
        assert!(output.contains("AGENT=tester")); // normal headers still pass

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_a_job_kills_the_script() {
        // A script that would run for 30 seconds.
        let dir = std::env::temp_dir().join("cgi_sleep_test");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("sleepy.py");
        std::fs::write(&script, "import time\ntime.sleep(30)\n").unwrap();

        let mut route = python_route();
        route.root = Some(dir.clone());

        let before = std::time::Instant::now();
        let job = CgiJob::start(&get_request("/sleepy.py"), &route, &script).unwrap();
        drop(job); // kill + wait happen here
        assert!(before.elapsed() < std::time::Duration::from_secs(5));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Runs a job to the end, like the event loop will, and returns how it ended.
    fn run_to_end(job: &mut CgiJob) -> CgiRead {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            job.write_body();
            match job.read_output() {
                CgiRead::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(10)),
                other => return other,
            }
        }
        panic!("script did not finish in 5 seconds");
    }

    #[test]
    fn body_goes_in_and_answer_comes_out() {
        let dir = std::env::temp_dir().join("cgi_echo_test");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("echo.py");
        std::fs::write(
            &script,
            "import sys\n\
             body = sys.stdin.read()\n\
             print('Content-Type: text/plain')\n\
             print()\n\
             print('got:' + body)\n",
        )
        .unwrap();

        let mut route = python_route();
        route.root = Some(dir.clone());

        let mut req = get_request("/echo.py");
        req.method = Method::Post;
        req.body = b"hello".to_vec();

        let mut job = CgiJob::start(&req, &route, &script).unwrap();
        assert_eq!(run_to_end(&mut job), CgiRead::Eof);
        let text = String::from_utf8_lossy(&job.output).to_string();
        assert!(text.contains("got:hello"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_body_in_a_file_is_the_scripts_stdin() {
        let dir = std::env::temp_dir().join(format!("cgi_file_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("count.py");
        std::fs::write(
            &script,
            "import os, sys\n\
             data = sys.stdin.buffer.read()\n\
             print('Content-Type: text/plain')\n\
             print()\n\
             print(len(data), os.environ['CONTENT_LENGTH'])\n",
        )
        .unwrap();
        let body_path = dir.join("body.tmp");
        std::fs::write(&body_path, vec![b'b'; 2_000_000]).unwrap();
        let body_file = std::fs::File::open(&body_path).unwrap();

        let mut route = python_route();
        route.root = Some(dir.clone());
        let mut req = get_request("/count.py");
        req.method = Method::Post;
        req.body_file = Some(crate::http::BodyFile::new(body_path, 2_000_000, body_file));

        let mut job = CgiJob::start(&req, &route, &script).unwrap();
        assert!(job.stdin.is_none(), "no pipe: the script reads the file itself");
        drop(req); // the temp file name is deleted; the script still has it open
        assert_eq!(run_to_end(&mut job), CgiRead::Eof);
        let text = String::from_utf8_lossy(&job.output).to_string();
        assert!(text.contains("2000000 2000000"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn endless_output_is_cut_off() {
        let dir = std::env::temp_dir().join("cgi_flood_test");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("flood.py");
        std::fs::write(&script, "print('x' * 11000000)\n").unwrap();

        let mut route = python_route();
        route.root = Some(dir.clone());

        let mut job = CgiJob::start(&get_request("/flood.py"), &route, &script).unwrap();
        assert_eq!(run_to_end(&mut job), CgiRead::TooLarge);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn header<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
        resp.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn normal_output_is_200() {
        let resp = CgiJob::parse_output(b"Content-Type: text/html\n\n<h1>Hi</h1>");
        assert_eq!(resp.status, 200);
        assert_eq!(header(&resp, "content-type"), Some("text/html"));
        assert_eq!(resp.body, b"<h1>Hi</h1>");
    }

    #[test]
    fn status_header_sets_the_code() {
        let resp =
            CgiJob::parse_output(b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nnope");
        assert_eq!(resp.status, 404);
        assert!(header(&resp, "status").is_none());
        assert_eq!(resp.body, b"nope");
    }

    #[test]
    fn output_without_blank_line_is_502() {
        let resp = CgiJob::parse_output(b"Content-Type: text/html");
        assert_eq!(resp.status, 502);
    }

    #[test]
    fn bad_header_lines_and_bad_status_are_ignored() {
        let resp = CgiJob::parse_output(b"this line has no colon\nStatus: banana\n\nbody");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"body");
    }

    #[test]
    fn body_may_contain_blank_lines() {
        let resp = CgiJob::parse_output(b"Content-Type: text/plain\n\nline1\n\nline2");
        assert_eq!(resp.body, b"line1\n\nline2");
    }
}
