//! the event loop: listeners, clients, virtual hosts, timeouts.
//!
//! One thread, one `Poll`. Listeners use tokens `0..listeners.len()`, clients
//! use the tokens after. Each event moves one connection forward (`drive`)
//! until its socket says `WouldBlock`, so no client can block the others.
use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::panic::{self, AssertUnwindSafe};
use std::time::{Duration, Instant};

use mio::net::TcpListener;
use mio::{Events, Interest, Poll, Token};

use crate::config::{Config, ServerConfig};

use crate::error::error_response;
use crate::http::parser::{BodyKind, ChunkedDecoder, RequestHead, body_kind, parse_head};
use crate::http::serialize::{Wire, to_wire};
use crate::http::{Request, Response};

use crate::cgi::{CgiJob, CgiRead};
use crate::connection::{
    CgiRun, Connection, Expired, Pending, PendingBody, READ_CHUNK, ReadOutcome, State,
    TURN_BUDGET, UploadRun, WriteOutcome,
};
use crate::upload::upload_response;
use crate::router::{self, Outcome};
use crate::spool::{self, SPOOL_ABOVE, Spool};

/// Max time a client may stay idle, take to send one request (then 408), or
/// go without write progress.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How often expired connections are looked for.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

// Max time a CGI script may run before the client gets a 504.
const CGI_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
enum Pipe {
    Out, // the script's stdout
    In,  // the script's stdin
}

// pipe token -> (token of the client that owns it, which pipe)
type CgiPipes = HashMap<Token, (Token, Pipe)>;

/// Binds every address in the config and runs the event loop forever.
///
/// An address that cannot be used (unknown host, port already taken) is
/// skipped with a message: the other servers still start. Returns an error
/// only if no address at all could be used, or if `poll` itself fails.
pub fn run(config: Config) -> io::Result<()> {
    let mut poll = Poll::new()?;
    spool::clean_old_files(); // body files left by a server that was killed
    let mut events = Events::with_capacity(1024);

    // ── 1. Group servers by address: one listener per host:port ─────
    // Servers sharing an address are told apart later by `Host`.
    let mut groups: Vec<(SocketAddr, Vec<usize>)> = Vec::new();
    for (i, server) in config.servers.iter().enumerate() {
        for &port in &server.ports {
            let addr = match resolve(&server.host, port) {
                Ok(addr) => addr,
                Err(e) => {
                    eprintln!("server #{i}: skipping {}:{port}: {e}", server.host);
                    continue;
                }
            };

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
    let mut listener_servers: Vec<Vec<usize>> = Vec::new();

    for (addr, group) in groups {
        let mut listener = match TcpListener::bind(addr) {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("cannot listen on {addr}: {e} (servers {group:?} skipped)");
                continue;
            }
        };
        let token = Token(listeners.len());
        if let Err(e) = poll
            .registry()
            .register(&mut listener, token, Interest::READABLE)
        {
            eprintln!("cannot watch {addr}: {e} (servers {group:?} skipped)");
            continue;
        }

        println!("listening on http://{addr} ({token:?}) servers {group:?}");
        listeners.push(listener);
        listener_servers.push(group);
    }

    if listeners.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no server could start (see the errors above)",
        ));
    }

    let mut connections: HashMap<Token, Connection> = HashMap::new();
    let mut cgi_pipes: CgiPipes = HashMap::new();
    let mut next_token = listeners.len();
    let mut last_sweep = Instant::now();
    // accept_blocked[i]: accept() failed on listener i (e.g. too many open
    // files). Its waiting clients send no new event, so we retry ourselves.
    let mut accept_blocked: Vec<bool> = vec![false; listeners.len()];

    // Some connection stopped because of TURN_BUDGET and has work left.
    let mut busy = false;

    loop {
        // Wake up at least every 500 ms, so timeouts run even with no events.
        // A busy connection gets no event for the work it has left (its
        // socket may have nothing new), so then poll only looks for events
        // and returns at once.
        let wait = if busy {
            Duration::ZERO
        } else {
            Duration::from_millis(500)
        };
        if let Err(e) = poll.poll(&mut events, Some(wait)) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }

        for index in 0..listeners.len() {
            if accept_blocked[index] {
                try_accept(
                    index,
                    &listeners,
                    &poll,
                    &mut connections,
                    &mut next_token,
                    &mut accept_blocked,
                );
            }
        }

        for event in events.iter() {
            let token = event.token();

            if token.0 < listeners.len() {
                // ── accept every waiting client ─────────────────────
                try_accept(
                    token.0,
                    &listeners,
                    &poll,
                    &mut connections,
                    &mut next_token,
                    &mut accept_blocked,
                );
            } else if let Some(&(owner, pipe)) = cgi_pipes.get(&token) {
                // ── a CGI pipe rang ─────────────────────────────────
                let finished = match connections.get_mut(&owner) {
                    Some(conn) => cgi_event(conn, pipe, &config.servers),
                    None => None,
                };
                if let Some(response) = finished {
                    finish_cgi(
                        owner,
                        response,
                        &mut connections,
                        &mut cgi_pipes,
                        &poll,
                        &config.servers,
                        &listener_servers,
                        &mut next_token,
                    );
                }
            } else {
                // ── a client rang: let its state machine move ───────
                let keep_open = match connections.get_mut(&token) {
                    Some(conn) => {
                        let group = &listener_servers[conn.listener];
                        advance(
                            token,
                            conn,
                            &config.servers,
                            group,
                            &poll,
                            &mut cgi_pipes,
                            &mut next_token,
                        )
                    }
                    None => continue,
                };
                if !keep_open {
                    close_connection(token, &mut connections, &poll, &mut cgi_pipes);
                }
            }
        }

        // ── one more turn for the connections that used up their budget ─
        // (a big upload being saved, a big download, a fast uploader).
        // Each gets about TURN_BUDGET bytes, then the loop goes back to poll,
        // so every client keeps being served.
        let turn: Vec<Token> = connections
            .iter()
            .filter(|(_, conn)| conn.more_work)
            .map(|(token, _)| *token)
            .collect();
        for token in turn {
            let keep_open = match connections.get_mut(&token) {
                Some(conn) if conn.more_work => {
                    let group = &listener_servers[conn.listener];
                    advance(
                        token,
                        conn,
                        &config.servers,
                        group,
                        &poll,
                        &mut cgi_pipes,
                        &mut next_token,
                    )
                }
                _ => continue,
            };
            if !keep_open {
                close_connection(token, &mut connections, &poll, &mut cgi_pipes);
            }
        }

        // ── about once a second: enforce the timeouts ───────────────
        if last_sweep.elapsed() >= SWEEP_EVERY {
            last_sweep = Instant::now();
            sweep_timeouts(
                &mut connections,
                &mut cgi_pipes,
                &listener_servers,
                &config.servers,
                &poll,
                &mut next_token,
            );
        }

        // ── free some disk space of deleted temp files (a piece per turn) ─
        let trash_left = spool::free_some_space();

        busy = trash_left || connections.values().any(|conn| conn.more_work);
    }
}

