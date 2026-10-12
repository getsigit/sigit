//! Local credential store.
//!
//! Holds the session token from `sigit login`, used to authenticate siGit Code
//! Cloud requests. Stored as TOML at `$SIGIT_CONFIG_DIR/credentials.toml` or
//! `~/.config/sigit/credentials.toml`, with `0600` permissions on Unix.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The persisted session, written on login and cleared on logout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// Bearer token issued by sigit.si.
    pub access_token: String,
    /// Account email, kept for `whoami` display.
    #[serde(default)]
    pub email: Option<String>,
}

/// Config directory: `$SIGIT_CONFIG_DIR` or `~/.config/sigit`.
fn config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SIGIT_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/sigit"))
}

fn credentials_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("credentials.toml"))
}

/// Load stored credentials, or `None` if not logged in.
pub fn load() -> Option<Credentials> {
    let path = credentials_path()?;
    let contents = std::fs::read_to_string(&path).ok()?;
    match toml::from_str::<Credentials>(&contents) {
        Ok(credentials) if !credentials.access_token.trim().is_empty() => Some(credentials),
        _ => None,
    }
}

/// Convenience: the bearer token alone, if logged in.
pub fn load_token() -> Option<String> {
    load().map(|credentials| credentials.access_token)
}

/// Persist credentials, creating the config dir and restricting permissions.
pub fn store(credentials: &Credentials) -> Result<(), String> {
    let dir = config_dir().ok_or_else(|| "cannot resolve config directory".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {dir:?}: {error}"))?;
    let path = dir.join("credentials.toml");
    let body =
        toml::to_string(credentials).map_err(|error| format!("serialize credentials: {error}"))?;
    std::fs::write(&path, body).map_err(|error| format!("write {path:?}: {error}"))?;
    restrict_permissions(&path);
    Ok(())
}

/// Remove stored credentials. Returns `true` if a file was deleted.
pub fn clear() -> bool {
    match credentials_path() {
        Some(path) if path.exists() => std::fs::remove_file(&path).is_ok(),
        _ => false,
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_credentials_via_temp_dir() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("sigit_creds_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: single-threaded test; restores below.
        unsafe { std::env::set_var("SIGIT_CONFIG_DIR", &dir) };

        assert!(load().is_none());
        store(&Credentials {
            access_token: "tok_123".to_string(),
            email: Some("dev@sigit.si".to_string()),
        })
        .unwrap();

        let loaded = load().expect("credentials present");
        assert_eq!(loaded.access_token, "tok_123");
        assert_eq!(loaded.email.as_deref(), Some("dev@sigit.si"));
        assert_eq!(load_token().as_deref(), Some("tok_123"));

        assert!(clear());
        assert!(load().is_none());

        unsafe { std::env::remove_var("SIGIT_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
