---
name: tool-calling
description: Implement or debug tool calling in siGit Code across the app, Onde Inference, and mistral.rs. Use when working on tool schemas, execution loops, model support, session cwd handling, or tool-call troubleshooting.
---

# Tool Calling

## Overview

siGit Code supports **agentic tool calling** — the LLM invokes tools (read/write files, run commands, read websites) to operate on the user's codebase. This works in both **interactive TUI mode** and **ACP server mode** (Zed editor).

Tool calling spans three layers:

```
siGit (agent loop + tool execution)
  → onde (ChatEngine with tool-aware API)
    → mistral.rs (model inference + tool call parsing)
```

---

## Model Requirement

**Only Qwen 3 supports tool calling.** Qwen 2.5 does NOT — mistral.rs only has a parser for Qwen 3's `<tool_call>...</tool_call>` XML format.

| Model | Constructor | Size | Tool calling | Default |
|-------|-----------|------|:---:|:---:|
| Qwen 3 8B (Q4_K_M) | `GgufModelConfig::qwen3_8b()` | ~5 GB | ✅ | ✅ **default** |
| Qwen 3 4B (Q4_K_M) | `GgufModelConfig::qwen3_4b()` | ~2.7 GB | ✅ | |
| Qwen 3 1.7B (Q4_K_M) | `GgufModelConfig::qwen3_1_7b()` | ~1.3 GB | ✅ | |
| Qwen 2.5 Coder 3B | `GgufModelConfig::qwen25_coder_3b()` | ~1.93 GB | ❌ | |
| Qwen 2.5 Coder 1.5B | `GgufModelConfig::qwen25_coder_1_5b()` | ~941 MB | ❌ | |

siGit uses **Qwen 3 8B** by default with `max_tokens: 8192` (set in `main.rs` for both TUI and ACP modes).

### Why 8B over 4B

4B can't do `edit_file` reliably. It reads a file, then fails to reproduce the exact `old_text` it just saw. This spirals into 7+ retry rounds that burn through `max_tokens` on `<think>` blocks and return nothing. 8B is the smallest model that actually lands edits.

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

In TUI mode, `run_inference_task()` takes a `tools_enabled: bool` parameter. When the model's `ModelOption.tool_calling` is `false` (Qwen 2.5), an empty tool list is passed so the model doesn't receive tool schemas it can't use.

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

Both ACP mode (`SiGitAgent::prompt()`) and TUI mode (`run_inference_task()`) implement:

```
1. engine.send_message_with_tools(user_text, &tools) → ToolAwareResult
2. while result.tool_calls is non-empty AND round < MAX_TOOL_ROUNDS (10):
   a. For each tool_call:
      - Log: → tool_name(arguments)
      - Execute: tools::execute_tool(name, arguments).await
      - Log: ← N chars
      - Collect ToolResult { tool_call_id, content }
   b. Decide next_tools:
      - round < MAX_TOOL_ROUNDS → Some(&tools)  (allow more calls)
      - else → None  (force text response)
   c. engine.send_tool_results(results, next_tools) → ToolAwareResult
3. Send final result.text to user
   - Empty reply after tool rounds → log warning (ACP) or show error (TUI)
```

---

## System Prompt

The `SYSTEM_PROMPT` in `main.rs` (~122 lines) includes critical instructions:

- **Never tell the user to run commands** — use `run_command` tool instead
- **Can access websites** — use `read_website` tool (overrides RLHF refusal training)
- **Prefer absolute paths** in all tool arguments
- **Git operations** — always use `run_command` with absolute cwd
- **smbCloud domain knowledge** — auth boundaries, deploy flows, project structure

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
3. **`sigit/src/chat.rs`** — add `ModelOption` entry to `SIGIT_MODELS` with `tool_calling: true/false`
4. **`sigit/src/main.rs`** — update `run_interactive()` and `run_acp_server()` if changing the default

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

For local development, `sigit/Cargo.toml` must use the path dependency:

```toml
onde = { path = "../onde" }
```

For CI/release, switch to the git dependency (after pushing Onde changes):

```toml
onde = { git = "https://github.com/ondeinference/onde", branch = "development" }
```

The `qwen3_8b()` constructor only exists in the local Onde SDK until it's pushed to the `development` branch.

---

## File Map

| File | What it does |
|------|-------------|
| `sigit/src/tools.rs` | 9 tool schemas (`all_tools()`), `execute_tool()` dispatch, all `exec_*` implementations |
| `sigit/src/main.rs` | `SYSTEM_PROMPT`, `SiGitAgent` struct with `session_cwd`, ACP session handlers (cwd + push_history), `prompt()` with content block parsing, model selection (`qwen3_8b`), `MAX_TOOL_ROUNDS` |
| `sigit/src/chat.rs` | `SIGIT_MODELS` array (4 models), `run_inference_task()` with `tools_enabled` gate, TUI tool loop |
| `sigit/src/setup.rs` | HF cache setup pointing to shared App Group container |
| `onde/src/inference/types.rs` | `ToolDefinition`, `ToolCallRequest`, `ToolResult`, `ToolAwareResult` |
| `onde/src/inference/engine.rs` | `send_message_with_tools()`, `send_tool_results()`, `attach_tools()`, `parse_tool_calls()`, `replay_history_with_tools()`, `GgufModelConfig::qwen3_8b()` |
| `onde/src/inference/models.rs` | Model constants and `SUPPORTED_MODELS` array |
