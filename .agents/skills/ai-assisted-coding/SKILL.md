---
name: ai-assisted-coding
description: Build or maintain AI-assisted coding features in Rust using Onde Inference. Use when working on ChatEngine integration, model loading, streaming inference, history management, sampling config, or local coding-agent architecture.
---

# Skill: AI-Assisted Coding Agents — Onde Inference Integration

## Overview

Building a local AI coding agent in Rust using Onde Inference as the LLM backend.
Onde wraps mistral.rs with a clean API for model loading, history management, and
streaming inference across macOS (Metal), iOS, Android, Linux, and Windows.

Crate:  `onde = { path = "../onde" }` or from crates.io when published
Repo:   https://github.com/ondeinference/onde
Docs:   https://ondeinference.com

---

## Onde `ChatEngine` API

### Construction and lifecycle

```rust
use onde::inference::{ChatEngine, GgufModelConfig, SamplingConfig};

let engine = ChatEngine::new();        // starts unloaded
engine.is_loaded().await               // -> bool
engine.unload_model().await            // -> ()
```

### Loading a model

```rust
// Platform-aware default (Qwen 2.5 3B on macOS, 1.5B on iOS/tvOS/Android)
let config = GgufModelConfig::platform_default();

// Load — blocks until model is in memory and on GPU
engine
    .load_gguf_model(
        config,
        Some("You are a helpful assistant.".to_string()),  // system prompt
        None,  // sampling config (uses SamplingConfig::default() internally)
    )
    .await?;

// AlreadyLoaded error if called twice — check first:
if !engine.is_loaded().await {
    engine.load_gguf_model(...).await?;
}
```

**Model sizes (macOS/Windows/Linux default — Qwen 2.5 3B Q4_K_M):** ~1.93 GB  
**Model sizes (iOS/tvOS/Android default — Qwen 2.5 1.5B Q4_K_M):** ~941 MB  
First run downloads from HuggingFace Hub into `~/.cache/huggingface/`.

### Blocking (non-streaming) inference

```rust
let result = engine.send_message("What is Rust's ownership model?").await?;
// result: InferenceResult
println!("{}", result.text);
println!("took {}", result.duration_display);  // e.g. "3.2s"
```

`send_message` appends both the user message and assistant reply to conversation
history automatically.

### Streaming inference

```rust
let mut rx: tokio::sync::mpsc::Receiver<StreamChunk> =
    engine.stream_message("Tell me a story.").await?;

while let Some(chunk) = rx.recv().await {
    if !chunk.delta.is_empty() {
        print!("{}", chunk.delta);   // partial token text
    }
    if chunk.done {
        // chunk.finish_reason: Option<String> — e.g. "stop", "length"
        break;
    }
}
```

`StreamChunk` fields:
- `delta: String` — the new token(s) in this chunk
- `done: bool` — true on the last chunk
- `finish_reason: Option<String>` — present on final chunk only

History is updated automatically after the stream completes.

### One-shot generation (no history side-effects)

```rust
use onde::inference::ChatMessage;

let result = engine.generate(
    vec![ChatMessage::user("Expand: a cat in space")],
    Some(SamplingConfig::deterministic()),
).await?;
println!("{}", result.text);
// Does NOT modify conversation history
```

### History management

```rust
let history: Vec<ChatMessage> = engine.history().await;
let removed: usize = engine.clear_history().await;  // returns count cleared
engine.push_history(ChatMessage::user("context")).await;
engine.set_system_prompt("new system prompt").await;
engine.clear_system_prompt().await;
```

### Engine status

```rust
let info: EngineInfo = engine.info().await;
// info.status: EngineStatus (Unloaded | Loading | Ready | Generating | Error)
// info.model_name: Option<String>
// info.approx_memory: Option<String>  e.g. "~1.93 GB"
// info.history_length: u64
```

---

## `InferenceError` variants

