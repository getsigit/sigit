//! siGit Code is a local coding agent built on Onde Inference.
//!
//! When you run it in an interactive terminal, all process output goes to
//! `$TMPDIR/sigit.log` first. That includes `log::` events, `tracing` output
//! from mistralrs_core, and even stray `println!` calls from dependencies.
//! Ratatui gets its own copy of the real terminal handle, so the UI can keep
//! drawing normally while the noisy stuff goes to the log file.
//!
//! siGit has two modes:
//! - ACP mode over stdio for editors like Zed
//! - interactive terminal mode when you run it directly in a TTY
//!
//! Current platform support:
//! - macOS: ACP mode and interactive terminal mode
//! - Linux: ACP mode and interactive terminal mode
//! - Windows: ACP mode only for now
//!
//! The interactive terminal path is still Unix-only because it relies on
//! Unix file-descriptor redirection to keep logs away from the TUI.
//!
//! The model loads before the ACP `LocalSet` starts. That is important because
//! `mistralrs` calls `block_in_place` internally, and that blows up inside
//! `spawn_local` tasks. Loading it on a normal multi-thread worker avoids the
//! problem.
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
//!     "siGit Code": {
//!       "type": "custom",
//!       "command": "/absolute/path/to/target/release/sigit"
//!     }
//!   }
//! }
//! ```

#[cfg(unix)]
mod chat;
mod models;
mod setup;
mod tools;

use std::io::IsTerminal;
#[cfg(unix)]
use std::io::{BufWriter, Write};
use std::sync::Arc;

use onde::inference::SamplingConfig;

use agent_client_protocol::{
    Agent, AgentCapabilities, AgentSideConnection, AuthMethod, AuthMethodAgent,
    AuthenticateRequest, AuthenticateResponse, CancelNotification, Client, ContentBlock,
    ContentChunk, ForkSessionRequest, ForkSessionResponse, Implementation, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, Meta, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion, SessionCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionConfigValueId, SessionForkCapabilities, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
};
use futures::future::LocalBoxFuture;
use onde::inference::{ChatEngine, GgufModelConfig, ToolDefinition, ToolResult};
use std::path::PathBuf;
use tokio::sync::mpsc;
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

/// Slim system prompt for models that do not support tool calling.
///
/// These models (e.g. DeepSeek Coder v1) cannot use the agent tools, so
/// the long tool-oriented instructions in [`SYSTEM_PROMPT`] would waste
/// context and confuse the model. Keep this short and code-focused.
const SIMPLE_SYSTEM_PROMPT: &str = "\
Your name is siGit — a coding assistant. \
You are helpful, concise, and write clean, idiomatic code. \
Answer any question the user asks — programming, general knowledge, or casual chat. \
When debugging, address the root cause, not the symptom. \
Be direct and brief.";

/// Pick the right system prompt based on whether the model supports tool calling.
pub(crate) fn system_prompt_for_model(tool_calling: bool) -> &'static str {
    if tool_calling {
        SYSTEM_PROMPT
    } else {
        SIMPLE_SYSTEM_PROMPT
    }
}

/// Maximum number of tool-calling rounds before forcing a text response.
const MAX_TOOL_ROUNDS: usize = 10;

