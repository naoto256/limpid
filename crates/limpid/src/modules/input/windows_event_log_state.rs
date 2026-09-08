//! Event Log checkpoint policy matching the existing journal input: optional
//! state, future-only when absent/unreadable, and fallback on resume failure.

use super::windows_event_log_sys::{StartPosition, Subscription};
use std::io;
use std::path::Path;

pub fn load(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Called only for a pipeline-completed bookmark. The actor must warn on Err
/// and keep running, like journal::save_cursor; this is not a delivery ACK or
/// an fsync durability guarantee. A failed publication leaves the older state.
pub fn save(path: &Path, bookmark: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temporary = path.with_extension("tmp");
    let result =
        std::fs::write(&temporary, bookmark).and_then(|_| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub fn subscribe(
    channel: &str,
    query: &str,
    state: Option<&Path>,
) -> io::Result<(Subscription, Option<io::Error>)> {
    let warning = if let Some(bookmark) = state.and_then(load) {
        match Subscription::open(channel, query, StartPosition::AfterBookmark(&bookmark)) {
            Ok(subscription) => return Ok((subscription, None)),
            Err(error) => Some(error),
        }
    } else {
        None
    };
    // Return the failed resume to the actor for warning; do not rewrite state
    // before an event has completed the existing pipeline ACK boundary.
    Ok((
        Subscription::open(channel, query, StartPosition::Future)?,
        warning,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn absent_empty_and_unreadable_state_mean_no_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bookmark");
        assert!(load(&path).is_none());
        std::fs::write(&path, " \r\n").unwrap();
        assert!(load(&path).is_none());
        assert!(load(dir.path()).is_none());
    }

    #[test]
    fn checkpoint_replaces_previous_position_at_unicode_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("状態").join("bookmark");
        save(&path, "first").unwrap();
        save(&path, "second").unwrap();
        assert_eq!(load(&path).as_deref(), Some("second"));
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn surrounding_whitespace_is_ignored_like_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bookmark");
        std::fs::write(&path, " \r\nposition\r\n").unwrap();
        assert_eq!(load(&path).as_deref(), Some("position"));
    }

    #[test]
    fn failed_save_is_observable_and_does_not_replace_destination() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bookmark");
        std::fs::create_dir(&path).unwrap();
        assert!(save(&path, "position").is_err());
        assert!(path.is_dir());
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn malformed_bookmark_reports_resume_failure_and_starts_future() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bookmark");
        std::fs::write(&path, "not bookmark XML").unwrap();
        let (mut subscription, warning) =
            subscribe("System", "*[System[EventID=999999]]", Some(&path)).unwrap();
        assert!(
            warning.is_some(),
            "caller must be able to log the failed resume"
        );
        assert!(
            subscription
                .next(Duration::from_millis(1))
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not bookmark XML");
    }
}
