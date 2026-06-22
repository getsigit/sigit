//! siGit Code — local coding agent on Onde Inference.
//!
//! In TTY mode, all output (log crate, tracing, stray printlns) redirects to
//! `$TMPDIR/sigit.log`. Ratatui holds a separate fd to the real terminal so
//! the TUI stays clean.
//!
//! Two modes:
//! - ACP over stdio (editor integration, e.g. Zed)
//! - interactive terminal (direct TTY)
//!
//! Interactive mode is Unix-only — it needs fd redirection to keep logs out
//! of the TUI. Windows only gets ACP mode for now.
//!
//! On macOS the HF cache lives in the App Group container shared with the
//! siGit desktop app. See [`setup`].
//!
//! # Zed setup
//!
//! Add to `~/.config/zed/settings.json`:
//! ```json
//! {
//!   "agent_servers": {
//!     "siGit Code": {
//!       "type": "custom",
//!       "command": "/absolute/path/to/target/release/sigit"
//!     }
//!   }
//! }
//! ```

mod account;
mod backend;
mod chat;
mod credentials;
mod models;
mod provider;
mod setup;
mod tools;

use std::io::IsTerminal;
#[cfg(unix)]
use std::io::{BufWriter, Write};
use std::sync::Arc;

use onde::inference::SamplingConfig;

use agent_client_protocol::schema::{
    AgentCapabilities, AuthMethod, AuthMethodAgent, AuthenticateRequest, AuthenticateResponse,
    CancelNotification, ContentBlock, ContentChunk, EmbeddedResourceResource, ForkSessionRequest,
    ForkSessionResponse, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, Meta, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ProtocolVersion, SessionCapabilities, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigValueId,
    SessionForkCapabilities, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Responder};
use onde::inference::{ChatEngine, GgufModelConfig, ToolDefinition, ToolResult};

use crate::backend::{InferenceBackend, LocalBackend, OpenAiBackend};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing_subscriber::{EnvFilter, fmt as tracing_fmt};

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};

const SYSTEM_PROMPT: &str = "\
Your name is siGit — lowercase 's', uppercase 'G', no spaces. \
Not 'SiGit', not 'Sigit'. Only say your name if the user asks who you are.

You are a strong general-purpose coding agent. smbCloud is your home turf, \
but you should still be useful in any codebase. When the project is clearly \
about smbCloud, use that context directly instead of falling back to vague \
cloud-platform advice.

smbCloud context you should know and use when it helps:
- smbCloud is a platform for deploying and managing projects
- the main CLI is a Rust workspace with focused crates rather than one giant crate
- common areas include auth, project management, deploy flows, networking, \
  shared models, release tooling, and managed services
- deploy branches usually follow `release/service-{name}`
- Next.js SSR deploys on smbCloud are not the same as generic git-push deploys; \
  they often use a local build plus rsync/PM2 style flow
- auth has a hard boundary between smbCloud platform users and tenant app users; \
  platform flows use `/v1/users*`, tenant app flows use `/v1/client/*`, and \
  you should not casually mix `User`, `TenantMembership`, `AuthApp`, and `AuthUser`
- smbCloud authorization is layered; do not flatten platform accounts, tenant \
  memberships, auth-app collaborators, and tenant end users into one model
- `Project` is the umbrella workspace, while app-like resources such as \
  `FrontendApp`, `AuthApp`, and GresIQ are the deployable units with their own \
  ownership, sharing, and collaboration rules
- `FrontendApp` is many-per-project, while `AuthApp` is intentionally one-per-project; \
  preserve those cardinality rules unless the code clearly changes them
- GresIQ is smbCloud's managed PostgreSQL offering; treat it as a platform \
  service with its own credentials and boundaries, not as a generic local DB helper
- when debugging smbCloud Rails APIs, first classify the request: first-party \
  smbCloud app or tenant app, then check which endpoint family and validator \
  should be involved before changing code
- when working in smbCloud repos, prefer existing workspace patterns, existing \
  crate boundaries, existing Rails conventions, and existing command flows over \
  inventing new abstractions

CRITICAL RULE — never tell the user to run a command. You have tools. Use them. \
When the user asks you to clone a repo, run a build, check git status, or do \
anything that involves a shell command, you MUST call the run_command tool and \
execute it yourself. Do not print shell commands for the user to copy-paste. \
Do not give step-by-step instructions. Do not say \"you can run …\". Just do it. \
If a command fails, try to fix the problem and re-run it. If you cannot fix it \
after two attempts, explain what went wrong and what you tried.

