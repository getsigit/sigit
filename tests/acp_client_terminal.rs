//! `run_command` runs in the ACP client's terminal when it offers one.
//!
//! An editor that advertises `terminal` can run a command itself and show it
//! live in the tool call. These tests spawn `sigit` in ACP mode against a
//! scripted endpoint and play that editor: it answers `terminal/*` with
//! output the command could not have printed locally, so each assertion can
//! tell where the command ran.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
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

fn sse_tool_call(id: &str, name: &str, arguments: Value) -> String {
    sse_body(&[json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "function": {"name": name, "arguments": arguments.to_string()},
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

impl FakeEndpoint {
    /// The result the model was given for the tool call `id`.
    fn tool_result(&self, id: &str) -> String {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .flat_map(|request| request["messages"].as_array().cloned().unwrap_or_default())
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .and_then(|message| message["content"].as_str().map(str::to_string))
            .unwrap_or_else(|| panic!("no tool result for {id} reached the endpoint"))
    }
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
            let _ = stream.flush();
        }
    });

    FakeEndpoint { port, requests }
}

// ── ACP client over the binary's stdio ──────────────────────────────────────

/// What the editor's terminal does, and what it was asked.
#[derive(Default)]
struct Editor {
    /// Answer `terminal/create` with an error, the way a client with
    /// terminals turned off would.
    refuse_create: bool,
    /// Every `terminal/*` request the agent sent, in order.
    requests: Vec<Value>,
    /// Every `session/update` the agent sent, in order.
    updates: Vec<Value>,
}

impl Editor {
    fn methods(&self) -> Vec<&str> {
        self.requests
            .iter()
            .filter_map(|request| request["method"].as_str())
            .collect()
    }

    fn request(&self, method: &str) -> &Value {
        self.requests
            .iter()
            .find(|request| request["method"] == method)
            .unwrap_or_else(|| panic!("no {method} request in {:?}", self.requests))
    }

    /// The `tool_call_update`s for one tool call, in order.
    fn call_updates(&self, tool_call_id: &str) -> Vec<&Value> {
        self.updates
            .iter()
            .map(|update| &update["params"]["update"])
            .filter(|update| {
                update["sessionUpdate"] == "tool_call_update"
                    && update["toolCallId"] == tool_call_id
            })
            .collect()
    }
}

/// What the editor's terminal printed, which no local run could.
const EDITOR_OUTPUT: &str = "printed in the editor\n";

struct AgentUnderTest {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
}

