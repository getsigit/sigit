//! End-to-end stdio MCP transport test against the real binary.
//!
//! Spawns `sigit` in ACP mode wired to a scripted OpenAI-compatible SSE
//! endpoint (the same harness as `acp_permissions.rs`) and to a temp
//! `SIGIT_CONFIG_DIR` whose `mcp.toml` configures three stdio servers, all
//! backed by the `mcp_stdio_stub` helper binary:
//!
//! - `stub` — healthy; exposes one `echo` tool and prefixes replies with the
//!   `STUB_PREFIX` env var from `[server.env]`, proving env propagation.
//! - `dying` — completes the discovery handshake, then exits. Its tool is
//!   offered to the model, but calling it must fail with an in-band error.
//! - `deadone` — exits(1) at spawn; discovery must record it unavailable and
//!   offer no tools for it.
//!
//! The scripted model calls `mcp__stub__echo`, then `mcp__dying__echo`, then
//! finishes. The test asserts the tool round-trip, the dead-child error, and
//! the `/mcp` listing (command lines shown, dead server flagged).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

// ── Scripted OpenAI-compatible endpoint ─────────────────────────────────────

fn sse_body(events: &[Value]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn sse_tool_call(id: &str, name: &str, arguments: &str) -> String {
    sse_body(&[json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "function": {"name": name, "arguments": arguments},
        }]}}]
    })])
}

fn sse_text(text: &str) -> String {
    sse_body(&[json!({"choices": [{"delta": {"content": text}}]})])
}

/// Serves one scripted SSE response per request and records each request body.
struct FakeEndpoint {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
}

fn start_fake_endpoint(responses: Vec<String>) -> FakeEndpoint {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake endpoint");
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let recorded = Arc::clone(&requests);
    let queue = Mutex::new(VecDeque::from(responses));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(clone) => clone,
                Err(_) => continue,
            });
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim();
                if line.is_empty() {
                    break;
                }
                if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = length.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            if let Ok(request) = serde_json::from_slice::<Value>(&body) {
                recorded.lock().unwrap().push(request);
            }
            let payload = queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| sse_text("out of scripted responses"));
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    FakeEndpoint { port, requests }
}

// ── ACP client over the binary's stdio ──────────────────────────────────────

struct AgentUnderTest {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
}

fn spawn_agent(port: u16, config_dir: &std::path::Path, cwd: &std::path::Path) -> AgentUnderTest {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sigit"))
        .current_dir(cwd)
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
        .env("SIGIT_CONFIG_DIR", config_dir)
        // MCP stays ON (that's what we test), but the baked-in official
        // server must not phone home from CI.
        .env("SIGIT_MCP_OFFICIAL", "off")
        .env_remove("SIGIT_MCP")
        // MCP tools are mutating and would otherwise wait at the permission
        // gate; permissions have their own test.
        .env("SIGIT_PERMISSIONS", "allow")
        .env_remove("SIGIT_LOCAL_INFERENCE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sigit in ACP mode");

    let stdout = child.stdout.take().unwrap();
    let (message_tx, incoming) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && message_tx.send(message).is_err()
            {
                break;
            }
        }
    });

    let stdin = child.stdin.take().unwrap();
    AgentUnderTest {
        child,
        stdin,
        incoming,
        next_id: 0,
    }
}

impl AgentUnderTest {
    fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("write to agent stdin");
        self.stdin.flush().expect("flush agent stdin");
    }

    fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Wait for the response to our request `id`, collecting the raw JSON of
    /// every `session/update` notification that arrives before it.
    fn wait_for_response_collecting_updates(&mut self, id: u64) -> (Value, String) {
        let deadline = Instant::now() + TIMEOUT;
        let mut updates = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.incoming.recv_timeout(remaining) {
                Ok(message) if message["id"] == id && message.get("method").is_none() => {
                    assert!(
                        message.get("error").is_none(),
                        "request {id} failed: {message}"
                    );
                    return (message, updates);
                }
                Ok(message) => {
                    if message["method"] == "session/update" {
                        updates.push_str(&message["params"].to_string());
                        updates.push('\n');
                    }
                }
                Err(_) => panic!("timed out waiting for response to request {id}"),
            }
        }
    }

    fn wait_for_response(&mut self, id: u64) -> Value {
        self.wait_for_response_collecting_updates(id).0
    }
}

