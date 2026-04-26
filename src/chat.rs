//! Full-screen terminal chat UI for siGit Code.
//!
//! Takes over the alternate screen and multiplexes terminal events with
//! streaming LLM tokens via `tokio::select!`.
//!
//! The UI has two phases:
//!
//! 1. **Loading phase** — a centered spinner is shown while the model loads
//!    in the background.  The oneshot channel from the caller signals
//!    completion or failure.
//! 2. **Chat phase** — normal interactive chat once `load_rx` resolves.

use std::future::pending;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use onde::inference::{ChatEngine, SamplingConfig, StreamChunk, ToolDefinition, ToolResult};

use crate::models::{ModelCacheHealth, ModelPickerItem, ModelSource, build_model_picker_items};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval};

// ── Think-block stripping ─────────────────────────────────────────────────────

/// Strip `<think>…</think>` blocks from a model response.
///
/// Qwen 3 models emit `<think>…</think>` before the real answer. This
/// function separates the thinking content from the visible reply so the
/// UI can render them differently (dimmed / collapsed).
///
/// Returns `(thinking_text, visible_reply)`. Either may be empty.
pub(crate) fn strip_think_blocks(raw: &str) -> (String, String) {
    let mut thinking = String::new();
    let mut remainder = raw;

    while let Some(start) = remainder.find("<think>") {
        // Text before <think> is visible.
        let before = &remainder[..start];
        if let Some(end) = remainder[start..].find("</think>") {
            let block = &remainder[start + 7..start + end];
            thinking.push_str(block.trim());
            remainder = &remainder[start + end + 8..];
            // Prepend any text before <think> to the leftover.
            if !before.trim().is_empty() {
                // Unusual — text before <think>. Keep it visible.
                let mut combined = before.to_string();
                combined.push_str(remainder);
                return (thinking, combined.trim().to_string());
            }
        } else {
            // Unclosed <think> — treat rest as thinking (model ran out of tokens).
            thinking.push_str(remainder[start + 7..].trim());
            remainder = before;
            break;
        }
    }

    (thinking, remainder.trim().to_string())
}

// ── Message types ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    System,
    /// Banner art — each character gets its own color.
    Banner,
}

struct ChatMessage {
    role: Role,
    text: String,
    /// Extracted `<think>…</think>` content, if any (Qwen 3 reasoning).
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
        let (think, visible) = strip_think_blocks(&raw);
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

// ── Inference updates from background task ───────────────────────────────────

/// Messages sent from the spawned inference task back to the event loop.
enum InferenceUpdate {
    /// The model is calling a tool — show its name in the chat.
    ToolUse(String),
    /// The model produced a final text response.
    Response(String),
    /// Something went wrong during inference.
    Error(String),
}

enum ModelLoadUpdate {
    Loaded(String),
    Error(String),
}

// ── App state ─────────────────────────────────────────────────────────────────

struct App {
    messages: Vec<ChatMessage>,
    input: String,
    cursor: usize,
    scroll_offset: u16,
    stream_rx: Option<mpsc::Receiver<StreamChunk>>,
    stream_buf: String,
    /// Channel for receiving results from the background inference task.
    inference_rx: Option<mpsc::Receiver<InferenceUpdate>>,
    /// Channel for receiving results from a model switch.
    model_load_rx: Option<mpsc::Receiver<ModelLoadUpdate>>,
    /// True while waiting for inference to finish.
    thinking: bool,
    /// Counter driving the thinking spinner animation.
    thinking_tick: u8,
    quit: bool,
    /// Flips every few ticks while streaming to make the cursor blink.
    blink_on: bool,
    blink_counter: u8,
    /// True while a model switch is in progress.
    switching_model: bool,
    /// Tool-calling flag for the model currently being loaded in the background.
    /// Applied to `app.tool_calling` when `ModelLoadUpdate::Loaded` arrives.
    pending_tool_calling: Option<bool>,

