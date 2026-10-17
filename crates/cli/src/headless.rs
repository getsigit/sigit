//! Headless programmatic mode: `sigit run "<prompt>"` runs one prompt and exits.
//!
//! This is the entry point for CI, scripts, cron, and the Cloud Agent sandbox
//! runner: no client, no TTY, just plain stdio. Assistant text streams to
//! stdout as it is generated (`--quiet` restricts stdout to the final message);
//! logs and tool progress go to stderr.
//!
//! Cross-platform by design — unlike the ratatui TUI this is NOT gated on
//! `#[cfg(unix)]`, so Windows gets it too.
//!
//! Permission model: nobody is around to answer `ask`, so `ask` collapses to a
//! denial telling the model the tool was not pre-approved (mentioning
//! `--allow-tool`). `--allow-tool <name>` pre-grants a tool for the run's
//! session; `--deny-tool <name>` blocks a tool even if settings would allow it.
//! `SIGIT_PERMISSIONS=allow` remains the blunt instrument.
//!
//! Exit codes: 0 — turn completed; 1 — inference/tool-loop or backend
//! resolution error; 2 — bad invocation (handled by the caller in `main`).

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::{
    self, InferenceBackend, OpenAiBackend, TokenChunk, ToolResult as BackendToolResult, TurnResult,
};
use crate::{permissions, provider, session_store, settings, tools};

pub const USAGE: &str = "Usage: sigit run \"<prompt>\" [--cwd <dir>] [--add-dir <dir>]... \
                         [--output text|jsonl] [--quiet] [--resume <session-id>] \
                         [--allow-tool <name>]... [--deny-tool <name>]... \
                         [--max-tool-rounds <n>]\n       \
                         sigit -p \"<prompt>\" [same options]";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Text,
    Jsonl,
}

/// Parsed `sigit run` or legacy `sigit -p` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessConfig {
    pub prompt: String,
    /// Durable session id. A fresh UUID is generated unless `--resume` names
    /// an existing session.
    pub session_id: String,
    /// Restore the saved conversation before sending `prompt`.
    pub resume: bool,
    /// Working directory to enter before anything loads (instruction files and
    /// project-local MCP config then resolve from it).
    pub cwd: Option<PathBuf>,
    /// Extra project roots beyond `cwd`, the headless counterpart of a
    /// multi-root project in the editor (see `workspace.rs`).
    pub add_dirs: Vec<PathBuf>,
    /// Print only the final assistant message to stdout (no streaming).
    pub quiet: bool,
    /// Machine-readable event output for CI and factory clients.
    pub output: OutputFormat,
    /// Tools pre-approved for the run (fed to `permissions::grant_for_session`).
    pub allow_tools: Vec<String>,
    /// Tools blocked for the run, overriding even settings-level allow.
    pub deny_tools: Vec<String>,
    /// How many rounds of tool calls the run may make before the model is
    /// asked for its final message. `--max-tool-rounds`, else
    /// `SIGIT_MAX_TOOL_ROUNDS`, else the built-in cap.
    pub max_tool_rounds: usize,
}

/// Upper bound for `--max-tool-rounds` / `SIGIT_MAX_TOOL_ROUNDS`.
pub const MAX_TOOL_ROUNDS_LIMIT: usize = 500;

/// Environment override for the tool-round cap of a headless run or an ACP
/// prompt turn.
pub const MAX_TOOL_ROUNDS_ENV: &str = "SIGIT_MAX_TOOL_ROUNDS";

fn parse_tool_rounds(value: &str, source: &str) -> Result<usize, String> {
    match value.trim().parse::<usize>() {
        Ok(rounds) if (1..=MAX_TOOL_ROUNDS_LIMIT).contains(&rounds) => Ok(rounds),
        _ => Err(format!(
            "{source} must be a whole number from 1 to {MAX_TOOL_ROUNDS_LIMIT}"
        )),
    }
}

