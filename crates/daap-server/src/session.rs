//! Session tracking for DAAP /login → subsequent-request session-id flow.
//! Sessions are ephemeral, in-memory, and monotonically issued.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct SessionStore {
    next: AtomicU32,
    live: Mutex<HashMap<u32, ()>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            next: AtomicU32::new(1),
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Allocate a new session id.
    pub fn create(&self) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.live.lock().unwrap().insert(id, ());
        id
    }

    pub fn is_valid(&self, id: u32) -> bool {
        self.live.lock().unwrap().contains_key(&id)
    }

    pub fn end(&self, id: u32) {
        self.live.lock().unwrap().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_monotonic_ids() {
        let s = SessionStore::new();
        assert_eq!(s.create(), 1);
        assert_eq!(s.create(), 2);
        assert_eq!(s.create(), 3);
    }

    #[test]
    fn tracks_validity() {
        let s = SessionStore::new();
        let id = s.create();
        assert!(s.is_valid(id));
        s.end(id);
        assert!(!s.is_valid(id));
    }
}
