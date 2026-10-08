//! End-to-end check of embedded context over ACP (issue #141).
//!
//! A client only inlines a resource's contents into a prompt when the agent
//! advertises `promptCapabilities.embeddedContext`. Otherwise it sends a
//! `resource_link` and sigit reads the file from disk, which misses edits the
//! user has not saved. This runs the real binary against a scripted
//! OpenAI-compatible endpoint and checks that the capability is advertised
//! and that an embedded resource reaches the model as sent, not as it is on
//! disk.

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

#[test]
fn an_embedded_resource_reaches_the_model_as_the_client_sent_it() {
    let endpoint = start_fake_endpoint(vec![sse_text("done")]);
    let scratch = std::env::temp_dir().join(format!("sigit_acp_embedded_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let project = scratch.join("project");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let notes = project.join("notes.txt");
    std::fs::write(&notes, "saved on disk").unwrap();

    let mut agent = spawn_agent(endpoint.port, &config_dir);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let initialize = agent.wait_for_response(id);
    assert_eq!(
        initialize["result"]["agentCapabilities"]["promptCapabilities"]["embeddedContext"], true,
        "a client only embeds resources for an agent that advertises it: {initialize}"
    );

    let id = agent.request("session/new", json!({"cwd": project, "mcpServers": []}));
    let session_id = agent.wait_for_response(id)["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    // The editor's buffer has an edit the file on disk does not.
    let uri = format!("file://{}", notes.display());
    let id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [
                {"type": "text", "text": "summarize this"},
                {"type": "resource", "resource": {
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": "unsaved buffer edit",
                }},
            ],
        }),
    );
    let response = agent.wait_for_response(id);
    assert_eq!(response["result"]["stopReason"], "end_turn");

    let request = endpoint
        .requests
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("the endpoint received a completion request");
    let user = request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .expect("a user message")
        .to_string();
    assert!(
        user.contains("unsaved buffer edit"),
        "the embedded contents must reach the model: {user}"
    );
    assert!(
        !user.contains("saved on disk"),
        "an embedded resource must not be re-read from disk: {user}"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}
