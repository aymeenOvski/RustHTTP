//! Owner: Person A — mio event loop, listeners, virtual hosts, timeouts.
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

    // ── 1. Group servers by address: one listener per host:port ─────
    // e.g. 127.0.0.1:8080 → [0, 1]   127.0.0.1:8081 → [0]
    let mut groups: Vec<(SocketAddr, Vec<usize>)> = Vec::new();
    for (i, server) in config.servers.iter().enumerate() {
        for &port in &server.ports {
            let addr: SocketAddr = format!("{}:{}", server.host, port)
                .parse()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

            match groups.iter_mut().find(|(a, _)| *a == addr) {
                Some((_, list)) => {
                    if !list.contains(&i) {
                        list.push(i);
                    }
                }
                None => groups.push((addr, vec![i])),
            }
        }
    }

    // ── 2. Bind one listener per group ──────────────────────────────
    let mut listeners: Vec<TcpListener> = Vec::new();
    // listener_servers[i] = the servers behind listener i
    let mut listener_servers: Vec<Vec<usize>> = Vec::new();

    for (addr, group) in groups {
        let mut listener = TcpListener::bind(addr)?;
        let token = Token(listeners.len());
        poll.registry()
            .register(&mut listener, token, Interest::READABLE)?;

        println!("listening on http://{addr} ({token:?}) servers {group:?}");
        listeners.push(listener);
        listener_servers.push(group);
    }

    if listeners.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no server in config",
        ));
    }

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
                            // Remember the listener: it decides which servers apply.
                            connections.insert(client_token, Connection::new(stream, token.0));
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
                    Some(conn) => {
                        let group = &listener_servers[conn.listener];
                        drive(token, conn, &config.servers, group)
                    }
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
/// `group` = the servers behind this connection's listener.
/// Returns false when the connection must be closed.
fn drive(token: Token, conn: &mut Connection, servers: &[ServerConfig], group: &[usize]) -> bool {
    loop {
        match conn.state {
            State::Reading => {
                // 1. Head not parsed yet? Try now (once per request).
                if conn.pending.is_none() {
                    match parse_head(&conn.read_buf) {
                        Ok(Some(head)) => {
                            // Choose the virtual server, then apply ITS body limit.
                            let index = select_server(servers, group, head.header("host"));
                            let server = &servers[index];
                            match body_length(&head, server.client_max_body_size) {
                                Ok(body_len) => {
                                    conn.pending = Some(Pending {
                                        head,
                                        body_len,
                                        server: index,
                                    })
                                }
                                Err(status) => {
                                    println!("{token:?}: refused by server #{index} -> {status}");
                                    conn.queue(error_bytes(status, server), true);
                                    continue;
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            // No usable Host header: use the default server's error page.
                            let server = &servers[default_server(servers, group)];
                            println!("{token:?}: bad request -> {}", e.status());
                            conn.queue(error_bytes(e.status(), server), true);
                            continue;
                        }
                    }
                }

                // 2. Whole request here? Hand it to the chosen server's router.
                if let Some((request, index)) = take_request(conn) {
                    let server = &servers[index];
                    let close = !wants_keep_alive(&request);
                    let response = run_router(&request, server);
                    println!(
                        "{token:?}: {} {} [server #{index} {:?}] -> {} ({})",
                        request.method,
                        request.path,
                        server.server_names,
                        response.status,
                        if close { "close" } else { "keep-alive" }
                    );
                    conn.queue(finalize(response, close), close);
                    continue;
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
                    conn.finish_response(); // keep-alive: loop, maybe a pipelined request
                }
                WriteOutcome::WouldBlock => return true,
                WriteOutcome::Error => return false,
            },
        }
    }
}

// ───────────────────────────── virtual hosts ────────────────────────────

/// 1. Host matches a server_name in this group → that server.
/// 2. Otherwise → the group's default server (see default_server).
fn select_server(servers: &[ServerConfig], group: &[usize], host: Option<&str>) -> usize {
    if let Some(host) = host {
        let name = strip_port(host);
        for &i in group {
            if servers[i]
                .server_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name))
            {
                return i;
            }
        }
    }
    default_server(servers, group)
}

