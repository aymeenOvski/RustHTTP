//! Owner: Person A — mio event loop, listeners, timeouts.
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use mio::net::TcpListener;
use mio::{Events, Interest, Poll, Token};

use crate::config::Config;
use crate::connection::{Connection, ReadOutcome, State, WriteOutcome};

use crate::http::parser::parse_head;
use crate::http::reason_phrase;

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
                    Some(conn) => drive(token, conn, &response),
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
fn drive(token: Token, conn: &mut Connection, response: &[u8]) -> bool {
    loop {
        match conn.state {
            State::Reading => {
                match parse_head(&conn.read_buf) {
                    Ok(Some(head)) => {
                        println!(
                            "{token:?}: {} {} query={:?} host={:?} ({} headers)",
                            head.method,
                            head.path,
                            head.query,
                            head.header("host"),
                            head.headers.len()
                        );
                        conn.queue(response.to_vec(), true);
                        continue; // write NOW
                    }
                    Ok(None) => {} // head not complete: read more below
                    Err(e) => {
                        println!("{token:?}: bad request -> {}", e.status());
                        conn.queue(error_bytes(e.status()), true);
                        continue; // send the error, then close
                    }
                }
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
