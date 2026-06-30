//! Local persistence of ACP chat conversations.
//!
//! Editors such as Zed remember a thread's `SessionId` across restarts and call
//! `session/load` to reopen it. Before this module siGit cleared history on
//! load, so every reopened thread started blank. Here we store a compact
//! transcript — the user prompts and the assistant's visible replies — per
//! session under `$SIGIT_CONFIG_DIR/sessions/<id>.json`. On reload the
//! transcript is replayed to the editor and pushed back into the active backend
//! so the model keeps its context.
//!
//! Only finished user/assistant turns are stored: no tool-call plumbing and no
//! system context (that is rebuilt fresh from the cwd on every load). This keeps
//! the format backend-agnostic — the same file restores whether the session
//! resumes on-device or on a siGit Code Cloud tier.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Author of a stored message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One persisted turn in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: Role,
    pub text: String,
}

/// A persisted conversation, keyed on disk by its ACP `SessionId`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredSession {
    /// The session's working directory, kept for reference/debugging.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
}

impl StoredSession {
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Config directory: `$SIGIT_CONFIG_DIR` or `~/.config/sigit`. Mirrors
/// [`crate::settings`] and [`crate::credentials`].
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

/// Reduce a `SessionId` to a single, traversal-safe file-name stem. Editors
/// pick the id (usually a UUID); keep `[A-Za-z0-9._-]` and map anything else to
/// `_` so it can never escape the sessions directory.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Path to the transcript file for `id`, or `None` if the id can't form a valid
/// file name (empty, or only dots/separators after sanitizing).
fn session_path(id: &str) -> Option<PathBuf> {
    let stem = sanitize_id(id);
    if stem.trim_matches(['.', '_', '-']).is_empty() {
        return None;
    }
    sessions_dir().map(|dir| dir.join(format!("{stem}.json")))
}

/// Load the stored transcript for `id`, or an empty session if none exists or
/// the file can't be read or parsed.
pub fn load(id: &str) -> StoredSession {
    let Some(path) = session_path(id) else {
        return StoredSession::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|error| {
            log::warn!("sessions: ignoring unreadable {}: {error}", path.display());
            StoredSession::default()
        }),
        Err(_) => StoredSession::default(),
    }
}

/// Persist `session` for `id`, creating the sessions directory if needed.
pub fn save(id: &str, session: &StoredSession) -> Result<(), String> {
    let path = session_path(id).ok_or_else(|| format!("invalid session id: {id:?}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("create {parent:?}: {error}"))?;
    }
    let body =
        serde_json::to_string_pretty(session).map_err(|error| format!("serialize: {error}"))?;
    std::fs::write(&path, body).map_err(|error| format!("write {path:?}: {error}"))
}

/// Append a completed turn to `id`'s transcript. The user text is always
/// recorded; the assistant reply only when non-empty. Best-effort: a write
/// failure is logged, never surfaced, so persistence can't break a live turn.
pub fn append_turn(id: &str, cwd: Option<&Path>, user_text: &str, assistant_text: &str) {
    let mut session = load(id);
    if session.cwd.is_none() {
        session.cwd = cwd.map(|path| path.display().to_string());
    }
    session.messages.push(StoredMessage {
        role: Role::User,
        text: user_text.to_string(),
    });
    let assistant = assistant_text.trim();
    if !assistant.is_empty() {
        session.messages.push(StoredMessage {
            role: Role::Assistant,
            text: assistant.to_string(),
        });
    }
    if let Err(error) = save(id, &session) {
        log::warn!("sessions: could not persist turn for {id}: {error}");
    }
}

/// Forget `id`'s transcript (used by `/clear`). A missing file is not an error.
pub fn clear(id: &str) {
    if let Some(path) = session_path(id)
        && let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("sessions: could not clear {}: {error}", path.display());
    }
}

/// Copy `from`'s transcript onto `to` when a session is forked, so the fork
/// opens with the parent's history instead of blank. Best-effort.
pub fn fork(from: &str, to: &str) {
    let session = load(from);
    if session.is_empty() {
        return;
    }
    if let Err(error) = save(to, &session) {
        log::warn!("sessions: could not fork {from} -> {to}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_load_clear_round_trip() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("sigit_sessions_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: single-threaded test guarded by ENV_TEST_LOCK; restored below.
        unsafe { std::env::set_var("SIGIT_CONFIG_DIR", &dir) };

        let id = "11111111-2222-3333-4444-555555555555";
        assert!(load(id).is_empty(), "unknown session starts empty");

        append_turn(id, Some(Path::new("/tmp/project")), "hello", "hi there");
        append_turn(id, None, "second", "");

        let session = load(id);
        assert_eq!(session.cwd.as_deref(), Some("/tmp/project"));
        // user, assistant, user — the empty assistant reply is dropped.
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text, "hello");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].text, "hi there");
        assert_eq!(session.messages[2].role, Role::User);
        assert_eq!(session.messages[2].text, "second");

        let forked = "99999999-2222-3333-4444-555555555555";
        fork(id, forked);
        assert_eq!(load(forked).messages.len(), 3, "fork copies the transcript");

        clear(id);
        assert!(load(id).is_empty(), "clear forgets the transcript");
        clear(id); // clearing a missing session is a no-op

        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: single-threaded test guarded by ENV_TEST_LOCK.
        unsafe { std::env::remove_var("SIGIT_CONFIG_DIR") };
    }

    #[test]
    fn sanitize_id_blocks_path_traversal() {
        // Dots are kept, but every path separator becomes `_`, so the result is
        // always a single, non-traversing path component.
        assert_eq!(sanitize_id("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_id("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_id("uuid-1234_AB.cd"), "uuid-1234_AB.cd");
        // Ids that sanitize to nothing usable yield no path.
        assert!(session_path("..").is_none());
        assert!(session_path("/").is_none());
    }
}
