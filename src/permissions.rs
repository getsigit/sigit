//! Tool permission policy: which agent tools may run, and when to ask.
//!
//! Every tool call funnels through one decision point before execution
//! (`decision_for`). Tools are classified by risk: *read-only* tools (reading
//! files, searching, listing, fetching a web page) always run, while *mutating*
//! tools (writing files, deleting, shell commands, MCP tools) are governed by
//! policy. The policy layers, first match wins:
//!
//! 1. **Plan mode** — a per-session switch that denies every mutating tool with
//!    a message telling the model to present a plan instead. Toggled via
//!    `/plan on|off` (TUI and ACP).
//! 2. **Session grants** — "always allow this session", recorded when the user
//!    picks that option in an approval prompt.
//! 3. **Per-tool override** — `[permissions.tools]` in `settings.toml`, e.g.
//!    `run_command = "ask"`, `edit_file = "allow"`, `delete_file = "deny"`.
//! 4. **Default mode** — `[permissions] default = "ask"|"allow"|"deny"` in
//!    `settings.toml`; `ask` on a fresh install.
//!
//! The `SIGIT_PERMISSIONS` env var (`allow`/`ask`/`deny`) overrides the stored
//! default without writing the file — the escape hatch for ACP clients that
//! cannot answer `session/request_permission` and for CI/headless runs.
//!
//! Tools discovered from MCP servers (`mcp__*`) and any unknown tool name are
//! treated as mutating: external tools can have arbitrary side effects, so the
//! safe assumption is to gate them.
//!
//! Session state (grants + plan mode) lives in a process-global keyed by
//! session id — the same pattern as `mcp.rs`'s server cache — so the ACP
//! multi-session surface and the single-session TUI share one implementation.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use crate::settings::{self, PermissionMode};

/// Session key used by the interactive TUI, which only ever has one session.
/// The TUI (`chat.rs`) is `#[cfg(unix)]`, so this is its only consumer and is
/// dead on non-Unix targets — the rest of the module is used on all platforms.
#[cfg_attr(not(unix), allow(dead_code))]
pub const TUI_SESSION: &str = "tui";

/// How risky a tool is to run without the user's sign-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRisk {
    /// Observes state without changing it; always allowed to run.
    ReadOnly,
    /// Changes files, runs commands, or has unknown side effects; governed by
    /// the permission policy.
    Mutating,
}

/// The outcome of the policy check for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run the tool without asking.
    Allow,
    /// Ask the user before running (surface-specific: ACP permission request
    /// or TUI approval prompt).
    Ask,
    /// Do not run the tool; the string is returned to the model as the tool
    /// result so it can adapt instead of retrying blindly.
    Deny(String),
}

/// Classify a tool by name. Unknown names and MCP tools are mutating: the
/// conservative default for anything whose side effects we can't see.
/// `task` is read-only because the subagent it launches is restricted to the
/// read-only toolset (see `SUBAGENT_TOOL_NAMES` in `tools.rs`), so delegated
/// research stays available in plan mode.
pub fn classify(tool_name: &str) -> ToolRisk {
    match tool_name {
        "read_file" | "list_directory" | "search_files" | "glob" | "read_website"
        | "write_todos" | "skill" | "task" | "command_output" => ToolRisk::ReadOnly,
        _ => ToolRisk::Mutating,
    }
}

/// Per-session permission state.
#[derive(Default)]
struct SessionPerms {
    /// Tools the user chose "always allow this session" for.
    always_allow: HashSet<String>,
    /// When set, every mutating tool is denied with a plan-mode message.
    plan_mode: bool,
}

fn sessions() -> &'static Mutex<HashMap<String, SessionPerms>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, SessionPerms>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_session<T>(session: &str, f: impl FnOnce(&mut SessionPerms) -> T) -> T {
    let mut map = sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(map.entry(session.to_string()).or_default())
}

/// The message returned to the model when a mutating tool is blocked by plan
/// mode. Instructive rather than terse so the model changes course in one turn.
fn plan_mode_denial(tool_name: &str) -> String {
    format!(
        "Plan mode is active: `{tool_name}` was not executed because it modifies state. \
         Present a concise plan of the changes you intend to make and ask the user to \
         approve it (they can run /plan off to enable execution). Read-only tools \
         (read_file, search_files, glob, list_directory) remain available for research."
    )
}

/// The message returned to the model when the user (or policy) denies a tool.
pub fn user_denial(tool_name: &str) -> String {
    format!(
        "The user denied permission to run `{tool_name}`. Do not retry the same call. \
         Explain what you wanted to do and ask the user how to proceed, or continue \
         with an approach that does not need this tool."
    )
}

/// Render a tool call's arguments for an approval prompt. The person deciding
/// must be able to see what they are approving, so the cap is generous and any
/// cut is marked with how much is hidden — silently truncating could hide the
/// tail of a command from the user who is about to allow it.
pub fn approval_preview(arguments: &str) -> String {
    const MAX_CHARS: usize = 500;
    let total = arguments.chars().count();
    if total <= MAX_CHARS {
        return arguments.to_string();
    }
    let shown: String = arguments.chars().take(MAX_CHARS).collect();
    format!("{shown}… [+{} more chars]", total - MAX_CHARS)
}

