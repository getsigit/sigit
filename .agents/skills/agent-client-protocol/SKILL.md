---
name: agent-client-protocol
description: Implement or debug Agent Client Protocol (ACP) support in Rust for siGit Code. Use when working on ACP JSON-RPC over stdio, the agent-client-protocol crate, session/prompt/fork handlers, config options (model picker), slash commands, streaming notifications, or editor integration.
---

# Skill: Agent Client Protocol (ACP) — Rust Implementation

## Overview

ACP is a JSON-RPC 2.0 protocol over **stdio** for integrating AI coding agents
with editors (Zed, JetBrains, Neovim, etc.). The agent runs as a subprocess;
the editor is the client. Communication is newline-delimited JSON on stdin/stdout.

Crate: `agent-client-protocol = "0.13"` (siGit pins 0.13.0 in `Cargo.lock`)
Docs:  https://docs.rs/agent-client-protocol
Spec:  https://agentclientprotocol.com

> **Big change since 0.10:** the crate moved from a `#[async_trait(?Send)] impl Agent`
> model to a **builder** model. You no longer implement a trait. You build an
> `Agent` with per-message handler closures and `.connect_to(transport)`. Each
> handler receives a `ConnectionTo<Client>` (`cx`) you use to send notifications
> and spawn tasks — so the old mpsc "circular dependency" pattern is gone.

siGit's entire ACP server lives in `src/main.rs` (`run_acp_server`, the
`SiGitAgent` struct, and its `handle_*` methods). Read it alongside this skill.

---

## Dependency setup

```toml
[dependencies]
agent-client-protocol = { version = "0.13", features = [
    "unstable_session_fork",                   # session/fork support
    "unstable_session_additional_directories", # additional_directories on session requests
    "unstable_auth_methods",                   # AuthMethod::Agent etc.
] }
async-trait = "0.1"
tokio       = { version = "1", features = ["rt", "rt-multi-thread", "macros", "io-std", "io-util", "sync", "time"] }
tokio-util  = { version = "0.7", features = ["compat"] }
futures     = "0.3"
uuid        = { version = "1", features = ["v4"] }
```

The `unstable_*` features gate real types/methods (`ForkSessionRequest`,
`additional_directories`, `AuthMethod::Agent`). Without them the corresponding
APIs don't exist and you'll get "no variant/method" errors.

---

## Imports

Protocol message/data types live under `agent_client_protocol::schema::*`.
Connection/runtime types live at the crate root.

```rust
use agent_client_protocol::schema::{
    AgentCapabilities, AuthMethod, AuthMethodAgent, AuthenticateRequest, AuthenticateResponse,
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, CancelNotification,
    ConfigOptionUpdate, ContentBlock, ContentChunk, EmbeddedResourceResource, ForkSessionRequest,
    ForkSessionResponse, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, Meta, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ProtocolVersion, SessionCapabilities, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigValueId,
    SessionForkCapabilities, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UnstructuredCommandInput,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Responder};
```

---

## Wiring up the server — the builder

You do **not** implement a trait. You hold your state in an `Arc<MyState>`, then
register one closure per incoming message type on `Agent.builder()`, and finish
with `.connect_to(transport).await`. The builder owns the JSON-RPC loop and runs
until the client disconnects.

```rust
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

async fn run_acp_server() -> anyhow::Result<()> {
    let state = Arc::new(SiGitAgent::new(/* … */));

    // Adapt tokio stdio to the futures AsyncRead/AsyncWrite the SDK expects.
    let stdin  = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();
    let transport = ByteStreams::new(stdout, stdin);   // note: (writer, reader)

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
                async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    handle_response(responder, state.handle_prompt(&cx, req).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // … one .on_receive_request(…) per request type you support …
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

    Ok(())
}
```

Key points:

- **`Arc::clone(&state)` per closure.** Each handler closure is `move` and owns
  its own `Arc` clone of shared state.
- **The macro is required.** Each handler is paired with
  `agent_client_protocol::on_receive_request!()` (or `on_receive_notification!()`).
  It wires the closure's concrete message type into the dispatcher. Don't omit it.
- **Closure signature for requests:** `async move |req: T, responder, cx: ConnectionTo<Client>|`.
  Use `_cx` when a handler doesn't send notifications (e.g. `initialize`, `authenticate`).