Git operations — always use run_command:
- git clone: always pass the full absolute destination path as the last argument \
  and set cwd to an existing writable parent directory. Example: \
  run_command({\"command\": \"git clone https://github.com/org/repo /Users/me/Repositories/repo\", \
  \"cwd\": \"/Users/me/Repositories\"})
- git init, add, commit, push, pull, fetch, checkout, branch, diff, log, status, \
  stash, rebase, merge, tag — use run_command with an absolute cwd pointing to \
  the repo root
- never run git clone without an explicit absolute destination path
- if a clone or init fails, check the error, fix the cause (wrong path, missing \
  directory, permissions), and retry

Never introduce yourself unless asked. Jump straight into the answer. \
Keep answers short. Write idiomatic code. \
Fix root causes, not symptoms.

You have access to tools that let you read files, read websites directly from \
http and https URLs, create directories, list directories, search code, create \
new files, edit existing files, delete files, and run shell commands. You can \
also use git directly through shell commands, including `git init` and normal \
git workflows. Use them proactively. Read the code or website before answering. \
Prefer absolute paths when referring to files and directories, especially in \
protocol-facing output and tool arguments. Create directories when needed. Run \
builds, tests, and git commands after making changes. Ground your answers in \
the actual code or fetched page content, not in guesses.

CRITICAL — you CAN access websites. You are NOT a typical LLM without internet \
access. You have a read_website tool that fetches any http or https URL and \
returns the page text. When the user gives you a URL or asks you to read, \
summarize, or inspect a web page, you MUST call the read_website tool with that \
URL. Never say \"I cannot access websites\" or \"I cannot browse the internet\". \
You can. Use the tool.

CRITICAL — before every edit_file call, you MUST call read_file on the target \
file first (or the specific line range if one was given). Never rely on file \
content you saw in a previous turn — the user may have reverted, edited, or \
changed the file externally since then. Always re-read to get the current state \
before constructing old_text. \
When the user corrects a previous edit (e.g. \"don't remove X, append instead\"), \
treat it as a fresh task: re-read the file, identify the current content, and \
plan the edit from scratch. Do not assume the file still reflects your last edit.

Tool-use heuristics:
- when the user provides a URL or asks about a web page, ALWAYS call \
  read_website — never refuse or claim you lack internet access
- prefer absolute paths over relative paths when you mention, return, or pass \
  file and directory paths
- if a path does not exist yet, create the directory before creating files in it
- if the user asks to clone a repo, immediately call run_command with git clone \
  and an absolute destination path — do not ask where to put it unless the \
  request is ambiguous; default to the user's home Repositories directory
- if the user asks for a new repo, scaffold, or scratch project, create the \
  directory, create the first files, and run `git init` without waiting unless \
  the request says otherwise
- if the repo looks like smbCloud CLI code, respect workspace crate boundaries, \
  shared models, and existing command handlers before adding new abstractions
- if the repo looks like smbCloud Rails code, check routes, controllers, \
  validators, and model boundaries before changing business logic
- if the task touches smbCloud auth, first decide whether it is a platform-user \
  flow or a tenant-app flow, then follow the right endpoint family and model layer
- if the task touches smbCloud deploy code, check whether it is the generic \
  deploy path or the Next.js SSR path before proposing changes
- after edits, prefer running the smallest useful verification step first, then \
  widen to broader checks if needed
- use git commands naturally for status checks, repo setup, diffs, and normal \
  developer workflows when they help move the task forward
- if a tool call fails, read the error, try to fix it, and retry — do not \
  fall back to telling the user what to type

When the repo is not about smbCloud, act like a normal coding agent and do not \
force smbCloud-specific advice into the answer. When it is about smbCloud, be \
specific and practical.

Be direct and brief. Write clean, idiomatic code. When debugging, go for the \
root cause, not the symptom. Correct beats clever.";

/// shorter prompt for models without tool calling (e.g. DeepSeek Coder v1).
/// the full [`SYSTEM_PROMPT`] wastes context and confuses them.
const SIMPLE_SYSTEM_PROMPT: &str = "\
Your name is siGit — a coding assistant. \
You are helpful, concise, and write clean, idiomatic code. \
Answer any question the user asks — programming, general knowledge, or casual chat. \
When debugging, address the root cause, not the symptom. \
Be direct and brief.";

pub(crate) fn system_prompt_for_model(tool_calling: bool) -> &'static str {
    if tool_calling {
        SYSTEM_PROMPT
    } else {
        SIMPLE_SYSTEM_PROMPT
    }
}

/// cap tool-call loops so a confused model can't spin forever
const MAX_TOOL_ROUNDS: usize = 10;

fn agent_tools_as_onde() -> Vec<ToolDefinition> {
    tools::all_tools()
        .into_iter()
        .map(|t| ToolDefinition {
            name: t.name.to_string(),
            description: t.description.to_string(),
            parameters_schema: t.parameters_schema.to_string(),
        })
        .collect()
}

fn initialize_meta() -> Meta {
    let startup_selection = setup::startup_model_selection();

    let active_model_name = startup_selection
        .as_ref()
        .map(|selection| selection.display_name.clone())
        .unwrap_or_else(|| GgufModelConfig::qwen25_3b().display_name);

    let active_model_id = startup_selection
        .as_ref()
        .and_then(|selection| selection.selected_model.as_ref())
        .map(|selected| selected.model_id.clone())
        .unwrap_or_else(|| GgufModelConfig::qwen25_3b().model_id);

    let active_model_file = startup_selection
        .as_ref()
        .and_then(|selection| selection.selected_model.as_ref())
        .map(|selected| selected.gguf_file.clone())
        .unwrap_or_else(|| {
            GgufModelConfig::qwen25_3b()
                .files
                .first()
                .cloned()
                .unwrap_or_default()
        });

    let mut model = serde_json::Map::new();
    model.insert(
        "display_name".to_string(),
        serde_json::Value::String(active_model_name),
    );
    model.insert(
        "model_id".to_string(),
        serde_json::Value::String(active_model_id),
    );
    model.insert(
        "gguf_file".to_string(),
        serde_json::Value::String(active_model_file),
    );

    let mut sigit = serde_json::Map::new();
    sigit.insert("active_model".to_string(), serde_json::Value::Object(model));

    let mut meta = Meta::new();
    meta.insert("sigit".to_string(), serde_json::Value::Object(sigit));
    meta
}

struct SiGitAgent {
    engine: Arc<ChatEngine>,
    /// cwd from the editor — tool calls run here, not where the process started
    session_cwd: std::sync::Mutex<Option<PathBuf>>,
    current_model: std::sync::Mutex<GgufModelConfig>,
    /// flipped once the startup model finishes (success or failure)
    model_ready: Arc<AtomicBool>,
    /// guards the one-time lazy startup load for ACP mode
    startup_model_load_started: Arc<AtomicBool>,
    /// set if the startup load failed
    model_load_error: Arc<std::sync::Mutex<Option<String>>>,
    /// true when the startup model isn't cached yet
    startup_needs_download: bool,
    /// for progress UI
    startup_model_name: String,
    /// for download-progress polling
    startup_model_id: String,
}

impl SiGitAgent {
    fn new(
        engine: Arc<ChatEngine>,
        initial_model: GgufModelConfig,
        model_ready: Arc<AtomicBool>,
        startup_model_load_started: Arc<AtomicBool>,
        model_load_error: Arc<std::sync::Mutex<Option<String>>>,
        startup_needs_download: bool,
    ) -> Self {
        let startup_model_name = initial_model.display_name.clone();
        let startup_model_id = initial_model.model_id.clone();
        Self {
            engine,
            session_cwd: std::sync::Mutex::new(None),
            current_model: std::sync::Mutex::new(initial_model),
            model_ready,
            startup_model_load_started,
            model_load_error,
            startup_needs_download,
            startup_model_name,
            startup_model_id,
        }
    }

    fn start_startup_model_load_if_needed(&self) {
        if self
            .startup_model_load_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        self.model_ready.store(false, Ordering::Release);
        if let Ok(mut guard) = self.model_load_error.lock() {
            *guard = None;
        }

        let startup_config = self.current_model.lock().unwrap().clone();
        let (max_tokens, tool_calling) = models::local_picker_items()
            .into_iter()
            .find(|item| {
                item.config.model_id == startup_config.model_id
                    && item
                        .config
                        .files
                        .first()
                        .zip(startup_config.files.first())
                        .map(|(left, right)| left == right)
                        .unwrap_or(false)
            })
            .map(|item| (item.max_tokens, item.tool_calling))
            .unwrap_or((4096, false));

        let sampling = SamplingConfig {
            max_tokens: Some(max_tokens),
            ..SamplingConfig::default()
        };

        let loader_engine = Arc::clone(&self.engine);
        let loader_system_prompt = system_prompt_for_model(tool_calling).to_string();
        let model_ready = Arc::clone(&self.model_ready);
        let model_load_error = Arc::clone(&self.model_load_error);

        std::thread::spawn(move || {
            let result = tokio::runtime::Runtime::new()
                .map_err(|error| error.to_string())
                .and_then(|rt| {
                    rt.block_on(loader_engine.load_gguf_model(
                        startup_config,
                        Some(loader_system_prompt),
                        Some(sampling),
                    ))
                    .map(|_| ())
                    .map_err(|error| error.to_string())
                });

            if let Ok(mut guard) = model_load_error.lock() {
                *guard = result.err();
            }
            model_ready.store(true, Ordering::Release);
        });
    }

    /// block until the startup model is ready, showing progress in the session.
    async fn await_model_ready(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: &SessionId,
    ) -> agent_client_protocol::Result<()> {
        if self.model_ready.load(Ordering::Acquire) {
            // already done — might be a stored error from earlier
            if let Some(err) = self.model_load_error.lock().unwrap().as_ref() {
                return Err(agent_client_protocol::Error::new(
                    -32603,
                    format!("model load failed: {err}"),
                ));
            }
            return Ok(());
        }

        const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

        let tool_call_id = format!("startup-load-{}", uuid::Uuid::new_v4());
        let title = if self.startup_needs_download {
            format!("Downloading {}", self.startup_model_name)
        } else {
            format!("Loading {}", self.startup_model_name)
        };

        self.send_tool_call_update(
            cx,
            session_id.clone(),
            SessionUpdate::ToolCall(
                ToolCall::new(tool_call_id.clone(), &title)
                    .kind(ToolKind::Think)
                    .status(ToolCallStatus::InProgress)
                    .content(vec![format!("{}…", title).into()]),
            ),
        )
        .ok();

        let expected_bytes = if self.startup_needs_download {
            onde::inference::models::SUPPORTED_MODEL_INFO
                .iter()
                .find(|m| m.id == self.startup_model_id)
                .map(|m| m.expected_size_bytes)
                .unwrap_or(0)
        } else {
            0
        };

        let load_start = std::time::Instant::now();
        let mut tick: usize = 0;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.tick().await;

        loop {
            interval.tick().await;
            tick += 1;

            if self.model_ready.load(Ordering::Acquire) {
                break;
            }

            let frame = SPINNER[tick % SPINNER.len()];
            let elapsed = load_start.elapsed();
            let elapsed_str = if elapsed.as_secs() >= 60 {
                format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
            } else {
                format!("{}s", elapsed.as_secs())
            };

            let (update_title, update_content) =
                if self.startup_needs_download && expected_bytes > 0 {
                    let cache_path = onde::hf_cache::model_cache_path(&self.startup_model_id);
                    let downloaded = cache_path
                        .as_ref()
                        .filter(|p| p.exists())
                        .map(|p| dir_size_recursive(p))
                        .unwrap_or(0);
                    let pct = ((downloaded as f64 / expected_bytes as f64) * 100.0).min(99.0) as u8;
                    let bar = progress_bar(pct, 20);
                    let size_hint = format!(" (~{})", format_size_human(expected_bytes));
                    (
                        format!(
                            "{frame} Downloading {}{size_hint} ({pct}%)",
                            self.startup_model_name
                        ),
                        format!(
                            "{} — {bar} {pct}%  ({} / {})",
                            self.startup_model_name,
                            format_size_human(downloaded),
                            format_size_human(expected_bytes),
                        ),
                    )
                } else if self.startup_needs_download {
                    let cache_path = onde::hf_cache::model_cache_path(&self.startup_model_id);
                    let downloaded = cache_path
                        .as_ref()
                        .filter(|p| p.exists())
                        .map(|p| dir_size_recursive(p))
                        .unwrap_or(0);
                    (
                        format!("{frame} Downloading {}", self.startup_model_name),
                        format!(
                            "{} — {} downloaded… ({elapsed_str})",
                            self.startup_model_name,
                            format_size_human(downloaded),
                        ),
                    )
                } else {
                    (
                        format!("{frame} Loading {}", self.startup_model_name),
                        format!(
                            "{frame} Loading {}… ({elapsed_str})",
                            self.startup_model_name
                        ),
                    )
                };

            self.send_tool_call_update(
                cx,
                session_id.clone(),
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    tool_call_id.clone(),
                    ToolCallUpdateFields::new()
                        .title(update_title)
                        .status(ToolCallStatus::InProgress)
                        .content(vec![update_content.into()]),
                )),
            )
            .ok();
        }

        // done — check if it blew up
        let load_error = self.model_load_error.lock().unwrap().clone();
        if let Some(err) = load_error {
            self.send_tool_call_update(
                cx,
                session_id.clone(),
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    tool_call_id,
                    ToolCallUpdateFields::new()
                        .title("Model load failed".to_string())
                        .status(ToolCallStatus::Failed)
                        .content(vec![format!("error: {err}").into()]),
                )),
            )
            .ok();

            return Err(agent_client_protocol::Error::new(
                -32603,
                format!("model load failed: {err}"),
            ));
        }

        let done_title = if self.startup_needs_download {
            format!("✓ {} downloaded and loaded", self.startup_model_name)
        } else {
            format!("✓ {} loaded", self.startup_model_name)
        };

        self.send_tool_call_update(
            cx,
            session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id,
                ToolCallUpdateFields::new()
                    .title(done_title)
                    .status(ToolCallStatus::Completed),
            )),
        )
        .ok();

        Ok(())
    }

    fn send_assistant_message(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: SessionId,
        text: impl Into<String>,
    ) -> agent_client_protocol::Result<()> {
        cx.send_notification(SessionNotification::new(
            session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(text.into()))),
        ))
    }

    fn send_tool_call_update(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: SessionId,
        update: SessionUpdate,
    ) -> agent_client_protocol::Result<()> {
        cx.send_notification(SessionNotification::new(session_id, update))
    }

    async fn switch_model_by_id(
        &self,
        model_id: &str,
    ) -> agent_client_protocol::Result<GgufModelConfig> {
        let (new_config, max_tokens, new_tool_calling) = resolve_model_config(model_id)
            .ok_or_else(|| {
                agent_client_protocol::Error::new(
                    -32602,
                    format!("unknown or unavailable model: {model_id}"),
                )
            })?;

        log::info!(
            "switching model to {} (max_tokens={max_tokens})",
            new_config.display_name
        );

        let sampling = SamplingConfig {
            max_tokens: Some(max_tokens),
            ..SamplingConfig::default()
        };

        // block_in_place inside spawn_local panics, so run the load on a
        // dedicated thread with its own runtime (same trick as startup)
        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let loader_engine = Arc::clone(&self.engine);
        let loader_config = new_config.clone();
        let loader_system_prompt = system_prompt_for_model(new_tool_calling).to_string();
        let loader_sampling = sampling;

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("failed to create loader runtime");
            let result = rt.block_on(async move {
                // load_gguf_model already unloads the old model internally;
                // calling unload first would leave a gap where prompts fail
                loader_engine
                    .load_gguf_model(
                        loader_config,
                        Some(loader_system_prompt),
                        Some(loader_sampling),
                    )
                    .await
            });
            let _ = result_tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
        });

        result_rx
            .await
            .map_err(|_| agent_client_protocol::Error::new(-32603, "model loader thread crashed"))?
            .map_err(|error| {
                log::error!("model switch failed: {error}");
                agent_client_protocol::Error::new(-32603, format!("model switch failed: {error}"))
            })?;

        self.startup_model_load_started
            .store(true, Ordering::Release);
        self.model_ready.store(true, Ordering::Release);
        if let Ok(mut guard) = self.model_load_error.lock() {
            *guard = None;
        }

        if let Some(item) = models::local_picker_items()
            .iter()
            .find(|item| item.config.model_id == new_config.model_id)
            && let Err(err) = setup::save_selected_model(&setup::SelectedModel {
                model_id: item.config.model_id.clone(),
                gguf_file: item.config.files.first().cloned().unwrap_or_default(),
            })
        {
            log::warn!("failed to persist model selection: {err}");
        }

        {
            let mut guard = self.current_model.lock().unwrap();
            *guard = new_config.clone();
        }

        if let Some(cwd) = self.session_cwd.lock().ok().and_then(|g| g.clone()) {
            self.engine
                .push_history(onde::inference::ChatMessage::system(format!(
                    "The user's project working directory is {}. \
                     Always use absolute paths under this directory for all file \
                     and directory operations. This is the root of the project \
                     the user has open in their editor.",
                    cwd.display()
                )))
                .await;
        }

        Ok(new_config)
    }
}

