//! Extensibility hooks for agent lifecycle and tool execution.
//!
//! Hooks allow users to run custom logic at key moments:
//! - `SessionStart`: when a session begins (new/load/fork)
//! - `PreToolUse`: before a tool is executed
//! - `PostToolUse`: after a tool is executed
//!
//! Hooks are configured in `settings.toml` under `[hooks]`:
//! ```toml
//! [hooks]
//! session_start = ["echo 'Starting siGit session'"]
//! pre_tool_use = ["echo 'About to run {tool_name}'"]
//! post_tool_use = ["echo 'Finished {tool_name}: {tool_result_len} chars'"]
//! ```
//!
//! Hooks support substitution for context variables:
//! - SessionStart: `{cwd}`, `{session_id}`
//! - PreToolUse: `{tool_name}`, `{tool_args_len}`
//! - PostToolUse: `{tool_name}`, `{tool_result_len}`
//!
//! Hooks run in the session cwd and inherit the environment. Shell hook failures
//! are logged but do not interrupt the session.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

/// Hook configuration: lists of shell commands to run at key moments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSettings {
    /// Commands to run when a session starts (new/load/fork).
    #[serde(default)]
    pub session_start: Vec<String>,
    /// Commands to run before a tool is executed.
    #[serde(default)]
    pub pre_tool_use: Vec<String>,
    /// Commands to run after a tool is executed.
    #[serde(default)]
    pub post_tool_use: Vec<String>,
}

impl HookSettings {
    /// Whether any hooks are configured.
    pub fn has_hooks(&self) -> bool {
        !self.session_start.is_empty()
            || !self.pre_tool_use.is_empty()
            || !self.post_tool_use.is_empty()
    }
}

/// Context passed to hook substitution.
pub struct HookContext {
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_args_len: Option<usize>,
    pub tool_result_len: Option<usize>,
}

impl HookContext {
    /// Create a new empty context.
    pub fn new() -> Self {
        Self {
            cwd: None,
            session_id: None,
            tool_name: None,
            tool_args_len: None,
            tool_result_len: None,
        }
    }

    /// Substitute variables in the command string.
    fn substitute(&self, cmd: &str) -> String {
        let mut result = cmd.to_string();
        if let Some(cwd) = &self.cwd {
            result = result.replace("{cwd}", cwd);
        }
        if let Some(session_id) = &self.session_id {
            result = result.replace("{session_id}", session_id);
        }
        if let Some(tool_name) = &self.tool_name {
            result = result.replace("{tool_name}", tool_name);
        }
        if let Some(tool_args_len) = self.tool_args_len {
            result = result.replace("{tool_args_len}", &tool_args_len.to_string());
        }
        if let Some(tool_result_len) = self.tool_result_len {
            result = result.replace("{tool_result_len}", &tool_result_len.to_string());
        }
        result
    }
}

impl Default for HookContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a single hook command in the given context.
fn run_hook(cmd: &str, context: &HookContext, cwd: &Path) {
    let substituted = context.substitute(cmd);
    log::debug!("Running hook: {substituted}");
    match Command::new("sh")
        .arg("-c")
        .arg(&substituted)
        .current_dir(cwd)
        .output()
    {
        Ok(output) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::warn!(
                    "Hook failed with status {:?}: {}",
                    output.status.code(),
                    stderr.trim()
                );
            } else {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.is_empty() {
                    log::debug!("Hook output: {}", stdout.trim());
                }
            }
        }
        Err(err) => {
            log::warn!("Failed to run hook: {err}");
        }
    }
}

/// Run all session start hooks.
pub fn run_session_start_hooks(hooks: &HookSettings, cwd: &Path, session_id: &str) {
    if hooks.session_start.is_empty() {
        return;
    }
    log::info!("Running {} session_start hooks", hooks.session_start.len());
    let context = HookContext {
        cwd: Some(cwd.display().to_string()),
        session_id: Some(session_id.to_string()),
        ..HookContext::new()
    };
    for cmd in &hooks.session_start {
        run_hook(cmd, &context, cwd);
    }
}

/// Run all pre-tool-use hooks.
pub fn run_pre_tool_use_hooks(hooks: &HookSettings, tool_name: &str, tool_args: &str, cwd: &Path) {
    if hooks.pre_tool_use.is_empty() {
        return;
    }
    log::debug!("Running {} pre_tool_use hooks", hooks.pre_tool_use.len());
    let context = HookContext {
        cwd: Some(cwd.display().to_string()),
        tool_name: Some(tool_name.to_string()),
        tool_args_len: Some(tool_args.len()),
        ..HookContext::new()
    };
    for cmd in &hooks.pre_tool_use {
        run_hook(cmd, &context, cwd);
    }
}

/// Run all post-tool-use hooks.
pub fn run_post_tool_use_hooks(
    hooks: &HookSettings,
    tool_name: &str,
    tool_result: &str,
    cwd: &Path,
) {
    if hooks.post_tool_use.is_empty() {
        return;
    }
    log::debug!("Running {} post_tool_use hooks", hooks.post_tool_use.len());
    let context = HookContext {
        cwd: Some(cwd.display().to_string()),
        tool_name: Some(tool_name.to_string()),
        tool_result_len: Some(tool_result.len()),
        ..HookContext::new()
    };
    for cmd in &hooks.post_tool_use {
        run_hook(cmd, &context, cwd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_context_substitution() {
        let context = HookContext {
            cwd: Some("/home/user/project".to_string()),
            session_id: Some("session-123".to_string()),
            tool_name: Some("read_file".to_string()),
            tool_args_len: Some(42),
            tool_result_len: Some(1000),
        };
        assert_eq!(
            context.substitute("cd {cwd} && echo {session_id}"),
            "cd /home/user/project && echo session-123"
        );
        assert_eq!(
            context.substitute("Tool: {tool_name}, args={tool_args_len}, result={tool_result_len}"),
            "Tool: read_file, args=42, result=1000"
        );
    }

    #[test]
    fn test_hook_settings_has_hooks() {
        let empty = HookSettings::default();
        assert!(!empty.has_hooks());

        let with_hooks = HookSettings {
            session_start: vec!["echo start".to_string()],
            ..HookSettings::default()
        };
        assert!(with_hooks.has_hooks());
    }
}