- **Closure signature for notifications:** `async move |notif: T, cx: ConnectionTo<Client>|`
  returning `agent_client_protocol::Result<()>` — no responder (notifications get no reply).
- **Unmatched messages fall to the SDK default** — you only register what you support.

### The `Responder` + `handle_response` helper

Requests reply through a `Responder<T>`. siGit funnels every handler's
`Result` through one helper:

```rust
fn handle_response<T: agent_client_protocol::JsonRpcResponse>(
    responder: Responder<T>,
    result: agent_client_protocol::Result<T>,
) -> agent_client_protocol::Result<()> {
    match result {
        Ok(resp) => responder.respond(resp),
        Err(err) => responder.respond_with_error(err),
    }
}
```

So each `handle_*` method just returns `Result<SomeResponse>` and stays free of
protocol plumbing.

---

## `ConnectionTo<Client>` — the `cx`

The per-handler `cx: ConnectionTo<Client>` replaces the old mpsc-channel forwarder.
It is `Clone`. Two things you do with it:

```rust
// 1. Send a server→client notification (streaming chunks, tool-call updates, …)
cx.send_notification(SessionNotification::new(session_id.clone(), update))?;

// 2. Spawn a background task that keeps using cx (e.g. a progress spinner poller).
let cx_for_poller = cx.clone();
cx.spawn(async move {
    loop {
        // … cx_for_poller.send_notification(progress_update) …
        # break;
    }
    Ok(())
}).ok();
```

Because `cx` is handed to you directly, there is **no circular dependency** between
the connection and the agent anymore. Don't reintroduce the mpsc forwarder pattern.

---

## The handlers siGit implements

| Message | Method | Notes |
|---------|--------|-------|
| `InitializeRequest` | `handle_initialize` | capabilities, auth methods, agent info, `meta` |
| `AuthenticateRequest` | `handle_authenticate` | verifies stored siGit Code Cloud session |
| `NewSessionRequest` | `handle_new_session` | sets cwd, resets history, advertises commands + config options |
| `LoadSessionRequest` | `handle_load_session` | like new_session; gated by `load_session(true)` capability |
| `ForkSessionRequest` | `handle_fork_session` | gated by `unstable_session_fork` + `SessionForkCapabilities` |
| `PromptRequest` | `handle_prompt` | the turn: parse blocks → slash commands or tool-calling loop |
| `SetSessionConfigOptionRequest` | `handle_set_session_config_option` | the Zed model picker — switches/downloads models |
| `CancelNotification` | `handle_cancel` | notification, no response |

Everything else is left to the SDK default (method not found).

---

## Types and their builders

All `#[non_exhaustive]` structs require builder methods — struct-literal syntax
won't compile.

### `InitializeResponse`

```rust
Ok(InitializeResponse::new(ProtocolVersion::V1)        // use V1, not args.protocol_version
    .agent_info(
        Implementation::new("sigit", env!("CARGO_PKG_VERSION"))
            .title("siGit Code - AI Coding Agent"),
    )
    .auth_methods(vec![AuthMethod::Agent(
        AuthMethodAgent::new("sigit", "Sign in to siGit Code")
            .description("Sign in with `/login <email> <password>` in the message box."),
    )])
    .agent_capabilities(
        AgentCapabilities::default()
            .load_session(true)                        // enables LoadSessionRequest
            .session_capabilities(
                SessionCapabilities::new()
                    .fork(SessionForkCapabilities::new()),  // enables ForkSessionRequest
            ),
    )
    .meta(initialize_meta()))                          // free-form Meta (see below)
```

`auth_methods` must include at least one `AuthMethod::Agent` or **Zed hangs on
"Loading…" forever.** siGit uses `Agent` (not `Terminal`) because Zed advertises
terminal-auth for custom agents but never actually spawns the login terminal, so
the button would be a silent no-op. With `Agent`, clicking calls `authenticate`.

### `Meta` — free-form server metadata

`Meta` is a string-keyed JSON map you can attach to `InitializeResponse` (siGit
publishes the active model there so the editor can show it):

