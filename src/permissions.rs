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
//!    picks that option in an approval prompt. For `run_command` the grant is
//!    scoped to the command's first two whitespace-separated tokens (approving
//!    `git push origin main` records `run_command(git push)`); other tools
//!    record the bare tool name.
//! 3. **Rule lists** — `[permissions.rules]` in `settings.toml`: ordered
//!    `deny` and `allow` lists of rules shaped `tool_name` or
//!    `tool_name(argument_pattern)`, e.g. `run_command(git status)`,
//!    `run_command(cargo *)`, `edit_file(src/*)`. The pattern matches the
//!    command string for `run_command` and the path argument for the
//!    file-mutating tools; for tools with no obvious argument (MCP tools,
//!    unknown tools) only a bare `tool_name` rule matches. `deny` is checked
//!    before `allow`, so a deny always beats an allow matching the same call.
//! 4. **Per-tool override** — `[permissions.tools]` in `settings.toml`, e.g.
//!    `run_command = "ask"`, `edit_file = "allow"`, `delete_file = "deny"`.
//! 5. **Default mode** — `[permissions] default = "ask"|"allow"|"deny"` in
//!    `settings.toml`; `ask` on a fresh install.
//!
//! Pattern matching (rules and session grants share it): `*` is a glob-style
//! wildcard (the `glob` tool's translator). A pattern ending in `*` matches
//! everything from the wildcard on — `run_command(cargo *)` covers
//! `cargo build src/main.rs`. A pattern without a trailing `*` must be the
//! whole argument or end at a whitespace boundary: `run_command(git status)`
//! matches `git status` and `git status --short` but not `git status-x`.
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

use regex::Regex;

use crate::settings::{self, PermissionMode};
use crate::tools::glob_to_regex;

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

// ── Rule patterns ────────────────────────────────────────────────────────────
// A rule is `tool_name` or `tool_name(argument_pattern)`; rules from
// `[permissions.rules]` and session grants share this matcher.

/// Split a rule into tool name and optional argument pattern:
/// `run_command(git *)` → `("run_command", Some("git *"))`;
/// `edit_file` → `("edit_file", None)`.
fn parse_rule(rule: &str) -> (&str, Option<&str>) {
    let rule = rule.trim();
    if let Some(open) = rule.find('(')
        && let Some(inner) = rule[open + 1..].strip_suffix(')')
    {
        return (rule[..open].trim(), Some(inner));
    }
    (rule, None)
}

/// Compile a rule's argument pattern. Reuses the `glob` tool's translator
/// (`*`, `**`, `?`, `{a,b}`), then swaps its end anchor for rule semantics:
/// a pattern ending in `*` matches everything from the wildcard on (prefix
/// semantics — `cargo *` covers `cargo build src/main.rs`), while any other
/// pattern must be the whole argument or end at a whitespace boundary
/// (`git status` matches `git status --short` but not `git status-x`).
fn pattern_regex(pattern: &str) -> Option<Regex> {
    let anchored = glob_to_regex(pattern);
    let body = anchored.strip_suffix('$').unwrap_or(&anchored);
    let source = if pattern.ends_with('*') {
        body.to_string()
    } else {
        format!("{body}(?:$|\\s)")
    };
    Regex::new(&source).ok()
}

/// Whether one rule covers one tool call. A bare `tool_name` rule matches any
/// call of that tool; a pattern rule additionally needs the call's matchable
/// argument to fit the pattern — so a pattern rule never matches a tool that
/// has no matchable argument (an unreadable rule must not widen access).
fn rule_matches(rule: &str, tool_name: &str, argument: Option<&str>) -> bool {
    let (rule_tool, pattern) = parse_rule(rule);
    if rule_tool != tool_name {
        return false;
    }
    match (pattern, argument) {
        (None, _) => true,
        (Some(pattern), Some(argument)) => {
            pattern_regex(pattern).is_some_and(|re| re.is_match(argument))
        }
        (Some(_), None) => false,
    }
}

/// The argument a rule pattern is matched against, extracted from the tool
/// call's raw JSON arguments: the command string for `run_command`, the path
/// for the file-mutating tools. Read-only tools never reach the matcher, and
/// other tools (MCP, unknown) have no obvious single argument, so they return
/// `None` and are governed only by bare `tool_name` rules.
fn matchable_argument(tool_name: &str, arguments: &str) -> Option<String> {
    let key = match tool_name {
        "run_command" => "command",
        "edit_file" | "create_file" | "multi_edit" | "delete_file" | "create_directory" => "path",
        _ => return None,
    };
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    Some(value.get(key)?.as_str()?.to_string())
}