/// Policy check for one tool call. See the module docs for the layering.
pub fn decision_for(session: &str, tool_name: &str) -> Decision {
    if classify(tool_name) == ToolRisk::ReadOnly {
        return Decision::Allow;
    }

    let (plan_mode, granted) = with_session(session, |s| {
        (s.plan_mode, s.always_allow.contains(tool_name))
    });

    if plan_mode {
        return Decision::Deny(plan_mode_denial(tool_name));
    }
    if granted {
        return Decision::Allow;
    }

    match settings::permission_mode_for(tool_name) {
        PermissionMode::Allow => Decision::Allow,
        PermissionMode::Ask => Decision::Ask,
        PermissionMode::Deny => Decision::Deny(format!(
            "`{tool_name}` is denied by the permission policy in settings.toml. \
             Do not retry it; work without this tool or ask the user to change \
             the policy."
        )),
    }
}

/// Record an "always allow this session" grant for a tool.
pub fn grant_for_session(session: &str, tool_name: &str) {
    with_session(session, |s| {
        s.always_allow.insert(tool_name.to_string());
    });
}

/// Toggle plan mode for a session. Returns the new state.
pub fn set_plan_mode(session: &str, enabled: bool) -> bool {
    with_session(session, |s| {
        s.plan_mode = enabled;
        s.plan_mode
    })
}

/// Whether plan mode is active for a session.
pub fn plan_mode(session: &str) -> bool {
    with_session(session, |s| s.plan_mode)
}

/// Drop all recorded state for a session (fresh session, /clear, or session
/// teardown) so grants never outlive the conversation they were given in.
pub fn reset_session(session: &str) {
    let mut map = sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.remove(session);
}

/// Drop the recorded state for *every* session. Called at ACP session
/// boundaries (new/load/fork): the agent drives one shared engine, so only one
/// conversation is live at a time and grants must never cross a boundary. This
/// also keeps the map from accumulating entries for session ids that will
/// never be used again.
pub fn reset_all() {
    let mut map = sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.clear();
}

/// One-line status summary for `/permissions` and `/status`.
pub fn describe(session: &str) -> String {
    let plan = if plan_mode(session) { "on" } else { "off" };
    let default = settings::permission_default();
    let granted = with_session(session, |s| {
        let mut names: Vec<&str> = s.always_allow.iter().map(String::as_str).collect();
        names.sort_unstable();
        names.join(", ")
    });
    let granted = if granted.is_empty() {
        "none".to_string()
    } else {
        granted
    };
    format!(
        "permissions: default={default} | plan mode: {plan} | session grants: {granted}\n\
         read-only tools always run; configure [permissions] in settings.toml"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `decision_for` reads settings (env + file), and the settings test
    /// mutates `SIGIT_CONFIG_DIR`/`SIGIT_PERMISSIONS` under this lock — hold it
    /// here too so parallel test runs don't race, and point the config dir at
    /// an empty sandbox so a developer's real settings.toml can't skew results.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("sigit_perm_tests_{}", std::process::id()));
        // SAFETY: process-global env mutation, serialized by ENV_TEST_LOCK; the
        // other env-touching tests re-set these before reading.
        unsafe { std::env::set_var("SIGIT_CONFIG_DIR", &dir) };
        unsafe { std::env::remove_var("SIGIT_PERMISSIONS") };
        guard
    }

    #[test]
    fn read_only_tools_always_allowed() {
        let _guard = env_guard();
        for tool in [
            "read_file",
            "list_directory",
            "search_files",
            "glob",
            "read_website",
            "write_todos",
            "skill",
            "task",
            "command_output",
        ] {
            assert_eq!(classify(tool), ToolRisk::ReadOnly, "{tool}");
            assert_eq!(decision_for("t-ro", tool), Decision::Allow, "{tool}");
        }
    }

    #[test]
    fn mutating_and_unknown_tools_are_gated() {
        for tool in [
            "edit_file",
            "multi_edit",
            "create_file",
            "create_directory",
            "delete_file",
            "run_command",
            "kill_command",
            "remember",
            "mcp__server__anything",
            "totally_unknown_tool",
        ] {
            assert_eq!(classify(tool), ToolRisk::Mutating, "{tool}");
        }
    }

    #[test]
    fn plan_mode_denies_mutating_and_spares_read_only() {
        let _guard = env_guard();
        let session = "t-plan";
        reset_session(session);
        set_plan_mode(session, true);
        assert!(matches!(
            decision_for(session, "run_command"),
            Decision::Deny(_)
        ));
        assert_eq!(decision_for(session, "read_file"), Decision::Allow);
        set_plan_mode(session, false);
        reset_session(session);
    }

    #[test]
    fn session_grant_short_circuits_ask() {
        let _guard = env_guard();
        let session = "t-grant";
        reset_session(session);
        grant_for_session(session, "edit_file");
        assert_eq!(decision_for(session, "edit_file"), Decision::Allow);
        // Other tools are unaffected by the grant.
        assert_ne!(decision_for(session, "delete_file"), Decision::Allow);
        reset_session(session);
        assert_ne!(decision_for(session, "edit_file"), Decision::Allow);
    }

    #[test]
    fn approval_preview_shows_short_arguments_in_full() {
        let args = r#"{"command":"cargo test"}"#;
        assert_eq!(approval_preview(args), args);
    }

    #[test]
    fn approval_preview_marks_truncation_explicitly() {
        let args = format!(r#"{{"command":"echo {}; rm -rf /"}}"#, "x".repeat(600));
        let preview = approval_preview(&args);
        assert!(preview.chars().count() < args.chars().count());
        assert!(
            preview.contains("more chars]"),
            "hidden content must be flagged, got: {preview}"
        );
    }

    #[test]
    fn plan_mode_outranks_session_grant() {
        let _guard = env_guard();
        let session = "t-rank";
        reset_session(session);
        grant_for_session(session, "edit_file");
        set_plan_mode(session, true);
        assert!(matches!(
            decision_for(session, "edit_file"),
            Decision::Deny(_)
        ));
        reset_session(session);
    }
}
