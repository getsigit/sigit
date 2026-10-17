//! Two ACP sessions in one agent process, on an HTTP backend.
//!
//! An editor runs a single sigit process for every thread it has open. A turn
//! spends most of its time waiting on the endpoint, and nothing in that wait
//! needs the process's working directory or workspace roots, so a second
//! thread should be able to open, run a tool, and finish while the first is
//! still streaming. Each thread also has to find its own working directory
//! again when its next tool call runs.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

/// The prompt text that makes the endpoint hold its reply open.
const HOLD: &str = "HOLD-THIS-TURN";

/// The prompt text that makes the model ask for a command instead of a read,
/// which is what the permission gate stops on.
const RUN: &str = "RUN-A-COMMAND";

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn text_delta(text: &str) -> Value {
    json!({"choices": [{"delta": {"content": text}}]})
}

fn finish(reason: &str) -> Value {
    json!({"choices": [{"delta": {}, "finish_reason": reason}]})
}

fn read_marker_call() -> Value {
    json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_marker",
            "function": {"name": "read_file", "arguments": "{\"path\":\"marker.txt\"}"},
        }]}}]
    })
}

/// Writes `ran.txt` into the working directory. Plain redirection, so the
/// same line works under `sh` and `cmd`.
fn run_command_call() -> Value {
    json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_run",
            "function": {"name": "run_command", "arguments": "{\"command\":\"echo ran > ran.txt\"}"},
        }]}}]
    })
}

/// An endpoint that answers each connection on its own thread.
///
/// A request that already carries a tool result gets a closing line of text.
/// Otherwise the model "asks" to read `marker.txt`, a relative path, so the
/// result shows which directory the agent resolved it against. A first request
/// mentioning [`HOLD`] streams one fragment and then waits for `release`
/// before asking for the file. One mentioning [`RUN`] asks for a command.
struct Endpoint {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
    release: Arc<AtomicBool>,
}

fn read_request(stream: &TcpStream) -> Option<Value> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
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
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn mentions(request: &Value, needle: &str) -> bool {
    request["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|m| m.to_string().contains(needle)))
}

fn has_tool_result(request: &Value) -> bool {
    request["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"))
}

fn start_endpoint() -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind endpoint");
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(AtomicBool::new(false));

    let (seen, gate) = (Arc::clone(&requests), Arc::clone(&release));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (seen, gate) = (Arc::clone(&seen), Arc::clone(&gate));
            std::thread::spawn(move || {
                let Some(request) = read_request(&stream) else {
                    return;
                };
                seen.lock().unwrap().push(request.clone());

                // No content-length: the body ends when the connection closes,
                // which lets a fragment go out before the rest is decided.
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                            connection: close\r\n\r\n";
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }

                let body = if has_tool_result(&request) {
                    sse(&[text_delta("done"), finish("stop")])
                } else {
                    if mentions(&request, HOLD) {
                        let first = sse(&[text_delta("holding")]);
                        if stream.write_all(first.as_bytes()).is_err() || stream.flush().is_err() {
                            return;
                        }
                        let deadline = Instant::now() + TIMEOUT;
                        while !gate.load(Ordering::Acquire) && Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                    let call = if mentions(&request, RUN) {
                        run_command_call()
                    } else {
                        read_marker_call()
                    };
                    sse(&[call, finish("tool_calls")])
                };
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.write_all(b"data: [DONE]\n\n");
                let _ = stream.flush();
            });
        }
    });

    Endpoint {
        port,
        requests,
        release,
    }
}

struct AgentUnderTest {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
    /// Messages read while waiting for something else, oldest first.
    seen: Vec<Value>,
}

fn spawn_agent(port: u16, config_dir: &std::path::Path) -> AgentUnderTest {
    spawn_agent_with_permissions(port, config_dir, "allow")
}

