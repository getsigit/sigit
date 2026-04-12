# Skill: Agent Client Protocol (ACP) — Rust Implementation

## Overview

ACP is a JSON-RPC 2.0 protocol over **stdio** that lets AI coding agents integrate
with editors (Zed, JetBrains, Neovim, etc.). The agent is a subprocess; the editor
is the client. Communication is newline-delimited JSON on stdin/stdout.

Crate: `agent-client-protocol = "0.10.4"` (latest as of 2025)
Docs:  https://docs.rs/agent-client-protocol
Spec:  https://agentclientprotocol.com

---

## Dependency setup

```toml
[dependencies]
agent-client-protocol = "0.10.4"
async-trait           = "0.1"
tokio                 = { version = "1", features = ["rt", "macros", "io-std", "io-util", "sync"] }
tokio-util            = { version = "0.7", features = ["compat"] }
futures               = "0.3"
```

---

## The `Agent` trait

Defined as `#[async_trait::async_trait(?Send)]` — futures are `!Send`.
You **must** annotate your impl the same way:

```rust
#[async_trait::async_trait(?Send)]
impl Agent for MyAgent {
    async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse> { ... }
    async fn authenticate(&self, args: AuthenticateRequest) -> Result<AuthenticateResponse> { ... }
    async fn new_session(&self, args: NewSessionRequest) -> Result<NewSessionResponse> { ... }
    async fn prompt(&self, args: PromptRequest) -> Result<PromptResponse> { ... }
    async fn cancel(&self, args: CancelNotification) -> Result<()> { ... }
    // All other methods have default impls that return Error::method_not_found()
}
```

**Mandatory** to implement: `initialize`, `authenticate`, `new_session`, `prompt`, `cancel`.
All others (`load_session`, `set_session_mode`, etc.) have default `Err(method_not_found)` impls.

---

## Key types and their builders

All `#[non_exhaustive]` structs must be constructed via their builder methods, NOT
struct literal syntax.

### `InitializeRequest` / `InitializeResponse`

```rust
// Request field you need:
args.protocol_version  // type: ProtocolVersion — echo it back

// Response builder:
InitializeResponse::new(args.protocol_version)
    .agent_info(
        Implementation::new("my-agent", env!("CARGO_PKG_VERSION"))
            .title("My Agent — Description"),
    )
    .agent_capabilities(AgentCapabilities::default())
```

### `AuthenticateResponse`

```rust
Ok(AuthenticateResponse::default())  // No auth = just return default
```

### `NewSessionResponse`

```rust
let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
Ok(NewSessionResponse::new(session_id))
```

`SessionId` is a newtype. It implements `Clone`, `PartialEq`, `Display`, `Into<String>`,
and `AsRef<str>`. Store it directly (not as `String`) so `==` comparisons work.

### `PromptRequest`

```rust
args.session_id   // type: SessionId
args.prompt       // type: Vec<ContentBlock>
```

Extract user text from the prompt:
```rust
let user_text: String = args.prompt.iter()
    .filter_map(|block| match block {
        ContentBlock::Text(t) => Some(t.text.as_str()),
        _ => None,
    })
    .collect::<Vec<_>>()
    .join("\n");
```

### `PromptResponse`

```rust
Ok(PromptResponse::new(StopReason::EndTurn))
// Other reasons: MaxTokens, Cancelled, MaxTurnRequests, Refusal
```

### `ContentBlock`

```rust
// Text block — use the From impl:
ContentBlock::from("some text")  // impl From<T: Into<String>> for ContentBlock

// Pattern-match incoming blocks:
match block {
    ContentBlock::Text(t)         => t.text.as_str(),
    ContentBlock::ResourceLink(_) => ...,
    ContentBlock::Resource(_)     => ...,
    _ => ...,  // non_exhaustive — always need wildcard
}
```

### `ContentChunk` + `SessionUpdate` — for streaming

```rust
let chunk = ContentChunk::new(ContentBlock::from(delta_text));
let update = SessionUpdate::AgentMessageChunk(chunk);
// Other variants: UserMessageChunk, AgentThoughtChunk, ToolCall, Plan, ...
```

### `SessionNotification` — send streaming content to client

```rust
let notification = SessionNotification::new(session_id.clone(), update);
// Then deliver via AgentSideConnection::session_notification()
```

### `Error`

