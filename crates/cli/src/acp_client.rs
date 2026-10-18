//! The terminal UI's connection to its agent.
//!
//! The TUI is a plain ACP client of siGit Code. It starts this very binary as
//! a subprocess (`current_exe()`, stdin piped, so `main` takes the ACP branch)
//! and drives it over `session/*` the way an editor does. There is no
//! `--agent` option: it relies on sigit's own config ids, slash commands and
//! `_meta`.
//!
//! Nothing here draws anything. The connection reports what happens on one
//! unbounded channel of [`AgentEvent`]s, and the UI sends requests through
//! [`AgentConn`]. Requests that take a while (a turn, a model switch) run on
//! their own tasks and answer on the same channel, so the UI loop stays a
//! single synchronous `select!`.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, CancelNotification, ClientCapabilities, CloseSessionRequest,
    CompleteElicitationNotification, ContentBlock, CreateElicitationRequest,
    CreateElicitationResponse, DeleteSessionRequest, ElicitationAcceptAction, ElicitationAction,
    ElicitationCapabilities, ElicitationMode, ElicitationUrlCapabilities, ErrorCode,
    InitializeRequest, ListSessionsRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionConfigOption, SessionConfigOptionsCapabilities, SessionConfigValueId, SessionId,
    SessionInfo, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, StopReason, TextContent,
};
use agent_client_protocol::schema::v1::{
    BooleanConfigOptionCapabilities, ClientSessionCapabilities,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo, LineDirection};
use tokio::sync::{mpsc, oneshot};

/// How long `initialize` and `session/new` get.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// What the agent process reports, in the order the UI sees it.
pub enum AgentEvent {
    /// A `session/update` for `SessionId`. The id lets the UI drop what a
    /// thread it has since left still sends.
    Update(SessionId, Box<SessionUpdate>),
    /// The agent wants a tool call approved; the connection is waiting on the
    /// sender. Dropping it answers `Cancelled`.
    Permission(
        Box<RequestPermissionRequest>,
        oneshot::Sender<RequestPermissionOutcome>,
    ),
    /// The agent sends the user to a sign-in page (URL-mode elicitation). The
    /// UI shows `url`, tries to open it, and answers `true` to accept.
    SignIn {
        url: String,
        message: String,
        answer: oneshot::Sender<bool>,
    },
    /// The sign-in the agent asked about is done.
    SignInComplete,
    /// A `session/prompt` ended.
    TurnDone(Result<StopReason, AgentError>),
    /// A `session/set_config_option` was answered with the fresh options.
    ConfigSet(Result<Vec<SessionConfigOption>, String>),
    /// `session/list` for this directory.
    Sessions(Result<Vec<SessionInfo>, String>),
    /// `session/load` finished. The replay arrives as `Update`s before it.
    Loaded(SessionId, Result<Vec<SessionConfigOption>, String>),
    /// `session/delete` finished for this id.
    Deleted(String, Result<(), String>),
    /// `authenticate` finished.
    Authenticated(Result<(), String>),
}

/// A request the agent answered with an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    /// What to show the user.
    pub message: String,
    /// The agent says the user has to sign in first (`auth_required`).
    pub auth_required: bool,
}

impl AgentError {
    fn from_rpc(error: &agent_client_protocol::Error) -> Self {
        Self {
            message: error_text(error),
            auth_required: error.code == ErrorCode::AuthRequired,
        }
    }
}

/// What `session/new` returned.
pub struct Started {
    pub config_options: Vec<SessionConfigOption>,
    pub modes: Option<SessionModeState>,
}