fn spawn_agent_with_permissions(
    port: u16,
    config_dir: &std::path::Path,
    permissions: &str,
) -> AgentUnderTest {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sigit"))
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
        .env("SIGIT_CONFIG_DIR", config_dir)
        .env("SIGIT_MCP", "off")
        .env("SIGIT_PERMISSIONS", permissions)
        .env_remove("SIGIT_LOCAL_INFERENCE")
        .env_remove("SIGIT_MAX_TOOL_ROUNDS")
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
        seen: Vec::new(),
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

    /// Read messages until one satisfies `wanted`, keeping every message read
    /// on the way in `seen`. Panics with `what` when none arrives in time.
    fn wait_for(&mut self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.incoming.recv_timeout(remaining) else {
                panic!("timed out waiting for {what}");
            };
            self.seen.push(message.clone());
            if wanted(&message) {
                return message;
            }
        }
    }

    fn wait_for_response(&mut self, id: u64) -> Value {
        let message = self.wait_for(&format!("the response to request {id}"), |message| {
            message["id"] == id && message.get("method").is_none()
        });
        assert!(
            message.get("error").is_none(),
            "request {id} failed: {message}"
        );
        message
    }

    /// Answer a request the agent sent, such as a permission request.
    fn respond(&mut self, id: &Value, result: Value) {
        let mut line = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).expect("write stdin");
        self.stdin.flush().expect("flush stdin");
    }

    fn close_session(&mut self, session_id: &str) -> u64 {
        self.request("session/close", json!({"sessionId": session_id}))
    }

    fn responded(&self, id: u64) -> bool {
        self.seen
            .iter()
            .any(|message| message["id"] == id && message.get("method").is_none())
    }

    fn new_session(&mut self, cwd: &std::path::Path) -> String {
        let id = self.request("session/new", json!({"cwd": cwd, "mcpServers": []}));
        self.wait_for_response(id)["result"]["sessionId"]
            .as_str()
            .expect("session id")
            .to_string()
    }

    fn send_prompt(&mut self, session_id: &str, text: &str) -> u64 {
        self.request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}),
        )
    }
}

impl Drop for AgentUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The tool result a follow-up request carries.
fn tool_result(request: &Value) -> String {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].to_string())
        .collect()
}

#[test]
fn a_second_session_opens_runs_and_finishes_while_the_first_is_streaming() {
    let dir = std::env::temp_dir().join(format!("sigit_acp_concurrent_{}", std::process::id()));
    let (work_a, work_b) = (dir.join("work-a"), dir.join("work-b"));
    for (work, marker) in [(&work_a, "marker-from-a"), (&work_b, "marker-from-b")] {
        std::fs::create_dir_all(work).unwrap();
        std::fs::write(work.join("marker.txt"), marker).unwrap();
    }
    std::fs::create_dir_all(dir.join("config")).unwrap();

    let endpoint = start_endpoint();
    let mut agent = spawn_agent(endpoint.port, &dir.join("config"));
    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    agent.wait_for_response(id);

    // Thread A starts a turn and the endpoint holds it open mid-stream.
    let session_a = agent.new_session(&work_a);
    let prompt_a = agent.send_prompt(&session_a, HOLD);
    agent.wait_for("thread A's first streamed fragment", |message| {
        let update = &message["params"]["update"];
        message["params"]["sessionId"] == session_a.as_str()
            && update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("holding"))
    });

    // Thread B opens and runs a whole turn, tool call included, meanwhile.
    let session_b = agent.new_session(&work_b);
    let prompt_b = agent.send_prompt(&session_b, "read the marker");
    let response_b = agent.wait_for_response(prompt_b);
    assert_eq!(response_b["result"]["stopReason"], "end_turn");
    assert!(
        !agent.responded(prompt_a),
        "thread A finished before it was released, so it was never mid-turn"
    );

    // Thread A picks up where it was, in its own directory.
    endpoint.release.store(true, Ordering::Release);
    let response_a = agent.wait_for_response(prompt_a);
    assert_eq!(response_a["result"]["stopReason"], "end_turn");

    let requests = endpoint.requests.lock().unwrap().clone();
    let follow_ups: Vec<&Value> = requests.iter().filter(|r| has_tool_result(r)).collect();
    assert_eq!(
        follow_ups.len(),
        2,
        "one tool round per thread: {requests:?}"
    );
    for request in follow_ups {
        let result = tool_result(request);
        if mentions(request, HOLD) {
            assert!(result.contains("marker-from-a"), "thread A read {result}");
        } else {
            assert!(result.contains("marker-from-b"), "thread B read {result}");
        }
    }
    // Neither thread's conversation reached the other's requests.
    for request in requests.iter().filter(|r| !mentions(r, HOLD)) {
        assert!(!mentions(request, "holding"), "{request}");
    }

    std::fs::remove_dir_all(&dir).ok();
}

