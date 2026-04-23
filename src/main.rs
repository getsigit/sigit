//! siGit Code — an ACP coding agent that runs a local LLM via Onde.
//!
//! In interactive (TTY) mode **all** process output — `log::` crate events,
//! `tracing` events from mistralrs_core, and even raw `println!` calls buried
//! inside third-party crates — is redirected to `$TMPDIR/sigit.log` by
//! rewiring the stdout/stderr file descriptors with `dup2(2)` before any
//! library code runs.  Ratatui receives a private copy of the original
//! terminal fd so its rendering is unaffected.
//!
//! Two modes of operation:
//!
//! The model loads before the ACP `LocalSet` starts. This matters because
//! `mistralrs` calls `block_in_place` internally, which panics inside
//! `spawn_local` tasks. Loading on a normal multi-thread worker avoids that.
//!
//! On macOS, the HF cache points at the App Group container shared with the
//! siGit desktop app. See [`setup`].
//!
//! # Zed setup
//!
//! Add to `~/.config/zed/settings.json`:
//! ```json
//! {
//!   "agent_servers": {
//!     "siGit": {
//!       "type": "custom",
//!       "command": "/absolute/path/to/target/release/sigit"
//!     }
//!   }
//! }
//! ```

mod chat;
mod setup;

use std::io::{BufWriter, IsTerminal, Write};
use std::sync::Arc;

use agent_client_protocol::{
    Agent, AgentCapabilities, AgentSideConnection, AuthMethod, AuthMethodAgent,
    AuthenticateRequest, AuthenticateResponse, CancelNotification, Client, ContentBlock,
    ContentChunk, Implementation, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion, SessionId,
    SessionNotification, SessionUpdate, StopReason,
};
use futures::future::LocalBoxFuture;
use onde::inference::{ChatEngine, GgufModelConfig};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing_subscriber::{EnvFilter, fmt as tracing_fmt};

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};

const SYSTEM_PROMPT: &str = "\
Your name is siGit — lowercase 's', uppercase 'G', no spaces. \
Not 'SiGit', not 'Sigit'. Say 'I am siGit' when asked.

You are the official coding agent for smbCloud (https://smbcloud.xyz), \
a cloud platform for deploying and managing projects. \
You help developers build, debug, and ship software on the smbCloud platform.

Keep answers short. Write idiomatic code. \
Fix root causes, not symptoms.";

// Agent

struct SiGitAgent {
    engine: Arc<ChatEngine>,
    notification_tx: mpsc::Sender<SessionNotification>,
}

impl SiGitAgent {
    fn new(engine: Arc<ChatEngine>, notification_tx: mpsc::Sender<SessionNotification>) -> Self {
        Self {
            engine,
            notification_tx,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Agent for SiGitAgent {
    async fn initialize(
        &self,
        _args: InitializeRequest,
    ) -> agent_client_protocol::Result<InitializeResponse> {
        log::info!("initialize");

        Ok(InitializeResponse::new(ProtocolVersion::V1)
            .agent_info(
                Implementation::new("sigit", env!("CARGO_PKG_VERSION"))
                    .title("siGit — AI Coding Agent"),
            )
            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                "sigit", "siGit",
            ))])
            .agent_capabilities(AgentCapabilities::default()))
    }

    async fn authenticate(
        &self,
        _args: AuthenticateRequest,
    ) -> agent_client_protocol::Result<AuthenticateResponse> {
        log::info!("authenticate");
        Ok(AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        _args: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        log::info!("new_session: id={session_id}");

        // Clear history — the model is already loaded.
        self.engine.clear_history().await;

        Ok(NewSessionResponse::new(session_id))
    }

    async fn prompt(&self, args: PromptRequest) -> agent_client_protocol::Result<PromptResponse> {
        let session_id = args.session_id.clone();

        let user_text: String = args
            .prompt
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if user_text.trim().is_empty() {
            return Ok(PromptResponse::new(StopReason::EndTurn));
        }

        log::info!(
            "prompt({}): \"{}\"",
            session_id,
            user_text.chars().take(80).collect::<String>()
        );

        // Stream tokens from the LLM and forward each one as an ACP update.
        let mut token_rx = self
            .engine
            .stream_message(user_text)
            .await
            .map_err(|error| {
                log::error!("stream_message failed: {error}");
                agent_client_protocol::Error::new(-32603, format!("inference failed: {error}"))
            })?;

        while let Some(chunk) = token_rx.recv().await {
            if !chunk.delta.is_empty() {
                let notification = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                        chunk.delta,
                    ))),
                );

                if self.notification_tx.send(notification).await.is_err() {
                    log::warn!("notification channel closed — stopping stream");
                    break;
                }
            }

            if chunk.done {
                break;
            }
        }

        log::info!("prompt({}) complete", session_id);
        Ok(PromptResponse::new(StopReason::EndTurn))
    }

    async fn cancel(&self, args: CancelNotification) -> agent_client_protocol::Result<()> {
        log::info!("cancel requested for session {}", args.session_id);
        Ok(())
    }
}

