//! Owner: Person A — mio event loop, listeners, timeouts.
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::panic::{self, AssertUnwindSafe};
use std::time::Duration;

use mio::net::TcpListener;
use mio::{Events, Interest, Poll, Token};

use crate::config::{Config, ServerConfig};
use crate::connection::{Connection, Pending, ReadOutcome, State, WriteOutcome};
use crate::error::error_response;
use crate::http::parser::{BodyKind, RequestHead, body_kind, parse_head};
use crate::http::{Request, Response, serialize};
use crate::router;

pub fn run(config: Config) -> io::Result<()> {
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);

    // ── listeners ────────────────────────────────────────────────────
    let mut listeners: Vec<TcpListener> = Vec::new();

    for server in &config.servers {
        for &port in &server.ports {
            let addr: SocketAddr = format!("{}:{}", server.host, port)
                .parse()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

            let mut listener = TcpListener::bind(addr)?;
            let token = Token(listeners.len());
            poll.registry()
                .register(&mut listener, token, Interest::READABLE)?;

            println!("listening on http://{addr} ({token:?})");
            listeners.push(listener);
        }
    }

    // Step 3 will pick the server from the Host header; for now: the first one.
    let server = match config.servers.first() {
        Some(s) => s,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no server in config",
            ));
        }
    };

    let mut connections: HashMap<Token, Connection> = HashMap::new();
    let mut next_token = listeners.len();

    loop {
        if let Err(e) = poll.poll(&mut events, Some(Duration::from_millis(500))) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }

        for event in events.iter() {
            let token = event.token();

            if token.0 < listeners.len() {
                // ── accept every waiting client ─────────────────────
                loop {
                    match listeners[token.0].accept() {
                        Ok((mut stream, addr)) => {
                            let client_token = Token(next_token);
                            next_token += 1;

                            if let Err(e) = poll.registry().register(
                                &mut stream,
                                client_token,
                                Interest::READABLE | Interest::WRITABLE,
                            ) {
                                eprintln!("register error: {e}");
                                continue;
                            }

                            println!("new client {addr} -> {client_token:?}");
                            connections.insert(client_token, Connection::new(stream));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            eprintln!("accept error: {e}");
                            break;
                        }
                    }
                }
            } else {
                // ── a client rang: let its state machine move ───────
                let keep_open = match connections.get_mut(&token) {
                    Some(conn) => drive(token, conn, server),
                    None => continue,
                };

                if !keep_open {
                    if let Some(mut gone) = connections.remove(&token) {
                        let _ = poll.registry().deregister(&mut gone.stream);
                    }
                    println!("client {token:?} closed");
                }
            }
        }
    }
}

/// Moves one connection forward as far as possible WITHOUT waiting.
/// Returns false when the connection must be closed.
fn drive(token: Token, conn: &mut Connection, server: &ServerConfig) -> bool {
    loop {
        match conn.state {
            State::Reading => {
                // 1. Head not parsed yet? Try now (once per request).
                if conn.pending.is_none() {
                    match parse_head(&conn.read_buf) {
                        Ok(Some(head)) => match body_length(&head, server.client_max_body_size) {
                            Ok(body_len) => conn.pending = Some(Pending { head, body_len }),
                            Err(status) => {
                                println!("{token:?}: refused -> {status}");
                                conn.queue(error_bytes(status, server), true);
                                continue;
                            }
                        },
                        Ok(None) => {}
                        Err(e) => {
                            println!("{token:?}: bad request -> {}", e.status());
                            conn.queue(error_bytes(e.status(), server), true);
                            continue;
                        }
                    }
                }

                // 2. Whole request here? Hand it to the application.
                if let Some(request) = take_request(conn) {
                    let close = !wants_keep_alive(&request);
                    let response = run_router(&request, server);
                    println!(
                        "{token:?}: {} {} -> {} ({})",
                        request.method,
                        request.path,
                        response.status,
                        if close { "close" } else { "keep-alive" }
                    );
                    conn.queue(finalize(response, close), close);
                    continue; // write NOW
                }

                // 3. Not complete: read one more chunk.
                match conn.fill() {
                    ReadOutcome::Data => {}
                    ReadOutcome::WouldBlock => return true,
                    ReadOutcome::Closed => return false,
                }
            }
            State::Writing => match conn.flush() {
                WriteOutcome::Done => {
                    if conn.close_after_write {
                        return false;
                    }
                    // Keep-alive: back to Reading, and LOOP (don't return):
                    // a pipelined request may already be in read_buf, and
                    // epoll will never wake us for bytes we already read.
                    conn.finish_response();
                }
                WriteOutcome::WouldBlock => return true,
                WriteOutcome::Error => return false,
            },
        }
    }
}