    // ── Loading-phase state ───────────────────────────────────────────────────
    /// True while the model is still loading; switches to false on completion.
    is_loading: bool,
    /// Monotonic counter incremented on every animation tick.  Drives the
    /// braille spinner shown during loading.
    load_tick: u32,
    /// Set when model loading fails; keeps the loading view up with the error.
    load_error: Option<String>,
    /// When loading started — drives the elapsed-time counter.
    load_start: Instant,
    /// Display name of the model being loaded (shown in the spinner line).
    load_model_name: String,

    // ── Model picker state ────────────────────────────────────────────────────
    show_model_picker: bool,
    model_picker_index: usize,
    model_picker_items: Vec<ModelPickerItem>,
    current_model_name: String,
    /// Whether the currently loaded model supports tool calling.
    tool_calling: bool,
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

/// Spinner frames for the "thinking" animation.
const THINKING_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl App {
    fn new(load_model_name: String) -> Self {
        let items = build_model_picker_items();
        let tool_calling = items
            .iter()
            .find(|m| m.display_name == load_model_name)
            .map(|m| m.tool_calling)
            .unwrap_or(true);
        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll_offset: 0,
            stream_rx: None,
            stream_buf: String::new(),
            inference_rx: None,
            model_load_rx: None,
            thinking: false,
            thinking_tick: 0,
            quit: false,
            blink_on: true,
            blink_counter: 0,
            switching_model: false,
            pending_tool_calling: None,
            is_loading: true,
            load_tick: 0,
            load_error: None,
            load_start: Instant::now(),
            load_model_name: load_model_name.clone(),
            show_model_picker: false,
            model_picker_index: 0,
            model_picker_items: items,
            current_model_name: crate::setup::load_selected_model_name()
                .unwrap_or_else(|| load_model_name.clone()),
            tool_calling,
        }
    }

    /// True when either streaming tokens or waiting for inference.
    fn is_busy(&self) -> bool {
        self.is_streaming() || self.thinking || self.switching_model
    }

    fn switching_frame(&self) -> &'static str {
        let idx = (self.thinking_tick as usize) % THINKING_FRAMES.len();
        THINKING_FRAMES[idx]
    }

    fn is_streaming(&self) -> bool {
        self.stream_rx.is_some()
    }

    fn finalize_stream(&mut self) {
        self.stream_rx = None;
        if !self.stream_buf.is_empty() {
            let text = std::mem::take(&mut self.stream_buf);
            self.messages.push(ChatMessage::assistant(text));
        }
        self.blink_on = false;
    }

    fn push_stream_delta(&mut self, delta: &str) {
        self.stream_buf.push_str(delta);
        self.blink_counter = self.blink_counter.wrapping_add(1);
        self.blink_on = self.blink_counter % 4 < 2;
    }

    fn start_thinking(&mut self) {
        self.thinking = true;
        self.thinking_tick = 0;
    }

    fn stop_thinking(&mut self) {
        self.thinking = false;
        self.inference_rx = None;
    }

    fn tick_thinking(&mut self) {
        self.thinking_tick = self.thinking_tick.wrapping_add(1);
    }

    fn thinking_frame(&self) -> &'static str {
        let idx = (self.thinking_tick as usize) % THINKING_FRAMES.len();
        THINKING_FRAMES[idx]
    }

    /// Advance the spinner tick counter.
    fn tick(&mut self) {
        self.load_tick = self.load_tick.wrapping_add(1);
    }

    /// Transition from loading phase to normal chat.
    /// Adds the banner art and welcome messages to the message log.
    fn finish_loading(&mut self) {
        self.is_loading = false;
        for line in BANNER_ART.lines() {
            self.messages.push(ChatMessage::banner(line));
        }
        self.messages.push(ChatMessage::system(""));
        self.messages.push(ChatMessage::system(
            "In this world, nothing can be said to be certain, except death and taxes. ~ Pak Sigit",
        ));
        self.messages.push(ChatMessage::system(format!(
            "Current model: {}",
            self.current_model_name
        )));
        self.messages
            .push(ChatMessage::system("Type /help for commands."));
    }

    /// Record a loading error.  The loading view stays visible so the user can
    /// read the message before pressing Ctrl+C.
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

    /// Total lines the messages area would need (rough estimate for scrolling).
    fn total_message_lines(&self, width: u16) -> u16 {
        if width == 0 {
            return 0;
        }
        let w = width.saturating_sub(2) as usize; // subtract border columns
        let mut lines: u16 = 0;
        for msg in &self.messages {
            lines += wrapped_line_count(&msg.text, msg.role, w);
        }
        // count any in-progress streaming text too
        if !self.stream_buf.is_empty() {
            lines += wrapped_line_count(&self.stream_buf, Role::Assistant, w);
        }
        // thinking / switching indicator
        if self.thinking || self.switching_model {
            lines += 1;
        }
        lines
    }

    fn auto_scroll(&mut self, visible_height: u16, width: u16) {
        let total = self.total_message_lines(width);
        if total > visible_height {
            self.scroll_offset = total - visible_height;
        } else {
            self.scroll_offset = 0;
        }
    }
}

