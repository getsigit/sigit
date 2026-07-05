//! Local user preferences.
//!
//! Persisted as TOML at `$SIGIT_CONFIG_DIR/settings.toml` or
//! `~/.config/sigit/settings.toml`. Mirrors the storage pattern of
//! [`crate::credentials`] but holds preferences rather than secrets, so it is
//! not permission-restricted.
//!
//! Settings today: `local_inference` (whether on-device inference is the
//! active mode — the source of truth for the local/cloud toggle, also surfaced
//! as a session config option for ACP clients without slash commands, e.g.
//! Xcode) and `[permissions]` (the tool permission policy consumed by
//! `crate::permissions`: a default mode for mutating tools plus per-tool
//! overrides).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Env override for `local_inference`. When set to a truthy/falsy value it wins
/// over the stored file for reads (matching the existing `SIGIT_*` override
/// style); it never writes the file.
const LOCAL_INFERENCE_ENV: &str = "SIGIT_LOCAL_INFERENCE";

/// Env override for the default permission mode (`allow`/`ask`/`deny`). Wins
/// over the stored default (but not over per-tool overrides); never writes the
/// file. The escape hatch for ACP clients that cannot answer permission
/// requests and for headless runs.
const PERMISSIONS_ENV: &str = "SIGIT_PERMISSIONS";

fn default_local_inference() -> bool {
    true
}

/// What to do when the model calls a mutating tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Run without asking.
    Allow,
    /// Ask the user first (ACP permission request / TUI approval prompt).
    #[default]
    Ask,
    /// Never run; the model gets an explanatory tool result.
    Deny,
}

impl PermissionMode {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "allow" => Some(Self::Allow),
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        })
    }
}

/// The `[permissions.rules]` table: ordered allow/deny lists of rule strings.
/// A rule is `tool_name` or `tool_name(argument_pattern)`; the pattern is
/// matched against the command string for `run_command` and the path for the
/// file-mutating tools (see `crate::permissions` for the matching semantics).
///
/// ```toml
/// [permissions.rules]
/// allow = ["run_command(git *)", "edit_file(src/*)"]
/// deny = ["run_command(git push*)"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PermissionRules {
    /// Rules that let a matching call run without asking.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Rules that block a matching call. Checked before `allow`, so a deny
    /// always beats an allow that matches the same call.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// The `[permissions]` table: a default mode for mutating tools plus per-tool
/// overrides and argument-level rule lists, e.g.
///
/// ```toml
/// [permissions]
/// default = "ask"
///
/// [permissions.tools]
/// edit_file = "allow"
/// delete_file = "deny"
///
/// [permissions.rules]
/// allow = ["run_command(cargo *)"]
/// deny = ["run_command(git push*)"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PermissionSettings {
    /// Mode for mutating tools without a per-tool override. `ask` on a fresh
    /// install.
    #[serde(default)]
    pub default: PermissionMode,
    /// Per-tool overrides by tool name (MCP tools use their full
    /// `mcp__<server>__<tool>` name).
    #[serde(default)]
    pub tools: BTreeMap<String, PermissionMode>,
    /// Argument-level allow/deny rules, consulted after session grants and
    /// before the per-tool overrides.
    #[serde(default)]
    pub rules: PermissionRules,
}

/// Persisted preferences. New fields must carry `#[serde(default)]` so older
/// files keep deserializing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Whether on-device inference is the active mode. `true` (local-first) on a
    /// fresh install.
    #[serde(default = "default_local_inference")]
    pub local_inference: bool,
    /// Permission policy for mutating agent tools.
    #[serde(default)]
    pub permissions: PermissionSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            local_inference: default_local_inference(),
            permissions: PermissionSettings::default(),
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

/// The default permission mode for mutating tools: the `SIGIT_PERMISSIONS` env
/// var when set to a recognized mode, else the stored `[permissions] default`.
pub fn permission_default() -> PermissionMode {
    if let Ok(raw) = std::env::var(PERMISSIONS_ENV)
        && let Some(mode) = PermissionMode::parse(&raw)
    {
        return mode;
    }
    load().permissions.default
}

/// The stored `[permissions.rules]` allow/deny lists.
pub fn permission_rules() -> PermissionRules {
    load().permissions.rules
}

/// The effective permission mode for one tool: its `[permissions.tools]`
/// override when present, else the default (see [`permission_default`]).
pub fn permission_mode_for(tool_name: &str) -> PermissionMode {
    let settings = load();
    if let Some(mode) = settings.permissions.tools.get(tool_name) {
        return *mode;
    }
    if let Ok(raw) = std::env::var(PERMISSIONS_ENV)
        && let Some(mode) = PermissionMode::parse(&raw)
    {
        return mode;
    }
    settings.permissions.default
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

        // Permissions: fresh install asks; per-tool overrides win over the
        // default; the env var overrides the stored default but not per-tool
        // overrides.
        unsafe { std::env::remove_var(PERMISSIONS_ENV) };
        assert_eq!(permission_default(), PermissionMode::Ask);
        assert_eq!(permission_mode_for("run_command"), PermissionMode::Ask);

        let mut settings = load();
        settings.permissions.default = PermissionMode::Allow;
        settings
            .permissions
            .tools
            .insert("delete_file".to_string(), PermissionMode::Deny);
        store(&settings).unwrap();
        assert_eq!(permission_default(), PermissionMode::Allow);
        assert_eq!(permission_mode_for("run_command"), PermissionMode::Allow);
        assert_eq!(permission_mode_for("delete_file"), PermissionMode::Deny);

        unsafe { std::env::set_var(PERMISSIONS_ENV, "deny") };
        assert_eq!(permission_default(), PermissionMode::Deny);
        assert_eq!(permission_mode_for("run_command"), PermissionMode::Deny);
        assert_eq!(
            permission_mode_for("delete_file"),
            PermissionMode::Deny,
            "per-tool override still wins"
        );
        unsafe { std::env::set_var(PERMISSIONS_ENV, "garbage") };
        assert_eq!(
            permission_default(),
            PermissionMode::Allow,
            "unrecognized env value falls back to stored setting"
        );

        // Permission rules: absent on a fresh file, and they survive a
        // store/load round trip without disturbing the other settings.
        assert_eq!(permission_rules(), PermissionRules::default());
        let mut settings = load();
        settings.permissions.rules.allow = vec![
            "run_command(git *)".to_string(),
            "edit_file(src/*)".to_string(),
        ];
        settings.permissions.rules.deny = vec!["run_command(git push*)".to_string()];
        store(&settings).unwrap();
        let reloaded = load();
        assert_eq!(reloaded.permissions.rules, settings.permissions.rules);
        assert_eq!(permission_rules(), settings.permissions.rules);
        assert_eq!(
            reloaded.permissions.tools.get("delete_file"),
            Some(&PermissionMode::Deny),
            "storing rules preserves the per-tool overrides"
        );

        unsafe { std::env::remove_var(PERMISSIONS_ENV) };
        unsafe { std::env::remove_var(LOCAL_INFERENCE_ENV) };
        unsafe { std::env::remove_var("SIGIT_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
