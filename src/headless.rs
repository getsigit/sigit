//! Headless one-shot mode: `sigit run`.
//!
//! Runs a single task non-interactively and exits: the prompt arrives as a
//! CLI flag, progress is emitted as JSONL events on stdout (one object per
//! line), and logs stay on stderr. Built for cloud runners (siGit Code Cloud
//! Agent) and scripting, where nobody is present to answer a permission
//! prompt — on `Decision::Ask` the tool is declined with a pointer to
//! `SIGIT_PERMISSIONS=allow` instead of blocking.
//!
//! The tool loop mirrors the ACP prompt handler (`handle_prompt` in
//! `main.rs`): permission gate → execute → feed results back, with
//! auto-compaction between rounds and a forced text reply on the final
//! round. Keep the two in sync when changing loop semantics.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use crate::backend::{self, InferenceBackend, OpenAiBackend, ToolResult, ToolSpec};
use crate::{permissions, provider, tools};

/// Headless runs default to a higher round cap than interactive prompts: an
/// autonomous task routinely needs long edit/build/test chains and there is
/// no user present to re-prompt a stopped run.
const DEFAULT_MAX_ROUNDS: usize = 40;

/// Cap on `arguments`/`output` strings embedded in JSONL events. Full outputs
/// still reach the model; events only need enough for a live transcript.
const EVENT_FIELD_MAX_CHARS: usize = 4_000;

const USAGE: &str = "usage: sigit run [--prompt <text> | --prompt-file <path>] \
                     [--cwd <dir>] [--max-rounds <n>] [--output jsonl|text]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Jsonl,
    Text,
}

#[derive(Debug)]
pub struct HeadlessOptions {
    pub prompt: String,
    pub cwd: PathBuf,
    pub max_rounds: usize,
    pub output: OutputMode,
}

/// The value following a flag, or a usage error naming the flag.
fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
}

/// Parse `sigit run` arguments (everything after the subcommand).
pub fn parse_args(mut args: impl Iterator<Item = String>) -> Result<HeadlessOptions, String> {
    let mut prompt: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut max_rounds = DEFAULT_MAX_ROUNDS;
    let mut output = OutputMode::Jsonl;

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--prompt" => {
                let value = next_value(&mut args, "--prompt")?;
                if prompt.is_some() {
                    return Err(format!("give --prompt or --prompt-file once\n{USAGE}"));
                }
                prompt = Some(value);
            }
            "--prompt-file" => {
                let path = next_value(&mut args, "--prompt-file")?;
                if prompt.is_some() {
                    return Err(format!("give --prompt or --prompt-file once\n{USAGE}"));
                }
                let text = std::fs::read_to_string(&path)
                    .map_err(|error| format!("cannot read --prompt-file {path}: {error}"))?;
                prompt = Some(text);
            }
            "--cwd" => {
                cwd = Some(PathBuf::from(next_value(&mut args, "--cwd")?));
            }
            "--max-rounds" => {
                max_rounds = next_value(&mut args, "--max-rounds")?
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("--max-rounds needs a positive integer\n{USAGE}"))?;
            }
            "--output" => {
                output = match next_value(&mut args, "--output")?.as_str() {
                    "jsonl" => OutputMode::Jsonl,
                    "text" => OutputMode::Text,
                    other => return Err(format!("unknown --output {other}\n{USAGE}")),
                };
            }
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }

    let prompt = prompt
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("a non-empty --prompt or --prompt-file is required\n{USAGE}"))?;

    let cwd = cwd.unwrap_or_else(|| PathBuf::from("."));
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("--cwd {}: {error}", cwd.display()))?;

    Ok(HeadlessOptions {
        prompt,
        cwd,
        max_rounds,
        output,
    })
}