/// How many terminal rows a message takes up after line-wrapping.
fn wrapped_line_count(text: &str, role: Role, width: usize) -> u16 {
    let prefix_len = match role {
        Role::User => 6,      // "you > "
        Role::Assistant => 8, // "siGit > "
        Role::System | Role::Banner => 0,
    };
    let effective = if width > prefix_len {
        width - prefix_len
    } else {
        1
    };

    let mut count: u16 = 0;
    for line in text.split('\n') {
        if line.is_empty() {
            count += 1;
        } else {
            count += ((line.len() as f64) / (effective as f64)).ceil() as u16;
        }
    }
    count.max(1)
}

// ── Model table ──────────────────────────────────────────────────────────────

// ModelSource, ModelPickerItem, and build_model_picker_items live in
// crate::models so they are available on all platforms (including Windows),
// not just unix where this chat module is compiled.

fn render_model_picker(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let popup = centered_rect(82, 72, area);

    // Erase whatever is behind the popup so the panel is fully readable.
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Select a model… ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = Vec::new();
    let mut last_section: Option<ModelSource> = None;

    for (index, item) in app.model_picker_items.iter().enumerate() {
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
            };

            lines.push(
                Line::from(vec![
                    Span::styled(format!("{section_mark} "), section_style),
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
        };
        let source = format!("  [{} {}]", brand_mark, item.source_label);

        let base_style = if selected {
            Style::default().fg(Color::Black).bg(Color::Green)
        } else {
            Style::default().fg(Color::White).bg(Color::Black)
        };

        let source_style = if selected {
            Style::default().fg(Color::Black).bg(Color::Green)
        } else {
            match item.source {
                ModelSource::Onde => Style::default().fg(Color::Green).bg(Color::Black),
                ModelSource::HuggingFace => Style::default().fg(Color::Cyan).bg(Color::Black),
                ModelSource::Available => Style::default().fg(Color::Blue).bg(Color::Black),
                ModelSource::Fallback => Style::default().fg(Color::Yellow).bg(Color::Black),
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

// ── Slash commands ────────────────────────────────────────────────────────────

enum SlashCommand {
    Help,
    Clear,
    Status,
    /// `/models` opens the model picker. `/models N` still works as a shortcut.
    Models(Option<usize>),
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
        "/exit" | "/quit" | "/q" => SlashCommand::Exit,
        other => SlashCommand::Unknown(other.to_string()),
    })
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    if app.is_loading {
        // Loading phase: title bar with spinner | loading info | footer hint.
        let zones = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

        render_loading_title(frame, app, zones[0]);
        render_loading(frame, app, zones[1]);
        render_loading_footer(frame, zones[2]);
    } else {
        // Chat phase: title | messages | input | footer.
        let zones = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

        render_title(frame, zones[0]);
        render_messages(frame, app, zones[1]);
        render_input(frame, app, zones[2]);
        render_footer(frame, app, zones[3]);

        if app.show_model_picker {
            render_model_picker(frame, app, area);
        }
    }
}

fn render_title(frame: &mut Frame, area: ratatui::layout::Rect) {
    let title = Line::from(vec![
        Span::styled(
            "siGit",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Code", Style::default().fg(Color::White)),
        Span::styled(
            format!(" v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).style(Style::default().bg(Color::Black)),
        area,
    );
}

/// Title bar during loading: `⠹ siGit Code v0.1.1`
fn render_loading_title(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let spinner = SPINNER[(app.load_tick as usize) % SPINNER.len()];

    let title = Line::from(vec![
        Span::styled(
            format!("{spinner} "),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "siGit",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Code", Style::default().fg(Color::White)),
        Span::styled(
            format!(" v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).style(Style::default().bg(Color::Black)),
        area,
    );
}

/// Loading body — model name, elapsed time, or error message.
fn render_loading(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let elapsed = app.load_start.elapsed();
    let elapsed_str = if elapsed.as_secs() >= 60 {
        format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{}s", elapsed.as_secs())
    };

    let mut lines: Vec<Line<'_>> = Vec::new();

    if let Some(ref err) = app.load_error {
        lines.push(Line::from(vec![
            Span::styled(
                " ✘ ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(err.clone(), Style::default().fg(Color::Red)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(" Loading ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.load_model_name.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {elapsed_str}"),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// One-line footer shown only during the loading phase.
fn render_loading_footer(frame: &mut Frame, area: ratatui::layout::Rect) {
    let spans = vec![
        Span::styled(
            " Ctrl+C ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black)),
        area,
    );
}

fn render_messages(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line<'_>> = Vec::new();

    for msg in &app.messages {
        render_chat_message(&mut lines, msg);
    }

    // Streaming partial response.
    if !app.stream_buf.is_empty() || app.is_streaming() {
        let mut spans = vec![Span::styled(
            "siGit > ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )];

        // Split on newlines so multi-line streaming renders correctly.
        let buf_lines: Vec<&str> = app.stream_buf.split('\n').collect();
        for (i, segment) in buf_lines.iter().enumerate() {
            if i > 0 {
                lines.push(Line::from(std::mem::take(&mut spans)));
                // Continuation lines get no prefix.
            }
            spans.push(Span::raw(segment.to_string()));
        }

        // Blinking block cursor while streaming.
        if app.is_streaming() && app.blink_on {
            spans.push(Span::styled("█", Style::default().fg(Color::Green)));
        }

        lines.push(Line::from(spans));
    }

    // switching-model indicator (animated spinner)
    if app.switching_model {
        let frame_char = app.switching_frame();
        lines.push(Line::from(vec![
            Span::styled(
                "siGit > ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{frame_char} loading model…"),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            ),
        ]));
    }

    // thinking indicator (animated spinner)
    if app.thinking {
        let frame_char = app.thinking_frame();
        lines.push(Line::from(vec![
            Span::styled(
                "siGit > ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{frame_char} thinking…"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
    }

    // auto-scroll
    app.auto_scroll(area.height, area.width);

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset, 0))
        .style(Style::default());

    frame.render_widget(paragraph, area);
}

fn render_chat_message<'a>(lines: &mut Vec<Line<'a>>, msg: &ChatMessage) {
    let text_lines: Vec<&str> = msg.text.split('\n').collect();

    match msg.role {
        Role::User => {
            for (i, segment) in text_lines.iter().enumerate() {
                let mut spans = Vec::new();
                if i == 0 {
                    spans.push(Span::styled(
                        "you > ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                spans.push(Span::styled(
                    segment.to_string(),
                    Style::default().fg(Color::White),
                ));
                lines.push(Line::from(spans));
            }
        }
        Role::Assistant => {
            // Show thinking block dimmed if present.
            if let Some(ref think) = msg.think_block {
                let think_summary = if think.len() > 120 {
                    format!("{}…", &think[..120])
                } else {
                    think.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled("💭 ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        think_summary,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }

            for (i, segment) in text_lines.iter().enumerate() {
                let mut spans = Vec::new();
                if i == 0 {
                    spans.push(Span::styled(
                        "siGit > ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                spans.push(Span::styled(
                    segment.to_string(),
                    Style::default().fg(Color::White),
                ));
                lines.push(Line::from(spans));
            }
        }
        Role::System => {
            for segment in &text_lines {
                lines.push(Line::from(Span::styled(
                    segment.to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        Role::Banner => {
            for segment in &text_lines {
                lines.push(Line::from(Span::styled(
                    segment.to_string(),
                    Style::default().fg(Color::White),
                )));
            }
        }
    }
}

fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(if app.thinking {
            " thinking… "
        } else if app.is_streaming() {
            " streaming… "
        } else {
            " message "
        })
        .title_style(Style::default().fg(if app.is_busy() {
            Color::Yellow
        } else {
            Color::DarkGray
        }));

    let input_text = Paragraph::new(app.input.as_str())
        .style(Style::default().fg(if app.is_busy() {
            Color::DarkGray
        } else {
            Color::White
        }))
        .block(block);

    frame.render_widget(input_text, area);

    // Place cursor inside the input block (offset by 1 for the border).
    if !app.is_busy() {
        let x = area.x + app.cursor as u16 + 1;
        let y = area.y + 1;
        frame.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), y));
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let hints: &[(&str, &str)] = if app.show_model_picker {
        &[("↑↓", "select"), ("Enter", "load"), ("Esc", "close")]
    } else if app.is_busy() {
        &[("Ctrl+C", "cancel")]
    } else {
        &[
            ("Enter", "send"),
            ("/help", "commands"),
            ("/models", "models"),
            ("Ctrl+C", "quit"),
        ]
    };

    let mut spans: Vec<Span<'_>> = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(Color::Gray),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black)),
        area,
    );
}

// ── Input handling ────────────────────────────────────────────────────────────

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

// ── Slash command execution ───────────────────────────────────────────────────

async fn exec_slash<B: ratatui::backend::Backend>(
    app: &mut App,
    cmd: SlashCommand,
    engine: Arc<ChatEngine>,
    terminal: &mut ratatui::Terminal<B>,
) {
    match cmd {
        SlashCommand::Help => {
            app.messages.push(ChatMessage::system(
                "/help      — show this message\n\
                 /models    — open the model picker\n\
                 /models N  — switch to model N\n\
                 /clear     — wipe conversation history\n\
                 /status    — show engine status\n\
                 /exit      — quit chat",
            ));
        }
        SlashCommand::Clear => {
            let cleared = engine.clear_history().await;
            app.messages.clear();
            app.scroll_offset = 0;
            app.messages.push(ChatMessage::system(format!(
                "Cleared {cleared} turn(s). History is empty.",
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
                        if model.cache_health == ModelCacheHealth::Incomplete {
                            app.close_model_picker();
                            app.messages.push(ChatMessage::system(format!(
                                "error: {} has an incomplete local cache and cannot be selected yet.",
                                model.display_name
                            )));
                            return;
                        }

                        let loading_msg = if model.cache_health == ModelCacheHealth::NotDownloaded {
                            format!(
                                "Downloading and loading {} ({})… this may take a few minutes.",
                                model.display_name, model.description
                            )
                        } else {
                            format!("Loading {}…", model.display_name)
                        };

                        app.close_model_picker();
                        app.messages.push(ChatMessage::system(loading_msg));
                        terminal.draw(|frame| render(frame, app)).ok();

                        let (tx, rx) = mpsc::channel(1);
                        app.model_load_rx = Some(rx);
                        app.switching_model = true;

                        let sampling = SamplingConfig {
                            max_tokens: Some(model.max_tokens),
                            ..SamplingConfig::default()
                        };

                        // Spawn onto a background task so the event loop keeps
                        // running (and the spinner keeps animating) during the
                        // download + load — which can take several minutes for
                        // a large model fetched from HuggingFace for the first time.
                        let system_prompt = crate::system_prompt_for_model(model.tool_calling);
                        let engine_handle = Arc::clone(&engine);
                        let tool_calling = model.tool_calling;
                        tokio::spawn(async move {
                            let update = match engine_handle
                                .load_gguf_model(
                                    model.config.clone(),
                                    Some(system_prompt.to_string()),
                                    Some(sampling),
                                )
                                .await
                            {
                                Ok(_) => ModelLoadUpdate::Loaded(model.display_name.clone()),
                                Err(err) => ModelLoadUpdate::Error(err.to_string()),
                            };
                            let _ = tx.send(update).await;
                        });
                        // tool_calling is applied when ModelLoadUpdate::Loaded
                        // arrives in the event loop (see model_load_rx handler).
                        app.pending_tool_calling = Some(tool_calling);
                    }
                }
            }
        },
        SlashCommand::Exit => {
            app.quit = true;
        }
        SlashCommand::Unknown(cmd) => {
            app.messages
                .push(ChatMessage::system(format!("unknown command: {cmd}")));
        }
    }
}

// ── Background inference task ────────────────────────────────────────────────

/// Maximum number of tool-calling rounds before forcing a text response.
const MAX_TOOL_ROUNDS: usize = 10;

/// Build onde `ToolDefinition`s from our agent tools.
fn build_onde_tools() -> Vec<ToolDefinition> {
    crate::tools::all_tools()
        .into_iter()
        .map(|t| ToolDefinition {
            name: t.name.to_string(),
            description: t.description.to_string(),
            parameters_schema: t.parameters_schema.to_string(),
        })
        .collect()
}

/// Runs the agentic tool-calling loop on a background task and sends
/// progress updates back through `tx`.
///
/// The sender is dropped when the task finishes, which the event loop
/// detects as `None` from `rx.recv()`.
async fn run_inference_task(
    engine: Arc<ChatEngine>,
    text: String,
    tx: mpsc::Sender<InferenceUpdate>,
    tools_enabled: bool,
) {
    let onde_tools = if tools_enabled {
        build_onde_tools()
    } else {
        vec![]
    };

    let mut result = match engine.send_message_with_tools(&text, &onde_tools).await {
        Ok(r) => r,
        Err(err) => {
            let _ = tx.send(InferenceUpdate::Error(err.to_string())).await;
            return;
        }
    };

    let mut round = 0;

    while !result.tool_calls.is_empty() && round < MAX_TOOL_ROUNDS {
        round += 1;
        log::info!("tool round {} — {} call(s)", round, result.tool_calls.len());

        let mut tool_results = Vec::new();

        for tc in &result.tool_calls {
            log::info!(
                "  → {}({})",
                tc.function_name,
                tc.arguments.chars().take(120).collect::<String>()
            );

            // Notify the UI about the tool call.
            let _ = tx
                .send(InferenceUpdate::ToolUse(tc.function_name.clone()))
                .await;

            // Execute the tool (async — read_website uses spawn_blocking internally).
            let output = crate::tools::execute_tool(&tc.function_name, &tc.arguments).await;
            log::info!("  ← {} chars", output.len());

            tool_results.push(ToolResult {
                tool_call_id: tc.id.clone(),
                content: output,
            });
        }

        // Allow further tool calls unless we've hit the limit.
        let next_tools = if round < MAX_TOOL_ROUNDS {
            Some(onde_tools.as_slice())
        } else {
            None // force a text response on the last round
        };

        match engine.send_tool_results(tool_results, next_tools).await {
            Ok(r) => result = r,
            Err(err) => {
                let _ = tx.send(InferenceUpdate::Error(err.to_string())).await;
                return;
            }
        }
    }

    // Send the final text response, or a fallback if the model returned nothing.
    if result.tool_calls.is_empty() {
        if result.text.is_empty() {
            log::warn!("model returned empty reply — may have exhausted max_tokens on thinking");
            let _ = tx
                .send(InferenceUpdate::Error(
                    "(empty response — the model may have used all tokens on internal reasoning. \
                     Try a shorter or simpler prompt.)"
                        .to_string(),
                ))
                .await;
        } else {
            let _ = tx.send(InferenceUpdate::Response(result.text)).await;
        }
    }

    log::info!("inference complete — {} tool round(s)", round);
    // Sender drops here → event loop sees `None`.
}

// ── Main loop ─────────────────────────────────────────────────────────────────

/// Run the interactive chat UI.  Blocks until the user quits.
///
/// Accepts a terminal that has already been initialised by the caller —
/// [`ratatui::init`] and [`ratatui::restore`] are the caller's responsibility.
///
/// `load_rx` is the receiving end of a [`std::sync::mpsc`] channel.  A
/// dedicated OS thread loads the model and sends `Ok(())` or `Err(msg)` when
/// done.  The event loop polls `try_recv()` on every tick — non-blocking,
/// zero contention with the tokio runtime.
pub async fn run_with<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    engine: Arc<ChatEngine>,
    load_rx: std_mpsc::Receiver<Result<(), String>>,
    load_model_name: String,
) -> Result<()> {
    event_loop(terminal, engine, load_rx, load_model_name).await
}

async fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    engine: Arc<ChatEngine>,
    load_rx: std_mpsc::Receiver<Result<(), String>>,
    load_model_name: String,
) -> Result<()> {
    let mut app = App::new(load_model_name);
    let mut event_stream = EventStream::new();

    // 100 ms per tick ≈ 10 fps — enough for a smooth spinner.
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
                    app.model_load_rx = None;
                    app.messages
                        .push(ChatMessage::system(format!("error loading model: {error}")));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    app.switching_model = false;
                    app.model_load_rx = None;
                    app.messages.push(ChatMessage::system(
                        "error loading model: loader task disconnected".to_string(),
                    ));
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

            // ── Streaming LLM tokens ──────────────────────────────────────
            chunk = async {
                match app.stream_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => pending().await,
                }
            } => {
                match chunk {
                    Some(chunk) => {
                        if !chunk.delta.is_empty() {
                            app.push_stream_delta(&chunk.delta);
                        }
                        if chunk.done {
                            app.finalize_stream();
                        }
                    }
                    // Sender dropped without sending done=true.
                    None => {
                        app.finalize_stream();
                    }
                }
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
                    Some(InferenceUpdate::Response(text)) => {
                        app.stop_thinking();
                        app.messages.push(ChatMessage::assistant(text));
                    }
                    Some(InferenceUpdate::Error(msg)) => {
                        app.stop_thinking();
                        app.messages.push(ChatMessage::system(format!("error: {msg}")));
                    }
                    None => {
                        // Sender dropped — task finished (possibly with no
                        // text response, e.g. all tool calls with empty final).
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
            }

            // ── Terminal events ───────────────────────────────────────────
            maybe_event = event_stream.next() => {
                let Some(Ok(event)) = maybe_event else {
                    break;
                };

                if let Event::Key(key) = event {
                    // During loading, only Ctrl+C / Ctrl+D are accepted.
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

                    // While busy (streaming or thinking), only Ctrl+C/D work.
                    if app.is_busy() {
                        if key.kind == KeyEventKind::Press {
                            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                            if ctrl && (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('d')) {
                                if app.is_streaming() {
                                    app.finalize_stream();
                                    app.messages.push(ChatMessage::system("(cancelled)"));
                                }
                                if app.thinking {
                                    // Drop the receiver — the background task
                                    // will see a closed channel and stop.
                                    app.stop_thinking();
                                    app.messages.push(ChatMessage::system("(cancelled)"));
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

                        // ── Spawn inference on a background task ─────────
                        app.messages.push(ChatMessage::user(&text));
                        app.start_thinking();

                        let (tx, rx) = mpsc::channel::<InferenceUpdate>(64);
                        app.inference_rx = Some(rx);

                        let engine_handle = Arc::clone(&engine);
                        let user_text = text.clone();
                        let tools_enabled = app.tool_calling;
                        tokio::spawn(async move {
                            run_inference_task(engine_handle, user_text, tx, tools_enabled).await;
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::strip_think_blocks;

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
}
