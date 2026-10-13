//! Owner: Person A — mio event loop, listeners, timeouts.
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};

use crate::config::Config;

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
    // Tokens 0..listeners.len() belong to listeners, so clients start after.
    let mut connections: HashMap<Token, TcpStream> = HashMap::new();
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
                // A listener rang: seat EVERY waiting client (edge-triggered).
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
                                continue; // `stream` is dropped here = socket closed
                            }

                            println!("new client {addr} -> {client_token:?}");
                            connections.insert(client_token, stream);
                        }
                        // The door queue is empty: done for now.
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        // A signal interrupted accept(): just try again.
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        // Anything else: report it, but never crash.
                        Err(e) => {
                            eprintln!("accept error: {e}");
                            break;
                        }
                    }
                }
            } else {
                // A client rang. Reading comes in 1c.
                println!(
                    "client event on {:?} (readable: {}, writable: {})",
                    token,
                    event.is_readable(),
                    event.is_writable()
                );
            }
        }
    }
}