/// The tool-round cap for a run: the flag wins, then the environment, then
/// the built-in cap. An empty environment value counts as unset.
fn resolve_max_tool_rounds(flag: Option<usize>, env: Option<&str>) -> Result<usize, String> {
    if let Some(rounds) = flag {
        return Ok(rounds);
    }
    match env.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => parse_tool_rounds(value, MAX_TOOL_ROUNDS_ENV),
        None => Ok(crate::MAX_TOOL_ROUNDS),
    }
}

/// The tool-round cap for a surface with no flag of its own (an ACP prompt
/// turn). A value that does not parse is logged and ignored: the editor
/// launched the agent, so there is nobody to show a startup error to.
pub fn max_tool_rounds_from_env() -> usize {
    resolve_max_tool_rounds(None, std::env::var(MAX_TOOL_ROUNDS_ENV).ok().as_deref())
        .unwrap_or_else(|error| {
            log::warn!(
                "{error}; using the built-in cap of {}",
                crate::MAX_TOOL_ROUNDS
            );
            crate::MAX_TOOL_ROUNDS
        })
}

/// What the model is told when a run would otherwise end with no final
/// message. `cap_reached` says the tool-round cap is why.
fn wrap_up_prompt(cap_reached: bool, max_tool_rounds: usize) -> String {
    let lead = if cap_reached {
        format!("You have used all {max_tool_rounds} tool rounds available for this run. ")
    } else {
        String::new()
    };
    format!(
        "{lead}Write your final message now, without calling any tools: what you did, \
         what you found, and anything left unfinished."
    )
}

/// Parse the process arguments (without argv[0]) for headless mode.
///
/// Returns `Ok(None)` when neither `run` nor `-p`/`--prompt` is present — the
/// invocation is not headless and falls through to the TTY/ACP dispatch. Once
/// headless mode is selected,
/// every remaining argument must be a recognized flag (position-insensitive);
/// anything else is a usage error the caller reports on stderr with exit 2.
pub fn parse_args(args: &[String]) -> Result<Option<HeadlessConfig>, String> {
    let run_command = args.first().is_some_and(|arg| arg == "run");
    if !run_command && !args.iter().any(|arg| arg == "-p" || arg == "--prompt") {
        return Ok(None);
    }

    let mut prompt: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut add_dirs: Vec<PathBuf> = Vec::new();
    let mut quiet = false;
    let mut output = OutputFormat::Text;
    let mut resume_session: Option<String> = None;
    let mut allow_tools: Vec<String> = Vec::new();
    let mut deny_tools: Vec<String> = Vec::new();
    let mut max_tool_rounds: Option<usize> = None;

    let mut iter = args.iter().skip(usize::from(run_command));
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" | "--prompt" => {
                let value = iter
                    .next()
                    .ok_or_else(|| format!("{arg} requires a prompt argument"))?;
                if prompt.is_some() {
                    return Err("the prompt was given more than once".to_string());
                }
                prompt = Some(value.clone());
            }
            "--cwd" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--cwd requires a directory argument".to_string())?;
                cwd = Some(PathBuf::from(value));
            }
            "--add-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--add-dir requires a directory argument".to_string())?;
                add_dirs.push(PathBuf::from(value));
            }
            "--quiet" => quiet = true,
            "--output" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--output requires text or jsonl".to_string())?;
                output = match value.as_str() {
                    "text" => OutputFormat::Text,
                    "jsonl" => OutputFormat::Jsonl,
                    _ => return Err("--output must be text or jsonl".to_string()),
                };
            }
            "--resume" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--resume requires a session id".to_string())?;
                if resume_session.is_some() {
                    return Err("--resume was given more than once".to_string());
                }
                validate_session_id(value)?;
                resume_session = Some(value.clone());
            }
            "--allow-tool" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--allow-tool requires a tool name".to_string())?;
                allow_tools.push(value.clone());
            }
            "--deny-tool" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--deny-tool requires a tool name".to_string())?;
                deny_tools.push(value.clone());
            }
            "--max-tool-rounds" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--max-tool-rounds requires a number".to_string())?;
                max_tool_rounds = Some(parse_tool_rounds(value, "--max-tool-rounds")?);
            }
            // A mistyped flag must not become the prompt. A prompt that
            // really starts with '-' can still go through -p.
            other if other.starts_with('-') => return Err(format!("unknown argument: {other}")),
            other if run_command && prompt.is_none() => prompt = Some(other.to_string()),
            other if run_command => {
                return Err(format!(
                    "unexpected extra argument: {other} (quote the prompt as one argument)"
                ));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    // Reaching this without a prompt means `run` had no positional prompt, or
    // a legacy prompt flag was consumed as another flag's value.
    let prompt = prompt.ok_or_else(|| {
        if run_command {
            "missing prompt".to_string()
        } else {
            "missing -p/--prompt".to_string()
        }
    })?;
    if prompt.trim().is_empty() {
        return Err("the prompt must not be empty".to_string());
    }
    if quiet && output == OutputFormat::Jsonl {
        return Err("--quiet cannot be combined with --output jsonl".to_string());
    }

    let resume = resume_session.is_some();
    let session_id = resume_session.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let max_tool_rounds = resolve_max_tool_rounds(
        max_tool_rounds,
        std::env::var(MAX_TOOL_ROUNDS_ENV).ok().as_deref(),
    )?;

    Ok(Some(HeadlessConfig {
        prompt,
        session_id,
        resume,
        cwd,
        add_dirs,
        quiet,
        output,
        allow_tools,
        deny_tools,
        max_tool_rounds,
    }))
}

