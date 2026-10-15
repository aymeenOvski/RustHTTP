//! Owner: Person B
use std::collections::HashMap;
use std::time::Instant;

pub struct Session {
    pub data: HashMap<String, String>,
    pub last_seen: Instant,
}

#[derive(Default)]
pub struct SessionStore {
    sessions: HashMap<String, Session>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}
