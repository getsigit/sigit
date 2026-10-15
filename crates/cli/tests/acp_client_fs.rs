//! The file tools go through the ACP client when it offers to serve files.
//!
//! An editor that advertises `fs.readTextFile` / `fs.writeTextFile` holds the
//! truth about an open file: the buffer, which may have changes the disk does
//! not. These tests spawn `sigit` in ACP mode against a scripted endpoint and
//! play that editor: they answer `fs/read_text_file` with text that differs
//! from the file on disk and record `fs/write_text_file` without touching the
//! disk, so each assertion can tell which of the two the tool used.

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

/// What the editor's buffers hold, and what it does with a file request.
#[derive(Default)]
struct Editor {
    /// Buffer text by file name. A read of anything else is an error, the way
    /// an editor answers for a file it cannot open.
    buffers: Vec<(&'static str, &'static str)>,
    /// Every `fs/*` request the agent sent, in order.
    requests: Vec<Value>,
}

impl Editor {
    fn requests_for(&self, method: &str) -> Vec<&Value> {
        self.requests
            .iter()
            .filter(|request| request["method"] == method)
            .collect()
    }
}

struct AgentUnderTest {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
}

fn spawn_agent(port: u16, config_dir: &Path, client_fs: Option<&str>) -> AgentUnderTest {
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
    match client_fs {
        None => command.env_remove("SIGIT_CLIENT_FS"),
        Some(value) => command.env("SIGIT_CLIENT_FS", value),
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

    /// Wait for the response to request `id`, serving the agent's `fs/*`
    /// requests from `editor` on the way.
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
            if !method.starts_with("fs/") {
                continue;
            }
            editor.requests.push(message.clone());
            let request_id = message["id"].clone();
            let path = message["params"]["path"].as_str().unwrap_or_default();
            let name = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let buffer = editor.buffers.iter().find(|(file, _)| *file == name);
            let reply = match (method, buffer) {
                ("fs/read_text_file", Some((_, text))) => {
                    json!({"jsonrpc": "2.0", "id": request_id, "result": {"content": text}})
                }
                ("fs/read_text_file", None) => json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32002, "message": "no such buffer"},
                }),
                // A write is recorded and deliberately not applied to the disk.
                _ => json!({"jsonrpc": "2.0", "id": request_id, "result": {}}),
            };
            self.send(reply);
        }
    }

    /// `initialize` with the given `fs` capabilities, then a session in `cwd`.
    fn open_session(&mut self, fs: Value, cwd: &Path) -> String {
        let mut editor = Editor::default();
        let id = self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {"fs": fs}}),
        );
        self.wait_for_response(id, &mut editor);

        let id = self.request("session/new", json!({"cwd": cwd, "mcpServers": []}));
        self.wait_for_response(id, &mut editor)["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string()
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
        let root = std::env::temp_dir().join(format!("sigit_acp_fs_{name}_{}", std::process::id()));
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

const BOTH: fn() -> Value = || json!({"readTextFile": true, "writeTextFile": true});

// ── The tests ───────────────────────────────────────────────────────────────

#[test]
fn read_file_sees_the_unsaved_buffer() {
    let scratch = Scratch::new("read");
    let file = scratch.work.join("notes.txt");
    std::fs::write(&file, "saved text").unwrap();

    let endpoint = start_fake_endpoint(vec![
        // A relative path: the request to the client still has to be absolute.
        sse_tool_call("call_1", "read_file", json!({"path": "notes.txt"})),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(BOTH(), &scratch.work);

    let mut editor = Editor {
        buffers: vec![("notes.txt", "unsaved text")],
        ..Editor::default()
    };
    agent.prompt(&session_id, &mut editor);

    assert_eq!(endpoint.tool_result("call_1"), "unsaved text");

    let reads = editor.requests_for("fs/read_text_file");
    assert_eq!(reads.len(), 1, "requests: {:?}", editor.requests);
    assert_eq!(reads[0]["params"]["sessionId"], session_id.as_str());
    let sent = Path::new(reads[0]["params"]["path"].as_str().unwrap());
    assert!(sent.is_absolute(), "path sent to the client: {sent:?}");
    assert!(sent.ends_with("notes.txt"));
}

#[test]
fn edit_file_edits_the_buffer_and_writes_through_the_client() {
    let scratch = Scratch::new("edit");
    let file = scratch.work.join("main.rs");
    std::fs::write(&file, "fn saved() {}\n").unwrap();

    let endpoint = start_fake_endpoint(vec![
        // `unsaved` exists only in the buffer, so the edit can only match
        // if the tool read the client's copy.
        sse_tool_call(
            "call_1",
            "edit_file",
            json!({"path": file, "old_text": "unsaved", "new_text": "edited"}),
        ),
        sse_tool_call(
            "call_2",
            "multi_edit",
            json!({"path": file, "edits": [
                {"old_text": "fn ", "new_text": "pub fn "},
                {"old_text": "{}", "new_text": "{ }"},
            ]}),
        ),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(BOTH(), &scratch.work);

    let mut editor = Editor {
        buffers: vec![("main.rs", "fn unsaved() {}\n")],
        ..Editor::default()
    };
    agent.prompt(&session_id, &mut editor);

    let first = endpoint.tool_result("call_1");
    assert!(first.starts_with("Edited file:"), "edit_file said: {first}");
    let second = endpoint.tool_result("call_2");
    assert!(
        second.starts_with("Applied 2 edits"),
        "multi_edit said: {second}"
    );

    let writes = editor.requests_for("fs/write_text_file");
    assert_eq!(writes.len(), 2, "requests: {:?}", editor.requests);
    assert_eq!(writes[0]["params"]["sessionId"], session_id.as_str());
    assert_eq!(writes[0]["params"]["content"], "fn edited() {}\n");
    assert_eq!(writes[1]["params"]["content"], "pub fn unsaved() { }\n");

    // The editor owns the write; sigit must not also have written the disk.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn saved() {}\n");
}

#[test]
fn create_file_writes_through_the_client() {
    let scratch = Scratch::new("create");
    let file = scratch.work.join("src").join("new.txt");

    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "create_file",
            json!({"path": file, "content": "hello"}),
        ),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(BOTH(), &scratch.work);

    let mut editor = Editor::default();
    agent.prompt(&session_id, &mut editor);

    let result = endpoint.tool_result("call_1");
    assert!(
        result.starts_with("Created file:"),
        "create_file said: {result}"
    );
    let writes = editor.requests_for("fs/write_text_file");
    assert_eq!(writes.len(), 1, "requests: {:?}", editor.requests);
    assert_eq!(writes[0]["params"]["content"], "hello");
    assert!(!file.exists(), "sigit wrote the file itself");
}

#[test]
fn a_client_without_fs_capabilities_gets_no_fs_requests() {
    let scratch = Scratch::new("nocaps");
    let file = scratch.work.join("notes.txt");
    std::fs::write(&file, "saved text").unwrap();

    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "read_file", json!({"path": file})),
        sse_tool_call(
            "call_2",
            "edit_file",
            json!({"path": file, "old_text": "saved", "new_text": "edited"}),
        ),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(json!({}), &scratch.work);

    let mut editor = Editor {
        buffers: vec![("notes.txt", "unsaved text")],
        ..Editor::default()
    };
    agent.prompt(&session_id, &mut editor);

    assert_eq!(endpoint.tool_result("call_1"), "saved text");
    assert!(editor.requests.is_empty(), "{:?}", editor.requests);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited text");
}

#[test]
fn only_the_advertised_half_goes_through_the_client() {
    let scratch = Scratch::new("readonly");
    let file = scratch.work.join("notes.txt");
    std::fs::write(&file, "saved text").unwrap();

    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "edit_file",
            json!({"path": file, "old_text": "unsaved", "new_text": "edited"}),
        ),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(json!({"readTextFile": true}), &scratch.work);

    let mut editor = Editor {
        buffers: vec![("notes.txt", "unsaved text")],
        ..Editor::default()
    };
    agent.prompt(&session_id, &mut editor);

    assert_eq!(editor.requests_for("fs/read_text_file").len(), 1);
    assert!(editor.requests_for("fs/write_text_file").is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited text");
}

#[test]
fn a_path_outside_the_session_roots_stays_on_disk() {
    let scratch = Scratch::new("outside");
    let outside = scratch.root.join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let file = outside.join("notes.txt");
    std::fs::write(&file, "saved text").unwrap();

    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "read_file", json!({"path": file})),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(BOTH(), &scratch.work);

    let mut editor = Editor {
        buffers: vec![("notes.txt", "unsaved text")],
        ..Editor::default()
    };
    agent.prompt(&session_id, &mut editor);

    assert_eq!(endpoint.tool_result("call_1"), "saved text");
    assert!(editor.requests.is_empty(), "{:?}", editor.requests);
}

#[test]
fn a_read_the_client_refuses_falls_back_to_disk() {
    let scratch = Scratch::new("fallback");
    let file = scratch.work.join("notes.txt");
    std::fs::write(&file, "saved text").unwrap();

    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "read_file", json!({"path": file})),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, None);
    let session_id = agent.open_session(BOTH(), &scratch.work);

    // No buffers: the editor answers the read with an error.
    let mut editor = Editor::default();
    agent.prompt(&session_id, &mut editor);

    assert_eq!(editor.requests_for("fs/read_text_file").len(), 1);
    assert_eq!(endpoint.tool_result("call_1"), "saved text");
}

#[test]
fn sigit_client_fs_off_keeps_the_tools_on_disk() {
    let scratch = Scratch::new("off");
    let file = scratch.work.join("notes.txt");
    std::fs::write(&file, "saved text").unwrap();

    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "read_file", json!({"path": file})),
        sse_text("done"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &scratch.config, Some("off"));
    let session_id = agent.open_session(BOTH(), &scratch.work);

    let mut editor = Editor {
        buffers: vec![("notes.txt", "unsaved text")],
        ..Editor::default()
    };
    agent.prompt(&session_id, &mut editor);

    assert_eq!(endpoint.tool_result("call_1"), "saved text");
    assert!(editor.requests.is_empty(), "{:?}", editor.requests);
}