/// The server marked `default` in this group, else the first one.
fn default_server(servers: &[ServerConfig], group: &[usize]) -> usize {
    group
        .iter()
        .copied()
        .find(|&i| servers[i].is_default)
        .or_else(|| group.first().copied())
        .unwrap_or(0)
}

/// "localhost:8080" → "localhost",  "[::1]:8080" → "::1",  "example.com" → "example.com"
fn strip_port(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.rsplit_once(':') {
        Some((name, _)) => name,
        None => host,
    }
}

// ───────────────────────────── helpers ──────────────────────────────────

/// The boundary with Person B's code: a panic becomes a 500 for this request.
fn run_router(request: &Request, server: &ServerConfig) -> Response {
    match panic::catch_unwind(AssertUnwindSafe(|| router::handle(request, server))) {
        Ok(response) => response,
        Err(_) => {
            eprintln!("router panicked on {} {}", request.method, request.path);
            error_response(500, server)
        }
    }
}

/// Adds the connection-level headers and turns the response into bytes.
fn finalize(mut response: Response, close: bool) -> Vec<u8> {
    response
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("connection"));
    let response = response
        .with_header("Server", "localserver")
        .with_header("Connection", if close { "close" } else { "keep-alive" });
    serialize::to_bytes(&response)
}

fn error_bytes(status: u16, server: &ServerConfig) -> Vec<u8> {
    finalize(error_response(status, server), true)
}

/// How many body bytes to wait for, or which status to refuse with.
fn body_length(head: &RequestHead, max_body: usize) -> Result<usize, u16> {
    match body_kind(head) {
        Ok(BodyKind::None) => Ok(0),
        Ok(BodyKind::Length(n)) if n > max_body => Err(413),
        Ok(BodyKind::Length(n)) => Ok(n),
        Ok(BodyKind::Chunked) => Err(501), // Step 4
        Err(e) => Err(e.status()),
    }
}

/// If the whole request is in the buffer, takes it out.
/// Returns the request and the index of the server chosen for it.
fn take_request(conn: &mut Connection) -> Option<(Request, usize)> {
    let needed = match &conn.pending {
        Some(p) => p.head.head_len + p.body_len,
        None => return None,
    };
    if conn.read_buf.len() < needed {
        return None;
    }
    let pending = conn.pending.take()?;
    let body = conn.read_buf[pending.head.head_len..needed].to_vec();
    conn.read_buf.drain(..needed);
    Some((pending.head.into_request(body), pending.server))
}

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

    fn named(names: &[&str], is_default: bool) -> ServerConfig {
        ServerConfig {
            server_names: names.iter().map(|n| n.to_string()).collect(),
            is_default,
            ..Default::default()
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

    #[test]
    fn strips_port_from_host() {
        assert_eq!(strip_port("localhost:8080"), "localhost");
        assert_eq!(strip_port("example.com"), "example.com");
        assert_eq!(strip_port("[::1]:8080"), "::1");
        assert_eq!(strip_port(" test.local "), "test.local");
    }

    #[test]
    fn picks_server_by_host() {
        let servers = vec![
            named(&["localhost"], false),
            named(&["test.local"], true),
            named(&["other.local"], false),
        ];
        let group = [0, 1]; // server 2 listens elsewhere

        assert_eq!(select_server(&servers, &group, Some("localhost:8080")), 0);
        assert_eq!(select_server(&servers, &group, Some("TEST.LOCAL")), 1);
        // other.local exists, but not on this address → default of this group
        assert_eq!(select_server(&servers, &group, Some("other.local")), 1);
        assert_eq!(select_server(&servers, &group, Some("unknown")), 1);
        assert_eq!(select_server(&servers, &group, None), 1);
        // no default in the group → the first one
        assert_eq!(select_server(&servers, &[0, 2], None), 0);
    }
}
