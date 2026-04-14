//! Full-screen terminal chat UI for siGit Code.
//!
//! Takes over the alternate screen and multiplexes terminal events with
//! streaming LLM tokens via `tokio::select!`.

use std::future::pending;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use onde::inference::{ChatEngine, StreamChunk};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tokio::sync::mpsc;

// ── Message types ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    System,
    /// Banner art — rendered with per-character digit colorization.
    Banner,
}

struct ChatMessage {
    role: Role,
    text: String,
}

impl ChatMessage {
    fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
        }
    }

    fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
        }
    }

    fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            text: text.into(),
        }
    }

    fn banner(text: impl Into<String>) -> Self {
        Self {
            role: Role::Banner,
            text: text.into(),
        }
    }
}

// ── App state ────────────────────────────────────────────────────────────────

struct App {
    messages: Vec<ChatMessage>,
    input: String,
    cursor: usize,
    scroll_offset: u16,
    stream_rx: Option<mpsc::Receiver<StreamChunk>>,
    stream_buf: String,
    quit: bool,
    /// Toggled every other tick while streaming — drives the blinking cursor.
    blink_on: bool,
    blink_counter: u8,
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

impl App {
    fn new() -> Self {
        let mut messages = Vec::new();
        for line in BANNER_ART.lines() {
            messages.push(ChatMessage::banner(line));
        }
        messages.push(ChatMessage::system(""));
        messages.push(ChatMessage::system(format!(
            "siGit Code v{}",
            env!("CARGO_PKG_VERSION"),
        )));
        messages.push(ChatMessage::system(
            "In this world, nothing can be said to be certain, except death and taxes. ~ Pak Sigit",
        ));
        messages.push(ChatMessage::system("Type /help for commands."));

        Self {
            messages,
            input: String::new(),
            cursor: 0,
            scroll_offset: 0,
            stream_rx: None,
            stream_buf: String::new(),
            quit: false,
            blink_on: true,
            blink_counter: 0,
        }
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
        // tick the blink
        self.blink_counter = self.blink_counter.wrapping_add(1);
        self.blink_on = self.blink_counter % 4 < 2;
    }

    /// Total lines the messages area would need (rough estimate for scrolling).
    fn total_message_lines(&self, width: u16) -> u16 {
        if width == 0 {
            return 0;
        }
        let w = width.saturating_sub(2) as usize; // account for block borders
        let mut lines: u16 = 0;
        for msg in &self.messages {
            lines += wrapped_line_count(&msg.text, msg.role, w);
        }
        // streaming buffer
        if !self.stream_buf.is_empty() {
            lines += wrapped_line_count(&self.stream_buf, Role::Assistant, w);
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

fn banner_char_color(_ch: char) -> Color {
    Color::White
}

/// Estimate how many terminal rows a message takes once wrapped.
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

// ── Slash commands ───────────────────────────────────────────────────────────

enum SlashCommand {
    Help,
    Clear,
    Status,
    Exit,
    Unknown(String),
}

fn parse_slash(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let cmd = trimmed.split_whitespace().next().unwrap_or("");
    Some(match cmd {
        "/help" => SlashCommand::Help,
        "/clear" => SlashCommand::Clear,
        "/status" => SlashCommand::Status,
        "/exit" | "/quit" | "/q" => SlashCommand::Exit,
        other => SlashCommand::Unknown(other.to_string()),
    })
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Layout: title(1) | messages(flex) | input(3) | footer(1)
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
            " — maybe deploy later?",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).style(Style::default().bg(Color::Black)),
        area,
    );
}

fn render_messages(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line<'_>> = Vec::new();

    for msg in &app.messages {
        render_chat_message(&mut lines, msg);
    }

    // streaming partial response
    if !app.stream_buf.is_empty() || app.is_streaming() {
        let mut spans = vec![Span::styled(
            "siGit > ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )];

        // split on newlines so multi-line streaming renders correctly
        let buf_lines: Vec<&str> = app.stream_buf.split('\n').collect();
        for (i, segment) in buf_lines.iter().enumerate() {
            if i > 0 {
                lines.push(Line::from(spans.drain(..).collect::<Vec<_>>()));
                // continuation lines get no prefix
            }
            spans.push(Span::raw(segment.to_string()));
        }

        // blinking cursor while streaming
        if app.is_streaming() && app.blink_on {
            spans.push(Span::styled("█", Style::default().fg(Color::Green)));
        }

        lines.push(Line::from(spans));
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
                let spans: Vec<Span<'_>> = segment
                    .chars()
                    .map(|ch| {
                        Span::styled(ch.to_string(), Style::default().fg(banner_char_color(ch)))
                    })
                    .collect();
                lines.push(Line::from(spans));
            }
        }
    }
}

fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(if app.is_streaming() {
            " streaming… "
        } else {
            " message "
        })
        .title_style(Style::default().fg(if app.is_streaming() {
            Color::Yellow
        } else {
            Color::DarkGray
        }));

