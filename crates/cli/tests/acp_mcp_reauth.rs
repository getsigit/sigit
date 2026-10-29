//! The official MCP server follows the signed-in account.
//!
//! An MCP connection fixes its bearer when it connects, so `web_search` and the
//! other `mcp__sigit__*` tools used to keep the account that was signed in at
//! startup. The server is now reconnected when the stored token changes, and
//! the check runs before the call so the old bearer is never sent.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n\
         content-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn serve(stream: TcpStream, calls: Sender<String>) {
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut stream = stream;
    let mut reader = BufReader::new(clone);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let method = request_line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        let mut content_length = 0usize;
        let mut authorization = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
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
            return;
        }
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

        if path.ends_with("/chat/completions") {
            // Search once per prompt: answer in text only after a tool result.
            let answered = body["messages"]
                .as_array()
                .and_then(|m| m.last())
                .is_some_and(|m| m["role"] == "tool");
            let delta = if answered {
                json!({"content": "done"})
            } else {
                json!({"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "mcp__sigit__web_search",
                        "arguments": "{\"query\":\"rust\"}"
                    }
                }]})
            };
            let sse = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": delta}]})
            );
            respond(&mut stream, "200 OK", "text/event-stream", &sse);
        } else if path.ends_with("/mcp") && method == "POST" {
            match body["method"].as_str() {
                Some("initialize") => {
                    let result = json!({
                        "jsonrpc": "2.0",
                        "id": body["id"],
                        "result": {
                            "protocolVersion": body["params"]["protocolVersion"],
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "fake", "version": "1"}
                        }
                    });
                    respond(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        &result.to_string(),
                    );
                }
                Some("tools/list") => {
                    let result = json!({
                        "jsonrpc": "2.0",
                        "id": body["id"],
                        "result": {"tools": [{
                            "name": "web_search",
                            "description": "Search the web",
                            "inputSchema": {"type": "object"}
                        }]}
                    });
                    respond(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        &result.to_string(),
                    );
                }
                Some("tools/call") => {
                    let _ = calls.send(authorization);
                    let result = json!({
                        "jsonrpc": "2.0",
                        "id": body["id"],
                        "result": {"content": [{"type": "text", "text": "found"}]}
                    });
                    respond(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        &result.to_string(),
                    );
                }
                // Notifications carry no id and get an empty 202.
                _ => respond(&mut stream, "202 Accepted", "text/plain", ""),
            }
        } else {
            respond(&mut stream, "405 Method Not Allowed", "text/plain", "");
        }
    }
}

/// Stand in for the cloud: `/mcp` and `/chat/completions`. The `authorization`
/// header of every MCP `tools/call` is forwarded to the returned receiver.
fn start_fake_cloud() -> (u16, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake cloud");
    let port = listener.local_addr().unwrap().port();
    let (call_tx, calls) = channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let calls = call_tx.clone();
            std::thread::spawn(move || serve(stream, calls));
        }
    });
    (port, calls)
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
fn the_official_mcp_server_uses_the_current_account() {
    let scratch = std::env::temp_dir().join(format!("sigit_acp_mcp_reauth_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let work_dir = scratch.join("work");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();
    store_token(&config_dir, "token-a");

    let (port, calls) = start_fake_cloud();
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
    let first = calls.recv_timeout(TIMEOUT).expect("first tools/call");
    assert_eq!(first, "authorization: bearer token-a");

    // What signing in as another account does to the stored session.
    store_token(&config_dir, "token-b");
    agent.prompt(&session_id, "second");
    let second = calls.recv_timeout(TIMEOUT).expect("second tools/call");
    assert_eq!(second, "authorization: bearer token-b");

    // And signing out.
    std::fs::remove_file(config_dir.join("credentials.toml")).unwrap();
    agent.prompt(&session_id, "third");
    let third = calls.recv_timeout(TIMEOUT).expect("third tools/call");
    assert_eq!(third, "");

    std::fs::remove_dir_all(&scratch).ok();
}