impl Drop for AgentUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── The round-trip ──────────────────────────────────────────────────────────

#[test]
fn stdio_mcp_discovery_call_and_dead_child() {
    let stub = env!("CARGO_BIN_EXE_mcp_stdio_stub");

    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "mcp__stub__echo", r#"{"text":"hello"}"#),
        sse_tool_call("call_2", "mcp__dying__echo", r#"{"text":"gone"}"#),
        sse_text("done"),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_mcp_stdio_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let cwd = scratch.join("cwd");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    // TOML literal strings (single quotes) keep Windows backslashes intact.
    let mcp_toml = format!(
        r#"official = false

[[server]]
name = "stub"
command = '{stub}'

[server.env]
STUB_PREFIX = "pfx:"

[[server]]
name = "dying"
command = '{stub}'
args = ["--exit-after-list"]

[[server]]
name = "deadone"
command = '{stub}'
args = ["--fail"]
"#
    );
    std::fs::write(config_dir.join("mcp.toml"), mcp_toml).unwrap();

    let mut agent = spawn_agent(endpoint.port, &config_dir, &cwd);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    agent.wait_for_response(id);

    let id = agent.request("session/new", json!({"cwd": cwd, "mcpServers": []}));
    let session_id = agent.wait_for_response(id)["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    // ── /mcp: listing shows command lines and flags the dead server ─────
    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "/mcp"}],
        }),
    );
    let (_, listing) = agent.wait_for_response_collecting_updates(prompt_id);
    // Raw JSON of the update notifications; escape the path the way JSON does
    // so Windows backslashes compare correctly.
    let stub_json = serde_json::to_string(stub).unwrap();
    let stub_escaped = stub_json.trim_matches('"');
    assert!(
        listing.contains("mcp__stub__echo"),
        "/mcp must list the healthy server's tool, got: {listing}"
    );
    assert!(
        listing.contains(stub_escaped),
        "/mcp must show the stdio server's command line, got: {listing}"
    );
    assert!(
        listing.contains("--exit-after-list"),
        "/mcp must include the args in the command line, got: {listing}"
    );
    assert!(
        listing.contains("unavailable"),
        "/mcp must flag the server that died at spawn, got: {listing}"
    );

    // ── One prompt: echo round-trip, then the dead-child call ───────────
    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "use the stub tools"}],
        }),
    );
    let response = agent.wait_for_response(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    // ── What the endpoint saw ────────────────────────────────────────────
    let requests = endpoint.requests.lock().unwrap();
    // Slash commands never reach the model, so all three completions belong
    // to the tool-calling prompt.
    assert_eq!(requests.len(), 3, "expected exactly three completions");

    // The offered tool specs must include both live servers' echo tools and
    // nothing from the server that failed discovery.
    let tools = requests[0]["tools"].to_string();
    assert!(
        tools.contains("mcp__stub__echo"),
        "stub tool missing from specs: {tools}"
    );
    assert!(
        tools.contains("mcp__dying__echo"),
        "dying server's tool missing from specs: {tools}"
    );
    assert!(
        !tools.contains("mcp__deadone__"),
        "a server that failed discovery must contribute no tools: {tools}"
    );

    // The echo call's result must round-trip, carrying the [server.env]
    // prefix (proving env vars reached the child).
    let messages = requests[1]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_1")
        .expect("tool result for the echo call");
    assert_eq!(
        result["content"].as_str().unwrap_or_default(),
        "pfx:hello",
        "echo result should carry the env-var prefix"
    );

    // The call to the server that died after discovery must come back as an
    // in-band error string, not hang or crash the agent.
    let messages = requests[2]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_2")
        .expect("tool result for the dead server's call");
    let content = result["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("Error") && content.contains("stdio server 'dying'"),
        "dead-child call must fail in-band, got: {content}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

// ── Servers the client supplies per session ─────────────────────────────────

/// ACP clients pass MCP servers in `mcpServers` on `session/new`, and every
/// agent has to connect to the stdio ones (issue #135). The server belongs to
/// the session that named it: a second session opened without it must not be
/// offered its tools.
#[test]
fn client_supplied_stdio_server_is_scoped_to_its_session() {
    let stub = env!("CARGO_BIN_EXE_mcp_stdio_stub");

    let endpoint = start_fake_endpoint(vec![
        // The session without the server: one plain answer.
        sse_text("nothing to call"),
        // The session with it: call the tool, then finish.
        sse_tool_call("call_1", "mcp__editor__echo", r#"{"text":"hello"}"#),
        sse_text("done"),
    ]);

    let scratch =
        std::env::temp_dir().join(format!("sigit_mcp_client_servers_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let cwd = scratch.join("cwd");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    // No startup servers at all, so every MCP tool seen below came from the
    // client.
    std::fs::write(
        config_dir.join("mcp.toml"),
        "official = false\nsmbcloud = false\nascapi = false\n",
    )
    .unwrap();

    let mut agent = spawn_agent(endpoint.port, &config_dir, &cwd);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    agent.wait_for_response(id);

    let id = agent.request(
        "session/new",
        json!({
            "cwd": cwd,
            "mcpServers": [{
                "name": "editor",
                "command": stub,
                "args": [],
                "env": [{"name": "STUB_PREFIX", "value": "ed:"}],
            }],
        }),
    );
    let with_server = agent.wait_for_response(id)["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    let id = agent.request("session/new", json!({"cwd": cwd, "mcpServers": []}));
    let without_server = agent.wait_for_response(id)["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    let id = agent.request(
        "session/prompt",
        json!({
            "sessionId": without_server,
            "prompt": [{"type": "text", "text": "anything to call?"}],
        }),
    );
    agent.wait_for_response(id);

    // Switching back to the first session has to bring its server with it.
    let id = agent.request(
        "session/prompt",
        json!({
            "sessionId": with_server,
            "prompt": [{"type": "text", "text": "/mcp"}],
        }),
    );
    let (_, listing) = agent.wait_for_response_collecting_updates(id);
    assert!(
        listing.contains("mcp__editor__echo") && listing.contains("from the editor"),
        "/mcp must list the client-supplied server and say where it came from, got: {listing}"
    );

    let id = agent.request(
        "session/prompt",
        json!({
            "sessionId": with_server,
            "prompt": [{"type": "text", "text": "use the editor tool"}],
        }),
    );
    let response = agent.wait_for_response(id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "expected exactly three completions");

    let tools = requests[0]["tools"].to_string();
    assert!(
        !tools.contains("mcp__editor__"),
        "a session must not be offered another session's server: {tools}"
    );
    let tools = requests[1]["tools"].to_string();
    assert!(
        tools.contains("mcp__editor__echo"),
        "the client-supplied server's tool missing from specs: {tools}"
    );

    // The call round-trips, and the env the client sent reached the child.
    let messages = requests[2]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_1")
        .expect("tool result for the echo call");
    assert_eq!(result["content"].as_str().unwrap_or_default(), "ed:hello");

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

// ── HTTP servers the client supplies ────────────────────────────────────────

/// A Streamable HTTP MCP server with one `echo` tool. Records the
/// `Authorization` header of every request so the test can check that the
/// headers the client sent with the server definition reached it.
struct FakeMcpServer {
    port: u16,
    authorizations: Arc<Mutex<Vec<String>>>,
}

fn start_fake_mcp_server() -> FakeMcpServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake MCP server");
    let port = listener.local_addr().unwrap().port();
    let authorizations: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = Arc::clone(&authorizations);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(clone) => clone,
                Err(_) => continue,
            });
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim();
                if line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(length) = lower.strip_prefix("content-length:") {
                    content_length = length.trim().parse().unwrap_or(0);
                }
                if lower.starts_with("authorization:") {
                    let value = line["authorization:".len()..].trim().to_string();
                    recorded.lock().unwrap().push(value);
                }
            }
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let Ok(message) = serde_json::from_slice::<Value>(&body) else {
                continue;
            };

            let result = match message["method"].as_str().unwrap_or_default() {
                "initialize" => Some(json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake-http-mcp", "version": "0.0.0"},
                })),
                "tools/list" => Some(json!({
                    "tools": [{
                        "name": "echo",
                        "description": "Echo the text back.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                        },
                    }],
                })),
                "tools/call" => {
                    let text = message["params"]["arguments"]["text"]
                        .as_str()
                        .unwrap_or_default();
                    Some(json!({
                        "content": [{"type": "text", "text": format!("http:{text}")}],
                    }))
                }
                _ => None,
            };

            let response = match (message.get("id"), result) {
                (Some(id), Some(result)) => {
                    let payload = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    )
                }
                // Notifications get an empty 202.
                _ => "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .to_string(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });

    FakeMcpServer {
        port,
        authorizations,
    }
}

/// A client only sends HTTP MCP servers to an agent that advertises
/// `mcpCapabilities.http`, and then expects them connected with the headers it
/// supplied (issue #151). SSE stays unadvertised.
#[test]
fn client_supplied_http_server_is_connected() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "mcp__remote__echo", r#"{"text":"hello"}"#),
        sse_text("done"),
    ]);
    let mcp_server = start_fake_mcp_server();

    let scratch =
        std::env::temp_dir().join(format!("sigit_mcp_client_http_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let cwd = scratch.join("cwd");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        config_dir.join("mcp.toml"),
        "official = false\nsmbcloud = false\nascapi = false\n",
    )
    .unwrap();

    let mut agent = spawn_agent(endpoint.port, &config_dir, &cwd);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let initialize = agent.wait_for_response(id);
    let capabilities = &initialize["result"]["agentCapabilities"]["mcpCapabilities"];
    assert_eq!(
        capabilities["http"], true,
        "HTTP MCP servers must be advertised: {initialize}"
    );
    assert_eq!(
        capabilities["sse"], false,
        "the deprecated SSE transport must stay off: {initialize}"
    );

    let id = agent.request(
        "session/new",
        json!({
            "cwd": cwd,
            "mcpServers": [{
                "type": "http",
                "name": "remote",
                "url": format!("http://127.0.0.1:{}/mcp", mcp_server.port),
                "headers": [{"name": "Authorization", "value": "Bearer from-the-editor"}],
            }],
        }),
    );
    let session_id = agent.wait_for_response(id)["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    let id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "use the remote tool"}],
        }),
    );
    let response = agent.wait_for_response(id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "expected exactly two completions");
    let tools = requests[0]["tools"].to_string();
    assert!(
        tools.contains("mcp__remote__echo"),
        "the HTTP server's tool missing from specs: {tools}"
    );
    let messages = requests[1]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_1")
        .expect("tool result for the echo call");
    assert_eq!(result["content"].as_str().unwrap_or_default(), "http:hello");

    // Every request to the server, handshake and call alike, carried the
    // header from the server definition.
    let authorizations = mcp_server.authorizations.lock().unwrap();
    assert!(
        authorizations.len() >= 4,
        "expected initialize, initialized, tools/list and tools/call, got {authorizations:?}"
    );
    assert!(
        authorizations
            .iter()
            .all(|value| value == "Bearer from-the-editor"),
        "the client's headers must reach the server: {authorizations:?}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}
