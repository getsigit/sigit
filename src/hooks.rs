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
//! Every value used in `{var}` substitution is quote-escaped for the platform
//! shell before being spliced into the command line (`posix_quote` /
//! `windows_quote`), so a project directory or MCP-server-supplied tool name
//! containing shell metacharacters can never execute as shell syntax. The
//! same values are also exported as `SIGIT_HOOK_*` env vars, which hook
//! scripts should prefer when they need to consume a value programmatically
//! rather than embed it in a `{var}` placeholder.
//!
//! Hooks run in the session cwd and inherit the environment. Shell hook
//! failures are logged but do not interrupt the session. `PreToolUse` /
//! `PostToolUse` hooks fire for every call to [`crate::tools::execute_tool`],
//! including tool calls made by a `task` subagent — there is no separate
//! "top-level only" mode today.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a single hook command may run before it is killed, unless
/// `[hooks] timeout_secs` overrides it. Without a bound, a hook that never
/// exits (waits on input, a stalled network mount, a deadlock) blocks the
/// tool call or session start that fired it forever.
const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 30;

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
    /// Per-hook wall-clock limit in seconds. `None` uses
    /// [`DEFAULT_HOOK_TIMEOUT_SECS`]. Raise it for slow hooks (a build, a
    /// linter); a hook that exceeds it is killed and logged.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl HookSettings {
    /// Whether any hooks are configured.
    pub fn has_hooks(&self) -> bool {
        !self.session_start.is_empty()
            || !self.pre_tool_use.is_empty()
            || !self.post_tool_use.is_empty()
    }

    /// The wall-clock limit applied to each hook command.
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS))
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

    /// Substitute `{var}` placeholders in the command string, passing each
    /// value through `quote` first so it can never be interpreted as shell
    /// syntax regardless of its contents.
    fn substitute(&self, cmd: &str, quote: fn(&str) -> String) -> String {
        let mut result = cmd.to_string();
        if let Some(cwd) = &self.cwd {
            result = result.replace("{cwd}", &quote(cwd));
        }
        if let Some(session_id) = &self.session_id {
            result = result.replace("{session_id}", &quote(session_id));
        }
        if let Some(tool_name) = &self.tool_name {
            result = result.replace("{tool_name}", &quote(tool_name));
        }
        if let Some(tool_args_len) = self.tool_args_len {
            // Numeric — always a safe literal, no quoting needed.
            result = result.replace("{tool_args_len}", &tool_args_len.to_string());
        }
        if let Some(tool_result_len) = self.tool_result_len {
            result = result.replace("{tool_result_len}", &tool_result_len.to_string());
        }
        result
    }

    /// The same context values as `SIGIT_HOOK_*` environment variables, so a
    /// hook script can read them without any quoting or substitution at all.
    fn env_vars(&self) -> Vec<(&'static str, String)> {
        let mut vars = Vec::new();
        if let Some(cwd) = &self.cwd {
            vars.push(("SIGIT_HOOK_CWD", cwd.clone()));
        }
        if let Some(session_id) = &self.session_id {
            vars.push(("SIGIT_HOOK_SESSION_ID", session_id.clone()));
        }
        if let Some(tool_name) = &self.tool_name {
            vars.push(("SIGIT_HOOK_TOOL_NAME", tool_name.clone()));
        }
        if let Some(tool_args_len) = self.tool_args_len {
            vars.push(("SIGIT_HOOK_TOOL_ARGS_LEN", tool_args_len.to_string()));
        }
        if let Some(tool_result_len) = self.tool_result_len {
            vars.push(("SIGIT_HOOK_TOOL_RESULT_LEN", tool_result_len.to_string()));
        }
        vars
    }
}

