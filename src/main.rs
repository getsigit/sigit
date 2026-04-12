//! siGit Code — AI coding agent powered by a local LLM via Onde Inference.
//!
//! Two modes of operation:
//!
//! - **Interactive** (stdin is a TTY): full-screen chat UI built on ratatui.
//! - **ACP server** (stdin is piped): JSON-RPC over stdio for editors like Zed.
//!
//! On macOS the model cache is shared with the siGit desktop app through an
//! App Group container. See [`setup`].
//!
//! # Zed setup (ACP mode)
//!
//! Add to `~/.config/zed/settings.json`:
//! ```json
//! {
//!   "agent_servers": {
//!     "siGit": {
//!       "command": "sigit",
//!       "args": []
//!     }
//!   }
//! }
//! ```

mod chat;
mod setup;

use std::io::IsTerminal;
use std::sync::Arc;

use agent_client_protocol::{
    Agent, AgentCapabilities, AgentSideConnection, AuthenticateRequest, AuthenticateResponse,
    CancelNotification, Client, ContentBlock, ContentChunk, Implementation, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason,
};
use futures::future::LocalBoxFuture;
use onde::inference::{ChatEngine, GgufModelConfig};
use tokio::sync::{Mutex, mpsc};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const SYSTEM_PROMPT: &str = "\
You are siGit, an expert AI coding agent integrated directly into your editor \
via the Agent Client Protocol. You specialize in:

- Code analysis, writing, and refactoring
- Bug hunting and debugging
- Git workflows and commit messages
- Software architecture and design patterns
- Code review and best practices

Be concise, precise, and practical. Write clean, idiomatic code with brief \
explanations. Identify root causes when debugging. Prefer correctness over brevity.";

// ── Per-session state ────────────────────────────────────────────────────────

/// One active session at a time. We store the `SessionId` directly (not as a
/// `String`) so `==` just works.
struct Session {
    id: SessionId,
}

// ── Agent implementation ─────────────────────────────────────────────────────

/// The actual agent. Holds one `ChatEngine` (loaded lazily on the first
/// session) and talks ACP over stdio.
struct SiGitAgent {
    engine: Arc<ChatEngine>,
    active_session: Arc<Mutex<Option<Session>>>,
    /// Sends streaming chunks to the forwarder task, which writes them out.
    notification_tx: mpsc::Sender<SessionNotification>,
}

