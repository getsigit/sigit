//! End-to-end ACP permission round-trip against the real binary.
//!
//! Spawns `sigit` in ACP mode (stdin piped, so not a TTY) wired to a scripted
//! OpenAI-compatible SSE endpoint via the `OPENAI_BASE_URL` override, then
//! drives newline-delimited JSON-RPC over stdio. The scripted model calls
//! `run_command` — a mutating tool — so the agent must send
//! `session/request_permission` mid-turn (the exact path the spawned-handler /
//! `turn_lock` design exists for). The test answers it twice:
//!
//! 1. `cancelled` — the prompt must stop with `stopReason: "cancelled"`, and
//!    the *next* request to the endpoint must show the abandoned round closed
//!    out with `role: "tool"` results, or a strict OpenAI-compatible endpoint
//!    would reject the whole session.
//! 2. `selected: allow_once` — the tool must actually execute and its output
//!    travel back to the endpoint as a tool result.

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
        // `connection: close` below means one request per connection, so the
        // serial accept loop matches the agent's serial completion requests.
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

fn spawn_agent(port: u16, config_dir: &std::path::Path) -> AgentUnderTest {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sigit"))
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
        .env("SIGIT_CONFIG_DIR", config_dir)
        .env("SIGIT_MCP", "off")
        // A fresh config dir means the default permission mode, `ask` — make
        // sure the environment can't turn the gate off underneath the test.
        .env_remove("SIGIT_PERMISSIONS")
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

    fn respond(&mut self, id: Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    /// Skip notifications and unrelated traffic until `matches` is satisfied.
    fn wait_for(&mut self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.incoming.recv_timeout(remaining) {
                Ok(message) if matches(&message) => return message,
                Ok(_) => continue,
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }

    /// The response to one of *our* requests (has our id, no `method`).
    fn wait_for_response(&mut self, id: u64) -> Value {
        let response = self.wait_for(&format!("response to request {id}"), |message| {
            message["id"] == id && message.get("method").is_none()
        });
        assert!(
            response.get("error").is_none(),
            "request {id} failed: {response}"
        );
        response
    }

    /// A request *from* the agent (has a `method` and its own id).
    fn wait_for_agent_request(&mut self, method: &str) -> Value {
        self.wait_for(&format!("agent request {method}"), |message| {
            message["method"] == method && message.get("id").is_some()
        })
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
fn permission_round_trip_cancel_then_allow() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "run_command", r#"{"command":"echo sigit-first"}"#),
        sse_tool_call(
            "call_2",
            "run_command",
            r#"{"command":"echo sigit-approved"}"#,
        ),
        sse_text("done"),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_perm_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let cwd = scratch.join("cwd");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    let mut agent = spawn_agent(endpoint.port, &config_dir);

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

    // ── Turn 1: cancel at the permission gate ───────────────────────────
    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the first command"}],
        }),
    );

    let permission = agent.wait_for_agent_request("session/request_permission");
    let params = &permission["params"];
    assert_eq!(params["sessionId"], session_id.as_str());
    let title = params["toolCall"]["title"].as_str().expect("title");
    assert!(
        title.contains("run_command") && title.contains("echo sigit-first"),
        "the dialog must show the tool and its arguments, got: {title}"
    );
    assert_eq!(
        params["toolCall"]["rawInput"]["command"], "echo sigit-first",
        "full arguments must travel as rawInput"
    );
    let option_ids: Vec<&str> = params["options"]
        .as_array()
        .expect("options")
        .iter()
        .map(|option| option["optionId"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(option_ids, ["allow_once", "allow_session", "reject_once"]);

    agent.respond(
        permission["id"].clone(),
        json!({"outcome": {"outcome": "cancelled"}}),
    );

    let response = agent.wait_for_response(prompt_id);
    assert_eq!(response["result"]["stopReason"], "cancelled");

    // ── Turn 2: history must be repaired; then approve once ─────────────
    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the second command"}],
        }),
    );

    let permission = agent.wait_for_agent_request("session/request_permission");
    agent.respond(
        permission["id"].clone(),
        json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
    );

    let response = agent.wait_for_response(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    // ── What the endpoint saw ────────────────────────────────────────────
    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "expected exactly three completions");

    // Request 2 replays the full history: the cancelled round's tool call
    // must be answered by a `role: "tool"` message, not left dangling.
    let messages = requests[1]["messages"].as_array().expect("messages");
    let call_position = messages
        .iter()
        .position(|message| message["tool_calls"][0]["id"] == "call_1")
        .expect("cancelled turn's assistant tool call in replayed history");
    let repair = &messages[call_position + 1];
    assert_eq!(repair["role"], "tool", "dangling tool call not closed out");
    assert_eq!(repair["tool_call_id"], "call_1");
    assert!(
        repair["content"]
            .as_str()
            .unwrap_or_default()
            .contains("cancelled"),
        "repair message should say the turn was cancelled: {repair}"
    );

    // Request 3 carries the approved call's real output.
    let messages = requests[2]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_2")
        .expect("tool result for the approved call");
    assert!(
        result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("sigit-approved"),
        "the approved command's output should reach the endpoint: {result}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}