/// Convert the agent tool definitions into onde's `ToolDefinition` type.
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
        .unwrap_or_else(|| GgufModelConfig::qwen3_4b().display_name);

    let active_model_id = startup_selection
        .as_ref()
        .and_then(|selection| selection.selected_model.as_ref())
        .map(|selected| selected.model_id.clone())
        .unwrap_or_else(|| GgufModelConfig::qwen3_4b().model_id);

    let active_model_file = startup_selection
        .as_ref()
        .and_then(|selection| selection.selected_model.as_ref())
        .map(|selected| selected.gguf_file.clone())
        .unwrap_or_else(|| {
            GgufModelConfig::qwen3_4b()
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

// Agent

struct SiGitAgent {
    engine: Arc<ChatEngine>,
    notification_tx: mpsc::Sender<SessionNotification>,
    /// The project working directory provided by the editor via ACP session
    /// creation. Tool calls use this as `cwd` so file operations target the
    /// correct project, not wherever the agent process was spawned.
    session_cwd: std::sync::Mutex<Option<PathBuf>>,
    /// The currently loaded model config, used for config_options reporting.
    current_model: std::sync::Mutex<GgufModelConfig>,
}

impl SiGitAgent {
    fn new(
        engine: Arc<ChatEngine>,
        notification_tx: mpsc::Sender<SessionNotification>,
        initial_model: GgufModelConfig,
    ) -> Self {
        Self {
            engine,
            notification_tx,
            session_cwd: std::sync::Mutex::new(None),
            current_model: std::sync::Mutex::new(initial_model),
        }
    }

    async fn send_assistant_message(&self, session_id: SessionId, text: impl Into<String>) {
        let notification = SessionNotification::new(
            session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(text.into()))),
        );
        if self.notification_tx.send(notification).await.is_err() {
            log::warn!("notification channel closed");
        }
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

        // load_gguf_model calls block_in_place internally.  Calling it from
        // inside the ACP LocalSet (spawn_local) panics with "can call blocking
        // only when running on the multi-threaded runtime".  Fix: run the
        // unload + load on a dedicated OS thread with its own runtime, then
        // await the result over a oneshot channel — same pattern used at
        // startup in run_acp_server.
        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let loader_engine = Arc::clone(&self.engine);
        let loader_config = new_config.clone();
        let loader_system_prompt = system_prompt_for_model(new_tool_calling).to_string();
        let loader_sampling = sampling;

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("failed to create loader runtime");
            let result = rt.block_on(async move {
                // load_gguf_model unloads any existing model internally before
                // loading the new one.  Calling unload_model() explicitly first
                // would create a window where no model is loaded — if a prompt
                // arrived in that gap it would fail with NoModelLoaded.
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

        if let Some(item) = models::build_model_picker_items()
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

/// The config option ID used for the model selector in the Zed agent panel.
const MODEL_CONFIG_ID: &str = "sigit-model";

/// Build the `SessionConfigOption` list for model selection.
fn build_model_config_options(current_model: &GgufModelConfig) -> Vec<SessionConfigOption> {
    let items = models::build_model_picker_items();

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

/// Look up the GgufModelConfig for a given model_id value from the picker items.
///
/// Returns `(config, max_tokens, tool_calling)`.
fn resolve_model_config(model_id: &str) -> Option<(GgufModelConfig, u64, bool)> {
    let items = models::build_model_picker_items();
    items
        .into_iter()
        .find(|item| {
            item.config.model_id == model_id
                && item.cache_health != setup::ModelCacheHealth::Incomplete
        })
        .map(|item| (item.config, item.max_tokens, item.tool_calling))
}

#[derive(Debug, Clone)]
enum SlashCommand {
    Help,
    Clear,
    Status,
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
    let command = parts.next().unwrap_or("");
    let argument = parts.next().map(str::trim);
    Some(match command {
        "/help" => SlashCommand::Help,
        "/clear" => SlashCommand::Clear,
        "/status" => SlashCommand::Status,
        "/models" => SlashCommand::Models(argument.and_then(|v| v.parse::<usize>().ok())),
        "/exit" | "/quit" | "/q" => SlashCommand::Exit,
        other => SlashCommand::Unknown(other.to_string()),
    })
}

fn format_models_list(current_model: &GgufModelConfig) -> String {
    let items = models::build_model_picker_items();
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
    session_id: SessionId,
    command: SlashCommand,
) -> agent_client_protocol::Result<PromptResponse> {
    match command {
        SlashCommand::Help => {
            agent
                .send_assistant_message(
                    session_id,
                    "/help      - show this message\n\
                     /models    - list available models\n\
                     /models N  - switch to model N\n\
                     /clear     - wipe conversation history\n\
                     /status    - show engine status\n\
                     /exit      - end this turn",
                )
                .await;
        }
        SlashCommand::Clear => {
            let cleared = agent.engine.clear_history().await;
            agent
                .send_assistant_message(
                    session_id,
                    format!("Cleared {cleared} turn(s). History is empty."),
                )
                .await;
        }
        SlashCommand::Status => {
            let info = agent.engine.info().await;
            let model = info.model_name.as_deref().unwrap_or("(none)");
            let memory = info.approx_memory.as_deref().unwrap_or("unknown");
            agent
                .send_assistant_message(
                    session_id,
                    format!(
                        "status: {:?}  model: {}  memory: {}  history: {} turns",
                        info.status, model, memory, info.history_length,
                    ),
                )
                .await;
        }
        SlashCommand::Models(None) => {
            let current_model = agent.current_model.lock().unwrap().clone();
            agent
                .send_assistant_message(session_id, format_models_list(&current_model))
                .await;
        }
        SlashCommand::Models(Some(number)) => {
            let items = models::build_model_picker_items();
            let index = number.saturating_sub(1);
            match items.get(index).cloned() {
                None => {
                    agent
                        .send_assistant_message(
                            session_id,
                            format!("error: no model #{number} - type /models to see the list."),
                        )
                        .await;
                }
                Some(model) => {
                    if model.cache_health == setup::ModelCacheHealth::Incomplete {
                        agent
                            .send_assistant_message(
                                session_id,
                                format!(
                                    "error: {} has an incomplete local cache and cannot be selected yet.",
                                    model.display_name
                                ),
                            )
                            .await;
                    } else if model.cache_health == setup::ModelCacheHealth::NotDownloaded {
                        agent
                            .send_assistant_message(
                                session_id.clone(),
                                format!(
                                    "Downloading and loading {} ({})… this may take a few minutes.",
                                    model.display_name, model.description
                                ),
                            )
                            .await;

                        match agent.switch_model_by_id(&model.config.model_id).await {
                            Ok(new_config) => {
                                agent
                                    .send_assistant_message(
                                        session_id,
                                        format!(
                                            "✓ Downloaded and switched to {}",
                                            new_config.display_name
                                        ),
                                    )
                                    .await;
                            }
                            Err(err) => {
                                agent
                                    .send_assistant_message(
                                        session_id,
                                        format!("error downloading model: {}", err.message),
                                    )
                                    .await;
                            }
                        }
                    } else {
                        agent
                            .send_assistant_message(
                                session_id.clone(),
                                format!("Loading {}...", model.display_name),
                            )
                            .await;

                        let switched = agent.switch_model_by_id(&model.config.model_id).await?;
                        agent.engine.clear_history().await;

                        agent
                            .send_assistant_message(
                                session_id,
                                format!("Switched to {}.", switched.display_name),
                            )
                            .await;
                    }
                }
            }
        }
        SlashCommand::Exit => {
            agent
                .send_assistant_message(
                    session_id,
                    "Use the panel controls to close or switch threads.",
                )
                .await;
        }
        SlashCommand::Unknown(command) => {
            agent
                .send_assistant_message(session_id, format!("unknown command: {command}"))
                .await;
        }
    }

    Ok(PromptResponse::new(StopReason::EndTurn))
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
            .agent_capabilities(
                AgentCapabilities::default()
                    .load_session(true)
                    .session_capabilities(
                        SessionCapabilities::new().fork(SessionForkCapabilities::new()),
                    ),
            )
            .meta(initialize_meta()))
    }

    async fn authenticate(
        &self,
        _args: AuthenticateRequest,
    ) -> agent_client_protocol::Result<AuthenticateResponse> {
        log::info!("authenticate");
        Ok(AuthenticateResponse::default())
    }

    async fn load_session(
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

        // Capture the project working directory from the editor.
        if let Ok(mut guard) = self.session_cwd.lock() {
            *guard = Some(args.cwd.clone());
        }

        // Set the process cwd so tool calls using relative paths land in the
        // correct project directory.
        if args.cwd.is_dir()
            && let Err(err) = std::env::set_current_dir(&args.cwd)
        {
            log::warn!("could not set cwd to {}: {err}", args.cwd.display());
        }

        // Clear conversation history — siGit doesn't persist sessions, so a
        // "load" is effectively a fresh start with the same session ID.
        self.engine.clear_history().await;

        // Tell the model which project directory it's working in.
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

    async fn fork_session(
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

        // Update cwd if the fork provides a different one.
        if let Ok(mut guard) = self.session_cwd.lock() {
            *guard = Some(args.cwd.clone());
        }
        if args.cwd.is_dir()
            && let Err(err) = std::env::set_current_dir(&args.cwd)
        {
            log::warn!("could not set cwd to {}: {err}", args.cwd.display());
        }

        // siGit doesn't persist history, so a fork is effectively a fresh
        // session — clear the conversation and let the user start over from
        // their edited message.
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

    async fn new_session(
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

        // Capture the project working directory from the editor.
        if let Ok(mut guard) = self.session_cwd.lock() {
            *guard = Some(args.cwd.clone());
        }
        if args.cwd.is_dir()
            && let Err(err) = std::env::set_current_dir(&args.cwd)
        {
            log::warn!("could not set cwd to {}: {err}", args.cwd.display());
        }

        // Clear history — the model is already loaded.
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

    async fn prompt(&self, args: PromptRequest) -> agent_client_protocol::Result<PromptResponse> {
        let session_id = args.session_id.clone();

        // Debug: log every content block the editor sends so we can see
        // exactly what arrives for @ references, file context, etc.
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
                            agent_client_protocol::EmbeddedResourceResource::TextResourceContents(t) =>
                                format!("TextResource(uri={}, {} chars)", t.uri, t.text.len()),
                            agent_client_protocol::EmbeddedResourceResource::BlobResourceContents(b) =>
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
                    // Embedded file content sent by the editor (preferred over ResourceLink).
                    match &embedded.resource {
                        agent_client_protocol::EmbeddedResourceResource::TextResourceContents(
                            text_resource,
                        ) => {
                            parts.push(format!(
                                "\n--- {} ---\n{}\n--- end {} ---",
                                text_resource.uri, text_resource.text, text_resource.uri
                            ));
                        }
                        agent_client_protocol::EmbeddedResourceResource::BlobResourceContents(
                            blob,
                        ) => {
                            parts.push(format!("[binary resource: {}]", blob.uri));
                        }
                        _ => {
                            log::debug!("ignoring unsupported embedded resource variant");
                        }
                    }
                }
                ContentBlock::ResourceLink(link) => {
                    // The editor sent a reference but not the content — read it if it's a file.
                    let label = link.name.clone();

                    if let Some(raw_path) = link.uri.strip_prefix("file://") {
                        // Split off the #L<start>:<end> fragment if present.
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
                                    // Extract only the requested line range (1-based, inclusive).
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
            return exec_slash_acp(self, session_id, command).await;
        }

        log::info!(
            "prompt({}): \"{}\"",
            session_id,
            user_text.chars().take(80).collect::<String>()
        );

        // ── Agentic tool-calling loop ────────────────────────────────────
        //
        // 1. Send the user message with tool definitions (non-streaming).
        // 2. If the model responds with tool calls, execute them, feed
        //    results back, and repeat (up to MAX_TOOL_ROUNDS).
        // 3. Once the model produces a text response (no tool calls),
        //    stream it to the editor.

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

                // Execute the tool (async — read_website uses spawn_blocking internally).
                let output = tools::execute_tool(&tc.function_name, &tc.arguments).await;

                log::info!("  ← {} chars", output.len());

                tool_results.push(ToolResult {
                    tool_call_id: tc.id.clone(),
                    content: output,
                });
            }

            // Decide whether to allow further tool calls.
            let next_tools = if round < MAX_TOOL_ROUNDS {
                Some(onde_tools.as_slice())
            } else {
                None // force a text response on the last round
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
            // Strip Qwen 3 `<think>…</think>` blocks — the editor doesn't
            // need to see internal reasoning tokens.
            let (_think, visible) = chat::strip_think_blocks(&reply_text);
            visible
        };

        if !final_text.is_empty() {
            let notification = SessionNotification::new(
                session_id.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(final_text))),
            );
            if self.notification_tx.send(notification).await.is_err() {
                log::warn!("notification channel closed");
            }
        }

        log::info!("prompt({}) complete — {} tool round(s)", session_id, round);
        Ok(PromptResponse::new(StopReason::EndTurn))
    }

    async fn cancel(&self, args: CancelNotification) -> agent_client_protocol::Result<()> {
        log::info!("cancel requested for session {}", args.session_id);
        Ok(())
    }

    async fn set_session_config_option(
        &self,
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
        let _new_config = self.switch_model_by_id(model_id).await?;

        let config_options = {
            let guard = self.current_model.lock().unwrap();
            build_model_config_options(&guard)
        };

        log::info!("model switch complete");
        Ok(SetSessionConfigOptionResponse::new(config_options))
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
#[cfg(unix)]
async fn run_interactive(tty: std::fs::File, mut cleanup_tty: std::fs::File) -> anyhow::Result<()> {
    let engine = Arc::new(ChatEngine::new());

    let startup_selection = setup::startup_model_selection();
    let startup_model_name = startup_selection
        .as_ref()
        .map(|selection| selection.display_name.clone())
        .unwrap_or_else(|| GgufModelConfig::platform_default().display_name);

    let config = startup_selection
        .as_ref()
        .and_then(|selection| {
            models::build_model_picker_items()
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
        .unwrap_or_else(GgufModelConfig::platform_default);
    let sampling = SamplingConfig {
        max_tokens: Some(8192),
        ..SamplingConfig::default()
    };

    // std::sync::mpsc — the loader runs on a dedicated OS thread, completely
    // decoupled from the tokio runtime so it can't starve the TUI draw loop.
    let (load_tx, load_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let loader_engine = Arc::clone(&engine);
    let tool_calling = models::build_model_picker_items()
        .iter()
        .find(|item| item.config.model_id == config.model_id)
        .map(|item| item.tool_calling)
        .unwrap_or(false);
    let system_prompt = system_prompt_for_model(tool_calling).to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create loader runtime");
        let result =
            rt.block_on(loader_engine.load_gguf_model(config, Some(system_prompt), Some(sampling)));
        let _ = load_tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
    });

    // Set up the terminal manually on the real tty fd.
    crossterm::terminal::enable_raw_mode()?;
    let mut tty = BufWriter::new(tty);
    crossterm::execute!(tty, crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(tty);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // The TUI runs here on the main tokio runtime.  It polls load_rx via
    // try_recv() on every tick — non-blocking, zero contention.
    let chat_result = chat::run_with(&mut terminal, engine, load_rx, startup_model_name).await;

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

    let startup_selection = setup::startup_model_selection();
    let config = startup_selection
        .as_ref()
        .and_then(|selection| {
            models::build_model_picker_items()
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
        .unwrap_or_else(GgufModelConfig::qwen3_4b);

    let acp_tool_calling = models::build_model_picker_items()
        .iter()
        .find(|item| item.config.model_id == config.model_id)
        .map(|item| (item.tool_calling, item.max_tokens))
        .unwrap_or((true, 4096));

    let max_tokens = acp_tool_calling.1;
    let tool_calling = acp_tool_calling.0;

    let sampling = SamplingConfig {
        max_tokens: Some(max_tokens),
        ..SamplingConfig::default()
    };

    log::info!("ACP startup model: {}", config.display_name);

    let startup_config = config.clone();
    let acp_system_prompt = system_prompt_for_model(tool_calling).to_string();
    engine
        .load_gguf_model(config, Some(acp_system_prompt), Some(sampling))
        .await
        .map_err(|error| anyhow::anyhow!("model load failed: {error}"))?;

    log::info!("model loaded and ready");

    let (notification_tx, mut notification_rx) = mpsc::channel::<SessionNotification>(256);
    let agent = SiGitAgent::new(engine, notification_tx, startup_config);

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
        // ACP mode: no redirect needed, logs go to stderr.
        init_logging(false);
        setup::setup_shared_model_cache();
        log::info!("siGit v{} starting (ACP mode)", env!("CARGO_PKG_VERSION"));
        run_acp_server().await
    }
}