// ── Output capture ────────────────────────────────────────────────────────────

/// Redirect **both** stdout and stderr to `$TMPDIR/sigit.log` at the
/// file-descriptor level and return a [`std::fs::File`] handle to the *real*
/// terminal (the original stdout) so ratatui can still render to it.
///
/// This is the nuclear option — it catches absolutely everything that any
/// library writes to stdout (`println!` in mistralrs `print_metadata`) or
/// stderr (`tracing::info!`, `log::info!`, raw `eprintln!`).
///
/// Returns **two** `File` handles to the real terminal (both created via
/// `dup(STDOUT)` *before* the redirect):
///
/// 1. **`tui`** — given to ratatui's `CrosstermBackend` for rendering.
/// 2. **`cleanup`** — kept by the caller for writing `LeaveAlternateScreen`
///    and restoring stdout/stderr after the TUI exits (since ratatui 0.29
///    does not expose `writer_mut()` on the backend).
#[cfg(unix)]
fn redirect_output_to_log() -> anyhow::Result<(std::fs::File, std::fs::File)> {
    let log_path = std::env::temp_dir().join("sigit.log");
    let log_file = std::fs::File::create(&log_path)?;
    let log_fd = log_file.as_raw_fd();

    // Save TWO copies of the real terminal fd before we clobber stdout.
    let saved_tui = unsafe { libc::dup(libc::STDOUT_FILENO) };
    anyhow::ensure!(
        saved_tui >= 0,
        "dup(stdout) for tui failed: {}",
        std::io::Error::last_os_error()
    );
    let saved_cleanup = unsafe { libc::dup(libc::STDOUT_FILENO) };
    anyhow::ensure!(
        saved_cleanup >= 0,
        "dup(stdout) for cleanup failed: {}",
        std::io::Error::last_os_error()
    );

    // Point stdout and stderr at the log file.
    unsafe {
        libc::dup2(log_fd, libc::STDOUT_FILENO);
        libc::dup2(log_fd, libc::STDERR_FILENO);
    }

    // `log_file` can drop — dup2 created independent references to the
    // underlying file description, so stdout/stderr keep it alive.

    Ok((unsafe { std::fs::File::from_raw_fd(saved_tui) }, unsafe {
        std::fs::File::from_raw_fd(saved_cleanup)
    }))
}

// ── Logging ───────────────────────────────────────────────────────────────────

/// Initialise `tracing-subscriber` as the single logging backend.
///
/// In TUI mode stdout/stderr have already been redirected to the log file by
/// [`redirect_output_to_log`], so the subscriber simply writes to stderr
/// (which *is* the log file).  In ACP mode stderr is the real stderr.
fn init_logging(is_tty: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(!is_tty)
        .try_init();
}

// ── Interactive TUI mode ──────────────────────────────────────────────────────

