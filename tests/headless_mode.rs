//! End-to-end headless (`sigit -p`) runs against the real binary.
//!
//! Same harness idea as `tests/acp_permissions.rs`: a scripted OpenAI-compatible
//! SSE endpoint stands in for the model via the `OPENAI_BASE_URL` override, and
//! the BUILT binary is driven like CI would drive it — one `-p` invocation, then
//! assertions on the exit code, stdout, and what the endpoint saw.
//!
//! 1. With `--allow-tool run_command`, the scripted tool call executes and its
//!    output travels back to the endpoint as a `role: "tool"` result.
//! 2. Without `--allow-tool`, the ask-level tool is denied, the denial is fed
//!    to the model, and the run still exits 0 (the turn completed).
//! 3. An unknown flag is a usage error: exit 2, nothing hits the endpoint.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

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

fn sse_tool_call(id: &str, name: &str, arguments: &str) -> String {
    sse_body(&[json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "function": {"name": name, "arguments": arguments},
        }]}}]
    })])
}

fn sse_text(text: &str) -> String {
    sse_body(&[json!({"choices": [{"delta": {"content": text}}]})])
}

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
        // `connection: close` below means one request per connection, so the
        // serial accept loop matches the agent's serial completion requests.
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
                .unwrap_or_else(|| sse_text("out of scripted responses"));
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

// ── Running the binary ──────────────────────────────────────────────────────

/// A scratch layout for one test: an isolated config dir and working dir.
struct Scratch {
    root: std::path::PathBuf,
    config_dir: std::path::PathBuf,
    cwd: std::path::PathBuf,
}

fn scratch(tag: &str) -> Scratch {
    let root = std::env::temp_dir().join(format!("sigit_headless_{tag}_{}", std::process::id()));
    let config_dir = root.join("config");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    Scratch {
        root,
        config_dir,
        cwd,
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Run `sigit` headless with `args` against the fake endpoint and collect the
/// full output. Stdin is null: headless mode must not depend on a TTY.
fn run_headless(port: u16, scratch: &Scratch, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sigit"))
        .args(args)
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
        .env("SIGIT_CONFIG_DIR", &scratch.config_dir)
        .env("SIGIT_MCP", "off")
        // A fresh config dir means the default permission mode, `ask` — make
        // sure the environment can't turn the gate off underneath the test.
        .env_remove("SIGIT_PERMISSIONS")
        .env_remove("SIGIT_LOCAL_INFERENCE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run sigit -p")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

// ── The runs ────────────────────────────────────────────────────────────────

#[test]
fn allowed_tool_runs_and_result_reaches_the_endpoint() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call("call_1", "run_command", r#"{"command":"echo headless-ok"}"#),
        sse_text("finished: headless-final-answer"),
    ]);
    let scratch = scratch("allow");
    let cwd = scratch.cwd.to_str().unwrap().to_string();

    let output = run_headless(
        endpoint.port,
        &scratch,
        &[
            "-p",
            "do the thing",
            "--allow-tool",
            "run_command",
            "--cwd",
            &cwd,
        ],
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout_of(&output).contains("finished: headless-final-answer"),
        "final answer must reach stdout, got: {}",
        stdout_of(&output)
    );
    // Tool progress goes to stderr, not stdout.
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("run_command"),
        "tool progress should be reported on stderr"
    );

    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "expected exactly two completions");

    // The first request carries the user's prompt.
    let messages = requests[0]["messages"].as_array().expect("messages");
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == "do the thing"),
        "prompt must reach the endpoint"
    );

    // The second request carries the executed tool's real output.
    let messages = requests[1]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "call_1")
        .expect("tool result for the approved call");
    assert!(
        result["content"]
            .as_str()
            .unwrap_or_default()
            .contains("headless-ok"),
        "the command's output should reach the endpoint: {result}"
    );

    // The conversation is saved under the "headless" session id for resume.
    assert!(
        scratch
            .config_dir
            .join("sessions")
            .join("headless.jsonl")
            .is_file(),
        "headless session must be persisted"
    );
}

#[test]
fn ask_level_tool_without_allow_flag_is_denied_but_run_completes() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "run_command",
            r#"{"command":"echo must-not-run"}"#,
        ),
        sse_text("understood, stopping"),
    ]);
    let scratch = scratch("deny");
    let cwd = scratch.cwd.to_str().unwrap().to_string();

    let output = run_headless(
        endpoint.port,
        &scratch,
        &["-p", "try the command", "--cwd", &cwd],
    );

    // A denial is fed to the model, not treated as a failure: the turn still
    // completes, so the exit code is 0.
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout_of(&output).contains("understood, stopping"),
        "final answer must reach stdout, got: {}",
        stdout_of(&output)
    );

    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "expected exactly two completions");

    // The endpoint sees the denial as the tool result — including the
    // --allow-tool hint — and never the command's real output.
    let messages = requests[1]["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "call_1")
        .expect("tool result for the denied call");
    let content = result["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("was not executed") && content.contains("--allow-tool"),
        "denial text should reach the model and mention --allow-tool: {content}"
    );
    assert!(
        !content.contains("must-not-run"),
        "the denied command must not have executed: {content}"
    );
}

#[test]
fn unknown_flag_exits_2_with_usage() {
    let endpoint = start_fake_endpoint(vec![]);
    let scratch = scratch("usage");

    let output = run_headless(endpoint.port, &scratch, &["-p", "x", "--frobnicate"]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--frobnicate") && stderr.contains("Usage:"),
        "usage must be printed to stderr: {stderr}"
    );
    assert!(
        endpoint.requests.lock().unwrap().is_empty(),
        "a bad invocation must never reach the endpoint"
    );
}
