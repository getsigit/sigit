# Skill: Tool Calling in siGit Code

## Overview

siGit Code supports **agentic tool calling** — the LLM can invoke tools (read files, list directories, search code) to ground its answers in the actual codebase. This works in both **interactive TUI mode** and **ACP server mode** (Zed editor).

Tool calling spans three layers:

```
siGit (agent loop + tool execution)
  → onde (ChatEngine with tool-aware API)
    → mistral.rs (model inference + tool call parsing)
```

---

## Model Requirement

**Only Qwen 3 supports tool calling.** Qwen 2.5 does NOT — mistral.rs only has a parser for Qwen 3's `<tool_call>...</tool_call>` XML format.

| Model | Constructor | Size | Tool calling |
|-------|-----------|------|:---:|
| Qwen 3 4B (Q4_K_M) | `GgufModelConfig::qwen3_4b()` | ~2.6 GB | ✅ |
| Qwen 3 1.7B (Q4_K_M) | `GgufModelConfig::qwen3_1_7b()` | ~1.2 GB | ✅ |
| Qwen 2.5 Coder 3B | `GgufModelConfig::qwen25_coder_3b()` | ~1.9 GB | ❌ |
| Qwen 2.5 1.5B | `GgufModelConfig::qwen25_1_5b()` | ~0.9 GB | ❌ |

siGit uses **Qwen 3 4B** by default (set in `main.rs` via `GgufModelConfig::qwen3_4b()`).

### bartowski GGUF naming convention

bartowski's repos use the publisher name as a prefix with an underscore:

| Constant | Value |
|----------|-------|
| `BARTOWSKI_QWEN3_4B_GGUF` | `"bartowski/Qwen_Qwen3-4B-GGUF"` |
| `QWEN3_4B_GGUF_FILE` | `"Qwen_Qwen3-4B-Q4_K_M.gguf"` |
| `BARTOWSKI_QWEN3_1_7B_GGUF` | `"bartowski/Qwen_Qwen3-1.7B-GGUF"` |
| `QWEN3_1_7B_GGUF_FILE` | `"Qwen_Qwen3-1.7B-Q4_K_M.gguf"` |

These constants live in `onde/src/inference/models.rs`.

---

## Architecture

### Layer 1: mistral.rs (model-level)

mistral.rs handles the low-level tool calling protocol:

- **`RequestBuilder::set_tools(Vec<Tool>)`** — attach tool definitions (JSON Schema) to a request
- **`RequestBuilder::set_tool_choice(ToolChoice::Auto)`** — let the model decide whether to call tools
- **`RequestBuilder::add_message_with_tool_call(role, content, tool_calls)`** — replay an assistant message that contained tool calls
- **`RequestBuilder::add_tool_message(content, tool_call_id)`** — send a tool execution result back

Key types (all re-exported from `mistralrs` crate, accessible via `onde::mistralrs::*`):

| Type | Purpose |
|------|---------|
| `Tool` | A tool definition: `{ tp: ToolType::Function, function: Function }` |
| `Function` | Name, description, JSON Schema parameters, `strict` flag |
| `ToolChoice` | `None`, `Auto`, or `Tool(...)` |
| `ToolCallResponse` | Model's tool call: `{ id, function: CalledFunction }` |
| `CalledFunction` | `{ name, arguments }` where arguments is a JSON string |
| `ToolCallType` | Currently only `Function` |

### Layer 2: onde (engine-level)

onde's `ChatEngine` wraps mistral.rs with conversation history management. The tool calling API is **Rust-only** (no UniFFI annotations — Swift/Kotlin bindings are not affected).

#### Types (`onde/src/inference/types.rs`)

| Type | Purpose |
|------|---------|
| `ToolDefinition` | `{ name, description, parameters_schema: String }` — tool schema for the model |
| `ToolCallRequest` | `{ id, function_name, arguments: String }` — parsed tool call from model response |
| `ToolResult` | `{ tool_call_id, content: String }` — execution result to feed back |
| `ToolAwareResult` | `{ text, tool_calls: Vec<ToolCallRequest>, duration_secs, ... }` — inference result that may contain tool calls |

#### Methods (`onde/src/inference/engine.rs`)