/// Accepts on one listener and remembers whether it failed.
/// Logs only when the state changes, not on every retry.
fn try_accept(
    index: usize,
    listeners: &[TcpListener],
    poll: &Poll,
    connections: &mut HashMap<Token, Connection>,
    next_token: &mut usize,
    accept_blocked: &mut [bool],
) {
    match accept_clients(index, &listeners[index], poll, connections, next_token) {
        Ok(()) => {
            if accept_blocked[index] {
                println!("listener {index} accepting again");
            }
            accept_blocked[index] = false;
        }
        Err(e) => {
            if !accept_blocked[index] {
                eprintln!("accept error on listener {index}: {e} (will retry)");
            }
            accept_blocked[index] = true;
        }
    }
}

/// Accepts clients until `WouldBlock`; each gets the next token and is
/// watched for reading and writing. An accept error is returned; the clients
/// not accepted yet stay in the kernel's queue.
fn accept_clients(
    index: usize,
    listener: &TcpListener,
    poll: &Poll,
    connections: &mut HashMap<Token, Connection>,
    next_token: &mut usize,
) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok((mut stream, addr)) => {
                let client_token = Token(*next_token);
                *next_token += 1;

                if let Err(e) = poll.registry().register(
                    &mut stream,
                    client_token,
                    Interest::READABLE | Interest::WRITABLE,
                ) {
                    eprintln!("register error: {e}");
                    continue; // stream dropped here = socket closed
                }

                println!("new client {addr} -> {client_token:?}");
                connections.insert(client_token, Connection::new(stream, index));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// `drive`, but a panic closes only this connection, not the whole server.
/// Returns false when the connection must be closed.
fn drive_safely(
    token: Token,
    conn: &mut Connection,
    servers: &[ServerConfig],
    group: &[usize],
) -> bool {
    match panic::catch_unwind(AssertUnwindSafe(|| drive(token, conn, servers, group))) {
        Ok(keep_open) => keep_open,
        Err(_) => {
            eprintln!("{token:?}: internal error (panic), connection closed");
            false
        }
    }
}

/// Stops watching the client and drops it, which closes its socket.
fn close_connection(
    token: Token,
    connections: &mut HashMap<Token, Connection>,
    poll: &Poll,
    cgi_pipes: &mut CgiPipes,
) {
    if let Some(mut gone) = connections.remove(&token) {
        if let Some(mut run) = gone.cgi.take() {
            release_cgi(&mut run, poll, cgi_pipes); // run dropped: kill + wait
        }
        let _ = poll.registry().deregister(&mut gone.stream);
        println!("client {token:?} closed");
    }
}

/// Closes every connection that broke a timeout rule.
/// A stuck request first gets a 408 response.
fn sweep_timeouts(
    connections: &mut HashMap<Token, Connection>,
    cgi_pipes: &mut CgiPipes,
    listener_servers: &[Vec<usize>],
    servers: &[ServerConfig],
    poll: &Poll,
    next_token: &mut usize,
) {
    let now = Instant::now();

    // Scripts that ran too long -> 504.
    let slow: Vec<(Token, usize)> = connections
        .iter()
        .filter_map(|(t, c)| {
            c.cgi
                .as_ref()
                .filter(|r| r.job.started.elapsed() > CGI_TIMEOUT)
                .map(|r| (*t, r.server))
        })
        .collect();
    for (token, server) in slow {
        println!("{token:?}: CGI timed out -> 504");
        finish_cgi(
            token,
            error_response(504, &servers[server]),
            connections,
            cgi_pipes,
            poll,
            servers,
            listener_servers,
            next_token,
        );
    }

    // Ordinary connection timeouts.
    let expired: Vec<(Token, Expired)> = connections
        .iter()
        .filter_map(|(token, conn)| conn.expired(now, TIMEOUT).map(|why| (*token, why)))
        .collect();

    for (token, why) in expired {
        println!("{token:?} timed out ({why:?})");

        let keep_open = match why {
            Expired::Idle | Expired::StalledWrite | Expired::DrainOver => false,
            Expired::StuckRequest => match connections.get_mut(&token) {
                Some(conn) => {
                    let group = &listener_servers[conn.listener];
                    let index = match &conn.pending {
                        Some(p) => p.server,
                        None => default_server(servers, group),
                    };
                    conn.pending = None;
                    conn.queue(error_wire(408, &servers[index]), true);
                    advance(token, conn, servers, group, poll, cgi_pipes, next_token)
                }
                None => false,
            },
        };

        if !keep_open {
            close_connection(token, connections, poll, cgi_pipes);
        }
    }
}

/// Moves one connection forward until its socket says `WouldBlock`, or
/// until it used its TURN_BUDGET (then `more_work` is set, and the loop gives
/// it another turn after the others).
///
/// - Reading: parse the head, pick the server, plan the body, wait for the
///   whole request, call the router, queue the response.
/// - Saving: save the next piece of an upload.
/// - Writing: send; then close gracefully, or go back to Reading (keep-alive).
/// - Draining: throw away input until the client closes.
///
/// It loops instead of returning after each step, so pipelined requests
/// already in `read_buf` are answered without waiting for a new event.
/// Returns false when the connection must be closed.
fn drive(token: Token, conn: &mut Connection, servers: &[ServerConfig], group: &[usize]) -> bool {
    conn.more_work = false;
    // Reads allowed in this turn (TURN_BUDGET bytes).
    let mut reads_left = TURN_BUDGET / READ_CHUNK;
    loop {
        match conn.state {
            State::Reading => {
                // 1. Head not parsed yet? Try now (once per request).
                if conn.pending.is_none() {
                    match parse_head(&conn.read_buf) {
                        Ok(Some(head)) => {
                            let index = select_server(servers, group, head.header("host"));
                            let server = &servers[index];
                            match body_plan(&head, server.client_max_body_size) {
                                Ok(body) => {
                                    let received = conn.read_buf.len() - head.head_len;
                                    if should_send_continue(&head, &body, received) {
                                        println!("{token:?}: 100 Continue");
                                        conn.send_continue();
                                    }
                                    conn.pending = Some(Pending {
                                        head,
                                        body,
                                        server: index,
                                    });
                                }
                                Err(status) => {
                                    println!("{token:?}: refused by server #{index} -> {status}");
                                    conn.queue(error_wire(status, server), true);
                                    continue;
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let server = &servers[default_server(servers, group)];
                            println!("{token:?}: bad request -> {}", e.status());
                            conn.queue(error_wire(e.status(), server), true);
                            continue;
                        }
                    }
                }

                // 2. Whole request here? Hand it to the chosen server's router.
                match take_request(conn, servers) {
                    Ok(Some((request, index))) => {
                        let server = &servers[index];
                        let close = !wants_keep_alive(&request);
                        match run_router(&request, server) {
                            Outcome::Response(response) => {
                                println!(
                                    "{token:?}: {} {} [server #{index}] body {} bytes -> {} ({})",
                                    request.method,
                                    request.path,
                                    request.body_len(),
                                    response.status,
                                    if close { "close" } else { "keep-alive" }
                                );
                                conn.queue(finalize(response, close), close);
                            }
                            Outcome::Cgi { job, set_cookie } => {
                                println!(
                                    "{token:?}: {} {} [server #{index}] -> CGI started",
                                    request.method, request.path
                                );
                                conn.start_cgi(CgiRun {
                                    job,
                                    server: index,
                                    close,
                                    set_cookie,
                                    out_token: None,
                                    in_token: None,
                                    registered: false,
                                });
                            }
                            Outcome::Upload { job, set_cookie } => {
                                println!(
                                    "{token:?}: {} {} [server #{index}] body {} bytes -> saving",
                                    request.method,
                                    request.path,
                                    request.body_len()
                                );
                                conn.start_upload(UploadRun {
                                    job,
                                    server: index,
                                    close,
                                    set_cookie,
                                });
                            }
                        }
                        continue;
                    }
                    Ok(None) => {} // body still arriving
                    Err((status, index)) => {
                        // Broken chunked body (400) or too big (413).
                        println!("{token:?}: bad body -> {status}");
                        conn.queue(error_wire(status, &servers[index]), true);
                        continue;
                    }
                }

                // 3. Not complete: read one more chunk (if this turn allows it).
                if reads_left == 0 {
                    conn.more_work = true; // bytes may still wait in the kernel
                    return true;
                }
                reads_left -= 1;
                match conn.fill() {
                    ReadOutcome::Data => {}
                    ReadOutcome::WouldBlock => return true,
                    ReadOutcome::Closed => return false,
                }
            }
            State::Saving => {
                let Some(run) = conn.upload.as_mut() else {
                    return false; // cannot happen: Saving always has a job
                };
                let Some(result) = run.job.step(TURN_BUDGET) else {
                    conn.more_work = true; // not finished: next turn
                    return true;
                };
                let Some(run) = conn.upload.take() else {
                    return false;
                };
                let mut response = upload_response(result, &servers[run.server]);
                println!("{token:?}: upload saved -> {}", response.status);
                if let Some(cookie) = &run.set_cookie {
                    response = response.with_header("Set-Cookie", cookie);
                }
                conn.queue(finalize(response, run.close), run.close);
                // `run` is dropped here: the body file is closed.
            }
            State::Writing => match conn.flush() {
                WriteOutcome::Done => {
                    if conn.close_after_write {
                        conn.start_draining();
                    } else {
                        conn.finish_response();
                    }
                }
                WriteOutcome::WouldBlock => return true,
                WriteOutcome::Yield => {
                    conn.more_work = true; // more to send: next turn
                    return true;
                }
                WriteOutcome::Error => return false,
            },
            State::Draining(_) => match conn.drain() {
                ReadOutcome::WouldBlock => return true, // wait for the client to close
                _ => return false,                      // it closed (or misbehaves): close now
            },
            State::WaitingCgi => return true,
        }
    }
}

// `drive_safely`, then register the pipes of a CGI job it may have started.
// Returns false when the connection must be closed.
fn advance(
    token: Token,
    conn: &mut Connection,
    servers: &[ServerConfig],
    group: &[usize],
    poll: &Poll,
    cgi_pipes: &mut CgiPipes,
    next_token: &mut usize,
) -> bool {
    loop {
        if !drive_safely(token, conn, servers, group) {
            return false;
        }
        if !matches!(&conn.cgi, Some(run) if !run.registered) {
            return true;
        }
        match register_cgi(token, conn, poll, cgi_pipes, next_token) {
            Ok(()) => return true,
            Err(e) => {
                eprintln!("{token:?}: cannot watch CGI pipes: {e}");
                if let Some(mut run) = conn.cgi.take() {
                    release_cgi(&mut run, poll, cgi_pipes);
                    conn.queue(error_wire(500, &servers[run.server]), true);
                }
                // loop: drive sends the 500
            }
        }
    }
}

// Gives the job's pipes their own tokens and watches them.
fn register_cgi(
    owner: Token,
    conn: &mut Connection,
    poll: &Poll,
    cgi_pipes: &mut CgiPipes,
    next_token: &mut usize,
) -> io::Result<()> {
    let run = conn
        .cgi
        .as_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no CGI job"))?;

    if let Some(out) = run.job.stdout.as_mut() {
        let t = Token(*next_token);
        *next_token += 1;
        poll.registry().register(out, t, Interest::READABLE)?;
        cgi_pipes.insert(t, (owner, Pipe::Out));
        run.out_token = Some(t);
    }
    if let Some(inp) = run.job.stdin.as_mut() {
        let t = Token(*next_token);
        *next_token += 1;
        poll.registry().register(inp, t, Interest::WRITABLE)?;
        cgi_pipes.insert(t, (owner, Pipe::In));
        run.in_token = Some(t);
    }
    run.registered = true;
    Ok(())
}

// Stops watching the job's pipes and forgets their tokens.
// When the `CgiRun` is dropped afterwards, the script is killed and reaped.
fn release_cgi(run: &mut CgiRun, poll: &Poll, cgi_pipes: &mut CgiPipes) {
    if let Some(t) = run.out_token.take() {
        if let Some(out) = run.job.stdout.as_mut() {
            let _ = poll.registry().deregister(out);
        }
        cgi_pipes.remove(&t);
    }
    if let Some(t) = run.in_token.take() {
        if let Some(inp) = run.job.stdin.as_mut() {
            let _ = poll.registry().deregister(inp);
        }
        cgi_pipes.remove(&t);
    }
}

// A pipe event: send more body, or read output.
// Returns the response once the script is done (or failed).
fn cgi_event(conn: &mut Connection, pipe: Pipe, servers: &[ServerConfig]) -> Option<Response> {
    let run = conn.cgi.as_mut()?;
    let server = &servers[run.server];
    match pipe {
        Pipe::In => {
            run.job.write_body();
            None
        }
        Pipe::Out => match run.job.read_output() {
            CgiRead::WouldBlock => None,
            CgiRead::Eof => Some(CgiJob::parse_output(&std::mem::take(&mut run.job.output))),
            CgiRead::TooLarge => Some(error_response(502, server)),
            CgiRead::Failed => Some(error_response(500, server)),
        },
    }
}

// The script is over: free its pipes and process, queue the response,
// and let the connection send it.
fn finish_cgi(
    token: Token,
    response: Response,
    connections: &mut HashMap<Token, Connection>,
    cgi_pipes: &mut CgiPipes,
    poll: &Poll,
    servers: &[ServerConfig],
    listener_servers: &[Vec<usize>],
    next_token: &mut usize,
) {
    let keep_open = match connections.get_mut(&token) {
        Some(conn) => {
            let Some(mut run) = conn.cgi.take() else {
                return;
            };
            release_cgi(&mut run, poll, cgi_pipes);

            let mut response = response;
            if let Some(cookie) = &run.set_cookie {
                response = response.with_header("Set-Cookie", cookie);
            }
            conn.queue(finalize(response, run.close), run.close);
            // `run` is dropped at the end of this block: kill + wait, no zombie.

            let group = &listener_servers[conn.listener];
            advance(token, conn, servers, group, poll, cgi_pipes, next_token)
        }
        None => return,
    };
    if !keep_open {
        close_connection(token, connections, poll, cgi_pipes);
    }
}

/// The address to listen on for `host:port`. `host` can be an IP
/// ("127.0.0.1", "0.0.0.0") or a name ("localhost"), which is looked up.
/// When a name has several addresses, IPv4 is preferred.
fn resolve(host: &str, port: u16) -> io::Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address for this host"))
}

// ───────────────────────────── virtual hosts ────────────────────────────

/// The server of this group whose `server_names` match the `Host` header,
/// otherwise the group's default.
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

/// The group's server marked `is_default`, otherwise the first one.
fn default_server(servers: &[ServerConfig], group: &[usize]) -> usize {
    group
        .iter()
        .copied()
        .find(|&i| servers[i].is_default)
        .or_else(|| group.first().copied())
        .unwrap_or(0)
}

/// `example.com:8080` → `example.com`, `[::1]:8080` → `::1`.
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

/// Calls the router. A panic inside it becomes a 500 response.
fn run_router(request: &Request, server: &ServerConfig) -> Outcome {
    match panic::catch_unwind(AssertUnwindSafe(|| router::dispatch(request, server))) {
        Ok(outcome) => outcome,
        Err(_) => {
            eprintln!("router panicked on {} {}", request.method, request.path);
            Outcome::Response(error_response(500, server))
        }
    }
}

/// Adds the `Server` and `Connection` headers, then builds the bytes.
/// The router's own `Connection` header is replaced: keep-alive is decided
/// here only.
fn finalize(mut response: Response, close: bool) -> Wire {
    response
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("connection"));
    let response = response
        .with_header("Server", "localserver")
        .with_header("Connection", if close { "close" } else { "keep-alive" });
    to_wire(response)
}

/// The server's error page for `status`; the connection closes after it.
fn error_wire(status: u16, server: &ServerConfig) -> Wire {
    finalize(error_response(status, server), true)
}

/// How to receive the body, or the status (400, 413) to answer right away.
fn body_plan(head: &RequestHead, max_body: usize) -> Result<PendingBody, u16> {
    match body_kind(head) {
        Ok(BodyKind::None) => Ok(PendingBody::Length(0)),
        Ok(BodyKind::Length(n)) if n > max_body => Err(413), // before reading it!
        // Big body: straight to a file, piece by piece (500 if no file can be made).
        Ok(BodyKind::Length(n)) if n > SPOOL_ABOVE => match Spool::create() {
            Ok(spool) => Ok(PendingBody::LengthToFile { missing: n, spool }),
            Err(e) => {
                eprintln!("cannot create a body file: {e}");
                Err(500)
            }
        },
        Ok(BodyKind::Length(n)) => Ok(PendingBody::Length(n)),
        // The total size is unknown: the decoder checks the limit chunk by chunk.
        Ok(BodyKind::Chunked) => Ok(PendingBody::Chunked {
            decoder: ChunkedDecoder::new(),
            spool: None,
        }),
        Err(e) => Err(e.status()),
    }
}

/// True if the client waits for "100 Continue" before sending its body: it
/// sent `Expect: 100-continue`, speaks HTTP/1.1, announced a body, and no
/// body byte has arrived yet.
fn should_send_continue(head: &RequestHead, body: &PendingBody, received: usize) -> bool {
    let asked = head
        .header("expect")
        .map(|v| v.trim().eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false);
    let has_body = match body {
        PendingBody::Length(n) => *n > 0,
        PendingBody::LengthToFile { .. } | PendingBody::Chunked { .. } => true,
    };
    asked && head.version == "HTTP/1.1" && has_body && received == 0
}

/// Takes the pending request out of the connection once its body is complete.
///
/// - `Ok(Some((request, server)))`: complete. Bytes after it stay in
///   `read_buf` (the next pipelined request).
/// - `Ok(None)`: no head yet, or the body is still arriving.
/// - `Err((status, server))`: broken chunked body (400), too big (413), or
///   the body file could not be written (500).
///
/// Body bytes leave `read_buf` as soon as possible: decoded chunks right
/// away, and every piece of a big body goes to its file. So `read_buf` stays
/// small whatever the size of the upload.
fn take_request(
    conn: &mut Connection,
    servers: &[ServerConfig],
) -> Result<Option<(Request, usize)>, (u16, usize)> {
    let pending = match conn.pending.as_mut() {
        Some(p) => p,
        None => return Ok(None),
    };
    let start = pending.head.head_len;
    let server = pending.server;
    let max_body = servers[server].client_max_body_size;
    let disk_error = |e: std::io::Error| {
        eprintln!("cannot write the body file: {e}");
        (500, server)
    };

    // 1. Is the body complete? If yes: how many bytes of read_buf does this
    //    request still use?
    let used = match &mut pending.body {
        PendingBody::Length(n) => {
            if conn.read_buf.len() < start + *n {
                return Ok(None);
            }
            start + *n
        }
        PendingBody::LengthToFile { missing, spool } => {
            // Move what arrived after the head into the file.
            let take = (*missing).min(conn.read_buf.len() - start);
            if take > 0 {
                spool
                    .write(&conn.read_buf[start..start + take])
                    .map_err(disk_error)?;
                conn.read_buf.drain(start..start + take);
                *missing -= take;
            }
            if *missing > 0 {
                return Ok(None);
            }
            start // only the head is left in read_buf
        }
        PendingBody::Chunked { decoder, spool } => {
            let finished = decoder
                .advance(&conn.read_buf[start..], max_body)
                .map_err(|e| (e.status(), server))?;
            // The decoded body got big: continue it in a file.
            if spool.is_none() && decoder.decoded_len() > SPOOL_ABOVE {
                *spool = Some(Spool::create().map_err(disk_error)?);
            }
            if let Some(spool) = spool {
                spool.write(&decoder.take_body()).map_err(disk_error)?;
            }
            if finished {
                start + decoder.consumed()
            } else {
                let done = decoder.forget_consumed();
                conn.read_buf.drain(start..start + done);
                return Ok(None);
            }
        }
    };

    // 2. Complete: take the request out.
    let Some(pending) = conn.pending.take() else {
        return Ok(None);
    };
    let (body, body_file) = match pending.body {
        PendingBody::Length(_) => {
            // MOVE the body instead of copying it:
            // 1. cut off what follows this request (usually nothing),
            let next_request = conn.read_buf.split_off(used);
            // 2. take the whole buffer out of the connection (no copy),
            let mut bytes = std::mem::replace(&mut conn.read_buf, next_request);
            // 3. drop the head: the body slides to the front, same memory.
            bytes.drain(..start);
            (bytes, None)
        }
        PendingBody::LengthToFile { spool, .. } => {
            conn.read_buf.drain(..used); // what follows = the next request
            (Vec::new(), Some(spool.finish().map_err(disk_error)?))
        }
        PendingBody::Chunked { mut decoder, spool } => {
            conn.read_buf.drain(..used); // what follows = the next request
            match spool {
                Some(spool) => (Vec::new(), Some(spool.finish().map_err(disk_error)?)),
                None => (decoder.take_body(), None),
            }
        }
    };
    // The next request's clock starts now if its bytes are already here.
    conn.request_started = if conn.read_buf.is_empty() {
        None
    } else {
        Some(Instant::now())
    };
    let mut request = pending.head.into_request(body);
    request.body_file = body_file;
    Ok(Some((request, server)))
}

/// `Connection: close` → close; `Connection: keep-alive` → keep.
/// Otherwise HTTP/1.1 keeps the connection open and HTTP/1.0 closes it.
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
            body_file: None,
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

    fn planned(raw: &[u8]) -> (RequestHead, PendingBody) {
        let head = match parse_head(raw) {
            Ok(Some(h)) => h,
            other => panic!("expected a head, got {other:?}"),
        };
        let body = match body_plan(&head, 1_000_000) {
            Ok(b) => b,
            Err(status) => panic!("refused with {status}"),
        };
        (head, body)
    }

    #[test]
    fn continue_rules() {
        let yes = |raw: &[u8], received: usize| {
            let (head, body) = planned(raw);
            should_send_continue(&head, &body, received)
        };
        let base = "POST /up HTTP/1.1\r\nHost: a\r\nContent-Length: 10\r\n";
        let with = |extra: &str| format!("{base}{extra}\r\n");

        assert!(yes(with("Expect: 100-continue\r\n").as_bytes(), 0));
        assert!(yes(with("Expect: 100-Continue\r\n").as_bytes(), 0)); // any letter case
        assert!(!yes(with("").as_bytes(), 0)); // not asked
        assert!(!yes(with("Expect: something-else\r\n").as_bytes(), 0));
        assert!(!yes(with("Expect: 100-continue\r\n").as_bytes(), 3)); // body already coming
        assert!(!yes(
            b"POST /up HTTP/1.0\r\nContent-Length: 10\r\nExpect: 100-continue\r\n\r\n",
            0
        )); // never to HTTP/1.0
        assert!(!yes(
            b"POST /up HTTP/1.1\r\nHost: a\r\nContent-Length: 0\r\nExpect: 100-continue\r\n\r\n",
            0
        )); // no body
        assert!(yes(
            b"POST /up HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n",
            0
        )); // chunked: size unknown, so yes
    }

    /// A Connection on a real (local) socket, for testing take_request.
    fn test_connection(bytes: &[u8]) -> Connection {
        test_connection_with_limit(bytes, 1024)
    }

    fn test_connection_with_limit(bytes: &[u8], max_body: usize) -> Connection {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut conn = Connection::new(mio::net::TcpStream::from_std(stream), 0);
        conn.read_buf = bytes.to_vec();
        let head = parse_head(&conn.read_buf).unwrap().unwrap();
        let body = body_plan(&head, max_body).unwrap();
        conn.pending = Some(Pending {
            head,
            body,
            server: 0,
        });
        conn
    }

    #[test]
    fn body_is_moved_and_next_request_kept() {
        let servers = vec![ServerConfig {
            client_max_body_size: 1024,
            ..Default::default()
        }];
        let mut conn = test_connection(
            b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhelloGET /b HTTP/1.1\r\n",
        );
        let address = conn.read_buf.as_ptr(); // where the bytes were read into
        let (request, server) = take_request(&mut conn, &servers).unwrap().unwrap();
        assert_eq!(request.body, b"hello");
        assert_eq!(server, 0);
        assert_eq!(request.body.as_ptr(), address); // same memory: moved, not copied
        assert_eq!(conn.read_buf, b"GET /b HTTP/1.1\r\n"); // pipelined request kept
    }

    #[test]
    fn decoded_chunks_leave_read_buf() {
        let servers = vec![ServerConfig {
            client_max_body_size: 1024,
            ..Default::default()
        }];
        let head = b"POST /a HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut first_part = head.to_vec();
        first_part.extend_from_slice(b"5\r\nhello\r\n");
        let mut conn = test_connection(&first_part);

        // Not finished yet, but the decoded chunk is already gone from read_buf.
        assert!(take_request(&mut conn, &servers).unwrap().is_none());
        assert_eq!(conn.read_buf.len(), head.len());

        conn.read_buf.extend_from_slice(b"0\r\n\r\nGET /b");
        let (request, _) = take_request(&mut conn, &servers).unwrap().unwrap();
        assert_eq!(request.body, b"hello");
        assert_eq!(conn.read_buf, b"GET /b");
    }

    #[test]
    fn resolves_ips_and_names() {
        let local: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(resolve("127.0.0.1", 8080).unwrap(), local);
        assert_eq!(resolve("localhost", 8080).unwrap(), local);
        assert_eq!(
            resolve("0.0.0.0", 9000).unwrap(),
            "0.0.0.0:9000".parse::<SocketAddr>().unwrap()
        );
        assert!(resolve("", 8080).is_err());
        assert!(resolve("bad host!", 8080).is_err());
    }

    /// Feeds `body` in 64 KB pieces, like `fill()` does, and checks that
    /// read_buf never holds more than the head plus one piece.
    fn feed_in_pieces(conn: &mut Connection, servers: &[ServerConfig], body: &[u8]) -> Option<Request> {
        let head_len = conn.read_buf.len();
        for piece in body.chunks(64 * 1024) {
            conn.read_buf.extend_from_slice(piece);
            if let Some((request, _)) = take_request(conn, servers).unwrap() {
                return Some(request);
            }
            assert!(conn.read_buf.len() <= head_len + 64 * 1024, "body piling up in RAM");
        }
        None
    }

    #[test]
    fn big_body_goes_to_a_file_not_to_ram() {
        let size = SPOOL_ABOVE + 100_000;
        let servers = vec![ServerConfig {
            client_max_body_size: 10 * SPOOL_ABOVE,
            ..Default::default()
        }];
        let head = format!("POST /up HTTP/1.1\r\nHost: x\r\nContent-Length: {size}\r\n\r\n");
        let mut conn = test_connection_with_limit(head.as_bytes(), 10 * SPOOL_ABOVE);
        let body: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let mut with_next = body.clone();
        with_next.extend_from_slice(b"GET /next HTTP/1.1\r\n");

        let request = feed_in_pieces(&mut conn, &servers, &with_next).expect("request complete");
        assert!(request.body.is_empty()); // nothing in memory...
        let file = request.body_file.as_ref().expect("...the body is in a file");
        assert_eq!(file.len(), size as u64);
        assert_eq!(std::fs::read(file.path()).unwrap(), body);
        assert!(conn.read_buf.starts_with(b"GET /next")); // pipelined request kept

        let path = file.path().to_path_buf();
        drop(request); // request handled
        assert!(!path.exists(), "the temp file is deleted");
    }

    #[test]
    fn big_chunked_body_goes_to_a_file() {
        let servers = vec![ServerConfig {
            client_max_body_size: 10 * SPOOL_ABOVE,
            ..Default::default()
        }];
        let head = b"POST /up HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut conn = test_connection_with_limit(head, 10 * SPOOL_ABOVE);
        let part = vec![b'z'; 700_000];
        let mut raw = Vec::new();
        for _ in 0..2 {
            raw.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            raw.extend_from_slice(&part);
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"0\r\n\r\n");

        let request = feed_in_pieces(&mut conn, &servers, &raw).expect("request complete");
        let file = request.body_file.as_ref().expect("the body is in a file");
        assert_eq!(file.len(), 1_400_000);
        assert!(request.body.is_empty());
    }

    #[test]
    fn small_body_stays_in_memory() {
        let servers = vec![ServerConfig {
            client_max_body_size: 1024,
            ..Default::default()
        }];
        let mut conn =
            test_connection(b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello");
        let (request, _) = take_request(&mut conn, &servers).unwrap().unwrap();
        assert_eq!(request.body, b"hello");
        assert!(request.body_file.is_none());
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
        let group = [0, 1];
        assert_eq!(select_server(&servers, &group, Some("localhost:8080")), 0);
        assert_eq!(select_server(&servers, &group, Some("TEST.LOCAL")), 1);
        assert_eq!(select_server(&servers, &group, Some("other.local")), 1);
        assert_eq!(select_server(&servers, &group, Some("unknown")), 1);
        assert_eq!(select_server(&servers, &group, None), 1);
        assert_eq!(select_server(&servers, &[0, 2], None), 0);
    }
}