/// Policy check for one tool call. `arguments` is the call's raw JSON argument
/// string, consulted by rule patterns and granular session grants. See the
/// module docs for the layering.
pub fn decision_for(session: &str, tool_name: &str, arguments: &str) -> Decision {
    if classify(tool_name) == ToolRisk::ReadOnly {
        return Decision::Allow;
    }

    let argument = matchable_argument(tool_name, arguments);
    let (plan_mode, granted) = with_session(session, |s| {
        (
            s.plan_mode,
            s.always_allow
                .iter()
                .any(|grant| rule_matches(grant, tool_name, argument.as_deref())),
        )
    });

    if plan_mode {
        return Decision::Deny(plan_mode_denial(tool_name));
    }
    if granted {
        return Decision::Allow;
    }

    let rules = settings::permission_rules();
    if let Some(rule) = rules
        .deny
        .iter()
        .find(|rule| rule_matches(rule, tool_name, argument.as_deref()))
    {
        return Decision::Deny(format!(
            "`{tool_name}` is denied by the permission rule `{rule}` in settings.toml. \
             Do not retry it; work without this tool or ask the user to change \
             the policy."
        ));
    }
    if rules
        .allow
        .iter()
        .any(|rule| rule_matches(rule, tool_name, argument.as_deref()))
    {
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

/// Record an "always allow this session" grant for a tool call. For
/// `run_command` the grant is scoped to the command's first two
/// whitespace-separated tokens (approving `git push origin main` records
/// `run_command(git push)`, covering the `git push …` family only); other
/// tools record the bare tool name, matching any call.
pub fn grant_for_session(session: &str, tool_name: &str, arguments: &str) {
    let grant = session_grant_rule(tool_name, arguments);
    with_session(session, |s| {
        s.always_allow.insert(grant);
    });
}

/// The rule string recorded for one approved call (see [`grant_for_session`]).
/// Falls back to the bare tool name when the command is absent or empty, which
/// grants the whole tool — exactly what the pre-granular behavior was.
fn session_grant_rule(tool_name: &str, arguments: &str) -> String {
    if tool_name == "run_command"
        && let Some(command) = matchable_argument(tool_name, arguments)
    {
        let prefix: Vec<&str> = command.split_whitespace().take(2).collect();
        if !prefix.is_empty() {
            return format!("{tool_name}({})", prefix.join(" "));
        }
    }
    tool_name.to_string()
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

/// Status summary for `/permissions` and `/status`: the default mode, plan
/// mode, the granular session grants, and the active rule lists.
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
    let rules = settings::permission_rules();
    let render = |list: &[String]| {
        if list.is_empty() {
            "none".to_string()
        } else {
            list.join(", ")
        }
    };
    format!(
        "permissions: default={default} | plan mode: {plan} | session grants: {granted}\n\
         rules: deny: {} | allow: {}\n\
         read-only tools always run; configure [permissions] in settings.toml",
        render(&rules.deny),
        render(&rules.allow),
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
        // Start from an empty sandbox: a settings.toml written by an earlier
        // test in this process (e.g. one storing rule lists) must not leak
        // into the next.
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: process-global env mutation, serialized by ENV_TEST_LOCK; the
        // other env-touching tests re-set these before reading.
        unsafe { std::env::set_var("SIGIT_CONFIG_DIR", &dir) };
        unsafe { std::env::remove_var("SIGIT_PERMISSIONS") };
        guard
    }

    /// Persist rule lists into the sandboxed settings.toml.
    fn store_rules(allow: &[&str], deny: &[&str]) {
        let mut settings = settings::load();
        settings.permissions.rules.allow = allow.iter().map(|s| s.to_string()).collect();
        settings.permissions.rules.deny = deny.iter().map(|s| s.to_string()).collect();
        settings::store(&settings).unwrap();
    }

    /// `decision_for` on a `run_command` call with the given command string.
    fn run_command_decision(session: &str, command: &str) -> Decision {
        let args = serde_json::json!({ "command": command }).to_string();
        decision_for(session, "run_command", &args)
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
            assert_eq!(decision_for("t-ro", tool, "{}"), Decision::Allow, "{tool}");
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
            run_command_decision(session, "ls"),
            Decision::Deny(_)
        ));
        assert_eq!(decision_for(session, "read_file", "{}"), Decision::Allow);
        set_plan_mode(session, false);
        reset_session(session);
    }

    #[test]
    fn session_grant_short_circuits_ask() {
        let _guard = env_guard();
        let session = "t-grant";
        reset_session(session);
        let args = r#"{"path":"src/a.rs","old_text":"a","new_text":"b"}"#;
        grant_for_session(session, "edit_file", args);
        assert_eq!(decision_for(session, "edit_file", args), Decision::Allow);
        // Non-run_command grants record the bare tool name: any path is covered.
        assert_eq!(
            decision_for(session, "edit_file", r#"{"path":"docs/other.md"}"#),
            Decision::Allow
        );
        // Other tools are unaffected by the grant.
        assert_ne!(
            decision_for(session, "delete_file", r#"{"path":"src/a.rs"}"#),
            Decision::Allow
        );
        reset_session(session);
        assert_ne!(decision_for(session, "edit_file", args), Decision::Allow);
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
        let args = r#"{"path":"src/a.rs"}"#;
        grant_for_session(session, "edit_file", args);
        set_plan_mode(session, true);
        assert!(matches!(
            decision_for(session, "edit_file", args),
            Decision::Deny(_)
        ));
        reset_session(session);
    }

    #[test]
    fn rules_gate_run_command_by_argument() {
        let _guard = env_guard();
        let session = "t-rules";
        reset_session(session);
        store_rules(&["run_command(git *)"], &["run_command(git push*)"]);

        assert_eq!(run_command_decision(session, "git status"), Decision::Allow);
        assert!(matches!(
            run_command_decision(session, "git push"),
            Decision::Deny(_)
        ));
        assert!(matches!(
            run_command_decision(session, "git push --force"),
            Decision::Deny(_)
        ));
        assert_eq!(
            run_command_decision(session, "cargo test"),
            Decision::Ask,
            "an unmatched command falls through to the default mode"
        );
        reset_session(session);
    }

    #[test]
    fn deny_rule_beats_matching_allow_rule() {
        let _guard = env_guard();
        let session = "t-deny-wins";
        reset_session(session);
        store_rules(&["run_command(git *)"], &["run_command(git *)"]);
        match run_command_decision(session, "git status") {
            Decision::Deny(reason) => assert!(
                reason.contains("run_command(git *)"),
                "the denial must name the rule, got: {reason}"
            ),
            other => panic!("expected a deny, got {other:?}"),
        }
        reset_session(session);
    }

    #[test]
    fn rule_pattern_wildcard_and_prefix_edges() {
        // Whole-token prefix: a pattern without a trailing `*` matches at a
        // whitespace boundary or the end, never mid-token.
        let rule = "run_command(git status)";
        assert!(rule_matches(rule, "run_command", Some("git status")));
        assert!(rule_matches(
            rule,
            "run_command",
            Some("git status --short")
        ));
        assert!(!rule_matches(rule, "run_command", Some("git status-x")));
        assert!(!rule_matches(rule, "run_command", Some("git statu")));
        assert!(!rule_matches(rule, "run_command", Some("xgit status")));

        // Trailing `*`: everything from the wildcard on matches.
        let rule = "run_command(git push*)";
        assert!(rule_matches(rule, "run_command", Some("git push")));
        assert!(rule_matches(rule, "run_command", Some("git pushx")));
        assert!(rule_matches(rule, "run_command", Some("git push --force")));
        assert!(!rule_matches(rule, "run_command", Some("git pus")));
        let rule = "run_command(cargo *)";
        assert!(rule_matches(
            rule,
            "run_command",
            Some("cargo test --locked")
        ));
        assert!(
            rule_matches(rule, "run_command", Some("cargo build --bin src/x")),
            "a trailing `*` also covers arguments containing `/`"
        );
        assert!(!rule_matches(rule, "run_command", Some("cargo")));

        // A rule only applies to its own tool.
        assert!(!rule_matches(rule, "delete_file", Some("cargo test")));
        // Bare tool rules match any call, including argument-less tools.
        assert!(rule_matches("run_command", "run_command", Some("anything")));
        assert!(rule_matches("mcp__srv__tool", "mcp__srv__tool", None));
        // A pattern rule never matches a tool without a matchable argument.
        assert!(!rule_matches("mcp__srv__tool(x)", "mcp__srv__tool", None));
    }

    #[test]
    fn run_command_session_grant_is_scoped_to_command_prefix() {
        let _guard = env_guard();
        let session = "t-grant-scope";
        reset_session(session);
        grant_for_session(
            session,
            "run_command",
            r#"{"command":"git push origin main"}"#,
        );
        // The grant is `run_command(git push)`: the `git push …` family only.
        assert_eq!(run_command_decision(session, "git push"), Decision::Allow);
        assert_eq!(
            run_command_decision(session, "git push --force-with-lease"),
            Decision::Allow
        );
        assert_eq!(run_command_decision(session, "git pull"), Decision::Ask);
        assert_eq!(run_command_decision(session, "git pushx"), Decision::Ask);
        assert_eq!(run_command_decision(session, "rm -rf /"), Decision::Ask);

        // A single-token command grants that token's family.
        grant_for_session(session, "run_command", r#"{"command":"ls"}"#);
        assert_eq!(run_command_decision(session, "ls -la"), Decision::Allow);
        assert_eq!(run_command_decision(session, "lsof"), Decision::Ask);
        reset_session(session);
    }

    #[test]
    fn file_tool_rules_match_on_path() {
        let _guard = env_guard();
        let session = "t-file-rules";
        reset_session(session);
        store_rules(&["edit_file(src/*)"], &["delete_file(src/*)"]);
        assert_eq!(
            decision_for(
                session,
                "edit_file",
                r#"{"path":"src/main.rs","old_text":"a","new_text":"b"}"#
            ),
            Decision::Allow
        );
        assert_eq!(
            decision_for(session, "edit_file", r#"{"path":"docs/readme.md"}"#),
            Decision::Ask,
            "a path outside the rule falls through to the default mode"
        );
        assert!(matches!(
            decision_for(session, "delete_file", r#"{"path":"src/main.rs"}"#),
            Decision::Deny(_)
        ));
        assert_eq!(
            decision_for(session, "delete_file", r#"{"path":"docs/readme.md"}"#),
            Decision::Ask
        );
        reset_session(session);
    }
}
