---
name: tool-calling
description: Implement or debug tool calling in siGit Code across the app, Onde Inference, and mistral.rs. Use when working on tool schemas, execution loops, model support, session cwd handling, or tool-call troubleshooting.
---

# Skill: Tool Calling in siGit Code

## Overview

siGit Code supports **agentic tool calling** — the LLM invokes tools (read/write files, run commands, read websites) to operate on the user's codebase. This works in both **interactive TUI mode** and **ACP server mode** (Zed editor).

Tool calling spans these layers:

```
siGit (agent loop + tool execution)
  → InferenceBackend (src/backend.rs — LocalBackend or OpenAiBackend/cloud)
    → onde ChatEngine (tool-aware API)   ── for LocalBackend
      → mistral.rs (model inference + tool call parsing)
    └ OpenAI-compatible HTTP endpoint    ── for OpenAiBackend (siGit Code Cloud)
```

The agent loop talks to an `InferenceBackend` trait object, not the engine
directly. `LocalBackend` wraps the on-device `ChatEngine`; `OpenAiBackend` calls
a remote OpenAI-compatible endpoint (the siGit Code Cloud tiers). Both implement
`send_message_with_tools` / `send_tool_results`, so the loop is identical.

---

## Model Requirement

Tool calling needs a model mistral.rs has a tool-call parser for. The supported
set is the **Qwen 3 family** (`<tool_call>...</tool_call>` XML) plus **Qwen 2.5
Coder 7B**. Plain Qwen 2.5 and the smaller Qwen 2.5 Coder variants do NOT support
tool calling. The authoritative list is `is_tool_calling()` in `src/models.rs`.

| Model | Constructor | Size | Tool calling |
|-------|-----------|------|:---:|
| Qwen 3 14B (Q4_K_M) | `GgufModelConfig::qwen3_14b()` | ~9 GB | ✅ |
| Qwen 3 8B (Q4_K_M) | `GgufModelConfig::qwen3_8b()` | ~5 GB | ✅ |
| Qwen 3 4B (Q4_K_M) | `GgufModelConfig::qwen3_4b()` | ~2.7 GB | ✅ |
| Qwen 3 1.7B (Q4_K_M) | `GgufModelConfig::qwen3_1_7b()` | ~1.3 GB | ✅ |
| Qwen 2.5 Coder 7B | `GgufModelConfig::qwen25_coder_7b()` | ~5 GB | ✅ |
| Qwen 2.5 Coder 3B | `GgufModelConfig::qwen25_coder_3b()` | ~1.93 GB | ❌ |
| Qwen 2.5 Coder 1.5B | `GgufModelConfig::qwen25_coder_1_5b()` | ~941 MB | ❌ |
| Qwen 2.5 3B / 1.5B | `qwen25_3b()` / `qwen25_1_5b()` | ~1.93 GB / ~941 MB | ❌ |

### Default model

There is **no hardcoded default model.** Startup uses the saved selection
(`setup::startup_model_selection`), then the first complete locally-cached model,
falling back to `GgufModelConfig::platform_default()` (Qwen 2.5 3B on macOS) when
nothing is cached. The TUI/ACP code in `main.rs` uses `qwen25_3b()` as that final
fallback. Users pick a tool-calling model via the `/models` picker.

### max_tokens

`max_tokens_for()` in `src/models.rs` gives tool-calling models **4096** tokens
and non-tool models **512** (tool models need headroom because `<think>` blocks
eat the budget). The TUI startup load in `run_interactive` overrides this to
**8192**. Don't assume a single value.

### Why prefer 8B+ over 4B for editing

4B struggles with `edit_file`: it reads a file, then fails to reproduce the exact
`old_text` it just saw, spiralling into retry rounds that burn `max_tokens` on
`<think>` blocks and return nothing. 8B (or larger) lands edits far more reliably.

### bartowski GGUF naming convention

bartowski's repos use the publisher name as a prefix with an underscore:

| Constant | Value |
|----------|-------|
| `BARTOWSKI_QWEN3_8B_GGUF` | `"bartowski/Qwen_Qwen3-8B-GGUF"` |
| `QWEN3_8B_GGUF_FILE` | `"Qwen_Qwen3-8B-Q4_K_M.gguf"` |
| `BARTOWSKI_QWEN3_4B_GGUF` | `"bartowski/Qwen_Qwen3-4B-GGUF"` |
| `QWEN3_4B_GGUF_FILE` | `"Qwen_Qwen3-4B-Q4_K_M.gguf"` |

These constants live in `onde/src/inference/models.rs`.

---

## Tools (9 total)