```rust
let mut meta = Meta::new();
meta.insert("sigit".to_string(), serde_json::json!({
    "active_model": { "display_name": "...", "model_id": "...", "gguf_file": "..." }
}));
```

### `AuthenticateResponse`

```rust
Ok(AuthenticateResponse::default())                    // success
// failure: return an Error — siGit uses -32000 "not signed in …"
```

### `NewSessionResponse` / `LoadSessionResponse` / `ForkSessionResponse`

```rust
let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());

Ok(NewSessionResponse::new(session_id).config_options(config_options))
Ok(LoadSessionResponse::new().config_options(config_options))     // no id arg — it's in the request
Ok(ForkSessionResponse::new(new_id).config_options(config_options))
```

`SessionId` is a newtype with `Clone`, `PartialEq`, `Display`, `Into<String>`,
`AsRef<str>`. Store it as-is so `==` works. `config_options` powers the editor's
per-session picker (see Config options below).

The session requests carry `cwd: PathBuf` and (with the feature)
`additional_directories: Vec<PathBuf>`. siGit stashes `cwd`, `set_current_dir`s
to it, and pushes a system message telling the model to use absolute paths under it.

### `PromptRequest` / blocks

```rust
args.session_id   // SessionId
args.prompt       // Vec<ContentBlock>
```

Editors send several block kinds — handle the three siGit cares about:

```rust
for block in &args.prompt {
    match block {
        ContentBlock::Text(t) => { /* t.text */ }
        ContentBlock::Resource(embedded) => match &embedded.resource {
            // editor already inlined file content
            EmbeddedResourceResource::TextResourceContents(tr) => { /* tr.uri, tr.text */ }
            EmbeddedResourceResource::BlobResourceContents(b)   => { /* b.uri */ }
            _ => {}
        },
        ContentBlock::ResourceLink(link) => {
            // a reference (e.g. `@file`); read it yourself.
            // link.uri is "file:///abs/path#L207:219" (or #L207-219). Strip "file://",
            // split the "#L<start>:<end>" fragment, read & slice the lines.
        }
        _ => {}   // non_exhaustive — always a wildcard
    }
}
```

### `PromptResponse`

```rust
Ok(PromptResponse::new(StopReason::EndTurn))
// other reasons: MaxTokens, Cancelled, MaxTurnRequests, Refusal
```

### Streaming: `ContentChunk` + `SessionUpdate` + `SessionNotification`

```rust
let chunk  = ContentChunk::new(ContentBlock::from(delta_text));   // From<Into<String>>
let update = SessionUpdate::AgentMessageChunk(chunk);
cx.send_notification(SessionNotification::new(session_id.clone(), update))?;
```

`SessionUpdate` variants siGit uses:

- `AgentMessageChunk(ContentChunk)` — assistant text.
- `ToolCall(ToolCall)` — start a tool-call card (used for model load/download progress).
- `ToolCallUpdate(ToolCallUpdate)` — update that card's title/status/content.
- `AvailableCommandsUpdate(AvailableCommandsUpdate)` — advertise slash commands.
- `ConfigOptionUpdate(ConfigOptionUpdate)` — refresh the picker mid-session.

(Other variants exist: `UserMessageChunk`, `AgentThoughtChunk`, `Plan`, …)

### `ToolCall` / `ToolCallUpdate` — progress cards

siGit reuses tool-call cards as a generic progress UI (model loading/download):

```rust
// open the card
SessionUpdate::ToolCall(
    ToolCall::new(tool_call_id.clone(), "Loading Qwen 2.5 3B")
        .kind(ToolKind::Think)
        .status(ToolCallStatus::InProgress)
        .content(vec!["Loading…".into()]),
)
// update it (only the fields you set)
SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
    tool_call_id.clone(),
    ToolCallUpdateFields::new()
        .title("✓ Qwen 2.5 3B loaded")
        .status(ToolCallStatus::Completed),
))
```

`ToolCallStatus`: `InProgress`, `Completed`, `Failed`. `ToolKind::Think` is the
"thinking/util" kind.

### `Error`

```rust
agent_client_protocol::Error::new(-32603, "internal error message")  // there is NO Error::internal()
agent_client_protocol::Error::new(-32602, "invalid params: …")        // or Error::invalid_params()
agent_client_protocol::Error::new(-32000, "not signed in …")          // app-defined
```

