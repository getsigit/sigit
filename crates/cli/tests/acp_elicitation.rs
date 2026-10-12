//! `authenticate` through a URL-mode elicitation (issue #197).
//!
//! A client that advertises `elicitation.url` is handed the sign-in page
//! instead of sigit launching a browser itself. The test plays the client and
//! the browser: it answers the `elicitation/create`, follows the authorize URL
//! back to sigit's loopback listener, and serves the token and profile
//! endpoints from a fake siGit Code Cloud.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

/// A fake siGit Code Cloud: answers `POST /oauth/token` with a token and
/// `GET /api/v1/user` with an email. Returns its base URL.
fn start_fake_cloud() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake cloud");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).ok();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = length.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).ok();
            let payload = if request_line.contains("/oauth/token") {
                json!({"access_token": "tok_from_browser", "token_type": "Bearer"})
            } else {
                json!({"email": "dev@sigit.si"})
            }
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            stream.write_all(response.as_bytes()).ok();
        }
    });
    format!("http://127.0.0.1:{port}")
}

struct AgentUnderTest {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
}

fn spawn_agent(cloud: &str, config_dir: &std::path::Path) -> AgentUnderTest {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sigit"))
        .env("SIGIT_API_URL", cloud)
        .env("SIGIT_CLOUD_URL", cloud)
        .env("SIGIT_CONFIG_DIR", config_dir)
        .env("SIGIT_MCP", "off")
        // Nothing here talks to a model; any endpoint will do.
        .env("OPENAI_BASE_URL", "http://127.0.0.1:9")
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
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
    }
}

impl AgentUnderTest {
    fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn next(&mut self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.incoming.recv_timeout(remaining) {
                Ok(message) if matches(&message) => return message,
                Ok(_) => continue,
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }

    fn response(&mut self, id: u64) -> Value {
        self.next(&format!("the response to {id}"), |message| {
            message["id"] == id && message.get("method").is_none()
        })
    }
}

impl Drop for AgentUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The value of `key` in the authorize URL's query, percent-decoded.
fn query_value(url: &str, key: &str) -> String {
    let query = url.split_once('?').expect("query").1;
    let raw = query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("{key} missing from {url}"));
    let mut decoded = String::new();
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hex: String = [bytes.next().unwrap(), bytes.next().unwrap()]
                .iter()
                .map(|&b| b as char)
                .collect();
            decoded.push(u8::from_str_radix(&hex, 16).unwrap() as char);
        } else {
            decoded.push(byte as char);
        }
    }
    decoded
}

/// Start `authenticate` from a client that renders URL elicitations, and
/// return the agent, the request id, and the elicitation it was sent.
fn start_sign_in(tag: &str) -> (AgentUnderTest, std::path::PathBuf, Value) {
    let cloud = start_fake_cloud();
    let scratch =
        std::env::temp_dir().join(format!("sigit_acp_elicit_{tag}_{}", std::process::id()));
    let config_dir = scratch.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let mut agent = spawn_agent(&cloud, &config_dir);
    agent.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {"elicitation": {"url": {}}},
        },
    }));
    agent.response(1);

    agent.send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "authenticate",
        "params": {"methodId": "sigit"},
    }));
    let elicitation = agent.next("the sign-in elicitation", |message| {
        message["method"] == "elicitation/create"
    });
    let params = &elicitation["params"];
    assert_eq!(params["mode"], "url", "{params}");
    assert_eq!(
        params["requestId"], 2,
        "scoped to the authenticate request: {params}"
    );
    assert!(
        params["url"]
            .as_str()
            .unwrap_or_default()
            .starts_with(&format!("{cloud}/oauth/authorize?")),
        "{params}"
    );
    assert!(params["elicitationId"].is_string(), "{params}");
    (agent, scratch, elicitation)
}

#[test]
fn sign_in_hands_the_page_to_a_client_that_renders_urls() {
    let (mut agent, scratch, elicitation) = start_sign_in("accept");
    let params = &elicitation["params"];
    let url = params["url"].as_str().unwrap().to_string();

    agent.send(json!({
        "jsonrpc": "2.0", "id": elicitation["id"], "result": {"action": "accept"},
    }));

    // The browser finishes the authorization and comes back to sigit.
    let redirect = query_value(&url, "redirect_uri");
    let state = query_value(&url, "state");
    let address = redirect
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .expect("loopback redirect")
        .0
        .to_string();
    let mut browser = TcpStream::connect(&address).expect("reach the loopback listener");
    write!(
        browser,
        "GET /callback?code=abc&state={state} HTTP/1.1\r\nHost: {address}\r\n\r\n"
    )
    .unwrap();
    let mut page = String::new();
    browser.read_to_string(&mut page).ok();
    assert!(page.contains("Signed in"), "{page}");

    // The client hears that the page is done with, then gets its answer.
    let complete = agent.next("elicitation/complete", |message| {
        message["method"] == "elicitation/complete"
    });
    assert_eq!(complete["params"]["elicitationId"], params["elicitationId"]);
    let response = agent.response(2);
    assert!(response.get("error").is_none(), "{response}");
    let saved = std::fs::read_to_string(scratch.join("config/credentials.toml")).unwrap();
    assert!(saved.contains("tok_from_browser"), "{saved}");

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn declining_the_sign_in_page_ends_the_sign_in() {
    let (mut agent, scratch, elicitation) = start_sign_in("decline");

    agent.send(json!({
        "jsonrpc": "2.0", "id": elicitation["id"], "result": {"action": "decline"},
    }));

    let response = agent.response(2);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Sign-in did not complete"),
        "{response}"
    );
    assert!(!scratch.join("config/credentials.toml").exists());

    drop(agent);
    let _ = std::fs::remove_dir_all(&scratch);
}
