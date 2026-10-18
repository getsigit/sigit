//! Full-screen terminal chat UI.
//!
//! An ACP client of siGit Code itself (see `acp_client`): the agent runs in a
//! subprocess, and this renders its `session/update`s. It runs on every
//! platform. Uses `tokio::select!` to multiplex terminal events with the
//! agent's updates.

// ── Think-block stripping ─────────────────────────────────────────────────────

/// Split out `<think>…</think>` blocks from a model response.
///
/// Qwen 3 emits reasoning inside `<think>` tags before the actual answer.
/// Returns `(thinking_text, visible_reply)`. Either may be empty.
pub(crate) fn strip_think_blocks(raw: &str) -> (String, String) {
    let mut thinking = String::new();
    let mut remainder = raw;

    while let Some(start) = remainder.find("<think>") {
        let before = &remainder[..start];
        if let Some(end) = remainder[start..].find("</think>") {
            let block = &remainder[start + 7..start + end];
            thinking.push_str(block.trim());
            remainder = &remainder[start + end + 8..];
            if !before.trim().is_empty() {
                // rare: text before <think> — keep it visible
                let mut combined = before.to_string();
                combined.push_str(remainder);
                return (thinking, combined.trim().to_string());
            }
        } else {
            // unclosed tag — model probably ran out of tokens
            thinking.push_str(remainder[start + 7..].trim());
            remainder = before;
            break;
        }
    }

    (thinking, remainder.trim().to_string())
}

/// The live reasoning tail to show while a streaming buffer holds only
/// `<think>` content (no visible reply yet).
///
/// Returns the last `max_lines` lines of the in-progress thinking text after
/// word-wrapping at `width` columns, so the TUI can render them under the
/// "thinking…" label and the user watches the model reason. Returns an empty
/// vec once visible text has arrived (the reply streams instead) or when
/// there is no thinking text yet (plain spinner).
pub(crate) fn streaming_think_tail(raw: &str, width: usize, max_lines: usize) -> Vec<String> {
    let (thinking, visible) = strip_think_blocks(raw);
    if thinking.is_empty() || !visible.trim().is_empty() || max_lines == 0 {
        return Vec::new();
    }

    let width = width.max(1);
    let mut wrapped: Vec<String> = Vec::new();
    for line in thinking.lines() {
        let mut current = String::new();
        for word in line.split_whitespace() {
            // hard-split words wider than the wrap column
            let mut word = word;
            while word.chars().count() > width {
                if !current.is_empty() {
                    wrapped.push(std::mem::take(&mut current));
                }
                let head: String = word.chars().take(width).collect();
                word = &word[head.len()..];
                wrapped.push(head);
            }
            if current.is_empty() {
                current.push_str(word);
            } else if current.chars().count() + 1 + word.chars().count() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                wrapped.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            wrapped.push(current);
        }
    }

    let skip = wrapped.len().saturating_sub(max_lines);
    wrapped.split_off(skip)
}

/// One-line dim indicator shown above a reply whose reasoning is collapsed,
/// e.g. `· thought for a bit (12 lines) — /thinking to show`.
pub(crate) fn think_indicator(think: &str) -> String {
    let count = think.lines().count().max(1);
    let noun = if count == 1 { "line" } else { "lines" };
    format!("· thought for a bit ({count} {noun}) — /thinking to show")
}

pub(crate) fn parse_rich_text_segments(text: &str) -> Vec<(String, bool)> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    let mut bold = false;

    while let Some(ch) = chars.next() {
        if ch == '*' && chars.peek() == Some(&'*') {
            chars.next();
            if !current.is_empty() {
                segments.push((std::mem::take(&mut current), bold));
            }
            bold = !bold;
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        segments.push((current, bold));
    }

    segments
}

// ── Tool-call display helpers ─────────────────────────────────────────────────
//
// Pure helpers for the TUI's collapsible tool-call entries. Top-level (not in
// `mod tui`) so they stay testable without a terminal.

/// Max characters for the summary half of a tool-call title line.
const TOOL_TITLE_SUMMARY_MAX: usize = 60;

/// Max characters of tool output kept for the expanded-entry result preview.
const TOOL_RESULT_PREVIEW_MAX: usize = 2000;

/// Truncate to `max_chars` characters, ending with an ellipsis when cut.
fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One-line title for a tool call, Claude Code style: the tool name plus the
/// argument that best identifies the call (`run_command · cargo test`,
/// `edit_file · src/main.rs`, `sigit · list_issues` for MCP tools). Falls back
/// to the bare tool name when the arguments are malformed or carry nothing
/// summarizable.
pub(crate) fn tool_title(name: &str, arguments_json: &str) -> String {
    let args: Option<serde_json::Value> = serde_json::from_str(arguments_json).ok();
    let str_arg =
        |key: &str| -> Option<String> { Some(args.as_ref()?.get(key)?.as_str()?.to_string()) };

    let (label, summary) = if let Some(rest) = name.strip_prefix("mcp__") {
        // mcp__<server>__<tool> → "<server> · <tool>"
        match rest.split_once("__") {
            Some((server, tool)) => (server.to_string(), Some(tool.to_string())),
            None => (rest.to_string(), None),
        }
    } else {
        let summary = match name {
            "run_command" => str_arg("command"),
            "read_file" | "create_file" | "edit_file" | "multi_edit" | "delete_file" => {
                str_arg("path")
            }
            "search_files" | "glob" => str_arg("pattern"),
            _ => None,
        };
        (name.to_string(), summary)
    };

    // A title is a single line: collapse any internal whitespace runs
    // (multi-line commands) before truncating.
    let summary = summary
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
        .map(|s| truncate_with_ellipsis(&s, TOOL_TITLE_SUMMARY_MAX));

    match summary {
        Some(summary) => format!("{label} · {summary}"),
        None => label,
    }
}

/// Pretty-print a tool call's JSON arguments for the expanded view; malformed
/// JSON is logged and shown raw.
pub(crate) fn pretty_tool_arguments(arguments_json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments_json) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|e| {
            log::warn!("failed to pretty-print tool arguments: {e}");
            arguments_json.to_string()
        }),
        Err(e) => {
            log::warn!("malformed JSON in tool arguments: {e}");
            arguments_json.to_string()
        }
    }
}

/// The tail of a tool's output for display under an expanded tool entry,
/// capped at [`TOOL_RESULT_PREVIEW_MAX`] chars with a truncation note. Display
/// only — the model always receives the full output.
pub(crate) fn cap_output_preview(output: &str) -> String {
    let total = output.chars().count();
    if total <= TOOL_RESULT_PREVIEW_MAX {
        return output.to_string();
    }
    let tail: String = output
        .chars()
        .skip(total - TOOL_RESULT_PREVIEW_MAX)
        .collect();
    format!("… (truncated — showing the last {TOOL_RESULT_PREVIEW_MAX} of {total} chars)\n{tail}")
}

/// The display lines of one tool-call entry. Collapsed: a single `▸ 🔧 title`
/// line. Expanded: `▾ 🔧 title`, the indented argument detail, and the result
/// preview under a `result:` label when present.
pub(crate) fn tool_entry_lines(
    title: &str,
    detail: &str,
    result_preview: Option<&str>,
    expanded: bool,
) -> Vec<String> {
    if !expanded {
        return vec![format!("▸ 🔧 {title}")];
    }
    let mut out = vec![format!("▾ 🔧 {title}")];
    if !detail.is_empty() {
        for line in detail.split('\n') {
            out.push(format!("    {line}"));
        }
    }
    if let Some(result) = result_preview {
        out.push("    result:".to_string());
        for line in result.split('\n') {
            out.push(format!("      {line}"));
        }
    }
    out
}

// ── Tabs ──────────────────────────────────────────────────────────────────────
//
// The top-level tab bar (GitHub Copilot CLI-style). Defined outside `mod tui`
// so the pure cycling logic is testable without a terminal.

/// The top-level TUI tabs, cycled with the Tab key. `Repo` only exists when
/// the session cwd's `origin` remote points at the sigit.si host (see
/// [`parse_repo_remote`]); every method takes `repo_visible` so the hidden tab
/// is skipped entirely and the bar renders exactly three tabs without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    /// The chat itself (default).
    Session,
    /// Saved sessions from the session store.
    History,
    /// Issues and pull requests of the sigit.si-hosted repo (when detected).
    Repo,
    /// siGit Code Cloud status and settings.
    Cloud,
}

impl Tab {
    /// Tab-bar titles, in cycle order.
    pub(crate) fn titles(repo_visible: bool) -> Vec<&'static str> {
        if repo_visible {
            vec!["Session", "History", "Repo", "Cloud"]
        } else {
            vec!["Session", "History", "Cloud"]
        }
    }

    /// Session → History → (Repo →) Cloud → Session.
    pub(crate) fn next(self, repo_visible: bool) -> Self {
        match self {
            Tab::Session => Tab::History,
            Tab::History => {
                if repo_visible {
                    Tab::Repo
                } else {
                    Tab::Cloud
                }
            }
            Tab::Repo => Tab::Cloud,
            Tab::Cloud => Tab::Session,
        }
    }

    /// Position in [`Tab::titles`], for the ratatui `Tabs` widget.
    pub(crate) fn index(self, repo_visible: bool) -> usize {
        match self {
            Tab::Session => 0,
            Tab::History => 1,
            Tab::Repo => 2,
            Tab::Cloud => {
                if repo_visible {
                    3
                } else {
                    2
                }
            }
        }
    }
}

// ── Repo tab: remote detection and data parsing ───────────────────────────────
//
// Pure helpers for the Repo tab (issues / pull requests of a sigit.si-hosted
// repo). Kept at the top level so they test without a terminal.

/// Host of the sigit.si instance this build talks to: the host of
/// `SIGIT_API_URL` when set (same variable `account.rs` uses for the account
/// API), else the production default.
pub(crate) fn sigit_host() -> String {
    host_from_api_url(std::env::var("SIGIT_API_URL").ok().as_deref())
}

/// Pure core of [`sigit_host`]: derive the host from an optional
/// `SIGIT_API_URL` value, defaulting to `sigit.si`.
pub(crate) fn host_from_api_url(api_url: Option<&str>) -> String {
    api_url
        .and_then(url_host)
        .unwrap_or_else(|| "sigit.si".to_string())
}

/// Extract the hostname (no scheme, userinfo, port, or path) from an http(s)
/// URL. `None` for anything else.
pub(crate) fn url_host(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Extract `owner/name` from a git remote URL *iff* it points at `host`.
///
/// Understands the two forms sigit.si issues:
/// - ssh (scp-like): `git@sigit.si:owner/name.git` (and without `.git`)
/// - https: `https://sigit.si/owner/name.git` (and without `.git`)
///
/// A different host, extra path segments, or anything unparsable → `None`,
/// which hides the Repo tab.
pub(crate) fn parse_repo_remote(remote: &str, host: &str) -> Option<String> {
    let remote = remote.trim();

    // ssh, scp-like: git@<host>:<owner>/<name>(.git)
    if let Some(rest) = remote.strip_prefix("git@") {
        let (remote_host, path) = rest.split_once(':')?;
        if !remote_host.eq_ignore_ascii_case(host) {
            return None;
        }
        return repo_owner_name(path);
    }

    // http(s)://<host>(:port)/<owner>/<name>(.git)
    let rest = remote
        .strip_prefix("https://")
        .or_else(|| remote.strip_prefix("http://"))?;
    let (authority, path) = rest.split_once('/')?;
    let remote_host = authority.rsplit('@').next()?.split(':').next()?;
    if !remote_host.eq_ignore_ascii_case(host) {
        return None;
    }
    repo_owner_name(path)
}

/// Validate and normalize a remote path into `owner/name` (strips a trailing
/// `.git` / `/`). Exactly two non-empty segments, or `None`.
fn repo_owner_name(path: &str) -> Option<String> {
    let path = path.trim().trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    if owner.is_empty() || name.is_empty() || segments.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// One row of the Repo tab's Issues / Pull requests lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoItem {
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) state: String,
}

/// Parse the JSON text block a `list_issues` / `list_pull_requests` MCP tool
/// returns into rows. Accepts a top-level array or an object wrapping one
/// under `issues` / `pull_requests` / `items`; entries missing `number` or
/// `title` are skipped. Unparsable input (e.g. an `Error: …` string from
/// `mcp::call_tool`) comes back as `Err` with the original text so the tab can
/// show it verbatim.
pub(crate) fn parse_repo_items(text: &str) -> Result<Vec<RepoItem>, String> {
    let trimmed = text.trim();
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|_| trimmed.to_string())?;
    let entries = value
        .as_array()
        .cloned()
        .or_else(|| {
            value.as_object().and_then(|object| {
                ["issues", "pull_requests", "items"]
                    .iter()
                    .find_map(|key| object.get(*key).and_then(|v| v.as_array()).cloned())
            })
        })
        .ok_or_else(|| trimmed.to_string())?;

    Ok(entries
        .iter()
        .filter_map(|entry| {
            Some(RepoItem {
                number: entry.get("number")?.as_u64()?,
                title: entry.get("title")?.as_str()?.to_string(),
                state: entry
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect())
}

/// One list row: number, state, title.
pub(crate) fn repo_row(item: &RepoItem) -> String {
    if item.state.is_empty() {
        format!("#{}  {}", item.number, item.title)
    } else {
        format!("#{}  [{}]  {}", item.number, item.state, item.title)
    }
}

/// Render a `get_issue` / `get_pull_request` result for the detail view:
/// pretty-print when it's JSON, pass anything else (e.g. error text) through.
pub(crate) fn format_repo_detail(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text.trim()) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_string()),
        Err(_) => text.to_string(),
    }
}

// ── The terminal UI ───────────────────────────────────────────────────────────
//
// An ACP client of siGit Code itself: `acp_client` starts this binary as the
// agent, and everything the user sees is a rendering of its `session/update`s.
// The agent loop lives in `main.rs` only. Runs on every platform.

mod tui {
    use std::future::pending;
    use std::path::Path;

