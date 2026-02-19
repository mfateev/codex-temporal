//! In-memory storage backend for Temporal workflows.

use std::sync::Mutex;

use codex_protocol::protocol::RolloutItem;

/// An [`StorageBackend`] that stores rollout items in memory.
pub struct InMemoryStorage {
    items: Mutex<Vec<RolloutItem>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }

    /// Read all stored items.
    pub fn items(&self) -> Vec<RolloutItem> {
        self.items.lock().expect("lock poisoned").clone()
    }
}

#[async_trait::async_trait]
impl codex_core::StorageBackend for InMemoryStorage {
    async fn save(&self, items: &[RolloutItem]) {
        let mut guard = self.items.lock().expect("lock poisoned");
        guard.extend_from_slice(items);
    }
}