impl SiGitAgent {
    fn new(notification_tx: mpsc::Sender<SessionNotification>) -> Self {
        Self {
            engine: Arc::new(ChatEngine::new()),
            active_session: Arc::new(Mutex::new(None)),
            notification_tx,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Agent for SiGitAgent {
    async fn initialize(
        &self,
        args: InitializeRequest,
    ) -> agent_client_protocol::Result<InitializeResponse> {
        log::info!("initialize: protocol_version={}", args.protocol_version);

        Ok(InitializeResponse::new(args.protocol_version)
            .agent_info(
                Implementation::new("sigit", env!("CARGO_PKG_VERSION"))
                    .title("siGit — AI Coding Agent"),
            )
            .agent_capabilities(AgentCapabilities::default()))
    }

    async fn authenticate(
        &self,
        _args: AuthenticateRequest,
    ) -> agent_client_protocol::Result<AuthenticateResponse> {
        // Local LLM, no credentials needed.
        Ok(AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        _args: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        log::info!("new_session: id={session_id}");

        if self.engine.is_loaded().await {
            // Model is already warm — just wipe the conversation.
            log::info!("model already loaded — clearing history for new session");
            self.engine.clear_history().await;
        } else {
            // First session — pull the model (if needed) and load it.
            log::info!("loading default model (this may take a minute on first run)...");
            let config = GgufModelConfig::platform_default();
            self.engine
                .load_gguf_model(config, Some(SYSTEM_PROMPT.to_string()), None)
                .await
                .map_err(|e| {
                    log::error!("model load failed: {e}");
                    agent_client_protocol::Error::new(-32603, format!("model load failed: {e}"))
                })?;
            log::info!("model loaded and ready");
        }

        let mut active = self.active_session.lock().await;
        *active = Some(Session {
            id: session_id.clone(),
        });

        Ok(NewSessionResponse::new(session_id))
    }

    async fn prompt(&self, args: PromptRequest) -> agent_client_protocol::Result<PromptResponse> {
        let session_id = args.session_id.clone();

        // Make sure this session actually exists.
        {
            let active = self.active_session.lock().await;
            match active.as_ref() {
                Some(s) if s.id == session_id => {}
                _ => {
                    return Err(agent_client_protocol::Error::invalid_params());
                }
            }
        }

        // Pull out the text blocks; ignore images/resources for now.
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
        let mut rx = self
            .engine
            .stream_message(user_text)
            .await
            .map_err(|e| agent_client_protocol::Error::new(-32603, e.to_string()))?;

        while let Some(chunk) = rx.recv().await {
            if !chunk.delta.is_empty() {
                let notification = SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                        chunk.delta,
                    ))),
                );
                // Forwarder gone (client disconnected?) — stop.
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
        // ChatEngine can't cancel mid-stream yet, so the stream just drains
        // when the receiver drops. Good enough for now.
        log::info!("cancel requested for session {}", args.session_id);
        Ok(())
    }
}

// ── Banner ───────────────────────────────────────────────────────────────────

fn print_banner() {
    const BANNER: &str = r#"
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
88888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888

    siGit Code v%VERSION%
"#;

    let art = BANNER.replace("%VERSION%", env!("CARGO_PKG_VERSION"));
    eprintln!("{art}");
}

// ── Interactive mode ─────────────────────────────────────────────────────────

/// Load the model, then hand off to the ratatui chat TUI.
async fn run_interactive() -> anyhow::Result<()> {
    println!("  Loading model...");

    let engine = ChatEngine::new();
    let config = GgufModelConfig::platform_default();
    engine
        .load_gguf_model(config, Some(SYSTEM_PROMPT.to_string()), None)
        .await
        .map_err(|e| anyhow::anyhow!("model load failed: {e}"))?;

    let info = engine.info().await;
    println!(
        "  \x1b[32m✓\x1b[0m {} ({})\n",
        info.model_name.as_deref().unwrap_or("unknown"),
        info.approx_memory.as_deref().unwrap_or("?"),
    );

    chat::run(&engine).await
}

// ── ACP server mode ──────────────────────────────────────────────────────────

/// Speak ACP JSON-RPC over stdio. The editor spawns us as a subprocess.
async fn run_acp_server() -> anyhow::Result<()> {
    // Agent::prompt sends chunks here; the forwarder task writes them out.
    let (notification_tx, mut notification_rx) = mpsc::channel::<SessionNotification>(256);

    let agent = SiGitAgent::new(notification_tx);

    // AgentSideConnection wants futures-io, not tokio-io.
    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();

    // ACP futures are !Send, so we need a LocalSet.
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            // Wire up the ACP connection. The closure spawns its internal IO tasks.
            let (conn, io_task) = AgentSideConnection::new(
                agent,
                stdout,
                stdin,
                |fut: LocalBoxFuture<'static, ()>| {
                    tokio::task::spawn_local(fut);
                },
            );

            // Forwarder: drains the mpsc channel and pushes chunks to the client.
            tokio::task::spawn_local(async move {
                while let Some(notification) = notification_rx.recv().await {
                    if let Err(err) = conn.session_notification(notification).await {
                        log::warn!("session_notification failed: {err}");
                    }
                }
            });

            // Blocks until the editor disconnects.
            if let Err(err) = io_task.await {
                log::error!("ACP IO error: {err}");
            }
        })
        .await;

    Ok(())
}

// ── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs always go to stderr (stdout is either the TUI or the ACP wire).
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    // Shared model cache (macOS App Group) — must run before anything
    // touches hf-hub or ChatEngine.
    setup::setup_shared_model_cache();

    if std::io::stdin().is_terminal() {
        // Interactive mode — full-screen chat TUI.
        print_banner();
        run_interactive().await
    } else {
        // Editor spawned us — speak ACP over stdio.
        log::info!("siGit v{} starting (ACP mode)", env!("CARGO_PKG_VERSION"));
        run_acp_server().await
    }
}
