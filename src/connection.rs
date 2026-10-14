//! Owner: Person A — one client: its socket, its buffers, its state.
use std::io::{self, Read, Write};
use std::time::Instant;

use mio::net::TcpStream;

use crate::http::parser::RequestHead;

/// Bytes read per `fill()` call.
const READ_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Reading,
    Writing,
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

/// A request whose head is parsed, waiting for its body.
pub struct Pending {
    pub head: RequestHead,
    pub body_len: usize,
}

pub struct Connection {
    pub stream: TcpStream,
    pub state: State,
    pub read_buf: Vec<u8>,
    pub write_buf: Vec<u8>,
    pub written: usize,
    pub close_after_write: bool,
    /// Head already parsed (so we never parse it twice).
    pub pending: Option<Pending>,
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
            pending: None,
            last_activity: Instant::now(),
        }
    }

    /// Reads ONE chunk from the socket into `read_buf`.
    pub fn fill(&mut self) -> ReadOutcome {
        let mut tmp = [0u8; READ_CHUNK];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return ReadOutcome::Closed,
                Ok(n) => {
                    self.read_buf.extend_from_slice(&tmp[..n]);
                    self.last_activity = Instant::now();
                    return ReadOutcome::Data;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return ReadOutcome::WouldBlock,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return ReadOutcome::Closed,
            }
        }
    }

    /// Sends as much of `write_buf` as the socket accepts right now.
    pub fn flush(&mut self) -> WriteOutcome {
        while self.written < self.write_buf.len() {
            match self.stream.write(&self.write_buf[self.written..]) {
                Ok(0) => return WriteOutcome::Error,
                Ok(n) => {
                    self.written += n;
                    self.last_activity = Instant::now();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return WriteOutcome::WouldBlock,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return WriteOutcome::Error,
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
