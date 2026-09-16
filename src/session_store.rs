//! Durable session storage.
//!
//! One JSON-lines file per session at `$SIGIT_CONFIG_DIR/sessions/<id>.jsonl`
//! (config dir resolution matches [`crate::settings`] / [`crate::credentials`]:
//! `$SIGIT_CONFIG_DIR` or `~/.config/sigit`). Each line is one history message
//! as produced by `InferenceBackend::history_snapshot`, so a saved file can be
//! restored into either backend.
//!
//! A session also carries a small sidecar at `<id>.meta.json` holding the
//! directories it ran in. History alone can't say where a thread belongs, and
//! ACP's `session/list` — the "Import Threads" picker in editors — must report
//! an absolute `cwd` per session and may ask for only the sessions of one
//! directory. A session without a sidecar is therefore not listable (it still
//! loads fine); that is the case for threads saved before sidecars existed.
//!
//! Writes are atomic (temp file + rename) so a crash mid-save never leaves a
//! truncated session behind. Session ids are sanitized to a filename-safe
//! alphabet before touching the filesystem.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
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

fn meta_path(session_id: &str) -> Option<PathBuf> {
    sessions_dir().map(|dir| dir.join(format!("{}.meta.json", sanitize_id(session_id))))
}

/// Write `body` to `path` through a uniquely named temp file in the same
/// directory, then rename it into place. The unique name keeps two processes
/// saving the same session from clobbering each other's half-written file;
/// rename is atomic on the same filesystem.
fn write_atomic(path: &Path, session_id: &str, body: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "session path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|error| format!("create {dir:?}: {error}"))?;

    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        sanitize_id(session_id),
        std::process::id()
    ));
    std::fs::write(&tmp, body).map_err(|error| format!("write {tmp:?}: {error}"))?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {tmp:?} -> {path:?}: {error}")
    })
}

/// The directories a saved session ran in, as stored in its sidecar.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The session's primary working directory (absolute).
    pub cwd: PathBuf,
    /// Extra workspace roots of a multi-root project, in order.
    #[serde(default)]
    pub additional_directories: Vec<PathBuf>,
}

/// Record where `session_id` is running so it can be listed later. Cheap enough
/// to call on every save; the sidecar is a few hundred bytes.
pub fn save_meta(session_id: &str, cwd: &Path, additional_directories: &[PathBuf]) {
    let Some(path) = meta_path(session_id) else {
        return;
    };
    let meta = SessionMeta {
        cwd: cwd.to_path_buf(),
        additional_directories: additional_directories.to_vec(),
    };
    let Ok(body) = serde_json::to_string(&meta) else {
        return;
    };
    if let Err(error) = write_atomic(&path, session_id, &body) {
        log::warn!("session meta save failed for {session_id}: {error}");
    }
}

/// Read the sidecar for `session_id`, or `None` when there isn't a readable one.
fn load_meta(session_id: &str) -> Option<SessionMeta> {
    let path = meta_path(session_id)?;
    let contents = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Persist a history snapshot for `session_id`, replacing any previous save.
/// The write is atomic: a temp file in the same directory is renamed over the
/// final path.
pub fn save(session_id: &str, history: &[Value]) -> Result<(), String> {
    let path =
        session_path(session_id).ok_or_else(|| "cannot resolve config directory".to_string())?;

    let mut body = String::new();
    for message in history {
        body.push_str(&message.to_string());
        body.push('\n');
    }

    write_atomic(&path, session_id, &body)
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

/// Remove the saved history for `session_id`, sidecar included. Missing files
/// are fine.
pub fn delete(session_id: &str) {
    if let Some(path) = session_path(session_id) {
        let _ = std::fs::remove_file(path);
    }
    if let Some(path) = meta_path(session_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// One saved session as seen on disk.
///
/// Consumed by the Unix-only TUI (`chat.rs` History tab) and, on every
/// platform, by the ACP `session/list` handler in `main.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// The sanitized id (the file stem), which `load`/`delete` accept as-is.
    pub id: String,
    /// Last-modified time of the session file; `UNIX_EPOCH` when unreadable.
    pub modified: std::time::SystemTime,
    /// Number of history messages (non-empty lines) in the file.
    pub message_count: usize,
    /// The session's working directory, when a sidecar recorded one.
    pub cwd: Option<PathBuf>,
    /// Extra workspace roots recorded alongside `cwd`.
    pub additional_directories: Vec<PathBuf>,
    /// A one-line title taken from the first user message, when there is one.
    pub title: Option<String>,
}

/// Longest title handed out by [`session_title`] before it is cut short.
const TITLE_MAX_CHARS: usize = 80;

/// The plain text of a history message, whether `content` is a string or the
/// structured `[{ "type": "text", "text": … }]` form.
fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A human-readable title for a session: the first line of its first user
/// message, trimmed and cut to [`TITLE_MAX_CHARS`].
fn session_title(contents: &str) -> Option<String> {
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if message["role"] != "user" {
            continue;
        }
        let text = message_text(&message);
        let Some(first_line) = text.lines().map(str::trim).find(|l| !l.is_empty()) else {
            continue;
        };
        let mut title: String = first_line.chars().take(TITLE_MAX_CHARS).collect();
        if first_line.chars().count() > TITLE_MAX_CHARS {
            title.push('…');
        }
        return Some(title);
    }
    None
}

/// List the saved sessions, newest first. Non-`.jsonl` entries (temp files,
/// sidecars, stray junk) are skipped. A missing or unreadable sessions dir
/// yields an empty list.
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
            let contents = std::fs::read_to_string(&path).ok()?;
            let message_count = contents.lines().filter(|l| !l.trim().is_empty()).count();
            let title = session_title(&contents);
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            let meta = load_meta(&id);
            let (cwd, additional_directories) = match meta {
                Some(meta) => (Some(meta.cwd), meta.additional_directories),
                None => (None, Vec::new()),
            };
            Some(SessionEntry {
                id,
                modified,
                message_count,
                cwd,
                additional_directories,
                title,
            })
        })
        .collect();
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    sessions
}

