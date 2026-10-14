//! Owner: Person A — one client: its socket, its buffers, its state.
use std::io::{self, Read, Write};
use std::time::Instant;

use mio::net::TcpStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Collecting the request.
    Reading,
    /// Sending the response from `write_buf`.
    Writing,
}

/// What one `fill()` call found.
pub enum ReadOutcome {
    /// New bytes were added to `read_buf`.
    Data,
    /// Nothing more to read right now.
    WouldBlock,
    /// The client hung up, or the connection broke.
    Closed,
}

/// What one `flush()` call managed.
pub enum WriteOutcome {
    /// The whole `write_buf` has been sent.
    Done,
    /// The socket is full: continue on the next WRITABLE event.
    WouldBlock,
    /// The connection broke.
    Error,
}

pub struct Connection {
    pub stream: TcpStream,
    pub state: State,
    pub read_buf: Vec<u8>,
    pub write_buf: Vec<u8>,
    /// How many bytes of `write_buf` are already sent.
    pub written: usize,
    pub close_after_write: bool,
    /// Used for timeouts in Step 3.
    pub last_activity: Instant,
}

impl Connection {
    pub fn new(stream: TcpStream) -> Self {
        Connection {
            stream,
            state: State::Reading,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            written: 0,
            close_after_write: false,
            last_activity: Instant::now(),
        }
    }

    /// Reads ONE chunk from the socket into `read_buf`.
    /// The caller loops: check the buffer, fill, check again...
    pub fn fill(&mut self) -> ReadOutcome {
        let mut tmp = [0u8; 4096];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => {
                    return ReadOutcome::Closed;
                }
                Ok(n) => {
                    self.read_buf.extend_from_slice(&tmp[..n]);
                    self.last_activity = Instant::now();
                    return ReadOutcome::Data;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return ReadOutcome::WouldBlock;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    continue;
                }
                Err(_) => {
                    return ReadOutcome::Closed;
                }
            }
        }
    }

    /// Sends as much of `write_buf` as the socket accepts right now.
    pub fn flush(&mut self) -> WriteOutcome {
        while self.written < self.write_buf.len() {
            match self.stream.write(&self.write_buf[self.written..]) {
                Ok(0) => {
                    return WriteOutcome::Error;
                }
                Ok(n) => {
                    self.written += n;
                    self.last_activity = Instant::now();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return WriteOutcome::WouldBlock;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    continue;
                }
                Err(_) => {
                    return WriteOutcome::Error;
                }
            }
        }
        WriteOutcome::Done
    }

    /// Puts a full response in `write_buf` and switches to Writing.
    pub fn queue(&mut self, bytes: Vec<u8>, close: bool) {
        self.write_buf = bytes;
        self.written = 0;
        self.close_after_write = close;
        self.state = State::Writing;
    }
}
