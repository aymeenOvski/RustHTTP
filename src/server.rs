//! Owner: Person A — mio event loop, listeners, timeouts.
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use mio::net::TcpListener;
use mio::{Events, Interest, Poll, Token};

use crate::config::Config;
use crate::connection::{Connection, Pending, ReadOutcome, State, WriteOutcome};
use crate::http::parser::{BodyKind, RequestHead, body_kind, parse_head};
use crate::http::{Request, reason_phrase};
/// Temporary (2a): size of the fixed answer's body.
/// 5 MB to test partial writes; set it to 5 to get a small answer back.
const TEST_BODY_SIZE: usize = 5;

pub fn run(config: Config) -> io::Result<()> {
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);

    // ── listeners (1a) ───────────────────────────────────────────────
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

    let mut connections: HashMap<Token, Connection> = HashMap::new();
    let mut next_token = listeners.len();
    let response = test_response(); // built once, copied per request
    // Step 3 will pick the server from the Host header; for now: the first one.
    let max_body = config
        .servers
        .first()
        .map(|s| s.client_max_body_size)
        .unwrap_or(1_000_000);

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
                // ── accept every waiting client (1b) ────────────────
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
                    Some(conn) => drive(token, conn, &response, max_body),
                    None => continue, // already closed: old event
                };

                if !keep_open {
                    if let Some(mut gone) = connections.remove(&token) {
                        let _ = poll.registry().deregister(&mut gone.stream);
                    } // dropped here = socket closed
                    println!("client {token:?} closed");
                }
            }
        }
    }
}

/// Moves one connection forward as far as possible WITHOUT waiting.
/// Returns false when the connection must be closed.
fn drive(token: Token, conn: &mut Connection, response: &[u8], max_body: usize) -> bool {
    loop {
        match conn.state {
            State::Reading => {
                // 1. Head not parsed yet? Try now (only once per request).
                if conn.pending.is_none() {
                    match parse_head(&conn.read_buf) {
                        Ok(Some(head)) => match body_length(&head, max_body) {
                            Ok(body_len) => conn.pending = Some(Pending { head, body_len }),
                            Err(status) => {
                                println!("{token:?}: refused -> {status}");
                                conn.queue(error_bytes(status), true);
                                continue;
                            }
                        },
                        Ok(None) => {} // head not complete yet
                        Err(e) => {
                            println!("{token:?}: bad request -> {}", e.status());
                            conn.queue(error_bytes(e.status()), true);
                            continue;
                        }
                    }
                }

                // 2. Head known: has the whole body arrived?
                if let Some(request) = take_request(conn) {
                    println!(
                        "{token:?}: {} {} body: {} bytes",
                        request.method,
                        request.path,
                        request.body.len()
                    );
                    conn.queue(response.to_vec(), true);
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
                    println!("{token:?}: response fully sent ({} bytes)", conn.written);
                    return false; // 2e: keep-alive goes here
                }
                WriteOutcome::WouldBlock => {
                    println!(
                        "{token:?}: sent {}/{} bytes, waiting for WRITABLE",
                        conn.written,
                        conn.write_buf.len()
                    );
                    return true;
                }
                WriteOutcome::Error => return false,
            },
        }
    }
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
        return None; // body still arriving
    }
    let pending = conn.pending.take()?;
    let body = conn.read_buf[pending.head.head_len..needed].to_vec();
    // Remove exactly this request; anything after it is the NEXT request (2e).
    conn.read_buf.drain(..needed);
    Some(pending.head.into_request(body))
}

/// Temporary until 2d (real error pages come from error.rs).
fn error_bytes(status: u16) -> Vec<u8> {
    let reason = reason_phrase(status);
    let body = format!("{status} {reason}\n");
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

/// Temporary (2a): a fixed answer of TEST_BODY_SIZE bytes of 'a'.
fn test_response() -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {TEST_BODY_SIZE}\r\n\
         Connection: close\r\n\
         \r\n"
    )
    .into_bytes();
    out.resize(out.len() + TEST_BODY_SIZE, b'a');
    out
}