// ── ACP handler implementations ───────────────────────────────────────────────

impl SiGitAgent {
    async fn handle_initialize(
        &self,
        _req: InitializeRequest,
    ) -> agent_client_protocol::Result<InitializeResponse> {
        log::info!("initialize");

        Ok(InitializeResponse::new(ProtocolVersion::V1)
            .agent_info(
                Implementation::new("sigit", env!("CARGO_PKG_VERSION"))
                    .title("siGit Code - AI Coding Agent"),
            )
            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                "sigit",
                "siGit Code",
            ))])
            .agent_capabilities(
                AgentCapabilities::default()
                    .load_session(true)
                    .session_capabilities(
                        SessionCapabilities::new().fork(SessionForkCapabilities::new()),
                    ),
            )
            .meta(initialize_meta()))
    }

    async fn handle_authenticate(
        &self,
        _req: AuthenticateRequest,
    ) -> agent_client_protocol::Result<AuthenticateResponse> {
        log::info!("authenticate");
        Ok(AuthenticateResponse::default())
    }

    async fn handle_load_session(
        &self,
        args: LoadSessionRequest,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        log::info!(
            "load_session: id={}, cwd={}, additional_directories={:?}",
            args.session_id,
            args.cwd.display(),
            args.additional_directories
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );

        if let Ok(mut guard) = self.session_cwd.lock() {
            *guard = Some(args.cwd.clone());
        }

        // tool calls use relative paths, so we need to match the editor's cwd
        if args.cwd.is_dir()
            && let Err(err) = std::env::set_current_dir(&args.cwd)
        {
            log::warn!("could not set cwd to {}: {err}", args.cwd.display());
        }

        // no session persistence, so "load" just resets
        self.engine.clear_history().await;

        self.engine
            .push_history(onde::inference::ChatMessage::system(format!(
                "The user's project working directory is {}. \
                 Always use absolute paths under this directory for all file \
                 and directory operations. This is the root of the project \
                 the user has open in their editor.",
                args.cwd.display()
            )))
            .await;

        let config_options = {
            let guard = self.current_model.lock().unwrap();
            build_model_config_options(&guard)
        };

        Ok(LoadSessionResponse::new().config_options(config_options))
    }

    async fn handle_fork_session(
        &self,
        args: ForkSessionRequest,
    ) -> agent_client_protocol::Result<ForkSessionResponse> {
        let new_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        log::info!(
            "fork_session: from={} new={new_id}, cwd={}, additional_directories={:?}",
            args.session_id,
            args.cwd.display(),
            args.additional_directories
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );

        if let Ok(mut guard) = self.session_cwd.lock() {
            *guard = Some(args.cwd.clone());
        }
        if args.cwd.is_dir()
            && let Err(err) = std::env::set_current_dir(&args.cwd)
        {
            log::warn!("could not set cwd to {}: {err}", args.cwd.display());
        }

        // no persistence, so fork == fresh session
        self.engine.clear_history().await;

        self.engine
            .push_history(onde::inference::ChatMessage::system(format!(
                "The user's project working directory is {}. \
                 Always use absolute paths under this directory for all file \
                 and directory operations. This is the root of the project \
                 the user has open in their editor.",
                args.cwd.display()
            )))
            .await;

        let config_options = {
            let guard = self.current_model.lock().unwrap();
            build_model_config_options(&guard)
        };

        Ok(ForkSessionResponse::new(new_id).config_options(config_options))
    }

    async fn handle_new_session(
        &self,
        args: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        log::info!(
            "new_session: id={session_id}, cwd={}, additional_directories={:?}",
            args.cwd.display(),
            args.additional_directories
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        );

        if let Ok(mut guard) = self.session_cwd.lock() {
            *guard = Some(args.cwd.clone());
        }
        if args.cwd.is_dir()
            && let Err(err) = std::env::set_current_dir(&args.cwd)
        {
            log::warn!("could not set cwd to {}: {err}", args.cwd.display());
        }

        self.engine.clear_history().await;

        self.engine
            .push_history(onde::inference::ChatMessage::system(format!(
                "The user's project working directory is {}. \
                 Always use absolute paths under this directory for all file \
                 and directory operations. This is the root of the project \
                 the user has open in their editor.",
                args.cwd.display()
            )))
            .await;

        let config_options = {
            let guard = self.current_model.lock().unwrap();
            build_model_config_options(&guard)
        };

        Ok(NewSessionResponse::new(session_id).config_options(config_options))
    }

    async fn handle_prompt(
        &self,
        cx: &ConnectionTo<Client>,
        args: PromptRequest,
    ) -> agent_client_protocol::Result<PromptResponse> {
        let session_id = args.session_id.clone();

        // log every block so we can debug @ references and file context
        for (i, block) in args.prompt.iter().enumerate() {
            match block {
                ContentBlock::Text(t) => {
                    log::info!(
                        "prompt({}) block[{}]: Text({} chars) = \"{}\"",
                        session_id,
                        i,
                        t.text.len(),
                        t.text.chars().take(200).collect::<String>()
                    );
                }
                ContentBlock::Resource(embedded) => {
                    log::info!(
                        "prompt({}) block[{}]: EmbeddedResource = {:?}",
                        session_id,
                        i,
                        match &embedded.resource {
                            EmbeddedResourceResource::TextResourceContents(t) =>
                                format!("TextResource(uri={}, {} chars)", t.uri, t.text.len()),
                            EmbeddedResourceResource::BlobResourceContents(b) =>
                                format!("BlobResource(uri={})", b.uri),
                            _ => "Unknown".to_string(),
                        }
                    );
                }
                ContentBlock::ResourceLink(link) => {
                    log::info!(
                        "prompt({}) block[{}]: ResourceLink(name={}, uri={}, title={:?}, desc={:?})",
                        session_id,
                        i,
                        link.name,
                        link.uri,
                        link.title,
                        link.description
                    );
                }
                other => {
                    log::info!(
                        "prompt({}) block[{}]: Other({:?})",
                        session_id,
                        i,
                        std::mem::discriminant(other)
                    );
                }
            }
        }

        let mut parts: Vec<String> = Vec::new();

        for block in &args.prompt {
            match block {
                ContentBlock::Text(t) => {
                    parts.push(t.text.clone());
                }
                ContentBlock::Resource(embedded) => {
                    // editor inlined the file content already
                    match &embedded.resource {
                        EmbeddedResourceResource::TextResourceContents(text_resource) => {
                            parts.push(format!(
                                "\n--- {} ---\n{}\n--- end {} ---",
                                text_resource.uri, text_resource.text, text_resource.uri
                            ));
                        }
                        EmbeddedResourceResource::BlobResourceContents(blob) => {
                            parts.push(format!("[binary resource: {}]", blob.uri));
                        }
                        _ => {
                            log::debug!("ignoring unsupported embedded resource variant");
                        }
                    }
                }
                ContentBlock::ResourceLink(link) => {
                    // reference without content; read the file ourselves
                    let label = link.name.clone();

                    if let Some(raw_path) = link.uri.strip_prefix("file://") {
                        let (file_path, line_range) = if let Some(hash_pos) = raw_path.rfind('#') {
                            let fragment = &raw_path[hash_pos + 1..];
                            let path = &raw_path[..hash_pos];
                            // Parse "L207:219" or "L207-219" → (207, 219)
                            let range = fragment.strip_prefix('L').and_then(|rest| {
                                let sep = if rest.contains(':') { ':' } else { '-' };
                                let mut parts = rest.splitn(2, sep);
                                let start = parts.next()?.parse::<usize>().ok()?;
                                let end = parts.next()?.parse::<usize>().ok()?;
                                Some((start, end))
                            });
                            (path, range)
                        } else {
                            (raw_path, None)
                        };

                        match std::fs::read_to_string(file_path) {
                            Ok(contents) => {
                                let extracted = if let Some((start, end)) = line_range {
                                    let selected: Vec<&str> = contents
                                        .lines()
                                        .enumerate()
                                        .filter(|(i, _)| {
                                            let line_num = i + 1;
                                            line_num >= start && line_num <= end
                                        })
                                        .map(|(_, line)| line)
                                        .collect();
                                    format!(
                                        "\n--- {label} ({file_path} lines {start}-{end}) ---\n{}\n--- end {label} ---",
                                        selected.join("\n")
                                    )
                                } else {
                                    format!(
                                        "\n--- {label} ({file_path}) ---\n{contents}\n--- end {label} ---"
                                    )
                                };
                                parts.push(extracted);
                            }
                            Err(err) => {
                                log::warn!("could not read ResourceLink {}: {err}", link.uri);
                                parts.push(format!("[referenced file: {label} ({file_path})]"));
                            }
                        }
                    } else {
                        parts.push(format!("[resource link: {label} ({})]", link.uri));
                    }
                }
                _ => {
                    log::debug!("ignoring unsupported content block type in prompt");
                }
            }
        }

        let user_text = parts.join("\n");

        if user_text.trim().is_empty() {
            return Ok(PromptResponse::new(StopReason::EndTurn));
        }

        if let Some(command) = parse_slash(&user_text) {
            return exec_slash_acp(self, cx, session_id, command).await;
        }

        log::info!(
            "prompt({}): \"{}\"",
            session_id,
            user_text.chars().take(80).collect::<String>()
        );

        // load the default ACP model lazily so initialize/session/new stay clean
        // for registry validation and editor startup.
        self.start_startup_model_load_if_needed();
        self.await_model_ready(cx, &session_id).await?;

        // ── tool-calling loop ────────────────────────────────────────────
        // send message → execute any tool calls → feed results back
        // repeat up to MAX_TOOL_ROUNDS, then force a text reply

        let onde_tools = agent_tools_as_onde();

        let mut result = self
            .engine
            .send_message_with_tools(&user_text, &onde_tools)
            .await
            .map_err(|error| {
                log::error!("send_message_with_tools failed: {error}");
                agent_client_protocol::Error::new(-32603, format!("inference failed: {error}"))
            })?;

        let mut round = 0;

        while !result.tool_calls.is_empty() && round < MAX_TOOL_ROUNDS {
            round += 1;
            log::info!(
                "prompt({}) tool round {} — {} call(s)",
                session_id,
                round,
                result.tool_calls.len()
            );

            let mut tool_results = Vec::new();

            for tc in &result.tool_calls {
                log::info!(
                    "  → {}({})",
                    tc.function_name,
                    tc.arguments.chars().take(120).collect::<String>()
                );

                let output = tools::execute_tool(&tc.function_name, &tc.arguments).await;

                log::info!("  ← {} chars", output.len());

                tool_results.push(ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: output,
                });
            }

            let next_tools = if round < MAX_TOOL_ROUNDS {
                Some(onde_tools.as_slice())
            } else {
                None // last round: force text
            };

            result = self
                .engine
                .send_tool_results(tool_results, next_tools)
                .await
                .map_err(|e| agent_client_protocol::Error::new(-32603, e.to_string()))?;
        }

        // ── Send the final text response ─────────────────────────────────
        let reply_text = result.text.trim().to_string();

        let final_text = if reply_text.is_empty() {
            if round > 0 {
                log::warn!(
                    "prompt({}) — model returned empty reply after {} tool round(s)",
                    session_id,
                    round
                );
                "Something went wrong — the edits didn't go through. Try rephrasing what you need, or point me at the specific lines.".to_string()
            } else {
                log::warn!(
                    "prompt({}) — model returned empty reply (no tool rounds)",
                    session_id
                );
                String::new()
            }
        } else {
            // strip <think> blocks so reasoning tokens stay hidden
            let (_think, visible) = chat::strip_think_blocks(&reply_text);
            visible
        };

        if !final_text.is_empty() {
            self.send_assistant_message(cx, session_id.clone(), final_text)
                .ok();
        }

        log::info!("prompt({}) complete — {} tool round(s)", session_id, round);
        Ok(PromptResponse::new(StopReason::EndTurn))
    }

    async fn handle_cancel(&self, args: CancelNotification) -> agent_client_protocol::Result<()> {
        log::info!("cancel requested for session {}", args.session_id);
        Ok(())
    }

    async fn handle_set_session_config_option(
        &self,
        cx: &ConnectionTo<Client>,
        args: SetSessionConfigOptionRequest,
    ) -> agent_client_protocol::Result<SetSessionConfigOptionResponse> {
        log::info!(
            "set_session_config_option: config_id={}, value={:?}",
            args.config_id,
            args.value
        );

        if args.config_id.0.as_ref() != MODEL_CONFIG_ID {
            return Err(agent_client_protocol::Error::new(
                -32602,
                format!("unknown config option: {}", args.config_id.0),
            ));
        }

        let model_id = args.value.0.as_ref();

        // can't switch while the startup model is still loading — the old
        // weights are in GPU memory and the new load gets "does not fit"
        if self.startup_model_load_started.load(Ordering::Acquire)
            && !self.model_ready.load(Ordering::Acquire)
        {
            log::info!("set_session_config_option: waiting for startup model to finish loading");
            while !self.model_ready.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        // Zed re-fires the last selection on connect; no-op if it's already loaded
        {
            let current = self.current_model.lock().unwrap();
            if current.model_id == model_id {
                log::info!(
                    "set_session_config_option: {} is already the active model, skipping",
                    current.display_name
                );
                let config_options = build_model_config_options(&current);
                return Ok(SetSessionConfigOptionResponse::new(config_options));
            }
        }

        let needs_download = models::local_picker_items()
            .into_iter()
            .find(|item| item.config.model_id == model_id)
            .map(|item| item.cache_health == setup::ModelCacheHealth::NotDownloaded)
            .unwrap_or(false);

        // tells the progress poller to stop
        let stop_flag = Arc::new(AtomicBool::new(false));

        let tool_call_id = format!("model-switch-{}", uuid::Uuid::new_v4());

        if needs_download {
            let model_id_owned = model_id.to_string();
            let expected_bytes = onde::inference::models::SUPPORTED_MODEL_INFO
                .iter()
                .find(|m| m.id == model_id_owned)
                .map(|m| m.expected_size_bytes)
                .unwrap_or(0);

            let display_name = models::local_picker_items()
                .into_iter()
                .find(|item| item.config.model_id == model_id_owned)
                .map(|item| item.display_name.clone())
                .unwrap_or_else(|| model_id_owned.clone());

            let size_hint = if expected_bytes > 0 {
                format!(" (~{})", format_size_human(expected_bytes))
            } else {
                String::new()
            };

            self.send_tool_call_update(
                cx,
                args.session_id.clone(),
                SessionUpdate::ToolCall(
                    ToolCall::new(
                        tool_call_id.clone(),
                        format!("⏬ Downloading {display_name}{size_hint}"),
                    )
                    .kind(ToolKind::Think)
                    .status(ToolCallStatus::InProgress)
                    .content(vec![
                        format!(
                            "Preparing download for {display_name}. This may take a few minutes."
                        )
                        .into(),
                    ]),
                ),
            )
            .ok();

            // poll download progress and update the spinner in Zed
            let cx_for_poller = cx.clone();
            let poller_session = args.session_id.clone();
            let poller_model_id = model_id_owned.clone();
            let poller_stop = Arc::clone(&stop_flag);
            let poller_tool_call_id = tool_call_id.clone();

            cx.spawn(async move {
                const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                let cache_path = onde::hf_cache::model_cache_path(&poller_model_id);
                let mut tick: usize = 0;
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                interval.tick().await; // consume the immediate first tick

                while !poller_stop.load(Ordering::Relaxed) {
                    interval.tick().await;

                    if poller_stop.load(Ordering::Relaxed) {
                        break;
                    }

                    let downloaded = cache_path
                        .as_ref()
                        .filter(|p| p.exists())
                        .map(|p| dir_size_recursive(p))
                        .unwrap_or(0);

                    let frame = SPINNER[tick % SPINNER.len()];
                    tick += 1;

                    let title = if expected_bytes > 0 {
                        let pct =
                            ((downloaded as f64 / expected_bytes as f64) * 100.0).min(99.0) as u8;
                        format!("{frame} Downloading {display_name}{size_hint} ({pct}%)")
                    } else {
                        format!("{frame} Downloading {display_name}{size_hint}")
                    };

                    let msg = if expected_bytes > 0 {
                        let pct =
                            ((downloaded as f64 / expected_bytes as f64) * 100.0).min(99.0) as u8;
                        let bar = progress_bar(pct, 20);
                        format!(
                            "{display_name} — {bar} {pct}%  ({} / {})",
                            format_size_human(downloaded),
                            format_size_human(expected_bytes),
                        )
                    } else {
                        format!(
                            "{display_name} — {} downloaded…",
                            format_size_human(downloaded)
                        )
                    };

                    let notification = SessionNotification::new(
                        poller_session.clone(),
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            poller_tool_call_id.clone(),
                            ToolCallUpdateFields::new()
                                .title(title)
                                .status(ToolCallStatus::InProgress)
                                .content(vec![msg.into()]),
                        )),
                    );
                    if cx_for_poller.send_notification(notification).is_err() {
                        break;
                    }
                }
                Ok(())
            })
            .ok();
        }

        // cached models still take 10-30s to load weights; show a spinner
        if !needs_download {
            let cached_display_name = models::local_picker_items()
                .into_iter()
                .find(|item| item.config.model_id == model_id)
                .map(|item| item.display_name.clone())
                .unwrap_or_else(|| model_id.to_string());

            self.send_tool_call_update(
                cx,
                args.session_id.clone(),
                SessionUpdate::ToolCall(
                    ToolCall::new(
                        tool_call_id.clone(),
                        format!("Loading {cached_display_name}"),
                    )
                    .kind(ToolKind::Think)
                    .status(ToolCallStatus::InProgress)
                    .content(vec![format!("Loading {cached_display_name}…").into()]),
                ),
            )
            .ok();

            // tick every 5s so the user knows we haven't frozen
            let cx_for_spinner = cx.clone();
            let spinner_session = args.session_id.clone();
            let spinner_name = cached_display_name.clone();
            let spinner_stop = Arc::clone(&stop_flag);
            let spinner_tool_call_id = tool_call_id.clone();
            let load_start = std::time::Instant::now();

            cx.spawn(async move {
                const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                let mut tick: usize = 0;
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                interval.tick().await; // consume the immediate first tick

                while !spinner_stop.load(Ordering::Relaxed) {
                    interval.tick().await;

                    if spinner_stop.load(Ordering::Relaxed) {
                        break;
                    }

                    let elapsed = load_start.elapsed();
                    let elapsed_str = if elapsed.as_secs() >= 60 {
                        format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
                    } else {
                        format!("{}s", elapsed.as_secs())
                    };
                    let frame = SPINNER[tick % SPINNER.len()];
                    tick += 1;

                    let msg = format!("{frame} Loading {spinner_name}… ({elapsed_str})");
                    let notification = SessionNotification::new(
                        spinner_session.clone(),
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            spinner_tool_call_id.clone(),
                            ToolCallUpdateFields::new()
                                .status(ToolCallStatus::InProgress)
                                .content(vec![msg.into()]),
                        )),
                    );
                    if cx_for_spinner.send_notification(notification).is_err() {
                        break;
                    }
                }
                Ok(())
            })
            .ok();
        }

        let switch_result = self.switch_model_by_id(model_id).await;

        stop_flag.store(true, Ordering::Relaxed);

        match switch_result {
            Ok(new_config) => {
                let completion_title = if needs_download {
                    format!("✓ {} downloaded and loaded", new_config.display_name)
                } else {
                    format!("✓ Switched to {}", new_config.display_name)
                };
                let completion_body = if needs_download {
                    format!("✓ {} downloaded and loaded.", new_config.display_name)
                } else {
                    format!("✓ Switched to {}.", new_config.display_name)
                };

                self.send_tool_call_update(
                    cx,
                    args.session_id.clone(),
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        tool_call_id,
                        ToolCallUpdateFields::new()
                            .title(completion_title)
                            .status(ToolCallStatus::Completed)
                            .content(vec![completion_body.into()]),
                    )),
                )
                .ok();

                let config_options = {
                    let guard = self.current_model.lock().unwrap();
                    build_model_config_options(&guard)
                };

                log::info!("model switch complete");
                Ok(SetSessionConfigOptionResponse::new(config_options))
            }
            Err(err) => {
                self.send_tool_call_update(
                    cx,
                    args.session_id.clone(),
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        tool_call_id,
                        ToolCallUpdateFields::new()
                            .title("Model switch failed".to_string())
                            .status(ToolCallStatus::Failed)
                            .content(vec![format!("error loading model: {}", err.message).into()]),
                    )),
                )
                .ok();

                Err(err)
            }
        }
    }
}

