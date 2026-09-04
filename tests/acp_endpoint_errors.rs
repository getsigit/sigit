//! What the editor shows when the endpoint refuses a turn.
//!
//! Every endpoint siGit talks to reports failures in the OpenAI error envelope,
//! and siGit Code Cloud writes those messages for the person reading them
//! ("Monthly siGit Code Cloud allowance reached…"). The agent used to hand the
//! raw status line and JSON body to the client, so Zed's error banner showed
//! the envelope rather than the sentence inside it.
//!
//! Two shapes to cover: a failing HTTP status, and an endpoint that only
//! discovers the problem after the status line is out and reports it as a
//! `data:` frame in the stream.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

const ALLOWANCE_MESSAGE: &str = "Monthly siGit Code Cloud allowance reached. It resets at the start of your next billing period.";

/// One scripted reply per request: an HTTP status plus a body.
struct Reply {
    status: &'static str,
    content_type: &'static str,
    body: String,
}

fn error_status_reply() -> Reply {
    Reply {
        status: "429 Too Many Requests",
        content_type: "application/json",
        body: json!({"error": {"message": ALLOWANCE_MESSAGE, "type": "server_error"}}).to_string(),
    }
}

/// A 200 that turns into an error partway through, the way an endpoint has to
/// report a failure it only sees once the response is already open.
fn error_in_stream_reply() -> Reply {
    let frame = json!({"error": {"message": ALLOWANCE_MESSAGE, "type": "server_error"}});
    Reply {
        status: "200 OK",
        content_type: "text/event-stream",
        body: format!("data: {frame}\n\n"),
    }
}

fn start_fake_endpoint(replies: Vec<Reply>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake endpoint");
    let port = listener.local_addr().unwrap().port();
    let queue = Mutex::new(VecDeque::from(replies));

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
            let reply = queue.lock().unwrap().pop_front().unwrap_or_else(|| Reply {
                status: "200 OK",
                content_type: "text/event-stream",
                body: "data: [DONE]\n\n".to_string(),
            });
            let response = format!(
                "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{}",
                reply.status,
                reply.content_type,
                reply.body.len(),
                reply.body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
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
        self.stdin.write_all(line.as_bytes()).expect("write stdin");
        self.stdin.flush().expect("flush stdin");
        id
    }

    fn wait_for_message(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.incoming.recv_timeout(remaining) else {
                panic!("timed out waiting for the response to request {id}");
            };
            if message["id"] == id && message.get("method").is_none() {
                return message;
            }
        }
    }

    fn wait_for_response(&mut self, id: u64) -> Value {
        let message = self.wait_for_message(id);
        assert!(
            message.get("error").is_none(),
            "request {id} failed: {message}"
        );
        message
    }

    fn open_session(&mut self, cwd: &std::path::Path) -> String {
        let id = self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        self.wait_for_response(id);

        let id = self.request("session/new", json!({"cwd": cwd, "mcpServers": []}));
        self.wait_for_response(id)["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string()
    }
}

impl Drop for AgentUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sigit_acp_err_{name}_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("work")).unwrap();
    dir
}

/// The message the endpoint wrote is the message the client gets: no status
/// line in front of it, no JSON envelope around it.
fn assert_is_the_endpoints_message(error: &Value) {
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(message, ALLOWANCE_MESSAGE, "full error: {error}");
}

#[test]
fn a_refused_turn_reports_the_endpoints_own_message() {
    let dir = scratch("status");
    let port = start_fake_endpoint(vec![error_status_reply()]);
    let mut agent = spawn_agent(port, &dir.join("config"));
    let session_id = agent.open_session(&dir.join("work"));

    let id = agent.request(
        "session/prompt",
        json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]}),
    );
    let response = agent.wait_for_message(id);

    assert!(
        response.get("error").is_some(),
        "expected a failure: {response}"
    );
    assert_is_the_endpoints_message(&response);

    std::fs::remove_dir_all(&dir).ok();
}

/// An error that arrives as a stream frame used to parse as a chunk with no
/// choices and get skipped, so the turn ended as if the model had said nothing.
#[test]
fn an_error_frame_mid_stream_is_not_swallowed() {
    let dir = scratch("stream");
    let port = start_fake_endpoint(vec![error_in_stream_reply()]);
    let mut agent = spawn_agent(port, &dir.join("config"));
    let session_id = agent.open_session(&dir.join("work"));

    let id = agent.request(
        "session/prompt",
        json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]}),
    );
    let response = agent.wait_for_message(id);

    assert!(
        response.get("error").is_some(),
        "expected a failure: {response}"
    );
    assert_is_the_endpoints_message(&response);

    std::fs::remove_dir_all(&dir).ok();
}
