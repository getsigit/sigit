//! siGit Code — minimal viable ACP agent.
//!
//! Speaks the Agent Client Protocol over stdio so editors like Zed can talk
//! to it. This is the skeleton: no LLM, no TUI — just the protocol wired
//! end-to-end with a simple echo reply.
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

use agent_client_protocol::{
    Agent, AgentCapabilities, AgentSideConnection, AuthMethod, AuthMethodAgent,
    AuthenticateRequest, AuthenticateResponse, CancelNotification, Client, ContentBlock,
    ContentChunk, Implementation, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion, SessionId,
    SessionNotification, SessionUpdate, StopReason,
};
use futures::future::LocalBoxFuture;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ── Agent ────────────────────────────────────────────────────────────────────

struct SiGitAgent {
    notification_tx: mpsc::Sender<SessionNotification>,
}

impl SiGitAgent {
    fn new(notification_tx: mpsc::Sender<SessionNotification>) -> Self {
        Self { notification_tx }
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

        log::info!(
            "prompt({}): \"{}\"",
            session_id,
            user_text.chars().take(80).collect::<String>()
        );

        // ── Echo reply ───────────────────────────────────────────────────
        // Replace this with real LLM inference later.
        let reply = format!(
            "👋 Hi from **siGit** v{}!\n\n\
             You said:\n\n\
             > {}\n\n\
             *(This is the echo-mode skeleton — no LLM attached yet.)*",
            env!("CARGO_PKG_VERSION"),
            user_text.lines().collect::<Vec<_>>().join("\n> "),
        );

        let notification = SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(reply))),
        );

        if self.notification_tx.send(notification).await.is_err() {
            log::warn!("notification channel closed");
        }

        log::info!("prompt({}) complete", session_id);
        Ok(PromptResponse::new(StopReason::EndTurn))
    }

    async fn cancel(&self, args: CancelNotification) -> agent_client_protocol::Result<()> {
        log::info!("cancel requested for session {}", args.session_id);
        Ok(())
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout is the ACP wire — all logs go to stderr.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    log::info!("siGit v{} starting", env!("CARGO_PKG_VERSION"));

    let (notification_tx, mut notification_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = SiGitAgent::new(notification_tx);

    // AgentSideConnection expects futures-io, not tokio-io.
    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();

    // ACP futures are !Send, so we need a LocalSet.
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

            // Forwarder: drains the channel and pushes chunks to the editor.
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

    log::info!("siGit shutting down");
    Ok(())
}