Defined in `sigit/src/tools.rs` via `all_tools()`:

| # | Tool | Parameters | Behavior |
|---|------|-----------|----------|
| 1 | `read_file` | `path` | Reads file contents, truncates at 10,000 chars |
| 2 | `create_directory` | `path` | Creates directory and all parents |
| 3 | `list_directory` | `path` | Lists entries with `[DIR]`/`[FILE]` prefix, dirs first |
| 4 | `search_files` | `pattern`, `path` (optional) | Recursive regex search, max 50 matches |
| 5 | `read_website` | `url` | Fetches HTTP/HTTPS, strips HTML, returns text |
| 6 | `create_file` | `path`, `content` | Creates new file (fails if exists) |
| 7 | `edit_file` | `path`, `old_text`, `new_text` | Find-and-replace (must match exactly once) |
| 8 | `delete_file` | `path` | Deletes file or empty directory |
| 9 | `run_command` | `command`, `cwd` (optional) | Shell command with 120s timeout |

### Async handling

`execute_tool()` is `async`. Most tools run synchronously, except:

- **`read_website`** — uses `tokio::task::spawn_blocking` because `reqwest::blocking::Client` panics inside a tokio runtime ("Cannot start a runtime from within a runtime")

### Tool gating by model

In TUI mode, `run_inference_task()` takes a `tools_enabled: bool` parameter. When the picker item's `tool_calling` (from `models::is_tool_calling`) is `false`, an empty tool list is passed so the model doesn't receive tool schemas it can't use.

In ACP mode, `handle_prompt` currently always passes the full tool set (`agent_tools_as_specs()`) regardless of the active model — there is no per-model gate on the ACP path.

---

## Architecture

### Layer 1: mistral.rs (model-level)

- `RequestBuilder::set_tools(Vec<Tool>)` — attach tool definitions
- `RequestBuilder::set_tool_choice(ToolChoice::Auto)` — let model decide
- `QwenParser` detects `<tool_call>...</tool_call>` tags in output
- Grammar-constrained decoding forces valid JSON inside tool calls
- `<think>...</think>` reasoning is separated from tool calls by the reasoning parser
- Works identically for GGUF and full-precision models

### Layer 2: onde (engine-level)

#### Key types (`onde/src/inference/types.rs`)

| Type | Purpose |
|------|---------|
| `ToolDefinition` | `{ name, description, parameters_schema: String }` |
| `ToolCallRequest` | `{ id, function_name, arguments: String }` |
| `ToolResult` | `{ tool_call_id, content: String }` |
| `ToolAwareResult` | `{ text, tool_calls: Vec<ToolCallRequest>, duration_secs, ... }` |

#### Key methods (`onde/src/inference/engine.rs`)

| Method | Purpose |
|--------|---------|
| `send_message_with_tools(msg, &[ToolDefinition])` | Returns `ToolAwareResult` with possible tool calls |
| `send_tool_results(Vec<ToolResult>, Option<&[ToolDefinition]>)` | Feed results back; `None` forces text response |

#### Layer 2.5: the `InferenceBackend` abstraction (`src/backend.rs`)

siGit doesn't call the engine directly from the agent loop — it goes through the
`InferenceBackend` trait so on-device and cloud inference share one code path:

| Item | Purpose |
|------|---------|
| `trait InferenceBackend` | `send_message_with_tools` / `send_tool_results` / `is_remote` |
| `LocalBackend` | wraps `Arc<ChatEngine>` — on-device inference |
| `OpenAiBackend` | OpenAI-compatible HTTP client — siGit Code Cloud tiers |
| `ToolSpec` | backend-level tool definition (`name`, `description`, `parameters_schema`) |
| `ToolCall` / `ToolResult` / `TurnResult` | backend-level request/result types |

`handle_prompt` snapshots `self.backend.lock().await.clone()` once per turn so a
mid-turn model/tier switch can't split the conversation across backends. When
`backend.is_remote()` it skips the local model load + readiness wait. Cloud tiers
(`fast`, `balanced`, `large`) come from `src/provider.rs` and are sign-in gated.

#### Internal details

- `attach_tools()` converts `ToolDefinition` → mistral.rs `Tool`, sets `ToolChoice::Auto` and `strict: Some(true)`
- `parse_tool_calls()` extracts tool calls from `choice.message.tool_calls`, generates fallback IDs if empty
- `replay_history_with_tools()` uses `.enumerate()` for correct sequential `index` values
- Malformed `parameters_schema` JSON logs a warning instead of silently producing empty params
- Malformed tool call `arguments` JSON logs a warning for debugging

### Layer 3: siGit (agent-level)

