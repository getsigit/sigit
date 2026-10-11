//! The JSON-RPC error codes the agent answers with.
//!
//! ACP uses the standard codes: `-32602` for a request whose parameters are
//! wrong (an auth method that was never offered, a relative `cwd`), `-32002`
//! for a session id the agent does not know. `-32000` is reserved for
//! "authentication required", so a sign-in that fails is `-32603`.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

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

fn sse_text(text: &str) -> String {
    sse_body(&[json!({"choices": [{"delta": {"content": text}}]})])
}

/// Serves one scripted SSE response per request and records each request body.
// Shared harness; this file never reads the recorded requests.
#[allow(dead_code)]
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

impl AgentUnderTest {
    /// The raw answer to request `id`, error or not.
    fn answer(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.incoming.recv_timeout(remaining) {
                Ok(message) if message["id"] == id && message.get("method").is_none() => {
                    return message;
                }
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for the answer to request {id}"),
            }
        }
    }
}

fn open() -> (AgentUnderTest, std::path::PathBuf, std::path::PathBuf) {
    let endpoint = start_fake_endpoint(vec![]);
    let scratch = std::env::temp_dir().join(format!(
        "sigit_acp_codes_{}_{}",
        std::process::id(),
        endpoint.port
    ));
    let config_dir = scratch.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let mut agent = spawn_agent(endpoint.port, &config_dir);
    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    agent.wait_for_response(id);
    (agent, scratch, config_dir)
}

#[test]
fn an_auth_method_that_was_never_offered_is_invalid_params() {
    let (mut agent, scratch, _) = open();
    let id = agent.request("authenticate", json!({"methodId": "not-a-method"}));
    let answer = agent.answer(id);
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    drop(agent);
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn a_session_the_agent_does_not_know_is_resource_not_found() {
    let (mut agent, scratch, _) = open();
    for (method, params) in [
        (
            "session/prompt",
            json!({"sessionId": "nope", "prompt": [{"type": "text", "text": "hi"}]}),
        ),
        (
            "session/set_mode",
            json!({"sessionId": "nope", "modeId": "permission-mode-auto"}),
        ),
        (
            "session/set_config_option",
            json!({"sessionId": "nope", "configId": "sigit-permission-mode",
                   "value": "permission-mode-auto"}),
        ),
    ] {
        let id = agent.request(method, params);
        let answer = agent.answer(id);
        assert_eq!(answer["error"]["code"], -32002, "{method}: {answer}");
    }
    drop(agent);
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn a_relative_cwd_filter_on_session_list_is_invalid_params() {
    let (mut agent, scratch, _) = open();
    let id = agent.request("session/list", json!({"cwd": "relative/dir"}));
    let answer = agent.answer(id);
    assert_eq!(answer["error"]["code"], -32602, "{answer}");

    let id = agent.request("session/list", json!({}));
    let answer = agent.answer(id);
    assert!(answer.get("error").is_none(), "{answer}");
    drop(agent);
    let _ = std::fs::remove_dir_all(scratch);
}
