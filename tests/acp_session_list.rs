//! End-to-end check that `session/list` and `session/delete` work over ACP.
//!
//! Zed's "Import Threads" picker calls `session/list` for every configured
//! agent; siGit Code advertised no capability for it, so the entry showed a
//! warning triangle even though the transcripts were on disk all along
//! (issue #14). This drives the real binary: list before anything ran, run
//! one prompt, list again filtered by cwd, then delete and confirm both the
//! transcript and its metadata sidecar are gone.

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

    /// The response to one of our requests. `session/update` notifications
    /// that arrive along the way are dropped — this test only cares about
    /// list/delete responses, not turn replay.
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
fn listing_and_deleting_sessions_over_acp() {
    let scratch = std::env::temp_dir().join(format!("sigit_acp_list_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let project = scratch.join("project");
    let elsewhere = scratch.join("elsewhere");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();

    let port = start_fake_endpoint(vec![sse_text("The notes say hello.")]);
    let mut agent = spawn_agent(port, &config_dir);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let initialize = agent.wait_for_response(id);
    let session_caps = &initialize["result"]["agentCapabilities"]["sessionCapabilities"];
    assert!(
        session_caps.get("list").is_some(),
        "session/list capability must be advertised: {initialize}"
    );
    assert!(
        session_caps.get("delete").is_some(),
        "session/delete capability must be advertised: {initialize}"
    );

    // Before anything ran, the list is empty.
    let id = agent.request("session/list", json!({}));
    let listed = agent.wait_for_response(id);
    assert_eq!(
        listed["result"]["sessions"].as_array().unwrap().len(),
        0,
        "no sessions yet: {listed}"
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
            "prompt": [{"type": "text", "text": "what do the notes say?"}],
        }),
    );
    agent.wait_for_response(id);

    // Listing filtered by the project's cwd finds exactly this session.
    let id = agent.request("session/list", json!({"cwd": project}));
    let listed = agent.wait_for_response(id);
    let sessions = listed["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "expected one session: {listed}");
    let entry = &sessions[0];
    assert_eq!(entry["sessionId"], session_id);
    assert_eq!(entry["cwd"], project.to_string_lossy().to_string());
    assert_eq!(entry["title"], "what do the notes say?");
    let updated_at = entry["updatedAt"].as_str().expect("updatedAt present");
    assert!(
        chrono::DateTime::parse_from_rfc3339(updated_at).is_ok(),
        "updatedAt must be RFC 3339: {updated_at}"
    );

    // Filtered by a different cwd, the list is empty.
    let id = agent.request("session/list", json!({"cwd": elsewhere}));
    let listed = agent.wait_for_response(id);
    assert_eq!(
        listed["result"]["sessions"].as_array().unwrap().len(),
        0,
        "wrong cwd must not match: {listed}"
    );

    // Delete it, then confirm it's gone from the list and off disk.
    let id = agent.request("session/delete", json!({"sessionId": session_id}));
    let deleted = agent.wait_for_response(id);
    assert!(deleted.get("error").is_none(), "delete failed: {deleted}");

    let id = agent.request("session/list", json!({"cwd": project}));
    let listed = agent.wait_for_response(id);
    assert_eq!(
        listed["result"]["sessions"].as_array().unwrap().len(),
        0,
        "deleted session must not be listed: {listed}"
    );

    let sessions_dir = config_dir.join("sessions");
    assert!(
        std::fs::read_dir(&sessions_dir)
            .into_iter()
            .flatten()
            .flatten()
            .all(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                !name.contains(&session_id)
            }),
        "both the transcript and its meta sidecar must be removed"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}
