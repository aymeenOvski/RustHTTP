//! one client: its socket, its buffers, its state.
//!
//! The socket is non-blocking: any read or write can stop with `WouldBlock`.
//! The connection keeps enough state (`read_buf`, `written`, `pending`) to
//! continue exactly where it stopped on the next event.
use std::io::{self, IoSlice, Read, Write};
use std::net::Shutdown;
use std::time::{Duration, Instant};

use crate::cgi::CgiJob;
use mio::Token;
use mio::net::TcpStream;

use crate::http::FileBody;
use crate::http::parser::{ChunkedDecoder, RequestHead};
use crate::http::serialize::Wire;
use crate::spool::Spool;
use crate::upload::UploadJob;

/// Bytes read per `fill()` call.
pub const READ_CHUNK: usize = 64 * 1024;

/// Most bytes one connection may read, write or save in one turn of the
/// event loop. A connection with more to do than that gets its next turn
/// right after the others had theirs, so one big upload or download never
/// makes the other clients wait.
pub const TURN_BUDGET: usize = 1024 * 1024;

/// Bytes of a file body read from disk at a time while it is sent.
const FILE_PIECE: usize = 64 * 1024;

/// Longest time a graceful close may take.
pub const LINGER: Duration = Duration::from_secs(2);
/// Most bytes thrown away during a graceful close before cutting the client off.
const MAX_DRAIN: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Receiving a request, or waiting for the next one (keep-alive).
    Reading,
    /// Sending a response.
    Writing,
    // A CGI script is running for this request.
    WaitingCgi,
    /// An upload is being saved from its body to the upload folder, a piece
    /// per turn of the event loop (files cannot be watched by epoll).
    Saving,
    /// Last response sent and our side shut down (FIN). Reads and throws away
    /// what the client still sends until it closes, so closing does not send
    /// a reset (RST) that could make the client lose the response.
    /// Holds the time draining started.
    Draining(Instant),
}

pub enum ReadOutcome {
    /// New bytes were added.
    Data,
    /// Nothing to read now: wait for the next event.
    WouldBlock,
    /// The client closed, or the socket failed.
    Closed,
}

pub enum WriteOutcome {
    /// The whole response is in the kernel's send buffer.
    Done,
    /// The send buffer is full: wait for WRITABLE.
    WouldBlock,
    /// TURN_BUDGET bytes were sent and there is more: continue on the next
    /// turn of the loop, after the other clients.
    Yield,
    /// The socket failed (or the file being sent got shorter): close.
    Error,
}

/// What `next_file_piece` found.
enum FilePiece {
    /// A new piece of the file is in `out_body`.
    Loaded,
    /// No file, or all of it was sent.
    End,
    /// The file cannot be read, or ended before its announced length.
    Failed,
}

/// Which timeout rule a connection broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expired {
    /// Waiting for a request, nothing received for too long → close.
    Idle,
    /// A request head that never finishes (slowloris), or a body that stopped
    /// arriving → 408.
    StuckRequest,
    /// Sending, but the client stopped reading → close.
    StalledWrite,
    /// Graceful close took longer than LINGER → close now.
    DrainOver,
}

/// How the body of the pending request is being received.
pub enum PendingBody {
    /// Exactly this many bytes follow the head (0 = no body). Small: they
    /// are collected in `read_buf`.
    Length(usize),
    /// A big Content-Length body: each piece goes to a file as it arrives.
    LengthToFile { missing: usize, spool: Spool },
    /// Transfer-Encoding: chunked, decoded as the bytes arrive. Once the
    /// decoded body passes SPOOL_ABOVE, it continues in a file.
    Chunked {
        decoder: ChunkedDecoder,
        spool: Option<Spool>,
    },
}

/// A request whose head is parsed, waiting for its body.
pub struct Pending {
    pub head: RequestHead,
    pub body: PendingBody,
    /// Index (in the config) of the server chosen from the `Host` header.
    pub server: usize,
}

// A running CGI script plus what is needed to answer once it ends.
pub struct CgiRun {
    pub job: CgiJob,
    pub server: usize,
    // Close the connection after the response?
    pub close: bool,
    // Session cookie to add to the response, if the session is new.
    pub set_cookie: Option<String>,
    pub out_token: Option<Token>,
    pub in_token: Option<Token>,
    // Pipes registered in the Poll yet?
    pub registered: bool,
}

