//! siGit Code — an ACP coding agent that runs a local LLM via Onde.
//!
//! Talks the Agent Client Protocol over stdio. Zed (or any ACP client) spawns
//! the binary and communicates through stdin/stdout.
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

mod setup;

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
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const SYSTEM_PROMPT: &str = "\
Your name is siGit — lowercase 's', uppercase 'G', no spaces. \
Not 'SiGit', not 'Sigit'. Say 'I am siGit' when asked.

You are a coding agent running inside the user's editor. \
You read and write code, debug problems, review patches, \
suggest architecture, and help with git.

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

// Entry point

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ACP uses stdout, so logs go to stderr.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    log::info!("siGit v{} starting", env!("CARGO_PKG_VERSION"));

    // On macOS, share the HF cache with the desktop app via App Group container.
    setup::setup_shared_model_cache();

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

    // ACP server

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