```rust
match err {
    InferenceError::NoModelLoaded       => { /* load model first */ }
    InferenceError::AlreadyLoaded { model_name } => { /* already loaded */ }
    InferenceError::ModelBuild { reason } => { /* load failure */ }
    InferenceError::Inference { reason }  => { /* runtime inference error */ }
    InferenceError::Cancelled            => { /* was cancelled */ }
    InferenceError::Other { reason }     => { /* unexpected */ }
}
```

Map to ACP errors:
```rust
.map_err(|e| agent_client_protocol::Error::new(-32603, e.to_string()))?
```

---

## `SamplingConfig` presets

| Preset | temp | top_p | max_tokens | Use case |
|--------|------|-------|------------|----------|
| `SamplingConfig::default()` | 0.7 | 0.95 | 512 | General chat |
| `SamplingConfig::deterministic()` | 0.0 | — | 512 | Code / reproducible |
| `SamplingConfig::mobile()` | 0.7 | 0.95 | 128 | Memory-constrained |
| `SamplingConfig::coding()` | 0.0 | — | 512 | Code generation |
| `SamplingConfig::coding_mobile()` | 0.0 | — | 128 | Code on mobile |

---

## `GgufModelConfig` constructors

```rust
GgufModelConfig::platform_default()    // auto-selects based on target_os
GgufModelConfig::qwen25_1_5b()         // force 1.5B
GgufModelConfig::qwen25_3b()           // force 3B
GgufModelConfig::qwen25_coder_1_5b()   // coder variant 1.5B
GgufModelConfig::qwen25_coder_3b()     // coder variant 3B
```

---

## Adding onde as a Rust library dependency

```toml
# In your crate's Cargo.toml — onde is a path dep since it's not on crates.io yet
onde = { path = "../onde" }
```

**Important:** `onde` declares `crate-type = ["lib", "cdylib", "staticlib"]`.
When used as a Rust library dep, only the `lib` target is compiled. The
`cdylib`/`staticlib` targets (used for Swift/Kotlin FFI) are not built. The
`uniffi::setup_scaffolding!()` macro generates `#[no_mangle] extern "C"` symbols
but these are harmless in a binary context.

**The `[patch.crates-io]` in onde's Cargo.toml does NOT propagate** to dependents
unless they are in the same workspace. The `sysctl` patch is only needed for
watchOS; macOS/iOS/Linux work without it.

**GPU feature selection is automatic** via `target_os` cfg flags in onde's
Cargo.toml — you get Metal on macOS/iOS without any extra features in your crate.

---

## Patterns for coding agents

### Single-engine, multi-session via history reset

For a simple MVP where one session is active at a time:

```rust
struct MyAgent {
    engine: Arc<ChatEngine>,
    active_session: Arc<Mutex<Option<SessionId>>>,
}

// new_session handler:
if self.engine.is_loaded().await {
    self.engine.clear_history().await;   // reuse model, fresh conversation
} else {
    self.engine
        .load_gguf_model(GgufModelConfig::platform_default(), Some(SYSTEM_PROMPT.into()), None)
        .await?;
}
```

**Why:** Loading the model is expensive (seconds + GB of RAM). Reloading for each
session would make the agent feel broken. `clear_history()` resets context in
microseconds.

### Per-session engines (multiple concurrent sessions)

When you need truly isolated parallel sessions:

```rust
use std::collections::HashMap;

struct MultiSessionAgent {
    sessions: Arc<Mutex<HashMap<String, Arc<ChatEngine>>>>,
}

// new_session: create and load a new engine per session
// prompt: look up session engine, call send_message or stream_message
// CAVEAT: each engine holds a separate model copy in GPU memory — expensive!
```

Better approach for shared GPU memory: use `engine.generate()` (no history
side-effects) with an explicitly managed message vec per session.

### System prompt design for coding agents

```rust
const SYSTEM_PROMPT: &str = "\
You are <AgentName>, an expert AI coding agent integrated into your editor \
via the Agent Client Protocol. You specialize in:

- Code analysis, writing, and refactoring
- Bug hunting and debugging
- Git workflows and commit messages
- Software architecture and design patterns
- Code review and best practices

Be concise, precise, and practical. Write clean, idiomatic code with brief \
explanations. Identify root causes when debugging. Prefer correctness over brevity.";
```

