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
        Response(String),
        Error(String),
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
        scroll_offset: u16,
        stream_rx: Option<mpsc::Receiver<StreamChunk>>,
        stream_buf: String,
        inference_rx: Option<mpsc::Receiver<InferenceUpdate>>,
        model_load_rx: Option<mpsc::Receiver<ModelLoadUpdate>>,
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
                current_model_name: crate::setup::load_selected_model_name()
                    .unwrap_or_else(|| load_model_name.clone()),
                tool_calling,
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
            self.messages.push(ChatMessage::system(format!(
                "Current model: {}",
                self.current_model_name
            )));
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

        /// rough line count for scroll math
        fn total_message_lines(&self, width: u16) -> u16 {
            if width == 0 {
                return 0;
            }
            let w = width.saturating_sub(2) as usize;
            let mut lines: u16 = 0;
            for msg in &self.messages {
                lines += wrapped_line_count(&msg.text, msg.role, w);
            }
            if !self.stream_buf.is_empty() {
                lines += wrapped_line_count(&self.stream_buf, Role::Assistant, w);
            }
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

    // ── Slash commands ────────────────────────────────────────────────────────

    enum SlashCommand {
        Help,
        Clear,
        Status,
        /// picker UI, or jump straight to model N
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

    fn render_messages(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
        let inner_width = area.width.saturating_sub(2);
        let inner_height = area.height.saturating_sub(2);

        app.auto_scroll(inner_height, area.width);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();

        for msg in &app.messages {
            render_chat_message(&mut lines, msg, inner_width as usize);
        }

        if !app.stream_buf.is_empty() {
            let fake = ChatMessage {
                role: Role::Assistant,
                text: app.stream_buf.clone(),
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
            let frame_str = app.switching_frame();
            let progress_str = if let Some((downloaded, expected)) = app.download_progress {
                if expected > 0 {
                    let pct = (downloaded as f64 / expected as f64 * 100.0).min(100.0) as u8;
                    let dl_str = format_size_human(downloaded);
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
                format!("  {frame_str} switching model{progress_str}…"),
                Style::default().fg(Color::DarkGray),
            )));
        }

        let total_lines = lines.len() as u16;
        let scroll = if total_lines > inner_height {
            app.scroll_offset.min(total_lines - inner_height)
        } else {
            0
        };

        frame.render_widget(
            Paragraph::new(lines)
                .scroll((scroll, 0))
                .wrap(Wrap { trim: false }),
            inner,
        );
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
                let mut first = true;
                for text_line in msg.text.split('\n') {
                    if first {
                        lines.push(Line::from(vec![
                            prefix.clone(),
                            Span::raw(text_line.to_string()),
                        ]));
                        first = false;
                    } else {
                        lines.push(Line::from(Span::raw(format!("         {text_line}"))));
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

        if app.thinking || app.switching_model || app.is_streaming() {
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

                            let loading_msg = if model.cache_health
                                == ModelCacheHealth::NotDownloaded
                            {
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
                            app.switching_model_id = Some(model.config.model_id.clone());
                            // Only show download progress for models not yet cached.
                            app.download_progress =
                                if model.cache_health == ModelCacheHealth::NotDownloaded {
                                    Some((0, 0))
                                } else {
                                    None
                                };

                            let sampling = SamplingConfig {
                                max_tokens: Some(model.max_tokens),
                                ..SamplingConfig::default()
                            };

                            // own thread + runtime so block_in_place doesn't starve the TUI loop
                            let system_prompt = crate::system_prompt_for_model(model.tool_calling);
                            let engine_handle = Arc::clone(&engine);
                            let tool_calling = model.tool_calling;
                            std::thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new()
                                    .expect("failed to create model-loader runtime");
                                let update = rt.block_on(async move {
                                    match engine_handle
                                        .load_gguf_model(
                                            model.config.clone(),
                                            Some(system_prompt.to_string()),
                                            Some(sampling),
                                        )
                                        .await
                                    {
                                        Ok(_) => {
                                            ModelLoadUpdate::Loaded(model.display_name.clone())
                                        }
                                        Err(err) => ModelLoadUpdate::Error(err.to_string()),
                                    }
                                });
                                // capacity-1 channel, receiver alive while switching
                                let _ = tx.blocking_send(update);
                            });
                            // applied on ModelLoadUpdate::Loaded
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

    // ── Background inference task ─────────────────────────────────────────────

    /// cap tool rounds so a confused model can't loop forever
    const MAX_TOOL_ROUNDS: usize = 10;

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

    /// run the tool-calling loop off the main thread, posting updates via `tx`.
    /// dropping `tx` signals completion to the event loop.
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

                let _ = tx
                    .send(InferenceUpdate::ToolUse(tc.function_name.clone()))
                    .await;

                let output = crate::tools::execute_tool(&tc.function_name, &tc.arguments).await;
                log::info!("  ← {} chars", output.len());

                tool_results.push(ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: output,
                });
            }

            // on the last round, pass no tools so the model must produce text
            let next_tools = if round < MAX_TOOL_ROUNDS {
                Some(onde_tools.as_slice())
            } else {
                None
            };

            match engine.send_tool_results(tool_results, next_tools).await {
                Ok(r) => result = r,
                Err(err) => {
                    let _ = tx.send(InferenceUpdate::Error(err.to_string())).await;
                    return;
                }
            }
        }

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
            } else {
                let _ = tx.send(InferenceUpdate::Response(result.text)).await;
            }
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
                        // sender dropped without done=true
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
                            // task finished, possibly with no text to show
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

                            // ── spawn inference ──────────────────────────────
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