/// Format a timestamp as an ISO 8601 UTC instant (`2026-09-16T10:08:15Z`),
/// which is what ACP's `SessionInfo.updatedAt` expects. Hand-rolled rather than
/// pulling in a date crate for one field; pre-epoch times clamp to the epoch.
pub fn iso8601(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let seconds_of_day = secs % 86_400;
    let (hour, minute, second) = (
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    );

    // Howard Hinnant's civil_from_days: shift the epoch to 0000-03-01 so leap
    // days land at the end of the era and the month arithmetic stays branchless.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
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
        // Titles come from the first user message; no sidecar means no cwd.
        assert_eq!(listed[0].title.as_deref(), Some("x"));
        assert_eq!(listed[1].title.as_deref(), Some("a"));
        assert_eq!(listed[0].cwd, None);
        assert!(listed[0].additional_directories.is_empty());

        // A sidecar makes the session listable with its directories, and
        // deleting the session takes the sidecar with it.
        save_meta(
            "newer",
            Path::new("/tmp/project"),
            &[PathBuf::from("/tmp/lib")],
        );
        let listed = list();
        assert_eq!(listed[0].cwd.as_deref(), Some(Path::new("/tmp/project")));
        assert_eq!(
            listed[0].additional_directories,
            vec![PathBuf::from("/tmp/lib")]
        );
        assert!(sessions.join("newer.meta.json").is_file());
        delete("newer");
        assert!(!sessions.join("newer.meta.json").exists());
        assert_eq!(list().len(), 1);

        unsafe { std::env::remove_var("SIGIT_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn title_uses_the_first_line_of_the_first_user_message() {
        let contents = "{\"role\":\"system\",\"content\":\"sys\"}\n\
             {\"role\":\"user\",\"content\":\"  fix the parser\\nmore detail\"}\n\
             {\"role\":\"user\",\"content\":\"second\"}\n";
        assert_eq!(session_title(contents).as_deref(), Some("fix the parser"));

        // Structured content blocks, not just plain strings.
        let blocks =
            "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"from a block\"}]}\n";
        assert_eq!(session_title(blocks).as_deref(), Some("from a block"));

        // Unparseable lines are skipped, not fatal.
        let junk = "not json\n{\"role\":\"user\",\"content\":\"still found\"}\n";
        assert_eq!(session_title(junk).as_deref(), Some("still found"));

        // Long titles are cut short with an ellipsis.
        let long = format!(
            "{{\"role\":\"user\",\"content\":\"{}\"}}\n",
            "x".repeat(TITLE_MAX_CHARS + 10)
        );
        let title = session_title(&long).unwrap();
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
        assert!(title.ends_with('…'));

        // Nothing from the user yet → no title.
        assert_eq!(
            session_title("{\"role\":\"assistant\",\"content\":\"hi\"}\n"),
            None
        );
        assert_eq!(session_title(""), None);
    }

    #[test]
    fn iso8601_formats_utc_instants() {
        use std::time::{Duration, UNIX_EPOCH};

        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        // 2026-09-16T10:08:15Z
        assert_eq!(
            iso8601(UNIX_EPOCH + Duration::from_secs(1_789_553_295)),
            "2026-09-16T10:08:15Z"
        );
        // A leap day, to exercise the civil-date arithmetic.
        assert_eq!(
            iso8601(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00Z"
        );
    }
}