/// Entry point for `sigit run`. Never returns on failure paths — exits the
/// process with 0 (run completed), 1 (run failed), or 2 (usage/config error).
pub async fn run(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("sigit run: {message}");
            std::process::exit(2);
        }
    };

    // Provider: the explicit override (env / providers.toml) first — this is
    // how a cloud runner injects a per-run endpoint and token — then the
    // signed-in cloud as a convenience. Never fall back to on-device: a
    // headless host should not silently download a multi-GB model.
    let Some(config) =
        provider::active_provider().or_else(|| provider::cloud_tier_provider("large"))
    else {
        eprintln!(
            "sigit run: no inference provider configured. Set OPENAI_BASE_URL and \
             OPENAI_API_KEY (and optionally SIGIT_MODEL), configure providers.toml, \
             or sign in with `sigit login`."
        );
        std::process::exit(2);
    };

    if let Err(error) = std::env::set_current_dir(&options.cwd) {
        eprintln!("sigit run: cannot enter {}: {error}", options.cwd.display());
        std::process::exit(2);
    }

    let system_prompt = format!(
        "{}\n\n{}",
        crate::system_prompt_for_model(true),
        crate::session_context_message(&options.cwd)
    );
    let backend: Arc<dyn InferenceBackend> = Arc::new(OpenAiBackend::new(
        config.base_url.clone(),
        config.api_key.clone(),
        config.model.clone(),
        Some(system_prompt),
    ));
    crate::register_subagent_factory_for(&config);
    let tools = crate::agent_tools_as_specs();

    let emitter = Emitter {
        mode: options.output,
    };
    emitter.event(json!({
        "type": "run_started",
        "cwd": options.cwd.display().to_string(),
        "model": config.model,
        "max_rounds": options.max_rounds,
    }));

    let rounds = match drive_loop(&backend, &tools, &options, &emitter).await {
        Ok((summary, rounds)) => {
            emitter.event(json!({
                "type": "result",
                "status": "completed",
                "summary": summary,
                "rounds": rounds,
            }));
            rounds
        }
        Err((error, rounds)) => {
            emitter.event(json!({
                "type": "result",
                "status": "failed",
                "error": error,
                "rounds": rounds,
            }));
            std::process::exit(1);
        }
    };
    log::info!("headless run completed after {rounds} tool round(s)");
    Ok(())
}

/// The tool loop. Returns `(summary, rounds)` or `(error, rounds)`.
///
/// Keep in sync with `handle_prompt` in `main.rs`: same permission gate, same
/// auto-compaction trigger, same force-text final round.
async fn drive_loop(
    backend: &Arc<dyn InferenceBackend>,
    tools: &[ToolSpec],
    options: &HeadlessOptions,
    emitter: &Emitter,
) -> Result<(String, usize), (String, usize)> {
    // Permission decisions are per-session state; a headless process is one
    // session. There are no grants to accumulate (nobody can answer "always
    // allow"), the id only namespaces the lookup.
    let session = format!("headless-{}", std::process::id());

    let mut result = backend
        .send_message_with_tools(&options.prompt, tools, None)
        .await
        .map_err(|error| (format!("inference failed: {error}"), 0))?;
    emitter.turn_text(&result.text);

    let mut round = 0usize;

    while !result.tool_calls.is_empty() && round < options.max_rounds {
        round += 1;

        // Auto-compaction: long tool runs grow history fast; fold it into a
        // summary before the next round rather than blowing the window.
        let estimate = backend::estimate_tokens(&backend.history_snapshot().await);
        if estimate > backend::DEFAULT_CONTEXT_TOKEN_BUDGET {
            match backend.compact_history(backend::COMPACT_KEEP_LAST).await {
                Ok(()) => {
                    let after = backend::estimate_tokens(&backend.history_snapshot().await);
                    emitter.event(json!({
                        "type": "compaction",
                        "approx_tokens_before": estimate,
                        "approx_tokens_after": after,
                    }));
                }
                Err(error) => log::warn!("headless compaction failed: {error}"),
            }
        }

        let mut tool_results = Vec::new();
        for call in &result.tool_calls {
            emitter.tool_call(call);
            let (output, denied) = match permissions::decision_for(&session, &call.name) {
                permissions::Decision::Allow => (
                    tools::execute_tool(&call.name, &call.arguments).await,
                    false,
                ),
                permissions::Decision::Deny(reason) => {
                    log::info!("headless: {} denied by policy", call.name);
                    (reason, true)
                }
                permissions::Decision::Ask => {
                    log::info!("headless: {} needs approval, declining", call.name);
                    (
                        format!(
                            "`{}` was not executed: headless mode cannot prompt for \
                             permission. Run with SIGIT_PERMISSIONS=allow to auto-approve \
                             mutating tools, or grant this tool in settings.toml.",
                            call.name
                        ),
                        true,
                    )
                }
            };
            emitter.tool_result(call, &output, denied);
            tool_results.push(ToolResult {
                tool_call_id: call.id.clone(),
                content: output,
            });
        }

        let next_tools = if round < options.max_rounds {
            Some(tools)
        } else {
            None // last round: force a text reply
        };
        result = backend
            .send_tool_results(tool_results, next_tools, None)
            .await
            .map_err(|error| (format!("inference failed: {error}"), round))?;
        emitter.turn_text(&result.text);
    }

    let (_think, visible) = crate::chat::strip_think_blocks(&result.text);
    let summary = if visible.trim().is_empty() {
        "The run finished without a final summary.".to_string()
    } else {
        visible.trim().to_string()
    };
    Ok((summary, round))
}