/// The UI's handle on the agent. Cheap to clone.
#[derive(Clone)]
pub struct AgentConn {
    conn: ConnectionTo<Agent>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    /// The thread requests go to. `session/load` moves it.
    pub session_id: SessionId,
    /// Workspace roots, primary first.
    pub roots: Vec<PathBuf>,
    /// The `session/set_config_option` in flight, if any.
    config_task: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl AgentConn {
    /// Send `text` as a prompt. The turn ends with [`AgentEvent::TurnDone`].
    pub fn prompt(&self, text: String) {
        let conn = self.conn.clone();
        let tx = self.tx.clone();
        let request = PromptRequest::new(
            self.session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(text))],
        );
        tokio::spawn(async move {
            let result = conn.send_request(request).block_task().await;
            let _ = tx.send(AgentEvent::TurnDone(
                result
                    .map(|r| r.stop_reason)
                    .map_err(|e| AgentError::from_rpc(&e)),
            ));
        });
    }

    /// `session/cancel`: stop the turn, and whatever it is running.
    pub fn cancel(&self) {
        if let Err(error) = self
            .conn
            .send_notification(CancelNotification::new(self.session_id.clone()))
        {
            log::warn!("session/cancel failed: {error}");
        }
    }

    /// Set a select option (model, permission mode) or a toggle. The answer
    /// is [`AgentEvent::ConfigSet`]. A model switch can take minutes; stop
    /// waiting for it with [`Self::cancel_config`].
    pub fn set_config(&self, config_id: &str, value: ConfigValue) {
        let conn = self.conn.clone();
        let tx = self.tx.clone();
        let session_id = self.session_id.clone();
        let config_id = config_id.to_string();
        let handle = tokio::spawn(async move {
            let request = match value {
                ConfigValue::Id(id) => SetSessionConfigOptionRequest::new(
                    session_id,
                    config_id,
                    SessionConfigValueId::new(id),
                ),
                ConfigValue::Bool(value) => {
                    SetSessionConfigOptionRequest::new(session_id, config_id, value)
                }
            };
            let result = conn.send_request(request).block_task().await;
            let _ = tx.send(AgentEvent::ConfigSet(
                result
                    .map(|response| response.config_options)
                    .map_err(|e| error_text(&e)),
            ));
        });
        *self.config_task.lock().unwrap() = Some(handle);
    }

    /// Stop waiting for the `set_config` in flight. Dropping its request tells
    /// the agent with `$/cancel_request`; the agent stops waiting too, and the
    /// load itself finishes by itself.
    pub fn cancel_config(&self) {
        if let Some(handle) = self.config_task.lock().unwrap().take() {
            handle.abort();
        }
    }

    /// `session/list` for the working directory.
    pub fn list_sessions(&self) {
        let conn = self.conn.clone();
        let tx = self.tx.clone();
        let cwd = self.roots.first().cloned();
        tokio::spawn(async move {
            let mut request = ListSessionsRequest::new();
            request.cwd = cwd;
            let result = conn.send_request(request).block_task().await;
            let _ = tx.send(AgentEvent::Sessions(
                result
                    .map(|response| response.sessions)
                    .map_err(|e| error_text(&e)),
            ));
        });
    }

    /// `session/load`: make `session_id` the thread. The agent replays its
    /// history as updates before it answers, so the UI starts that thread's
    /// scrollback over before calling this.
    pub fn load_session(&mut self, session_id: SessionId) {
        self.session_id = session_id.clone();
        let conn = self.conn.clone();
        let tx = self.tx.clone();
        let cwd = self.roots.first().cloned().unwrap_or_default();
        let extra: Vec<PathBuf> = self.roots.iter().skip(1).cloned().collect();
        tokio::spawn(async move {
            let request =
                LoadSessionRequest::new(session_id.clone(), cwd).additional_directories(extra);
            let result = conn.send_request(request).block_task().await;
            let _ = tx.send(AgentEvent::Loaded(
                session_id,
                result
                    .map(|response| response.config_options.unwrap_or_default())
                    .map_err(|e| error_text(&e)),
            ));
        });
    }

    /// `session/delete`.
    pub fn delete_session(&self, session_id: String) {
        let conn = self.conn.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = conn
                .send_request(DeleteSessionRequest::new(session_id.clone()))
                .block_task()
                .await;
            let _ = tx.send(AgentEvent::Deleted(
                session_id,
                result.map(|_| ()).map_err(|e| error_text(&e)),
            ));
        });
    }

    /// `authenticate` with siGit Code's one method. The agent sends the
    /// authorize URL back as an elicitation ([`AgentEvent::SignIn`]) and the
    /// call returns once the page has been completed.
    pub fn authenticate(&self) {
        let conn = self.conn.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = conn
                .send_request(AuthenticateRequest::new(crate::AUTH_METHOD_ID))
                .block_task()
                .await;
            let _ = tx.send(AgentEvent::Authenticated(
                result.map(|_| ()).map_err(|e| error_text(&e)),
            ));
        });
    }

    /// `session/close`, so the agent kills this thread's background commands
    /// before the connection goes away. On Windows the process group is not
    /// killed with the child, and this is what stops them there.
    pub async fn close(&self) {
        let request = CloseSessionRequest::new(self.session_id.clone());
        let closing = self.conn.send_request(request).block_task();
        let _ = tokio::time::timeout(Duration::from_secs(3), closing).await;
    }
}

/// The value a config option is set to.
pub enum ConfigValue {
    /// A select option's value id.
    Id(String),
    /// A toggle.
    Bool(bool),
}

/// A request error as the line the UI shows. The agent's message is user
/// facing (see `BackendError`), so it goes through verbatim.
fn error_text(error: &agent_client_protocol::Error) -> String {
    let data = error
        .data
        .as_ref()
        .and_then(|data| data.as_str())
        .filter(|data| !data.is_empty());
    match data {
        Some(data) if !error.message.contains(data) => format!("{} ({data})", error.message),
        _ => error.message.clone(),
    }
}

/// Start the agent and run `main` against it until it returns. `initialize`
/// and `session/new` happen first, with the working directory as the
/// session's `cwd`; a failure to start is the returned error, with the
/// child's stderr tail in it.
pub async fn connect<R>(
    roots: Vec<PathBuf>,
    main: impl AsyncFnOnce(AgentConn, Started, mpsc::UnboundedReceiver<AgentEvent>) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    connect_to(std::env::current_exe()?, &[], roots, main).await
}