/// Start the TUI immediately, load the model concurrently, signal completion
/// via a oneshot channel so the TUI can animate the banner while waiting.
///
/// The terminal is set up *manually* against the saved real-terminal `File`
/// returned by [`redirect_output_to_log`].  Because stdout/stderr have
/// already been redirected to the log file at that point, any `println!`,
/// `eprintln!`, `log::info!`, or `tracing::info!` emitted by mistralrs or
/// onde goes straight to `$TMPDIR/sigit.log` and never touches the screen.
///
/// `tty` is given to ratatui; `cleanup_tty` is a second fd to the same
/// terminal, used for `LeaveAlternateScreen` and restoring stdout/stderr
/// (we cannot access the backend's writer because `writer_mut()` is private
/// in ratatui 0.29).
async fn run_interactive(tty: std::fs::File, mut cleanup_tty: std::fs::File) -> anyhow::Result<()> {
    let engine = ChatEngine::new();
    let config = GgufModelConfig::platform_default();
    let (load_tx, load_rx) = oneshot::channel::<Result<(), String>>();

    // Set up the terminal manually on the real tty fd.
    crossterm::terminal::enable_raw_mode()?;
    let mut tty = BufWriter::new(tty);
    crossterm::execute!(tty, crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(tty);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Drive model loading and the TUI concurrently.  Both futures hold a
    // shared `&engine` reference, which is valid because ChatEngine is Sync
    // and all its methods take `&self`.
    let (_, chat_result) = tokio::join!(
        async {
            let result = engine
                .load_gguf_model(config, Some(SYSTEM_PROMPT.to_string()), None)
                .await;
            // Ignore send errors — the TUI may have already quit (Ctrl+C).
            let _ = load_tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
        },
        chat::run_with(&mut terminal, &engine, load_rx),
    );

    // Restore the terminal before exiting.
    // Use the separate cleanup fd — the backend's writer is private.
    crossterm::execute!(cleanup_tty, crossterm::terminal::LeaveAlternateScreen)?;
    cleanup_tty.flush()?;
    crossterm::terminal::disable_raw_mode()?;

    // Restore stdout/stderr so any post-TUI error messages are visible.
    #[cfg(unix)]
    {
        let cleanup_fd = cleanup_tty.as_raw_fd();
        unsafe {
            libc::dup2(cleanup_fd, libc::STDOUT_FILENO);
            libc::dup2(cleanup_fd, libc::STDERR_FILENO);
        }
    }

    chat_result
}

// ── ACP server mode ───────────────────────────────────────────────────────────

async fn run_acp_server() -> anyhow::Result<()> {
    log::info!("ACP mode — starting agent server");

    // Load before the LocalSet. block_in_place panics inside spawn_local,
    // so the model must load on a regular worker thread.
    log::info!("loading model (this may take a minute on first run)...");

    let engine = Arc::new(ChatEngine::new());
    let config = GgufModelConfig::platform_default();

    engine
        .load_gguf_model(config, Some(SYSTEM_PROMPT.to_string()), None)
        .await
        .map_err(|error| anyhow::anyhow!("model load failed: {error}"))?;

    log::info!("model loaded and ready");

    let (notification_tx, mut notification_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = SiGitAgent::new(engine, notification_tx);

    // AgentSideConnection wants futures-io, not tokio-io.
    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();

    // ACP futures are !Send — needs a LocalSet.
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            let (conn, io_task) = AgentSideConnection::new(
                agent,
                stdout,
                stdin,
                |fut: LocalBoxFuture<'static, ()>| {
                    tokio::task::spawn_local(fut);
                },
            );

            // Forward streamed chunks to the editor.
            tokio::task::spawn_local(async move {
                while let Some(notification) = notification_rx.recv().await {
                    if let Err(err) = conn.session_notification(notification).await {
                        log::warn!("session_notification failed: {err}");
                    }
                }
            });

            // Runs until the editor disconnects.
            if let Err(err) = io_task.await {
                log::error!("ACP IO error: {err}");
            }
        })
        .await;

    log::info!("siGit shutting down");
    Ok(())
}

// ── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let is_tty = std::io::stdin().is_terminal();

    if is_tty {
        // Redirect stdout/stderr to $TMPDIR/sigit.log *first* — before any
        // library code can println!/eprintln!/log to the real terminal.
        #[cfg(unix)]
        let (tty, cleanup_tty) = redirect_output_to_log()?;
        #[cfg(not(unix))]
        anyhow::bail!("interactive mode requires Unix (macOS / Linux)");

        init_logging(true);
        setup::setup_shared_model_cache();
        run_interactive(tty, cleanup_tty).await
    } else {
        // ACP mode: no redirect needed, logs go to stderr.
        init_logging(false);
        setup::setup_shared_model_cache();
        log::info!("siGit v{} starting (ACP mode)", env!("CARGO_PKG_VERSION"));
        run_acp_server().await
    }
}