// ── Event output ─────────────────────────────────────────────────────────────

struct Emitter {
    mode: OutputMode,
}

impl Emitter {
    /// Write one event. JSONL mode prints the object as-is; text mode renders
    /// a human-oriented line per event kind.
    fn event(&self, event: serde_json::Value) {
        match self.mode {
            OutputMode::Jsonl => {
                let mut stdout = std::io::stdout().lock();
                let _ = writeln!(stdout, "{event}");
                let _ = stdout.flush();
            }
            OutputMode::Text => {
                let line = match event["type"].as_str() {
                    Some("run_started") => format!(
                        "▶ run started in {} (model {})",
                        event["cwd"].as_str().unwrap_or("?"),
                        event["model"].as_str().unwrap_or("?"),
                    ),
                    Some("turn_text") => event["text"].as_str().unwrap_or_default().to_string(),
                    Some("tool_call") => format!(
                        "→ {}({})",
                        event["name"].as_str().unwrap_or("?"),
                        event["arguments"].as_str().unwrap_or_default(),
                    ),
                    Some("tool_result") => format!(
                        "← {} ({} chars{})",
                        event["name"].as_str().unwrap_or("?"),
                        event["output_chars"].as_u64().unwrap_or(0),
                        if event["denied"].as_bool().unwrap_or(false) {
                            ", denied"
                        } else {
                            ""
                        },
                    ),
                    Some("compaction") => "… compacted conversation history".to_string(),
                    Some("result") => match event["status"].as_str() {
                        Some("completed") => format!(
                            "✔ completed\n{}",
                            event["summary"].as_str().unwrap_or_default()
                        ),
                        _ => format!("✘ failed: {}", event["error"].as_str().unwrap_or("?")),
                    },
                    _ => event.to_string(),
                };
                if !line.is_empty() {
                    let mut stdout = std::io::stdout().lock();
                    let _ = writeln!(stdout, "{line}");
                    let _ = stdout.flush();
                }
            }
        }
    }

    /// Emit the visible part of an assistant turn, skipping empty turns.
    fn turn_text(&self, raw: &str) {
        let (_think, visible) = crate::chat::strip_think_blocks(raw);
        let visible = visible.trim();
        if visible.is_empty() {
            return;
        }
        let (text, truncated) = clip(visible, EVENT_FIELD_MAX_CHARS);
        let mut event = json!({ "type": "turn_text", "text": text });
        if truncated {
            event["truncated"] = json!(true);
        }
        self.event(event);
    }