| Method | Purpose |
|--------|---------|
| `send_message_with_tools(&self, msg, &[ToolDefinition])` | Send user message with tools available. Returns `ToolAwareResult`. If `tool_calls` is non-empty, the model wants to call tools. |
| `send_tool_results(&self, Vec<ToolResult>, Option<&[ToolDefinition]>)` | Feed tool execution results back. Pass tools to allow further rounds, or `None` to force a text response. |
| `stream_tool_results(&self, Vec<ToolResult>, Option<Vec<ToolDefinition>>)` | Streaming variant for the final text response after tool rounds. |

#### Internal history

`LoadedModel.history` uses `Vec<HistoryEntry>` (not `Vec<ChatMessage>`) to support tool-related messages:

```rust
enum HistoryEntry {
    Text(ChatMessage),                          // regular user/assistant/system
    AssistantToolCall { content, tool_calls },   // assistant response with tool calls
    ToolResult { tool_call_id, content },        // tool execution result
}
```

The existing `history()` public method converts back to `Vec<ChatMessage>` for backward compatibility. All existing methods (`send_message`, `stream_message`, etc.) work unchanged — they use `HistoryEntry::Text` internally.

When replaying history in requests:
- `build_request()` (no tools) — `AssistantToolCall` replays as plain assistant text, `ToolResult` is skipped
- `build_request_with_tools()` — uses `add_message_with_tool_call()` and `add_tool_message()` for full fidelity

### Layer 3: siGit (agent-level)

#### Tool definitions (`sigit/src/tools.rs`)

Three coding tools with JSON Schema definitions and execution functions:

| Tool | Parameters | Behavior |
|------|-----------|----------|
| `read_file` | `path` (required) | Reads file contents, truncates at 10,000 chars |
| `list_directory` | `path` (required) | Lists entries with `[DIR]`/`[FILE]` prefix, dirs first, sorted |
| `search_files` | `pattern` (required), `path` (optional) | Recursive regex search, max 50 matches, skips hidden dirs |

Public API:
- `all_tools() -> Vec<AgentTool>` — returns tool schemas (name, description, JSON Schema)
- `execute_tool(name: &str, arguments: &str) -> String` — dispatches by name, returns result string

#### Conversion to onde types

In both `main.rs` and `chat.rs`, `AgentTool` is converted to `ToolDefinition`:

```rust
let onde_tools: Vec<ToolDefinition> = tools::all_tools()
    .into_iter()
    .map(|t| ToolDefinition {
        name: t.name.to_string(),
        description: t.description.to_string(),
        parameters_schema: t.parameters_schema.to_string(),
    })
    .collect();
```

---

## The Agentic Loop

Both ACP mode (`main.rs` → `SiGitAgent::prompt()`) and TUI mode (`chat.rs` → event loop) implement the same pattern:

```
1. engine.send_message_with_tools(user_text, &tools)  → ToolAwareResult
2. while result.tool_calls is non-empty AND round < MAX_TOOL_ROUNDS (10):
   a. For each tool_call:
      - Show status to user (🔧 tool_name)
      - Execute: tools::execute_tool(name, arguments)
      - Collect ToolResult { tool_call_id, content }
   b. Decide next_tools:
      - If round < MAX_TOOL_ROUNDS → Some(&tools)  (allow more calls)
      - Else → None  (force text response)
   c. engine.send_tool_results(results, next_tools) → ToolAwareResult
3. Send final result.text to user
```

### ACP mode specifics (`main.rs`)

- Tool status is sent as `SessionUpdate::AgentMessageChunk` with a 🔧 prefix
- Final text is sent as a single `AgentMessageChunk`
- Returns `StopReason::EndTurn`

### TUI mode specifics (`chat.rs`)

- Tool status shown as `ChatMessage::system("🔧 tool_name")`
- Forces a `terminal.draw()` after each tool call for visual feedback
- Final text added as `ChatMessage::assistant(result.text)`
- The tool loop is **blocking** (non-streaming) — the TUI doesn't accept input during tool execution

---

## System Prompt

The system prompt in `main.rs` (`SYSTEM_PROMPT`) includes tool-awareness instructions:

```
You have access to tools that let you read files, list directories, and search
code. Use them proactively to understand the codebase before answering questions
or writing code. Always ground your answers in the actual code.
```

This is critical — without it, the model may not use the tools even when they're available.

---

## Adding a New Tool

1. **Define the schema** in `sigit/src/tools.rs`:
   - Add an `AgentTool` entry to `all_tools()` with name, description, and JSON Schema
   - The `parameters_schema` must be a valid JSON Schema object with `type`, `properties`, and `required`