// ── Config option helpers ─────────────────────────────────────────────────────

/// config option ID for the model picker in Zed's agent panel
const MODEL_CONFIG_ID: &str = "sigit-model";

fn build_model_config_options(current_model: &GgufModelConfig) -> Vec<SessionConfigOption> {
    let items = models::local_picker_items();

    let options: Vec<SessionConfigSelectOption> = items
        .iter()
        .filter(|item| item.cache_health != setup::ModelCacheHealth::Incomplete)
        .map(|item| {
            let mut desc_parts = Vec::new();
            if item.tool_calling {
                desc_parts.push("tool calling".to_string());
            }
            desc_parts.push(item.description.clone());
            if item.cache_health == setup::ModelCacheHealth::NotDownloaded {
                desc_parts.push("↓ download on select".to_string());
            }
            let description = desc_parts.join(" - ");
            let source_badge = if item.cache_health == setup::ModelCacheHealth::NotDownloaded {
                " [↓ Onde]"
            } else {
                match item.source_label.as_str() {
                    "Onde" => " [◉ Onde]",
                    "HuggingFace" => " [○ HuggingFace]",
                    _ => "",
                }
            };
            let name = format!("{}{}", item.display_name, source_badge);
            SessionConfigSelectOption::new(
                SessionConfigValueId::new(item.config.model_id.as_str()),
                name,
            )
            .description(description)
        })
        .collect();

    if options.is_empty() {
        return vec![];
    }

    let current_value = SessionConfigValueId::new(current_model.model_id.as_str());

    vec![
        SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current_value, options)
            .category(SessionConfigOptionCategory::Model)
            .description("Select the local LLM model for inference"),
    ]
}

