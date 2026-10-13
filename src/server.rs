//! Owner: Person A — mio event loop, listeners, timeouts.
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::time::Duration;

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};

use crate::config::Config;

/// One seated customer: their socket + the bytes they sent so far.
/// (Early version of `Connection` in connection.rs.)
struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
}

/// Temporary (1d): the same answer for every request.
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/plain\r\n\
Content-Length: 5\r\n\
Connection: close\r\n\
\r\n\
hello";

pub fn run(config: Config) -> io::Result<()> {
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);

    // ── 1a: one listener per (server, port) ──────────────────────────
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

    // ── 1b: the seating chart ────────────────────────────────────────
    let mut connections: HashMap<Token, Client> = HashMap::new();
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
                // ── 1b: a listener rang, seat every waiting client ──
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
                                continue; // stream dropped here = socket closed
                            }

                            println!("new client {addr} -> {client_token:?}");
                            connections.insert(
                                client_token,
                                Client {
                                    stream,
                                    buf: Vec::new(),
                                },
                            );
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
                // ── 1c: a client rang, read what it sent ────────────
                let mut close = false;

                let client = match connections.get_mut(&token) {
                    Some(c) => c,
                    None => continue,
                };

                if event.is_readable() {
                    let mut tmp = [0u8; 4096];
                    loop {
                        match client.stream.read(&mut tmp) {
                            Ok(0) => {
                                close = true;
                                break;
                            }
                            Ok(n) => client.buf.extend_from_slice(&tmp[..n]),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                eprintln!("read error on {token:?}: {e}");
                                close = true;
                                break;
                            }
                        }
                    }

                    if !client.buf.is_empty() {
                        println!(
                            "--- {:?} has {} bytes ---\n{}",
                            token,
                            client.buf.len(),
                            String::from_utf8_lossy(&client.buf)
                        );
                    }
                }

                // ── 1d (temporary): answer once the head is complete ─
                if !close && client.buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match client.stream.write(RESPONSE) {
                        Ok(n) => println!("answered {token:?} ({n} bytes)"),
                        Err(e) => eprintln!("write error on {token:?}: {e}"),
                    }
                    close = true; // Connection: close → we hang up after answering
                }

                if close {
                    if let Some(mut gone) = connections.remove(&token) {
                        let _ = poll.registry().deregister(&mut gone.stream);
                    }
                    println!("client {token:?} left");
                }
            }
        }
    }
}
