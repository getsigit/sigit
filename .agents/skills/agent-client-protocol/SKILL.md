# Skill: Agent Client Protocol (ACP) — Rust Implementation

## Overview

ACP is a JSON-RPC 2.0 protocol over **stdio** for integrating AI coding agents
with editors (Zed, JetBrains, Neovim, etc.). The agent runs as a subprocess;
the editor is the client. Communication is newline-delimited JSON on stdin/stdout.

Crate: `agent-client-protocol = "0.10.4"` (latest as of 2025)
Docs:  https://docs.rs/agent-client-protocol
Spec:  https://agentclientprotocol.com

---

## Dependency setup

```toml
[dependencies]
agent-client-protocol = "0.10.4"
async-trait           = "0.1"
tokio                 = { version = "1", features = ["rt", "rt-multi-thread", "macros", "io-std", "io-util", "sync"] }
tokio-util            = { version = "0.7", features = ["compat"] }
futures               = "0.3"
```

---

## The `Agent` trait

Declared `#[async_trait::async_trait(?Send)]` — futures are `!Send`.
Your impl needs the same annotation:

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

You must implement `initialize`, `authenticate`, `new_session`, `prompt`, and `cancel`.
Everything else (`load_session`, `set_session_mode`, etc.) defaults to `Err(method_not_found)`.

---

## Types and their builders

All `#[non_exhaustive]` structs require builder methods — struct literal syntax won't compile.

### `InitializeRequest` / `InitializeResponse`

```rust
// Response builder — use ProtocolVersion::V1, NOT args.protocol_version:
InitializeResponse::new(ProtocolVersion::V1)
    .agent_info(
        Implementation::new("my-agent", env!("CARGO_PKG_VERSION"))
            .title("My Agent"),
    )
    .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
        "my-agent", "My Agent",
    ))])
    .agent_capabilities(AgentCapabilities::default())
```

`auth_methods` must include at least one `AuthMethod::Agent` or Zed hangs on
"Loading…" forever. Import `AuthMethod`, `AuthMethodAgent`, and `ProtocolVersion`
from the crate.

### `AuthenticateResponse`

```rust
Ok(AuthenticateResponse::default())  // No auth = just return default
```

### `NewSessionResponse`

```rust
let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
Ok(NewSessionResponse::new(session_id))
```

`SessionId` is a newtype with `Clone`, `PartialEq`, `Display`, `Into<String>`,
and `AsRef<str>`. Store it as-is (not as `String`) so `==` works directly.

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
    _ => ...,  // non_exhaustive — always need a wildcard
}
```

### `ContentChunk` + `SessionUpdate` — streaming

```rust
let chunk = ContentChunk::new(ContentBlock::from(delta_text));
let update = SessionUpdate::AgentMessageChunk(chunk);
// Other variants: UserMessageChunk, AgentThoughtChunk, ToolCall, Plan, ...
```

### `SessionNotification` — send streaming content to client

```rust
let notification = SessionNotification::new(session_id.clone(), update);
// Deliver via AgentSideConnection::session_notification()
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

Wraps stdin/stdout with JSON-RPC machinery.

```rust
use futures::future::LocalBoxFuture;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// Adapt tokio I/O to futures AsyncRead/AsyncWrite (the SDK expects these)
let stdin  = tokio::io::stdin().compat();
let stdout = tokio::io::stdout().compat_write();

// Must run inside a LocalSet — the spawn fn takes LocalBoxFuture (!Send)
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

`AgentSideConnection::new` returns `(conn, io_task)` — you need both. `io_task`
drives the actual IO; `conn` sends notifications. The spawn closure gets
`LocalBoxFuture<'static, ()>` (not Send), so use `tokio::task::spawn_local`,
not `tokio::spawn`. Everything must sit inside
`tokio::task::LocalSet::new().run_until(...)`.

---

## Streaming — circular dependency pattern

`Agent::prompt()` needs to send `SessionNotification` through the connection,
but the connection is built *from* the agent. Break the cycle with an mpsc channel:

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

Inside `prompt()`, push chunks through the channel:
```rust
self.notification_tx.send(SessionNotification::new(
    session_id.clone(),
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(delta))),
)).await.ok();  // ignore send errors (channel closed = client gone)
```

---

## Logging

Log to **stderr** — stdout is the ACP JSON-RPC wire:

```rust
env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
    .target(env_logger::Target::Stderr)
    .init();
```

---

## Protocol flow

```
Editor                          Agent
  │                               │
  │── initialize ────────────────►│  (negotiate version + capabilities)
  │◄─ InitializeResponse ─────────│
  │                               │
  │── authenticate ──────────────►│  (method_id from authMethods)
  │◄─ AuthenticateResponse ───────│
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
      "type": "custom",
      "command": "/path/to/binary"
    }
  }
}
```

---

## Gotchas

1. **`Error::internal()` doesn't exist** — use `Error::new(-32603, msg)`.
2. **All protocol structs are `#[non_exhaustive]`** — use builder methods,
   never struct literals. Add `_ => ...` wildcards when matching.
3. **`LocalBoxFuture` is `!Send`** — `tokio::spawn` won't work; use
   `tokio::task::spawn_local` inside a `LocalSet`.
4. **`tokio::task::spawn_local` panics outside a `LocalSet`** — wrap with
   `LocalSet::new().run_until(async { ... }).await`.
5. **Store `SessionId` as `SessionId`**, not `String` — otherwise `==`
   comparisons get annoying.
6. **One session per connection is fine for MVP** — reuse the model with
   `clear_history()` instead of reloading.
7. **`AgentCapabilities::default()` exists** — all capabilities None/false.
8. **`block_in_place` panics inside `spawn_local`** — dependencies that call
   `tokio::task::block_in_place` internally (e.g. `mistralrs`) will blow up
   with "can call blocking only when running on the multi-threaded runtime"
   from a `spawn_local` task. Fix: do the blocking work *before* entering
   the `LocalSet`, while you're still on a normal multi-thread worker, then
   pass the result into your agent struct.
9. **Empty `authMethods` hangs Zed** — `InitializeResponse` with an empty
   `auth_methods` vec makes Zed show "Loading…" forever. Always include at
   least one `AuthMethod::Agent(AuthMethodAgent::new("id", "Name"))`.
   Import `AuthMethod`, `AuthMethodAgent`, and `ProtocolVersion` from the crate.
10. **Never write to stdout except JSON-RPC** — any library that prints to
    stdout (`mistralrs` model metadata, stray `println!`, whatever) will
    corrupt the wire. Redirect diagnostics to stderr. If a dependency writes
    to stdout internally, fix it or suppress it before shipping.