fn spawn_agent(port: u16, config_dir: &Path, client_terminal: Option<&str>) -> AgentUnderTest {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sigit"));
    command
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
        .env("SIGIT_CONFIG_DIR", config_dir)
        .env("SIGIT_MCP", "off")
        .env("SIGIT_PERMISSIONS", "allow")
        .env_remove("SIGIT_LOCAL_INFERENCE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    match client_terminal {
        None => command.env_remove("SIGIT_CLIENT_TERMINAL"),
        Some(value) => command.env("SIGIT_CLIENT_TERMINAL", value),
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
        self.stdin.write_all(line.as_bytes()).expect("write stdin");
        self.stdin.flush().expect("flush stdin");
    }

    fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Wait for the response to request `id`, serving the agent's
    /// `terminal/*` requests from `editor` on the way.
    fn wait_for_response(&mut self, id: u64, editor: &mut Editor) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.incoming.recv_timeout(remaining) else {
                panic!("timed out waiting for the response to request {id}");
            };
            if message["id"] == id && message.get("method").is_none() {
                assert!(
                    message.get("error").is_none(),
                    "request {id} failed: {message}"
                );
                return message;
            }
            let Some(method) = message["method"].as_str() else {
                continue;
            };
            if method == "session/update" {
                editor.updates.push(message.clone());
                continue;
            }
            if !method.starts_with("terminal/") {
                continue;
            }
            editor.requests.push(message.clone());
            let request_id = message["id"].clone();
            let result = match method {
                "terminal/create" if editor.refuse_create => {
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {"code": -32603, "message": "terminals are off"},
                    }));
                    continue;
                }
                "terminal/create" => json!({"terminalId": "term-1"}),
                "terminal/wait_for_exit" => json!({"exitCode": 0}),
                "terminal/output" => json!({
                    "output": EDITOR_OUTPUT,
                    "truncated": false,
                    "exitStatus": {"exitCode": 0},
                }),
                _ => json!({}),
            };
            self.send(json!({"jsonrpc": "2.0", "id": request_id, "result": result}));
        }
    }

    /// `initialize` with or without the `terminal` capability, then a session
    /// in `cwd`.
    fn open_session(&mut self, terminal: bool, cwd: &Path) -> String {
        let mut editor = Editor::default();
        let id = self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {"terminal": terminal}}),
        );
        self.wait_for_response(id, &mut editor);

        let id = self.request("session/new", json!({"cwd": cwd, "mcpServers": []}));
        self.wait_for_response(id, &mut editor)["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string()
    }

    /// Send a prompt, then `session/cancel` as soon as the agent starts
    /// waiting on the editor's terminal, which never answers. Returns the
    /// prompt's response.
    fn prompt_cancelled_mid_command(&mut self, session_id: &str, editor: &mut Editor) -> Value {
        let id = self.request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "go"}]}),
        );
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.incoming.recv_timeout(remaining) else {
                panic!("timed out: the cancel did not end the prompt");
            };
            if message["id"] == id && message.get("method").is_none() {
                return message;
            }
            let Some(method) = message["method"].as_str() else {
                continue;
            };
            if method == "session/update" {
                editor.updates.push(message.clone());
                continue;
            }
            if !method.starts_with("terminal/") {
                continue;
            }
            editor.requests.push(message.clone());
            let request_id = message["id"].clone();
            let result = match method {
                "terminal/create" => json!({"terminalId": "term-1"}),
                "terminal/wait_for_exit" => {
                    // The command is still running; the user presses stop.
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": {"sessionId": session_id},
                    }));
                    continue;
                }
                "terminal/output" => json!({
                    "output": "partial\n",
                    "truncated": false,
                }),
                _ => json!({}),
            };
            self.send(json!({"jsonrpc": "2.0", "id": request_id, "result": result}));
        }
    }

    fn prompt(&mut self, session_id: &str, editor: &mut Editor) {
        let id = self.request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "go"}]}),
        );
        self.wait_for_response(id, editor);
    }
}