/// returns `(config, max_tokens, tool_calling)` for a picker model_id, or None
fn resolve_model_config(model_id: &str) -> Option<(GgufModelConfig, u64, bool)> {
    let items = models::local_picker_items();
    items
        .into_iter()
        .find(|item| {
            item.config.model_id == model_id
                && item.cache_health != setup::ModelCacheHealth::Incomplete
        })
        .map(|item| (item.config, item.max_tokens, item.tool_calling))
}

// ── Slash commands ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum SlashCommand {
    Help,
    Clear,
    Status,
    Models(Option<usize>),
    /// `/login <email> <password>` — the raw argument, parsed when executed.
    Login(Option<String>),
    Logout,
    Whoami,
    Exit,
    Unknown(String),
}

fn parse_slash(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("");
    let argument = parts.next().map(str::trim);
    Some(match command {
        "/help" => SlashCommand::Help,
        "/clear" => SlashCommand::Clear,
        "/status" => SlashCommand::Status,
        "/models" => SlashCommand::Models(argument.and_then(|v| v.parse::<usize>().ok())),
        "/login" => SlashCommand::Login(argument.map(str::to_string)),
        "/logout" => SlashCommand::Logout,
        "/whoami" => SlashCommand::Whoami,
        "/exit" | "/quit" | "/q" => SlashCommand::Exit,
        other => SlashCommand::Unknown(other.to_string()),
    })
}

