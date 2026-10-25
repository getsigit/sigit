//! Signing in as another account takes effect on the next message.
//!
//! A cloud backend used to keep the session token it was built with, so after
//! Reauthenticate wrote a new one the thread went on sending the old account's
//! bearer until the editor restarted the agent. The token is now read from the
//! stored session on every request.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

/// Stand in for `<cloud>/chat/completions`. The `authorization` header of each
/// request is forwarded to the returned receiver.
fn start_fake_cloud() -> (u16, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake cloud");
    let port = listener.local_addr().unwrap().port();
    let (auth_tx, auths) = channel();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(clone) => clone,
                Err(_) => continue,
            });
            let mut content_length = 0usize;
            let mut authorization = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim().to_string();
                if line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(length) = lower.strip_prefix("content-length:") {
                    content_length = length.trim().parse().unwrap_or(0);
                }
                if lower.starts_with("authorization:") {
                    authorization = lower;
                }
            }
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let _ = auth_tx.send(authorization);

            let body = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": {"content": "ok"}}]})
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    (port, auths)
}

struct AgentUnderTest {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
}

fn spawn_agent(cloud_port: u16, config_dir: &std::path::Path) -> AgentUnderTest {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sigit"))
        // No provider override and no on-device model: the thread starts on a
        // cloud tier because a session is stored.
        .env_remove("OPENAI_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .env_remove("SIGIT_MODEL")
        .env("SIGIT_LOCAL_INFERENCE", "off")
        .env("SIGIT_CLOUD_URL", format!("http://127.0.0.1:{cloud_port}"))
        .env("SIGIT_CONFIG_DIR", config_dir)
        .env("SIGIT_MCP", "off")
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

    fn prompt(&mut self, session_id: &str, text: &str) {
        let id = self.request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}),
        );
        self.wait_for_response(id);
    }
}

impl Drop for AgentUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn store_token(config_dir: &std::path::Path, token: &str) {
    std::fs::write(
        config_dir.join("credentials.toml"),
        format!("access_token = \"{token}\"\nemail = \"dev@example.com\"\n"),
    )
    .unwrap();
}

#[test]
fn a_new_session_token_is_used_by_the_next_turn() {
    let scratch = std::env::temp_dir().join(format!("sigit_acp_reauth_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let work_dir = scratch.join("work");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();
    store_token(&config_dir, "token-a");

    let (port, auths) = start_fake_cloud();
    let mut agent = spawn_agent(port, &config_dir);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    agent.wait_for_response(id);
    let id = agent.request("session/new", json!({"cwd": work_dir, "mcpServers": []}));
    let session_id = agent.wait_for_response(id)["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    agent.prompt(&session_id, "first");
    let first = auths.recv_timeout(TIMEOUT).expect("first request");
    assert_eq!(first, "authorization: bearer token-a");

    // What Reauthenticate does once the browser flow hands back a code.
    store_token(&config_dir, "token-b");

    agent.prompt(&session_id, "second");
    let second = auths.recv_timeout(TIMEOUT).expect("second request");
    assert_eq!(second, "authorization: bearer token-b");

    std::fs::remove_dir_all(&scratch).ok();
}
