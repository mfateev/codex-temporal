//! Buffer-backed event sink for Temporal workflows.

use std::collections::VecDeque;
use std::sync::Mutex;

use codex_protocol::protocol::{Event, EventMsg, TokenUsage};

/// Default capacity for the rolling event buffer.
pub const DEFAULT_EVENT_BUFFER_CAPACITY: usize = 4096;

/// An [`EventSink`] that buffers events in a rolling window.
///
/// Events are stored in a bounded `VecDeque`. When the buffer exceeds
/// `capacity`, the oldest events are evicted and `offset` is incremented.
/// The `offset` tracks the total number of evicted events, so watermarks
/// remain monotonic across continue-as-new boundaries.
pub struct BufferEventSink {
    /// Total number of events evicted (monotonic across CAN).
    offset: Mutex<usize>,
    /// Bounded rolling buffer of events.
    events: Mutex<VecDeque<Event>>,
    /// Maximum number of events to retain.
    capacity: usize,
}

impl BufferEventSink {
    pub fn new(capacity: usize, offset: usize) -> Self {
        Self {
            offset: Mutex::new(offset),
            events: Mutex::new(VecDeque::new()),
            capacity,
        }
    }

    /// Drain all buffered events, advancing the offset.
    pub fn drain(&self) -> Vec<Event> {
        let mut guard = self.events.lock().expect("lock poisoned");
        let mut offset_guard = self.offset.lock().expect("lock poisoned");
        let count = guard.len();
        *offset_guard += count;
        guard.drain(..).collect()
    }

    /// Return JSON-serialized events starting from `from_index`, plus the new
    /// watermark (i.e. `offset + buffer_len`).
    ///
    /// Used by the `get_state_update` update handler: the client tracks the
    /// watermark and passes it back as `since_index` on the next call.
    pub fn events_since(&self, from_index: usize) -> (Vec<String>, usize) {
        let guard = self.events.lock().expect("lock poisoned");
        let offset = *self.offset.lock().expect("lock poisoned");
        let watermark = offset + guard.len();

        if from_index < offset {
            // Client is behind the window — return everything in the buffer.
            let jsons = guard
                .iter()
                .filter_map(|e| serde_json::to_string(e).ok())
                .collect();
            return (jsons, watermark);
        }

        if from_index >= watermark {
            return (Vec::new(), watermark);
        }

        let start = from_index - offset;
        let jsons = guard
            .iter()
            .skip(start)
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect();
        (jsons, watermark)
    }

    pub fn len(&self) -> usize {
        self.events.lock().expect("lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Current watermark (offset + buffer length).  Cheap check for whether
    /// new events exist without serializing them.
    pub fn watermark(&self) -> usize {
        let offset = *self.offset.lock().expect("lock poisoned");
        let len = self.events.lock().expect("lock poisoned").len();
        offset + len
    }

    /// Return the latest cumulative `TokenUsage` from buffered `TokenCount`
    /// events, if any.
    pub fn latest_token_usage(&self) -> Option<TokenUsage> {
        let guard = self.events.lock().expect("lock poisoned");
        guard
            .iter()
            .rev()
            .find_map(|e| {
                if let EventMsg::TokenCount(tc) = &e.msg {
                    tc.info.as_ref().map(|info| info.total_token_usage.clone())
                } else {
                    None
                }
            })
    }

    /// Synchronous event push — usable from non-async contexts (e.g. tool
    /// handler building approval events).
    pub fn emit_event_sync(&self, event: Event) {
        let mut guard = self.events.lock().expect("lock poisoned");
        guard.push_back(event);
        if guard.len() > self.capacity {
            guard.pop_front();
            *self.offset.lock().expect("lock poisoned") += 1;
        }
    }

    /// Return a snapshot of the buffer state for CAN serialization.
    ///
    /// Returns `(offset, events_vec)` so the new run can restore the exact
    /// buffer state.
    pub fn snapshot(&self) -> (usize, Vec<Event>) {
        let guard = self.events.lock().expect("lock poisoned");
        let offset = *self.offset.lock().expect("lock poisoned");
        (offset, guard.iter().cloned().collect())
    }

    /// Construct a `BufferEventSink` from a CAN snapshot.
    pub fn from_snapshot(offset: usize, events: Vec<Event>, capacity: usize) -> Self {
        Self {
            offset: Mutex::new(offset),
            events: Mutex::new(VecDeque::from(events)),
            capacity,
        }
    }
}

#[async_trait::async_trait]
impl codex_core::EventSink for BufferEventSink {
    async fn emit_event(&self, event: Event) {
        let mut guard = self.events.lock().expect("lock poisoned");
        guard.push_back(event);
        if guard.len() > self.capacity {
            guard.pop_front();
            *self.offset.lock().expect("lock poisoned") += 1;
        }
    }
}