Key principles:
- State the agent's role and name clearly (models respond better to named personas)
- List specializations explicitly (influences which parts of training are activated)
- Set tone expectations: "concise", "practical", "idiomatic"
- Avoid verbose instruction lists — they cost tokens on every turn

### Streaming tokens to ACP (connecting onde → ACP)

```rust
// In Agent::prompt():
let mut rx = self.engine.stream_message(user_text).await
    .map_err(|e| Error::new(-32603, e.to_string()))?;

while let Some(chunk) = rx.recv().await {
    if !chunk.delta.is_empty() {
        self.notification_tx.send(
            SessionNotification::new(
                session_id.clone(),
                SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(ContentBlock::from(chunk.delta)),
                ),
            )
        ).await.ok();  // .ok() — ignore if forwarder is gone
    }
    if chunk.done { break; }
}

Ok(PromptResponse::new(StopReason::EndTurn))
```

The `PromptResponse` is returned AFTER the stream finishes. The client receives
streaming tokens via `session/update` notifications while blocking on the
`session/prompt` response.

---

## Extracting text from ACP `PromptRequest`

ACP prompts can contain text, images, resource links, etc. For a text-only
coding agent:

```rust
let user_text: String = args.prompt.iter()
    .filter_map(|block| match block {
        ContentBlock::Text(t) => Some(t.text.as_str()),
        // Skip images, resource links, embedded resources for now
        _ => None,
    })
    .collect::<Vec<_>>()
    .join("\n");
```

For future resource context (e.g. open files provided by Zed):
```rust
ContentBlock::Resource(r) => match &r.resource {
    EmbeddedResourceResource::Text(t) => Some(t.text.as_str()),
    _ => None,
},
```

---

## `ChatEngine` threading model

- Internally uses `Arc<tokio::sync::Mutex<Option<LoadedModel>>>` — `Send + Sync`.
- Safe to wrap in `Arc<ChatEngine>` and share across tasks.
- `stream_message()` spawns a `tokio::spawn` background task internally — the
  mistralrs model must be `Send`, which it is on all supported platforms.
- Calling `stream_message()` from a `!Send` future (e.g. inside a `LocalSet`) is
  fine — the future itself doesn't hold a `!Send` value across `.await`.

---

## First-run model download

On first use, onde downloads the GGUF model from HuggingFace Hub:
- Requires internet connectivity
- Cached at `~/.cache/huggingface/` (or `HF_HUB_CACHE` env var)
- `HF_TOKEN` env var needed for gated models (public Qwen models don't need it)
- Subsequent runs load from disk cache — fast

For sandboxed environments (iOS, tvOS, Android):
- Set `HF_HOME` and `HF_HUB_CACHE` to a path inside the app container
- Do this BEFORE calling any ChatEngine method
- See `onde/docs/swift-package.md` for `setupInferenceEnvironment()` pattern

---

## Common mistakes

1. **Calling `load_gguf_model` twice** without checking `is_loaded()` first →
   `InferenceError::AlreadyLoaded`. Always guard with `is_loaded().await`.

2. **Blocking on the stream after the channel is closed** → the stream naturally
   ends when the `done` flag is true. Don't `recv()` after `done`.

3. **Losing `StreamChunk` deltas** when `delta` is empty (whitespace tokens) →
   always check `!chunk.delta.is_empty()` before sending to avoid empty
   notifications that waste bandwidth.

4. **Sharing one `ChatEngine` across parallel prompts** without coordination →
   the internal Mutex serializes inference, so concurrent prompts queue up.
   Design for sequential access per engine instance.

5. **Using `SamplingConfig::default()` for code generation** → prefer
   `SamplingConfig::coding()` (deterministic, temp=0) for more reliable code output.

6. **Forgetting that `generate()` doesn't update history** — use it for
   one-shot enhancements (prompt expansion, code review) that shouldn't pollute
   the main conversation. Use `send_message()` / `stream_message()` for the
   primary turn loop.