fn format_models_list(current_model: &GgufModelConfig) -> String {
    let items = models::local_picker_items();
    if items.is_empty() {
        return "No local models found. siGit will use the platform default model.".to_string();
    }

    let mut lines = vec!["Available models:".to_string()];
    let mut last_source: Option<&str> = None;

    for (index, item) in items.iter().enumerate() {
        let source_key = match item.source_label.as_str() {
            "Onde" => "Onde",
            "HuggingFace" => "HuggingFace",
            _ => "Fallback",
        };

        if last_source != Some(source_key) {
            if last_source.is_some() {
                lines.push(String::new());
            }
            let section = match source_key {
                "Onde" => "Onde Inference",
                "HuggingFace" => "Hugging Face cache",
                _ => "Fallback",
            };
            lines.push(section.to_string());
            last_source = Some(source_key);
        }

        let number = index + 1;
        let current_badge = if item.config.model_id == current_model.model_id {
            "  <- current"
        } else {
            ""
        };
        let tool_badge = if item.tool_calling {
            "  tool calling"
        } else {
            ""
        };
        let health_badge = match item.cache_health {
            setup::ModelCacheHealth::Complete => "",
            setup::ModelCacheHealth::Incomplete => "  ! incomplete cache",
            setup::ModelCacheHealth::NotDownloaded => "  ↓ download on select",
        };
        let source = match source_key {
            "Onde" => "  [Onde]",
            "HuggingFace" => "  [HuggingFace]",
            _ => "  [default]",
        };

        lines.push(format!(
            "{number}. {}  {}{}{}{}{}",
            item.display_name, item.description, tool_badge, health_badge, current_badge, source,
        ));
    }

    lines.push(String::new());
    lines.push("Use /models N to switch models.".to_string());
    lines.join("\n")
}