```rust
// There is NO Error::internal(msg) method — use:
agent_client_protocol::Error::new(-32603, "your message here")

// For invalid params:
agent_client_protocol::Error::invalid_params()

// For method not found (already the trait default):
agent_client_protocol::Error::method_not_found()
```

---

## Running the agent — `AgentSideConnection`

The ACP connection wraps stdin/stdout with JSON-RPC machinery.

```rust
use futures::future::LocalBoxFuture;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// Adapt tokio I/O to futures AsyncRead/AsyncWrite (required by the SDK)
let stdin  = tokio::io::stdin().compat();
let stdout = tokio::io::stdout().compat_write();

// MUST run inside a LocalSet because the spawn fn takes LocalBoxFuture (!Send)
let local = tokio::task::LocalSet::new();
local.run_until(async move {
    let (conn, io_task) = AgentSideConnection::new(
        agent,
        stdout,
        stdin,
        |fut: LocalBoxFuture<'static, ()>| {
            tokio::task::spawn_local(fut);  // requires LocalSet context
        },
    );

    // ... set up forwarder task using conn ...

    io_task.await  // drives JSON-RPC until client disconnects
}).await;
```

**Key facts:**
- `AgentSideConnection::new` returns `(conn, io_task)` — both are needed.
- `io_task` drives the actual IO. `conn` is used to send notifications.
- The `spawn` closure receives `LocalBoxFuture<'static, ()>` (not Send) — use
  `tokio::task::spawn_local`, not `tokio::spawn`.
- The whole thing must run inside `tokio::task::LocalSet::new().run_until(...)`.

---

## Streaming — circular dependency pattern

`Agent::prompt()` needs to send `SessionNotification` via the connection, but the
connection is created *from* the agent. Solve with an **mpsc channel forwarder**:

```rust
// 1. Create channel BEFORE the agent
let (notification_tx, mut notification_rx) = mpsc::channel::<SessionNotification>(256);

// 2. Pass sender into agent
let agent = MyAgent { notification_tx, ... };

// 3. Create connection
let (conn, io_task) = AgentSideConnection::new(agent, stdout, stdin, |fut| {
    tokio::task::spawn_local(fut);
});

// 4. Spawn forwarder that holds `conn`
tokio::task::spawn_local(async move {
    while let Some(notification) = notification_rx.recv().await {
        conn.session_notification(notification).await.ok();
    }
});

// 5. Run IO
io_task.await;
```

Inside `prompt()`, send chunks through the channel:
```rust
self.notification_tx.send(SessionNotification::new(
    session_id.clone(),
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(delta))),
)).await.ok();  // ignore send errors (channel closed = client gone)
```

---

## Logging

Always log to **stderr** — stdout is reserved for ACP JSON-RPC:

```rust
env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
    .target(env_logger::Target::Stderr)
    .init();
```

---

## Complete protocol flow

```
Editor                          siGit
  │                               │
  │── initialize ────────────────►│  (negotiate version + capabilities)
  │◄─ InitializeResponse ─────────│
  │                               │
  │── session/new ───────────────►│  (create session, load model)
  │◄─ NewSessionResponse ─────────│
  │                               │
  │── session/prompt ────────────►│  (user message)
  │◄─ session/update (N times) ───│  (streaming tokens via notification)
  │◄─ PromptResponse ─────────────│  (stop_reason = EndTurn when done)
  │                               │
  │── session/cancel (optional) ──►│
  │                               │
  │── [disconnect] ───────────────►│  (io_task future resolves → shutdown)
```

---

## Zed configuration

```json
{
  "agent_servers": {
    "MyAgent": {
      "command": "/path/to/binary",
      "args": []
    }
  }
}
```

---

## Gotchas

1. **`Error::internal()` does NOT exist** — use `Error::new(-32603, msg)`.
2. **All protocol structs are `#[non_exhaustive]`** — always use builder methods,
   never struct literal syntax. Always add `_ => ...` wildcard when matching.
3. **`LocalBoxFuture` is `!Send`** — `tokio::spawn` won't work; use
   `tokio::task::spawn_local` inside a `LocalSet`.
4. **`tokio::task::spawn_local` panics outside a `LocalSet`** — wrap everything
   in `LocalSet::new().run_until(async { ... }).await`.
5. **`SessionId` should be stored as `SessionId`**, not converted to `String`,
   so `==` comparisons work directly.
6. **One session per connection is the MVP norm** — reuse the model with
   `clear_history()` rather than loading it again.
7. **`AgentCapabilities::default()` exists** — returns all capabilities as None/false.