fn validate_session_id(session_id: &str) -> Result<(), String> {
    if session_id.is_empty()
        || !session_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
    {
        return Err(
            "session ids may contain only ASCII letters, numbers, '.', '_', and '-'".to_string(),
        );
    }
    Ok(())
}

/// Denial fed to the model when a tool at `ask` level fires in a headless run.
fn not_preapproved_denial(tool_name: &str) -> String {
    format!(
        "`{tool_name}` was not executed: this is a non-interactive headless run and \
         nobody can answer a permission prompt, so tools at the `ask` level are denied \
         unless pre-approved. The user can re-run with `--allow-tool {tool_name}` to \
         approve it (or set SIGIT_PERMISSIONS=allow). Do not retry the same call; \
         continue without this tool or report what remains to be done."
    )
}

/// Denial fed to the model when a tool was blocked with `--deny-tool`.
fn deny_flag_denial(tool_name: &str) -> String {
    format!(
        "`{tool_name}` is blocked for this headless run (--deny-tool). Do not retry it; \
         continue without this tool or report what remains to be done."
    )
}

/// Run one headless prompt to completion. Returns the process exit code.
///
/// The caller (`main`) has already applied `--cwd` and `--add-dir`, initialized
/// logging to stderr, set up the model cache, and run MCP discovery.
pub async fn run(config: HeadlessConfig) -> i32 {
    // Fresh permission state for the run, then apply the flag grants.
    permissions::reset_session(&config.session_id);
    tools::set_active_session(Some(&config.session_id));
    for tool in &config.allow_tools {
        // No call arguments at grant time: the empty string records the bare
        // tool name, granting the whole tool — the --allow-tool contract.
        permissions::grant_for_session(&config.session_id, tool, "");
    }
    let denied: HashSet<&str> = config.deny_tools.iter().map(String::as_str).collect();

    // Backend resolution mirrors the ACP server: the explicit provider override
    // first, else the signed-in cloud tier when local inference is off (what
    // `apply_startup_inference_mode` does at every ACP session entry). There is
    // never an implicit on-device load — a fresh process has no model in memory
    // and headless mode must not silently download gigabytes.
    let provider_cfg = provider::active_provider().or_else(|| {
        if settings::local_inference_enabled() {
            None
        } else {
            provider::cloud_tier_provider(provider::DEFAULT_CLOUD_TIER)
        }
    });
    let Some(cfg) = provider_cfg else {
        emit_error(
            &config,
            "headless mode needs a remote inference provider — running on-device would require \
             loading (and possibly downloading) a local model, which headless mode never does \
             implicitly. Set OPENAI_BASE_URL and OPENAI_API_KEY (or configure providers.toml), \
             or sign in with `sigit login` and turn local inference off.",
        );
        return 1;
    };

    log::info!(
        "headless: using {} (model {}) at {}",
        cfg.display_name,
        cfg.model,
        cfg.base_url
    );
    crate::register_subagent_factory_for(&cfg);

    // Same always-on project context the other surfaces inject: cwd guidance
    // plus AGENTS.md / CLAUDE.md instruction files.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut system_prompt = crate::system_prompt_for_model(true);
    system_prompt.push_str("\n\n");
    system_prompt.push_str(&crate::session_context_message(
        &cwd,
        &crate::workspace::additional_roots(),
    ));

    let backend: Arc<dyn InferenceBackend> = Arc::new(OpenAiBackend::new(
        cfg.base_url,
        cfg.api_key,
        cfg.model.clone(),
        Some(system_prompt),
    ));

    if config.resume {
        let Some(history) = session_store::load(&config.session_id) else {
            emit_error(&config, "saved session not found");
            return 1;
        };
        backend::adopt_carryover(backend.as_ref(), backend::carryover_history(history)).await;
    }

    emit_event(
        &config,
        serde_json::json!({
            "type": "session",
            "session_id": config.session_id,
            "resumed": config.resume,
        }),
    );
    if config.output == OutputFormat::Text {
        eprintln!("Session: {}", config.session_id);
    }

    let outcome = run_prompt(backend.as_ref(), &config, &denied).await;

    // Persist the conversation like the other surfaces, so a follow-up feature
    // can resume it. Saved even on error: a partial transcript beats none.
    let snapshot = backend.history_snapshot().await;
    if let Err(error) = session_store::save(&config.session_id, &snapshot) {
        log::warn!("headless: session save failed: {error}");
    }
    session_store::save_meta(
        &config.session_id,
        &cwd,
        &crate::workspace::additional_roots(),
        Some(&cfg.model),
    );

    match outcome {
        Ok(()) => 0,
        Err(error) => {
            emit_error(&config, &error.to_string());
            1
        }
    }
}