#### ACP session handling (`src/main.rs`)

All session handlers (`load_session`, `fork_session`, `new_session`) do:

1. **Store `args.cwd`** in `session_cwd: Mutex<Option<PathBuf>>`
2. **`std::env::set_current_dir(&args.cwd)`** — so relative paths in tool calls resolve correctly
3. **`engine.clear_history()`** — siGit doesn't persist sessions
4. **`engine.push_history(ChatMessage::system(...))`** — injects: *"The user's project working directory is {cwd}. Always use absolute paths..."*

Without step 4, the model uses the process `cwd` (often `$HOME`) and creates files in the wrong directory.

#### ACP content block handling (`prompt()`)

The `prompt()` handler processes all ACP content block types:

- **`ContentBlock::Text`** — passed through as-is
- **`ContentBlock::Resource` (EmbeddedResource)** — `TextResourceContents` inlined as `--- {uri} ---\n{text}\n--- end ---`
- **`ContentBlock::ResourceLink`** — `file://` URIs are read from disk. **Line range fragments** like `#L207:219` are parsed: the `#` fragment is stripped from the path, and only lines 207–219 are extracted and sent to the model

Example: Zed sends `@ index.html (207:219)` as:
```
ResourceLink(name="index.html (207:219)", uri="file:///path/to/index.html#L207:219")
```
siGit parses this into path `/path/to/index.html` + lines 207–219.

---

## The Agentic Loop

Both ACP mode (`SiGitAgent::handle_prompt()`) and TUI mode (`run_inference_task()`)
implement the same loop, driven through the active `InferenceBackend`:

```
1. backend.send_message_with_tools(user_text, &tools) → TurnResult
2. while result.tool_calls is non-empty AND round < MAX_TOOL_ROUNDS (10):
   a. For each tool_call:
      - Log: → tool_name(arguments)
      - Execute: tools::execute_tool(name, arguments).await
      - Log: ← N chars
      - Collect ToolResult { tool_call_id, content }
   b. Decide next_tools:
      - round < MAX_TOOL_ROUNDS → Some(&tools)  (allow more calls)
      - else → None  (force text response)
   c. backend.send_tool_results(results, next_tools) → TurnResult
3. Strip <think> blocks (chat::strip_think_blocks), send final text to user
   - Empty reply after tool rounds → log warning (ACP) or show error (TUI)
```

In ACP mode the final text is sent as one `AgentMessageChunk`; the tool-calling
loop is not streamed token-by-token.

---

## System Prompt

`main.rs` defines **two** prompts, picked by `system_prompt_for_model(tool_calling)`:

- **`SYSTEM_PROMPT`** (~120 lines) — the full agentic prompt for tool-calling models:
  - **Never tell the user to run commands** — use `run_command` tool instead
  - **Can access websites** — use `read_website` tool (overrides RLHF refusal training)
  - **Prefer absolute paths** in all tool arguments
  - **Git operations** — always use `run_command` with absolute cwd
  - **Always re-read a file before `edit_file`** — don't trust stale content
  - **smbCloud domain knowledge** — auth boundaries, deploy flows, project structure
- **`SIMPLE_SYSTEM_PROMPT`** — a short prompt for non-tool models; the full one
  wastes context and confuses them.

The session `cwd` is injected as a separate system message at session creation time (not part of the static prompt).

---

## Model Cache

Models are stored in the shared Onde App Group container on macOS:

```
~/Library/Group Containers/group.com.ondeinference.apps/models/hub/
```

`setup.rs` sets `HF_HOME` and `HF_HUB_CACHE` to point there at startup, so siGit reuses models downloaded by the Onde desktop app (and vice versa).

---

## Adding a New Tool

1. Add an `AgentTool` entry to `all_tools()` in `src/tools.rs`
2. Add a match arm to `execute_tool()` — use `spawn_blocking` if the implementation blocks
3. Write `exec_your_tool(arguments: &str) -> String`
4. Update `test_all_tools_count` test (currently expects 9)

No changes needed in onde or mistral.rs — tool definitions are passed dynamically.

---

## Adding a New Model

1. **`onde/src/inference/models.rs`** — add `pub const` for repo ID and GGUF filename, add to `SUPPORTED_MODELS` array and `SUPPORTED_MODEL_INFO`
2. **`onde/src/inference/engine.rs`** — add `pub fn model_name() -> Self` constructor to `impl GgufModelConfig`
3. **`sigit/src/models.rs`** — add a match arm to `model_id_to_config()` mapping the repo ID to the new constructor; if it supports tool calling, add the repo ID to `is_tool_calling()` (which also drives `max_tokens_for()`). The picker (`build_model_picker_items`) then surfaces it automatically.
4. **`sigit/src/main.rs`** — only if you're changing the fallback default (`qwen25_3b()`)

