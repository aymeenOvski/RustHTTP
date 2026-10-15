//! Owner: Person A — per-connection state machine.
use std::time::Instant;

#[derive(Debug, PartialEq)]
pub enum State {
    ReadingHead,
    ReadingBody,
    Writing,
    Closed,
}

pub struct Connection {
    pub state: State,
    pub read_buf: Vec<u8>,
    pub write_buf: Vec<u8>,
    pub written: usize,
    pub last_activity: Instant,
}

impl Connection {
    pub fn new() -> Self {
        Connection {
            state: State::ReadingHead,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            written: 0,
            last_activity: Instant::now(),
        }
    }
}