/// An upload being saved, plus what is needed to answer once it is done.
pub struct UploadRun {
    pub job: UploadJob,
    pub server: usize,
    /// Close the connection after the response?
    pub close: bool,
    /// Session cookie to add to the response, if the session is new.
    pub set_cookie: Option<String>,
}

pub struct Connection {
    pub stream: TcpStream,
    /// Index of the listener that accepted this client.
    pub listener: usize,
    pub state: State,
    /// Bytes received and not yet used by a request.
    pub read_buf: Vec<u8>,
    /// The response being sent: head, then body. Kept apart so the body is
    /// never copied.
    pub out_head: Vec<u8>,
    pub out_body: Vec<u8>,
    /// The rest of a file body, read into `out_body` one piece at a time.
    pub out_file: Option<FileBody>,
    /// Bytes of head + current `out_body` piece already sent.
    pub written: usize,
    /// Close the connection once the response is sent.
    pub close_after_write: bool,
    /// The request being received, once its head is parsed.
    pub pending: Option<Pending>,
    /// Last time a byte was read or written.
    pub last_activity: Instant,
    /// When the first byte of the current request arrived. Later bytes do not
    /// move it: this is what catches slowloris.
    pub request_started: Option<Instant>,
    /// Bytes thrown away while draining.
    pub drained: usize,
    /// The part of "100 Continue" the socket did not accept (almost always
    /// empty). Sent just before the final response.
    pub interim: Vec<u8>,
    // A CGI script is running for this request.
    pub cgi: Option<CgiRun>,
    /// An upload being saved (state Saving).
    pub upload: Option<UploadRun>,
    /// The last `drive` stopped because of TURN_BUDGET, not because of
    /// `WouldBlock`: no event will come for the work left, so the loop gives
    /// this connection another turn itself.
    pub more_work: bool,
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
            out_file: None,
            written: 0,
            close_after_write: false,
            pending: None,
            last_activity: Instant::now(),
            request_started: None,
            drained: 0,
            interim: Vec::new(),
            cgi: None,
            upload: None,
            more_work: false,
        }
    }

    // A CGI script was started for the pending request.
    pub fn start_cgi(&mut self, run: CgiRun) {
        self.cgi = Some(run);
        self.state = State::WaitingCgi;
        self.last_activity = Instant::now();
    }

    /// An upload job was started for the pending request.
    pub fn start_upload(&mut self, run: UploadRun) {
        self.upload = Some(run);
        self.state = State::Saving;
        self.last_activity = Instant::now();
    }

    /// Reads ONE chunk from the socket into `read_buf`.
    ///
    /// One chunk only, so the caller checks the request (head, limits)
    /// between reads instead of buffering everything first. The first byte of
    /// a new request starts its clock (`request_started`).
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

    /// Sends as much of the response (head, then body) as the socket accepts,
    /// but at most about TURN_BUDGET bytes in one call.
    ///
    /// Continues from `written`. While part of the head is unsent, head and
    /// body go out in one `write_vectored` call, so they leave together and
    /// a small response is not delayed.
    ///
    /// A file body is read in pieces of FILE_PIECE bytes: the next piece is
    /// read only when the previous one is in the kernel, so a 1 GB download
    /// needs no more RAM than a 1 KB one.
    pub fn flush(&mut self) -> WriteOutcome {
        let mut sent_now = 0;
        loop {
            let head_len = self.out_head.len();
            // The current piece is sent (or there is none yet, so the first
            // piece can leave together with the head): load the next one.
            let piece_sent = self.written >= head_len + self.out_body.len()
                || (self.out_body.is_empty() && self.out_file.is_some());
            if piece_sent {
                match self.next_file_piece() {
                    // The head (if sent) stays counted; the new piece starts after it.
                    FilePiece::Loaded => {
                        self.written = self.written.min(head_len);
                        continue;
                    }
                    FilePiece::End if self.written >= head_len + self.out_body.len() => {
                        return WriteOutcome::Done;
                    }
                    FilePiece::End => {} // only the head is left to send
                    FilePiece::Failed => return WriteOutcome::Error,
                }
            }
            if sent_now >= TURN_BUDGET {
                return WriteOutcome::Yield;
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
                    sent_now += n;
                    self.last_activity = Instant::now(); // progress!
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return WriteOutcome::WouldBlock,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return WriteOutcome::Error,
            }
        }
    }

    /// Reads the next piece of the file body into `out_body` (the buffer of
    /// the piece just sent is reused).
    fn next_file_piece(&mut self) -> FilePiece {
        let Some(body) = self.out_file.as_mut() else {
            return FilePiece::End;
        };
        if body.len == 0 {
            self.out_file = None; // closes the file
            return FilePiece::End;
        }
        let want = body.len.min(FILE_PIECE as u64) as usize;
        let mut piece = std::mem::take(&mut self.out_body);
        piece.resize(want, 0);
        let n = loop {
            match body.file.read(&mut piece) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return FilePiece::Failed,
            }
        };
        if n == 0 {
            // The file got shorter after Content-Length was sent: the client
            // would wait forever for the missing bytes, so close instead.
            return FilePiece::Failed;
        }
        piece.truncate(n);
        body.len -= n as u64;
        self.out_body = piece;
        FilePiece::Loaded
    }

    /// Answers `Expect: 100-continue`: tells the client to send its body.
    /// Whatever the socket does not accept now waits in `interim`.
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
    /// `close`: close the connection after sending it.
    pub fn queue(&mut self, wire: Wire, close: bool) {
        // An unsent piece of "100 Continue" must go out first.
        let mut head = std::mem::take(&mut self.interim);
        head.extend_from_slice(&wire.head);
        self.out_head = head;
        self.out_body = wire.body;
        self.out_file = wire.file;
        self.written = 0;
        self.close_after_write = close;
        self.state = State::Writing;
        self.last_activity = Instant::now();
    }

    /// After a keep-alive response: get ready for the next request.
    pub fn finish_response(&mut self) {
        self.out_head = Vec::new();
        self.out_body = Vec::new(); // the sent body is freed here
        self.out_file = None;
        self.written = 0;
        self.close_after_write = false;
        self.state = State::Reading;
    }

    /// Starts the graceful close: sends FIN, frees the buffers, switches to
    /// Draining.
    pub fn start_draining(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Write);
        self.state = State::Draining(Instant::now());
        self.pending = None;
        self.read_buf = Vec::new(); // free the memory
        self.out_head = Vec::new();
        self.out_body = Vec::new();
        self.out_file = None;
        self.written = 0;
    }

    /// Reads and throws away what the client still sends.
    /// `Closed`: the client closed, or it sent more than `MAX_DRAIN`.
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
            self.pending.is_some(),
            self.last_activity,
            self.request_started,
            now,
            timeout,
        )
    }
}

