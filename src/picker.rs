//! Helpers for integrating Temporal sessions with the codex TUI session picker.
//!
//! The codex TUI's `resume_picker` module renders an interactive paginated list
//! backed by `ThreadsPage` / `ThreadItem`.  This module provides conversion
//! functions that translate Temporal `SessionEntry` values into the types the
//! picker expects, encoding the Temporal session ID inside a synthetic `PathBuf`
//! with the `temporal://` scheme so it can be recovered after selection.

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use codex_core::{ThreadItem, ThreadsPage};

use crate::types::SessionEntry;

/// Scheme prefix used in synthetic `PathBuf` values to carry a Temporal
/// session ID through the picker's `SessionSelection::Resume(PathBuf)`.
const TEMPORAL_SCHEME: &str = "temporal://";

/// Convert a list of Temporal `SessionEntry` values into a single
/// `ThreadsPage` suitable for the resume picker.
///
/// Each entry becomes a `ThreadItem` with:
/// - `path` set to `temporal://<session_id>` (synthetic, for ID recovery)
/// - `first_user_message` set from `SessionEntry.name`
/// - `created_at` / `updated_at` as RFC-3339 strings from `created_at_millis`
///
/// The returned page has `next_cursor: None` (no pagination — the harness
/// returns all sessions at once).
pub fn sessions_to_threads_page(sessions: Vec<SessionEntry>) -> ThreadsPage {
    let items: Vec<ThreadItem> = sessions
        .into_iter()
        .map(|entry| {
            let ts = millis_to_rfc3339(entry.created_at_millis);
            ThreadItem {
                path: PathBuf::from(format!("{TEMPORAL_SCHEME}{}", entry.session_id)),
                thread_id: None,
                first_user_message: entry.name.or(Some(entry.session_id.clone())),
                cwd: None,
                git_branch: None,
                git_sha: None,
                git_origin_url: None,
                source: None,
                model_provider: None,
                cli_version: None,
                created_at: Some(ts.clone()),
                updated_at: Some(ts),
            }
        })
        .collect();
    ThreadsPage {
        items,
        next_cursor: None,
        num_scanned_files: 0,
        reached_scan_cap: false,
    }
}

/// Convert Unix-millisecond timestamp to an RFC-3339 string.
pub fn millis_to_rfc3339(millis: u64) -> String {
    let secs = (millis / 1000) as i64;
    let nanos = ((millis % 1000) * 1_000_000) as u32;
    match Utc.timestamp_opt(secs, nanos) {
        chrono::LocalResult::Single(dt) => dt.to_rfc3339(),
        _ => DateTime::<Utc>::default().to_rfc3339(),
    }
}

/// Extract the Temporal session ID from a synthetic `temporal://<id>` path
/// produced by [`sessions_to_threads_page`].
pub fn extract_session_id_from_path(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    s.strip_prefix(TEMPORAL_SCHEME).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionStatus;

    #[test]
    fn sessions_to_threads_page_basic() {
        let entries = vec![
            SessionEntry {
                session_id: "sess-1".to_string(),
                name: Some("first session".to_string()),
                model: "gpt-4o".to_string(),
                created_at_millis: 1_700_000_000_000,
                status: SessionStatus::Running,
            },
            SessionEntry {
                session_id: "sess-2".to_string(),
                name: None,
                model: "gpt-4o-mini".to_string(),
                created_at_millis: 1_700_000_060_000,
                status: SessionStatus::Running,
            },
        ];
        let page = sessions_to_threads_page(entries);
        assert_eq!(page.items.len(), 2);
        assert!(page.next_cursor.is_none());

        // First entry: named
        assert_eq!(
            page.items[0].first_user_message.as_deref(),
            Some("first session")
        );
        assert_eq!(
            page.items[0].path,
            PathBuf::from("temporal://sess-1")
        );

        // Second entry: unnamed → falls back to session_id
        assert_eq!(
            page.items[1].first_user_message.as_deref(),
            Some("sess-2")
        );
    }

    #[test]
    fn extract_session_id_roundtrip() {
        let path = PathBuf::from("temporal://my-workflow-id");
        assert_eq!(
            extract_session_id_from_path(&path),
            Some("my-workflow-id".to_string())
        );
    }

    #[test]
    fn extract_session_id_non_temporal_path() {
        let path = PathBuf::from("/home/user/.codex/sessions/rollout.jsonl");
        assert_eq!(extract_session_id_from_path(&path), None);
    }

    #[test]
    fn millis_to_rfc3339_known_value() {
        // 2023-11-14T22:13:20Z
        let rfc = millis_to_rfc3339(1_700_000_000_000);
        assert!(rfc.starts_with("2023-11-14T22:13:20"));
    }
}
