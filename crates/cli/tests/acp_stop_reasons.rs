//! The stop reason a prompt turn ends with (issue #140).
//!
//! ACP defines five and the agent only ever sent `end_turn` and `cancelled`,
//! so a client could not tell a finished answer from one cut off at the token
//! limit, withheld by the endpoint, or stopped at the tool-round cap. The
//! `cancelled` case is covered in `acp_permissions.rs`; this file drives the
//! other three through a scripted endpoint.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

/// The built-in tool-round cap (`MAX_TOOL_ROUNDS` in `main.rs`).
const TOOL_ROUND_CAP: usize = 24;

/// A streamed text reply that ends with `finish_reason`.
fn sse_text(text: &str, finish_reason: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": text}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": finish_reason}]}),
    )
}

fn sse_tool_call(id: &str, name: &str, arguments: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": id,
                "function": {"name": name, "arguments": arguments},
            }]}}]
        }),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    )
}

/// A scripted endpoint: one SSE body per request, in order, then empty
/// replies. Every request body it receives is kept for the test to read.
struct FakeEndpoint {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
}

fn start_fake_endpoint(replies: Vec<String>) -> FakeEndpoint {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake endpoint");
    let port = listener.local_addr().unwrap().port();
    let queue = Mutex::new(VecDeque::from(replies));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);

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
                seen.lock().unwrap().push(request);
            }
            let reply = queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "data: [DONE]\n\n".to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    FakeEndpoint { port, requests }
}

struct AgentUnderTest {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
}

fn spawn_agent(port: u16, config_dir: &std::path::Path) -> AgentUnderTest {
    spawn_agent_with_env(port, config_dir, &[])
}

fn spawn_agent_with_env(
    port: u16,
    config_dir: &std::path::Path,
    env: &[(&str, &str)],
) -> AgentUnderTest {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sigit"))
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
        .env("SIGIT_CONFIG_DIR", config_dir)
        .env("SIGIT_MCP", "off")
        .env("SIGIT_PERMISSIONS", "allow")
        .env_remove("SIGIT_LOCAL_INFERENCE")
        .env_remove("SIGIT_MAX_TOOL_ROUNDS")
        .envs(env.iter().copied())
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

    /// Send one text prompt and return the stop reason the turn ended with.
    fn prompt(&mut self, session_id: &str, text: &str) -> String {
        self.prompt_with_reply(session_id, text).0
    }

    /// Like [`Self::prompt`], also returning the assistant text the client
    /// was sent on the way, joined the way a client renders it.
    fn prompt_with_reply(&mut self, session_id: &str, text: &str) -> (String, String) {
        let id = self.request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}),
        );
        let mut rendered = String::new();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.incoming.recv_timeout(remaining) else {
                panic!("timed out waiting for the response to request {id}");
            };
            let update = &message["params"]["update"];
            if message["method"] == "session/update"
                && update["sessionUpdate"] == "agent_message_chunk"
                && let Some(chunk) = update["content"]["text"].as_str()
            {
                rendered.push_str(chunk);
            }
            if message["id"] == id && message.get("method").is_none() {
                assert!(message.get("error").is_none(), "prompt failed: {message}");
                let stop_reason = message["result"]["stopReason"]
                    .as_str()
                    .expect("stop reason")
                    .to_string();
                return (stop_reason, rendered);
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

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sigit_acp_stop_{name}_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("work")).unwrap();
    dir
}