    let input_text = Paragraph::new(app.input.as_str())
        .style(Style::default().fg(if app.is_streaming() {
            Color::DarkGray
        } else {
            Color::White
        }))
        .block(block);

    frame.render_widget(input_text, area);

    // place cursor inside the input block (1 for border padding)
    if !app.is_streaming() {
        let x = area.x + app.cursor as u16 + 1;
        let y = area.y + 1;
        frame.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), y));
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let hints: &[(&str, &str)] = if app.is_streaming() {
        &[("Ctrl+C", "cancel")]
    } else {
        &[("Enter", "send"), ("/help", "commands"), ("Ctrl+C", "quit")]
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
            Style::default().fg(Color::DarkGray),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black)),
        area,
    );
}

// ── Input handling ───────────────────────────────────────────────────────────

fn handle_key(app: &mut App, key: KeyEvent) -> Option<String> {
    if key.kind != KeyEventKind::Press {
        return None;
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

// ── Slash command execution ──────────────────────────────────────────────────

async fn exec_slash(app: &mut App, cmd: SlashCommand, engine: &ChatEngine) {
    match cmd {
        SlashCommand::Help => {
            app.messages.push(ChatMessage::system(
                "/help    — show this message\n\
                 /clear   — wipe conversation history\n\
                 /status  — show engine status\n\
                 /exit    — quit chat",
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
            let info = engine.info().await;
            let model = info.model_name.as_deref().unwrap_or("(none)");
            let mem = info.approx_memory.as_deref().unwrap_or("unknown");
            app.messages.push(ChatMessage::system(format!(
                "status: {:?}  model: {}  memory: {}  history: {} turns",
                info.status, model, mem, info.history_length,
            )));
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

// ── Main loop ────────────────────────────────────────────────────────────────

/// Run the interactive chat UI. Blocks until the user quits.
///
/// The caller must have already loaded a model into `engine`.
pub async fn run(engine: &ChatEngine) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, engine).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut ratatui::DefaultTerminal, engine: &ChatEngine) -> Result<()> {
    let mut app = App::new();
    let mut event_stream = EventStream::new();

    loop {
        // draw
        terminal.draw(|frame| render(frame, &mut app))?;

        if app.quit {
            break;
        }

        // multiplex terminal events and streaming tokens
        tokio::select! {
            biased;

            // streaming chunks — only active when we have a receiver
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

            // terminal events
            maybe_event = event_stream.next() => {
                let Some(Ok(event)) = maybe_event else {
                    // stream ended or error — bail
                    break;
                };

                if let Event::Key(key) = event {
                    // while streaming, only ctrl+c/d work
                    if app.is_streaming() {
                        if key.kind == KeyEventKind::Press {
                            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                            if ctrl && (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('d')) {
                                // drop the receiver to stop reading
                                app.finalize_stream();
                                app.messages.push(ChatMessage::system("(cancelled)"));
                            }
                        }
                        continue;
                    }

                    if let Some(text) = handle_key(&mut app, key) {
                        // check for slash command first
                        if let Some(cmd) = parse_slash(&text) {
                            exec_slash(&mut app, cmd, engine).await;
                            continue;
                        }

                        // regular message — send to engine
                        app.messages.push(ChatMessage::user(&text));

                        match engine.stream_message(text).await {
                            Ok(rx) => {
                                app.stream_rx = Some(rx);
                                app.stream_buf.clear();
                                app.blink_counter = 0;
                                app.blink_on = true;
                            }
                            Err(err) => {
                                app.messages.push(ChatMessage::system(format!(
                                    "error: {err}"
                                )));
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
