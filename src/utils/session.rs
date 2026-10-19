//! Owner: Person B — Session Management
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A session unused for this long is deleted (Tomcat's default is 30 minutes too).
const SESSION_MAX_AGE: Duration = Duration::from_secs(30 * 60);
/// How often old sessions are looked for.
const CLEANUP_EVERY: Duration = Duration::from_secs(60);

pub struct Session {
    pub data: HashMap<String, String>,
    pub last_seen: Instant,
}

pub struct SessionStore {
    sessions: HashMap<String, Session>,
    /// Counts the sessions created, so no two ids are built from the same input.
    counter: u64,
    last_cleanup: Instant,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            counter: 0,
            last_cleanup: Instant::now(),
        }
    }

    // Creates a brand new session with a random, unique ID and stores it.
    pub fn create_session(&mut self) -> String {
        // From time to time, forget the sessions nobody used for a long time.
        if self.last_cleanup.elapsed() >= CLEANUP_EVERY {
            self.cleanup_expired(SESSION_MAX_AGE);
            self.last_cleanup = Instant::now();
        }

        // A new random id; in the (practically impossible) case it already
        // exists, take another one, so two users never share a session.
        let id = loop {
            self.counter += 1;
            let id = random_id(self.counter);
            if !self.sessions.contains_key(&id) {
                break id;
            }
        };

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
        self.sessions
            .retain(|_, session| now.duration_since(session.last_seen) < max_age);
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }
}

/// A random id of 32 hex characters (128 bits), like Tomcat's JSESSIONID.
///
/// `RandomState` hashes with secret keys that the standard library picks at
/// random when the program starts, so the result cannot be guessed from the
/// outside. No extra crate needed, and it works on Linux and Windows.
fn random_id(counter: u64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut halves = [0u64; 2];
    for (i, half) in halves.iter_mut().enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(counter);
        hasher.write_u128(nanos);
        hasher.write_usize(i);
        *half = hasher.finish();
    }
    format!("{:016x}{:016x}", halves[0], halves[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_long_hex_and_all_different() {
        let mut store = SessionStore::new();
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let id = store.create_session();
            assert_eq!(id.len(), 32);
            assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(seen.insert(id), "the same id was given twice");
        }
        assert_eq!(store.len(), 10_000);
    }

    #[test]
    fn a_created_session_can_be_found_again() {
        let mut store = SessionStore::new();
        let id = store.create_session();
        assert!(store.get_session(&id).is_some());
        assert!(store.get_session("1d").is_none()); // a guessed id is unknown
    }

    #[test]
    fn old_sessions_are_removed() {
        let mut store = SessionStore::new();
        let id = store.create_session();
        store.cleanup_expired(Duration::ZERO); // everything counts as old
        assert!(store.get_session(&id).is_none());
        assert_eq!(store.len(), 0);
    }
}
