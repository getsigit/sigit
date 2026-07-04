//! Durable session storage.
//!
//! One JSON-lines file per session at `$SIGIT_CONFIG_DIR/sessions/<id>.jsonl`
//! (config dir resolution matches [`crate::settings`] / [`crate::credentials`]:
//! `$SIGIT_CONFIG_DIR` or `~/.config/sigit`). Each line is one history message
//! as produced by `InferenceBackend::history_snapshot`, so a saved file can be
//! restored into either backend.
//!
//! Writes are atomic (temp file + rename) so a crash mid-save never leaves a
//! truncated session behind. Session ids are sanitized to a filename-safe
//! alphabet before touching the filesystem.

use std::path::PathBuf;

use serde_json::Value;

/// Config directory: `$SIGIT_CONFIG_DIR` or `~/.config/sigit`.
fn config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SIGIT_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/sigit"))
}

fn sessions_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("sessions"))
}

/// Reduce a session id to a filename-safe form: `[A-Za-z0-9._-]` pass through,
/// anything else becomes `_`. An empty id maps to `_` so the file name never
/// collapses to just the extension.
fn sanitize_id(session_id: &str) -> String {
    if session_id.is_empty() {
        return "_".to_string();
    }
    session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn session_path(session_id: &str) -> Option<PathBuf> {
    sessions_dir().map(|dir| dir.join(format!("{}.jsonl", sanitize_id(session_id))))
}

/// Persist a history snapshot for `session_id`, replacing any previous save.
/// The write is atomic: a temp file in the same directory is renamed over the
/// final path.
pub fn save(session_id: &str, history: &[Value]) -> Result<(), String> {
    let path =
        session_path(session_id).ok_or_else(|| "cannot resolve config directory".to_string())?;
    let dir = path
        .parent()
        .ok_or_else(|| "session path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|error| format!("create {dir:?}: {error}"))?;

    let mut body = String::new();
    for message in history {
        body.push_str(&message.to_string());
        body.push('\n');
    }

    // Unique temp name so two processes saving the same session can't clobber
    // each other's half-written file; rename is atomic on the same filesystem.
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        sanitize_id(session_id),
        std::process::id()
    ));
    std::fs::write(&tmp, body).map_err(|error| format!("write {tmp:?}: {error}"))?;
    std::fs::rename(&tmp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {tmp:?} -> {path:?}: {error}")
    })?;
    Ok(())
}

/// Load the saved history for `session_id`, or `None` when no save exists (or
/// it cannot be read). Unparseable lines are skipped rather than failing the
/// whole restore.
pub fn load(session_id: &str) -> Option<Vec<Value>> {
    let path = session_path(session_id)?;
    let contents = std::fs::read_to_string(&path).ok()?;
    Some(
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect(),
    )
}

/// Remove the saved history for `session_id`. Missing files are fine.
pub fn delete(session_id: &str) {
    if let Some(path) = session_path(session_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// One saved session as seen on disk.
///
/// Cross-platform like the rest of the store, though today only the Unix-only
/// TUI (`chat.rs` History tab) consumes it — hence the non-Unix dead-code gate,
/// mirroring `permissions::TUI_SESSION`.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// The sanitized id (the file stem), which `load`/`delete` accept as-is.
    pub id: String,
    /// Last-modified time of the session file; `UNIX_EPOCH` when unreadable.
    pub modified: std::time::SystemTime,
    /// Number of history messages (non-empty lines) in the file.
    pub message_count: usize,
}

/// List the saved sessions, newest first. Non-`.jsonl` entries (temp files,
/// stray junk) are skipped. A missing or unreadable sessions dir yields an
/// empty list.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn list() -> Vec<SessionEntry> {
    let Some(dir) = sessions_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<SessionEntry> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            // Cheap line count: no JSON parsing, just non-empty lines.
            let contents = std::fs::read_to_string(&path).ok()?;
            let message_count = contents.lines().filter(|l| !l.trim().is_empty()).count();
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some(SessionEntry {
                id,
                modified,
                message_count,
            })
        })
        .collect();
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_chars_and_replaces_the_rest() {
        assert_eq!(sanitize_id("abc-DEF_123.z"), "abc-DEF_123.z");
        assert_eq!(sanitize_id("a/b\\c:d e"), "a_b_c_d_e");
        assert_eq!(sanitize_id("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_id(""), "_");
    }

    // One test for the filesystem behavior because it mutates the
    // process-global `SIGIT_CONFIG_DIR` env var (same pattern as the settings
    // tests): splitting would race under the parallel test runner.
    #[test]
    fn save_load_delete_round_trip() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("sigit_sessions_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: serialized by ENV_TEST_LOCK; restored below.
        unsafe { std::env::set_var("SIGIT_CONFIG_DIR", &dir) };

        // Missing file → None.
        assert_eq!(load("nope"), None);

        let history = vec![
            serde_json::json!({ "role": "system", "content": "sys" }),
            serde_json::json!({ "role": "user", "content": "hi\nthere" }),
            serde_json::json!({
                "role": "assistant", "content": null,
                "tool_calls": [{ "id": "call_1", "type": "function",
                    "function": { "name": "read_file", "arguments": "{}" } }],
            }),
        ];
        save("sess-1", &history).unwrap();
        assert_eq!(load("sess-1"), Some(history.clone()));

        // Saving again replaces, not appends.
        let shorter = vec![serde_json::json!({ "role": "user", "content": "only" })];
        save("sess-1", &shorter).unwrap();
        assert_eq!(load("sess-1"), Some(shorter));

        // A hostile id stays inside the sessions dir via sanitization.
        save("../escape", &history).unwrap();
        assert!(dir.join("sessions").join(".._escape.jsonl").is_file());
        assert_eq!(load("../escape"), Some(history));
        delete("../escape");
        assert_eq!(load("../escape"), None);

        delete("sess-1");
        assert_eq!(load("sess-1"), None);
        // Deleting a missing session is a no-op.
        delete("sess-1");

        unsafe { std::env::remove_var("SIGIT_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Same single-test-per-env-var pattern as `save_load_delete_round_trip`:
    // this mutates `SIGIT_CONFIG_DIR`, so everything runs under one lock hold.
    #[test]
    fn list_returns_sessions_newest_first_and_skips_junk() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("sigit_sessions_list_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: serialized by ENV_TEST_LOCK; restored below.
        unsafe { std::env::set_var("SIGIT_CONFIG_DIR", &dir) };

        // No sessions dir at all → empty list, not an error.
        assert!(list().is_empty());

        let msg = |text: &str| serde_json::json!({ "role": "user", "content": text });
        save("older", &[msg("a"), msg("b"), msg("c")]).unwrap();
        // Distinct mtimes so the newest-first order is deterministic.
        std::thread::sleep(std::time::Duration::from_millis(50));
        save("newer", &[msg("x")]).unwrap();

        // Junk the lister must skip: wrong extension, a stray temp file, and a
        // subdirectory.
        let sessions = dir.join("sessions");
        std::fs::write(sessions.join("notes.txt"), "not a session\n").unwrap();
        std::fs::write(sessions.join(".older.999.tmp"), "half-written\n").unwrap();
        std::fs::create_dir_all(sessions.join("nested.jsonl")).unwrap();

        let listed = list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "newer");
        assert_eq!(listed[0].message_count, 1);
        assert_eq!(listed[1].id, "older");
        assert_eq!(listed[1].message_count, 3);
        assert!(listed[0].modified >= listed[1].modified);

        unsafe { std::env::remove_var("SIGIT_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
