//! End-to-end check that saved threads can be imported through `session/list`.
//!
//! Editors offer an "Import Threads" picker only to an agent that advertises
//! ACP's `sessionCapabilities.list` and answers `session/list` with the
//! sessions it has stored — before this, Zed said siGit Code doesn't support
//! the capability (issue #93).
//!
//! This drives the real binary against a scripted OpenAI-compatible endpoint:
//! run one prompt so a session gets saved, then list and assert the thread
//! comes back with its directory, title, and timestamp — and that the `cwd`
//! filter keeps another project's threads out.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

fn sse_text(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": text}}]})
    )
}

/// Serves one scripted SSE response per request.
fn start_fake_endpoint(responses: Vec<String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake endpoint");
    let port = listener.local_addr().unwrap().port();
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
            let payload = queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "data: [DONE]\n\n".to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    port
}

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
    fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let mut line =
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("write to agent stdin");
        self.stdin.flush().expect("flush agent stdin");
        id
    }

    fn wait_for_response(&mut self, id: u64) -> Value {
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
        }
    }
}

impl Drop for AgentUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn saved_sessions_are_listed_for_their_project() {
    let scratch = std::env::temp_dir().join(format!("sigit_acp_list_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let project = scratch.join("project");
    let other = scratch.join("other");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&other).unwrap();

    let port = start_fake_endpoint(vec![sse_text("Sure thing.")]);
    let mut agent = spawn_agent(port, &config_dir);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let initialize = agent.wait_for_response(id);
    assert!(
        initialize["result"]["agentCapabilities"]["sessionCapabilities"]["list"].is_object(),
        "the import picker is gated on the session/list capability: {initialize}"
    );

    // Nothing saved yet.
    let id = agent.request("session/list", json!({}));
    let empty = agent.wait_for_response(id);
    assert_eq!(
        empty["result"]["sessions"].as_array().map(Vec::len),
        Some(0),
        "no sessions have been saved yet: {empty}"
    );

    let id = agent.request("session/new", json!({"cwd": project, "mcpServers": []}));
    let new_session = agent.wait_for_response(id);
    let session_id = new_session["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    let id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "rename the parser module"}],
        }),
    );
    agent.wait_for_response(id);

    // The saved thread shows up for its own project …
    let id = agent.request("session/list", json!({"cwd": project}));
    let listed = agent.wait_for_response(id);
    let sessions = listed["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("sessions array: {listed}"));
    assert_eq!(sessions.len(), 1, "one saved thread expected: {listed}");
    let info = &sessions[0];
    assert_eq!(info["sessionId"], json!(session_id));
    assert_eq!(
        std::fs::canonicalize(info["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&project).unwrap()
    );
    assert_eq!(
        info["title"], "rename the parser module",
        "the title comes from the first user message: {info}"
    );
    let updated_at = info["updatedAt"].as_str().unwrap_or_default();
    assert!(
        updated_at.len() == 24 && updated_at.ends_with('Z') && updated_at.contains('.'),
        "updatedAt should be an ISO 8601 UTC instant with milliseconds, got {updated_at:?}"
    );

    // … and not for a different one.
    let id = agent.request("session/list", json!({"cwd": other}));
    let elsewhere = agent.wait_for_response(id);
    assert_eq!(
        elsewhere["result"]["sessions"].as_array().map(Vec::len),
        Some(0),
        "another project's threads must not be offered: {elsewhere}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}