---

## Debugging

### Log locations

- **TUI mode:** `$TMPDIR/sigit.log` (e.g. `/var/folders/.../sigit.log`)
- **ACP mode (Zed):** `~/Library/Logs/Zed/Zed.log` — grep for `agent stderr:.*sigit`

### Key log patterns

```
# Model loaded successfully
ChatEngine: model Qwen 3 8B loaded in 6.9s

# Session cwd captured
load_session: id=..., cwd=/path/to/project, additional_directories=[...]

# Tool call parsed by mistral.rs
ChatEngine: tool inference END — 12.3s — tool_calls: 1

# Tool executed
→ read_file({"path":"/absolute/path/to/file.rs"})
← 6506 chars

# Tool result sent back
ChatEngine: tool results inference START — 1 results

# Model returned empty (exhausted max_tokens on thinking)
model returned empty reply after 7 tool round(s)

# ResourceLink received from Zed
block[1]: ResourceLink(name=index.html (207:219), uri=file:///path/to/index.html#L207:219)

# ResourceLink read failed (fragment not stripped — old bug, now fixed)
could not read ResourceLink file:///path/to/index.html#L207:219: No such file or directory
```

### Common issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| Model says "I cannot access websites" | RLHF refusal override not in system prompt | System prompt now has CRITICAL block about `read_website` |
| `0 tool call(s)` for every prompt | Wrong model loaded (Qwen 2.5) | Check log for `loading GGUF model` — must be Qwen 3 |
| `edit_file` returns `← 161 chars` repeatedly | `old_text not found` — model can't match exact text | Use Qwen 3 8B (not 4B); consider line-based edit tool |
| Files created in wrong directory | `cwd` not captured from ACP session | Session handlers must call `set_current_dir` + `push_history` with cwd |
| `@ file.html (207:219)` context missing | `#L207:219` fragment not stripped from file path | `prompt()` now parses URI fragments and extracts line ranges |
| `read_website` panics/hangs | `reqwest::blocking` inside tokio runtime | `exec_read_website` wrapped in `spawn_blocking` |
| Empty reply after many tool rounds | Model exhausted `max_tokens` on `<think>` blocks | Set `max_tokens: 8192`; 8B model wastes fewer tokens on thinking |

---

## Cargo Dependency Note

`onde` is published on crates.io; `sigit/Cargo.toml` pins it:

```toml
onde = "1.1.2"
```

The Qwen 3 / Coder-7B constructors (`qwen3_8b()`, etc.) ship in that release. For
local SDK development against an `onde` checkout, swap to a path dep
(`onde = { path = "../onde" }`) — but the committed form must stay the crates.io
version so CI/release builds resolve.

---

## File Map

| File | What it does |
|------|-------------|
| `sigit/src/tools.rs` | 9 tool schemas (`all_tools()`), `execute_tool()` dispatch, all `exec_*` implementations |
| `sigit/src/main.rs` | `SYSTEM_PROMPT`, `SiGitAgent` struct with `session_cwd` + `backend`, ACP handlers (cwd + push_history), `handle_prompt()` content-block parsing + tool loop, `MAX_TOOL_ROUNDS`, ACP builder wiring |
| `sigit/src/backend.rs` | `InferenceBackend` trait, `LocalBackend`, `OpenAiBackend`, `ToolSpec`/`ToolCall`/`ToolResult`/`TurnResult` |
| `sigit/src/models.rs` | `ModelPickerItem`, `model_id_to_config()`, `is_tool_calling()`, `max_tokens_for()`, `build_model_picker_items()` / `local_picker_items()` |
| `sigit/src/provider.rs` | `CLOUD_TIERS`, `cloud_tier_provider()`, cloud endpoint config |
| `sigit/src/chat.rs` | TUI app, model picker UI (uses `build_model_picker_items`), `run_inference_task()` with `tools_enabled` gate, TUI tool loop |
| `sigit/src/setup.rs` | HF cache setup (shared App Group container), `startup_model_selection()` |
| `onde/src/inference/types.rs` | `ToolDefinition`, `ToolCallRequest`, `ToolResult`, `ToolAwareResult` |
| `onde/src/inference/engine.rs` | `send_message_with_tools()`, `send_tool_results()`, `attach_tools()`, `parse_tool_calls()`, `replay_history_with_tools()`, `GgufModelConfig::qwen3_8b()` |
| `onde/src/inference/models.rs` | Model constants and `SUPPORTED_MODELS` array |
