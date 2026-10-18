//! Owner: Person B — Session Management
use std::collections::HashMap;
use std::time::{Duration, Instant};

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
        Self {
            sessions: HashMap::new(),
        }
    }

    // Creates a brand new session, generates a unique ID string, and stores it.
    pub fn create_session(&mut self) -> String {
        // Generate a unique session ID using elapsed time / nanoseconds
        let id = format!("{:x}", Instant::now().elapsed().as_nanos());
        
        let session = Session {
            data: HashMap::new(),
            last_seen: Instant::now(),
        };
        
        self.sessions.insert(id.clone(), session);
        id
    }

    // Fetches an active session by its ID and refreshes its last_seen timestamp.
    pub fn get_session(&mut self, id: &str) -> Option<&mut Session> {
        if let Some(session) = self.sessions.get_mut(id) {
            session.last_seen = Instant::now();
            Some(session)
        } else {
            None
        }
    }

    // Removes sessions that have been inactive longer than the specified max_age.
    pub fn cleanup_expired(&mut self, max_age: Duration) {
        let now = Instant::now();
        self.sessions.retain(|_, session| {
            now.duration_since(session.last_seen) < max_age
        });
    }
}