    use agent_client_protocol::schema::MaybeUndefined;
    use agent_client_protocol::schema::v1::{
        AvailableCommand, ContentBlock, ContentChunk, Diff, PermissionOption, PermissionOptionKind,
        Plan, PlanEntryStatus, RequestPermissionOutcome, RequestPermissionRequest,
        SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
        SessionConfigSelectOption, SessionConfigSelectOptions, SessionId, SessionInfo,
        SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    };
    use anyhow::Result;
    use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use futures::StreamExt;
    use ratatui::{
        Frame,
        layout::{Constraint, Layout, Position},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap},
    };
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::{Duration, Instant};

    use super::{RepoItem, Tab};
    use crate::acp_client::{self, AgentConn, AgentError, AgentEvent, ConfigValue, Started};

    // ── Message types ─────────────────────────────────────────────────────────

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Role {
        User,
        Assistant,
        System,
        /// rainbow-colored banner art
        Banner,
        /// a collapsible tool-call entry (`text` holds the title line)
        Tool,
        /// the agent's plan, replaced in place as it changes
        Plan,
    }

    #[derive(Debug)]
    struct ChatMessage {
        role: Role,
        text: String,
        /// Qwen 3 reasoning extracted from `<think>` tags, if any.
        think_block: Option<String>,
        /// pretty-printed tool arguments (Role::Tool only)
        tool_detail: Option<String>,
        /// tail of the tool output, filled in as the call progresses
        /// (Role::Tool only)
        tool_result: Option<String>,
        /// the ACP id of the call, so its updates find it (Role::Tool only)
        tool_call_id: Option<String>,
        /// where the call stands (Role::Tool only)
        status: Option<ToolCallStatus>,
    }

    impl ChatMessage {
        fn plain(role: Role, text: String) -> Self {
            Self {
                role,
                text,
                think_block: None,
                tool_detail: None,
                tool_result: None,
                tool_call_id: None,
                status: None,
            }
        }

        fn user(text: impl Into<String>) -> Self {
            Self::plain(Role::User, text.into())
        }

        fn assistant(text: impl Into<String>) -> Self {
            let raw = text.into();
            let (think, visible) = super::strip_think_blocks(&raw);
            let mut message = Self::plain(Role::Assistant, visible);
            message.think_block = if think.is_empty() { None } else { Some(think) };
            message
        }

        fn system(text: impl Into<String>) -> Self {
            Self::plain(Role::System, text.into())
        }

        fn banner(text: impl Into<String>) -> Self {
            Self::plain(Role::Banner, text.into())
        }

        fn plan(text: impl Into<String>) -> Self {
            Self::plain(Role::Plan, text.into())
        }

        fn tool_call(title: impl Into<String>, detail: impl Into<String>) -> Self {
            let mut message = Self::plain(Role::Tool, title.into());
            message.tool_detail = Some(detail.into());
            message
        }
    }

    // ── The transcript ────────────────────────────────────────────────────────
    //
    // Everything a `session/update` can change, apart from the connection
    // itself, so the mapping can be tested without an agent.

    /// Id prefix of the cards the agent shows while an on-device model downloads
    /// or loads (`model_load_progress` in `main.rs`).
    const MODEL_LOAD_CARD_PREFIX: &str = "model-load-";

    /// Id of the picker's group of cloud tiers (`sigit-cloud:<tier>`).
    const CLOUD_ID_PREFIX: &str = "sigit-cloud:";

    struct Chat {
        messages: Vec<ChatMessage>,
        /// true while assistant tokens are streaming into `stream_buf`
        streaming: bool,
        stream_buf: String,
        /// the `messageId` of the reply being streamed; a new one starts a new
        /// message
        stream_message_id: Option<String>,
        /// a `<think>` block is open in `stream_buf`
        in_thought: bool,
        thinking: bool,
        /// toggled periodically so the streaming cursor blinks
        blink_on: bool,
        blink_counter: u8,
        /// index of the plan entry in `messages`, once there is one
        plan_index: Option<usize>,
        /// `(used, size)` of the context window, as the agent last reported it
        context: Option<(u64, u64)>,
        session_title: Option<String>,
        config_options: Vec<SessionConfigOption>,
        /// the session mode id the agent last announced
        mode_id: Option<String>,
        /// slash commands the agent advertises, as `(name, description)`
        commands: Vec<(String, String)>,
        /// an on-device model finished loading in this run
        local_loaded: bool,
        /// A configured endpoint (`OPENAI_BASE_URL`, a `providers.toml`
        /// profile) serves every turn whatever the picker says, so it is the
        /// model the title shows.
        endpoint: Option<String>,
    }

    impl Chat {
        fn new() -> Self {
            Self {
                messages: Vec::new(),
                streaming: false,
                stream_buf: String::new(),
                stream_message_id: None,
                in_thought: false,
                thinking: false,
                blink_on: true,
                blink_counter: 0,
                plan_index: None,
                context: None,
                session_title: None,
                config_options: Vec::new(),
                mode_id: None,
                commands: Vec::new(),
                local_loaded: false,
                endpoint: None,
            }
        }

        /// Map one `session/update` onto the transcript.
        fn apply_update(&mut self, update: SessionUpdate) {
            match update {
                SessionUpdate::AgentMessageChunk(chunk) => self.message_chunk(chunk, false),
                SessionUpdate::AgentThoughtChunk(chunk) => self.message_chunk(chunk, true),
                SessionUpdate::UserMessageChunk(chunk) => {
                    // Only a replayed thread sends these.
                    self.finalize_stream();
                    if let Some(text) = chunk_text(&chunk.content) {
                        self.messages.push(ChatMessage::user(text));
                    }
                }
                SessionUpdate::ToolCall(call) => self.tool_call(call),
                SessionUpdate::ToolCallUpdate(update) => self.tool_update(update),
                SessionUpdate::Plan(plan) => self.set_plan(&plan),
                SessionUpdate::UsageUpdate(usage) => {
                    self.context = Some((usage.used, usage.size));
                }
                SessionUpdate::ConfigOptionUpdate(update) => {
                    self.config_options = update.config_options;
                }
                SessionUpdate::CurrentModeUpdate(update) => {
                    self.mode_id = Some(update.current_mode_id.0.to_string());
                }
                SessionUpdate::SessionInfoUpdate(update) => match update.title {
                    MaybeUndefined::Value(title) => self.session_title = Some(title),
                    MaybeUndefined::Null => self.session_title = None,
                    MaybeUndefined::Undefined => {}
                },
                SessionUpdate::AvailableCommandsUpdate(update) => {
                    self.commands = update
                        .available_commands
                        .into_iter()
                        .map(
                            |AvailableCommand {
                                 name, description, ..
                             }| (name, description),
                        )
                        .collect();
                }
                _ => {}
            }
        }

        fn message_chunk(&mut self, chunk: ContentChunk, thought: bool) {
            let Some(text) = chunk_text(&chunk.content) else {
                return;
            };
            let message_id = chunk.message_id.map(|id| id.0.to_string());
            // A new `messageId` is a new message; the server gives each
            // inference round one, and a standalone notice one of its own.
            if message_id.is_some() && message_id != self.stream_message_id {
                self.finalize_stream();
            }
            if message_id.is_some() {
                self.stream_message_id = message_id;
            }
            if thought {
                if !self.in_thought {
                    self.in_thought = true;
                    self.push_stream_delta("<think>");
                }
                self.push_stream_delta(&text);
            } else {
                self.close_thought();
                self.push_stream_delta(&text);
            }
        }

        fn close_thought(&mut self) {
            if self.in_thought {
                self.in_thought = false;
                self.push_stream_delta("</think>");
            }
        }

        fn tool_call(&mut self, call: ToolCall) {
            self.finalize_stream();
            let detail = call
                .raw_input
                .as_ref()
                .and_then(|value| serde_json::to_string_pretty(value).ok())
                .unwrap_or_default();
            let mut message = ChatMessage::tool_call(call.title, detail);
            message.tool_call_id = Some(call.tool_call_id.0.to_string());
            message.tool_result = content_preview(&call.content);
            self.note_status(message.tool_call_id.as_deref(), call.status);
            message.status = Some(call.status);
            self.messages.push(message);
        }

        fn tool_update(&mut self, update: ToolCallUpdate) {
            let id = update.tool_call_id.0.to_string();
            let fields = update.fields;
            if let Some(status) = fields.status {
                self.note_status(Some(&id), status);
            }
            let Some(entry) =
                self.messages.iter_mut().rev().find(|m| {
                    m.role == Role::Tool && m.tool_call_id.as_deref() == Some(id.as_str())
                })
            else {
                return;
            };
            if let Some(title) = fields.title {
                entry.text = title;
            }
            if let Some(status) = fields.status {
                entry.status = Some(status);
            }
            if let Some(input) = fields.raw_input.as_ref()
                && let Ok(pretty) = serde_json::to_string_pretty(input)
            {
                entry.tool_detail = Some(pretty);
            }
            if let Some(content) = fields.content.as_deref()
                && let Some(preview) = content_preview(content)
            {
                entry.tool_result = Some(preview);
            }
        }

        /// A load card finishing is how the UI learns an on-device model is in
        /// memory.
        fn note_status(&mut self, id: Option<&str>, status: ToolCallStatus) {
            if id.is_some_and(|id| id.starts_with(MODEL_LOAD_CARD_PREFIX))
                && status == ToolCallStatus::Completed
            {
                self.local_loaded = true;
            }
        }

        /// The plan is one entry, replaced in place as the agent revises it.
        fn set_plan(&mut self, plan: &Plan) {
            let mut lines = vec!["plan".to_string()];
            for entry in &plan.entries {
                let mark = match entry.status {
                    PlanEntryStatus::Completed => "☑",
                    PlanEntryStatus::InProgress => "◐",
                    _ => "☐",
                };
                lines.push(format!("  {mark} {}", entry.content));
            }
            let text = lines.join("\n");
            match self.plan_index {
                Some(index)
                    if self
                        .messages
                        .get(index)
                        .is_some_and(|message| message.role == Role::Plan) =>
                {
                    self.messages[index].text = text;
                }
                _ => {
                    self.finalize_stream();
                    self.plan_index = Some(self.messages.len());
                    self.messages.push(ChatMessage::plan(text));
                }
            }
        }

        fn finalize_stream(&mut self) {
            self.close_thought();
            self.streaming = false;
            self.stream_message_id = None;
            if !self.stream_buf.is_empty() {
                let text = std::mem::take(&mut self.stream_buf);
                self.messages.push(ChatMessage::assistant(text));
            }
            self.blink_on = false;
        }

        fn push_stream_delta(&mut self, delta: &str) {
            self.streaming = true;
            self.stream_buf.push_str(delta);
            // Hide reasoning the way the rest of the app does: keep the "thinking"
            // spinner until visible (non-<think>) text appears, then show the
            // live reply.
            let (_think, visible) = super::strip_think_blocks(&self.stream_buf);
            self.thinking = visible.trim().is_empty();
            self.blink_counter = self.blink_counter.wrapping_add(1);
            self.blink_on = self.blink_counter % 4 < 2;
        }

        /// The portion of the streaming buffer to show live, with reasoning hidden.
        fn visible_stream(&self) -> String {
            let (_think, visible) = super::strip_think_blocks(&self.stream_buf);
            visible
        }

        /// Forget the conversation on screen, keeping what describes the
        /// session itself (options, commands, whether a model is in memory).
        fn fresh_transcript(&mut self) -> Chat {
            let mut fresh = Chat::new();
            fresh.config_options = self.config_options.clone();
            fresh.mode_id = self.mode_id.clone();
            fresh.commands = self.commands.clone();
            fresh.local_loaded = self.local_loaded;
            fresh.endpoint = self.endpoint.clone();
            std::mem::replace(self, fresh)
        }

        // ── Reading the agent's config options ────────────────────────────────

        fn option(&self, id: &str) -> Option<&SessionConfigOption> {
            self.config_options
                .iter()
                .find(|option| option.id.0.as_ref() == id)
        }

        /// The value id the `Model` option currently holds.
        fn current_model_value(&self) -> Option<String> {
            match &self.option(crate::MODEL_CONFIG_ID)?.kind {
                SessionConfigKind::Select(select) => Some(select.current_value.0.to_string()),
                _ => None,
            }
        }

        fn model_row(&self) -> Option<PickerRow> {
            let current = self.current_model_value()?;
            picker_rows(&self.config_options)
                .into_iter()
                .find(|row| row.value == current)
        }

        /// The model label for the title bar, with the on-device state next to
        /// it: a local model is "not loaded" until a load card completes.
        fn model_label(&self) -> String {
            if let Some(endpoint) = &self.endpoint {
                return endpoint.clone();
            }
            match self.model_row() {
                Some(row) if !row.cloud && !self.local_loaded => {
                    format!("{} (not loaded)", row.name)
                }
                Some(row) => row.name,
                None => "siGit Code".to_string(),
            }
        }

        fn on_cloud(&self) -> bool {
            self.current_model_value()
                .is_some_and(|value| value.starts_with(CLOUD_ID_PREFIX))
        }

        fn tool_calling(&self) -> bool {
            if self.endpoint.is_some() {
                return true;
            }
            self.model_row()
                .map(|row| row.description.contains("tool calling"))
                .unwrap_or(true)
        }

        /// `Local inference` as the agent reports it. A toggle is a boolean, a
        /// select (an older client path) carries the two ids.
        fn local_inference(&self) -> Option<bool> {
            match &self.option(crate::LOCAL_INFERENCE_CONFIG_ID)?.kind {
                SessionConfigKind::Boolean(toggle) => Some(toggle.current_value),
                SessionConfigKind::Select(select) => {
                    Some(select.current_value.0.as_ref() == crate::LOCAL_INFERENCE_ON)
                }
                _ => None,
            }
        }

        /// The permission mode id: the config option when there is one, the
        /// last `current_mode_update` otherwise.
        fn permission_mode(&self) -> Option<String> {
            match self
                .option(crate::PERMISSION_MODE_CONFIG_ID)
                .map(|option| &option.kind)
            {
                Some(SessionConfigKind::Select(select)) => Some(select.current_value.0.to_string()),
                _ => self.mode_id.clone(),
            }
        }

        fn plan_mode(&self) -> bool {
            self.permission_mode().as_deref() == Some(crate::PERMISSION_MODE_PLAN)
        }
    }

    /// Plain text of a content block; images, audio and the like have none.
    fn chunk_text(block: &ContentBlock) -> Option<String> {
        match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        }
    }

    /// Lines of a diff kept in a tool card; the full change is on disk.
    const DIFF_PREVIEW_LINES: usize = 6;

    /// A short `-`/`+` preview of a file change.
    fn diff_preview(diff: &Diff) -> String {
        fn push(out: &mut Vec<String>, sign: char, text: &str) {
            let total = text.lines().count();
            for line in text.lines().take(DIFF_PREVIEW_LINES) {
                out.push(format!("{sign} {line}"));
            }
            if total > DIFF_PREVIEW_LINES {
                out.push(format!(
                    "{sign} … (+{} more lines)",
                    total - DIFF_PREVIEW_LINES
                ));
            }
        }
        let mut out = vec![diff.path.display().to_string()];
        if let Some(old) = diff.old_text.as_deref() {
            push(&mut out, '-', old);
        }
        push(&mut out, '+', &diff.new_text);
        out.join("\n")
    }

    /// What a tool card shows of its content: text through the usual output
    /// cap, a diff as its preview.
    fn content_preview(content: &[ToolCallContent]) -> Option<String> {
        let parts: Vec<String> = content
            .iter()
            .filter_map(|item| match item {
                ToolCallContent::Content(content) => {
                    chunk_text(&content.content).map(|text| super::cap_output_preview(&text))
                }
                ToolCallContent::Diff(diff) => Some(diff_preview(diff)),
                _ => None,
            })
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    // ── Model picker rows ─────────────────────────────────────────────────────

    /// One row of the picker, read off the agent's `Model` option.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct PickerRow {
        value: String,
        name: String,
        description: String,
        /// a siGit Code Cloud tier rather than an on-device model
        cloud: bool,
        current: bool,
    }

    fn picker_rows(options: &[SessionConfigOption]) -> Vec<PickerRow> {
        let Some(option) = options
            .iter()
            .find(|option| option.id.0.as_ref() == crate::MODEL_CONFIG_ID)
        else {
            return Vec::new();
        };
        let SessionConfigKind::Select(select) = &option.kind else {
            return Vec::new();
        };
        let flat: Vec<&SessionConfigSelectOption> = match &select.options {
            SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
            SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .collect(),
            _ => Vec::new(),
        };
        flat.into_iter()
            .map(|item| {
                let value = item.value.0.to_string();
                PickerRow {
                    cloud: value.starts_with(CLOUD_ID_PREFIX),
                    current: item.value == select.current_value,
                    name: item.name.clone(),
                    description: item.description.clone().unwrap_or_default(),
                    value,
                }
            })
            .collect()
    }

    // ── Permission prompts ────────────────────────────────────────────────────

    /// A tool call waiting on the user's y/a/n/d answer; the agent's turn is
    /// paused on the other end.
    struct PermissionPrompt {
        title: String,
        options: Vec<PermissionOption>,
        answer: oneshot::Sender<RequestPermissionOutcome>,
    }

    /// The kind of answer a key gives: y once, a for the session, n (or Esc)
    /// no, d no for the session.
    fn permission_kind_for_key(code: KeyCode) -> Option<PermissionOptionKind> {
        match code {
            KeyCode::Char('y' | 'Y') => Some(PermissionOptionKind::AllowOnce),
            KeyCode::Char('a' | 'A') => Some(PermissionOptionKind::AllowAlways),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(PermissionOptionKind::RejectOnce),
            KeyCode::Char('d' | 'D') => Some(PermissionOptionKind::RejectAlways),
            _ => None,
        }
    }

    /// The outcome for `kind` among the options the agent offered. The agent
    /// offers all four; if an "always" one is missing the single-use answer
    /// stands in, and an answer with nothing to match is a cancellation.
    fn permission_outcome(
        options: &[PermissionOption],
        kind: PermissionOptionKind,
    ) -> RequestPermissionOutcome {
        let fallback = match kind {
            PermissionOptionKind::AllowAlways => PermissionOptionKind::AllowOnce,
            PermissionOptionKind::RejectAlways => PermissionOptionKind::RejectOnce,
            other => other,
        };
        options
            .iter()
            .find(|option| option.kind == kind)
            .or_else(|| options.iter().find(|option| option.kind == fallback))
            .map(|option| {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    option.option_id.clone(),
                ))
            })
            .unwrap_or(RequestPermissionOutcome::Cancelled)
    }

    /// The ⚠ line (and the change it would make) for a permission request.
    fn permission_notice(request: &RequestPermissionRequest) -> String {
        let title = request
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "a tool call".to_string());
        let mut notice = format!(
            "⚠ permission — allow {title}?  [y]es · [a]lways this session · [n]o · [d]eny this session"
        );
        if let Some(content) = request.tool_call.fields.content.as_deref() {
            for item in content {
                if let ToolCallContent::Diff(diff) = item {
                    notice.push('\n');
                    notice.push_str(&diff_preview(diff));
                }
            }
        }
        notice
    }

    // ── Input routing ─────────────────────────────────────────────────────────

    /// The commands only this client knows. Everything else, built-ins and
    /// user-defined commands alike, is the agent's, and goes out as a prompt.
    #[derive(Debug, PartialEq, Eq)]
    enum LocalCommand {
        Exit,
        /// expand or collapse model reasoning; `None` flips it
        Thinking(Option<bool>),
        /// expand or collapse tool entries; `None` flips it
        Tools(Option<bool>),
        /// `/models` with no argument opens the picker
        Models,
        /// load the newest saved session of this directory
        Resume,
    }

    fn parse_local(input: &str) -> Option<LocalCommand> {
        let trimmed = input.trim();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let command = parts.next()?;
        let argument = parts.next().map(str::trim).filter(|a| !a.is_empty());
        match command {
            "/exit" | "/quit" | "/q" => Some(LocalCommand::Exit),
            "/thinking" => Some(LocalCommand::Thinking(parse_on_off(argument))),
            "/tools" => Some(LocalCommand::Tools(parse_on_off(argument))),
            "/models" if argument.is_none() => Some(LocalCommand::Models),
            "/resume" => Some(LocalCommand::Resume),
            _ => None,
        }
    }

    /// `on`/`off` (and synonyms) → `Some(bool)`; missing or unrecognized → `None`
    /// (meaning "toggle the current value").
    fn parse_on_off(arg: Option<&str>) -> Option<bool> {
        match arg.map(|s| s.trim().to_ascii_lowercase())?.as_str() {
            "on" | "true" | "1" | "yes" => Some(true),
            "off" | "false" | "0" | "no" => Some(false),
            _ => None,
        }
    }

    /// The client-only lines `/help` ends with, since the agent's reply knows
    /// nothing of them.
    const CLIENT_HELP: &str = "/thinking [on|off] - show or hide the model's reasoning\n\
         /tools [on|off]    - expand or collapse tool-call entries\n\
         /models            - open the model picker (/models N still switches by number)\n\
         /resume            - load the newest saved session of this directory\n\
         /exit              - quit (so does Ctrl+C when idle)\n\
         Ctrl+C / Esc       - cancel the running turn or stop waiting for a model load";

    /// The line shown for a turn that ended some way other than normally.
    fn stop_notice(stop: StopReason) -> Option<&'static str> {
        match stop {
            StopReason::Cancelled => Some("(cancelled)"),
            StopReason::MaxTokens => Some("(the reply was cut off: the model hit its token limit)"),
            StopReason::MaxTurnRequests => {
                Some("(stopped: the tool-round limit for one turn was reached)")
            }
            StopReason::Refusal => Some("(the model declined this request; it is not kept)"),
            _ => None,
        }
    }

    /// The text of a failed turn: the agent's message as it came, plus a
    /// `/login` hint when the endpoint turned the session down.
    fn turn_error_text(error: &AgentError) -> String {
        let lowered = error.message.to_ascii_lowercase();
        let unauthorized = error.auth_required
            || lowered.contains("401")
            || lowered.contains("unauthorized")
            || lowered.contains("session expired");
        if unauthorized {
            format!("error: {} — sign in with /login", error.message)
        } else {
            format!("error: {}", error.message)
        }
    }

    /// An ISO 8601 timestamp as `2026-10-18 10:15`; anything shorter as it is.
    fn session_when(updated_at: Option<&str>) -> String {
        match updated_at {
            Some(stamp) if stamp.len() >= 16 && stamp.is_char_boundary(16) => {
                stamp[..16].replace('T', " ")
            }
            Some(stamp) => stamp.to_string(),
            None => "time unknown".to_string(),
        }
    }

    /// One History-tab row: title (or id), when, and the id.
    fn session_row(info: &SessionInfo) -> String {
        let id = info.session_id.0.as_ref();
        let when = session_when(info.updated_at.as_deref());
        match info.title.as_deref().filter(|title| !title.is_empty()) {
            Some(title) => format!("{title}  ·  {when}  ·  {id}"),
            None => format!("{id}  ·  {when}"),
        }
    }

    /// Whether `a` and `b` name the same directory, tolerating a symlinked path
    /// on one side.
    fn same_dir(a: &Path, b: &Path) -> bool {
        a == b
            || match (a.canonicalize(), b.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
    }

    // ── Repo tab types ────────────────────────────────────────────────────────
    /// Which list the Repo tab is showing.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RepoSection {
        Issues,
        PullRequests,
    }

    impl RepoSection {
        fn other(self) -> Self {
            match self {
                RepoSection::Issues => RepoSection::PullRequests,
                RepoSection::PullRequests => RepoSection::Issues,
            }
        }
    }

    /// Lifecycle of one Repo-tab list. `Failed` carries the error text returned
    /// by the MCP layer (signed out, MCP off, tool error, unparsable reply) so
    /// the tab can show it verbatim plus a hint.
    enum RepoData {
        /// Nothing fetched yet — kicked off on first entry to the tab.
        NotFetched,
        Loading,
        Ready(Vec<RepoItem>),
        Failed(String),
    }

    impl RepoData {
        fn len(&self) -> usize {
            match self {
                RepoData::Ready(items) => items.len(),
                _ => 0,
            }
        }
    }

    /// Identifies which list row a detail fetch belongs to, so a slow reply
    /// can be matched against the currently-open detail and dropped if the
    /// user has since navigated elsewhere.
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct RepoTarget {
        section: RepoSection,
        number: u64,
    }

    /// The full-tab detail view opened with Enter on a list row.
    struct RepoDetail {
        /// Which issue/PR this view is showing; matched against arriving
        /// `RepoUpdate::Detail` replies so a stale fetch can't fill it.
        target: RepoTarget,
        title: String,
        /// `None` while the `get_issue` / `get_pull_request` fetch runs.
        text: Option<String>,
        scroll: u16,
    }

    /// Results from the Repo tab's spawned fetch tasks, multiplexed into the
    /// event loop alongside inference updates (same pattern as the Cloud tab).
    enum RepoUpdate {
        Issues(Result<Vec<RepoItem>, String>),
        PullRequests(Result<Vec<RepoItem>, String>),
        Detail { target: RepoTarget, text: String },
    }

    // ── Startup screen ────────────────────────────────────────────────────────

    /// What shows while the agent process starts, and why it did not.
    struct Startup {
        tick: u32,
        /// keeps the screen up so the user can read the error
        error: Option<String>,
        started: Instant,
    }

    impl Startup {
        fn new() -> Self {
            Self {
                tick: 0,
                error: None,
                started: Instant::now(),
            }
        }
    }

    // ── App state ─────────────────────────────────────────────────────────────

    struct Picker {
        index: usize,
        rows: Vec<PickerRow>,
    }

    struct App {
        chat: Chat,
        agent: AgentConn,
        input: String,
        cursor: usize,
        thinking_tick: u8,
        quit: bool,
        /// a `session/prompt` is in flight; stays set until the agent answers it
        turn_active: bool,
        /// waiting on a model switch (`session/set_config_option`)
        config_inflight: bool,
        /// waiting on `session/load`
        loading_session: bool,
        /// `/thinking` — expand the model's reasoning on rendered messages.
        /// Display-only: never changes what is sent to the model or saved.
        show_thinking: bool,
        /// `/tools` — expand every tool-call entry (default: collapsed titles)
        tools_expanded: bool,

        // ── Model picker ──────────────────────────────────────────────────────
        picker: Option<Picker>,
        /// the model the user last asked for, to notice a cloud tier that did
        /// not take because nobody is signed in
        model_request: Option<String>,
        /// a cloud tier to pick once the sign-in it waits on is done
        pending_cloud_pick: Option<String>,

        // ── Tool approval ─────────────────────────────────────────────────────
        permission: Option<PermissionPrompt>,

        // ── Client-side follow-ups of a forwarded command ─────────────────────
        /// `/help` was sent; the client's own lines follow the reply
        help_pending: bool,
        /// `/resume` asked for the list; load the newest when it arrives
        resume_pending: bool,
        /// the transcript and session a `session/load` replaced, kept until it
        /// is known to have worked
        restore_on_load_failure: Option<(Chat, SessionId)>,

        // ── Tab bar state ─────────────────────────────────────────────────────
        /// Which top-level tab is showing. A turn keeps running while the
        /// user is on History/Cloud; updates land in `chat` regardless.
        active_tab: Tab,

        // History tab: this directory's threads, from `session/list`.
        history_sessions: Vec<SessionInfo>,
        history_index: usize,
        history_loading: bool,
        /// Session id awaiting the confirming second `d`; any other key clears it.
        history_pending_delete: Option<String>,
        /// One-shot notice shown under the session list (e.g. a failed load).
        history_notice: Option<String>,

        // Cloud tab: status text is fetched async when the tab opens and cached.
        /// `None` while a fetch is in flight (renders as "fetching…").
        cloud_lines: Option<Vec<String>>,
        cloud_rx: Option<oneshot::Receiver<Vec<String>>>,

        // Repo tab: issues and pull requests of a sigit.si-hosted repo.
        /// `owner/name` when the cwd's `origin` remote points at the sigit.si
        /// host; `None` hides the Repo tab entirely.
        repo: Option<String>,
        repo_section: RepoSection,
        repo_issues: RepoData,
        repo_prs: RepoData,
        repo_issue_index: usize,
        repo_pr_index: usize,
        /// Detail view over the list, opened with Enter, closed with Esc.
        repo_detail: Option<RepoDetail>,
        /// Sender cloned into the spawned fetch tasks; kept here so the
        /// receiver never reads a spurious disconnect between fetches.
        repo_tx: Option<mpsc::UnboundedSender<RepoUpdate>>,
        repo_rx: Option<mpsc::UnboundedReceiver<RepoUpdate>>,
    }

    const BANNER_ART: &str = "\
77777777777777777777777777777777777777777777777777777777777777777777777777777777777777777777
77777777322222222222222222222222222222223777389969902208431358831999699051111177777777777777
1111111125555555555555555555555511113222311159    5002         088    3081771691111111111111
1111111111111111111111111111131136841   1482853332007    05    9043332891    400811111111111
1111111111111111111111111111111201        109    304    40     00    79      100041111111111
333333255555555555555555555552392   102   503    90    7000000005    903    0000023333333333
333333245454545454545454545433381    7600000    302    61    780    109    20009533333333333
3333333333333333333333333333333402      7001    08    761    202    902    90003333333333333
2222255555555555555555555555250899901    49    304    403    08    108    300042222222222222
2222222222222222222222222222269   106    03    901    06    505    402    000052222222222222
2222255555555555555555555555299        708    1002          80     00      90852222222222222
55555555555555555555555555555560953258000866660000051140866908666600008966900065555555555555
88888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888";

    const THINKING_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    fn rich_text_spans(text: &str, base_style: Style, bold_style: Style) -> Vec<Span<'static>> {
        let mut spans = Vec::new();

        for (segment, is_bold) in super::parse_rich_text_segments(text) {
            let style = if is_bold { bold_style } else { base_style };
            spans.push(Span::styled(segment, style));
        }

        if spans.is_empty() {
            spans.push(Span::styled(String::new(), base_style));
        }

        spans
    }

    impl App {
        fn new(agent: AgentConn, started: Started) -> Self {
            let mut chat = Chat::new();
            chat.endpoint = crate::provider::active_provider().map(|p| p.display_name);
            chat.config_options = started.config_options;
            chat.mode_id = started
                .modes
                .map(|modes| modes.current_mode_id.0.to_string());
            let mut app = Self {
                chat,
                agent,
                input: String::new(),
                cursor: 0,
                thinking_tick: 0,
                quit: false,
                turn_active: false,
                config_inflight: false,
                loading_session: false,
                show_thinking: false,
                tools_expanded: false,
                picker: None,
                model_request: None,
                pending_cloud_pick: None,
                permission: None,
                help_pending: false,
                resume_pending: false,
                restore_on_load_failure: None,
                active_tab: Tab::Session,
                history_sessions: Vec::new(),
                history_index: 0,
                history_loading: false,
                history_pending_delete: None,
                history_notice: None,
                cloud_lines: None,
                cloud_rx: None,
                repo: detect_sigit_repo(),
                repo_section: RepoSection::Issues,
                repo_issues: RepoData::NotFetched,
                repo_prs: RepoData::NotFetched,
                repo_issue_index: 0,
                repo_pr_index: 0,
                repo_detail: None,
                repo_tx: None,
                repo_rx: None,
            };
            app.show_welcome();
            app
        }

        /// the welcome banner and where the agent stands
        fn show_welcome(&mut self) {
            for line in BANNER_ART.lines() {
                self.chat.messages.push(ChatMessage::banner(line));
            }
            self.chat.messages.push(ChatMessage::system(""));
            self.chat.messages.push(ChatMessage::system(
                "In this world, nothing can be said to be certain, except death and taxes. ~ Pak Sigit",
            ));
            let model = self.chat.model_label();
            if let Some(provider) = crate::provider::active_provider() {
                self.chat.messages.push(ChatMessage::system(format!(
                    "Using {} (model {}).",
                    provider.display_name, provider.model
                )));
            } else if self.chat.on_cloud() {
                self.chat
                    .messages
                    .push(ChatMessage::system(format!("Current model: {model}")));
            } else {
                // On-device models are never loaded implicitly; prompt the user to
                // load one explicitly before their first message.
                let name = self
                    .chat
                    .model_row()
                    .map(|row| row.name)
                    .unwrap_or_else(|| "the selected model".to_string());
                self.chat.messages.push(ChatMessage::system(format!(
                    "No on-device model loaded. Run /load to load {name}, or /models to choose one."
                )));
            }
            self.chat
                .messages
                .push(ChatMessage::system("Type /help for commands."));
        }

        /// Whether the app is waiting on the agent for something.
        fn is_busy(&self) -> bool {
            self.turn_active || self.config_inflight || self.loading_session
        }

        fn spinner_frame(&self) -> &'static str {
            THINKING_FRAMES[(self.thinking_tick as usize) % THINKING_FRAMES.len()]
        }

        fn tick_thinking(&mut self) {
            self.thinking_tick = self.thinking_tick.wrapping_add(1);
        }

        /// Whether the Repo tab exists for this session.
        fn repo_visible(&self) -> bool {
            self.repo.is_some()
        }

        /// The data behind the Repo tab's active section.
        fn repo_list(&self) -> &RepoData {
            match self.repo_section {
                RepoSection::Issues => &self.repo_issues,
                RepoSection::PullRequests => &self.repo_prs,
            }
        }

        /// Selection index of the Repo tab's active section.
        fn repo_index_mut(&mut self) -> &mut usize {
            match self.repo_section {
                RepoSection::Issues => &mut self.repo_issue_index,
                RepoSection::PullRequests => &mut self.repo_pr_index,
            }
        }

        /// The History entry the cursor is on, if any.
        fn selected_session(&self) -> Option<&SessionInfo> {
            self.history_sessions.get(self.history_index)
        }

        /// Ask the agent for this directory's threads.
        fn refresh_history(&mut self) {
            self.history_loading = true;
            self.history_pending_delete = None;
            self.agent.list_sessions();
        }

        fn say(&mut self, text: impl Into<String>) {
            self.chat.messages.push(ChatMessage::system(text));
        }

        // ── Model picker ──────────────────────────────────────────────────────

        fn open_model_picker(&mut self) {
            let rows = picker_rows(&self.chat.config_options);
            if rows.is_empty() {
                self.say(
                    "The agent has not offered a model list yet. Try /models for the text list.",
                );
                return;
            }
            let index = rows.iter().position(|row| row.current).unwrap_or(0);
            self.picker = Some(Picker { index, rows });
        }

        fn move_picker(&mut self, forward: bool) {
            let Some(picker) = self.picker.as_mut() else {
                return;
            };
            let len = picker.rows.len();
            if len == 0 {
                return;
            }
            picker.index = if forward {
                (picker.index + 1) % len
            } else {
                (picker.index + len - 1) % len
            };
        }

        /// Pick the highlighted row. A download or load arrives as a card from
        /// the agent; a cloud tier chosen while signed out comes back as the
        /// agent's sign-in prompt, which `ConfigSet` notices.
        fn choose_model(&mut self) {
            let Some(picker) = self.picker.take() else {
                return;
            };
            let Some(row) = picker.rows.get(picker.index) else {
                return;
            };
            if row.current {
                return;
            }
            self.model_request = Some(row.value.clone());
            self.config_inflight = true;
            self.agent
                .set_config(crate::MODEL_CONFIG_ID, ConfigValue::Id(row.value.clone()));
        }

        /// Start the browser sign-in; the agent sends the URL back as an
        /// elicitation.
        fn start_sign_in(&mut self) {
            self.say("Signing in to siGit Code Cloud…");
            self.agent.authenticate();
        }
    }

    // ── Agent events ──────────────────────────────────────────────────────────

    fn handle_agent_event(app: &mut App, event: AgentEvent) {
        match event {
            AgentEvent::Update(session_id, update) => {
                // What a thread we have since left still sends.
                if session_id != app.agent.session_id {
                    return;
                }
                app.chat.apply_update(*update);
                // Outside a turn (a load that landed later, a notice) every
                // chunk is a whole message; nothing is going to continue it.
                if !app.turn_active && !app.loading_session {
                    app.chat.finalize_stream();
                }
            }
            AgentEvent::Permission(request, answer) => {
                // The y/a/n/d prompt lives on the Session tab; make sure the
                // user can see what they're answering.
                app.active_tab = Tab::Session;
                app.chat.finalize_stream();
                let notice = permission_notice(&request);
                app.chat.messages.push(ChatMessage::system(notice));
                app.permission = Some(PermissionPrompt {
                    title: request
                        .tool_call
                        .fields
                        .title
                        .clone()
                        .unwrap_or_else(|| "a tool call".to_string()),
                    options: request.options,
                    answer,
                });
            }
            AgentEvent::SignIn {
                url,
                message,
                answer,
            } => {
                app.active_tab = Tab::Session;
                // The URL has to be visible and copyable (ssh), and the
                // browser is tried too.
                let opened = crate::browser_auth::open_browser(&url);
                let hint = if opened {
                    "Your browser should open. If it does not, go to"
                } else {
                    "Open this page in a browser"
                };
                app.say(format!("{message}\n{hint}:\n{url}"));
                let _ = answer.send(true);
            }
            AgentEvent::SignInComplete => app.say("✓ Signed in to siGit Code Cloud."),
            AgentEvent::TurnDone(result) => finish_turn(app, result),
            AgentEvent::ConfigSet(result) => {
                app.config_inflight = false;
                match result {
                    Ok(options) => {
                        app.chat.config_options = options;
                        let asked = app.model_request.take();
                        if let Some(asked) = asked
                            && asked.starts_with(CLOUD_ID_PREFIX)
                            && app.chat.current_model_value().as_deref() != Some(asked.as_str())
                        {
                            // The agent kept the model and answered with its
                            // sign-in prompt: nobody is signed in.
                            app.pending_cloud_pick = Some(asked);
                            app.start_sign_in();
                        }
                    }
                    Err(error) => {
                        app.model_request = None;
                        app.say(format!("error: {error}"));
                    }
                }
            }
            AgentEvent::Sessions(result) => finish_listing(app, result),
            AgentEvent::Loaded(session_id, result) => finish_load(app, session_id, result),
            AgentEvent::Deleted(id, result) => {
                app.history_notice = Some(match result {
                    Ok(()) => format!("Deleted session '{id}'."),
                    Err(error) => format!("Could not delete '{id}': {error}"),
                });
                app.refresh_history();
            }
            AgentEvent::Authenticated(result) => match result {
                Ok(()) => {
                    app.say("✓ Signed in.");
                    if let Some(tier) = app.pending_cloud_pick.take() {
                        app.model_request = Some(tier.clone());
                        app.config_inflight = true;
                        app.agent
                            .set_config(crate::MODEL_CONFIG_ID, ConfigValue::Id(tier));
                    }
                }
                Err(error) => {
                    app.pending_cloud_pick = None;
                    app.say(format!("Sign-in failed: {error}"));
                }
            },
        }
    }

    /// A `session/prompt` ended.
    fn finish_turn(app: &mut App, result: Result<StopReason, AgentError>) {
        app.chat.finalize_stream();
        app.chat.thinking = false;
        app.turn_active = false;
        // Dropping a pending prompt answers it `Cancelled` on the agent's side,
        // so a turn that ended can't leave a tool waiting.
        app.permission = None;
        match result {
            Ok(stop) => {
                if let Some(notice) = stop_notice(stop) {
                    app.say(notice);
                }
                if std::mem::take(&mut app.help_pending) {
                    app.say(CLIENT_HELP);
                }
            }
            Err(error) => {
                app.help_pending = false;
                app.say(turn_error_text(&error));
                if error.auth_required {
                    app.start_sign_in();
                }
            }
        }
    }

    /// `session/list` answered.
    fn finish_listing(app: &mut App, result: Result<Vec<SessionInfo>, String>) {
        app.history_loading = false;
        let cwd = app.agent.roots.first().cloned().unwrap_or_default();
        let sessions = match result {
            Ok(sessions) => sessions
                .into_iter()
                .filter(|info| same_dir(&info.cwd, &cwd))
                .collect::<Vec<_>>(),
            Err(error) => {
                app.resume_pending = false;
                app.history_notice = Some(format!("Could not list sessions: {error}"));
                return;
            }
        };
        if std::mem::take(&mut app.resume_pending) {
            let current = app.agent.session_id.0.to_string();
            match sessions
                .iter()
                .find(|info| info.session_id.0.as_ref() != current)
            {
                Some(info) => start_load(app, info.session_id.clone()),
                None => app.say("No earlier session of this directory to resume."),
            }
        }
        app.history_sessions = sessions;
        app.history_index = app
            .history_index
            .min(app.history_sessions.len().saturating_sub(1));
    }

    /// Switch to another saved thread. The agent replays it as updates, so the
    /// scrollback starts over, and comes back if the load fails.
    fn start_load(app: &mut App, session_id: SessionId) {
        let previous = app.agent.session_id.clone();
        let old = app.chat.fresh_transcript();
        app.restore_on_load_failure = Some((old, previous));
        app.loading_session = true;
        app.active_tab = Tab::Session;
        app.agent.load_session(session_id);
    }

    fn finish_load(
        app: &mut App,
        session_id: SessionId,
        result: Result<Vec<agent_client_protocol::schema::v1::SessionConfigOption>, String>,
    ) {
        app.loading_session = false;
        app.chat.finalize_stream();
        match result {
            Ok(options) => {
                app.restore_on_load_failure = None;
                if !options.is_empty() {
                    app.chat.config_options = options;
                }
                app.say(format!(
                    "Loaded session '{}'. The scrollback above is its history.",
                    session_id.0
                ));
            }
            Err(error) => {
                if let Some((old, previous)) = app.restore_on_load_failure.take() {
                    app.chat = old;
                    app.agent.session_id = previous;
                }
                app.history_notice = Some(format!("Could not load '{}': {error}", session_id.0));
                app.active_tab = Tab::History;
            }
        }
    }

    // ── Model picker ─────────────────────────────────────────────────────────

    fn render_model_picker(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let Some(picker) = app.picker.as_ref() else {
            return;
        };
        let popup = centered_rect(82, 72, area);

        // clear the background so text doesn't bleed through
        frame.render_widget(Clear, popup);

        let block = Block::default()
            .title(" Select a model… ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .style(Style::default().bg(Color::Black));

        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let on_black = Style::default().bg(Color::Black);
        let local_on = app.chat.local_inference().unwrap_or(true);
        let (state_word, state_style) = if local_on {
            ("ON  (on-device)", Style::default().fg(Color::Green))
        } else {
            (
                "OFF (siGit Code Cloud)",
                Style::default().fg(Color::Magenta),
            )
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "Local inference: ",
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    state_word,
                    state_style.bg(Color::Black).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "    toggle with /local on|off",
                    Style::default().fg(Color::DarkGray).bg(Color::Black),
                ),
            ]),
            Line::from("").style(on_black),
        ];

        let mut last_cloud: Option<bool> = None;
        let mut selected_line = 0;
        for (index, row) in picker.rows.iter().enumerate() {
            // One header per group, whenever the group changes.
            if last_cloud != Some(row.cloud) {
                if last_cloud.is_some() {
                    lines.push(Line::from("").style(on_black));
                }
                let (mark, label, color) = if row.cloud {
                    ("☁", "siGit Code Cloud", Color::Magenta)
                } else {
                    ("◉", "On this machine", Color::Green)
                };
                let active = row.cloud != local_on;
                let style = if active {
                    Style::default()
                        .fg(color)
                        .bg(Color::Black)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(Color::DarkGray).bg(Color::Black)
                };
                lines.push(
                    Line::from(vec![Span::styled(format!("{mark} {label}"), style)])
                        .style(on_black),
                );
                last_cloud = Some(row.cloud);
            }

            let selected = index == picker.index;
            if selected {
                selected_line = lines.len();
            }
            let active = row.cloud != local_on;
            let base = if selected {
                Style::default().fg(Color::Black).bg(Color::Green)
            } else if active {
                Style::default().fg(Color::White).bg(Color::Black)
            } else {
                // Inactive group: still visible (we surface the offering) but dimmed.
                Style::default().fg(Color::DarkGray).bg(Color::Black)
            };
            let marker = if selected { "› " } else { "  " };
            let mut spans = vec![Span::styled(format!("{marker}{}", row.name), base)];
            if row.description.contains("tool calling") {
                spans.push(Span::styled(
                    "  ✓ tool calling",
                    if selected {
                        base
                    } else {
                        Style::default().fg(Color::Green).bg(Color::Black)
                    },
                ));
            }
            if row.description.contains("download on select") {
                spans.push(Span::styled(
                    "  ↓ download",
                    if selected {
                        base
                    } else {
                        Style::default().fg(Color::Blue).bg(Color::Black)
                    },
                ));
            }
            if row.current {
                spans.push(Span::styled(
                    "  ← current",
                    if selected {
                        base
                    } else {
                        Style::default().fg(Color::Cyan).bg(Color::Black)
                    },
                ));
            }
            lines.push(Line::from(spans).style(on_black));
            if selected && !row.description.is_empty() {
                lines.push(
                    Line::from(Span::styled(
                        format!("    {}", row.description),
                        Style::default().fg(Color::Gray).bg(Color::Black),
                    ))
                    .style(on_black),
                );
            }
        }

        // Keep the highlighted row visible when the list outgrows the popup.
        let scroll = selected_line.saturating_sub(inner.height.saturating_sub(3) as usize) as u16;
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .style(on_black),
            inner,
        );
    }

    fn centered_rect(
        percent_x: u16,
        percent_y: u16,
        area: ratatui::layout::Rect,
    ) -> ratatui::layout::Rect {
        let vertical = Layout::vertical([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

        Layout::horizontal([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
    }

    // ── Tab bar (Session / History / Repo / Cloud) ────────────────────────────

    /// Switch to `tab`, refreshing the data it shows. Entering History asks the
    /// agent for the threads; entering Cloud kicks off the async status fetch;
    /// entering Repo fetches the issue/PR lists on first visit.
    fn switch_tab(app: &mut App, tab: Tab) {
        app.active_tab = tab;
        match tab {
            Tab::Session => {}
            Tab::History => {
                app.refresh_history();
                app.history_notice = None;
            }
            Tab::Repo => {
                // Fetch once on first entry; `r` refreshes after that.
                if matches!(app.repo_issues, RepoData::NotFetched) {
                    refresh_repo(app);
                }
            }
            Tab::Cloud => refresh_cloud(app),
        }
    }

    /// Fetch the Cloud tab's status text on a background task and cache it.
    /// The account check may hit the network, so the tab shows "fetching…"
    /// until the oneshot resolves in the event loop. The rest is what the agent
    /// last told us, read now.
    fn refresh_cloud(app: &mut App) {
        let (tx, rx) = oneshot::channel();
        app.cloud_rx = Some(rx);
        app.cloud_lines = None;

        let on_cloud = app.chat.on_cloud();
        let model = app.chat.model_label();
        let local_inference = app.chat.local_inference();
        let permission_mode = app.chat.permission_mode();
        tokio::spawn(async move {
            // `status_line` already folds failures into its message, so a dead
            // network degrades to an error string rather than a stuck tab.
            let account = crate::account::status_line().await;

            let mut lines = Vec::new();
            lines.push(format!("Account:          {account}"));
            lines.push(format!(
                "Inference:        {}",
                if on_cloud {
                    "siGit Code Cloud"
                } else {
                    "on-device"
                }
            ));
            lines.push(format!("Model:            {model}"));
            lines.push(format!(
                "Local inference:  {}",
                match local_inference {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "unknown",
                }
            ));
            lines.push(format!(
                "Permissions:      {}",
                permission_mode_name(permission_mode.as_deref())
            ));
            let config_dir = std::env::var("SIGIT_CONFIG_DIR").unwrap_or_else(|_| {
                let home = std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_else(|_| "~".to_string());
                format!("{home}/.config/sigit")
            });
            lines.push(format!("Config dir:       {config_dir}"));

            let _ = tx.send(lines);
        });
    }

    /// The name the permission dropdown gives a mode id.
    fn permission_mode_name(mode: Option<&str>) -> &'static str {
        match mode {
            Some(crate::PERMISSION_MODE_AUTO) => "Auto — tools run without asking",
            Some(crate::PERMISSION_MODE_PLAN) => "Plan — research only, no edits or commands",
            Some(_) => "Manual — the agent asks before mutating tools",
            None => "unknown",
        }
    }

    // ── Repo tab (issues / pull requests via the official MCP server) ─────────

    /// `owner/name` when the current directory's `origin` remote lives on the
    /// sigit.si host (per `SIGIT_API_URL`). Runs `git remote get-url origin`
    /// once at startup; any failure (no git, no repo, no origin) → `None`.
    fn detect_sigit_repo() -> Option<String> {
        let output = std::process::Command::new("git")
            .args(["remote", "get-url", "origin"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let url = String::from_utf8_lossy(&output.stdout);
        super::parse_repo_remote(url.trim(), &super::sigit_host())
    }

    /// Fetch both Repo-tab lists on spawned tasks feeding `repo_rx` — the same
    /// spawned-fetch pattern as the Cloud tab, so the render loop never blocks
    /// on the network. Replacing the channel drops any in-flight fetch's
    /// updates on the floor, which is exactly what a refresh wants.
    fn refresh_repo(app: &mut App) {
        let Some(repo) = app.repo.clone() else {
            return;
        };

        let (tx, rx) = mpsc::unbounded_channel();
        app.repo_tx = Some(tx.clone());
        app.repo_rx = Some(rx);
        app.repo_issues = RepoData::Loading;
        app.repo_prs = RepoData::Loading;
        // A stale detail view would outlive the list it was opened from.
        app.repo_detail = None;

        let args = serde_json::json!({ "repo": repo }).to_string();

        let issues_tx = tx.clone();
        let issues_args = args.clone();
        tokio::spawn(async move {
            let tool = crate::mcp::official_tool_name("list_issues");
            let text = crate::mcp::call_tool(&tool, &issues_args).await;
            let _ = issues_tx.send(RepoUpdate::Issues(super::parse_repo_items(&text)));
        });

        tokio::spawn(async move {
            let tool = crate::mcp::official_tool_name("list_pull_requests");
            let text = crate::mcp::call_tool(&tool, &args).await;
            let _ = tx.send(RepoUpdate::PullRequests(super::parse_repo_items(&text)));
        });
    }

    /// Open the detail view for the selected row and fetch its body
    /// (`get_issue` / `get_pull_request`) on a spawned task.
    fn open_repo_detail(app: &mut App) {
        let Some(repo) = app.repo.clone() else {
            return;
        };
        let (tool, label, index) = match app.repo_section {
            RepoSection::Issues => ("get_issue", "Issue", app.repo_issue_index),
            RepoSection::PullRequests => ("get_pull_request", "Pull request", app.repo_pr_index),
        };
        let RepoData::Ready(items) = app.repo_list() else {
            return;
        };
        let Some(item) = items.get(index) else {
            return;
        };

        let number = item.number;
        let target = RepoTarget {
            section: app.repo_section,
            number,
        };
        app.repo_detail = Some(RepoDetail {
            target,
            title: format!("{label} #{number} — {}", item.title),
            text: None,
            scroll: 0,
        });

        let Some(tx) = app.repo_tx.clone() else {
            return;
        };
        let tool = crate::mcp::official_tool_name(tool);
        let args = serde_json::json!({ "repo": repo, "number": number }).to_string();
        tokio::spawn(async move {
            let text = crate::mcp::call_tool(&tool, &args).await;
            let _ = tx.send(RepoUpdate::Detail {
                target,
                text: super::format_repo_detail(&text),
            });
        });
    }

    /// Keys on the History, Repo and Cloud tabs (the Session tab keeps
    /// `handle_key`). Tab/Esc navigation is handled earlier in the event loop;
    /// this gets the rest.
    fn handle_tab_key(app: &mut App, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
            app.quit = true;
            return;
        }

        match app.active_tab {
            Tab::Session => {}
            Tab::History => match key.code {
                KeyCode::Up => {
                    app.history_pending_delete = None;
                    app.history_index = app.history_index.saturating_sub(1);
                }
                KeyCode::Down => {
                    app.history_pending_delete = None;
                    if app.history_index + 1 < app.history_sessions.len() {
                        app.history_index += 1;
                    }
                }
                KeyCode::Char('r') => {
                    app.refresh_history();
                    app.history_notice = None;
                }
                KeyCode::Char('d') => {
                    let Some(id) = app.selected_session().map(|e| e.session_id.0.to_string())
                    else {
                        return;
                    };
                    if app.history_pending_delete.as_deref() == Some(id.as_str()) {
                        app.history_pending_delete = None;
                        app.agent.delete_session(id);
                    } else {
                        app.history_pending_delete = Some(id);
                    }
                }
                KeyCode::Enter => {
                    app.history_pending_delete = None;
                    let Some(id) = app.selected_session().map(|e| e.session_id.clone()) else {
                        return;
                    };
                    start_load(app, id);
                }
                // Any other key cancels a pending delete confirmation.
                _ => app.history_pending_delete = None,
            },
            Tab::Repo => {
                // Detail view open: Up/Down scroll it; Esc (handled in the
                // event loop) closes it back to the list.
                if app.repo_detail.is_some() {
                    match key.code {
                        KeyCode::Up => {
                            if let Some(detail) = app.repo_detail.as_mut() {
                                detail.scroll = detail.scroll.saturating_sub(1);
                            }
                        }
                        KeyCode::Down => {
                            if let Some(detail) = app.repo_detail.as_mut() {
                                detail.scroll = detail.scroll.saturating_add(1);
                            }
                        }
                        // "r refresh" (as the footer advertises) re-fetches the
                        // item on screen, rather than closing the view back to
                        // the list as refresh_repo would.
                        KeyCode::Char('r') => open_repo_detail(app),
                        _ => {}
                    }
                    return;
                }
                match key.code {
                    KeyCode::Left | KeyCode::Right => {
                        app.repo_section = app.repo_section.other();
                    }
                    KeyCode::Char('i') => app.repo_section = RepoSection::Issues,
                    KeyCode::Char('p') => app.repo_section = RepoSection::PullRequests,
                    KeyCode::Up => {
                        let index = app.repo_index_mut();
                        *index = index.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let max = app.repo_list().len().saturating_sub(1);
                        let index = app.repo_index_mut();
                        *index = (*index + 1).min(max);
                    }
                    KeyCode::Enter => open_repo_detail(app),
                    KeyCode::Char('r') => refresh_repo(app),
                    _ => {}
                }
            }
            Tab::Cloud => match key.code {
                KeyCode::Char('l') => {
                    // Flip the agent's own setting; it answers with fresh options.
                    let enabled = !app.chat.local_inference().unwrap_or(true);
                    let boolean = matches!(
                        app.chat
                            .option(crate::LOCAL_INFERENCE_CONFIG_ID)
                            .map(|option| &option.kind),
                        Some(SessionConfigKind::Boolean(_))
                    );
                    let value = if boolean {
                        ConfigValue::Bool(enabled)
                    } else if enabled {
                        ConfigValue::Id(crate::LOCAL_INFERENCE_ON.to_string())
                    } else {
                        ConfigValue::Id(crate::LOCAL_INFERENCE_OFF.to_string())
                    };
                    app.config_inflight = true;
                    app.agent
                        .set_config(crate::LOCAL_INFERENCE_CONFIG_ID, value);
                    app.active_tab = Tab::Session;
                }
                KeyCode::Char('r') => refresh_cloud(app),
                _ => {}
            },
        }
    }

    fn render_tab_bar(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let titles: Vec<Line> = Tab::titles(app.repo_visible())
            .into_iter()
            .map(Line::from)
            .collect();
        let tabs = Tabs::new(titles)
            .select(app.active_tab.index(app.repo_visible()))
            .style(Style::default().fg(Color::DarkGray))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(tabs, area);
    }

    fn render_history_tab(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" saved sessions ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();

        if app.history_sessions.is_empty() {
            let text = if app.history_loading {
                "  fetching sessions…"
            } else {
                "  No saved sessions of this directory yet. Sessions are saved after each turn."
            };
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (index, info) in app.history_sessions.iter().enumerate() {
                let selected = index == app.history_index;
                let marker = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default().fg(Color::Black).bg(Color::Green)
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(Span::styled(
                    format!("{marker}{}", session_row(info)),
                    style,
                )));
            }
        }

        if let Some(ref id) = app.history_pending_delete {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  Delete '{id}'? Press d again to confirm — any other key cancels."),
                Style::default().fg(Color::Yellow),
            )));
        } else if let Some(ref notice) = app.history_notice {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  {notice}"),
                Style::default().fg(Color::Yellow),
            )));
        }

        // Keep the selection visible when the list outgrows the pane.
        let inner_height = inner.height as usize;
        let scroll = app
            .history_index
            .saturating_sub(inner_height.saturating_sub(1)) as u16;
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );
    }

    fn render_repo_tab(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(format!(" {} ", app.repo.as_deref().unwrap_or("repo")));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Detail view: a full-tab scrollable paragraph; Esc goes back.
        if let Some(ref detail) = app.repo_detail {
            let mut lines: Vec<Line> = vec![
                Line::from(Span::styled(
                    format!("  {}", detail.title),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ];
            match detail.text {
                None => lines.push(Line::from(Span::styled(
                    "  fetching…",
                    Style::default().fg(Color::DarkGray),
                ))),
                Some(ref text) => {
                    for text_line in text.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("  {text_line}"),
                            Style::default().fg(Color::White),
                        )));
                    }
                }
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((detail.scroll, 0)),
                inner,
            );
            return;
        }

        // Section header: Issues ←→ Pull requests.
        let selected_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD);
        let idle_style = Style::default().fg(Color::DarkGray);
        let issues_active = app.repo_section == RepoSection::Issues;
        let mut lines: Vec<Line> = vec![
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    " Issues ",
                    if issues_active {
                        selected_style
                    } else {
                        idle_style
                    },
                ),
                Span::raw("  "),
                Span::styled(
                    " Pull requests ",
                    if issues_active {
                        idle_style
                    } else {
                        selected_style
                    },
                ),
            ]),
            Line::from(""),
        ];
        // Rows above the first list entry, for the keep-selection-visible math.
        let header_rows = lines.len();

        let (data, index) = match app.repo_section {
            RepoSection::Issues => (&app.repo_issues, app.repo_issue_index),
            RepoSection::PullRequests => (&app.repo_prs, app.repo_pr_index),
        };

        match data {
            RepoData::NotFetched | RepoData::Loading => {
                lines.push(Line::from(Span::styled(
                    "  fetching…",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            RepoData::Failed(error) => {
                for error_line in error.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {error_line}"),
                        Style::default().fg(Color::Red),
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  These lists come from the official sigit.si MCP server. Sign in \
                     with /login, and make sure SIGIT_MCP / SIGIT_MCP_OFFICIAL are not \
                     set to off. Press r to retry.",
                    Style::default().fg(Color::Yellow),
                )));
            }
            RepoData::Ready(items) if items.is_empty() => {
                lines.push(Line::from(Span::styled(
                    if issues_active {
                        "  No issues."
                    } else {
                        "  No pull requests."
                    },
                    Style::default().fg(Color::DarkGray),
                )));
            }
            RepoData::Ready(items) => {
                for (row_index, item) in items.iter().enumerate() {
                    let selected = row_index == index;
                    let marker = if selected { "› " } else { "  " };
                    let style = if selected {
                        Style::default().fg(Color::Black).bg(Color::Green)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{marker}{}", super::repo_row(item)),
                        style,
                    )));
                }
            }
        }

        // Keep the selection visible when the list outgrows the pane (same
        // approach as the History tab, offset by the section header rows).
        let inner_height = inner.height as usize;
        let scroll = (index + header_rows).saturating_sub(inner_height.saturating_sub(1)) as u16;
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );
    }

    fn render_cloud_tab(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" siGit Code Cloud ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        match app.cloud_lines {
            None => lines.push(Line::from(Span::styled(
                "  fetching status…",
                Style::default().fg(Color::DarkGray),
            ))),
            Some(ref cloud_lines) => {
                for text in cloud_lines {
                    lines.push(Line::from(Span::styled(
                        format!("  {text}"),
                        Style::default().fg(Color::White),
                    )));
                }
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  l flips local inference; pick a model with /models. Sign in or out with /login and /logout.",
            Style::default().fg(Color::DarkGray),
        )));

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    fn render(frame: &mut Frame, app: &mut App) {
        let area = frame.area();

        match app.active_tab {
            Tab::Session => {
                let zones = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(area);

                render_tab_bar(frame, app, zones[0]);
                render_title(frame, app, zones[1]);
                render_messages(frame, app, zones[2]);
                render_input(frame, app, zones[3]);
                render_footer(frame, app, zones[4]);
            }
            // No input pane on the non-chat tabs: the Tab key always cycles.
            Tab::History | Tab::Repo | Tab::Cloud => {
                let zones = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(area);

                render_tab_bar(frame, app, zones[0]);
                render_title(frame, app, zones[1]);
                match app.active_tab {
                    Tab::History => render_history_tab(frame, app, zones[2]),
                    Tab::Repo => render_repo_tab(frame, app, zones[2]),
                    _ => render_cloud_tab(frame, app, zones[2]),
                }
                render_footer(frame, app, zones[3]);
            }
        }

        if app.picker.is_some() {
            render_model_picker(frame, app, area);
        }
    }

    fn render_title(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let model_label = format!(" siGit — {} ", app.chat.model_label());
        let tool_label = if app.chat.tool_calling() {
            " [tools on] "
        } else {
            " [tools off] "
        };
        let (used, window) = app.chat.context.unwrap_or((0, 0));
        let mut spans = vec![
            Span::styled(
                model_label,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                tool_label,
                Style::default().fg(Color::Black).bg(Color::DarkGray),
            ),
            Span::styled(
                format!(" {} ", context_gauge_label(used, window)),
                Style::default()
                    .fg(Color::Black)
                    .bg(context_gauge_color(used, window)),
            ),
        ];
        if app.chat.plan_mode() {
            spans.push(Span::styled(
                " [plan mode] ",
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ));
        }
        if let Some(title) = app.chat.session_title.as_deref() {
            spans.push(Span::styled(
                format!(" {title}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn context_gauge_label(used: u64, window: u64) -> String {
        if window == 0 {
            return "ctx —".to_string();
        }
        let percent = (used.saturating_mul(100) / window).min(100);
        format!(
            "{percent}% · {}/{}",
            compact_tokens(used),
            crate::models::format_context_window(window)
        )
    }

    /// Warn as the window fills: auto-compaction kicks in near the budget, so
    /// the colour is a heads-up that the next turn may be summarized.
    fn context_gauge_color(used: u64, window: u64) -> Color {
        if window == 0 {
            return Color::DarkGray;
        }
        match used.saturating_mul(100) / window {
            0..=69 => Color::DarkGray,
            70..=89 => Color::Yellow,
            _ => Color::Red,
        }
    }

    fn compact_tokens(tokens: u64) -> String {
        if tokens >= 1_000 {
            format!("{}K", tokens / 1_000)
        } else {
            tokens.to_string()
        }
    }

    fn render_startup_title(frame: &mut Frame, startup: &Startup, area: ratatui::layout::Rect) {
        const SPINNER: &[&str] = &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
        let spin = SPINNER[(startup.tick as usize) % SPINNER.len()];
        let label = format!(" siGit {spin} Starting siGit Code… ");
        let line = Line::from(Span::styled(
            label,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_startup(frame: &mut Frame, startup: &Startup) {
        let zones = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
        render_startup_title(frame, startup, zones[0]);

        let elapsed = startup.started.elapsed().as_secs();
        let elapsed_str = if elapsed < 60 {
            format!("{elapsed}s")
        } else {
            format!("{}m {}s", elapsed / 60, elapsed % 60)
        };
        let content = if let Some(ref err) = startup.error {
            format!(
                "\n\n  ✗ siGit Code did not start ({elapsed_str}).\n\n  {err}\n\n  Press Ctrl+C to exit."
            )
        } else {
            format!("\n\n  Starting the agent… ({elapsed_str})")
        };
        let style = if startup.error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::White)
        };
        frame.render_widget(
            Paragraph::new(content)
                .style(style)
                .wrap(Wrap { trim: false }),
            zones[1],
        );

        let line = Line::from(vec![
            Span::styled(" Ctrl+C ", Style::default().fg(Color::Black).bg(Color::Red)),
            Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        ]);
        frame.render_widget(Paragraph::new(line), zones[2]);
    }

    fn render_messages(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let inner_width = area.width.saturating_sub(2);
        let inner_height = area.height.saturating_sub(2);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();

        for msg in &app.chat.messages {
            render_chat_message(
                &mut lines,
                msg,
                inner_width as usize,
                app.tools_expanded,
                app.show_thinking,
            );
        }

        let streamed_visible = app.chat.visible_stream();
        if !streamed_visible.is_empty() {
            let fake = ChatMessage::plain(Role::Assistant, streamed_visible);
            render_chat_message(
                &mut lines,
                &fake,
                inner_width as usize,
                app.tools_expanded,
                app.show_thinking,
            );
            if app.chat.blink_on
                && let Some(last) = lines.last_mut()
            {
                last.spans
                    .push(Span::styled("▋", Style::default().fg(Color::Green)));
            }
        }

        if app.chat.thinking {
            lines.push(Line::from(Span::styled(
                format!("  {} thinking…", app.spinner_frame()),
                Style::default().fg(Color::DarkGray),
            )));
            // While the stream buffer holds only reasoning, show its live tail
            // under the label so the user watches the model think.
            let tail_width = (inner_width as usize).saturating_sub(4).max(8);
            for tail_line in super::streaming_think_tail(&app.chat.stream_buf, tail_width, 3) {
                lines.push(Line::from(Span::styled(
                    format!("    {tail_line}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC),
                )));
            }
        } else if app.turn_active && !app.chat.streaming {
            // The agent is working (a tool is running, the model is loading
            // into a round); its cards above say what.
            lines.push(Line::from(Span::styled(
                format!("  {} working…", app.spinner_frame()),
                Style::default().fg(Color::DarkGray),
            )));
        } else if app.config_inflight {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} waiting for the model… (Ctrl+C stops waiting; it keeps loading)",
                    app.spinner_frame()
                ),
                Style::default().fg(Color::DarkGray),
            )));
        } else if app.loading_session {
            lines.push(Line::from(Span::styled(
                format!("  {} loading the session…", app.spinner_frame()),
                Style::default().fg(Color::DarkGray),
            )));
        }

        // Always pin to the bottom so the latest message stays visible. There is
        // no scrollback, so we just need the exact number of wrapped rows the
        // paragraph occupies at this width — `line_count` runs the same
        // WordWrapper as rendering, so it never diverges from what's drawn (an
        // estimate would, e.g. by forgetting the `<think>` box lines, and scroll
        // too little — the bug this fixes).
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total_lines = paragraph.line_count(inner_width) as u16;
        let scroll = total_lines.saturating_sub(inner_height);

        frame.render_widget(paragraph.scroll((scroll, 0)), inner);
    }

    /// The glyph after a tool title that says where the call stands.
    fn status_suffix(status: Option<ToolCallStatus>) -> &'static str {
        match status {
            Some(ToolCallStatus::Failed) => " ✗",
            Some(ToolCallStatus::Pending) => " (waiting for approval)",
            Some(ToolCallStatus::InProgress) => " …",
            _ => "",
        }
    }

    fn render_chat_message(
        lines: &mut Vec<Line<'static>>,
        msg: &ChatMessage,
        _width: usize,
        tools_expanded: bool,
        show_thinking: bool,
    ) {
        match msg.role {
            Role::Tool => {
                let dim = Style::default()
                    .fg(Color::Rgb(132, 132, 145))
                    .add_modifier(Modifier::DIM);
                for entry_line in super::tool_entry_lines(
                    &format!("{}{}", msg.text, status_suffix(msg.status)),
                    msg.tool_detail.as_deref().unwrap_or(""),
                    msg.tool_result.as_deref(),
                    tools_expanded,
                ) {
                    lines.push(Line::from(Span::styled(format!("  {entry_line}"), dim)));
                }
            }
            Role::Plan => {
                for (index, text_line) in msg.text.split('\n').enumerate() {
                    let style = if index == 0 {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    lines.push(Line::from(Span::styled(format!("  {text_line}"), style)));
                }
            }
            Role::Banner => {
                let palette = [
                    Color::Red,
                    Color::Yellow,
                    Color::Green,
                    Color::Cyan,
                    Color::Blue,
                    Color::Magenta,
                ];
                let mut spans = Vec::new();
                for (i, ch) in msg.text.chars().enumerate() {
                    let color = palette[i % palette.len()];
                    spans.push(Span::styled(ch.to_string(), Style::default().fg(color)));
                }
                lines.push(Line::from(spans));
            }
            Role::System => {
                for text_line in msg.text.split('\n') {
                    let trimmed = text_line.trim();
                    let (prefix, body) = if trimmed.is_empty() {
                        ("", "")
                    } else {
                        ("  · ", trimmed)
                    };

                    lines.push(Line::from(vec![
                        Span::styled(
                            prefix.to_string(),
                            Style::default()
                                .fg(Color::Rgb(90, 90, 98))
                                .add_modifier(Modifier::DIM),
                        ),
                        Span::styled(
                            body.to_string(),
                            Style::default()
                                .fg(Color::Rgb(132, 132, 145))
                                .add_modifier(Modifier::ITALIC | Modifier::DIM),
                        ),
                    ]));
                }
            }
            Role::User => {
                let prefix = Span::styled(
                    "you > ".to_string(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                );
                let mut first = true;
                for text_line in msg.text.split('\n') {
                    if first {
                        lines.push(Line::from(vec![
                            prefix.clone(),
                            Span::raw(text_line.to_string()),
                        ]));
                        first = false;
                    } else {
                        lines.push(Line::from(Span::raw(format!("       {text_line}"))));
                    }
                }
            }
            Role::Assistant => {
                if let Some(ref think) = msg.think_block {
                    if show_thinking {
                        // /thinking on — the full reasoning, dim italic, with a
                        // blank line separating it from the answer below.
                        lines.push(Line::from(Span::styled(
                            "  · thinking".to_string(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::DIM),
                        )));
                        for think_line in think.split('\n') {
                            lines.push(Line::from(Span::styled(
                                format!("    {think_line}"),
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::DIM | Modifier::ITALIC),
                            )));
                        }
                        lines.push(Line::from(""));
                    } else {
                        // collapsed — one dim line noting reasoning exists
                        lines.push(Line::from(Span::styled(
                            format!("  {}", super::think_indicator(think)),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::DIM),
                        )));
                    }
                }

                // Reasoning with no reply after it (a turn that ended in a tool
                // call) leaves nothing to put behind the prefix.
                if msg.text.is_empty() {
                    return;
                }
                let prefix = Span::styled(
                    "siGit > ".to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
                let body_style = Style::default();
                let bold_style = Style::default().add_modifier(Modifier::BOLD);
                let mut first = true;
                for text_line in msg.text.split('\n') {
                    if first {
                        let mut spans = vec![prefix.clone()];
                        spans.extend(rich_text_spans(text_line, body_style, bold_style));
                        lines.push(Line::from(spans));
                        first = false;
                    } else {
                        let mut spans = vec![Span::raw("         ".to_string())];
                        spans.extend(rich_text_spans(text_line, body_style, bold_style));
                        lines.push(Line::from(spans));
                    }
                }
            }
        }
    }

    fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" message ");

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let display = app.input.clone();
        frame.render_widget(
            Paragraph::new(display.clone()).wrap(Wrap { trim: false }),
            inner,
        );

        let col = (app.cursor as u16) % inner.width;
        let row = (app.cursor as u16) / inner.width;
        frame.set_cursor_position(Position {
            x: inner.x + col,
            y: inner.y + row,
        });
    }

    fn render_footer(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let key_style = Style::default().fg(Color::Black).bg(Color::Green);
        let label_style = Style::default().fg(Color::DarkGray);

        if app.active_tab != Tab::Session {
            let hints: &[(&str, &str)] = match app.active_tab {
                Tab::History => &[
                    (" ↑/↓ ", " select  "),
                    (" Enter ", " load  "),
                    (" d d ", " delete  "),
                    (" r ", " refresh  "),
                    (" Tab ", " next tab  "),
                    (" Esc ", " session"),
                ],
                Tab::Repo if app.repo_detail.is_some() => &[
                    (" ↑/↓ ", " scroll  "),
                    (" Esc ", " back  "),
                    (" r ", " refresh  "),
                    (" Tab ", " next tab"),
                ],
                Tab::Repo => &[
                    (" ←/→ i/p ", " section  "),
                    (" ↑/↓ ", " select  "),
                    (" Enter ", " open  "),
                    (" r ", " refresh  "),
                    (" Tab ", " next tab  "),
                    (" Esc ", " session"),
                ],
                _ => &[
                    (" l ", " toggle local inference  "),
                    (" r ", " refresh  "),
                    (" Tab ", " next tab  "),
                    (" Esc ", " session"),
                ],
            };
            let mut spans = Vec::new();
            for (key, label) in hints {
                spans.push(Span::styled(key.to_string(), key_style));
                spans.push(Span::styled(label.to_string(), label_style));
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }

        let mut spans = vec![
            Span::styled(" Enter ", key_style),
            Span::styled(" send  ", label_style),
            Span::styled(" Tab ", key_style),
            Span::styled(" tabs  ", label_style),
            Span::styled(
                " /help ",
                Style::default().fg(Color::Black).bg(Color::DarkGray),
            ),
            Span::styled(" commands  ", Style::default().fg(Color::DarkGray)),
            Span::styled(" Ctrl+C ", Style::default().fg(Color::Black).bg(Color::Red)),
            Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        ];

        if let Some(prompt) = &app.permission {
            spans.push(Span::styled(
                format!(
                    "  allow {}? [y]es · [a]lways · [n]o · [d]eny always",
                    prompt.title
                ),
                Style::default().fg(Color::Yellow),
            ));
        } else if app.is_busy() {
            spans.push(Span::styled(
                "  (busy — Ctrl+C to cancel)",
                Style::default().fg(Color::Yellow),
            ));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    // ── Keys ──────────────────────────────────────────────────────────────────

    /// What a key does on the Session tab when nothing is running. Returns the
    /// line to send when Enter ends one.
    fn handle_key(app: &mut App, key: KeyEvent) -> Option<String> {
        if app.picker.is_some() {
            match key.code {
                KeyCode::Esc => app.picker = None,
                KeyCode::Up => app.move_picker(false),
                KeyCode::Down => app.move_picker(true),
                KeyCode::Enter => app.choose_model(),
                _ => {}
            }
            return None;
        }

        match key.code {
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.quit = true;
                None
            }
            KeyCode::Enter => {
                if app.input.trim().is_empty() {
                    return None;
                }
                let text = std::mem::take(&mut app.input);
                app.cursor = 0;
                Some(text)
            }
            KeyCode::Backspace => {
                if app.cursor > 0 {
                    app.cursor -= 1;
                    app.input.remove(app.cursor);
                }
                None
            }
            KeyCode::Delete => {
                if app.cursor < app.input.len() {
                    app.input.remove(app.cursor);
                }
                None
            }
            KeyCode::Left => {
                app.cursor = app.cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                if app.cursor < app.input.len() {
                    app.cursor += 1;
                }
                None
            }
            KeyCode::Home => {
                app.cursor = 0;
                None
            }
            KeyCode::End => {
                app.cursor = app.input.len();
                None
            }
            KeyCode::Char(ch) => {
                app.input.insert(app.cursor, ch);
                app.cursor += 1;
                None
            }
            _ => None,
        }
    }

    /// The reaction to one key press, whatever the app is doing.
    fn handle_terminal_key(app: &mut App, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let quit_key = ctrl && matches!(key.code, KeyCode::Char('c' | 'd'));

        // A tool waiting on y/a/n/d. Checked before the busy gate because the
        // app *is* busy here: the turn is paused on the answer.
        if app.permission.is_some() {
            if quit_key {
                // cancel the whole turn; `Cancelled` is what an unanswered
                // request means to the agent
                app.permission = None;
                app.agent.cancel();
            } else if let Some(kind) = permission_kind_for_key(key.code)
                && let Some(prompt) = app.permission.take()
            {
                let verdict = match kind {
                    PermissionOptionKind::AllowOnce => "allowed once",
                    PermissionOptionKind::AllowAlways => "allowed for this session",
                    PermissionOptionKind::RejectOnce => "denied",
                    _ => "denied for this session",
                };
                app.say(format!("{}: {verdict}", prompt.title));
                let _ = prompt
                    .answer
                    .send(permission_outcome(&prompt.options, kind));
            }
            return;
        }

        // ── Tab-bar navigation ────────────────────────────────────────────────
        // Handled before the busy gate so the user can look at History/Cloud
        // while a turn runs (updates keep landing in the Session tab's
        // transcript). The Tab key only cycles when the input buffer is empty,
        // so pasted text containing tabs can't fight it; on non-Session tabs
        // the input is hidden, so it always cycles there.
        if app.picker.is_none() {
            if key.code == KeyCode::Tab && (app.active_tab != Tab::Session || app.input.is_empty())
            {
                let next = app.active_tab.next(app.repo_visible());
                switch_tab(app, next);
                return;
            }
            if app.active_tab != Tab::Session && key.code == KeyCode::Esc {
                // On the Repo tab, Esc first closes an open detail view (back
                // to the list).
                if app.active_tab == Tab::Repo && app.repo_detail.is_some() {
                    app.repo_detail = None;
                } else {
                    app.active_tab = Tab::Session;
                }
                return;
            }
        }

        // busy — only the cancel keys work
        if app.is_busy() {
            if quit_key || key.code == KeyCode::Esc {
                cancel_busy(app);
            }
            return;
        }

        // History / Repo / Cloud have their own key handling; the chat input is
        // inactive there.
        if app.active_tab != Tab::Session {
            handle_tab_key(app, key);
            return;
        }

        if let Some(text) = handle_key(app, key) {
            submit(app, text);
        }
    }

    /// Ctrl+C / Esc while the agent is working. A turn stays busy until the
    /// agent answers it, so the transcript and the server agree on what
    /// happened; waiting for a model switch is given up on the spot.
    fn cancel_busy(app: &mut App) {
        if app.config_inflight {
            app.agent.cancel_config();
            app.config_inflight = false;
            app.model_request = None;
            app.say(
                "(stopped waiting — the model keeps loading in the background and you will \
                 be told when it is ready; chat continues on the previous backend)",
            );
        }
        if app.turn_active {
            app.agent.cancel();
        }
    }

    /// An input line: the few commands only this client knows are handled
    /// here, everything else goes to the agent as a prompt.
    fn submit(app: &mut App, text: String) {
        match parse_local(&text) {
            Some(LocalCommand::Exit) => app.quit = true,
            Some(LocalCommand::Thinking(value)) => {
                app.show_thinking = value.unwrap_or(!app.show_thinking);
                let state = if app.show_thinking { "shown" } else { "hidden" };
                app.say(format!("Model reasoning is now {state}."));
            }
            Some(LocalCommand::Tools(value)) => {
                app.tools_expanded = value.unwrap_or(!app.tools_expanded);
                let state = if app.tools_expanded {
                    "expanded"
                } else {
                    "collapsed"
                };
                app.say(format!("Tool-call entries are now {state}."));
            }
            Some(LocalCommand::Models) => app.open_model_picker(),
            Some(LocalCommand::Resume) => {
                app.resume_pending = true;
                app.refresh_history();
            }
            None => send_prompt(app, text),
        }
    }

    fn send_prompt(app: &mut App, text: String) {
        app.chat.messages.push(ChatMessage::user(&text));
        if text.trim() == "/help" {
            app.help_pending = true;
        }
        app.turn_active = true;
        app.chat.thinking = true;
        app.agent.prompt(text);
    }

    // ── Main loop ─────────────────────────────────────────────────────────────

    /// entry point — blocks until the user quits. The caller owns terminal
    /// init/restore.
    pub async fn run_with<B: ratatui::backend::Backend>(
        terminal: &mut ratatui::Terminal<B>,
    ) -> Result<()>
    where
        // ratatui 0.30 gives each backend its own error type; `?` into anyhow needs it
        // to be thread-safe.
        B::Error: Send + Sync + 'static,
    {
        let mut startup = Startup::new();
        terminal.draw(|frame| render_startup(frame, &startup))?;

        let cwd = std::env::current_dir()?;
        let mut running = false;
        let outcome = acp_client::connect(vec![cwd], async |agent, started, agent_rx| {
            running = true;
            let mut app = App::new(agent, started);
            event_loop(terminal, &mut app, agent_rx).await
        })
        .await;

        match outcome {
            Err(error) if !running => {
                // The agent never came up: keep the reason on screen.
                startup.error = Some(format!("{error:#}"));
                show_startup_error(terminal, &mut startup).await
            }
            other => other,
        }
    }

    /// Hold the startup failure on screen until the user quits.
    async fn show_startup_error<B: ratatui::backend::Backend>(
        terminal: &mut ratatui::Terminal<B>,
        startup: &mut Startup,
    ) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        let mut event_stream = EventStream::new();
        loop {
            terminal.draw(|frame| render_startup(frame, startup))?;
            let Some(Ok(event)) = event_stream.next().await else {
                return Ok(());
            };
            if let Event::Key(key) = event
                && key.kind == KeyEventKind::Press
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'd'))
            {
                return Ok(());
            }
        }
    }

    async fn event_loop<B: ratatui::backend::Backend>(
        terminal: &mut ratatui::Terminal<B>,
        app: &mut App,
        mut agent_rx: mpsc::UnboundedReceiver<AgentEvent>,
    ) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        let mut event_stream = EventStream::new();

        loop {
            // redraw every iteration
            terminal.draw(|frame| render(frame, app))?;

            if app.quit {
                break;
            }

            let animate = app.is_busy() || app.chat.thinking;

            // multiplex terminal events, the agent's updates, the tab fetches,
            // and the spinner timer.
            tokio::select! {
                // ── the agent ─────────────────────────────────────────────────
                event = agent_rx.recv() => {
                    match event {
                        Some(event) => handle_agent_event(app, event),
                        None => break,
                    }
                }

                // ── Cloud tab status fetch resolving ─────────────────────────
                status = async {
                    match app.cloud_rx.as_mut() {
                        Some(rx) => rx.await,
                        None => pending().await,
                    }
                } => {
                    app.cloud_rx = None;
                    app.cloud_lines = Some(status.unwrap_or_else(|_| {
                        vec!["error: the status fetch task died — press r to retry".to_string()]
                    }));
                }

                // ── Repo tab fetches resolving ────────────────────────────────
                update = async {
                    match app.repo_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => pending().await,
                    }
                } => {
                    match update {
                        Some(RepoUpdate::Issues(result)) => {
                            app.repo_issues = match result {
                                Ok(items) => RepoData::Ready(items),
                                Err(error) => RepoData::Failed(error),
                            };
                            app.repo_issue_index = app
                                .repo_issue_index
                                .min(app.repo_issues.len().saturating_sub(1));
                        }
                        Some(RepoUpdate::PullRequests(result)) => {
                            app.repo_prs = match result {
                                Ok(items) => RepoData::Ready(items),
                                Err(error) => RepoData::Failed(error),
                            };
                            app.repo_pr_index = app
                                .repo_pr_index
                                .min(app.repo_prs.len().saturating_sub(1));
                        }
                        Some(RepoUpdate::Detail { target, text }) => {
                            // Apply only to the matching open view: a reply for
                            // a detail the user has since closed or navigated
                            // away from is dropped, never shown under the wrong
                            // title.
                            if let Some(detail) = app.repo_detail.as_mut()
                                && detail.target == target
                            {
                                detail.text = Some(text);
                            }
                        }
                        // All senders gone (can't happen while app.repo_tx is
                        // held) — stop polling this channel.
                        None => app.repo_rx = None,
                    }
                }

                // ── spinner tick (100ms) ──────────────────────────────────────
                _ = async {
                    if animate {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    } else {
                        pending().await
                    }
                } => {
                    app.tick_thinking();
                }

                // ── Terminal events ───────────────────────────────────────────
                maybe_event = event_stream.next() => {
                    let Some(Ok(event)) = maybe_event else {
                        break;
                    };
                    // Windows reports both Press and Release; only a press acts.
                    if let Event::Key(key) = event
                        && key.kind == KeyEventKind::Press
                    {
                        handle_terminal_key(app, key);
                    }
                }
            }
        }

        Ok(())
    }

    #[cfg(test)]
    mod gauge_tests {
        use super::{compact_tokens, context_gauge_color, context_gauge_label};
        use ratatui::style::Color;

        #[test]
        fn context_gauge_reports_share_of_the_window() {
            assert_eq!(context_gauge_label(168_000, 200_000), "84% · 168K/200K ctx");
            assert_eq!(context_gauge_label(0, 24_000), "0% · 0/24K ctx");
            // Over-full history (pre-compaction) clamps rather than reading 130%.
            assert_eq!(context_gauge_label(31_000, 24_000), "100% · 31K/24K ctx");
        }

        #[test]
        fn context_gauge_handles_an_unknown_window() {
            assert_eq!(context_gauge_label(1_000, 0), "ctx —");
            assert_eq!(context_gauge_color(1_000, 0), Color::DarkGray);
        }

        #[test]
        fn context_gauge_warms_up_as_the_window_fills() {
            assert_eq!(context_gauge_color(1_000, 24_000), Color::DarkGray);
            assert_eq!(context_gauge_color(18_000, 24_000), Color::Yellow);
            assert_eq!(context_gauge_color(23_000, 24_000), Color::Red);
        }

        #[test]
        fn compact_tokens_abbreviates_thousands_only() {
            assert_eq!(compact_tokens(999), "999");
            assert_eq!(compact_tokens(1_500), "1K");
        }
    }

    // ── Behaviour tests ───────────────────────────────────────────────────────

    #[cfg(test)]
    mod behaviour_tests {
        use agent_client_protocol::schema::v1::{
            ContentChunk, Diff, MessageId, PermissionOption, PermissionOptionId,
            PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
            RequestPermissionOutcome, RequestPermissionRequest, SessionConfigOption,
            SessionConfigSelectOption, SessionConfigValueId, SessionId, SessionUpdate, StopReason,
            ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
            UsageUpdate,
        };
        use crossterm::event::KeyCode;

        use super::{
            AgentError, Chat, LocalCommand, Role, diff_preview, parse_local,
            permission_kind_for_key, permission_notice, permission_outcome, picker_rows,
            session_row, session_when, stop_notice, turn_error_text,
        };

        fn text_chunk(text: &str, id: &str) -> ContentChunk {
            ContentChunk::new(text.into()).message_id(MessageId::new(id))
        }

        fn agent_chunk(chat: &mut Chat, text: &str, id: &str) {
            chat.apply_update(SessionUpdate::AgentMessageChunk(text_chunk(text, id)));
        }

        fn model_option(current: &str) -> SessionConfigOption {
            SessionConfigOption::select(
                crate::MODEL_CONFIG_ID,
                "Model",
                SessionConfigValueId::new(current),
                vec![
                    SessionConfigSelectOption::new(
                        SessionConfigValueId::new("onde/qwen"),
                        "Qwen 3B - 32K [Onde]",
                    )
                    .description("tool calling - 32K context - download on select"),
                    SessionConfigSelectOption::new(
                        SessionConfigValueId::new("onde/small"),
                        "Small 1B - 8K [Onde]",
                    )
                    .description("8K context"),
                    SessionConfigSelectOption::new(
                        SessionConfigValueId::new("sigit-cloud:balanced"),
                        "Balanced - 200K",
                    )
                    .description("tool calling - 200K context"),
                ],
            )
        }

        // ── updates → transcript ──────────────────────────────────────────────

        #[test]
        fn chunks_with_one_message_id_are_one_message() {
            let mut chat = Chat::new();
            agent_chunk(&mut chat, "Hel", "m1");
            agent_chunk(&mut chat, "lo", "m1");
            chat.finalize_stream();
            assert_eq!(chat.messages.len(), 1);
            assert_eq!(chat.messages[0].role, Role::Assistant);
            assert_eq!(chat.messages[0].text, "Hello");
        }

        #[test]
        fn a_new_message_id_starts_a_new_message() {
            let mut chat = Chat::new();
            agent_chunk(&mut chat, "first", "m1");
            agent_chunk(&mut chat, "second", "m2");
            chat.finalize_stream();
            let texts: Vec<_> = chat.messages.iter().map(|m| m.text.as_str()).collect();
            assert_eq!(texts, ["first", "second"]);
        }

        #[test]
        fn thought_chunks_become_a_think_block_on_the_reply() {
            let mut chat = Chat::new();
            chat.apply_update(SessionUpdate::AgentThoughtChunk(text_chunk("hmm, ", "m1")));
            chat.apply_update(SessionUpdate::AgentThoughtChunk(text_chunk(
                "the file", "m1",
            )));
            agent_chunk(&mut chat, "Done.", "m1");
            chat.finalize_stream();
            assert_eq!(chat.messages.len(), 1);
            assert_eq!(chat.messages[0].text, "Done.");
            assert_eq!(
                chat.messages[0].think_block.as_deref(),
                Some("hmm, the file")
            );
        }

        #[test]
        fn reasoning_alone_keeps_the_thinking_spinner_up() {
            let mut chat = Chat::new();
            chat.apply_update(SessionUpdate::AgentThoughtChunk(text_chunk(
                "pondering",
                "m1",
            )));
            assert!(chat.thinking);
            assert!(chat.visible_stream().is_empty());
            agent_chunk(&mut chat, "Answer", "m1");
            assert!(!chat.thinking);
        }

        #[test]
        fn replayed_user_text_is_a_user_message() {
            let mut chat = Chat::new();
            chat.apply_update(SessionUpdate::UserMessageChunk(text_chunk("fix it", "u1")));
            assert_eq!(chat.messages.len(), 1);
            assert_eq!(chat.messages[0].role, Role::User);
            assert_eq!(chat.messages[0].text, "fix it");
        }

        #[test]
        fn a_tool_call_and_its_updates_meet_by_id() {
            let mut chat = Chat::new();
            agent_chunk(&mut chat, "Let me look.", "m1");
            chat.apply_update(SessionUpdate::ToolCall(
                ToolCall::new("call-1", "run_command · ls")
                    .status(ToolCallStatus::InProgress)
                    .raw_input(serde_json::json!({ "command": "ls" })),
            ));
            // The reply before the call is kept, the call follows it.
            assert_eq!(chat.messages.len(), 2);
            assert_eq!(chat.messages[1].role, Role::Tool);
            assert_eq!(chat.messages[1].status, Some(ToolCallStatus::InProgress));
            assert!(
                chat.messages[1]
                    .tool_detail
                    .as_deref()
                    .unwrap()
                    .contains("\"command\"")
            );

            chat.apply_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "call-1",
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .title("run_command · ls (done)")
                    .content(vec!["Cargo.toml\nsrc".into()]),
            )));
            assert_eq!(chat.messages.len(), 2);
            let entry = &chat.messages[1];
            assert_eq!(entry.status, Some(ToolCallStatus::Completed));
            assert_eq!(entry.text, "run_command · ls (done)");
            assert_eq!(entry.tool_result.as_deref(), Some("Cargo.toml\nsrc"));
        }

        #[test]
        fn an_update_for_an_unknown_call_is_dropped() {
            let mut chat = Chat::new();
            chat.apply_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "ghost",
                ToolCallUpdateFields::new().status(ToolCallStatus::Failed),
            )));
            assert!(chat.messages.is_empty());
        }

        #[test]
        fn a_diff_in_a_tool_update_shows_a_short_preview() {
            let diff = Diff::new("src/lib.rs", "let a = 2;\nlet b = 3;").old_text("let a = 1;");
            let preview = diff_preview(&diff);
            assert_eq!(
                preview,
                "src/lib.rs\n- let a = 1;\n+ let a = 2;\n+ let b = 3;"
            );

            let mut chat = Chat::new();
            chat.apply_update(SessionUpdate::ToolCall(ToolCall::new(
                "c",
                "edit_file · src/lib.rs",
            )));
            chat.apply_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "c",
                ToolCallUpdateFields::new().content(vec![ToolCallContent::Diff(diff)]),
            )));
            assert_eq!(
                chat.messages[0].tool_result.as_deref(),
                Some(preview.as_str())
            );
        }

        #[test]
        fn a_long_diff_is_cut_with_a_count() {
            let new_text = (1..=10)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            let preview = diff_preview(&Diff::new("big.txt", new_text));
            assert!(preview.contains("+ line 6"));
            assert!(!preview.contains("+ line 7"));
            assert!(preview.ends_with("+ … (+4 more lines)"));
        }

        #[test]
        fn the_plan_is_one_entry_replaced_in_place() {
            let plan = |status| {
                Plan::new(vec![
                    PlanEntry::new("write tests", PlanEntryPriority::High, status),
                    PlanEntry::new("ship", PlanEntryPriority::Low, PlanEntryStatus::Pending),
                ])
            };
            let mut chat = Chat::new();
            chat.apply_update(SessionUpdate::Plan(plan(PlanEntryStatus::Pending)));
            agent_chunk(&mut chat, "working", "m1");
            chat.apply_update(SessionUpdate::Plan(plan(PlanEntryStatus::Completed)));
            chat.finalize_stream();

            let plans: Vec<_> = chat
                .messages
                .iter()
                .filter(|m| m.role == Role::Plan)
                .collect();
            assert_eq!(plans.len(), 1);
            assert!(plans[0].text.contains("☑ write tests"));
            assert!(plans[0].text.contains("☐ ship"));
        }

        #[test]
        fn usage_drives_the_context_gauge() {
            let mut chat = Chat::new();
            chat.apply_update(SessionUpdate::UsageUpdate(UsageUpdate::new(1_200, 32_000)));
            assert_eq!(chat.context, Some((1_200, 32_000)));
        }

        #[test]
        fn config_options_drive_the_model_label_and_flags() {
            let mut chat = Chat::new();
            assert_eq!(chat.model_label(), "siGit Code");

            chat.config_options = vec![model_option("onde/qwen")];
            // On-device, and nothing has finished loading yet.
            assert_eq!(chat.model_label(), "Qwen 3B - 32K [Onde] (not loaded)");
            assert!(chat.tool_calling());
            assert!(!chat.on_cloud());

            chat.config_options = vec![model_option("onde/small")];
            assert!(!chat.tool_calling());

            chat.config_options = vec![model_option("sigit-cloud:balanced")];
            assert_eq!(chat.model_label(), "Balanced - 200K");
            assert!(chat.on_cloud());
        }

        #[test]
        fn a_finished_load_card_clears_not_loaded() {
            let mut chat = Chat::new();
            chat.config_options = vec![model_option("onde/qwen")];
            chat.apply_update(SessionUpdate::ToolCall(
                ToolCall::new("model-load-1", "Loading Qwen").status(ToolCallStatus::InProgress),
            ));
            assert!(chat.model_label().ends_with("(not loaded)"));
            chat.apply_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                "model-load-1",
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )));
            assert_eq!(chat.model_label(), "Qwen 3B - 32K [Onde]");
        }

        #[test]
        fn the_picker_groups_by_the_cloud_prefix_and_marks_the_current_row() {
            let rows = picker_rows(&[model_option("sigit-cloud:balanced")]);
            assert_eq!(rows.len(), 3);
            assert!(!rows[0].cloud && !rows[1].cloud && rows[2].cloud);
            assert_eq!(rows.iter().filter(|r| r.current).count(), 1);
            assert!(rows[2].current);
            assert!(rows[0].description.contains("download on select"));
        }

        #[test]
        fn a_session_switch_keeps_the_options_but_not_the_conversation() {
            let mut chat = Chat::new();
            chat.config_options = vec![model_option("onde/qwen")];
            chat.local_loaded = true;
            agent_chunk(&mut chat, "old", "m1");
            chat.finalize_stream();
            let old = chat.fresh_transcript();
            assert_eq!(old.messages.len(), 1);
            assert!(chat.messages.is_empty());
            assert!(chat.local_loaded);
            assert_eq!(chat.config_options.len(), 1);
        }

        // ── permission prompt ─────────────────────────────────────────────────

        fn four_options() -> Vec<PermissionOption> {
            vec![
                PermissionOption::new("once", "Allow once", PermissionOptionKind::AllowOnce),
                PermissionOption::new(
                    "always",
                    "Allow for session",
                    PermissionOptionKind::AllowAlways,
                ),
                PermissionOption::new("no", "Deny", PermissionOptionKind::RejectOnce),
                PermissionOption::new(
                    "never",
                    "Deny for session",
                    PermissionOptionKind::RejectAlways,
                ),
            ]
        }

        fn selected(outcome: &RequestPermissionOutcome) -> Option<&str> {
            match outcome {
                RequestPermissionOutcome::Selected(s) => Some(s.option_id.0.as_ref()),
                _ => None,
            }
        }

        #[test]
        fn keys_map_to_permission_kinds() {
            assert_eq!(
                permission_kind_for_key(KeyCode::Char('y')),
                Some(PermissionOptionKind::AllowOnce)
            );
            assert_eq!(
                permission_kind_for_key(KeyCode::Char('A')),
                Some(PermissionOptionKind::AllowAlways)
            );
            assert_eq!(
                permission_kind_for_key(KeyCode::Char('n')),
                Some(PermissionOptionKind::RejectOnce)
            );
            assert_eq!(
                permission_kind_for_key(KeyCode::Esc),
                Some(PermissionOptionKind::RejectOnce)
            );
            assert_eq!(
                permission_kind_for_key(KeyCode::Char('d')),
                Some(PermissionOptionKind::RejectAlways)
            );
            assert_eq!(permission_kind_for_key(KeyCode::Char('x')), None);
        }

        #[test]
        fn an_answer_selects_the_option_of_its_kind() {
            let options = four_options();
            for (kind, id) in [
                (PermissionOptionKind::AllowOnce, "once"),
                (PermissionOptionKind::AllowAlways, "always"),
                (PermissionOptionKind::RejectOnce, "no"),
                (PermissionOptionKind::RejectAlways, "never"),
            ] {
                assert_eq!(selected(&permission_outcome(&options, kind)), Some(id));
            }
        }

        #[test]
        fn a_missing_always_option_falls_back_to_the_single_use_one() {
            let options = vec![
                PermissionOption::new("once", "Allow", PermissionOptionKind::AllowOnce),
                PermissionOption::new("no", "Deny", PermissionOptionKind::RejectOnce),
            ];
            let always = permission_outcome(&options, PermissionOptionKind::AllowAlways);
            assert_eq!(selected(&always), Some("once"));
            let never = permission_outcome(&options, PermissionOptionKind::RejectAlways);
            assert_eq!(selected(&never), Some("no"));
            // Nothing to match at all is a cancellation.
            assert!(matches!(
                permission_outcome(&[], PermissionOptionKind::AllowOnce),
                RequestPermissionOutcome::Cancelled
            ));
        }

        #[test]
        fn the_permission_notice_names_the_call_and_shows_its_diff() {
            let diff = Diff::new("a.txt", "new").old_text("old");
            let request = RequestPermissionRequest::new(
                "s",
                ToolCallUpdate::new(
                    "c",
                    ToolCallUpdateFields::new()
                        .title("edit_file · a.txt")
                        .content(vec![ToolCallContent::Diff(diff)]),
                ),
                four_options(),
            );
            let notice = permission_notice(&request);
            assert!(notice.starts_with("⚠ permission — allow edit_file · a.txt?"));
            assert!(notice.contains("[y]es · [a]lways this session · [n]o · [d]eny this session"));
            assert!(notice.contains("- old\n+ new"));
        }

        // ── input routing ─────────────────────────────────────────────────────

        #[test]
        fn only_client_commands_are_handled_locally() {
            assert_eq!(parse_local("/exit"), Some(LocalCommand::Exit));
            assert_eq!(parse_local("/q"), Some(LocalCommand::Exit));
            assert_eq!(parse_local("/thinking"), Some(LocalCommand::Thinking(None)));
            assert_eq!(
                parse_local("/thinking on"),
                Some(LocalCommand::Thinking(Some(true)))
            );
            assert_eq!(
                parse_local("/tools off"),
                Some(LocalCommand::Tools(Some(false)))
            );
            assert_eq!(parse_local("/models"), Some(LocalCommand::Models));
            assert_eq!(parse_local("/resume"), Some(LocalCommand::Resume));
        }

        #[test]
        fn everything_else_goes_to_the_agent() {
            for forwarded in [
                "/help",
                "/clear",
                "/status",
                "/models 3",
                "/models list",
                "/local on",
                "/skills",
                "/agents",
                "/commands",
                "/mcp",
                "/load",
                "/login",
                "/logout",
                "/whoami",
                "/reload",
                "/plan",
                "/permissions",
                "/compact",
                "/init",
                "/git:commit fix",
                "hello there",
                "explain /exit",
            ] {
                assert_eq!(
                    parse_local(forwarded),
                    None,
                    "{forwarded} must be forwarded"
                );
            }
        }

        // ── turn ends ─────────────────────────────────────────────────────────

        #[test]
        fn only_unusual_stops_get_a_notice() {
            assert_eq!(stop_notice(StopReason::EndTurn), None);
            assert_eq!(stop_notice(StopReason::Cancelled), Some("(cancelled)"));
            for stop in [
                StopReason::MaxTokens,
                StopReason::MaxTurnRequests,
                StopReason::Refusal,
            ] {
                assert!(stop_notice(stop).is_some());
            }
        }

        #[test]
        fn errors_come_through_verbatim_with_a_login_hint_for_a_401() {
            let plain = AgentError {
                message: "quota exceeded".into(),
                auth_required: false,
            };
            assert_eq!(turn_error_text(&plain), "error: quota exceeded");

            let expired = AgentError {
                message: "HTTP 401: token expired".into(),
                auth_required: false,
            };
            assert!(turn_error_text(&expired).ends_with("sign in with /login"));

            let gate = AgentError {
                message: "Authentication required".into(),
                auth_required: true,
            };
            assert!(turn_error_text(&gate).contains("/login"));
        }

        // ── history rows ──────────────────────────────────────────────────────

        #[test]
        fn session_rows_show_title_time_and_id() {
            assert_eq!(
                session_when(Some("2026-10-18T10:15:30Z")),
                "2026-10-18 10:15"
            );
            assert_eq!(session_when(Some("today")), "today");
            assert_eq!(session_when(None), "time unknown");

            let mut info =
                agent_client_protocol::schema::v1::SessionInfo::new(SessionId::new("abc"), "/work");
            assert_eq!(session_row(&info), "abc  ·  time unknown");
            info.title = Some("Fix the build".into());
            info.updated_at = Some("2026-10-18T10:15:30Z".into());
            assert_eq!(
                session_row(&info),
                "Fix the build  ·  2026-10-18 10:15  ·  abc"
            );
        }

        #[test]
        fn permission_option_ids_are_opaque() {
            // The agent chooses the ids; only the kind matters to the client.
            let id = PermissionOptionId::new("anything");
            assert_eq!(id.0.as_ref(), "anything");
        }
    }
} // end mod tui