---

## Config options — the editor model picker

ACP lets the agent expose per-session config controls; Zed renders them in the
agent panel. siGit uses one `select` option as a model picker.

```rust
const MODEL_CONFIG_ID: &str = "sigit-model";

let options: Vec<SessionConfigSelectOption> = models.iter().map(|m| {
    SessionConfigSelectOption::new(
        SessionConfigValueId::new(m.model_id.as_str()),
        format!("{} {badge}", m.display_name),
    ).description(desc)
}).collect();

let config_options = vec![
    SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current_value, options)
        .category(SessionConfigOptionCategory::Model)
        .description("Select an on-device model or a siGit Code Cloud tier"),
];
```

Return these from new/load/fork session via `.config_options(config_options)`.
When the user picks one, the client sends `SetSessionConfigOptionRequest`:

```rust
async fn handle_set_session_config_option(&self, cx: &ConnectionTo<Client>,
        args: SetSessionConfigOptionRequest) -> Result<SetSessionConfigOptionResponse> {
    if args.config_id.0.as_ref() != MODEL_CONFIG_ID { return Err(Error::new(-32602, "…")); }
    let model_id = args.value.0.as_ref();
    // … switch model, streaming ToolCall progress via cx …
    Ok(SetSessionConfigOptionResponse::new(rebuilt_config_options))
}
```

To refresh the picker mid-session (e.g. after `/reload`), push
`SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(config_options))`.

**Gotcha:** Zed re-fires the last selection on (re)connect. Guard against a no-op
re-select of the already-active model, and don't try to load a new model while a
startup load is still in flight (the old weights still hold GPU memory → the new
load fails with "does not fit"). siGit waits for `model_ready` first.

---

## Slash commands

Advertise them so the editor forwards `/`-prefixed input (Zed rejects unknown
slash commands client-side):

```rust
let commands = vec![
    AvailableCommand::new("help", "Show available commands"),
    AvailableCommand::new("models", "List available models").input(
        AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
            "model number to switch to (optional)"))),
    // … login/logout/whoami/reload/clear/status …
];
cx.send_notification(SessionNotification::new(
    session_id,
    SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(commands)),
))?;
```

siGit parses slash text out of the prompt itself (`parse_slash`) and dispatches in
`exec_slash_acp` before falling through to inference. The command turn still ends
with `Ok(PromptResponse::new(StopReason::EndTurn))`.

---

## Concurrency: the `block_in_place` trap (still real)

