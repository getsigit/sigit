//! Headless programmatic mode: `sigit -p "<prompt>"` runs one prompt and exits.
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
    self, InferenceBackend, OpenAiBackend, ToolResult as BackendToolResult, TurnResult,
};
use crate::{permissions, provider, session_store, settings, tools};

/// Session id for headless runs: permission grants and the saved conversation
/// live under this key, so a follow-up feature can resume it.
pub const HEADLESS_SESSION: &str = "headless";

pub const USAGE: &str = "Usage: sigit -p \"<prompt>\" [--cwd <dir>] [--quiet] \
                         [--allow-tool <name>]... [--deny-tool <name>]...";

/// Parsed `sigit -p` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessConfig {
    pub prompt: String,
    /// Working directory to enter before anything loads (instruction files and
    /// project-local MCP config then resolve from it).
    pub cwd: Option<PathBuf>,
    /// Print only the final assistant message to stdout (no streaming).
    pub quiet: bool,
    /// Tools pre-approved for the run (fed to `permissions::grant_for_session`).
    pub allow_tools: Vec<String>,
    /// Tools blocked for the run, overriding even settings-level allow.
    pub deny_tools: Vec<String>,
}

/// Parse the process arguments (without argv[0]) for headless mode.
///
/// Returns `Ok(None)` when `-p`/`--prompt` is absent — the invocation is not
/// headless and falls through to the TTY/ACP dispatch. Once `-p` is present,
/// every remaining argument must be a recognized flag (position-insensitive);
/// anything else is a usage error the caller reports on stderr with exit 2.
pub fn parse_args(args: &[String]) -> Result<Option<HeadlessConfig>, String> {
    if !args.iter().any(|arg| arg == "-p" || arg == "--prompt") {
        return Ok(None);
    }

    let mut prompt: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut quiet = false;
    let mut allow_tools: Vec<String> = Vec::new();
    let mut deny_tools: Vec<String> = Vec::new();

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-p" | "--prompt" => {
                let value = iter
                    .next()
                    .ok_or_else(|| format!("{arg} requires a prompt argument"))?;
                if prompt.is_some() {
                    return Err(format!("{arg} was given more than once"));
                }
                prompt = Some(value.clone());
            }
            "--cwd" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--cwd requires a directory argument".to_string())?;
                cwd = Some(PathBuf::from(value));
            }
            "--quiet" => quiet = true,
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
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    // Unreachable in practice (the pre-check saw a `-p` token), unless that
    // token was consumed as another flag's value — still a usage error.
    let prompt = prompt.ok_or_else(|| "missing -p/--prompt".to_string())?;
    if prompt.trim().is_empty() {
        return Err("the prompt must not be empty".to_string());
    }

    Ok(Some(HeadlessConfig {
        prompt,
        cwd,
        quiet,
        allow_tools,
        deny_tools,
    }))
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
/// The caller (`main`) has already applied `--cwd`, initialized logging to
/// stderr, set up the model cache, and run MCP discovery.
pub async fn run(config: HeadlessConfig) -> i32 {
    // Fresh permission state for the run, then apply the flag grants.
    permissions::reset_session(HEADLESS_SESSION);
    for tool in &config.allow_tools {
        permissions::grant_for_session(HEADLESS_SESSION, tool, "");
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
            provider::cloud_tier_provider("balanced")
        }
    });
    let Some(cfg) = provider_cfg else {
        eprintln!(
            "sigit: headless mode needs a remote inference provider — running on-device \
             would require loading (and possibly downloading) a local model, which \
             headless mode never does implicitly. Set OPENAI_BASE_URL and OPENAI_API_KEY \
             (or configure providers.toml), or sign in with `sigit login` and turn local \
             inference off."
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
    let mut system_prompt = crate::system_prompt_for_model(true).to_string();
    system_prompt.push_str("\n\n");
    system_prompt.push_str(&crate::session_context_message(&cwd));

    let backend: Arc<dyn InferenceBackend> = Arc::new(OpenAiBackend::new(
        cfg.base_url,
        cfg.api_key,
        cfg.model,
        Some(system_prompt),
    ));

    let outcome = run_prompt(backend.as_ref(), &config, &denied).await;

    // Persist the conversation like the other surfaces, so a follow-up feature
    // can resume it. Saved even on error: a partial transcript beats none.
    let snapshot = backend.history_snapshot().await;
    if let Err(error) = session_store::save(HEADLESS_SESSION, &snapshot) {
        log::warn!("headless: session save failed: {error}");
    }

    match outcome {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("sigit: {error}");
            1
        }
    }
}