/// [`connect`] with the agent executable and extra environment given. The UI
/// always runs `current_exe()`; this exists for the integration test, which
/// runs in a different binary and points the agent at a scripted endpoint.
pub async fn connect_to<R>(
    exe: PathBuf,
    env: &[(&str, &str)],
    roots: Vec<PathBuf>,
    main: impl AsyncFnOnce(AgentConn, Started, mpsc::UnboundedReceiver<AgentEvent>) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    let cwd = roots
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no working directory"))?;
    let extra: Vec<PathBuf> = roots.iter().skip(1).cloned().collect();

    let (tx, rx) = mpsc::unbounded_channel();
    let mut config = AcpAgentConfig::new(exe).env("SIGIT_SURFACE", "tui");
    for (name, value) in env {
        config = config.env(*name, *value);
    }
    let agent = AcpAgent::new(config).with_debug(|line, direction| {
        // The client's own stderr is the log file, so the agent's lines
        // land beside its own.
        if direction == LineDirection::Stderr {
            log::info!(target: "agent", "{line}");
        }
    });

    let result = Client
        .builder()
        .name("sigit-tui")
        .on_receive_notification(
            {
                let tx = tx.clone();
                async move |notification: SessionNotification, _cx| {
                    let _ = tx.send(AgentEvent::Update(
                        notification.session_id,
                        Box::new(notification.update),
                    ));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let tx = tx.clone();
                async move |_: CompleteElicitationNotification, _cx| {
                    let _ = tx.send(AgentEvent::SignInComplete);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let tx = tx.clone();
                async move |request: RequestPermissionRequest, responder, cx| {
                    // Wait for the user off the dispatch loop, so updates keep
                    // flowing while the prompt is up.
                    let (answer_tx, answer_rx) = oneshot::channel();
                    let _ = tx.send(AgentEvent::Permission(Box::new(request), answer_tx));
                    cx.spawn(async move {
                        let outcome = answer_rx
                            .await
                            .unwrap_or(RequestPermissionOutcome::Cancelled);
                        responder.respond(RequestPermissionResponse::new(outcome))
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let tx = tx.clone();
                async move |request: CreateElicitationRequest, responder, cx| {
                    let ElicitationMode::Url(mode) = request.mode else {
                        // Only the sign-in page is ever asked for, as a URL.
                        return responder
                            .respond(CreateElicitationResponse::new(ElicitationAction::Decline));
                    };
                    let (answer_tx, answer_rx) = oneshot::channel();
                    let _ = tx.send(AgentEvent::SignIn {
                        url: mode.url,
                        message: request.message,
                        answer: answer_tx,
                    });
                    cx.spawn(async move {
                        let action = match answer_rx.await {
                            Ok(true) => ElicitationAction::Accept(ElicitationAcceptAction::new()),
                            _ => ElicitationAction::Decline,
                        };
                        responder.respond(CreateElicitationResponse::new(action))
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async |conn: ConnectionTo<Agent>| {
            // No `fs` and no `terminal`: the agent shares this machine's disk
            // and runs commands itself, with stdin closed and group kill,
            // which is already right.
            let capabilities = ClientCapabilities::new()
                .session(
                    ClientSessionCapabilities::new().config_options(
                        SessionConfigOptionsCapabilities::new()
                            .boolean(BooleanConfigOptionCapabilities::default()),
                    ),
                )
                .elicitation(ElicitationCapabilities::new().url(ElicitationUrlCapabilities::new()));
            // Neither request loads a model, so a hang here is a broken agent.
            let handshake = async {
                let initialized = conn
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(capabilities),
                    )
                    .block_task()
                    .await?;
                let session = conn
                    .send_request(NewSessionRequest::new(cwd).additional_directories(extra))
                    .block_task()
                    .await?;
                Ok::<_, agent_client_protocol::Error>((initialized, session))
            };
            let (_initialized, session) = tokio::time::timeout(START_TIMEOUT, handshake)
                .await
                .map_err(|_| {
                agent_client_protocol::Error::internal_error()
                    .data("siGit Code did not start within 30 seconds")
            })??;
            let started = Started {
                config_options: session.config_options.unwrap_or_default(),
                modes: session.modes,
            };
            let agent_conn = AgentConn {
                conn,
                tx,
                session_id: session.session_id,
                roots,
                config_task: std::sync::Arc::default(),
            };
            let outcome = main(agent_conn.clone(), started, rx).await;
            // Leaving the connection kills the agent; closing the thread first
            // lets it stop the commands it started.
            agent_conn.close().await;
            // The UI's own failure is not a protocol error; carry it out whole.
            Ok(outcome)
        })
        .await;

    match result {
        Ok(outcome) => outcome,
        Err(error) => Err(anyhow::anyhow!("{}", error_text(&error))),
    }
}