`mistralrs` model loading calls `tokio::task::block_in_place` internally, which
**panics off a multi-threaded runtime worker** ("can call blocking only when
running on the multi-threaded runtime"). The builder's task context and
`cx.spawn` tasks are not safe for this.

siGit's fix: **do the blocking model load on a dedicated `std::thread` with its
own fresh `tokio::runtime::Runtime`**, and signal completion back via an
`AtomicBool` / `oneshot` channel. Never call `load_gguf_model` directly inside a
prompt handler or a `cx.spawn` task.

```rust
std::thread::spawn(move || {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(loader_engine.load_gguf_model(cfg, prompt, sampling));
    // store result, flip an AtomicBool / send on a oneshot
});
```

The prompt handler then `await`s readiness (siGit polls `model_ready` on a 1s
`tokio::time::interval`, streaming a spinner via `cx.send_notification`).

---

## Logging

stdout is the ACP JSON-RPC wire — **log only to stderr.** siGit uses
`tracing_subscriber` to stderr:

```rust
tracing_subscriber::fmt::Subscriber::builder()
    .with_env_filter(EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info")))
    .with_writer(std::io::stderr)
    .try_init();
```

In siGit's interactive TTY mode (not ACP), it goes further and redirects the
stdout/stderr **fds** to `$TMPDIR/sigit.log` so mistralrs/native noise can't
corrupt the ratatui screen. ACP mode keeps stdout pristine for protocol JSON.

---

## TTY vs ACP split

`main()` decides mode from `std::io::stdin().is_terminal()`:

- **TTY** → interactive ratatui chat (`run_interactive`, Unix-only — needs fd
  redirection).
- **non-TTY** → `run_acp_server()` (editor launched it over a pipe).

Account verbs (`sigit login` / `logout` / `whoami`) are handled before the split,
since the editor launches `sigit login` in an embedded terminal.

---

## Protocol flow

```
Editor                                Agent
  │── initialize ──────────────────────►│  capabilities + auth methods + meta
  │◄─ InitializeResponse ───────────────│
  │── authenticate ────────────────────►│  (button → verify stored session)
  │◄─ AuthenticateResponse ─────────────│
  │── session/new  (or load / fork) ───►│  cwd, reset history
  │◄─ …Response(config_options) ────────│
  │◄─ session/update AvailableCommands ─│  advertise slash commands
  │── session/setConfigOption ─────────►│  (model picker) → ToolCall progress
  │── session/prompt ──────────────────►│  user message (text + resources)
  │◄─ session/update (N×) ──────────────│  streaming chunks / tool-call cards
  │◄─ PromptResponse(EndTurn) ──────────│
  │── session/cancel (notification) ───►│
  │── [disconnect] ─────────────────────►│  connect_to future resolves → shutdown
```

---

## Zed configuration

```json
{
  "agent_servers": {
    "siGit Code": {
      "type": "custom",
      "command": "/absolute/path/to/target/release/sigit"
    }
  }
}
```

---

## Gotchas

1. **No `Agent` trait to implement** — it's a builder. Register handler closures
   with `.on_receive_request(closure, on_receive_request!())` and finish with
   `.connect_to(transport)`. The `on_receive_request!()` / `on_receive_notification!()`
   macro is mandatory per handler.
2. **`cx: ConnectionTo<Client>` replaces the mpsc forwarder** — send notifications
   with `cx.send_notification(...)` and background tasks with `cx.spawn(...)`.
   Don't reintroduce the old channel-based circular-dependency pattern.
3. **`Error::internal()` doesn't exist** — use `Error::new(-32603, msg)`.
4. **Everything in `agent_client_protocol::schema` is `#[non_exhaustive]`** — use
   builder methods, never struct literals; add `_ => …` wildcards when matching.
5. **`ByteStreams::new(stdout, stdin)`** — writer first, reader second. Adapt
   tokio stdio with `.compat()` / `.compat_write()` (tokio-util).
6. **`block_in_place` panics in handler/`cx.spawn` tasks** — run mistralrs model
   loads on a dedicated `std::thread` + its own `Runtime`; signal back via
   `AtomicBool`/`oneshot`. Never load inside a prompt handler directly.
7. **Empty `authMethods` hangs Zed** — always include at least one
   `AuthMethod::Agent(AuthMethodAgent::new("id", "Name"))`. Prefer `Agent` over
   `Terminal` for custom agents (Zed never spawns the terminal for them).
8. **Never write to stdout except JSON-RPC** — log to stderr; in TTY mode siGit
   redirects fds to `$TMPDIR/sigit.log`. Any stray `println!` or native library
   stdout write corrupts the wire.
9. **Unstable features gate real types** — `unstable_session_fork`,
   `unstable_session_additional_directories`, `unstable_auth_methods` must be on
   in `Cargo.toml` or `ForkSessionRequest`, `additional_directories`, and
   `AuthMethod::Agent` won't exist.
10. **Zed re-fires the last config selection on connect** — make
    `setConfigOption` a no-op when the requested model is already active, and
    never start a model switch while a startup load is still in flight (GPU OOM).
11. **Store `SessionId` as `SessionId`**, not `String`, so `==` is clean.
12. **`SetSessionConfigOptionResponse::new(config_options)`** — the response
    carries the *rebuilt* options so the picker reflects the new current value.

---

## Where to look in the code

Everything ACP lives in `src/main.rs`:

- `run_acp_server` — builder wiring + transport.
- `SiGitAgent` + `handle_*` — the handlers.
- `build_model_config_options` / `resolve_model_config` — picker.
- `parse_slash` / `exec_slash_acp` — slash commands.
- `handle_response` — the `Responder` helper.

`src/backend.rs` holds the `InferenceBackend` trait (`LocalBackend` /
`OpenAiBackend`) used by `handle_prompt`'s tool-calling loop; `src/tools.rs`
defines the agent tools and `execute_tool`.