/// The timeout rules, as a pure function (no socket, easy to test).
pub fn check_timeout(
    state: State,
    partial_request: bool,
    head_parsed: bool,
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
        // The body is arriving: it may take as long as it needs (a 1 GB
        // upload takes a while), as long as bytes keep coming.
        State::Reading if partial_request && head_parsed => {
            (now.duration_since(last_activity) > timeout).then_some(Expired::StuckRequest)
        }
        // The head is arriving: has it taken too long IN TOTAL? (slowloris)
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
        State::WaitingCgi => None,
        // Saving always moves forward (a piece per turn): no timeout.
        State::Saving => None,
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
            check_timeout(State::Reading, false, false, t0, None, t0 + secs(10), T),
            None
        );
        assert_eq!(
            check_timeout(State::Reading, false, false, t0, None, t0 + secs(31), T),
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
                false,
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
            check_timeout(State::Reading, true, false, last_byte, Some(t0), t0 + secs(31), T),
            Some(Expired::StuckRequest) // ...but the request started 31 s ago
        );
    }

    #[test]
    fn a_long_upload_that_keeps_moving_is_kept() {
        let t0 = Instant::now();
        // Head parsed 100 s ago, the last body bytes came 2 s ago.
        assert_eq!(
            check_timeout(State::Reading, true, true, t0 + secs(98), Some(t0), t0 + secs(100), T),
            None
        );
    }

    #[test]
    fn an_upload_that_stops_expires() {
        let t0 = Instant::now();
        assert_eq!(
            check_timeout(State::Reading, true, true, t0, Some(t0), t0 + secs(31), T),
            Some(Expired::StuckRequest)
        );
    }

    #[test]
    fn stalled_write_expires() {
        let t0 = Instant::now();
        assert_eq!(
            check_timeout(State::Writing, false, false, t0, None, t0 + secs(31), T),
            Some(Expired::StalledWrite)
        );
    }

    #[test]
    fn draining_is_limited_to_linger() {
        let t0 = Instant::now();
        let draining = State::Draining(t0);
        // Bytes still arriving (last_activity recent) do not extend it.
        assert_eq!(
            check_timeout(draining, false, false, t0 + secs(1), None, t0 + secs(1), T),
            None
        );
        assert_eq!(
            check_timeout(draining, false, false, t0 + secs(3), None, t0 + secs(3), T),
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
                false,
                t0 + secs(100),
                None,
                t0 + secs(111),
                T
            ),
            None
        );
    }
    
    #[test]
    fn waiting_cgi_never_hits_the_idle_rules() {
        let t0 = Instant::now();
        assert_eq!(
            check_timeout(State::WaitingCgi, false, false, t0, None, t0 + secs(3600), T),
            None
        );
    }

    /// A connection on a real local socket, and the client's end of it.
    fn socket_pair() -> (Connection, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        server_side.set_nonblocking(true).unwrap();
        (Connection::new(TcpStream::from_std(server_side), 0), client)
    }

    fn file_with(name: &str, data: &[u8]) -> (std::path::PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!("conn_{}_{name}", std::process::id()));
        std::fs::write(&path, data).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        (path, file)
    }

    #[test]
    fn a_file_body_is_sent_in_small_pieces() {
        use crate::http::Response;
        use crate::http::serialize::to_wire;

        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let (path, file) = file_with("download.bin", &data);
        let (mut conn, mut client) = socket_pair();
        conn.queue(to_wire(Response::new(200).with_file(file, data.len() as u64)), false);

        client.set_nonblocking(true).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let outcome = conn.flush();
            // Only one piece of the file is ever in memory.
            assert!(conn.out_body.len() <= FILE_PIECE, "{} bytes in RAM", conn.out_body.len());
            match outcome {
                WriteOutcome::Done => break,
                WriteOutcome::Yield | WriteOutcome::WouldBlock => {}
                WriteOutcome::Error => panic!("write failed"),
            }
            // The client reads what arrived, so the server can send more.
            while let Ok(n) = client.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
        }
        client.set_nonblocking(false).unwrap();
        let head_end = loop {
            if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = client.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        };
        while got.len() < head_end + data.len() {
            let n = client.read(&mut buf).unwrap();
            assert!(n > 0, "connection closed too early");
            got.extend_from_slice(&buf[..n]);
        }
        let head = String::from_utf8_lossy(&got[..head_end]).to_string();
        assert!(head.contains("Content-Length: 3000000\r\n"), "{head}");
        assert!(got[head_end..] == data[..], "the file arrived changed");
        assert!(conn.out_file.is_none(), "the file is closed once sent");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_shorter_than_announced_closes_the_connection() {
        use crate::http::Response;
        use crate::http::serialize::to_wire;

        let (path, file) = file_with("short.bin", b"only 20 bytes here..");
        let (mut conn, _client) = socket_pair();
        // Content-Length says 100, the file has 20: the client would wait forever.
        conn.queue(to_wire(Response::new(200).with_file(file, 100)), false);
        let outcome = loop {
            match conn.flush() {
                WriteOutcome::Yield | WriteOutcome::WouldBlock => continue,
                other => break other,
            }
        };
        assert!(matches!(outcome, WriteOutcome::Error));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn small_head_and_small_file_leave_in_one_write() {
        use crate::http::Response;
        use crate::http::serialize::to_wire;

        let (path, file) = file_with("small.html", b"<h1>hi</h1>");
        let (mut conn, mut client) = socket_pair();
        conn.queue(to_wire(Response::new(200).with_file(file, 11)), false);
        assert!(matches!(conn.flush(), WriteOutcome::Done));
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 4096];
        let n = client.read(&mut buf).unwrap();
        // Head and body in the same read: no small extra packet for the body.
        assert!(buf[..n].ends_with(b"\r\n\r\n<h1>hi</h1>"), "{:?}", String::from_utf8_lossy(&buf[..n]));
        let _ = std::fs::remove_file(&path);
    }
}