2. **Implement execution** in `sigit/src/tools.rs`:
   - Add a case to `execute_tool()` match
   - Write `exec_your_tool(arguments: &str) -> String`
   - Parse arguments with `serde_json::from_str::<Value>(arguments)`
   - Return results as a string, handle errors gracefully (never panic)

3. **No changes needed** in onde or mistral.rs — the tool definitions are passed dynamically via `send_message_with_tools()`.

### Example: adding a `write_file` tool

```rust
// In all_tools():
AgentTool {
    name: "write_file",
    description: "Create or overwrite a file with the given content.",
    parameters_schema: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "File path to write" },
            "content": { "type": "string", "description": "File content" }
        },
        "required": ["path", "content"]
    }),
}

// In execute_tool():
"write_file" => exec_write_file(arguments),

// Implementation:
fn exec_write_file(arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or_default();
    let path = args["path"].as_str().unwrap_or("");
    let content = args["content"].as_str().unwrap_or("");
    match std::fs::write(path, content) {
        Ok(()) => format!("Successfully wrote {} bytes to {}", content.len(), path),
        Err(e) => format!("Error writing {}: {}", path, e),
    }
}
```

---

## Dependencies

### onde (`Cargo.toml`)

- `serde_json = "1.0"` — parsing `parameters_schema` JSON strings into `HashMap<String, Value>` for mistral.rs `Function.parameters`

### siGit (`Cargo.toml`)

- `onde = { path = "../onde" }` — local path dep (required during development for the tool calling API)
- `serde_json = "1"` — parsing tool call arguments
- `regex = "1"` — used by the `search_files` tool

---

## Debugging Tool Calling

### Model doesn't call tools

- Verify the model is Qwen 3 (check `GgufModelConfig::qwen3_4b()` in both `main.rs` load sites)
- Check the system prompt includes tool-awareness instructions
- Check logs: `ChatEngine: tool inference END — tool_calls: 0` means the model chose not to use tools
- Try a more explicit prompt: *"Use the read_file tool to read src/main.rs"*

### Tool calls fail / wrong arguments

- Check `ToolDefinition.parameters_schema` is valid JSON Schema
- Check `strict: Some(true)` is set in onde's `attach_tools()` (it is by default) — this enables constrained decoding
- Check logs for the raw arguments: `→ read_file({"path":"..."})`

### History replay issues

- Tool call history is replayed via `replay_history_with_tools()` which uses `add_message_with_tool_call()` and `add_tool_message()`
- If history gets corrupted, `/clear` in the TUI or `engine.clear_history()` resets it
- The non-tool `build_request()` gracefully degrades: `AssistantToolCall` replays as plain text, `ToolResult` entries are skipped

---

## File Map

| File | What it does |
|------|-------------|
| `onde/src/inference/types.rs` | `ToolDefinition`, `ToolCallRequest`, `ToolResult`, `ToolAwareResult` types |
| `onde/src/inference/engine.rs` | `HistoryEntry` enum, `send_message_with_tools()`, `send_tool_results()`, `stream_tool_results()`, helper functions (`build_request_with_tools`, `replay_history_with_tools`, `attach_tools`, `parse_tool_calls`) |
| `onde/src/inference/models.rs` | Qwen 3 model constants (`BARTOWSKI_QWEN3_4B_GGUF`, `QWEN3_4B_GGUF_FILE`, etc.) and `GgufModelConfig::qwen3_4b()` / `qwen3_1_7b()` constructors |
| `onde/src/inference/mod.rs` | Re-exports `ToolAwareResult`, `ToolCallRequest`, `ToolDefinition`, `ToolResult` |
| `sigit/src/tools.rs` | `AgentTool` struct, `all_tools()`, `execute_tool()`, implementations for `read_file`, `list_directory`, `search_files` |
| `sigit/src/main.rs` | `agent_tools_as_onde()` converter, agentic loop in `SiGitAgent::prompt()`, `MAX_TOOL_ROUNDS` constant, Qwen 3 4B model selection |
| `sigit/src/chat.rs` | TUI agentic loop (same pattern as ACP), tool status display as system messages |
| `mistral.rs/docs/TOOL_CALLING.md` | Upstream docs for supported models and API |
| `mistral.rs/mistralrs/examples/advanced/tools/main.rs` | Reference example for mistral.rs tool calling |