    fn tool_call(&self, call: &backend::ToolCall) {
        let (arguments, truncated) = clip(&call.arguments, EVENT_FIELD_MAX_CHARS);
        let mut event = json!({
            "type": "tool_call",
            "id": call.id,
            "name": call.name,
            "arguments": arguments,
        });
        if truncated {
            event["truncated"] = json!(true);
        }
        self.event(event);
    }

    fn tool_result(&self, call: &backend::ToolCall, output: &str, denied: bool) {
        let (clipped, truncated) = clip(output, EVENT_FIELD_MAX_CHARS);
        let mut event = json!({
            "type": "tool_result",
            "id": call.id,
            "name": call.name,
            "output_chars": output.chars().count(),
            "output": clipped,
            "denied": denied,
        });
        if truncated {
            event["truncated"] = json!(true);
        }
        self.event(event);
    }
}

/// Truncate to `max` characters (not bytes — always on a char boundary).
fn clip(text: &str, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        (text.to_string(), false)
    } else {
        (text.chars().take(max).collect(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = String> {
        list.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_prompt_and_defaults() {
        let options = parse_args(args(&["--prompt", "fix the tests"])).expect("parses");
        assert_eq!(options.prompt, "fix the tests");
        assert_eq!(options.max_rounds, DEFAULT_MAX_ROUNDS);
        assert_eq!(options.output, OutputMode::Jsonl);
        assert!(options.cwd.is_absolute());
    }

    #[test]
    fn requires_a_prompt() {
        let error = parse_args(args(&[])).expect_err("missing prompt");
        assert!(error.contains("--prompt"));
    }

    #[test]
    fn rejects_empty_prompt() {
        let error = parse_args(args(&["--prompt", "   "])).expect_err("blank prompt");
        assert!(error.contains("non-empty"));
    }

    #[test]
    fn rejects_prompt_and_prompt_file_together() {
        let file = std::env::temp_dir().join(format!("sigit-prompt-{}.txt", std::process::id()));
        std::fs::write(&file, "task").unwrap();
        let error = parse_args(args(&[
            "--prompt",
            "one",
            "--prompt-file",
            file.to_str().unwrap(),
        ]))
        .expect_err("both prompt flags");
        assert!(error.contains("once"));
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn reads_prompt_file() {
        let file = std::env::temp_dir().join(format!("sigit-promptf-{}.txt", std::process::id()));
        std::fs::write(&file, "task from file\n").unwrap();
        let options = parse_args(args(&["--prompt-file", file.to_str().unwrap()])).expect("parses");
        assert_eq!(options.prompt, "task from file");
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn rejects_bad_flags_and_values() {
        assert!(parse_args(args(&["--prompt", "x", "--max-rounds", "0"])).is_err());
        assert!(parse_args(args(&["--prompt", "x", "--max-rounds", "abc"])).is_err());
        assert!(parse_args(args(&["--prompt", "x", "--output", "yaml"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
        assert!(parse_args(args(&["--prompt", "x", "--cwd", "/definitely/not/a/dir"])).is_err());
    }

    #[test]
    fn parses_overrides() {
        let options = parse_args(args(&[
            "--prompt",
            "x",
            "--max-rounds",
            "7",
            "--output",
            "text",
        ]))
        .expect("parses");
        assert_eq!(options.max_rounds, 7);
        assert_eq!(options.output, OutputMode::Text);
    }

    #[test]
    fn clip_is_char_boundary_safe() {
        let (out, truncated) = clip("héllo wörld", 5);
        assert_eq!(out, "héllo");
        assert!(truncated);
        let (out, truncated) = clip("short", 10);
        assert_eq!(out, "short");
        assert!(!truncated);
    }
}
