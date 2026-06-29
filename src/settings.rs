//! Local user preferences.
//!
//! Persisted as TOML at `$SIGIT_CONFIG_DIR/settings.toml` or
//! `~/.config/sigit/settings.toml`. Mirrors the storage pattern of
//! [`crate::credentials`] but holds preferences rather than secrets, so it is
//! not permission-restricted.
//!
//! The only setting today is `local_inference`: whether on-device inference is
//! the active mode. It is the source of truth for the local/cloud toggle and
//! drives how `/models` presents the picker. It is stored locally so the toggle
//! works even on ACP clients that do not support slash commands (e.g. Xcode),
//! where it is also surfaced as a session config option.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Env override for `local_inference`. When set to a truthy/falsy value it wins
/// over the stored file for reads (matching the existing `SIGIT_*` override
/// style); it never writes the file.
const LOCAL_INFERENCE_ENV: &str = "SIGIT_LOCAL_INFERENCE";

fn default_local_inference() -> bool {
    true
}

/// Persisted preferences. New fields must carry `#[serde(default)]` so older
/// files keep deserializing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Whether on-device inference is the active mode. `true` (local-first) on a
    /// fresh install.
    #[serde(default = "default_local_inference")]
    pub local_inference: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            local_inference: default_local_inference(),
        }
    }
}

/// Config directory: `$SIGIT_CONFIG_DIR` or `~/.config/sigit`.
fn config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SIGIT_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/sigit"))
}

fn settings_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("settings.toml"))
}

/// Parse an env value as a boolean. Accepts `1/0`, `true/false`, `on/off`,
/// `yes/no` (case-insensitive). Returns `None` for anything unrecognized so a
/// stray value falls back to the stored setting instead of silently flipping.
fn parse_bool_env(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Load stored settings, or defaults if the file is absent or unreadable.
pub fn load() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => toml::from_str::<Settings>(&contents).unwrap_or_else(|error| {
            log::warn!("settings: ignoring settings.toml: {error}");
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

/// Persist settings, creating the config dir if needed.
pub fn store(settings: &Settings) -> Result<(), String> {
    let dir = config_dir().ok_or_else(|| "cannot resolve config directory".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {dir:?}: {error}"))?;
    let path = dir.join("settings.toml");
    let body = toml::to_string(settings).map_err(|error| format!("serialize settings: {error}"))?;
    std::fs::write(&path, body).map_err(|error| format!("write {path:?}: {error}"))?;
    Ok(())
}

/// Whether on-device inference is the active mode. The `SIGIT_LOCAL_INFERENCE`
/// env var, when set to a recognized boolean, overrides the stored value.
pub fn local_inference_enabled() -> bool {
    if let Ok(raw) = std::env::var(LOCAL_INFERENCE_ENV)
        && let Some(value) = parse_bool_env(&raw)
    {
        return value;
    }
    load().local_inference
}

/// Persist a new `local_inference` value, preserving any other settings.
pub fn set_local_inference(enabled: bool) -> Result<(), String> {
    let mut settings = load();
    settings.local_inference = enabled;
    store(&settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test (not several) because each mutates the process-global
    // `SIGIT_CONFIG_DIR` / `SIGIT_LOCAL_INFERENCE` env vars; splitting would let
    // them race under `cargo test`'s parallel runner.
    #[test]
    fn defaults_round_trip_and_env_override() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("sigit_settings_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: single-threaded test; restores below.
        unsafe { std::env::set_var("SIGIT_CONFIG_DIR", &dir) };
        unsafe { std::env::remove_var(LOCAL_INFERENCE_ENV) };

        // Fresh install: no file → local-first.
        assert!(
            load().local_inference,
            "fresh install should be local-first"
        );
        assert!(local_inference_enabled());

        set_local_inference(false).unwrap();
        assert!(!load().local_inference);
        assert!(!local_inference_enabled());

        // Env override wins over the stored `false`.
        unsafe { std::env::set_var(LOCAL_INFERENCE_ENV, "true") };
        assert!(local_inference_enabled());
        unsafe { std::env::set_var(LOCAL_INFERENCE_ENV, "garbage") };
        assert!(
            !local_inference_enabled(),
            "unrecognized env value falls back to stored setting"
        );

        unsafe { std::env::remove_var(LOCAL_INFERENCE_ENV) };
        unsafe { std::env::remove_var("SIGIT_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
