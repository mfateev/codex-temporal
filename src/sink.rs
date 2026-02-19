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

    /// Return JSON-serialized events starting from `from_index`, plus the new
    /// watermark (i.e. the total number of events so far).
    ///
    /// This is designed for query-based polling: the client tracks the
    /// watermark and passes it back as `from_index` on the next call.
    pub fn events_since(&self, from_index: usize) -> (Vec<String>, usize) {
        let guard = self.events.lock().expect("lock poisoned");
        let total = guard.len();
        if from_index >= total {
            return (Vec::new(), total);
        }
        let jsons = guard[from_index..]
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect();
        (jsons, total)
    }

    pub fn len(&self) -> usize {
        self.events.lock().expect("lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Synchronous event push — usable from non-async contexts (e.g. tool
    /// handler building approval events).
    pub fn emit_event_sync(&self, event: Event) {
        self.events.lock().expect("lock poisoned").push(event);
    }
}

#[async_trait::async_trait]
impl codex_core::EventSink for BufferEventSink {
    async fn emit_event(&self, event: Event) {
        self.events.lock().expect("lock poisoned").push(event);
    }
}
