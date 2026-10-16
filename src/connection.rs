//! Owner: Person A — one client: its socket, its buffers, its state.
use std::io::{self, IoSlice, Read, Write};
use std::net::Shutdown;
use std::time::{Duration, Instant};

use mio::net::TcpStream;

use crate::http::parser::{ChunkedDecoder, RequestHead};
use crate::http::serialize::Wire;

/// Bytes read per `fill()` call.
const READ_CHUNK: usize = 64 * 1024;

pub const LINGER: Duration = Duration::from_secs(2);
const MAX_DRAIN: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Reading,
    Writing,
    Draining(Instant),
}

pub enum ReadOutcome {
    Data,
    WouldBlock,
    Closed,
}

pub enum WriteOutcome {
    Done,
    WouldBlock,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expired {
    /// Waiting for a request, nothing received for too long → close.
    Idle,
    /// A request started but never finished (slow client, slowloris) → 408.
    StuckRequest,
    /// Sending, but the client stopped reading → close.
    StalledWrite,
    /// Graceful close took longer than LINGER → close now.
    DrainOver,
}

/// How the body of the pending request is being received.
pub enum PendingBody {
    /// Exactly this many bytes follow the head (0 = no body).
    Length(usize),
    /// Transfer-Encoding: chunked, decoded as the bytes arrive.
    Chunked(ChunkedDecoder),
}

/// A request whose head is parsed, waiting for its body.
pub struct Pending {
    pub head: RequestHead,
    pub body: PendingBody,
    pub server: usize,
}

pub struct Connection {
    pub stream: TcpStream,
    pub listener: usize,
    pub state: State,
    pub read_buf: Vec<u8>,
    pub out_head: Vec<u8>,
    pub out_body: Vec<u8>,
    pub written: usize,
    pub close_after_write: bool,
    pub pending: Option<Pending>,
    pub last_activity: Instant,
    pub request_started: Option<Instant>,
    pub drained: usize,
    pub interim: Vec<u8>,
}

impl Connection {
    pub fn new(stream: TcpStream, listener: usize) -> Self {
        Connection {
            stream,
            listener,
            state: State::Reading,
            read_buf: Vec::new(),
            out_head: Vec::new(),
            out_body: Vec::new(),
            written: 0,
            close_after_write: false,
            pending: None,
            last_activity: Instant::now(),
            request_started: None,
            drained: 0,
            interim: Vec::new(),
        }
    }

    /// Reads ONE chunk from the socket into `read_buf`.
    pub fn fill(&mut self) -> ReadOutcome {
        let mut tmp = [0u8; READ_CHUNK];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return ReadOutcome::Closed,
                Ok(n) => {
                    let now = Instant::now();
                    if self.read_buf.is_empty() {
                        self.request_started = Some(now);
                    }
                    self.read_buf.extend_from_slice(&tmp[..n]);
                    self.last_activity = now;
                    return ReadOutcome::Data;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return ReadOutcome::WouldBlock,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return ReadOutcome::Closed,
            }
        }
    }

    /// Sends as much of the response (head, then body) as the socket accepts.
    pub fn flush(&mut self) -> WriteOutcome {
        loop {
            let head_len = self.out_head.len();
            if self.written >= head_len + self.out_body.len() {
                return WriteOutcome::Done;
            }
            let result = if self.written < head_len {
                let parts = [
                    IoSlice::new(&self.out_head[self.written..]),
                    IoSlice::new(&self.out_body),
                ];
                self.stream.write_vectored(&parts)
            } else {
                self.stream.write(&self.out_body[self.written - head_len..])
            };
            match result {
                Ok(0) => return WriteOutcome::Error,
                Ok(n) => {
                    self.written += n;
                    self.last_activity = Instant::now(); // progress!
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return WriteOutcome::WouldBlock,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return WriteOutcome::Error,
            }
        }
    }

    pub fn send_continue(&mut self) {
        const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
        let sent = loop {
            match self.stream.write(CONTINUE) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break 0, // WouldBlock or broken: keep it for later
            }
        };
        self.interim.extend_from_slice(&CONTINUE[sent..]);
    }

    /// Puts a response in the outgoing buffers and switches to Writing.
    pub fn queue(&mut self, wire: Wire, close: bool) {
        let mut head = std::mem::take(&mut self.interim);
        head.extend_from_slice(&wire.head);
        self.out_head = head;
        self.out_body = wire.body;
        self.written = 0;
        self.close_after_write = close;
        self.state = State::Writing;
        self.last_activity = Instant::now();
    }

