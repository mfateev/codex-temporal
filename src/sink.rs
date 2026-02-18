//! Buffer-backed event sink for Temporal workflows.

use std::sync::Mutex;

use codex_protocol::protocol::Event;

/// An [`EventSink`] that buffers events in memory.
pub struct BufferEventSink {
    events: Mutex<Vec<Event>>,
}

impl BufferEventSink {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    /// Drain all buffered events.
    pub fn drain(&self) -> Vec<Event> {
        let mut guard = self.events.lock().expect("lock poisoned");
        std::mem::take(&mut *guard)
    }

    pub fn len(&self) -> usize {
        self.events.lock().expect("lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait::async_trait]
impl codex_core::EventSink for BufferEventSink {
    async fn emit_event(&self, event: Event) {
        self.events.lock().expect("lock poisoned").push(event);
    }
}