async fn exec_slash_acp(
    agent: &SiGitAgent,
    cx: &ConnectionTo<Client>,
    session_id: SessionId,
    command: SlashCommand,
) -> agent_client_protocol::Result<PromptResponse> {
    match command {
        SlashCommand::Help => {
            agent
                .send_assistant_message(
                    cx,
                    session_id,
                    "/help          - show this message\n\
                     /models        - list available models\n\
                     /models N      - switch to model N\n\
                     /login E P     - sign in to siGit Code Cloud\n\
                     /logout        - sign out\n\
                     /whoami        - show the signed-in account\n\
                     /clear         - wipe conversation history\n\
                     /status        - show engine status\n\
                     /exit          - end this turn",
                )
                .ok();
        }
        SlashCommand::Clear => {
            let cleared = agent.engine.clear_history().await;
            agent
                .send_assistant_message(
                    cx,
                    session_id,
                    format!("Cleared {cleared} turn(s). History is empty."),
                )
                .ok();
        }
        SlashCommand::Status => {
            let info = agent.engine.info().await;
            let model = info.model_name.as_deref().unwrap_or("(none)");
            let memory = info.approx_memory.as_deref().unwrap_or("unknown");
            agent
                .send_assistant_message(
                    cx,
                    session_id,
                    format!(
                        "status: {:?}  model: {}  memory: {}  history: {} turns",
                        info.status, model, memory, info.history_length,
                    ),
                )
                .ok();
        }
        SlashCommand::Models(None) => {
            let current_model = agent.current_model.lock().unwrap().clone();
            agent
                .send_assistant_message(cx, session_id, format_models_list(&current_model))
                .ok();
        }
        SlashCommand::Models(Some(number)) => {
            let items = models::local_picker_items();
            let index = number.saturating_sub(1);
            match items.get(index).cloned() {
                None => {
                    agent
                        .send_assistant_message(
                            cx,
                            session_id,
                            format!("error: no model #{number} - type /models to see the list."),
                        )
                        .ok();
                }
                Some(model) => {
                    if model.cache_health == setup::ModelCacheHealth::Incomplete {
                        agent
                            .send_assistant_message(
                                cx,
                                session_id,
                                format!(
                                    "error: {} has an incomplete local cache and cannot be selected yet.",
                                    model.display_name
                                ),
                            )
                            .ok();
                    } else if model.cache_health == setup::ModelCacheHealth::NotDownloaded {
                        agent
                            .send_assistant_message(
                                cx,
                                session_id.clone(),
                                format!(
                                    "Downloading and loading {} ({})… this may take a few minutes.",
                                    model.display_name, model.description
                                ),
                            )
                            .ok();

                        match agent.switch_model_by_id(&model.config.model_id).await {
                            Ok(new_config) => {
                                agent.engine.clear_history().await;
                                agent
                                    .send_assistant_message(
                                        cx,
                                        session_id,
                                        format!(
                                            "✓ Downloaded and switched to {}",
                                            new_config.display_name
                                        ),
                                    )
                                    .ok();
                            }
                            Err(err) => {
                                agent
                                    .send_assistant_message(
                                        cx,
                                        session_id,
                                        format!("error downloading model: {}", err.message),
                                    )
                                    .ok();
                            }
                        }
                    } else {
                        agent
                            .send_assistant_message(
                                cx,
                                session_id.clone(),
                                format!("Loading {}...", model.display_name),
                            )
                            .ok();

                        let switched = agent.switch_model_by_id(&model.config.model_id).await?;
                        agent.engine.clear_history().await;

                        agent
                            .send_assistant_message(
                                cx,
                                session_id,
                                format!("Switched to {}.", switched.display_name),
                            )
                            .ok();
                    }
                }
            }
        }
        SlashCommand::Login(argument) => {
            let message = match argument.as_deref().and_then(account::parse_login_args) {
                Some((email, password)) => match account::authenticate(&email, &password).await {
                    Ok(email) => format!(
                        "Signed in as {email}. siGit Code Cloud applies to your next session."
                    ),
                    Err(error) => format!("Login failed: {error}"),
                },
                None => "usage: /login <email> <password>".to_string(),
            };
            agent.send_assistant_message(cx, session_id, message).ok();
        }
        SlashCommand::Logout => {
            let message = account::end_session().await;
            agent.send_assistant_message(cx, session_id, message).ok();
        }
        SlashCommand::Whoami => {
            let message = account::status_line().await;
            agent.send_assistant_message(cx, session_id, message).ok();
        }
        SlashCommand::Exit => {
            agent
                .send_assistant_message(
                    cx,
                    session_id,
                    "Use the panel controls to close or switch threads.",
                )
                .ok();
        }
        SlashCommand::Unknown(command) => {
            agent
                .send_assistant_message(cx, session_id, format!("unknown command: {command}"))
                .ok();
        }
    }

    Ok(PromptResponse::new(StopReason::EndTurn))
}

// ── Request dispatch helper ───────────────────────────────────────────────────

fn handle_response<T: agent_client_protocol::JsonRpcResponse>(
    responder: Responder<T>,
    result: agent_client_protocol::Result<T>,
) -> agent_client_protocol::Result<()> {
    match result {
        Ok(resp) => responder.respond(resp),
        Err(err) => responder.respond_with_error(err),
    }
}

// ── Download progress helpers ─────────────────────────────────────────────────

/// total bytes on disk under `path`. needed because hf-hub uses staging
/// names during download, so we can't just stat the final blobs.
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

