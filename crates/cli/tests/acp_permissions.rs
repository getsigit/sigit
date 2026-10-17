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
//!
//! A second test covers live progress: a slow command's `in_progress` tool call
//! must reach the client while the command is still running, which is what the
//! editor's spinner is driven from. A third test covers what the client
//! renders: text from two tool rounds must reach it as separate paragraphs,
//! since ACP clients concatenate consecutive agent-message chunks into one
//! block.

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

/// One round that says something and then asks for a tool — the shape that used
/// to leave the next round's text glued to this sentence.
fn sse_text_then_tool_call(text: &str, id: &str, name: &str, arguments: &str) -> String {
    sse_body(&[
        json!({"choices": [{"delta": {"content": text}}]}),
        json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": id,
                "function": {"name": name, "arguments": arguments},
            }]}}]
        }),
    ])
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
    spawn_agent_with_permissions(port, config_dir, None)
}

/// `permissions` sets `SIGIT_PERMISSIONS`; `None` leaves the default `ask` mode
/// in place, which is what the permission round-trip needs.
fn spawn_agent_with_permissions(
    port: u16,
    config_dir: &std::path::Path,
    permissions: Option<&str>,
) -> AgentUnderTest {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sigit"));
    command
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
        .env("SIGIT_CONFIG_DIR", config_dir)
        .env("SIGIT_MCP", "off")
        .env_remove("SIGIT_LOCAL_INFERENCE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    match permissions {
        // A fresh config dir means the default permission mode, `ask` — make
        // sure the environment can't turn the gate off underneath the test.
        None => command.env_remove("SIGIT_PERMISSIONS"),
        Some(mode) => command.env("SIGIT_PERMISSIONS", mode),
    };
    let mut child = command.spawn().expect("spawn sigit in ACP mode");

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

    /// The first `session/update` whose payload satisfies `matches`.
    ///
    /// Only used by tests that run with the permission gate off, so a
    /// `session/request_permission` means the run is misconfigured — say that
    /// rather than waiting out the timeout on a request nobody will answer.
    fn wait_for_update(&mut self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        let message = self.wait_for(what, |message| {
            assert!(
                message["method"] != "session/request_permission",
                "unexpected permission request while waiting for {what}: {message}"
            );
            message["method"] == "session/update" && matches(&message["params"]["update"])
        });
        message["params"]["update"].clone()
    }

    /// The response to `session/prompt`, paired with every `agent_message_chunk`
    /// seen on the way, concatenated exactly as a client renders them.
    fn wait_for_prompt(&mut self, id: u64) -> (Value, String) {
        let mut rendered = String::new();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.incoming.recv_timeout(remaining) else {
                panic!("timed out waiting for the prompt response");
            };
            let update = &message["params"]["update"];
            if message["method"] == "session/update"
                && update["sessionUpdate"] == "agent_message_chunk"
                && let Some(chunk) = update["content"]["text"].as_str()
            {
                rendered.push_str(chunk);
            }
            if message["id"] == id && message.get("method").is_none() {
                assert!(message.get("error").is_none(), "prompt failed: {message}");
                return (message, rendered);
            }
        }
    }

    fn wait_for_response_with_updates(&mut self, id: u64) -> (Value, Vec<Value>) {
        let mut updates = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.incoming.recv_timeout(remaining) else {
                panic!("timed out waiting for response to request {id}");
            };
            if message["method"] == "session/update" {
                updates.push(message["params"]["update"].clone());
            }
            assert!(
                message["method"] != "session/request_permission",
                "unexpected permission request while waiting for response {id}: {message}"
            );
            if message["id"] == id && message.get("method").is_none() {
                assert!(
                    message.get("error").is_none(),
                    "request {id} failed: {message}"
                );
                return (message, updates);
            }
        }
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
    assert_eq!(
        option_ids,
        [
            "allow_once",
            "allow_session",
            "reject_once",
            "reject_session"
        ]
    );
    let reject_always = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["optionId"] == "reject_session")
        .unwrap();
    assert_eq!(reject_always["kind"], "reject_always");

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

    // The client explicitly requests automatic tool selection. Some
    // OpenAI-compatible gateways do not honour OpenAI's implicit `auto`
    // default, which otherwise lets the model describe a tool action in prose
    // and end the turn without a tool call.
    assert_eq!(requests[0]["tool_choice"], "auto");

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