impl Drop for AgentUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A scratch project: `config/` for the agent and `work/` as the session root.
struct Scratch {
    root: PathBuf,
    config: PathBuf,
    work: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("sigit_acp_terminal_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let config = root.join("config");
        let work = root.join("work");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Self { root, config, work }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

/// A `run_command` call printing a marker, in `cwd`. Quote-free so the same
/// command line works under `sh -c` and `cmd /C`.
fn echo_call(id: &str, cwd: &Path, background: bool) -> String {
    sse_tool_call(
        id,
        "run_command",
        json!({"command": "echo ran-locally", "cwd": cwd, "run_in_background": background}),
    )
}

// ── The tests ───────────────────────────────────────────────────────────────

#[test]
fn run_command_runs_in_the_client_terminal_and_shows_it() {
    let scratch = Scratch::new("terminal");
    let endpoint = start_fake_endpoint(vec![
        echo_call("call_1", &scratch.work, false),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(true, &scratch.work);

    let mut editor = Editor::default();
    agent.prompt(&session_id, &mut editor);

    assert_eq!(
        endpoint.tool_result("call_1"),
        format!("Exit code 0:\n{EDITOR_OUTPUT}")
    );
    assert_eq!(
        editor.methods(),
        [
            "terminal/create",
            "terminal/wait_for_exit",
            "terminal/output",
            "terminal/release",
        ]
    );

    let create = &editor.request("terminal/create")["params"];
    assert_eq!(create["sessionId"], session_id.as_str());
    #[cfg(unix)]
    assert_eq!(create["command"], "sh");
    #[cfg(windows)]
    assert_eq!(create["command"], "cmd");
    assert_eq!(create["args"][1], "echo ran-locally");
    let cwd = PathBuf::from(create["cwd"].as_str().unwrap());
    assert_eq!(
        cwd.canonicalize().unwrap(),
        scratch.work.canonicalize().unwrap()
    );
    for method in [
        "terminal/wait_for_exit",
        "terminal/output",
        "terminal/release",
    ] {
        assert_eq!(editor.request(method)["params"]["terminalId"], "term-1");
    }

    // The terminal is in the card while the command runs and after it ends.
    let terminal = json!([{"type": "terminal", "terminalId": "term-1"}]);
    let updates = editor.call_updates("call_1");
    let shown = updates
        .iter()
        .position(|update| update["content"] == terminal && update["status"].is_null())
        .unwrap_or_else(|| panic!("terminal never embedded: {updates:?}"));
    let finished = updates
        .iter()
        .position(|update| update["status"] == "completed")
        .expect("call_1 never completed");
    assert!(shown < finished, "embedded after completion: {updates:?}");
    assert_eq!(updates[finished]["content"], terminal);
}

#[test]
fn a_client_without_a_terminal_gets_the_command_run_locally() {
    let scratch = Scratch::new("noterminal");
    let endpoint = start_fake_endpoint(vec![
        echo_call("call_1", &scratch.work, false),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(false, &scratch.work);

    let mut editor = Editor::default();
    agent.prompt(&session_id, &mut editor);

    let result = endpoint.tool_result("call_1");
    assert!(result.contains("ran-locally"), "run_command said: {result}");
    assert!(editor.requests.is_empty(), "{:?}", editor.requests);
}

#[test]
fn a_background_command_stays_local() {
    let scratch = Scratch::new("background");
    let endpoint = start_fake_endpoint(vec![
        echo_call("call_1", &scratch.work, true),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(true, &scratch.work);

    let mut editor = Editor::default();
    agent.prompt(&session_id, &mut editor);

    let result = endpoint.tool_result("call_1");
    assert!(result.contains("task"), "run_command said: {result}");
    assert!(editor.requests.is_empty(), "{:?}", editor.requests);
}

#[test]
fn a_directory_outside_the_session_roots_stays_local() {
    let scratch = Scratch::new("outside");
    let outside = scratch.root.join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let endpoint =
        start_fake_endpoint(vec![echo_call("call_1", &outside, false), sse_text("done")]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(true, &scratch.work);

    let mut editor = Editor::default();
    agent.prompt(&session_id, &mut editor);

    let result = endpoint.tool_result("call_1");
    assert!(result.contains("ran-locally"), "run_command said: {result}");
    assert!(editor.requests.is_empty(), "{:?}", editor.requests);
}

#[test]
fn a_terminal_the_client_refuses_falls_back_to_running_locally() {
    let scratch = Scratch::new("refused");
    let endpoint = start_fake_endpoint(vec![
        echo_call("call_1", &scratch.work, false),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(true, &scratch.work);

    let mut editor = Editor {
        refuse_create: true,
        ..Editor::default()
    };
    agent.prompt(&session_id, &mut editor);

    let result = endpoint.tool_result("call_1");
    assert!(result.contains("ran-locally"), "run_command said: {result}");
    assert_eq!(editor.methods(), ["terminal/create"]);
}

#[test]
fn sigit_client_terminal_off_keeps_commands_local() {
    let scratch = Scratch::new("off");
    let endpoint = start_fake_endpoint(vec![
        echo_call("call_1", &scratch.work, false),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, Some("off"));
    let session_id = agent.open_session(true, &scratch.work);

    let mut editor = Editor::default();
    agent.prompt(&session_id, &mut editor);

    let result = endpoint.tool_result("call_1");
    assert!(result.contains("ran-locally"), "run_command said: {result}");
    assert!(editor.requests.is_empty(), "{:?}", editor.requests);
}

#[test]
fn a_cancel_mid_command_kills_the_client_terminal_and_ends_the_turn_cancelled() {
    let scratch = Scratch::new("cancel");
    let endpoint = start_fake_endpoint(vec![
        echo_call("call_1", &scratch.work, false),
        sse_text("never asked for"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(true, &scratch.work);

    let mut editor = Editor::default();
    let response = agent.prompt_cancelled_mid_command(&session_id, &mut editor);

    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(
        editor.methods(),
        [
            "terminal/create",
            "terminal/wait_for_exit",
            "terminal/kill",
            "terminal/output",
            "terminal/release",
        ]
    );
    assert_eq!(
        editor.request("terminal/kill")["params"]["terminalId"],
        "term-1"
    );
    assert_eq!(
        endpoint.requests.lock().unwrap().len(),
        1,
        "a cancelled turn must not ask the model for another round"
    );
}