fn emit_event(config: &HeadlessConfig, event: serde_json::Value) {
    if config.output == OutputFormat::Jsonl {
        println!("{event}");
        let _ = std::io::stdout().flush();
    }
}

fn emit_error(config: &HeadlessConfig, message: &str) {
    if config.output == OutputFormat::Jsonl {
        emit_event(
            config,
            serde_json::json!({
                "type": "error",
                "session_id": config.session_id,
                "message": message,
            }),
        );
    } else {
        eprintln!("sigit: {message}");
    }
}

/// The turn loop: send the prompt, execute tool calls under the permission
/// policy, feed results back, repeat up to `config.max_tool_rounds` — the same
/// shape as the ACP `handle_prompt`, minus the ACP notifications.
async fn run_prompt(
    backend: &dyn InferenceBackend,
    config: &HeadlessConfig,
    denied: &HashSet<&str>,
) -> Result<(), backend::BackendError> {
    let tools = crate::agent_tools_as_specs();

    // Token sink: assistant text streams through this while a turn runs; the
    // drain loop forwards the visible portion to stdout live. In quiet mode no
    // sink is passed and only the final message is printed.
    let (sink, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<TokenChunk>();
    let sink_opt = if config.quiet { None } else { Some(&sink) };
    let mut reply = crate::StreamedReply::default();

    let mut result = drain_to_stdout(
        backend.send_message_with_tools(&config.prompt, &tools, sink_opt),
        &mut sink_rx,
        &mut reply,
        config,
    )
    .await?;

    let mut round = 0;

    while !result.tool_calls.is_empty() && round < config.max_tool_rounds {
        round += 1;

        // Auto-compaction: long tool runs grow history fast; fold it into a
        // summary before the next round rather than blowing the window.
        let estimate = backend::estimate_tokens(&backend.history_snapshot().await);
        if estimate > backend::DEFAULT_CONTEXT_TOKEN_BUDGET {
            log::info!(
                "headless: history ≈{estimate} tokens exceeds budget {} — compacting",
                backend::DEFAULT_CONTEXT_TOKEN_BUDGET
            );
            if let Err(error) = backend.compact_history(backend::COMPACT_KEEP_LAST).await {
                log::warn!("headless: compaction failed: {error}");
            }
        }

        let mut tool_results = Vec::new();

        for tc in &result.tool_calls {
            let args_preview: String = tc.arguments.chars().take(120).collect();
            if config.output == OutputFormat::Jsonl {
                emit_event(
                    config,
                    serde_json::json!({
                        "type": "tool_call",
                        "session_id": config.session_id,
                        "tool_call_id": tc.id,
                        "name": tc.name,
                        "arguments": tc.arguments,
                    }),
                );
            } else {
                eprintln!("→ {}({args_preview})", tc.name);
            }

            // The headless deny set outranks everything, including a
            // settings-level allow and a --allow-tool grant for the same name.
            let output = if denied.contains(tc.name.as_str()) {
                log::info!("headless: {} blocked by --deny-tool", tc.name);
                deny_flag_denial(&tc.name)
            } else {
                match permissions::decision_for(&config.session_id, &tc.name, &tc.arguments) {
                    permissions::Decision::Allow => {
                        tools::execute_tool(&tc.name, &tc.arguments).await
                    }
                    permissions::Decision::Deny(reason) => {
                        log::info!("headless: {} denied by policy", tc.name);
                        reason
                    }
                    // Nobody can answer an interactive prompt here: ask
                    // collapses to a denial pointing at --allow-tool.
                    permissions::Decision::Ask => {
                        log::info!("headless: {} not pre-approved — denied", tc.name);
                        not_preapproved_denial(&tc.name)
                    }
                }
            };

            emit_event(
                config,
                serde_json::json!({
                    "type": "tool_result",
                    "session_id": config.session_id,
                    "tool_call_id": tc.id,
                    "name": tc.name,
                    "content": output,
                }),
            );

            tool_results.push(BackendToolResult {
                tool_call_id: tc.id.clone(),
                content: output,
            });
        }

        let allow_tool_calls = round < config.max_tool_rounds;

        // Whatever this round says starts a new paragraph rather than
        // continuing the sentence the tool calls interrupted.
        reply.interrupt();

        result = drain_to_stdout(
            backend.send_tool_results(tool_results, &tools, allow_tool_calls, sink_opt),
            &mut sink_rx,
            &mut reply,
            config,
        )
        .await?;
    }

    // A run can reach this point with nothing to show: the last round was
    // forced to answer in text and the model answered with nothing, most
    // often because it was cut off at the tool-round cap mid-task. A caller
    // reading stdout could not tell that from a run with nothing to report,
    // so ask once, plainly, for the final message.
    if crate::chat::strip_think_blocks(result.text.trim())
        .1
        .is_empty()
    {
        let cap_reached = round >= config.max_tool_rounds;
        if cap_reached {
            log::warn!(
                "headless: reached the cap of {} tool round(s) without a final message",
                config.max_tool_rounds
            );
        }
        reply.interrupt();
        match drain_to_stdout(
            backend.send_message_with_tools(
                &wrap_up_prompt(cap_reached, config.max_tool_rounds),
                &[],
                sink_opt,
            ),
            &mut sink_rx,
            &mut reply,
            config,
        )
        .await
        {
            Ok(wrap_up) => result = wrap_up,
            Err(error) => log::warn!("headless: asking for a final message failed: {error}"),
        }
    }

    // Final text: in quiet mode nothing streamed, so print the final assistant
    // message now; otherwise print only what streaming did not already cover.
    let (_think, final_visible) = crate::chat::strip_think_blocks(result.text.trim());
    let mut stdout = std::io::stdout();
    if config.output == OutputFormat::Jsonl {
        emit_event(
            config,
            serde_json::json!({
                "type": "result",
                "session_id": config.session_id,
                "text": final_visible,
                "tool_rounds": round,
            }),
        );
    } else if config.quiet {
        if !final_visible.is_empty() {
            let _ = writeln!(stdout, "{final_visible}");
        }
    } else if reply.streamed_any {
        // The reply is already on stdout; end the line for the shell.
        let _ = writeln!(stdout);
    } else if !final_visible.is_empty() {
        let _ = writeln!(stdout, "{final_visible}");
    }
    let _ = stdout.flush();

    log::info!("headless: prompt complete — {round} tool round(s)");
    Ok(())
}

/// Run one inference turn while forwarding streamed tokens to stdout as they
/// arrive — the stdio counterpart of `SiGitAgent::drain_turn`.
async fn drain_to_stdout<F>(
    fut: F,
    sink_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TokenChunk>,
    reply: &mut crate::StreamedReply,
    config: &HeadlessConfig,
) -> Result<TurnResult, backend::BackendError>
where
    F: std::future::Future<Output = Result<TurnResult, backend::BackendError>>,
{
    tokio::pin!(fut);
    let result = loop {
        tokio::select! {
            done = &mut fut => break done,
            Some(chunk) = sink_rx.recv() => {
                emit_token_chunk(chunk, reply, config);
            }
        }
    };
    // Flush tokens that landed between the last poll and the future resolving.
    while let Ok(chunk) = sink_rx.try_recv() {
        emit_token_chunk(chunk, reply, config);
    }
    result
}

/// Fold a streamed fragment into `reply` and print whatever it newly reveals —
/// the stdio counterpart of `SiGitAgent::emit_token_chunk`. Reasoning (from a
/// dedicated field or an inline `<think>` block) never joins the stdout answer
/// stream; under `--output jsonl` it surfaces as a separate `reasoning_delta`
/// event, and is dropped in plain mode.
fn emit_token_chunk(chunk: TokenChunk, reply: &mut crate::StreamedReply, config: &HeadlessConfig) {
    match chunk {
        TokenChunk::Reasoning(text) => emit_reasoning_delta(&text, config),
        TokenChunk::Visible(piece) => {
            let revealed = reply.push(&piece);
            if let Some(reasoning) = revealed.reasoning {
                emit_reasoning_delta(&reasoning, config);
            }
            if let Some(extra) = revealed.visible {
                if config.output == OutputFormat::Jsonl {
                    emit_event(
                        config,
                        serde_json::json!({
                            "type": "assistant_delta",
                            "session_id": config.session_id,
                            "text": extra,
                        }),
                    );
                } else {
                    print!("{extra}");
                }
                let _ = std::io::stdout().flush();
            }
        }
    }
}

/// Emit model reasoning as a structured event under `--output jsonl`. In plain
/// mode reasoning is not part of the answer, so it is dropped.
fn emit_reasoning_delta(text: &str, config: &HeadlessConfig) {
    if config.output == OutputFormat::Jsonl {
        emit_event(
            config,
            serde_json::json!({
                "type": "reasoning_delta",
                "session_id": config.session_id,
                "text": text,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn absent_prompt_flag_is_not_headless() {
        assert_eq!(parse_args(&args(&[])), Ok(None));
        assert_eq!(parse_args(&args(&["login"])), Ok(None));
        // Flags alone don't trigger headless mode; only -p/--prompt does.
        assert_eq!(parse_args(&args(&["--quiet"])), Ok(None));
    }

    #[test]
    fn parses_minimal_invocation() {
        let config = parse_args(&args(&["-p", "do the thing"])).unwrap().unwrap();
        assert_eq!(config.prompt, "do the thing");
        assert!(!config.session_id.is_empty());
        assert!(!config.resume);
        assert_eq!(config.cwd, None);
        assert!(!config.quiet);
        assert_eq!(config.output, OutputFormat::Text);
        assert!(config.allow_tools.is_empty());
        assert!(config.deny_tools.is_empty());
    }

    #[test]
    fn run_subcommand_accepts_a_positional_prompt() {
        let config = parse_args(&args(&["run", "do the thing"]))
            .unwrap()
            .unwrap();
        assert_eq!(config.prompt, "do the thing");
        assert!(!config.resume);
    }

    #[test]
    fn long_form_prompt_flag_works() {
        let config = parse_args(&args(&["--prompt", "hello"])).unwrap().unwrap();
        assert_eq!(config.prompt, "hello");
    }

    #[test]
    fn parses_all_flags_position_insensitively() {
        let config = parse_args(&args(&[
            "--quiet",
            "--allow-tool",
            "run_command",
            "--cwd",
            "/tmp/project",
            "-p",
            "build it",
            "--deny-tool",
            "delete_file",
            "--output",
            "text",
            "--allow-tool",
            "edit_file",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.prompt, "build it");
        assert_eq!(config.cwd, Some(PathBuf::from("/tmp/project")));
        assert!(config.quiet);
        assert_eq!(config.output, OutputFormat::Text);
        assert_eq!(config.allow_tools, vec!["run_command", "edit_file"]);
        assert_eq!(config.deny_tools, vec!["delete_file"]);
    }

    #[test]
    fn resume_reuses_the_named_session() {
        let config = parse_args(&args(&[
            "run",
            "continue",
            "--resume",
            "session-123",
            "--output",
            "jsonl",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.session_id, "session-123");
        assert!(config.resume);
        assert_eq!(config.output, OutputFormat::Jsonl);
    }

    #[test]
    fn resume_rejects_unsafe_session_ids() {
        let error = parse_args(&args(&["run", "continue", "--resume", "../escape"])).unwrap_err();
        assert!(error.contains("session ids"), "{error}");
    }

    #[test]
    fn max_tool_rounds_flag_is_parsed_and_bounded() {
        let config = parse_args(&args(&["-p", "work", "--max-tool-rounds", "60"]))
            .unwrap()
            .unwrap();
        assert_eq!(config.max_tool_rounds, 60);

        for bad in ["0", "501", "-1", "many", ""] {
            let error = parse_args(&args(&["-p", "work", "--max-tool-rounds", bad])).unwrap_err();
            assert!(error.contains("--max-tool-rounds"), "{bad}: {error}");
        }
        assert!(parse_args(&args(&["-p", "work", "--max-tool-rounds"])).is_err());
    }

    #[test]
    fn max_tool_rounds_resolves_flag_then_environment_then_default() {
        assert_eq!(resolve_max_tool_rounds(Some(7), Some("60")), Ok(7));
        assert_eq!(resolve_max_tool_rounds(None, Some(" 60 ")), Ok(60));
        assert_eq!(
            resolve_max_tool_rounds(None, None),
            Ok(crate::MAX_TOOL_ROUNDS)
        );
        // An empty variable is unset, not an error.
        assert_eq!(
            resolve_max_tool_rounds(None, Some("")),
            Ok(crate::MAX_TOOL_ROUNDS)
        );
        let error = resolve_max_tool_rounds(None, Some("lots")).unwrap_err();
        assert!(error.contains(MAX_TOOL_ROUNDS_ENV), "{error}");
        assert!(resolve_max_tool_rounds(None, Some("0")).is_err());
    }

    #[test]
    fn wrap_up_prompt_names_the_cap_only_when_it_was_reached() {
        let capped = wrap_up_prompt(true, 24);
        assert!(capped.contains("all 24 tool rounds"), "{capped}");
        assert!(capped.contains("without calling any tools"));

        let plain = wrap_up_prompt(false, 24);
        assert!(!plain.contains("tool rounds"), "{plain}");
        assert!(plain.starts_with("Write your final message now"));
    }

    #[test]
    fn quiet_and_jsonl_are_mutually_exclusive() {
        let error =
            parse_args(&args(&["run", "work", "--quiet", "--output", "jsonl"])).unwrap_err();
        assert!(error.contains("cannot be combined"), "{error}");
    }

    #[test]
    fn add_dir_is_repeatable_and_keeps_its_order() {
        let config = parse_args(&args(&[
            "-p",
            "look around",
            "--add-dir",
            "/tmp/lib",
            "--add-dir",
            "/tmp/docs",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            config.add_dirs,
            vec![PathBuf::from("/tmp/lib"), PathBuf::from("/tmp/docs")]
        );
    }

    #[test]
    fn add_dir_without_a_value_is_a_usage_error() {
        assert!(parse_args(&args(&["-p", "x", "--add-dir"])).is_err());
    }

    #[test]
    fn prompt_value_may_look_like_a_flag() {
        // -p consumes the next argument verbatim.
        let config = parse_args(&args(&["-p", "--quiet"])).unwrap().unwrap();
        assert_eq!(config.prompt, "--quiet");
        assert!(!config.quiet);
    }

    #[test]
    fn unknown_flag_is_a_usage_error() {
        let error = parse_args(&args(&["-p", "x", "--frobnicate"])).unwrap_err();
        assert!(error.contains("--frobnicate"), "{error}");
    }

    #[test]
    fn missing_values_are_usage_errors() {
        assert!(parse_args(&args(&["-p"])).is_err());
        assert!(parse_args(&args(&["-p", "x", "--cwd"])).is_err());
        assert!(parse_args(&args(&["-p", "x", "--allow-tool"])).is_err());
        assert!(parse_args(&args(&["-p", "x", "--deny-tool"])).is_err());
        assert!(parse_args(&args(&["run", "x", "--output"])).is_err());
        assert!(parse_args(&args(&["run", "x", "--resume"])).is_err());
    }

    #[test]
    fn duplicate_prompt_is_a_usage_error() {
        assert!(parse_args(&args(&["-p", "a", "--prompt", "b"])).is_err());
    }

    #[test]
    fn run_rejects_a_second_prompt_from_either_source() {
        let error = parse_args(&args(&["run", "a", "b"])).unwrap_err();
        assert!(error.contains("unexpected extra argument: b"), "{error}");
        let error = parse_args(&args(&["run", "a", "-p", "b"])).unwrap_err();
        assert!(error.contains("more than once"), "{error}");
        let error = parse_args(&args(&["run", "-p", "a", "b"])).unwrap_err();
        assert!(error.contains("unexpected extra argument: b"), "{error}");
    }

    #[test]
    fn run_does_not_take_a_mistyped_flag_as_the_prompt() {
        let error = parse_args(&args(&["run", "--quite", "fix it"])).unwrap_err();
        assert!(error.contains("unknown argument: --quite"), "{error}");
    }

    #[test]
    fn bare_run_reports_a_missing_prompt() {
        assert_eq!(
            parse_args(&args(&["run"])),
            Err("missing prompt".to_string())
        );
    }

    #[test]
    fn empty_prompt_is_a_usage_error() {
        assert!(parse_args(&args(&["-p", "   "])).is_err());
    }
}
