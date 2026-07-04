//! Full-screen terminal chat UI.
//!
//! Two phases: a loading spinner while the model initializes, then
//! interactive chat. Uses `tokio::select!` to multiplex terminal events
//! with streaming LLM tokens.

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

// ── Unix-only TUI ─────────────────────────────────────────────────────────────
//
// macOS + Linux only. Windows uses ACP mode instead.

#[cfg(unix)]
mod tui {
    use std::future::pending;
    use std::sync::Arc;
    use std::sync::mpsc as std_mpsc;

    use anyhow::Result;
    use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use futures::StreamExt;
    use onde::inference::{ChatEngine, SamplingConfig};

    use crate::backend::{InferenceBackend, LocalBackend, OpenAiBackend, ToolResult, ToolSpec};
    use crate::models::{
        InferenceKind, ModelCacheHealth, ModelPickerItem, ModelSource, build_model_picker_items,
    };
    use ratatui::{
        Frame,
        layout::{Constraint, Layout, Position},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Clear, Paragraph, Wrap},
    };
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::{Duration, Instant, interval};

    // ── Message types ─────────────────────────────────────────────────────────

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Role {
        User,
        Assistant,
        System,
        /// rainbow-colored banner art
        Banner,
    }

    struct ChatMessage {
        role: Role,
        text: String,
        /// Qwen 3 reasoning extracted from `<think>` tags, if any.
        think_block: Option<String>,
    }

    impl ChatMessage {
        fn user(text: impl Into<String>) -> Self {
            Self {
                role: Role::User,
                text: text.into(),
                think_block: None,
            }
        }

        fn assistant(text: impl Into<String>) -> Self {
            let raw = text.into();
            let (think, visible) = super::strip_think_blocks(&raw);
            Self {
                role: Role::Assistant,
                text: visible,
                think_block: if think.is_empty() { None } else { Some(think) },
            }
        }

        fn system(text: impl Into<String>) -> Self {
            Self {
                role: Role::System,
                text: text.into(),
                think_block: None,
            }
        }

        fn banner(text: impl Into<String>) -> Self {
            Self {
                role: Role::Banner,
                text: text.into(),
                think_block: None,
            }
        }
    }

    // ── Inference updates from background task ────────────────────────────────

    enum InferenceUpdate {
        /// show tool name in chat while it runs
        ToolUse(String),
        /// a streamed token fragment of the assistant's reply
        Delta(String),
        /// the streamed reply is complete; commit the accumulated buffer
        StreamEnd,
        /// a complete (non-streamed) assistant reply
        Response(String),
        Error(String),
        /// the inference task wants to run a mutating tool and is paused on
        /// `reply`; the user answers with y (once) / a (session) / n (deny)
        ApprovalRequest {
            tool: String,
            /// arguments preview so the user can see what they are approving
            args: String,
            reply: oneshot::Sender<ApprovalChoice>,
        },
    }

    /// The user's answer to a tool-approval prompt. Dropping the reply channel
    /// (quit, cancel) counts as a denial on the inference side.
    enum ApprovalChoice {
        /// run this one call
        Once,
        /// run it and stop asking for this tool for the rest of the session
        Session,
        /// skip the call; the model gets an explanatory tool result
        Deny,
    }

    enum ModelLoadUpdate {
        Loaded(String),
        Error(String),
    }

    // ── App state ─────────────────────────────────────────────────────────────

    struct App {
        messages: Vec<ChatMessage>,
        input: String,
        cursor: usize,
        /// true while assistant tokens are streaming into `stream_buf`
        streaming: bool,
        stream_buf: String,
        inference_rx: Option<mpsc::Receiver<InferenceUpdate>>,
        model_load_rx: Option<mpsc::Receiver<ModelLoadUpdate>>,
        /// a tool call waiting on the user's y/a/n answer; the inference task is
        /// paused on the other end of the channel
        pending_approval: Option<(String, oneshot::Sender<ApprovalChoice>)>,
        thinking: bool,
        thinking_tick: u8,
        quit: bool,
        /// toggled periodically so the streaming cursor blinks
        blink_on: bool,
        blink_counter: u8,
        switching_model: bool,
        /// stashed until ModelLoadUpdate::Loaded applies it to `app.tool_calling`
        pending_tool_calling: Option<bool>,
        /// suppresses the spurious "disconnected" error when we drop model_load_rx on cancel
        model_load_cancelled: bool,

        // ── Loading-phase state ───────────────────────────────────────────────
        is_loading: bool,
        load_tick: u32,
        /// keeps the loading view visible so the user can read the error
        load_error: Option<String>,
        load_start: Instant,
        load_model_name: String,

        // ── Model picker state ────────────────────────────────────────────────
        show_model_picker: bool,
        model_picker_index: usize,
        model_picker_items: Vec<ModelPickerItem>,
        current_model_name: String,
        tool_calling: bool,

        // ── Model-switch download progress ────────────────────────────────────
        switching_model_id: Option<String>,
        /// (downloaded, expected) bytes — polled every tick during a model switch
        download_progress: Option<(u64, u64)>,

        // ── Active inference backend ──────────────────────────────────────────
        /// The backend serving inference. Swapped in place when the user picks a
        /// different model or cloud tier via `/models`.
        backend: Arc<dyn InferenceBackend>,
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
        fn new(load_model_name: String, backend: Arc<dyn InferenceBackend>) -> Self {
            let is_remote = backend.is_remote();
            let items = build_model_picker_items();
            let tool_calling = items
                .iter()
                .find(|m| m.display_name == load_model_name)
                .map(|m| m.tool_calling)
                .unwrap_or(true);
            // For a remote provider the passed-in name is authoritative; the
            // persisted local selection must not override it (or the title would
            // show an on-device model while requests go to the cloud).
            let current_model_name = if is_remote {
                load_model_name.clone()
            } else {
                crate::setup::load_selected_model_name().unwrap_or_else(|| load_model_name.clone())
            };
            Self {
                messages: Vec::new(),
                input: String::new(),
                cursor: 0,
                streaming: false,
                stream_buf: String::new(),
                inference_rx: None,
                model_load_rx: None,
                pending_approval: None,
                thinking: false,
                thinking_tick: 0,
                quit: false,
                blink_on: true,
                blink_counter: 0,
                switching_model: false,
                pending_tool_calling: None,
                model_load_cancelled: false,
                switching_model_id: None,
                download_progress: None,
                is_loading: true,
                load_tick: 0,
                load_error: None,
                load_start: Instant::now(),
                load_model_name: load_model_name.clone(),
                show_model_picker: false,
                model_picker_index: 0,
                model_picker_items: items,
                current_model_name,
                tool_calling,
                backend,
            }
        }

        fn is_busy(&self) -> bool {
            self.is_streaming() || self.thinking || self.switching_model
        }

        fn switching_frame(&self) -> &'static str {
            let idx = (self.thinking_tick as usize) % THINKING_FRAMES.len();
            THINKING_FRAMES[idx]
        }

        fn is_streaming(&self) -> bool {
            self.streaming
        }

        fn finalize_stream(&mut self) {
            self.streaming = false;
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
            // live reply. Don't call stop_thinking() — that drops the channel.
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

        fn start_thinking(&mut self) {
            self.thinking = true;
            self.thinking_tick = 0;
        }

        fn stop_thinking(&mut self) {
            self.thinking = false;
            self.inference_rx = None;
            // Dropping a pending reply channel reads as a denial on the
            // inference side, so a cancelled turn can't leave a tool waiting.
            self.pending_approval = None;
        }

        fn tick_thinking(&mut self) {
            self.thinking_tick = self.thinking_tick.wrapping_add(1);
        }

        fn thinking_frame(&self) -> &'static str {
            let idx = (self.thinking_tick as usize) % THINKING_FRAMES.len();
            THINKING_FRAMES[idx]
        }

        fn tick(&mut self) {
            self.load_tick = self.load_tick.wrapping_add(1);
        }

        /// check how much of the model has landed on disk so far
        fn poll_download_progress(&mut self) {
            let Some(ref model_id) = self.switching_model_id else {
                return;
            };
            let cache_path = onde::hf_cache::model_cache_path(model_id);
            let downloaded = cache_path
                .as_ref()
                .filter(|p| p.exists())
                .map(|p| dir_size_recursive(p))
                .unwrap_or(0);
            let expected = onde::inference::models::SUPPORTED_MODEL_INFO
                .iter()
                .find(|m| m.id == model_id.as_str())
                .map(|m| m.expected_size_bytes)
                .unwrap_or(0);
            self.download_progress = Some((downloaded, expected));
        }

        /// switch to chat phase and show the welcome banner
        fn finish_loading(&mut self) {
            self.is_loading = false;
            for line in BANNER_ART.lines() {
                self.messages.push(ChatMessage::banner(line));
            }
            self.messages.push(ChatMessage::system(""));
            self.messages.push(ChatMessage::system(
                "In this world, nothing can be said to be certain, except death and taxes. ~ Pak Sigit",
            ));
            if self.backend.is_remote() {
                self.messages.push(ChatMessage::system(format!(
                    "Current model: {}",
                    self.current_model_name
                )));
            } else {
                // On-device models are never loaded implicitly; prompt the user to
                // load one explicitly before their first message.
                self.messages.push(ChatMessage::system(format!(
                    "No on-device model loaded. Run /load to load {}, or /models to choose one.",
                    self.current_model_name
                )));
            }
            self.messages
                .push(ChatMessage::system("Type /help for commands."));
        }

        /// store the error but stay in loading view so the user can read it
        fn set_load_error(&mut self, error: String) {
            self.load_error = Some(error);
            // is_loading stays true so render_loading() keeps rendering.
        }

        fn open_model_picker(&mut self, engine: &ChatEngine) {
            let current = crate::setup::load_selected_model();
            let current_name = crate::setup::load_selected_model_name().unwrap_or_else(|| {
                futures::executor::block_on(engine.info())
                    .model_name
                    .unwrap_or_else(|| self.current_model_name.clone())
            });

            self.model_picker_items = build_model_picker_items();
            self.model_picker_index = current
                .as_ref()
                .and_then(|selected| {
                    self.model_picker_items.iter().position(|item| {
                        item.config.model_id == selected.model_id
                            && item
                                .config
                                .files
                                .iter()
                                .any(|file| file == &selected.gguf_file)
                    })
                })
                .or_else(|| {
                    self.model_picker_items
                        .iter()
                        .position(|item| item.display_name == current_name)
                })
                .unwrap_or(0);
            self.show_model_picker = true;
        }

        fn close_model_picker(&mut self) {
            self.show_model_picker = false;
        }

        fn move_model_picker_up(&mut self) {
            if self.model_picker_items.is_empty() {
                return;
            }
            if self.model_picker_index == 0 {
                self.model_picker_index = self.model_picker_items.len().saturating_sub(1);
            } else {
                self.model_picker_index -= 1;
            }
        }

        fn move_model_picker_down(&mut self) {
            if self.model_picker_items.is_empty() {
                return;
            }
            self.model_picker_index = (self.model_picker_index + 1) % self.model_picker_items.len();
        }
    }

    // ── Model picker ─────────────────────────────────────────────────────────
    //
    // picker data types live in crate::models so Windows (ACP-only) can use them too

    fn render_model_picker(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
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

        let active_kind = crate::models::active_inference_kind();
        let mut lines = Vec::new();

        // State banner: which mode is active, and how to flip it.
        let (state_word, state_style) = match active_kind {
            InferenceKind::Local => (
                "ON  (on-device)",
                Style::default().fg(Color::Green).bg(Color::Black),
            ),
            InferenceKind::Cloud => (
                "OFF (siGit Code Cloud)",
                Style::default().fg(Color::Magenta).bg(Color::Black),
            ),
        };
        lines.push(Line::from(vec![
            Span::styled(
                "Local inference: ",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(state_word, state_style.add_modifier(Modifier::BOLD)),
            Span::styled(
                "    toggle with /local on|off",
                Style::default().fg(Color::DarkGray).bg(Color::Black),
            ),
        ]));
        lines.push(Line::from("").style(Style::default().bg(Color::Black)));

        let mut last_section: Option<ModelSource> = None;
        let mut last_kind: Option<InferenceKind> = None;

        for (index, item) in app.model_picker_items.iter().enumerate() {
            let item_kind = item.source.kind();
            let item_active = item_kind == active_kind;

            // Top-level group header (Local / Cloud) whenever the nature changes.
            if last_kind != Some(item_kind) {
                if last_kind.is_some() {
                    lines.push(Line::from("").style(Style::default().bg(Color::Black)));
                }
                let group_label = match item_kind {
                    InferenceKind::Local => "LOCAL — on-device inference",
                    InferenceKind::Cloud => "CLOUD — siGit Code Cloud",
                };
                let group_style = if item_active {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Black)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(Color::DarkGray).bg(Color::Black)
                };
                lines.push(
                    Line::from(vec![Span::styled(group_label, group_style)])
                        .style(Style::default().bg(Color::Black)),
                );
                last_kind = Some(item_kind);
                last_section = None;
            }

            if last_section != Some(item.source) {
                if last_section.is_some() {
                    lines.push(Line::from("").style(Style::default().bg(Color::Black)));
                }

                let (section_mark, section_name, section_style) = match item.source {
                    ModelSource::Onde => (
                        "◉",
                        "Onde Inference",
                        Style::default()
                            .fg(Color::Green)
                            .bg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    ),
                    ModelSource::HuggingFace => (
                        "○",
                        "Hugging Face cache",
                        Style::default()
                            .fg(Color::Cyan)
                            .bg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    ),
                    ModelSource::Available => (
                        "↓",
                        "Available for download",
                        Style::default()
                            .fg(Color::Blue)
                            .bg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    ),
                    ModelSource::Fallback => (
                        "◎",
                        "Fallback",
                        Style::default()
                            .fg(Color::Yellow)
                            .bg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    ),
                    ModelSource::Cloud => (
                        "☁",
                        "siGit Code Cloud",
                        Style::default()
                            .fg(Color::Magenta)
                            .bg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    ),
                };

                // Dim the section header when it belongs to the inactive group.
                let section_style = if item_active {
                    section_style
                } else {
                    Style::default().fg(Color::DarkGray).bg(Color::Black)
                };

                lines.push(
                    Line::from(vec![
                        Span::styled(format!("  {section_mark} "), section_style),
                        Span::styled(section_name, section_style),
                    ])
                    .style(Style::default().bg(Color::Black)),
                );
                last_section = Some(item.source);
            }

            let selected = index == app.model_picker_index;
            let current = item.display_name == app.current_model_name;
            let marker = if selected { "› " } else { "  " };
            let tool_badge = if item.tool_calling {
                "  ✓ tool calling"
            } else {
                ""
            };
            let health_badge = match item.cache_health {
                ModelCacheHealth::Complete => "",
                ModelCacheHealth::Incomplete => "  ! incomplete cache",
                ModelCacheHealth::NotDownloaded => "  ↓ download",
            };
            let current_badge = if current { "  ← current" } else { "" };
            let disabled_badge = match item.cache_health {
                ModelCacheHealth::Complete | ModelCacheHealth::NotDownloaded => "",
                ModelCacheHealth::Incomplete => "  (unselectable)",
            };
            let brand_mark = match item.source {
                ModelSource::Onde => "◉",
                ModelSource::HuggingFace => "○",
                ModelSource::Available => "↓",
                ModelSource::Fallback => "◎",
                ModelSource::Cloud => "☁",
            };
            let source = format!("  [{} {}]", brand_mark, item.source_label);

            let base_style = if selected {
                Style::default().fg(Color::Black).bg(Color::Green)
            } else if item_active {
                Style::default().fg(Color::White).bg(Color::Black)
            } else {
                // Inactive group: still visible (we surface the offering) but dimmed.
                Style::default().fg(Color::DarkGray).bg(Color::Black)
            };

            let source_style = if selected {
                Style::default().fg(Color::Black).bg(Color::Green)
            } else if !item_active {
                Style::default().fg(Color::DarkGray).bg(Color::Black)
            } else {
                match item.source {
                    ModelSource::Onde => Style::default().fg(Color::Green).bg(Color::Black),
                    ModelSource::HuggingFace => Style::default().fg(Color::Cyan).bg(Color::Black),
                    ModelSource::Available => Style::default().fg(Color::Blue).bg(Color::Black),
                    ModelSource::Fallback => Style::default().fg(Color::Yellow).bg(Color::Black),
                    ModelSource::Cloud => Style::default().fg(Color::Magenta).bg(Color::Black),
                }
            };

            let health_style = if selected {
                Style::default().fg(Color::Red).bg(Color::Green)
            } else {
                Style::default().fg(Color::Red).bg(Color::Black)
            };

            lines.push(Line::from(vec![
                Span::styled(
                    format!("{marker}{}  {}", item.display_name, item.description),
                    base_style,
                ),
                Span::styled(
                    tool_badge.to_string(),
                    if selected {
                        Style::default().fg(Color::Black).bg(Color::Green)
                    } else {
                        Style::default().fg(Color::Green).bg(Color::Black)
                    },
                ),
                Span::styled(health_badge.to_string(), health_style),
                Span::styled(
                    disabled_badge.to_string(),
                    if selected {
                        Style::default().fg(Color::Black).bg(Color::Green)
                    } else {
                        Style::default().fg(Color::DarkGray).bg(Color::Black)
                    },
                ),
                Span::styled(
                    current_badge.to_string(),
                    if selected {
                        Style::default().fg(Color::Black).bg(Color::Green)
                    } else {
                        Style::default().fg(Color::Cyan).bg(Color::Black)
                    },
                ),
                Span::styled(source, source_style),
            ]));
        }

        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(Color::Black)),
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

    // ── Slash commands ────────────────────────────────────────────────────────

    enum SlashCommand {
        Help,
        Clear,
        Status,
        /// picker UI, or jump straight to model N
        Models(Option<usize>),
        /// toggle on-device inference mode. `Some(true/false)` sets it, `None` flips it.
        Local(Option<bool>),
        /// List discovered Agent Skills.
        Skills,
        /// List configured MCP servers and their tools.
        Mcp,
        /// explicitly load the selected (or default) on-device model
        Load,
        /// `/login <email> <password>` — the raw argument, parsed when executed.
        Login(Option<String>),
        Logout,
        Whoami,
        /// Toggle plan mode (research only; mutating tools are denied with a
        /// prompt to present a plan). `Some(true/false)` sets it, `None` flips it.
        Plan(Option<bool>),
        /// Show the effective permission policy for this session.
        Permissions,
        /// Summarize-and-shrink the conversation history on demand.
        Compact,
        /// Restore the saved TUI session from disk.
        Resume,
        Exit,
        Unknown(String),
    }

    fn parse_slash(input: &str) -> Option<SlashCommand> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().map(|s| s.trim());
        Some(match cmd {
            "/help" => SlashCommand::Help,
            "/clear" => SlashCommand::Clear,
            "/status" => SlashCommand::Status,
            "/models" => SlashCommand::Models(arg.and_then(|s| s.parse::<usize>().ok())),
            "/local" => SlashCommand::Local(parse_on_off(arg)),
            "/skills" => SlashCommand::Skills,
            "/mcp" => SlashCommand::Mcp,
            "/load" => SlashCommand::Load,
            "/login" => SlashCommand::Login(arg.map(str::to_string)),
            "/logout" => SlashCommand::Logout,
            "/whoami" => SlashCommand::Whoami,
            "/plan" => SlashCommand::Plan(parse_on_off(arg)),
            "/permissions" => SlashCommand::Permissions,
            "/compact" => SlashCommand::Compact,
            "/resume" => SlashCommand::Resume,
            "/exit" | "/quit" | "/q" => SlashCommand::Exit,
            other => SlashCommand::Unknown(other.to_string()),
        })
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

    // ── Rendering ─────────────────────────────────────────────────────────────

    fn render(frame: &mut Frame, app: &mut App) {
        let area = frame.area();

        if app.is_loading {
            let zones = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
            render_loading_title(frame, app, zones[0]);
            render_loading(frame, app, zones[1]);
            render_loading_footer(frame, zones[2]);
            return;
        }

        let zones = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

        render_title(frame, app, zones[0]);
        render_messages(frame, app, zones[1]);
        render_input(frame, app, zones[2]);
        render_footer(frame, app, zones[3]);

        if app.show_model_picker {
            render_model_picker(frame, app, area);
        }
    }

    fn render_title(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let model_label = format!(" siGit — {} ", app.current_model_name);
        let tool_label = if app.tool_calling {
            " [tools on] "
        } else {
            " [tools off] "
        };
        let line = Line::from(vec![
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
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_loading_title(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        const SPINNER: &[&str] = &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
        let spin = SPINNER[(app.load_tick as usize) % SPINNER.len()];
        let label = format!(" siGit {} loading {}… ", spin, app.load_model_name);
        let line = Line::from(Span::styled(
            label,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_loading(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
        let elapsed = app.load_start.elapsed().as_secs();
        let elapsed_str = if elapsed < 60 {
            format!("{}s", elapsed)
        } else {
            format!("{}m {}s", elapsed / 60, elapsed % 60)
        };

        let content = if let Some(ref err) = app.load_error {
            format!(
                "\n\n  ✗ Failed to load model after {}.\n\n  {}\n\n  Press Ctrl+C to exit.",
                elapsed_str, err
            )
        } else {
            format!(
                "\n\n  Loading model, please wait… ({})\n\n  The model is being initialised. This may take a moment on first run.",
                elapsed_str
            )
        };

        let style = if app.load_error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::White)
        };

        frame.render_widget(
            Paragraph::new(content)
                .style(style)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_loading_footer(frame: &mut Frame, area: ratatui::layout::Rect) {
        let line = Line::from(vec![
            Span::styled(" Ctrl+C ", Style::default().fg(Color::Black).bg(Color::Red)),
            Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
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

        for msg in &app.messages {
            render_chat_message(&mut lines, msg, inner_width as usize);
        }

        let streamed_visible = app.visible_stream();
        if !streamed_visible.is_empty() {
            let fake = ChatMessage {
                role: Role::Assistant,
                text: streamed_visible,
                think_block: None,
            };
            render_chat_message(&mut lines, &fake, inner_width as usize);
            if app.blink_on
                && let Some(last) = lines.last_mut()
            {
                last.spans
                    .push(Span::styled("▋", Style::default().fg(Color::Green)));
            }
        }

        if app.thinking {
            lines.push(Line::from(Span::styled(
                format!("  {} thinking…", app.thinking_frame()),
                Style::default().fg(Color::DarkGray),
            )));
        } else if app.switching_model {
            // Once the weights have fully landed on disk, swap the spinner for a
            // checkmark so it's clear the download finished and we're now loading
            // the model into memory (which can still take a while).
            let download_complete = matches!(
                app.download_progress,
                Some((downloaded, expected)) if expected > 0 && downloaded >= expected
            );

            if download_complete {
                let size_str = app
                    .download_progress
                    .map(|(_, expected)| format!(" ({})", format_size_human(expected)))
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled("  ✓ ", Style::default().fg(Color::Green)),
                    Span::styled(
                        format!("model downloaded{size_str} — loading into memory…"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            } else {
                let progress_str = if let Some((downloaded, expected)) = app.download_progress {
                    if expected > 0 {
                        let pct = (downloaded as f64 / expected as f64 * 100.0).min(100.0) as u8;
                        let dl_str = format_size_human(downloaded.min(expected));
                        let ex_str = format_size_human(expected);
                        format!(" — {dl_str} / {ex_str} ({pct}%)")
                    } else if downloaded > 0 {
                        format!(" — {} downloaded", format_size_human(downloaded))
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                lines.push(Line::from(Span::styled(
                    format!("  {} switching model{progress_str}…", app.switching_frame()),
                    Style::default().fg(Color::DarkGray),
                )));
            }
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

    fn render_chat_message(lines: &mut Vec<Line<'static>>, msg: &ChatMessage, _width: usize) {
        match msg.role {
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
                    lines.push(Line::from(Span::styled(
                        "  ┌ thinking ".to_string(),
                        Style::default().fg(Color::DarkGray),
                    )));
                    for think_line in think.split('\n') {
                        lines.push(Line::from(Span::styled(
                            format!("  │ {think_line}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                    lines.push(Line::from(Span::styled(
                        "  └─────────".to_string(),
                        Style::default().fg(Color::DarkGray),
                    )));
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
        let mut spans = vec![
            Span::styled(
                " Enter ",
                Style::default().fg(Color::Black).bg(Color::Green),
            ),
            Span::styled(" send  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                " /help ",
                Style::default().fg(Color::Black).bg(Color::DarkGray),
            ),
            Span::styled(" commands  ", Style::default().fg(Color::DarkGray)),
            Span::styled(" Ctrl+C ", Style::default().fg(Color::Black).bg(Color::Red)),
            Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        ];

        if let Some((tool, _)) = &app.pending_approval {
            spans.push(Span::styled(
                format!("  allow {tool}? [y]es · [a]lways · [n]o"),
                Style::default().fg(Color::Yellow),
            ));
        } else if app.thinking || app.switching_model || app.is_streaming() {
            spans.push(Span::styled(
                "  (busy — Ctrl+C to cancel)",
                Style::default().fg(Color::Yellow),
            ));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn handle_key(app: &mut App, key: KeyEvent) -> Option<String> {
        if key.kind != KeyEventKind::Press {
            return None;
        }

        if app.show_model_picker {
            match key.code {
                KeyCode::Esc => {
                    app.close_model_picker();
                    return None;
                }
                KeyCode::Up => {
                    app.move_model_picker_up();
                    return None;
                }
                KeyCode::Down => {
                    app.move_model_picker_down();
                    return None;
                }
                KeyCode::Enter => {
                    return Some(format!("/models {}", app.model_picker_index + 1));
                }
                _ => return None,
            }
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.quit = true;
                None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.quit = true;
                None
            }
            KeyCode::Enter => {
                if app.input.trim().is_empty() {
                    return None;
                }
                let text = app.input.drain(..).collect::<String>();
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

    // ── Explicit on-device model loading ──────────────────────────────────────

    /// The local model `/load` should bring up: the persisted selection if it
    /// still resolves to a known model, otherwise the first on-device (non-cloud)
    /// entry in the picker.
    fn default_local_model_item(app: &App) -> Option<ModelPickerItem> {
        if let Some(selected) = crate::setup::load_selected_model()
            && let Some(item) = app.model_picker_items.iter().find(|item| {
                item.config.model_id == selected.model_id
                    && item
                        .config
                        .files
                        .iter()
                        .any(|file| file == &selected.gguf_file)
            })
        {
            return Some(item.clone());
        }
        app.model_picker_items
            .iter()
            .find(|item| item.cloud_tier.is_none())
            .cloned()
    }

    /// Load `model` on-device on a dedicated loader thread, routing inference to a
    /// fresh `LocalBackend` and driving the switch-progress UI. The caller is
    /// responsible for any cloud-tier handling; this path is on-device only.
    fn start_local_model_load<B: ratatui::backend::Backend>(
        app: &mut App,
        model: ModelPickerItem,
        engine: Arc<ChatEngine>,
        terminal: &mut ratatui::Terminal<B>,
    ) {
        if model.cache_health == ModelCacheHealth::Incomplete {
            app.messages.push(ChatMessage::system(format!(
                "error: {} has an incomplete local cache and cannot be selected yet.",
                model.display_name
            )));
            return;
        }

        // Loading an on-device model puts us in local inference mode.
        let _ = crate::settings::set_local_inference(true);

        // Route inference on-device; the loader thread below fills the engine the
        // LocalBackend reads from.
        app.backend = Arc::new(LocalBackend::new(Arc::clone(&engine)));

        let loading_msg = if model.cache_health == ModelCacheHealth::NotDownloaded {
            format!(
                "Downloading and loading {} ({})… this may take a few minutes.",
                model.display_name, model.description
            )
        } else {
            format!("Loading {}…", model.display_name)
        };

        app.messages.push(ChatMessage::system(loading_msg));
        terminal.draw(|frame| render(frame, app)).ok();

        let (tx, rx) = mpsc::channel(1);
        app.model_load_rx = Some(rx);
        app.switching_model = true;
        app.switching_model_id = Some(model.config.model_id.clone());
        // Only show download progress for models not yet cached.
        app.download_progress = if model.cache_health == ModelCacheHealth::NotDownloaded {
            Some((0, 0))
        } else {
            None
        };

        let sampling = SamplingConfig {
            max_tokens: Some(model.max_tokens),
            ..SamplingConfig::default()
        };

        // own thread + runtime so block_in_place doesn't starve the TUI loop.
        // Fold in project instruction files (AGENTS.md / CLAUDE.md) for the launch
        // directory so the on-device model gets the same always-on context the
        // cloud and ACP paths get.
        let system_prompt = {
            let base = crate::system_prompt_for_model(model.tool_calling).to_string();
            match std::env::current_dir()
                .ok()
                .and_then(|cwd| crate::instructions::load_project_instructions(&cwd))
            {
                Some(extra) => format!("{base}\n\n{extra}"),
                None => base,
            }
        };
        let engine_handle = Arc::clone(&engine);
        let tool_calling = model.tool_calling;
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("failed to create model-loader runtime");
            let update = rt.block_on(async move {
                match engine_handle
                    .load_gguf_model(
                        model.config.clone(),
                        Some(system_prompt.to_string()),
                        Some(sampling),
                    )
                    .await
                {
                    Ok(_) => ModelLoadUpdate::Loaded(model.display_name.clone()),
                    Err(err) => ModelLoadUpdate::Error(err.to_string()),
                }
            });
            // capacity-1 channel, receiver alive while switching
            let _ = tx.blocking_send(update);
        });
        // applied on ModelLoadUpdate::Loaded
        app.pending_tool_calling = Some(tool_calling);
    }

    // ── Slash command execution ───────────────────────────────────────────────

    async fn exec_slash<B: ratatui::backend::Backend>(
        app: &mut App,
        cmd: SlashCommand,
        engine: Arc<ChatEngine>,
        terminal: &mut ratatui::Terminal<B>,
    ) {
        match cmd {
            SlashCommand::Help => {
                app.messages.push(ChatMessage::system(
                    "/help          — show this message\n\
                     /models        — open the model picker\n\
                     /models N      — switch to model N\n\
                     /local [on|off]— toggle on-device inference mode\n\
                     /skills        — list available Agent Skills\n\
                     /mcp           — list MCP servers and their tools\n\
                     /load          — load the selected on-device model\n\
                     /login E P     — sign in to siGit Code Cloud\n\
                     /logout        — sign out\n\
                     /whoami        — show the signed-in account\n\
                     /plan [on|off] — plan mode: research only, no edits or commands\n\
                     /permissions   — show the tool permission policy\n\
                     /compact       — summarize and shrink conversation history\n\
                     /resume        — restore the saved session from disk\n\
                     /clear         — wipe conversation history\n\
                     /status        — show engine status\n\
                     /exit          — quit chat",
                ));
            }
            SlashCommand::Clear => {
                let cleared = engine.clear_history().await;
                app.messages.clear();
                crate::permissions::reset_session(crate::permissions::TUI_SESSION);
                // The saved session must not resurrect what the user just wiped.
                crate::session_store::delete(TUI_STORE_SESSION);
                app.messages.push(ChatMessage::system(format!(
                    "Cleared {cleared} turn(s). History is empty.",
                )));
            }
            SlashCommand::Compact => {
                let before = crate::backend::estimate_tokens(&app.backend.history_snapshot().await);
                match app
                    .backend
                    .compact_history(crate::backend::COMPACT_KEEP_LAST)
                    .await
                {
                    Ok(()) => {
                        let snapshot = app.backend.history_snapshot().await;
                        let after = crate::backend::estimate_tokens(&snapshot);
                        // Keep the saved session in step with the compacted state.
                        if let Err(error) = crate::session_store::save(TUI_STORE_SESSION, &snapshot)
                        {
                            log::warn!("session save after /compact failed: {error}");
                        }
                        app.messages.push(ChatMessage::system(format!(
                            "Compacted history: ~{before} → ~{after} tokens (estimated)."
                        )));
                    }
                    Err(error) => {
                        app.messages
                            .push(ChatMessage::system(format!("Compaction failed: {error}")));
                    }
                }
            }
            SlashCommand::Resume => match crate::session_store::load(TUI_STORE_SESSION) {
                Some(history) if !history.is_empty() => {
                    let restored = history.len();
                    app.backend.restore_history(history).await;
                    app.messages.push(ChatMessage::system(format!(
                        "Restored {restored} message(s) from the saved session. \
                         The model remembers the conversation; the scrollback above does not \
                         replay it."
                    )));
                }
                _ => {
                    app.messages.push(ChatMessage::system(
                        "No saved session to resume. Sessions are saved after each turn.",
                    ));
                }
            },
            SlashCommand::Plan(value) => {
                use crate::permissions::{self, TUI_SESSION};
                let enabled = value.unwrap_or_else(|| !permissions::plan_mode(TUI_SESSION));
                permissions::set_plan_mode(TUI_SESSION, enabled);
                app.messages.push(ChatMessage::system(if enabled {
                    "Plan mode ON — research with read-only tools only; edits and commands \
                     are blocked until /plan off."
                } else {
                    "Plan mode OFF — tools may execute again (subject to the permission \
                     policy)."
                }));
            }
            SlashCommand::Permissions => {
                app.messages
                    .push(ChatMessage::system(crate::permissions::describe(
                        crate::permissions::TUI_SESSION,
                    )));
            }
            SlashCommand::Status => {
                let info = engine.as_ref().info().await;
                let model = info.model_name.as_deref().unwrap_or("(none)");
                let mem = info.approx_memory.as_deref().unwrap_or("unknown");
                app.messages.push(ChatMessage::system(format!(
                    "status: {:?}  model: {}  memory: {}  history: {} turns",
                    info.status, model, mem, info.history_length,
                )));
            }
            SlashCommand::Skills => {
                app.messages
                    .push(ChatMessage::system(crate::skills::format_skills_list()));
            }
            SlashCommand::Mcp => {
                app.messages
                    .push(ChatMessage::system(crate::mcp::status_summary()));
            }
            SlashCommand::Models(selection) => match selection {
                None => {
                    app.open_model_picker(&engine);
                }
                Some(n) => {
                    let idx = n.saturating_sub(1);
                    match app.model_picker_items.get(idx).cloned() {
                        None => {
                            app.messages.push(ChatMessage::system(format!(
                                "error: no model #{n} — type /models to see the list."
                            )));
                        }
                        Some(model) => {
                            // ── siGit Code Cloud tier: no local load; sign-in gated ──
                            if let Some(tier) = model.cloud_tier.clone() {
                                app.close_model_picker();
                                match crate::provider::cloud_tier_provider(&tier) {
                                    Some(provider) => {
                                        let system_prompt =
                                            crate::system_prompt_for_model(true).to_string();
                                        app.backend = Arc::new(OpenAiBackend::new(
                                            provider.base_url,
                                            provider.api_key,
                                            provider.model,
                                            Some(system_prompt),
                                        ));
                                        app.current_model_name = provider.display_name.clone();
                                        app.tool_calling = true;
                                        // Selecting a cloud tier puts us in cloud mode.
                                        let _ = crate::settings::set_local_inference(false);
                                        app.messages.push(ChatMessage::system(format!(
                                            "Switched to {}.",
                                            provider.display_name
                                        )));
                                    }
                                    None => {
                                        app.messages.push(ChatMessage::system(
                                            "siGit Code Cloud needs an account. Use \
                                             `/login <email> <password>`, or create one at sigit.si.",
                                        ));
                                    }
                                }
                                return;
                            }

                            app.close_model_picker();
                            start_local_model_load(app, model, Arc::clone(&engine), terminal);
                        }
                    }
                }
            },
            SlashCommand::Local(value) => {
                let enabled = value.unwrap_or(!crate::settings::local_inference_enabled());
                match crate::settings::set_local_inference(enabled) {
                    Ok(()) => {
                        let state = if enabled { "on" } else { "off" };
                        let hint = if enabled {
                            "On-device models are highlighted. Type /models to pick one."
                        } else {
                            "siGit Code Cloud tiers are highlighted. Type /models to pick one."
                        };
                        app.messages.push(ChatMessage::system(format!(
                            "Local inference is {state}. {hint}"
                        )));
                        // Refresh the picker so emphasis/order reflects the new mode.
                        if app.show_model_picker {
                            app.open_model_picker(&engine);
                        }
                    }
                    Err(error) => {
                        app.messages.push(ChatMessage::system(format!(
                            "error: could not save local inference setting: {error}"
                        )));
                    }
                }
            }
            SlashCommand::Load => match default_local_model_item(app) {
                None => {
                    app.messages.push(ChatMessage::system(
                        "No local model available to load. Use /models to see the list.",
                    ));
                }
                Some(model) => {
                    start_local_model_load(app, model, Arc::clone(&engine), terminal);
                }
            },
            SlashCommand::Login(arg) => {
                let message = match arg.as_deref().and_then(crate::account::parse_login_args) {
                    Some((email, password)) => {
                        match crate::account::authenticate(&email, &password).await {
                            Ok(email) => format!(
                                "Signed in as {email}. siGit Code Cloud applies to your next session."
                            ),
                            Err(error) => format!("Login failed: {error}"),
                        }
                    }
                    None => "usage: /login <email> <password>".to_string(),
                };
                app.messages.push(ChatMessage::system(message));
            }
            SlashCommand::Logout => {
                let message = crate::account::end_session().await;
                app.messages.push(ChatMessage::system(message));
            }
            SlashCommand::Whoami => {
                let message = crate::account::status_line().await;
                app.messages.push(ChatMessage::system(message));
            }
            SlashCommand::Exit => {
                app.quit = true;
            }
            SlashCommand::Unknown(cmd) => {
                app.messages
                    .push(ChatMessage::system(format!("unknown command: {cmd}")));
            }
        }
    }

    // ── Background inference task ─────────────────────────────────────────────

    /// cap tool rounds so a confused model can't loop forever; auto-compaction
    /// keeps long runs inside the context window, so the cap can be generous
    const MAX_TOOL_ROUNDS: usize = 24;

    /// The TUI is a single conversation, so it persists under one fixed
    /// session-store id (ACP sessions use their protocol-assigned ids).
    const TUI_STORE_SESSION: &str = "tui";

    fn build_tool_specs() -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = crate::tools::all_tools()
            .into_iter()
            .map(|t| ToolSpec {
                name: t.name.to_string(),
                description: t.description.to_string(),
                parameters_schema: t.parameters_schema.to_string(),
            })
            .collect();

        // Advertise the Agent Skills `skill` tool only when skills exist on disk
        // (https://agentskills.io). The tool description carries the discovery
        // list (name + description) for progressive disclosure.
        let discovered = crate::skills::discover_skills();
        if !discovered.is_empty() {
            specs.push(ToolSpec {
                name: crate::skills::SKILL_TOOL_NAME.to_string(),
                description: crate::skills::skill_tool_description(&discovered),
                parameters_schema: crate::skills::skill_tool_schema().to_string(),
            });
        }

        // Tools discovered from configured MCP servers (incl. the official one).
        specs.extend(crate::mcp::tool_specs());

        specs
    }

    /// Close out a cancelled round in backend history: the results of tools
    /// that already ran this round, plus cancellation notes for `unreached`
    /// calls. Leaving a round's tool calls unanswered breaks strict
    /// OpenAI-compatible endpoints on the session's next request.
    async fn abandon_round(
        backend: &dyn InferenceBackend,
        mut tool_results: Vec<ToolResult>,
        unreached: &[crate::backend::ToolCall],
    ) {
        for pending in unreached {
            tool_results.push(ToolResult {
                tool_call_id: pending.id.clone(),
                content: format!(
                    "`{}` was not executed: the user cancelled the turn.",
                    pending.name
                ),
            });
        }
        backend.record_cancelled_tool_results(tool_results).await;
    }

    /// run the tool-calling loop off the main thread, posting updates via `tx`.
    /// dropping `tx` signals completion to the event loop.
    async fn run_inference_task(
        backend: Arc<dyn InferenceBackend>,
        text: String,
        tx: mpsc::Sender<InferenceUpdate>,
        tools_enabled: bool,
    ) {
        let tools = if tools_enabled {
            build_tool_specs()
        } else {
            vec![]
        };

        // Bridge the backend's token sink (plain strings) onto the UI update
        // channel as `Delta` messages. The forwarder lives for the whole turn.
        let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<String>();
        let forward_tx = tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(piece) = delta_rx.recv().await {
                if forward_tx
                    .send(InferenceUpdate::Delta(piece))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // The first round offers tools, so on-device inference can't stream it
        // (it must buffer to detect tool calls). With tools disabled there are
        // none to offer, so it streams directly.
        let first_sink = if tools.is_empty() {
            Some(&delta_tx)
        } else {
            None
        };
        let mut streamed = first_sink.is_some();

        let mut result = match backend
            .send_message_with_tools(&text, &tools, first_sink)
            .await
        {
            Ok(r) => r,
            Err(err) => {
                let _ = tx.send(InferenceUpdate::Error(err)).await;
                return;
            }
        };

        let mut round = 0;

        while !result.tool_calls.is_empty() && round < MAX_TOOL_ROUNDS {
            // any tool call means the first round didn't produce a final answer
            streamed = false;
            round += 1;
            log::info!("tool round {} — {} call(s)", round, result.tool_calls.len());

            // Auto-compaction: long tool runs grow history fast; fold it into
            // a summary before the next round rather than blowing the window.
            let estimate = crate::backend::estimate_tokens(&backend.history_snapshot().await);
            if estimate > crate::backend::DEFAULT_CONTEXT_TOKEN_BUDGET {
                log::info!(
                    "history ≈{estimate} tokens exceeds budget {} — compacting",
                    crate::backend::DEFAULT_CONTEXT_TOKEN_BUDGET
                );
                match backend
                    .compact_history(crate::backend::COMPACT_KEEP_LAST)
                    .await
                {
                    Ok(()) => {
                        let after =
                            crate::backend::estimate_tokens(&backend.history_snapshot().await);
                        log::info!("compacted history to ≈{after} tokens");
                    }
                    Err(error) => log::warn!("history compaction failed: {error}"),
                }
            }

            let mut tool_results = Vec::new();

            for (call_index, tc) in result.tool_calls.iter().enumerate() {
                // The UI drops the receiver on Ctrl+C or quit. Stop the turn
                // at the next boundary instead of burning model rounds (and
                // possibly running granted tools) in the background.
                if tx.is_closed() {
                    log::info!("turn cancelled by the user — stopping the tool loop");
                    abandon_round(&*backend, tool_results, &result.tool_calls[call_index..]).await;
                    return;
                }

                log::info!(
                    "  → {}({})",
                    tc.name,
                    tc.arguments.chars().take(120).collect::<String>()
                );

                let _ = tx.send(InferenceUpdate::ToolUse(tc.name.clone())).await;

                // Permission gate: read-only tools pass straight through; a
                // mutating tool consults policy and may pause on the user's
                // y/a/n answer (delivered over a oneshot from the event loop).
                use crate::permissions::{self, Decision, TUI_SESSION};
                let output = match permissions::decision_for(TUI_SESSION, &tc.name) {
                    Decision::Allow => crate::tools::execute_tool(&tc.name, &tc.arguments).await,
                    Decision::Deny(reason) => {
                        log::info!("  ✗ {} denied by policy", tc.name);
                        reason
                    }
                    Decision::Ask => {
                        let (reply_tx, reply_rx) = oneshot::channel();
                        let _ = tx
                            .send(InferenceUpdate::ApprovalRequest {
                                tool: tc.name.clone(),
                                args: permissions::approval_preview(&tc.arguments),
                                reply: reply_tx,
                            })
                            .await;
                        match reply_rx.await {
                            Ok(ApprovalChoice::Once) => {
                                crate::tools::execute_tool(&tc.name, &tc.arguments).await
                            }
                            Ok(ApprovalChoice::Session) => {
                                permissions::grant_for_session(TUI_SESSION, &tc.name);
                                crate::tools::execute_tool(&tc.name, &tc.arguments).await
                            }
                            Ok(ApprovalChoice::Deny) => {
                                log::info!("  ✗ {} denied by user", tc.name);
                                permissions::user_denial(&tc.name)
                            }
                            // The UI dropped the reply channel (Ctrl+C or
                            // quit): the whole turn is over, not just this
                            // call. Close out the round and stop instead of
                            // continuing rounds in the background.
                            Err(_) => {
                                log::info!(
                                    "turn cancelled at the approval prompt — stopping the tool loop"
                                );
                                abandon_round(
                                    &*backend,
                                    tool_results,
                                    &result.tool_calls[call_index..],
                                )
                                .await;
                                return;
                            }
                        }
                    }
                };
                log::info!("  ← {} chars", output.len());

                tool_results.push(ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: output,
                });
            }

            // Cancelled while the round's tools ran: record what executed and
            // stop before paying for another model round nobody will see.
            if tx.is_closed() {
                log::info!("turn cancelled by the user — skipping the next model round");
                abandon_round(&*backend, tool_results, &[]).await;
                return;
            }

            // on the last round, pass no tools so the model must produce text —
            // that's also the round we can stream on-device.
            let next_tools = if round < MAX_TOOL_ROUNDS {
                Some(tools.as_slice())
            } else {
                None
            };
            let sink = if next_tools.is_none() {
                streamed = true;
                Some(&delta_tx)
            } else {
                None
            };

            match backend
                .send_tool_results(tool_results, next_tools, sink)
                .await
            {
                Ok(r) => result = r,
                Err(err) => {
                    let _ = tx.send(InferenceUpdate::Error(err)).await;
                    return;
                }
            }
        }

        // Drop the sink so the forwarder finishes draining any buffered tokens
        // before we commit the reply.
        drop(delta_tx);
        let _ = forwarder.await;

        if result.tool_calls.is_empty() {
            if result.text.is_empty() {
                log::warn!(
                    "model returned empty reply — may have exhausted max_tokens on thinking"
                );
                let _ = tx
                    .send(InferenceUpdate::Error(
                        "(empty response — the model may have used all tokens on internal reasoning. \
                         Try a shorter or simpler prompt.)"
                            .to_string(),
                    ))
                    .await;
            } else if streamed {
                // tokens already went out as deltas; just commit the buffer
                let _ = tx.send(InferenceUpdate::StreamEnd).await;
            } else {
                let _ = tx.send(InferenceUpdate::Response(result.text)).await;
            }
        }

        // Persist the completed turn so /resume (or a restart) can pick the
        // conversation back up.
        let snapshot = backend.history_snapshot().await;
        if let Err(error) = crate::session_store::save(TUI_STORE_SESSION, &snapshot) {
            log::warn!("session save failed: {error}");
        }

        log::info!("inference complete — {} tool round(s)", round);
        // tx drops here — event loop gets None from rx.recv()
    }

    // ── Main loop ─────────────────────────────────────────────────────────────

    /// entry point — blocks until the user quits.
    /// caller owns terminal init/restore. `load_rx` delivers the model-load result
    /// from a dedicated OS thread; we poll it non-blocking each tick.
    pub async fn run_with<B: ratatui::backend::Backend>(
        terminal: &mut ratatui::Terminal<B>,
        engine: Arc<ChatEngine>,
        backend: Arc<dyn InferenceBackend>,
        load_rx: std_mpsc::Receiver<Result<(), String>>,
        load_model_name: String,
    ) -> Result<()> {
        event_loop(terminal, engine, backend, load_rx, load_model_name).await
    }

    async fn event_loop<B: ratatui::backend::Backend>(
        terminal: &mut ratatui::Terminal<B>,
        engine: Arc<ChatEngine>,
        backend: Arc<dyn InferenceBackend>,
        load_rx: std_mpsc::Receiver<Result<(), String>>,
        load_model_name: String,
    ) -> Result<()> {
        let mut app = App::new(load_model_name, backend);
        let mut event_stream = EventStream::new();

        // 10 fps is plenty for spinners
        let mut ticker = interval(Duration::from_millis(100));

        loop {
            // ── Poll the loader channel (non-blocking) ────────────────────────
            if app.is_loading {
                match load_rx.try_recv() {
                    Ok(Ok(())) => app.finish_loading(),
                    Ok(Err(e)) => app.set_load_error(e),
                    Err(std_mpsc::TryRecvError::Empty) => {}
                    Err(std_mpsc::TryRecvError::Disconnected) => {
                        app.set_load_error("Model loader thread crashed.".to_string());
                    }
                }
            }

            // redraw every iteration
            terminal.draw(|frame| render(frame, &mut app))?;

            if let Some(rx) = app.model_load_rx.as_mut() {
                match rx.try_recv() {
                    Ok(ModelLoadUpdate::Loaded(model_name)) => {
                        engine.clear_history().await;
                        if let Some(tc) = app.pending_tool_calling.take() {
                            app.tool_calling = tc;
                        }
                        app.switching_model = false;
                        app.switching_model_id = None;
                        app.download_progress = None;
                        app.model_load_cancelled = false;
                        app.model_load_rx = None;
                        app.current_model_name = model_name.clone();

                        let save_result = app
                            .model_picker_items
                            .iter()
                            .find(|item| item.display_name == model_name)
                            .map(|item| crate::setup::SelectedModel {
                                model_id: item.config.model_id.clone(),
                                gguf_file: item
                                    .config
                                    .files
                                    .first()
                                    .cloned()
                                    .unwrap_or_else(String::new),
                            })
                            .filter(|selected| !selected.gguf_file.is_empty())
                            .map(|selected| crate::setup::save_selected_model(&selected))
                            .unwrap_or_else(|| {
                                Err(format!(
                                    "could not determine a stable identifier for {}",
                                    model_name
                                ))
                            });

                        if let Err(error) = save_result {
                            app.messages.push(ChatMessage::system(format!(
                                "warning: switched to {} but could not save the selection: {}",
                                model_name, error
                            )));
                        } else {
                            app.messages
                                .push(ChatMessage::system(format!("✓ Switched to {}", model_name)));
                        }
                    }
                    Ok(ModelLoadUpdate::Error(error)) => {
                        app.switching_model = false;
                        app.switching_model_id = None;
                        app.download_progress = None;
                        app.model_load_cancelled = false;
                        app.model_load_rx = None;
                        app.messages
                            .push(ChatMessage::system(format!("error loading model: {error}")));
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        let was_cancelled = app.model_load_cancelled;
                        app.switching_model = false;
                        app.switching_model_id = None;
                        app.download_progress = None;
                        app.model_load_cancelled = false;
                        app.model_load_rx = None;
                        if !was_cancelled {
                            app.messages.push(ChatMessage::system(
                                "error loading model: loader task disconnected".to_string(),
                            ));
                        }
                    }
                }
            }

            if app.quit {
                break;
            }

            // multiplex terminal events, streaming tokens, inference updates,
            // and the thinking-spinner timer.
            tokio::select! {
                biased;

                // ── Spinner tick (loading phase only) ─────────────────────────
                _ = ticker.tick(), if app.is_loading => {
                    app.tick();
                }

                // ── inference updates from background task ───────────────────
                update = async {
                    match app.inference_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => pending().await,
                    }
                } => {
                    match update {
                        Some(InferenceUpdate::ToolUse(name)) => {
                            app.messages.push(ChatMessage::system(format!("🔧 {name}")));
                        }
                        Some(InferenceUpdate::Delta(delta)) => {
                            app.push_stream_delta(&delta);
                        }
                        Some(InferenceUpdate::StreamEnd) => {
                            app.finalize_stream();
                        }
                        Some(InferenceUpdate::Response(text)) => {
                            app.stop_thinking();
                            app.messages.push(ChatMessage::assistant(text));
                        }
                        Some(InferenceUpdate::Error(msg)) => {
                            app.finalize_stream();
                            app.stop_thinking();
                            app.messages.push(ChatMessage::system(format!("error: {msg}")));
                        }
                        Some(InferenceUpdate::ApprovalRequest { tool, args, reply }) => {
                            let call = if args.is_empty() {
                                tool.clone()
                            } else {
                                format!("{tool}({args})")
                            };
                            app.messages.push(ChatMessage::system(format!(
                                "⚠ permission — allow {call}?  [y]es · [a]lways this session · [n]o"
                            )));
                            app.pending_approval = Some((tool, reply));
                        }
                        None => {
                            // task finished, possibly with no text to show
                            app.finalize_stream();
                            app.stop_thinking();
                        }
                    }
                }

                // ── thinking / switching spinner tick (100ms) ────────────────
                _ = async {
                    if app.thinking || app.switching_model {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    } else {
                        pending().await
                    }
                } => {
                    app.tick_thinking();
                    // keep the progress display fresh
                    if app.switching_model {
                        app.poll_download_progress();
                    }
                }

                // ── Terminal events ───────────────────────────────────────────
                maybe_event = event_stream.next() => {
                    let Some(Ok(event)) = maybe_event else {
                        break;
                    };

                    if let Event::Key(key) = event {
                        // loading phase — only quit keys work
                        if app.is_loading {
                            if key.kind == KeyEventKind::Press {
                                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                                if ctrl
                                    && (key.code == KeyCode::Char('c')
                                        || key.code == KeyCode::Char('d'))
                                {
                                    app.quit = true;
                                }
                            }
                            continue;
                        }

                        // pending tool approval — y/a/n answer the prompt; the
                        // inference task is paused on the reply channel. Checked
                        // before the busy gate because the app *is* busy here.
                        if app.pending_approval.is_some() {
                            if key.kind == KeyEventKind::Press {
                                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                                let choice = if ctrl
                                    && (key.code == KeyCode::Char('c')
                                        || key.code == KeyCode::Char('d'))
                                {
                                    // cancel the whole turn: denying is implicit
                                    // in dropping the reply channel
                                    app.pending_approval = None;
                                    app.stop_thinking();
                                    app.messages.push(ChatMessage::system("(cancelled)"));
                                    continue;
                                } else {
                                    match key.code {
                                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                                            Some(ApprovalChoice::Once)
                                        }
                                        KeyCode::Char('a') | KeyCode::Char('A') => {
                                            Some(ApprovalChoice::Session)
                                        }
                                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                            Some(ApprovalChoice::Deny)
                                        }
                                        _ => None,
                                    }
                                };
                                if let Some(choice) = choice
                                    && let Some((tool, reply)) = app.pending_approval.take()
                                {
                                    let verdict = match &choice {
                                        ApprovalChoice::Once => "allowed once",
                                        ApprovalChoice::Session => "allowed for this session",
                                        ApprovalChoice::Deny => "denied",
                                    };
                                    app.messages.push(ChatMessage::system(format!(
                                        "{tool}: {verdict}"
                                    )));
                                    let _ = reply.send(choice);
                                }
                            }
                            continue;
                        }

                        // busy — only cancel keys work
                        if app.is_busy() {
                            if key.kind == KeyEventKind::Press {
                                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                                if ctrl && (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('d')) {
                                    if app.is_streaming() {
                                        app.finalize_stream();
                                        app.messages.push(ChatMessage::system("(cancelled)"));
                                    }
                                    if app.thinking {
                                        // dropping rx kills the background task
                                        app.stop_thinking();
                                        app.messages.push(ChatMessage::system("(cancelled)"));
                                    }
                                    if app.switching_model {
                                        // flag before drop so Disconnected handler stays quiet
                                        app.model_load_cancelled = true;
                                        app.switching_model = false;
                                        app.switching_model_id = None;
                                        app.download_progress = None;
                                        app.model_load_rx = None;
                                        app.messages
                                            .push(ChatMessage::system("(download cancelled — model switch aborted)"));
                                    }
                                }
                            }
                            continue;
                        }

                        if let Some(text) = handle_key(&mut app, key) {
                            if let Some(cmd) = parse_slash(&text) {
                                exec_slash(&mut app, cmd, Arc::clone(&engine), terminal).await;
                                continue;
                            }

                            // On-device inference needs a model in memory, and we
                            // never load one implicitly: the user loads it with
                            // /load (or /models). Refuse rather than erroring out
                            // deep in the backend.
                            if !app.backend.is_remote()
                                && engine.info().await.status == onde::inference::EngineStatus::Unloaded
                            {
                                app.messages.push(ChatMessage::user(&text));
                                app.messages.push(ChatMessage::system(
                                    "No on-device model is loaded. Run /load to load the selected \
                                     model, or /models to choose one.",
                                ));
                                continue;
                            }

                            // ── spawn inference ──────────────────────────────
                            app.messages.push(ChatMessage::user(&text));
                            app.start_thinking();

                            let (tx, rx) = mpsc::channel::<InferenceUpdate>(64);
                            app.inference_rx = Some(rx);

                            let backend_handle = Arc::clone(&app.backend);
                            let user_text = text.clone();
                            let tools_enabled = app.tool_calling;
                            tokio::spawn(async move {
                                run_inference_task(backend_handle, user_text, tx, tools_enabled).await;
                            });
                        }
                    }
                }
            }
        }

        Ok(())
    }

    // ── Download progress helpers (TUI) ──────────────────────────────────────

    /// total bytes under `path`, following symlinks (hf-hub uses blobs + symlinks)
    fn dir_size_recursive(path: &std::path::Path) -> u64 {
        let mut total: u64 = 0;
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                total += dir_size_recursive(&entry_path);
            } else if let Ok(meta) = entry_path.metadata() {
                total += meta.len();
            }
        }
        total
    }

    fn format_size_human(bytes: u64) -> String {
        const GB: u64 = 1_073_741_824;
        const MB: u64 = 1_048_576;
        const KB: u64 = 1_024;
        if bytes >= GB {
            format!("{:.2} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.1} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.0} KB", bytes as f64 / KB as f64)
        } else {
            format!("{bytes} B")
        }
    }
} // end #[cfg(unix)] mod tui

// re-export so callers write `chat::run_with(...)` on all platforms
#[cfg(unix)]
pub use tui::run_with;

// ── Tests (platform-agnostic) ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{parse_rich_text_segments, strip_think_blocks};

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
