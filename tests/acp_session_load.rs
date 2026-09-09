//! End-to-end check that reopening a saved session redraws it in the client.
//!
//! ACP's `session/load` is a replay: the client renders the reopened thread
//! from the `session/update` notifications the agent streams while the request
//! is in flight, and keeps nothing of its own. Restoring the history into the
//! backend is what makes the *model* remember, but it puts nothing on screen —
//! before this, clicking a thread in Zed's history opened an empty one that
//! looked brand new (issue #77).
//!
//! This drives the real binary against a scripted OpenAI-compatible endpoint:
//! run one prompt that talks and calls a tool, then load the same session id
//! and assert the conversation comes back over the wire.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};
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

// ── The agent under test ────────────────────────────────────────────────────

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

    /// The response to one of our requests, plus every `session/update` that
    /// arrived before it — which for `session/load` is the whole replay.
    fn wait_for_response_with_updates(&mut self, id: u64) -> (Value, Vec<Value>) {
        let mut updates = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.incoming.recv_timeout(remaining) else {
                panic!("timed out waiting for the response to request {id}");
            };
            if message["method"] == "session/update" {
                updates.push(message["params"]["update"].clone());
            }
            if message["id"] == id && message.get("method").is_none() {
                assert!(
                    message.get("error").is_none(),
                    "request {id} failed: {message}"
                );
                return (message, updates);
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

fn text_of(update: &Value) -> &str {
    update["content"]["text"].as_str().unwrap_or_default()
}

#[test]
fn loading_a_saved_session_replays_it_to_the_client() {
    let scratch = std::env::temp_dir().join(format!("sigit_acp_load_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let project = scratch.join("project");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let notes = project.join("notes.md");
    std::fs::write(&notes, "hello from the notes\n").unwrap();

    // Round 1 talks and reads that file; round 2 answers.
    let port = start_fake_endpoint(vec![
        sse_text_then_tool_call(
            "Let me look at the notes.",
            "call_1",
            "read_file",
            &json!({"path": notes.to_string_lossy()}).to_string(),
        ),
        sse_text("The notes say hello."),
    ]);

    let mut agent = spawn_agent(port, &config_dir);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let (initialize, _) = agent.wait_for_response_with_updates(id);
    assert_eq!(
        initialize["result"]["agentCapabilities"]["loadSession"], true,
        "a client only offers to reopen threads for an agent that advertises loadSession: \
         {initialize}"
    );

    let id = agent.request("session/new", json!({"cwd": project, "mcpServers": []}));
    let (new_session, _) = agent.wait_for_response_with_updates(id);
    let session_id = new_session["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    let id = agent.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "what do the notes say?"}],
        }),
    );
    agent.wait_for_response_with_updates(id);

    // Reopen it, the way clicking the thread in the editor's history does.
    let id = agent.request(
        "session/load",
        json!({"sessionId": session_id, "cwd": project, "mcpServers": []}),
    );
    let (_, replay) = agent.wait_for_response_with_updates(id);

    let user: Vec<&str> = replay
        .iter()
        .filter(|update| update["sessionUpdate"] == "user_message_chunk")
        .map(text_of)
        .collect();
    assert_eq!(
        user,
        vec!["what do the notes say?"],
        "the user's turn must come back: {replay:#?}"
    );

    let agent_text: String = replay
        .iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .map(text_of)
        .collect();
    assert!(
        agent_text.contains("Let me look at the notes.")
            && agent_text.contains("The notes say hello."),
        "both rounds of the reply must come back, got {agent_text:?}"
    );

    let tool_call = replay
        .iter()
        .find(|update| update["sessionUpdate"] == "tool_call")
        .unwrap_or_else(|| panic!("the tool call must come back: {replay:#?}"));
    assert_eq!(tool_call["title"], "read_file");
    // Nothing is still running in a saved session, and the result is folded
    // back into its call rather than replayed as a loose message.
    assert_eq!(tool_call["status"], "completed");
    assert!(
        tool_call["rawOutput"]
            .as_str()
            .unwrap_or_default()
            .contains("hello from the notes"),
        "the tool result must ride along with its call: {tool_call}"
    );

    // The seeded system context is the model's, not the user's — it must never
    // surface as a message in the thread.
    for update in &replay {
        assert!(
            !text_of(update).contains("project working directory"),
            "system context leaked into the replay: {update}"
        );
    }

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}