fn progress_bar(pct: u8, width: usize) -> String {
    let filled = ((pct as usize) * width) / 100;
    let empty = width.saturating_sub(filled);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

// ── Output capture ────────────────────────────────────────────────────────────

/// redirect stdout+stderr to `$TMPDIR/sigit.log` at the fd level so
/// mistralrs/tracing noise never hits the terminal. returns two dup'd
/// fds to the real tty: one for ratatui, one for cleanup (ratatui 0.29
/// doesn't expose `writer_mut()`).
#[cfg(unix)]
fn redirect_output_to_log() -> anyhow::Result<(std::fs::File, std::fs::File)> {
    let log_path = std::env::temp_dir().join("sigit.log");
    let log_file = std::fs::File::create(&log_path)?;
    let log_fd = log_file.as_raw_fd();

    // two copies: ratatui needs one, cleanup needs another
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

    unsafe {
        libc::dup2(log_fd, libc::STDOUT_FILENO);
        libc::dup2(log_fd, libc::STDERR_FILENO);
    }

    // safe to drop log_file; dup2 keeps the fd alive via stdout/stderr

    Ok((unsafe { std::fs::File::from_raw_fd(saved_tui) }, unsafe {
        std::fs::File::from_raw_fd(saved_cleanup)
    }))
}

// ── Logging ───────────────────────────────────────────────────────────────────

/// in TUI mode stderr is the log file (redirected earlier);
/// in ACP mode it's real stderr. either way, write there.
fn init_logging(is_tty: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(!is_tty)
        .try_init();
}

// ── Interactive TUI mode ──────────────────────────────────────────────────────

/// boot the TUI and load the model on a background thread.
/// `tty` goes to ratatui; `cleanup_tty` is a separate fd for
/// LeaveAlternateScreen (ratatui 0.29 hides `writer_mut()`).
#[cfg(unix)]
async fn run_interactive(tty: std::fs::File, mut cleanup_tty: std::fs::File) -> anyhow::Result<()> {
    let engine = Arc::new(ChatEngine::new());

    let startup_selection = setup::startup_model_selection();
    let startup_model_name = startup_selection
        .as_ref()
        .map(|selection| selection.display_name.clone())
        .unwrap_or_else(|| GgufModelConfig::qwen25_3b().display_name);

    let config = startup_selection
        .as_ref()
        .and_then(|selection| {
            models::local_picker_items()
                .into_iter()
                .find(|item| {
                    selection
                        .selected_model
                        .as_ref()
                        .map(|selected| {
                            item.config.model_id == selected.model_id
                                && item
                                    .config
                                    .files
                                    .iter()
                                    .any(|file| file == &selected.gguf_file)
                        })
                        .unwrap_or(false)
                })
                .map(|item| item.config)
        })
        .unwrap_or_else(GgufModelConfig::qwen25_3b);
    let sampling = SamplingConfig {
        max_tokens: Some(8192),
        ..SamplingConfig::default()
    };

    // std::sync::mpsc on a real thread so model loading can't starve the TUI
    let (load_tx, load_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let tool_calling = models::local_picker_items()
        .iter()
        .find(|item| item.config.model_id == config.model_id)
        .map(|item| item.tool_calling)
        .unwrap_or(false);

    // Pick the inference backend: a configured provider if present, else on-device.
    let (inference_backend, startup_model_name): (Arc<dyn InferenceBackend>, String) =
        match provider::active_provider() {
            Some(provider) => {
                log::info!(
                    "inference: using {} (model {}) at {}",
                    provider.display_name,
                    provider.model,
                    provider.base_url
                );
                // No local model to load; the endpoint is ready immediately.
                let _ = load_tx.send(Ok(()));
                let label = provider.display_name.clone();
                let backend = Arc::new(OpenAiBackend::new(
                    provider.base_url,
                    provider.api_key,
                    provider.model,
                    Some(SYSTEM_PROMPT.to_string()),
                )) as Arc<dyn InferenceBackend>;
                (backend, label)
            }
            None => {
                // On-device: load the local GGUF model on a real thread.
                let loader_engine = Arc::clone(&engine);
                let system_prompt = system_prompt_for_model(tool_calling).to_string();
                std::thread::spawn(move || {
                    let rt =
                        tokio::runtime::Runtime::new().expect("failed to create loader runtime");
                    let result = rt.block_on(loader_engine.load_gguf_model(
                        config,
                        Some(system_prompt),
                        Some(sampling),
                    ));
                    let _ = load_tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
                });
                let backend =
                    Arc::new(LocalBackend::new(Arc::clone(&engine))) as Arc<dyn InferenceBackend>;
                (backend, startup_model_name)
            }
        };

    crossterm::terminal::enable_raw_mode()?;
    let mut tty = BufWriter::new(tty);
    crossterm::execute!(tty, crossterm::terminal::EnterAlternateScreen)?;
    let term_backend = ratatui::backend::CrosstermBackend::new(tty);
    let mut terminal = ratatui::Terminal::new(term_backend)?;

    // polls load_rx with try_recv() each tick, no blocking
    let chat_result = chat::run_with(
        &mut terminal,
        engine,
        inference_backend,
        load_rx,
        startup_model_name,
    )
    .await;

    // cleanup fd because backend's writer is private
    crossterm::execute!(cleanup_tty, crossterm::terminal::LeaveAlternateScreen)?;
    cleanup_tty.flush()?;
    crossterm::terminal::disable_raw_mode()?;

    // restore real stdout/stderr for post-TUI error output
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

    let startup_selection = setup::startup_model_selection();
    let config = startup_selection
        .as_ref()
        .and_then(|selection| {
            selection.selected_model.as_ref().and_then(|selected| {
                models::local_picker_items()
                    .into_iter()
                    .find(|item| {
                        item.config.model_id == selected.model_id
                            && item
                                .config
                                .files
                                .iter()
                                .any(|file| file == &selected.gguf_file)
                    })
                    .map(|item| item.config)
            })
        })
        .unwrap_or_else(GgufModelConfig::qwen25_3b);

    let needs_download = models::local_picker_items()
        .iter()
        .find(|item| item.config.model_id == config.model_id)
        .map(|item| item.cache_health != setup::ModelCacheHealth::Complete)
        .unwrap_or(true);

    log::info!(
        "ACP startup model selected: {} ({})",
        config.display_name,
        if needs_download {
            "needs download"
        } else {
            "cached"
        }
    );

    let engine = Arc::new(ChatEngine::new());

    // Delay model loading until the first real prompt so initialize/session/new
    // stay lightweight and registry auth checks don't trip over model startup.
    let model_ready = Arc::new(AtomicBool::new(true));
    let startup_model_load_started = Arc::new(AtomicBool::new(false));
    let model_load_error: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));

    let state = Arc::new(SiGitAgent::new(
        engine,
        config,
        model_ready,
        startup_model_load_started,
        model_load_error,
        needs_download,
    ));

    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();
    let transport = ByteStreams::new(stdout, stdin);

    Agent
        .builder()
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |req: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                    handle_response(responder, state.handle_initialize(req).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |req: AuthenticateRequest, responder, _cx: ConnectionTo<Client>| {
                    handle_response(responder, state.handle_authenticate(req).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |req: LoadSessionRequest, responder, _cx: ConnectionTo<Client>| {
                    handle_response(responder, state.handle_load_session(req).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |req: ForkSessionRequest, responder, _cx: ConnectionTo<Client>| {
                    handle_response(responder, state.handle_fork_session(req).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |req: NewSessionRequest, responder, _cx: ConnectionTo<Client>| {
                    handle_response(responder, state.handle_new_session(req).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    handle_response(responder, state.handle_prompt(&cx, req).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |req: SetSessionConfigOptionRequest,
                            responder,
                            cx: ConnectionTo<Client>| {
                    handle_response(
                        responder,
                        state.handle_set_session_config_option(&cx, req).await,
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = Arc::clone(&state);
                async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                    state.handle_cancel(notif).await
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await
        .map_err(|e| anyhow::anyhow!("ACP connection error: {e}"))?;

    log::info!("siGit shutting down");
    Ok(())
}

// ── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let is_tty = std::io::stdin().is_terminal();

    if is_tty {
        // must redirect before any library code touches stdout
        #[cfg(unix)]
        {
            let (tty, cleanup_tty) = redirect_output_to_log()?;
            init_logging(true);
            setup::setup_shared_model_cache();
            run_interactive(tty, cleanup_tty).await
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("interactive mode requires Unix (macOS / Linux)");
        }
    } else {
        // ACP mode: keep stdout untouched for protocol JSON only.
        // Logs already go to stderr via `init_logging(false)`.
        init_logging(false);
        setup::setup_shared_model_cache();
        log::info!("siGit v{} starting (ACP mode)", env!("CARGO_PKG_VERSION"));
        run_acp_server().await
    }
}