/// A call that needs approval is one tool call from start to finish (issue
/// #138). The permission request used to carry an id of its own, so a client
/// drew an approval card next to a call that already claimed to be running.
#[test]
fn a_permission_request_is_about_the_tool_call_it_was_announced_as() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "run_command",
            r#"{"command":"echo sigit-one-card"}"#,
        ),
        sse_text("done"),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_perm_id_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the command"}],
        }),
    );

    // Messages arrive in order and `wait_for` drops what it skips, so each
    // step below also checks that the one before it came first.
    let call_update = |message: &Value, kind: &str| {
        let update = &message["params"]["update"];
        message["method"] == "session/update"
            && update["sessionUpdate"] == kind
            && update["toolCallId"] == "call_1"
    };

    let announced = agent.wait_for("the tool call announcement", |message| {
        call_update(message, "tool_call")
    });
    // `pending` is the schema's default status, so it is left off the wire.
    let status = &announced["params"]["update"]["status"];
    assert!(
        status.is_null() || status == "pending",
        "a call waiting for approval has not started: {announced}"
    );

    let permission = agent.wait_for_agent_request("session/request_permission");
    assert_eq!(
        permission["params"]["toolCall"]["toolCallId"], "call_1",
        "the request must name the announced call: {permission}"
    );
    agent.respond(
        permission["id"].clone(),
        json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
    );

    let started = agent.wait_for("the approved call starting", |message| {
        call_update(message, "tool_call_update")
    });
    let started = &started["params"]["update"];
    assert_eq!(started["status"], "in_progress", "{started}");
    assert_eq!(
        started["title"], announced["params"]["update"]["title"],
        "the card gets its own title back after the approval dialog: {started}"
    );

    let finished = agent.wait_for("the call finishing", |message| {
        call_update(message, "tool_call_update")
    });
    assert_eq!(finished["params"]["update"]["status"], "completed");

    let response = agent.wait_for_response(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A call the user denies at the prompt did not run, so it ends `failed`
/// rather than `completed` (issue #139), and gets its own title back from
/// the permission request's.
#[test]
fn a_call_denied_at_the_prompt_ends_failed() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "run_command",
            r#"{"command":"echo sigit-denied"}"#,
        ),
        sse_text("ok, skipped"),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_perm_deny_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the command"}],
        }),
    );

    let call_update = |message: &Value, kind: &str| {
        let update = &message["params"]["update"];
        message["method"] == "session/update"
            && update["sessionUpdate"] == kind
            && update["toolCallId"] == "call_1"
    };

    let announced = agent.wait_for("the tool call announcement", |message| {
        call_update(message, "tool_call")
    });

    let permission = agent.wait_for_agent_request("session/request_permission");
    agent.respond(
        permission["id"].clone(),
        json!({"outcome": {"outcome": "selected", "optionId": "reject_once"}}),
    );

    let finished = agent.wait_for("the denied call closing", |message| {
        call_update(message, "tool_call_update")
    });
    let finished = &finished["params"]["update"];
    assert_eq!(finished["status"], "failed", "{finished}");
    assert_eq!(
        finished["title"], announced["params"]["update"]["title"],
        "the card gets its own title back after the approval dialog: {finished}"
    );

    let response = agent.wait_for_response(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// "Deny for this session" answers the call it was picked for and every later
/// call in the same family without asking again (issue #199).
#[test]
fn a_call_denied_for_the_session_is_not_asked_about_again() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "run_command", r#"{"command":"echo sigit-never"}"#),
        sse_tool_call(
            "call_2",
            "run_command",
            r#"{"command":"echo sigit-never again"}"#,
        ),
        sse_text("ok, I will stop"),
    ]);

    let scratch =
        std::env::temp_dir().join(format!("sigit_acp_perm_deny_all_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the command"}],
        }),
    );

    let permission = agent.wait_for_agent_request("session/request_permission");
    agent.respond(
        permission["id"].clone(),
        json!({"outcome": {"outcome": "selected", "optionId": "reject_session"}}),
    );

    // The second call is denied by the recorded choice: no second request,
    // which `wait_for_response_with_updates` would fail on.
    let (response, updates) = agent.wait_for_response_with_updates(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");
    let second = updates
        .iter()
        .rev()
        .find(|update| {
            update["sessionUpdate"] == "tool_call_update" && update["toolCallId"] == "call_2"
        })
        .expect("the second call closes");
    assert_eq!(second["status"], "failed", "{second}");

    let requests = endpoint.requests.lock().unwrap();
    let result = requests[2]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_2")
        .expect("tool result for the second call");
    assert!(
        result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("always deny"),
        "the model is told why: {result}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A client that renders boolean config options gets the Inference switch as
/// a toggle, and can flip it with a boolean value; any other client keeps the
/// two-value select (issue #198).
#[test]
fn the_inference_switch_is_a_toggle_for_clients_that_render_one() {
    let inference_option = |options: &Value| -> Value {
        options
            .as_array()
            .expect("config options")
            .iter()
            .find(|option| option["id"] == "sigit-local-inference")
            .cloned()
            .expect("inference option")
    };

    for (capabilities, expected_type) in [
        (
            json!({"session": {"configOptions": {"boolean": {}}}}),
            "boolean",
        ),
        (json!({}), "select"),
    ] {
        let endpoint = start_fake_endpoint(vec![]);
        let scratch = std::env::temp_dir().join(format!(
            "sigit_acp_bool_option_{expected_type}_{}",
            std::process::id()
        ));
        let config_dir = scratch.join("config");
        let cwd = scratch.join("cwd");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let mut agent = spawn_agent(endpoint.port, &config_dir);
        let id = agent.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": capabilities}),
        );
        agent.wait_for_response(id);

        let id = agent.request("session/new", json!({"cwd": cwd, "mcpServers": []}));
        let created = agent.wait_for_response(id);
        let session_id = created["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string();
        let option = inference_option(&created["result"]["configOptions"]);
        assert_eq!(option["type"], expected_type, "{option}");

        if expected_type == "boolean" {
            let id = agent.request(
                "session/set_config_option",
                json!({
                    "sessionId": session_id,
                    "configId": "sigit-local-inference",
                    "type": "boolean",
                    "value": true,
                }),
            );
            let response = agent.wait_for_response(id);
            let option = inference_option(&response["result"]["configOptions"]);
            assert_eq!(option["currentValue"], true, "{option}");
        }

        drop(agent);
        let _ = std::fs::remove_dir_all(&scratch);
    }
}

#[test]
fn successful_config_option_changes_are_rendered_as_system_status_cards() {
    let endpoint = start_fake_endpoint(vec![]);

    let scratch =
        std::env::temp_dir().join(format!("sigit_acp_config_messages_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let cwd = scratch.join("cwd");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        config_dir.join("credentials.toml"),
        "access_token = \"tok_test\"\nemail = \"dev@sigit.si\"\n",
    )
    .unwrap();

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

    let id = agent.request(
        "session/set_config_option",
        json!({
            "sessionId": session_id,
            "configId": "sigit-local-inference",
            "value": "local-inference-off",
        }),
    );
    let (response, updates) = agent.wait_for_response_with_updates(id);
    assert!(
        response["result"]["configOptions"].is_array(),
        "config response should refresh picker options: {response}"
    );
    assert!(
        !updates
            .iter()
            .any(|update| update["sessionUpdate"] == "agent_message_chunk"),
        "Local Inference picker changes are passive UI state, not chat text: {updates:?}"
    );
    let status = updates
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "tool_call"
                && update["kind"] == "think"
                && update["status"] == "completed"
        })
        .expect("Local Inference change should render as a system-style status card");
    assert_eq!(
        status["title"],
        "Local inference is off. siGit Code Cloud tiers are highlighted; pick one from Model."
    );

    let id = agent.request(
        "session/set_config_option",
        json!({
            "sessionId": session_id,
            "configId": "sigit-model",
            "value": "sigit-cloud:mini",
        }),
    );
    let (response, updates) = agent.wait_for_response_with_updates(id);
    assert!(
        response["result"]["configOptions"].is_array(),
        "config response should refresh picker options: {response}"
    );
    assert!(
        !updates
            .iter()
            .any(|update| update["sessionUpdate"] == "agent_message_chunk"),
        "successful model picker changes must not be rendered as assistant chat text: {updates:?}"
    );
    let status = updates
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "tool_call"
                && update["kind"] == "think"
                && update["status"] == "completed"
        })
        .expect("cloud tier change should render as a system-style status card");
    assert!(
        status["title"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Switched to siGit Code Cloud"),
        "unexpected cloud switch status title: {status}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn auto_permission_mode_runs_mutating_tools_without_asking() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "run_command", r#"{"command":"echo sigit-auto"}"#),
        sse_text("done"),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_auto_perm_{}", std::process::id()));
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

    let id = agent.request(
        "session/set_config_option",
        json!({
            "sessionId": session_id,
            "configId": "sigit-permission-mode",
            "value": "permission-mode-auto",
        }),
    );
    let (response, updates) = agent.wait_for_response_with_updates(id);
    let permissions = response["result"]["configOptions"]
        .as_array()
        .expect("config options")
        .iter()
        .find(|option| option["id"] == "sigit-permission-mode")
        .expect("permissions config option");
    assert_eq!(permissions["currentValue"], "permission-mode-auto");
    assert!(
        !updates
            .iter()
            .any(|update| update["sessionUpdate"] == "agent_message_chunk"),
        "Permissions picker changes are passive UI state, not chat text: {updates:?}"
    );

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the command"}],
        }),
    );
    let (response, _updates) = agent.wait_for_response_with_updates(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "expected tool round and final round");
    let messages = requests[1]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_1")
        .expect("tool result for the automatic call");
    assert!(
        result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("sigit-auto"),
        "automatic command output should reach the endpoint: {result}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn plan_permission_mode_denies_mutating_tools_without_asking() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "run_command", r#"{"command":"echo sigit-plan"}"#),
        sse_text("planned"),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_plan_perm_{}", std::process::id()));
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

    let id = agent.request(
        "session/set_config_option",
        json!({
            "sessionId": session_id,
            "configId": "sigit-permission-mode",
            "value": "permission-mode-plan",
        }),
    );
    let (response, _updates) = agent.wait_for_response_with_updates(id);
    let permissions = response["result"]["configOptions"]
        .as_array()
        .expect("config options")
        .iter()
        .find(|option| option["id"] == "sigit-permission-mode")
        .expect("permissions config option");
    assert_eq!(permissions["currentValue"], "permission-mode-plan");

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the command"}],
        }),
    );
    let (response, updates) = agent.wait_for_response_with_updates(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    // The tool never ran, so its card must not end as a success (issue #139).
    let closed = updates
        .iter()
        .rev()
        .find(|update| {
            update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == "call_1"
                && !update["status"].is_null()
        })
        .expect("the blocked call is closed out");
    assert_eq!(closed["status"], "failed", "{closed}");

    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "expected denied tool round and final round"
    );
    let messages = requests[1]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_1")
        .expect("tool result for the blocked call");
    let content = result["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("Plan mode is active") && content.contains("was not executed"),
        "plan mode should return an instructive denial to the endpoint: {result}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_slow_commands_progress_reaches_the_client_while_it_runs() {
    let scratch = std::env::temp_dir().join(format!("sigit_acp_progress_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let cwd = scratch.join("cwd");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    // The command has to run for a few seconds through `sh -c` / `cmd /C`.
    // `sleep` is neither a cmd builtin nor on a Windows runner's PATH (Git's
    // usr\bin, which carries the MSYS coreutils, is deliberately left off it),
    // and the Windows job hung on it rather than failing fast. `ping` ships in
    // System32, waits a second between echoes, and never reads stdin.
    //
    // Four seconds against the one-second assertion below is deliberate
    // padding. A busy runner only ever widens the gap being measured — it
    // cannot make the command finish sooner — so the one way a correct agent
    // could fail here is this thread being descheduled for seconds between
    // reading the two updates off the channel. Three seconds of slack covers
    // that without weakening the assertion, which is separating four seconds
    // from the 164µs the blocking version produced.
    #[cfg(unix)]
    let command = "sleep 4";
    #[cfg(windows)]
    let command = "ping -n 5 127.0.0.1";

    // `SIGIT_PERMISSIONS=allow` skips the gate, so the only thing between the
    // two updates below is the command itself.
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "run_command",
            &json!({"command": command, "cwd": cwd}).to_string(),
        ),
        sse_text("done"),
    ]);

    let mut agent = spawn_agent_with_permissions(endpoint.port, &config_dir, Some("allow"));

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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the slow command"}],
        }),
    );

    let started = agent.wait_for_update("the tool call starting", |update| {
        update["sessionUpdate"] == "tool_call" && update["toolCallId"] == "call_1"
    });
    let in_progress_at = Instant::now();
    assert_eq!(
        started["status"], "in_progress",
        "a running tool call must be announced as in_progress: {started}"
    );

    let finished = agent.wait_for_update("the tool call finishing", |update| {
        update["sessionUpdate"] == "tool_call_update"
            && update["toolCallId"] == "call_1"
            && update["status"] == "completed"
    });

    // The command runs between the two updates. If the turn blocked the
    // connection's actors, both would only reach the client once it had
    // already finished, and the editor would never draw a spinner.
    assert!(
        in_progress_at.elapsed() >= Duration::from_millis(1_000),
        "in_progress arrived only {:?} before completed — the client had no \
         window to show progress in. `{command}` said: {}",
        in_progress_at.elapsed(),
        finished["rawOutput"]
    );

    let response = agent.wait_for_response(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn text_from_two_tool_rounds_reaches_the_client_as_separate_paragraphs() {
    let endpoint = start_fake_endpoint(vec![
        // `list_directory` is read-only, so it runs without a permission gate
        // and the two rounds follow each other straight away.
        sse_text_then_tool_call(
            "I'll search for this pattern.",
            "call_1",
            "list_directory",
            r#"{"path":"."}"#,
        ),
        sse_text("Let me broaden the search:"),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_spacing_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "find it"}],
        }),
    );
    let (response, rendered) = agent.wait_for_prompt(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    assert!(
        rendered.contains("pattern.\n\nLet me broaden"),
        "the second round must open a new paragraph, got: {rendered:?}"
    );
    assert!(
        !rendered.contains("pattern.Let me"),
        "rounds must not run together into one sentence, got: {rendered:?}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Every chunk of one model message carries the same `messageId`, its
/// reasoning included, and the message after a tool round gets a new one
/// (issue #195).
#[test]
fn chunks_of_one_model_message_share_a_message_id() {
    let endpoint = start_fake_endpoint(vec![
        sse_body(&[
            json!({"choices": [{"delta": {"reasoning_content": "Where is it?"}}]}),
            json!({"choices": [{"delta": {"content": "I'll look "}}]}),
            json!({"choices": [{"delta": {"content": "around."}}]}),
            json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {"name": "list_directory", "arguments": r#"{"path":"."}"#},
                }]}}]
            }),
        ]),
        sse_text("Found it."),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_msg_ids_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "find it"}],
        }),
    );
    let (_, updates) = agent.wait_for_response_with_updates(prompt_id);

    let chunks: Vec<(&str, &str)> = updates
        .iter()
        .filter(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                || update["sessionUpdate"] == "agent_thought_chunk"
        })
        .map(|update| {
            (
                update["content"]["text"].as_str().unwrap_or_default(),
                update["messageId"]
                    .as_str()
                    .unwrap_or_else(|| panic!("chunk without a messageId: {update}")),
            )
        })
        .collect();
    let id_of = |text: &str| {
        chunks
            .iter()
            .find(|(chunk, _)| chunk.contains(text))
            .unwrap_or_else(|| panic!("no chunk with {text:?} in {chunks:?}"))
            .1
    };

    let first = id_of("Where is it?");
    assert_eq!(id_of("I'll look"), first, "{chunks:?}");
    assert_eq!(id_of("around."), first, "{chunks:?}");
    assert_ne!(
        id_of("Found it."),
        first,
        "the round after the tool call is a new message: {chunks:?}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// After each model response the client hears how much of the context window
/// the session uses: the endpoint's own count when it gives one, an estimate
/// when it does not (issue #194).
#[test]
fn each_model_response_reports_context_usage() {
    let endpoint = start_fake_endpoint(vec![
        // No usage on this one.
        sse_tool_call("call_1", "list_directory", r#"{"path":"."}"#),
        sse_body(&[
            json!({"choices": [{"delta": {"content": "Done."}}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 1500, "completion_tokens": 21}}),
        ]),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_usage_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "look around"}],
        }),
    );
    let (_, updates) = agent.wait_for_response_with_updates(prompt_id);

    let usage: Vec<&Value> = updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "usage_update")
        .collect();
    assert_eq!(usage.len(), 2, "one per model response: {usage:?}");
    let estimated = usage[0]["used"].as_u64().expect("used");
    assert!(estimated > 0, "an estimate stands in for a missing count");
    assert_eq!(usage[1]["used"], 1521, "the endpoint's own count wins");
    for update in &usage {
        assert!(update["size"].as_u64().unwrap_or_default() > 0, "{update}");
    }

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A model that writes its tool call out as literal `<tool_call>` text instead
/// of using the structured field must still drive the loop. Before recovery
/// the tag was streamed to the client as prose and the turn ended with no tool
/// calls, so the request silently went unanswered (getsigit/sigit#73).
#[test]
fn a_tool_call_emitted_as_text_is_executed_rather_than_rendered() {
    let endpoint = start_fake_endpoint(vec![
        // Split mid-tag, the way a real stream arrives.
        sse_body(&[
            json!({"choices": [{"delta": {"content": "Checking the repo: <tool_c"}}]}),
            json!({"choices": [{"delta": {"content": "all>command_output<arg_key>task_id</arg_key>"}}]}),
            json!({"choices": [{"delta": {"content": "<arg_value>2</arg_value></tool_call>"}}]}),
        ]),
        sse_text("All done."),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_inline_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "check the repo"}],
        }),
    );
    let (_response, rendered) = agent.wait_for_prompt(prompt_id);

    // The raw tag must never be shown to the user.
    assert!(
        !rendered.contains("<tool_call>") && !rendered.contains("<arg_key>"),
        "raw tool-call markup reached the client: {rendered:?}"
    );
    assert!(
        rendered.contains("Checking the repo:"),
        "surrounding prose should still stream: {rendered:?}"
    );

    // The loop has to keep going: a second request means the recovered call
    // ran and its result was sent back. Before recovery the turn ended here.
    let requests = endpoint.requests.lock().unwrap();
    assert!(
        requests.len() >= 2,
        "expected a follow-up request carrying the tool result, got {}",
        requests.len()
    );
    let follow_up = &requests[1];
    let messages = follow_up["messages"].as_array().expect("messages");
    assert!(
        messages.iter().any(|message| message["role"] == "tool"),
        "the follow-up should answer the recovered call: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| { message["role"] == "assistant" && message["tool_calls"].is_array() }),
        "history must record the recovered call, not the raw tag: {messages:?}"
    );
    drop(requests);

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A tool-call block that can't be parsed must not reach the editor, and must
/// not stay in history where the model reads it back as a call it made and
/// starts inventing results (getsigit/sigit#97, #105). The model gets one
/// retry with a note that the call didn't run.
#[test]
fn an_unparseable_tool_call_is_hidden_and_retried() {
    let endpoint = start_fake_endpoint(vec![
        sse_body(&[
            json!({"choices": [{"delta": {"content": "Checking. <tool_call>command_output\n\n<invokeID>reply_A"}}]}),
            json!({"choices": [{"delta": {"content": "</invokeID>\n<parameter>6</parameter>\n</invoke>\n<function_results>all green"}}]}),
        ]),
        sse_tool_call(
            "call_1",
            "command_output",
            &json!({"task_id": 6}).to_string(),
        ),
        sse_text("All done."),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_malformed_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "is the build done?"}],
        }),
    );
    let (response, rendered) = agent.wait_for_prompt(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");
    for markup in [
        "<tool_call>",
        "<invokeID>",
        "<function_results>",
        "all green",
    ] {
        assert!(
            !rendered.contains(markup),
            "{markup} reached the client: {rendered:?}"
        );
    }
    assert!(rendered.contains("Checking."), "{rendered:?}");
    assert!(rendered.contains("All done."), "{rendered:?}");

    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "expected the retry, then the round answering the retried call"
    );
    let retry_messages = requests[1]["messages"].as_array().expect("messages");
    let last = retry_messages.last().expect("retry note");
    assert_eq!(last["role"], "user");
    assert!(
        last["content"]
            .as_str()
            .is_some_and(|content| content.starts_with("[siGit Code]")),
        "{last:?}"
    );
    for message in requests[2]["messages"].as_array().expect("messages") {
        let content = message["content"].as_str().unwrap_or_default();
        assert!(
            !content.contains("<tool_call>") && !content.contains("<invokeID>"),
            "broken markup was kept in history: {message:?}"
        );
    }
    drop(requests);

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The third identical call is skipped by the repetition guard. It never ran,
/// so its card ends `failed` while the two that did run end `completed`
/// (issue #139).
#[test]
fn a_call_skipped_by_the_repeat_guard_ends_failed() {
    let repeated_arguments = json!({"path": "notes.txt"}).to_string();
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "read_file", &repeated_arguments),
        sse_tool_call("call_2", "read_file", &repeated_arguments),
        sse_tool_call("call_3", "read_file", &repeated_arguments),
        sse_text("Stopping here."),
    ]);

    let scratch =
        std::env::temp_dir().join(format!("sigit_acp_repeat_failed_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let cwd = scratch.join("cwd");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("notes.txt"), "hello").unwrap();

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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "read the notes"}],
        }),
    );
    let (response, updates) = agent.wait_for_response_with_updates(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    let final_status = |call_id: &str| {
        updates
            .iter()
            .rev()
            .find(|update| {
                update["sessionUpdate"] == "tool_call_update"
                    && update["toolCallId"] == call_id
                    && !update["status"].is_null()
            })
            .unwrap_or_else(|| panic!("{call_id} is closed out: {updates:#?}"))["status"]
            .clone()
    };
    assert_eq!(final_status("call_1"), "completed");
    assert_eq!(final_status("call_2"), "completed");
    assert_eq!(final_status("call_3"), "failed");

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Once the repetition guard removes tools from a request, a model may still
/// emit its learned tool syntax. Known calls must be removed from the visible
/// response without being executed or recorded as orphaned history entries.
#[test]
fn forced_text_suppresses_glm_check_status_tool_markup() {
    let repeated_arguments = json!({"path": "missing.txt"}).to_string();
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "read_file", &repeated_arguments),
        sse_tool_call("call_2", "read_file", &repeated_arguments),
        sse_tool_call("call_3", "read_file", &repeated_arguments),
        sse_body(&[
            json!({"choices": [{"delta": {"content": "Build is still running. <tool_call>command_output CheckStatus=true_or_poll_"}}]}),
            json!({"choices": [{"delta": {"content": "again_with_different_params</arg_value><arg_key>task_id</arg_key><arg_value>1</arg_value></tool_call>"}}]}),
            // Also enforce the forced-text boundary for a structured call.
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_4",
                "function": {"name": "command_output", "arguments": "{\"task_id\":999}"},
            }]}}]}),
        ]),
        sse_text("Next turn."),
    ]);

    let scratch =
        std::env::temp_dir().join(format!("sigit_acp_forced_text_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "keep polling"}],
        }),
    );
    let (response, rendered) = agent.wait_for_prompt(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert!(
        rendered.contains("Build is still running."),
        "surrounding prose should remain visible: {rendered:?}"
    );
    assert!(
        !rendered.contains("<tool_call>")
            && !rendered.contains("<arg_key>")
            && !rendered.contains("CheckStatus"),
        "forced tool markup reached the client: {rendered:?}"
    );

    {
        let requests = endpoint.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            4,
            "a forced-text tool call must not start another inference round"
        );
        assert!(
            requests[3].get("tools").is_none(),
            "the repetition guard must not advertise tools: {:?}",
            requests[3]
        );
    }

    // Start another turn so the prior assistant message is replayed and its
    // sanitized history shape can be inspected in the recorded request.
    let next_prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "what happened?"}],
        }),
    );
    agent.wait_for_prompt(next_prompt_id);

    let requests = endpoint.requests.lock().unwrap();
    let replayed_messages = requests[4]["messages"].as_array().expect("messages");
    let prior_assistant = replayed_messages
        .iter()
        .rev()
        .find(|message| {
            message["role"] == "assistant"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("Build is still running."))
        })
        .expect("sanitized forced-text assistant response");
    assert!(prior_assistant.get("tool_calls").is_none());
    assert!(
        !prior_assistant["content"]
            .as_str()
            .expect("assistant content")
            .contains("<tool_call>")
    );
    drop(requests);

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Kimi K3 may emit tool calls in XTML content blocks even when the endpoint is
/// OpenAI-compatible. The backend should recover those before ACP sees them,
/// unwrap visible response text, and hide private thinking text.
#[test]
fn a_kimi_k3_tool_call_emitted_as_text_is_executed_rather_than_rendered() {
    let endpoint = start_fake_endpoint(vec![
        // Split across frames to cover the streaming scanner, not just the
        // complete-response extractor.
        sse_body(&[
            json!({"choices": [{"delta": {"content": "<|open|>think<|sep|>private notes<|close|>think<|sep|>"}}]}),
            json!({"choices": [{"delta": {"content": "<|open|>response<|sep|>Checking the repo:<|close|>response<|sep|> <|open|>too"}}]}),
            json!({"choices": [{"delta": {"content": "ls<|sep|><|open|>call tool=\"command_output\" index=\"1\"<|sep|>"}}]}),
            json!({"choices": [{"delta": {"content": "<|open|>argument key=\"task_id\" type=\"integer\"<|sep|>2<|close|>argument<|sep|><|close|>call<|sep|><|close|>tools<|sep|>"}}]}),
        ]),
        sse_text("All done."),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_kimi_k3_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "check the repo"}],
        }),
    );
    let (_response, rendered) = agent.wait_for_prompt(prompt_id);

    assert!(
        !rendered.contains("<|open|>")
            && !rendered.contains("<|sep|>")
            && !rendered.contains("private notes"),
        "raw Kimi K3 markup reached the client: {rendered:?}"
    );
    assert!(
        rendered.contains("Checking the repo:"),
        "response text should still be shown: {rendered:?}"
    );

    let requests = endpoint.requests.lock().unwrap();
    assert!(
        requests.len() >= 2,
        "expected a follow-up request carrying the tool result, got {}",
        requests.len()
    );
    let messages = requests[1]["messages"].as_array().expect("messages");
    assert!(
        messages.iter().any(|message| message["role"] == "tool"),
        "the follow-up should answer the recovered call: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| { message["role"] == "assistant" && message["tool_calls"].is_array() }),
        "history must record the recovered call, not the raw tag: {messages:?}"
    );
    drop(requests);

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_silent_forced_text_round_still_ends_with_a_message() {
    let repeated_arguments = json!({"path": "missing.txt"}).to_string();
    let endpoint = start_fake_endpoint(vec![
        sse_text_then_tool_call(
            "Checking the file.",
            "call_1",
            "read_file",
            &repeated_arguments,
        ),
        sse_tool_call("call_2", "read_file", &repeated_arguments),
        sse_tool_call("call_3", "read_file", &repeated_arguments),
        // The forced no-tools round says nothing, as in the stalled thread.
        sse_body(&[]),
    ]);

    let scratch =
        std::env::temp_dir().join(format!("sigit_acp_silent_stop_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "read it"}],
        }),
    );
    let (response, rendered) = agent.wait_for_prompt(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert!(rendered.contains("Checking the file."), "{rendered:?}");
    assert!(
        rendered.contains("same `read_file` call") && rendered.contains("continue"),
        "the turn ended without telling the user why: {rendered:?}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn repeated_command_output_calls_remain_available() {
    let repeated_arguments = json!({"task_id": 999}).to_string();
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "command_output", &repeated_arguments),
        sse_tool_call("call_2", "command_output", &repeated_arguments),
        sse_tool_call("call_3", "command_output", &repeated_arguments),
        sse_text("The background task finished."),
    ]);

    let scratch =
        std::env::temp_dir().join(format!("sigit_acp_repeated_poll_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "wait for the background task"}],
        }),
    );
    let (response, rendered) = agent.wait_for_prompt(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert!(rendered.contains("finished"), "got: {rendered:?}");

    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[3].get("tools").is_some(),
        "command_output repetition must not force tools off: {:?}",
        requests[3]
    );
    drop(requests);

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn write_todos_reaches_the_client_as_an_acp_plan() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "write_todos",
            &json!({
                "todos": [
                    {"content": "Fetch issue screenshot", "status": "completed"},
                    {"content": "Implement ACP styling", "status": "in_progress"},
                    {"content": "Build and verify", "status": "pending"}
                ]
            })
            .to_string(),
        ),
        sse_text("Continuing."),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_plan_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "fix the ACP display"}],
        }),
    );
    let (response, updates) = agent.wait_for_response_with_updates(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    let plan = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "plan")
        .expect("write_todos should be rendered as an ACP plan update");
    assert_eq!(plan["entries"][0]["content"], "Fetch issue screenshot");
    assert_eq!(plan["entries"][0]["status"], "completed");
    assert_eq!(plan["entries"][1]["status"], "in_progress");
    assert_eq!(plan["entries"][2]["status"], "pending");

    assert!(
        !updates.iter().any(|update| {
            update["sessionUpdate"] == "tool_call" && update["title"] == "write_todos"
        }),
        "write_todos must not show up as a generic tool-call card: {updates:?}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// An empty `write_todos` is how the model clears a finished checklist. It has
/// to reach the client as a plan with no entries, since ACP plan updates
/// replace the whole plan, rather than as an error card that leaves the old
/// list on screen.
#[test]
fn an_empty_write_todos_clears_the_acp_plan() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "write_todos", &json!({"todos": []}).to_string()),
        sse_text("Checklist cleared."),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_plan_clear_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "clear the checklist"}],
        }),
    );
    let (response, updates) = agent.wait_for_response_with_updates(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    let plan = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "plan")
        .expect("an empty write_todos should still send a plan update");
    assert_eq!(plan["entries"], json!([]));
    assert!(
        !updates
            .iter()
            .any(|update| update["sessionUpdate"] == "tool_call"),
        "clearing the plan must not show a tool-call card: {updates:?}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// An off-enum status must not make the whole call vanish. `exec_write_todos`
/// renders it as pending and reports success, so the plan has to carry it the
/// same way; dropping the plan here would leave the client showing a stale list.
#[test]
fn write_todos_with_an_unknown_status_still_reaches_the_client() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "write_todos",
            &json!({
                "todos": [
                    {"content": "Fetch issue screenshot", "status": "completed"},
                    {"content": "Ship it", "status": "cancelled"}
                ]
            })
            .to_string(),
        ),
        sse_text("Continuing."),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_plan_odd_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "plan the work"}],
        }),
    );
    let (_response, updates) = agent.wait_for_response_with_updates(prompt_id);

    let plan = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "plan")
        .expect("an unknown status must not drop the plan");
    assert_eq!(plan["entries"][1]["content"], "Ship it");
    assert_eq!(plan["entries"][1]["status"], "pending");

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Arguments the converter cannot turn into a plan fall back to the ordinary
/// tool-call card. Without that fallback the call is announced nowhere at all.
#[test]
fn unconvertible_write_todos_falls_back_to_a_tool_call_card() {
    let endpoint = start_fake_endpoint(vec![
        // `todos` is absent, so there is no plan to build. `exec_write_todos`
        // answers with an error string, and the client still has to see the call.
        sse_tool_call("call_1", "write_todos", &json!({"items": []}).to_string()),
        sse_text("Continuing."),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_plan_bad_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "plan the work"}],
        }),
    );
    let (_response, updates) = agent.wait_for_response_with_updates(prompt_id);

    assert!(
        !updates
            .iter()
            .any(|update| update["sessionUpdate"] == "plan"),
        "there is no plan to send: {updates:?}"
    );
    assert!(
        updates.iter().any(|update| {
            update["sessionUpdate"] == "tool_call" && update["title"] == "write_todos"
        }),
        "the call must still be announced as a tool call: {updates:?}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

// ── Session modes (issue #148) ──────────────────────────────────────────────

/// The Permissions dropdown is also offered as ACP session modes, for a client
/// that draws the mode selector and not config options. Both are the same
/// state, so a change made through either has to show up in the other.
#[test]
fn session_modes_mirror_the_permissions_config_option() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "run_command", "{\"command\":\"echo sigit-mode\"}"),
        sse_text("planned"),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_modes_{}", std::process::id()));
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
    let response = agent.wait_for_response(id);
    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let modes = &response["result"]["modes"];
    assert_eq!(modes["currentModeId"], "permission-mode-manual");
    let mode_ids: Vec<&str> = modes["availableModes"]
        .as_array()
        .expect("available modes")
        .iter()
        .filter_map(|mode| mode["id"].as_str())
        .collect();
    assert_eq!(
        mode_ids,
        [
            "permission-mode-manual",
            "permission-mode-auto",
            "permission-mode-plan"
        ]
    );

    // set_mode changes the session and refreshes the config option.
    let id = agent.request(
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": "permission-mode-plan"}),
    );
    let (_response, updates) = agent.wait_for_response_with_updates(id);
    let refreshed = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "config_option_update")
        .expect("set_mode refreshes the config options");
    let permissions = refreshed["configOptions"]
        .as_array()
        .expect("config options")
        .iter()
        .find(|option| option["id"] == "sigit-permission-mode")
        .expect("permissions config option");
    assert_eq!(permissions["currentValue"], "permission-mode-plan");

    // Plan mode is really on: the mutating call is denied, not asked about.
    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run the command"}],
        }),
    );
    let (response, _updates) = agent.wait_for_response_with_updates(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");
    {
        let requests = endpoint.requests.lock().unwrap();
        let messages = requests[1]["messages"].as_array().expect("messages");
        let result = messages
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_1")
            .expect("tool result for the blocked call");
        assert!(
            !result["content"]
                .as_str()
                .unwrap_or_default()
                .contains("sigit-mode"),
            "plan mode must not run the command: {result}"
        );
    }

    // The config option changes the mode, and says so to the mode selector.
    let id = agent.request(
        "session/set_config_option",
        json!({
            "sessionId": session_id,
            "configId": "sigit-permission-mode",
            "value": "permission-mode-auto",
        }),
    );
    let (_response, updates) = agent.wait_for_response_with_updates(id);
    let current = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "current_mode_update")
        .expect("a config option change announces the new mode");
    assert_eq!(current["currentModeId"], "permission-mode-auto");

    // So does /plan.
    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "/plan on"}],
        }),
    );
    let (_response, updates) = agent.wait_for_response_with_updates(prompt_id);
    let current = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "current_mode_update")
        .expect("/plan announces the new mode");
    assert_eq!(current["currentModeId"], "permission-mode-plan");

    // An id that is not a mode is an invalid-params error, and changes nothing.
    let id = agent.request(
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": "permission-mode-yolo"}),
    );
    let response = agent.wait_for("the set_mode error", |message| {
        message["id"] == id && message.get("method").is_none()
    });
    assert_eq!(response["error"]["code"], -32602, "{response}");

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A mode change is heard while a turn waits on a permission prompt, and the
/// call that follows obeys it. The turn holds its session's lock for its whole
/// length, so a change that queued behind it would sit unanswered until the
/// prompt had been dealt with.
#[test]
fn a_mode_switch_during_a_permission_prompt_is_answered_at_once() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "run_command", r#"{"command":"echo sigit-first"}"#),
        sse_tool_call(
            "call_2",
            "run_command",
            r#"{"command":"echo sigit-second"}"#,
        ),
        sse_text("done"),
    ]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_mode_mid_{}", std::process::id()));
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

    let prompt_id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "run both commands"}],
        }),
    );
    let permission = agent.wait_for_agent_request("session/request_permission");

    // The turn is parked on the question. Switch to Auto, and expect the
    // answer now, not after the turn.
    let mode_id = agent.request(
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": "permission-mode-auto"}),
    );
    let response = agent.wait_for(
        "the set_mode response while the permission prompt is open",
        |message| {
            message["id"] == mode_id && message.get("method").is_none()
                || message["id"] == prompt_id && message.get("method").is_none()
        },
    );
    assert_eq!(
        response["id"], mode_id,
        "the turn ended before set_mode was answered: {response}"
    );
    assert!(response.get("error").is_none(), "{response}");

    // Same for the config option, which is the path most editors use.
    let option_id = agent.request(
        "session/set_config_option",
        json!({
            "sessionId": session_id,
            "configId": "sigit-permission-mode",
            "value": "permission-mode-auto",
        }),
    );
    let response = agent.wait_for(
        "the set_config_option response while the permission prompt is open",
        |message| {
            message["id"] == option_id && message.get("method").is_none()
                || message["id"] == prompt_id && message.get("method").is_none()
        },
    );
    assert_eq!(response["id"], option_id, "{response}");
    assert!(response.get("error").is_none(), "{response}");

    // Allow the pending call. The next one is decided under Auto: no prompt.
    agent.respond(
        permission["id"].clone(),
        json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
    );
    let (response, _updates) = agent.wait_for_response_with_updates(prompt_id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    let requests = endpoint.requests.lock().unwrap();
    let second_result = requests[2]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_2")
        .expect("tool result for the second call");
    assert!(
        second_result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("sigit-second"),
        "the second call should have run under Auto: {second_result}"
    );

    drop(requests);
    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn a_mode_change_for_an_unknown_session_is_resource_not_found() {
    let endpoint = start_fake_endpoint(vec![]);
    let scratch =
        std::env::temp_dir().join(format!("sigit_acp_mode_unknown_{}", std::process::id()));
    let config_dir = scratch.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let mut agent = spawn_agent(endpoint.port, &config_dir);
    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    agent.wait_for_response(id);

    let id = agent.request(
        "session/set_mode",
        json!({"sessionId": "nope", "modeId": "permission-mode-auto"}),
    );
    let response = agent.wait_for("set_mode answer", |m| m["id"] == id);
    assert_eq!(response["error"]["code"], -32002, "{response}");

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}