    /// After a keep-alive response: get ready for the next request.
    pub fn finish_response(&mut self) {
        self.out_head = Vec::new();
        self.out_body = Vec::new(); // the sent body is freed here
        self.written = 0;
        self.close_after_write = false;
        self.state = State::Reading;
    }

    pub fn start_draining(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Write);
        self.state = State::Draining(Instant::now());
        self.pending = None;
        self.read_buf = Vec::new(); // free the memory
        self.out_head = Vec::new();
        self.out_body = Vec::new();
        self.written = 0;
    }

    pub fn drain(&mut self) -> ReadOutcome {
        let mut tmp = [0u8; READ_CHUNK];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return ReadOutcome::Closed, // the client closed: done
                Ok(n) => {
                    self.drained += n;
                    if self.drained > MAX_DRAIN {
                        return ReadOutcome::Closed;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return ReadOutcome::WouldBlock,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return ReadOutcome::Closed,
            }
        }
    }

    /// Has this connection broken one of the timeout rules?
    pub fn expired(&self, now: Instant, timeout: Duration) -> Option<Expired> {
        let partial_request = !self.read_buf.is_empty() || self.pending.is_some();
        check_timeout(
            self.state,
            partial_request,
            self.last_activity,
            self.request_started,
            now,
            timeout,
        )
    }
}

/// The three timeout rules, as a pure function (no socket, easy to test).
pub fn check_timeout(
    state: State,
    partial_request: bool,
    last_activity: Instant,
    request_started: Option<Instant>,
    now: Instant,
    timeout: Duration,
) -> Option<Expired> {
    match state {
        // Sending: was there progress recently?
        State::Writing => {
            (now.duration_since(last_activity) > timeout).then_some(Expired::StalledWrite)
        }
        // A request is being received: has it taken too long IN TOTAL?
        State::Reading if partial_request => {
            let start = request_started.unwrap_or(last_activity);
            (now.duration_since(start) > timeout).then_some(Expired::StuckRequest)
        }
        // Nothing received: silent for too long?
        State::Reading => (now.duration_since(last_activity) > timeout).then_some(Expired::Idle),
        // Graceful close: never longer than LINGER, even if bytes keep coming.
        State::Draining(since) => {
            (now.duration_since(since) > LINGER).then_some(Expired::DrainOver)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(30);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn idle_connection_expires() {
        let t0 = Instant::now();
        assert_eq!(
            check_timeout(State::Reading, false, t0, None, t0 + secs(10), T),
            None
        );
        assert_eq!(
            check_timeout(State::Reading, false, t0, None, t0 + secs(31), T),
            Some(Expired::Idle)
        );
    }

    #[test]
    fn honest_request_in_progress_is_kept() {
        let t0 = Instant::now();
        assert_eq!(
            check_timeout(
                State::Reading,
                true,
                t0 + secs(9),
                Some(t0),
                t0 + secs(10),
                T
            ),
            None
        );
    }

    #[test]
    fn slowloris_is_caught_even_though_a_byte_just_arrived() {
        let t0 = Instant::now();
        let last_byte = t0 + secs(30); // bytes keep trickling in...
        assert_eq!(
            check_timeout(State::Reading, true, last_byte, Some(t0), t0 + secs(31), T),
            Some(Expired::StuckRequest) // ...but the request started 31 s ago
        );
    }

    #[test]
    fn stalled_write_expires() {
        let t0 = Instant::now();
        assert_eq!(
            check_timeout(State::Writing, false, t0, None, t0 + secs(31), T),
            Some(Expired::StalledWrite)
        );
    }

    #[test]
    fn draining_is_limited_to_linger() {
        let t0 = Instant::now();
        let draining = State::Draining(t0);
        // Bytes still arriving (last_activity recent) do not extend it.
        assert_eq!(
            check_timeout(draining, false, t0 + secs(1), None, t0 + secs(1), T),
            None
        );
        assert_eq!(
            check_timeout(draining, false, t0 + secs(3), None, t0 + secs(3), T),
            Some(Expired::DrainOver)
        );
    }

    #[test]
    fn slow_but_progressing_write_is_kept() {
        let t0 = Instant::now();
        // The download began long ago, but the last progress was 11 s ago.
        assert_eq!(
            check_timeout(
                State::Writing,
                false,
                t0 + secs(100),
                None,
                t0 + secs(111),
                T
            ),
            None
        );
    }
}
