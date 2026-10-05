//! ACP `logout`: the editor's sign out button.
//!
//! The method is optional in ACP v1 and gated by `agentCapabilities.auth.logout`.
//! A client that does not see the capability shows no button, and one that
//! does expects the request to end the account session the same way `/logout`
//! does: the server is told, and the stored token is gone.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

/// Stand in for the account API. Every request's head (request line and
/// headers, lowercased) is forwarded to the returned receiver.
fn start_fake_account_api() -> (u16, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake account api");
    let port = listener.local_addr().unwrap().port();
    let (head_tx, heads) = channel();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(clone) => clone,
                Err(_) => continue,
            });
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                head.push_str(&line.to_ascii_lowercase());
            }
            let _ = head_tx.send(head);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                  content-length: 2\r\nconnection: close\r\n\r\n{}",
            );
            let _ = stream.flush();
        }
    });

    (port, heads)
}

struct AgentUnderTest {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
}

fn spawn_agent(api_port: u16, config_dir: &std::path::Path) -> AgentUnderTest {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sigit"))
        // A provider override keeps inference off both the cloud and the
        // on-device model. No prompt is sent, so nothing ever connects to it.
        .env("OPENAI_BASE_URL", "http://127.0.0.1:9")
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
        .env("SIGIT_API_URL", format!("http://127.0.0.1:{api_port}"))
        .env("SIGIT_CONFIG_DIR", config_dir)
        .env("SIGIT_MCP", "off")
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
fn logout_is_advertised_and_ends_the_account_session() {
    let scratch = std::env::temp_dir().join(format!("sigit_acp_logout_{}", std::process::id()));
    let config_dir = scratch.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let credentials = config_dir.join("credentials.toml");
    std::fs::write(
        &credentials,
        "access_token = \"token-under-test\"\nemail = \"dev@example.com\"\n",
    )
    .unwrap();

    let (api_port, api_requests) = start_fake_account_api();
    let mut agent = spawn_agent(api_port, &config_dir);

    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let initialized = agent.wait_for_response(id);
    assert!(
        initialized["result"]["agentCapabilities"]["auth"]["logout"].is_object(),
        "logout capability missing: {initialized}"
    );

    let id = agent.request("logout", json!({}));
    let response = agent.wait_for_response(id);
    assert!(response["result"].is_object(), "{response}");

    assert!(!credentials.exists(), "the stored token should be gone");
    let head = api_requests
        .recv_timeout(TIMEOUT)
        .expect("the account API should be told about the sign-out");
    assert!(head.starts_with("delete /api/v1/auth/sign_out "), "{head}");
    assert!(
        head.contains("authorization: bearer token-under-test"),
        "{head}"
    );

    // Signing out twice is not an error: the editor's state may be stale.
    let id = agent.request("logout", json!({}));
    agent.wait_for_response(id);

    std::fs::remove_dir_all(&scratch).ok();
}