// re-export so callers write `chat::run_with(...)`
pub use tui::run_with;

// ── Tests (platform-agnostic) ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        RepoItem, TOOL_RESULT_PREVIEW_MAX, Tab, cap_output_preview, format_repo_detail,
        host_from_api_url, parse_repo_items, parse_repo_remote, parse_rich_text_segments,
        pretty_tool_arguments, repo_row, streaming_think_tail, strip_think_blocks, think_indicator,
        tool_entry_lines, tool_title, url_host,
    };

    // ── tool_title ────────────────────────────────────────────────────────────

    #[test]
    fn tool_title_run_command_shows_the_command() {
        assert_eq!(
            tool_title("run_command", r#"{"command":"cargo test"}"#),
            "run_command · cargo test"
        );
    }

    #[test]
    fn tool_title_file_tools_show_the_path() {
        for name in [
            "read_file",
            "create_file",
            "edit_file",
            "multi_edit",
            "delete_file",
        ] {
            assert_eq!(
                tool_title(name, r#"{"path":"src/main.rs","old_text":"a"}"#),
                format!("{name} · src/main.rs")
            );
        }
    }

    #[test]
    fn tool_title_pattern_tools_show_the_pattern() {
        assert_eq!(
            tool_title("search_files", r#"{"pattern":"fn main","path":"src"}"#),
            "search_files · fn main"
        );
        assert_eq!(
            tool_title("glob", r#"{"pattern":"**/*.rs"}"#),
            "glob · **/*.rs"
        );
    }

    #[test]
    fn tool_title_mcp_tools_show_server_and_bare_tool_name() {
        assert_eq!(
            tool_title("mcp__sigit__list_issues", r#"{"repo":"getsigit/sigit"}"#),
            "sigit · list_issues"
        );
    }

    #[test]
    fn tool_title_unknown_tool_is_the_name_alone() {
        assert_eq!(
            tool_title("write_todos", r#"{"todos":[{"text":"x"}]}"#),
            "write_todos"
        );
    }

    #[test]
    fn tool_title_malformed_json_falls_back_to_the_name() {
        assert_eq!(tool_title("run_command", "{not json"), "run_command");
        assert_eq!(tool_title("edit_file", ""), "edit_file");
    }

    #[test]
    fn tool_title_missing_or_non_string_arg_falls_back_to_the_name() {
        assert_eq!(tool_title("run_command", r#"{"other":"x"}"#), "run_command");
        assert_eq!(
            tool_title("run_command", r#"{"command":42}"#),
            "run_command"
        );
    }

    #[test]
    fn tool_title_truncates_long_summaries_with_an_ellipsis() {
        let long = "a".repeat(100);
        let title = tool_title("run_command", &format!(r#"{{"command":"{long}"}}"#));

        let summary = title.strip_prefix("run_command · ").expect("summary");
        assert_eq!(summary.chars().count(), 60);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn tool_title_collapses_multiline_commands_to_one_line() {
        assert_eq!(
            tool_title("run_command", "{\"command\":\"echo a &&\\n  echo b\"}"),
            "run_command · echo a && echo b"
        );
    }

    // ── cap_output_preview ────────────────────────────────────────────────────

    #[test]
    fn cap_output_preview_passes_short_output_through() {
        assert_eq!(cap_output_preview("hello\nworld"), "hello\nworld");
    }

    #[test]
    fn cap_output_preview_keeps_the_tail_and_notes_the_truncation() {
        let output = format!("{}{}", "x".repeat(3000), "the-very-end");
        let preview = cap_output_preview(&output);

        assert!(preview.starts_with("… (truncated — showing the last 2000 of 3012 chars)\n"));
        assert!(preview.ends_with("the-very-end"));
        let (_note, tail) = preview.split_once('\n').expect("note line");
        assert_eq!(tail.chars().count(), TOOL_RESULT_PREVIEW_MAX);
    }

    // ── pretty_tool_arguments ─────────────────────────────────────────────────

    #[test]
    fn pretty_tool_arguments_pretty_prints_valid_json() {
        assert_eq!(
            pretty_tool_arguments(r#"{"command":"ls"}"#),
            "{\n  \"command\": \"ls\"\n}"
        );
    }

    #[test]
    fn pretty_tool_arguments_shows_malformed_json_raw() {
        assert_eq!(pretty_tool_arguments("{oops"), "{oops");
    }

    // ── tool_entry_lines ──────────────────────────────────────────────────────

    #[test]
    fn tool_entry_lines_collapsed_is_a_single_title_line() {
        let lines = tool_entry_lines(
            "run_command · cargo test",
            "{\n  \"command\": \"cargo test\"\n}",
            Some("ok"),
            false,
        );
        assert_eq!(lines, vec!["▸ 🔧 run_command · cargo test"]);
    }

    #[test]
    fn tool_entry_lines_expanded_shows_detail_and_result() {
        let lines = tool_entry_lines(
            "run_command · cargo test",
            "{\n  \"command\": \"cargo test\"\n}",
            Some("test ok\ndone"),
            true,
        );
        assert_eq!(
            lines,
            vec![
                "▾ 🔧 run_command · cargo test",
                "    {",
                "      \"command\": \"cargo test\"",
                "    }",
                "    result:",
                "      test ok",
                "      done",
            ]
        );
    }

    #[test]
    fn tool_entry_lines_expanded_without_result_omits_the_result_block() {
        let lines = tool_entry_lines("glob · **/*.rs", "{}", None, true);
        assert_eq!(lines, vec!["▾ 🔧 glob · **/*.rs", "    {}"]);
    }

    // ── Tab ────────────────────────────────────────

    #[test]
    fn tab_next_without_repo_cycles_three_tabs() {
        assert_eq!(Tab::Session.next(false), Tab::History);
        assert_eq!(Tab::History.next(false), Tab::Cloud);
        assert_eq!(Tab::Cloud.next(false), Tab::Session);
        // Three hops return to the start — exactly the base three-tab cycle.
        assert_eq!(
            Tab::Session.next(false).next(false).next(false),
            Tab::Session
        );
    }

    #[test]
    fn tab_next_with_repo_cycles_four_tabs() {
        assert_eq!(Tab::Session.next(true), Tab::History);
        assert_eq!(Tab::History.next(true), Tab::Repo);
        assert_eq!(Tab::Repo.next(true), Tab::Cloud);
        assert_eq!(Tab::Cloud.next(true), Tab::Session);
    }

    #[test]
    fn tab_index_matches_titles_order() {
        for repo_visible in [false, true] {
            let titles = Tab::titles(repo_visible);
            assert_eq!(titles[Tab::Session.index(repo_visible)], "Session");
            assert_eq!(titles[Tab::History.index(repo_visible)], "History");
            assert_eq!(titles[Tab::Cloud.index(repo_visible)], "Cloud");
        }
        assert_eq!(Tab::titles(true)[Tab::Repo.index(true)], "Repo");
    }

    #[test]
    fn tab_titles_hide_repo_when_undetected() {
        assert_eq!(Tab::titles(false), vec!["Session", "History", "Cloud"]);
        assert_eq!(
            Tab::titles(true),
            vec!["Session", "History", "Repo", "Cloud"]
        );
    }

    // ── Repo remote detection ─────────────────────────────────────────────────

    #[test]
    fn parse_repo_remote_accepts_ssh_forms() {
        assert_eq!(
            parse_repo_remote("git@sigit.si:acme/demo.git", "sigit.si"),
            Some("acme/demo".to_string())
        );
        // bare (no .git) works too
        assert_eq!(
            parse_repo_remote("git@sigit.si:acme/demo", "sigit.si"),
            Some("acme/demo".to_string())
        );
    }

    #[test]
    fn parse_repo_remote_accepts_https_forms() {
        assert_eq!(
            parse_repo_remote("https://sigit.si/acme/demo.git", "sigit.si"),
            Some("acme/demo".to_string())
        );
        assert_eq!(
            parse_repo_remote("https://sigit.si/acme/demo", "sigit.si"),
            Some("acme/demo".to_string())
        );
        // trailing slash and http + port (dev instances) normalize too
        assert_eq!(
            parse_repo_remote("https://sigit.si/acme/demo/", "sigit.si"),
            Some("acme/demo".to_string())
        );
        assert_eq!(
            parse_repo_remote("http://127.0.0.1:8088/acme/demo.git", "127.0.0.1"),
            Some("acme/demo".to_string())
        );
    }

    #[test]
    fn parse_repo_remote_rejects_other_hosts() {
        assert_eq!(
            parse_repo_remote("git@github.com:acme/demo.git", "sigit.si"),
            None
        );
        assert_eq!(
            parse_repo_remote("https://github.com/acme/demo.git", "sigit.si"),
            None
        );
    }

    #[test]
    fn parse_repo_remote_rejects_garbage_and_bad_paths() {
        assert_eq!(parse_repo_remote("", "sigit.si"), None);
        assert_eq!(parse_repo_remote("not a url at all", "sigit.si"), None);
        assert_eq!(parse_repo_remote("/local/path/repo.git", "sigit.si"), None);
        // wrong number of path segments
        assert_eq!(
            parse_repo_remote("https://sigit.si/demo.git", "sigit.si"),
            None
        );
        assert_eq!(
            parse_repo_remote("https://sigit.si/a/b/c.git", "sigit.si"),
            None
        );
        assert_eq!(
            parse_repo_remote("git@sigit.si:/demo.git", "sigit.si"),
            None
        );
    }

    #[test]
    fn url_host_extracts_the_hostname() {
        assert_eq!(url_host("https://sigit.si"), Some("sigit.si".to_string()));
        assert_eq!(
            url_host("http://localhost:8088/api/v1"),
            Some("localhost".to_string())
        );
        assert_eq!(
            url_host("https://SiGit.SI/path"),
            Some("sigit.si".to_string())
        );
        assert_eq!(url_host("ftp://sigit.si"), None);
        assert_eq!(url_host(""), None);
    }

    #[test]
    fn host_from_api_url_defaults_to_production() {
        assert_eq!(host_from_api_url(None), "sigit.si");
        assert_eq!(host_from_api_url(Some("nonsense")), "sigit.si");
        assert_eq!(
            host_from_api_url(Some("http://127.0.0.1:8088")),
            "127.0.0.1"
        );
    }

    // ── Repo tab data parsing ─────────────────────────────────────────────────

    #[test]
    fn parse_repo_items_reads_a_top_level_array() {
        let text = r#"[
            {"number": 12, "title": "Fix the flux capacitor", "state": "open"},
            {"number": 7, "title": "Old bug", "state": "closed"}
        ]"#;
        let items = parse_repo_items(text).expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].number, 12);
        assert_eq!(items[0].title, "Fix the flux capacitor");
        assert_eq!(items[0].state, "open");
    }

    #[test]
    fn parse_repo_items_reads_wrapped_arrays_and_skips_malformed_entries() {
        let text = r#"{"issues": [
            {"number": 1, "title": "ok", "state": "open"},
            {"title": "no number"},
            {"number": 2, "title": "stateless"}
        ]}"#;
        let items = parse_repo_items(text).expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].state, "");
    }

    #[test]
    fn parse_repo_items_passes_error_text_through() {
        // `mcp::call_tool` returns error strings, not JSON — the tab shows them.
        let err = parse_repo_items("Error: MCP is not initialized.").unwrap_err();
        assert_eq!(err, "Error: MCP is not initialized.");
        // JSON that isn't a list shape is surfaced verbatim too.
        assert!(parse_repo_items("\"unexpected\"").is_err());
    }

    #[test]
    fn repo_row_formats_number_state_title() {
        let item = RepoItem {
            number: 12,
            title: "Fix the flux capacitor".to_string(),
            state: "open".to_string(),
        };
        assert_eq!(repo_row(&item), "#12  [open]  Fix the flux capacitor");
        let stateless = RepoItem {
            number: 3,
            title: "t".to_string(),
            state: String::new(),
        };
        assert_eq!(repo_row(&stateless), "#3  t");
    }

    #[test]
    fn format_repo_detail_pretty_prints_json_and_passes_text_through() {
        let pretty = format_repo_detail(r#"{"number":1,"title":"x"}"#);
        assert!(
            pretty.contains("\n"),
            "expected pretty-printed JSON: {pretty}"
        );
        assert_eq!(
            format_repo_detail("Error: server 'sigit' returned 401"),
            "Error: server 'sigit' returned 401"
        );
    }

    #[test]
    fn strip_think_blocks_separates_thinking_and_visible_reply() {
        let raw = "<think>I should inspect the code first.</think>Here is the fix.";
        let (thinking, visible) = strip_think_blocks(raw);

        assert_eq!(thinking, "I should inspect the code first.");
        assert_eq!(visible, "Here is the fix.");
    }

    #[test]
    fn strip_think_blocks_handles_unclosed_think_block() {
        let raw = "<think>I am still reasoning about the bug";
        let (thinking, visible) = strip_think_blocks(raw);

        assert_eq!(thinking, "I am still reasoning about the bug");
        assert_eq!(visible, "");
    }

    #[test]
    fn strip_think_blocks_leaves_plain_text_untouched() {
        let raw = "No hidden reasoning here.";
        let (thinking, visible) = strip_think_blocks(raw);

        assert_eq!(thinking, "");
        assert_eq!(visible, "No hidden reasoning here.");
    }

    #[test]
    fn streaming_think_tail_shows_last_lines_of_unclosed_think_buffer() {
        // the common mid-stream shape: `<think>` opened, not yet closed
        let raw = "<think>First thought.\nSecond thought.\nThird thought.\nFourth thought.";
        let tail = streaming_think_tail(raw, 40, 3);

        assert_eq!(
            tail,
            vec!["Second thought.", "Third thought.", "Fourth thought."]
        );
    }

    #[test]
    fn streaming_think_tail_wraps_long_lines_at_width() {
        let raw = "<think>one two three four five six seven";
        let tail = streaming_think_tail(raw, 10, 3);

        // wrapped at 10 cols: ["one two", "three four", "five six", "seven"];
        // the tail keeps only the last 3
        assert_eq!(tail, vec!["three four", "five six", "seven"]);
    }

    #[test]
    fn streaming_think_tail_is_empty_once_visible_text_arrives() {
        let raw = "<think>hidden reasoning</think>The answer is 42.";
        assert!(streaming_think_tail(raw, 40, 3).is_empty());
    }

    #[test]
    fn streaming_think_tail_is_empty_for_only_think_closed_but_no_visible_yet() {
        // closed think, visible not started: still show the reasoning tail
        let raw = "<think>done reasoning</think>";
        assert_eq!(streaming_think_tail(raw, 40, 3), vec!["done reasoning"]);
    }

    #[test]
    fn streaming_think_tail_is_empty_for_empty_or_plain_buffers() {
        assert!(streaming_think_tail("", 40, 3).is_empty());
        assert!(streaming_think_tail("plain visible text", 40, 3).is_empty());
        assert!(streaming_think_tail("<think>", 40, 3).is_empty());
    }

    #[test]
    fn streaming_think_tail_hard_splits_words_wider_than_the_wrap_column() {
        let raw = "<think>abcdefghijkl";
        assert_eq!(
            streaming_think_tail(raw, 5, 3),
            vec!["abcde", "fghij", "kl"]
        );
    }

    #[test]
    fn think_indicator_counts_lines_and_pluralizes() {
        assert_eq!(
            think_indicator("only one line of reasoning"),
            "· thought for a bit (1 line) — /thinking to show"
        );
        assert_eq!(
            think_indicator("a\nb\nc"),
            "· thought for a bit (3 lines) — /thinking to show"
        );
    }

    #[test]
    fn parse_rich_text_segments_marks_bold_runs() {
        let segments = parse_rich_text_segments(
            "The current weather is **72°F** with **Partly Cloudy** conditions.",
        );

        assert_eq!(
            segments,
            vec![
                ("The current weather is ".to_string(), false),
                ("72°F".to_string(), true),
                (" with ".to_string(), false),
                ("Partly Cloudy".to_string(), true),
                (" conditions.".to_string(), false),
            ]
        );
    }

    #[test]
    fn parse_rich_text_segments_treats_unclosed_marker_as_bold_to_end() {
        let segments = parse_rich_text_segments("Prefix **bold");

        assert_eq!(
            segments,
            vec![("Prefix ".to_string(), false), ("bold".to_string(), true),]
        );
    }
}