impl Default for HookContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap `value` as a single POSIX shell word, so it is always treated as
/// literal data by `sh -c` even if it contains spaces, quotes, or shell
/// metacharacters (e.g. a project directory or an MCP-server-supplied tool
/// name that happens to contain `$(...)` or backticks).
///
/// Only reached from production code on Unix (`run_hook`'s `#[cfg(unix)]`
/// arm); always exercised via tests below regardless of host platform.
#[cfg_attr(not(unix), allow(dead_code))]
fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Best-effort quoting for `cmd.exe`: wrap in double quotes and escape
/// embedded double quotes. `cmd` has no fully safe quoting story (see
/// `spawn_shell`'s platform split in `tools.rs`), so this covers common
/// cases but callers on Windows should prefer the `SIGIT_HOOK_*` env vars
/// (set on every hook invocation, see [`run_hook`]) over `{cwd}`-style
/// substitution when values may contain special characters.
///
/// Only reached from production code on Windows (`run_hook`'s `#[cfg(windows)]`
/// arm); exercised on every platform via `test_windows_quote_neutralizes_embedded_quotes`
/// below, since `aws-lc-sys` can't cross-compile to the Windows target from this host.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Run a single hook command in the given context. Substituted `{var}`
/// placeholders are shell-quoted so a value like a directory name or an
/// MCP tool name can never break out of its position and run as shell
/// syntax. The same values are also exported as `SIGIT_HOOK_*` env vars
/// (`SIGIT_HOOK_CWD`, `SIGIT_HOOK_SESSION_ID`, `SIGIT_HOOK_TOOL_NAME`,
/// `SIGIT_HOOK_TOOL_ARGS_LEN`, `SIGIT_HOOK_TOOL_RESULT_LEN`) so hook scripts
/// can read them without any quoting concerns at all.
fn run_hook(cmd: &str, context: &HookContext, cwd: &Path, timeout: Duration) {
    #[cfg(unix)]
    let (shell, flag, substituted) = ("sh", "-c", context.substitute(cmd, posix_quote));
    #[cfg(windows)]
    let (shell, flag, substituted) = ("cmd", "/C", context.substitute(cmd, windows_quote));

    log::debug!("Running hook: {substituted}");
    let mut command = Command::new(shell);
    command
        .arg(flag)
        .arg(&substituted)
        .current_dir(cwd)
        .envs(context.env_vars())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Put the hook in its own process group so a timeout kill can take out the
    // whole tree. Killing just the shell is not enough: `sh -c` may fork
    // rather than exec (dash does), and the surviving grandchild would both
    // keep running and hold the pipe write ends open, blocking the drains
    // below until it exits on its own.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            log::warn!("Failed to run hook: {err}");
            return;
        }
    };

    #[cfg(unix)]
    fn kill_hook(child: &mut std::process::Child) {
        // SIGKILL the process group (negative pid); the child is its leader.
        // Fall back to killing the child alone if that fails.
        if unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) } != 0 {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
    #[cfg(windows)]
    fn kill_hook(child: &mut std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    // Drain stdout/stderr on their own threads so a hook that writes more than
    // the pipe buffer holds (a build, say) keeps making progress instead of
    // blocking on a full pipe and looking like a hang. The threads report back
    // over a channel rather than being joined: a receive can be bounded, while
    // a join could block forever on a pipe a detached grandchild still holds.
    let (out_tx, out_rx) = std::sync::mpsc::channel::<(u8, Vec<u8>)>();
    let drain = |tag: u8, pipe: Option<Box<dyn Read + Send>>| {
        let tx = out_tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send((tag, buf));
        });
    };
    drain(0, child.stdout.take().map(|p| Box::new(p) as _));
    drain(1, child.stderr.take().map(|p| Box::new(p) as _));
    drop(out_tx);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_hook(&mut child);
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                log::warn!("Failed to wait on hook: {err}");
                kill_hook(&mut child);
                break None;
            }
        }
    };

    // Collect what the drains read, bounded: if something in the hook's tree
    // outlived the kill (or daemonized past the process group) and still holds
    // a pipe open, give up on its output after a short grace instead of
    // hanging the caller on it.
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let grace = Duration::from_secs(1);
    for _ in 0..2 {
        match out_rx.recv_timeout(grace) {
            Ok((0, buf)) => stdout = buf,
            Ok((_, buf)) => stderr = buf,
            Err(_) => {
                log::debug!("Hook output pipe still open after exit; skipping its output");
                break;
            }
        }
    }

    match status {
        None => log::warn!(
            "Hook timed out after {}s and was killed: {substituted}",
            timeout.as_secs()
        ),
        Some(status) if !status.success() => log::warn!(
            "Hook failed with status {:?}: {}",
            status.code(),
            String::from_utf8_lossy(&stderr).trim()
        ),
        Some(_) => {
            let out = String::from_utf8_lossy(&stdout);
            if !out.trim().is_empty() {
                log::debug!("Hook output: {}", out.trim());
            }
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
        run_hook(cmd, &context, cwd, hooks.timeout());
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
        run_hook(cmd, &context, cwd, hooks.timeout());
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
        run_hook(cmd, &context, cwd, hooks.timeout());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn hook_that_hangs_is_killed_at_the_timeout() {
        // A hook that would sleep far past the deadline must be killed near
        // it, not block the caller for the full sleep. The two-command form
        // forces the shell to fork `sleep` rather than exec it (what dash does
        // even for a single command), so this also proves the kill takes out
        // the whole process group: a surviving grandchild would hold the
        // output pipes open and stall the caller until its own exit.
        let context = HookContext::new();
        let start = Instant::now();
        run_hook(
            "sleep 30; echo done",
            &context,
            Path::new("."),
            Duration::from_millis(300),
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "hook should have been killed shortly after its 300ms timeout, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn timeout_defaults_when_unset_and_honors_override() {
        let default = HookSettings::default();
        assert_eq!(
            default.timeout(),
            Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS)
        );
        let custom = HookSettings {
            timeout_secs: Some(5),
            ..HookSettings::default()
        };
        assert_eq!(custom.timeout(), Duration::from_secs(5));
    }

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
            context.substitute("cd {cwd} && echo {session_id}", posix_quote),
            "cd '/home/user/project' && echo 'session-123'"
        );
        assert_eq!(
            context.substitute(
                "Tool: {tool_name}, args={tool_args_len}, result={tool_result_len}",
                posix_quote
            ),
            "Tool: 'read_file', args=42, result=1000"
        );
    }

    #[test]
    fn test_posix_quote_neutralizes_shell_metacharacters() {
        // A directory or MCP tool name containing shell syntax must come
        // through as inert literal text, not execute.
        let context = HookContext {
            cwd: Some("/tmp/$(rm -rf ~); echo pwned".to_string()),
            ..HookContext::new()
        };
        let substituted = context.substitute("echo {cwd}", posix_quote);
        assert_eq!(
            substituted, "echo '/tmp/$(rm -rf ~); echo pwned'",
            "the payload must stay inside single quotes, not break out of them"
        );

        // Embedded single quotes are the classic escape attempt; confirm the
        // standard '\'' technique closes and reopens the quoted string
        // safely instead of terminating it early.
        let context = HookContext {
            tool_name: Some("evil'; rm -rf ~; echo '".to_string()),
            ..HookContext::new()
        };
        let substituted = context.substitute("echo {tool_name}", posix_quote);
        assert_eq!(substituted, "echo 'evil'\\''; rm -rf ~; echo '\\'''");
    }

    #[test]
    fn test_windows_quote_neutralizes_embedded_quotes() {
        // aws-lc-sys can't cross-compile to the Windows target from this
        // host, so this is exercised as a plain unit test rather than a
        // Windows-target integration run.
        assert_eq!(
            windows_quote(r#"evil" & del /f /q *"#),
            r#""evil"" & del /f /q *""#
        );
        assert_eq!(
            windows_quote("/home/user/project"),
            "\"/home/user/project\""
        );
    }

    #[test]
    fn test_pre_tool_use_hook_does_not_execute_injected_shell_syntax() {
        // End-to-end: run a real `sh -c` (not just the string-substitution
        // layer) with a tool_name crafted to look like a shell command
        // injection, and confirm the injected `touch` never actually runs.
        let dir = std::env::temp_dir().join(format!(
            "sigit-hooks-test-{:?}-{}",
            std::thread::current().id(),
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("pwned");
        assert!(!marker.exists());

        let malicious_tool_name = format!("read_file'; touch {}; echo '", marker.display());
        let hooks = HookSettings {
            pre_tool_use: vec!["echo hook-ran {tool_name}".to_string()],
            ..HookSettings::default()
        };
        run_pre_tool_use_hooks(&hooks, &malicious_tool_name, "{}", &dir);

        assert!(
            !marker.exists(),
            "injected `touch` executed — {{tool_name}} substitution is not shell-safe"
        );
        std::fs::remove_dir_all(&dir).ok();
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