fn initialized_agent(port: u16, config_dir: &std::path::Path, permissions: &str) -> AgentUnderTest {
    std::fs::create_dir_all(config_dir).unwrap();
    let mut agent = spawn_agent_with_permissions(port, config_dir, permissions);
    let id = agent.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    agent.wait_for_response(id);
    agent
}

#[test]
fn closing_a_session_mid_stream_ends_its_turn_and_runs_no_tool() {
    let dir = std::env::temp_dir().join(format!("sigit_acp_close_stream_{}", std::process::id()));
    let (work_a, work_b) = (dir.join("work-a"), dir.join("work-b"));
    for (work, marker) in [(&work_a, "marker-from-a"), (&work_b, "marker-from-b")] {
        std::fs::create_dir_all(work).unwrap();
        std::fs::write(work.join("marker.txt"), marker).unwrap();
    }

    let endpoint = start_endpoint();
    let mut agent = initialized_agent(endpoint.port, &dir.join("config"), "allow");

    // Thread B exists first, so thread A is the installed one when it closes.
    let session_b = agent.new_session(&work_b);
    let session_a = agent.new_session(&work_a);
    let prompt_a = agent.send_prompt(&session_a, HOLD);
    agent.wait_for("thread A's first streamed fragment", |message| {
        message["params"]["sessionId"] == session_a.as_str()
            && message["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
    });

    // The editor closes the thread while its turn is waiting on the endpoint.
    let close_a = agent.close_session(&session_a);
    let response_a = agent.wait_for_response(prompt_a);
    assert_eq!(response_a["result"]["stopReason"], "cancelled");
    agent.wait_for_response(close_a);

    // The endpoint now asks for the file. Nobody is left to read it.
    endpoint.release.store(true, Ordering::Release);

    // The id is gone, and a request for it gets an answer instead of waiting
    // behind anything the closed turn left held.
    let late = agent.send_prompt(&session_a, "anyone there");
    let refused = agent.wait_for("the answer to a prompt on the closed thread", |message| {
        message["id"] == late && message.get("method").is_none()
    });
    assert!(refused.get("error").is_some(), "{refused}");

    // Thread B still runs its tool in its own directory.
    let prompt_b = agent.send_prompt(&session_b, "read the marker");
    let response_b = agent.wait_for_response(prompt_b);
    assert_eq!(response_b["result"]["stopReason"], "end_turn");

    let requests = endpoint.requests.lock().unwrap().clone();
    let follow_ups: Vec<&Value> = requests.iter().filter(|r| has_tool_result(r)).collect();
    assert_eq!(
        follow_ups.len(),
        1,
        "only thread B ran a tool: {requests:?}"
    );
    let result = tool_result(follow_ups[0]);
    assert!(result.contains("marker-from-b"), "thread B read {result}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn closing_a_session_at_a_permission_prompt_ends_its_turn_without_an_answer() {
    let dir = std::env::temp_dir().join(format!("sigit_acp_close_ask_{}", std::process::id()));
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();

    let endpoint = start_endpoint();
    let mut agent = initialized_agent(endpoint.port, &dir.join("config"), "ask");

    let session = agent.new_session(&work);
    let prompt = agent.send_prompt(&session, RUN);
    let permission = agent.wait_for("the permission request", |message| {
        message["method"] == "session/request_permission"
    });

    // The client closes the thread and never answers the request. The turn
    // must not keep the close waiting on an answer that is not coming.
    let close = agent.close_session(&session);
    let response = agent.wait_for_response(prompt);
    assert_eq!(response["result"]["stopReason"], "cancelled");
    agent.wait_for_response(close);

    // An approval that arrives after the close runs nothing.
    agent.respond(
        &permission["id"],
        json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
    );
    let late = agent.send_prompt(&session, "anyone there");
    agent.wait_for("the answer to a prompt on the closed thread", |message| {
        message["id"] == late && message.get("method").is_none()
    });
    assert!(
        !work.join("ran.txt").exists(),
        "the command ran after the close"
    );

    std::fs::remove_dir_all(&dir).ok();
}