/// The turn loop: send the prompt, execute tool calls under the permission
/// policy, feed results back, repeat up to `MAX_TOOL_ROUNDS` — the same shape
/// as the ACP `handle_prompt`, minus the ACP notifications.
async fn run_prompt(
    backend: &dyn InferenceBackend,
    config: &HeadlessConfig,
    denied: &HashSet<&str>,
) -> Result<(), backend::BackendError> {
    let tools = crate::agent_tools_as_specs();

    // Token sink: assistant text streams through this while a turn runs; the
    // drain loop forwards the visible portion to stdout live. In quiet mode no
    // sink is passed and only the final message is printed.
    let (sink, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let sink_opt = if config.quiet { None } else { Some(&sink) };
    let mut assembled = String::new();
    let mut sent = String::new();
    let mut streamed_any = false;

    let mut result = drain_to_stdout(
        backend.send_message_with_tools(&config.prompt, &tools, sink_opt),
        &mut sink_rx,
        &mut assembled,
        &mut sent,
        &mut streamed_any,
    )
    .await?;

    let mut round = 0;

    while !result.tool_calls.is_empty() && round < crate::MAX_TOOL_ROUNDS {
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
            eprintln!("→ {}({args_preview})", tc.name);

            // The headless deny set outranks everything, including a
            // settings-level allow and a --allow-tool grant for the same name.
            let output = if denied.contains(tc.name.as_str()) {
                log::info!("headless: {} blocked by --deny-tool", tc.name);
                deny_flag_denial(&tc.name)
            } else {
                match permissions::decision_for(HEADLESS_SESSION, &tc.name, &tc.arguments) {
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

            tool_results.push(BackendToolResult {
                tool_call_id: tc.id.clone(),
                content: output,
            });
        }

        let next_tools = if round < crate::MAX_TOOL_ROUNDS {
            Some(tools.as_slice())
        } else {
            None // last round: force text
        };

        result = drain_to_stdout(
            backend.send_tool_results(tool_results, next_tools, sink_opt),
            &mut sink_rx,
            &mut assembled,
            &mut sent,
            &mut streamed_any,
        )
        .await?;
    }

    // Final text: in quiet mode nothing streamed, so print the final assistant
    // message now; otherwise print only what streaming did not already cover.
    let (_think, final_visible) = crate::chat::strip_think_blocks(result.text.trim());
    let mut stdout = std::io::stdout();
    if config.quiet {
        if !final_visible.is_empty() {
            let _ = writeln!(stdout, "{final_visible}");
        }
    } else if streamed_any {
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
    sink_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    assembled: &mut String,
    sent: &mut String,
    streamed_any: &mut bool,
) -> Result<TurnResult, backend::BackendError>
where
    F: std::future::Future<Output = Result<TurnResult, backend::BackendError>>,
{
    tokio::pin!(fut);
    let result = loop {
        tokio::select! {
            done = &mut fut => break done,
            Some(piece) = sink_rx.recv() => {
                emit_visible_chunk(&piece, assembled, sent, streamed_any);
            }
        }
    };
    // Flush tokens that landed between the last poll and the future resolving.
    while let Ok(piece) = sink_rx.try_recv() {
        emit_visible_chunk(&piece, assembled, sent, streamed_any);
    }
    result
}

/// Append a streamed fragment, strip `<think>` reasoning from the running
/// text, and print only the newly revealed visible suffix. Tracking the
/// assembled text (not just deltas) keeps think-block stripping correct even
/// when a tag spans chunk boundaries — same approach as the ACP path.
fn emit_visible_chunk(
    piece: &str,
    assembled: &mut String,
    sent: &mut String,
    streamed_any: &mut bool,
) {
    assembled.push_str(piece);
    let (_think, visible) = crate::chat::strip_think_blocks(assembled);
    match visible.strip_prefix(sent.as_str()) {
        Some(extra) if !extra.is_empty() => {
            print!("{extra}");
            let _ = std::io::stdout().flush();
            *sent = visible;
            *streamed_any = true;
        }
        // No new visible text, or the visible prefix changed retroactively
        // (rare, e.g. a late-closing think tag): resync without reprinting.
        _ => *sent = visible,
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
        assert_eq!(config.cwd, None);
        assert!(!config.quiet);
        assert!(config.allow_tools.is_empty());
        assert!(config.deny_tools.is_empty());
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
            "--allow-tool",
            "edit_file",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.prompt, "build it");
        assert_eq!(config.cwd, Some(PathBuf::from("/tmp/project")));
        assert!(config.quiet);
        assert_eq!(config.allow_tools, vec!["run_command", "edit_file"]);
        assert_eq!(config.deny_tools, vec!["delete_file"]);
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
    }

    #[test]
    fn duplicate_prompt_is_a_usage_error() {
        assert!(parse_args(&args(&["-p", "a", "--prompt", "b"])).is_err());
    }

    #[test]
    fn empty_prompt_is_a_usage_error() {
        assert!(parse_args(&args(&["-p", "   "])).is_err());
    }
}