/// Whether any message of a chat-completion request mentions `needle`.
fn request_mentions(request: &Value, needle: &str) -> bool {
    request["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|m| m.to_string().contains(needle)))
}

#[test]
fn a_reply_cut_off_at_the_token_limit_ends_with_max_tokens() {
    let dir = scratch("length");
    let endpoint = start_fake_endpoint(vec![sse_text("The answer is", "length")]);
    let mut agent = spawn_agent(endpoint.port, &dir.join("config"));
    let session_id = agent.open_session(&dir.join("work"));

    assert_eq!(agent.prompt(&session_id, "explain"), "max_tokens");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_finished_reply_still_ends_with_end_turn() {
    let dir = scratch("stop");
    let endpoint = start_fake_endpoint(vec![sse_text("Done.", "stop")]);
    let mut agent = spawn_agent(endpoint.port, &dir.join("config"));
    let session_id = agent.open_session(&dir.join("work"));

    assert_eq!(agent.prompt(&session_id, "explain"), "end_turn");

    std::fs::remove_dir_all(&dir).ok();
}

/// A refusal is more than a label: ACP says the refused prompt and what
/// followed it are not part of the next prompt, and the client drops them from
/// its view on that promise. The agent has to drop them from history too.
#[test]
fn a_withheld_reply_ends_with_refusal_and_leaves_history() {
    let dir = scratch("refusal");
    let endpoint = start_fake_endpoint(vec![
        sse_text("Sure.", "stop"),
        sse_text("", "content_filter"),
        sse_text("Here you go.", "stop"),
    ]);
    let mut agent = spawn_agent(endpoint.port, &dir.join("config"));
    let session_id = agent.open_session(&dir.join("work"));

    assert_eq!(agent.prompt(&session_id, "first-kept-prompt"), "end_turn");
    assert_eq!(
        agent.prompt(&session_id, "second-refused-prompt"),
        "refusal"
    );
    assert_eq!(agent.prompt(&session_id, "third-prompt"), "end_turn");

    let requests = endpoint.requests.lock().unwrap();
    let last = requests
        .last()
        .expect("the third prompt reached the endpoint");
    assert!(
        request_mentions(last, "first-kept-prompt"),
        "the turn before the refusal stays in history: {last}"
    );
    assert!(
        !request_mentions(last, "second-refused-prompt"),
        "the refused prompt must not be replayed: {last}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The case the reason exists for: the model keeps asking for tools until the
/// agent stops offering them.
#[test]
fn a_turn_stopped_at_the_tool_round_cap_ends_with_max_turn_requests() {
    let dir = scratch("cap");
    // Each call differs, or the repeated-call guard ends the turn first. The
    // commands stay quote-free: `cmd /C` mangles quotes on Windows.
    let replies = (1..=TOOL_ROUND_CAP)
        .map(|round| {
            sse_tool_call(
                &format!("call_{round}"),
                "run_command",
                &json!({"command": format!("echo round-{round}")}).to_string(),
            )
        })
        .chain([sse_text("Out of rounds.", "stop")])
        .collect();
    let endpoint = start_fake_endpoint(replies);
    let mut agent = spawn_agent(endpoint.port, &dir.join("config"));
    let session_id = agent.open_session(&dir.join("work"));

    assert_eq!(agent.prompt(&session_id, "keep going"), "max_turn_requests");
    assert_eq!(
        endpoint.requests.lock().unwrap().len(),
        TOOL_ROUND_CAP + 1,
        "one request per tool round, plus the forced closing reply"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// What issue #120 looked like from the editor: the turn reached the cap, the
/// model used its last reply to announce the next step, the call behind it was
/// dropped, and the thread stopped on "Now commit:" with nothing after it.
#[test]
fn a_turn_stopped_at_the_cap_says_so_to_the_user_and_the_model() {
    let dir = scratch("cap_notice");
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "run_command", r#"{"command":"echo round-1"}"#),
        sse_tool_call("call_2", "run_command", r#"{"command":"echo round-2"}"#),
        sse_text("Now commit:", "stop"),
    ]);
    let mut agent = spawn_agent_with_env(
        endpoint.port,
        &dir.join("config"),
        &[("SIGIT_MAX_TOOL_ROUNDS", "2")],
    );
    let session_id = agent.open_session(&dir.join("work"));

    let (stop_reason, reply) = agent.prompt_with_reply(&session_id, "ship it");

    assert_eq!(stop_reason, "max_turn_requests");
    assert!(
        reply.contains("Now commit:") && reply.contains("all 2 of its tool rounds"),
        "the user must be told why the turn stopped: {reply:?}"
    );

    // The model is told before its last reply, on the result of the last
    // call, and only there.
    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let tool_results = |request: &Value| -> Vec<String> {
        request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["content"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    let before_cap = tool_results(&requests[1]);
    assert!(
        before_cap
            .iter()
            .all(|content| !content.contains("[siGit Code]")),
        "no note while rounds remain: {before_cap:?}"
    );
    let at_cap = tool_results(&requests[2]);
    assert!(
        at_cap.last().is_some_and(|content| {
            content.contains("round-2") && content.contains("no more tools can run")
        }),
        "the last tool result carries the note: {at_cap:?}"
    );
    assert!(
        requests[2].get("tools").is_none(),
        "the closing round offers no tools: {}",
        requests[2]
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A turn that finishes inside its rounds gets no notice.
#[test]
fn a_turn_that_finishes_inside_the_cap_gets_no_notice() {
    let dir = scratch("under_cap");
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "run_command", r#"{"command":"echo round-1"}"#),
        sse_text("Committed.", "stop"),
    ]);
    let mut agent = spawn_agent_with_env(
        endpoint.port,
        &dir.join("config"),
        &[("SIGIT_MAX_TOOL_ROUNDS", "2")],
    );
    let session_id = agent.open_session(&dir.join("work"));

    let (stop_reason, reply) = agent.prompt_with_reply(&session_id, "ship it");

    assert_eq!(stop_reason, "end_turn");
    assert_eq!(reply.trim(), "Committed.");

    std::fs::remove_dir_all(&dir).ok();
}
