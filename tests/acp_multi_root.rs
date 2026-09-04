//! End-to-end check that a multi-root project reaches the model.
//!
//! Zed (and any other ACP client that can open several directories at once)
//! sends the extra ones as `additionalDirectories` on `session/new`. This
//! spawns the real binary in ACP mode against a scripted OpenAI-compatible
//! endpoint, opens a session with two roots, and inspects what the endpoint was
//! actually sent: the tool list must include a skill that only exists in the
//! *second* root, which is only true if that root was recorded and
//! project-local discovery scanned it.
//!
//! The system context that names the roots is not asserted here. This test
//! drives the agent through the `OPENAI_BASE_URL` override, and that backend is
//! built once at startup — the per-session context message goes into the local
//! engine's history, which the override never reads. `session_context_message`
//! and the instruction loader are covered by unit tests instead.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

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

    FakeEndpoint { port, requests }
}

fn sse_text(text: &str) -> String {
    let event = json!({"choices": [{"delta": {"content": text}}]});
    format!("data: {event}\n\ndata: [DONE]\n\n")
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
fn a_skill_from_a_second_root_is_offered_to_the_model() {
    let endpoint = start_fake_endpoint(vec![sse_text("looked at both roots")]);

    let scratch = std::env::temp_dir().join(format!("sigit_acp_roots_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let primary = scratch.join("primary");
    let secondary = scratch.join("secondary");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&primary).unwrap();
    std::fs::create_dir_all(&secondary).unwrap();
    // A skill that exists only in the second root.
    let skill_dir = secondary.join(".sigit").join("skills").join("second-root");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: second-root\ndescription: Only reachable through the second root\n---\n\nDo the thing.\n",
    )
    .unwrap();

    let mut agent = spawn_agent(endpoint.port, &config_dir);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    agent.wait_for_response(id);

    let id = agent.request(
        "session/new",
        json!({
            "cwd": primary,
            "mcpServers": [],
            "additionalDirectories": [secondary],
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
            "prompt": [{"type": "text", "text": "where are we?"}],
        }),
    );
    agent.wait_for_response(id);

    let requests = endpoint.requests.lock().unwrap();
    let request = requests.first().expect("one completion request");

    let skill_tool = request["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["function"]["name"] == "skill")
        .expect("the skill tool is offered when a root has skills");
    assert!(
        skill_tool["function"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("second-root"),
        "a skill from the second root must be discoverable, got: {skill_tool}"
    );

    drop(requests);
    let _ = std::fs::remove_dir_all(&scratch);
}