/// The boundary with Person B's code. A panic in there becomes a 500
/// for this one request instead of killing the whole server.
fn run_router(request: &Request, server: &ServerConfig) -> Response {
    match panic::catch_unwind(AssertUnwindSafe(|| router::handle(request, server))) {
        Ok(response) => response,
        Err(_) => {
            eprintln!("router panicked on {} {}", request.method, request.path);
            error_response(500, server)
        }
    }
}

/// Adds the connection-level headers (our job, not the router's) and
/// turns the response into bytes.
fn finalize(mut response: Response, close: bool) -> Vec<u8> {
    response
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("connection"));
    let response = response
        .with_header("Server", "localserver")
        .with_header("Connection", if close { "close" } else { "keep-alive" });
    serialize::to_bytes(&response)
}

/// An error page from error.rs, ready to send.
fn error_bytes(status: u16, server: &ServerConfig) -> Vec<u8> {
    finalize(error_response(status, server), true)
}

/// How many body bytes to wait for, or which status to refuse with.
fn body_length(head: &RequestHead, max_body: usize) -> Result<usize, u16> {
    match body_kind(head) {
        Ok(BodyKind::None) => Ok(0),
        Ok(BodyKind::Length(n)) if n > max_body => Err(413), // before reading it!
        Ok(BodyKind::Length(n)) => Ok(n),
        Ok(BodyKind::Chunked) => Err(501), // Step 4
        Err(e) => Err(e.status()),
    }
}

/// If the whole request is in the buffer, takes it out and returns it.
fn take_request(conn: &mut Connection) -> Option<Request> {
    let needed = match &conn.pending {
        Some(p) => p.head.head_len + p.body_len,
        None => return None,
    };
    if conn.read_buf.len() < needed {
        return None;
    }
    let pending = conn.pending.take()?;
    let body = conn.read_buf[pending.head.head_len..needed].to_vec();
    conn.read_buf.drain(..needed); // what follows = the next request (2e)
    Some(pending.head.into_request(body))
}

/// HTTP/1.1 keeps the connection open unless the client says "close";
/// HTTP/1.0 closes it unless the client says "keep-alive".
fn wants_keep_alive(request: &Request) -> bool {
    let words: Vec<String> = request
        .header("connection")
        .map(|v| {
            v.split(',')
                .map(|w| w.trim().to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();

    if words.iter().any(|w| w == "close") {
        return false;
    }
    if words.iter().any(|w| w == "keep-alive") {
        return true;
    }
    request.version == "HTTP/1.1"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;

    fn request(version: &str, connection: Option<&str>) -> Request {
        let mut headers = HashMap::new();
        if let Some(value) = connection {
            headers.insert("connection".to_string(), value.to_string());
        }
        Request {
            method: Method::Get,
            path: "/".into(),
            query: None,
            version: version.into(),
            headers,
            body: Vec::new(),
        }
    }

    #[test]
    fn keep_alive_rules() {
        assert!(wants_keep_alive(&request("HTTP/1.1", None)));
        assert!(!wants_keep_alive(&request("HTTP/1.1", Some("close"))));
        assert!(!wants_keep_alive(&request(
            "HTTP/1.1",
            Some("Upgrade, Close")
        )));
        assert!(!wants_keep_alive(&request("HTTP/1.0", None)));
        assert!(wants_keep_alive(&request("HTTP/1.0", Some("Keep-Alive"))));
    }
}
