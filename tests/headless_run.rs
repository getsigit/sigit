//! End-to-end `sigit run` (headless mode) against the real binary.
//!
//! Spawns `sigit run` wired to a scripted OpenAI-compatible endpoint via the
//! `OPENAI_BASE_URL` override and asserts on the JSONL event stream. Headless
//! runs use non-streaming completions (no token sink), so the endpoint serves
//! plain JSON chat-completion bodies, not SSE.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// One scripted JSON completion body.
fn completion_text(text: &str) -> String {
    json!({
        "choices": [{"message": {"role": "assistant", "content": text}}]
    })
    .to_string()
}

fn completion_tool_call(id: &str, name: &str, arguments: &str) -> String {
    json!({
        "choices": [{"message": {
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            }],
        }}]
    })
    .to_string()
}

/// Serves one scripted JSON response per request and records request bodies.
struct FakeEndpoint {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
}

fn start_fake_endpoint(responses: Vec<String>) -> FakeEndpoint {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake endpoint");
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let recorded = Arc::clone(&requests);
    let queue = Mutex::new(std::collections::VecDeque::from(responses));

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
                recorded.lock().unwrap().push(request);
            }
            let payload = queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| completion_text("out of scripted responses"));
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    FakeEndpoint { port, requests }
}

/// Run `sigit run` to completion against the endpoint; returns (exit code,
/// parsed JSONL events). A watchdog kills the child if it wedges.
fn run_headless(
    endpoint: &FakeEndpoint,
    workdir: &Path,
    extra_env: &[(&str, &str)],
) -> (i32, Vec<Value>) {
    let config_dir = workdir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_sigit"));
    command
        .arg("run")
        .arg("--prompt")
        .arg("do the task")
        .arg("--cwd")
        .arg(workdir)
        .arg("--output")
        .arg("jsonl")
        .env_remove("SIGIT_PERMISSIONS")
        .env_remove("SIGIT_MODEL")
        .env(
            "OPENAI_BASE_URL",
            format!("http://127.0.0.1:{}", endpoint.port),
        )
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MCP", "off")
        .env("SIGIT_CONFIG_DIR", &config_dir)
        .env("HOME", workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }

    let child = command.spawn().expect("spawn sigit run");

    // Watchdog: a wedged run must fail the test, not hang CI.
    let pid = child.id();
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(120));
        // Best-effort; on the happy path the process is long gone.
        #[cfg(unix)]
        unsafe {
            libc_kill(pid as i32);
        }
        let _ = pid;
    });

    let output = child.wait_with_output().expect("wait for sigit run");
    drop(watchdog); // detached; happy path never joins it

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let events: Vec<Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("non-JSONL stdout line {line:?}: {error}\nstderr: {stderr}")
            })
        })
        .collect();
    (output.status.code().unwrap_or(-1), events)
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid, 9);
    }
}

fn event_types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect()
}

#[test]
fn completes_a_tool_free_run() {
    let endpoint = start_fake_endpoint(vec![completion_text("All done: nothing to change.")]);
    let workdir = std::env::temp_dir().join(format!("sigit-headless-a-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).unwrap();

    let (code, events) = run_headless(&endpoint, &workdir, &[]);

    assert_eq!(code, 0, "events: {events:?}");
    let types = event_types(&events);
    assert_eq!(types.first(), Some(&"run_started"), "events: {events:?}");
    assert!(types.contains(&"turn_text"), "events: {events:?}");

    let result = events.last().expect("has a result line");
    assert_eq!(result["type"], "result");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["rounds"], 0);
    assert_eq!(result["summary"], "All done: nothing to change.");

    // The request carried the task and offered tools.
    let requests = endpoint.requests.lock().unwrap();
    let first = &requests[0];
    assert_eq!(first["stream"], false);
    assert!(
        first["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    let messages = first["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == "do the task")
    );

    std::fs::remove_dir_all(&workdir).ok();
}

#[test]
fn declines_mutating_tools_without_permission_override() {
    // Round 1: the model asks to run a mutating tool. With SIGIT_PERMISSIONS
    // unset the policy is `ask`, and headless mode cannot prompt — the call
    // must be declined (denied: true) and the refusal fed back to the model.
    let endpoint = start_fake_endpoint(vec![
        completion_tool_call("call_1", "run_command", "{\"command\":\"echo hi\"}"),
        completion_text("Understood, stopping."),
    ]);
    let workdir = std::env::temp_dir().join(format!("sigit-headless-b-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).unwrap();

    let (code, events) = run_headless(&endpoint, &workdir, &[]);

    assert_eq!(code, 0, "events: {events:?}");
    let tool_result = events
        .iter()
        .find(|event| event["type"] == "tool_result")
        .expect("tool_result event");
    assert_eq!(tool_result["name"], "run_command");
    assert_eq!(tool_result["denied"], true);
    assert!(
        tool_result["output"]
            .as_str()
            .unwrap()
            .contains("SIGIT_PERMISSIONS=allow")
    );

    // The refusal went back as the tool result of round 1's call.
    let requests = endpoint.requests.lock().unwrap();
    let second = &requests[1];
    let messages = second["messages"].as_array().unwrap();
    assert!(messages.iter().any(|m| {
        m["role"] == "tool"
            && m["content"]
                .as_str()
                .is_some_and(|content| content.contains("was not executed"))
    }));

    let result = events.last().unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["rounds"], 1);

    std::fs::remove_dir_all(&workdir).ok();
}

#[test]
fn executes_allowed_tools_with_permission_override() {
    let endpoint = start_fake_endpoint(vec![
        completion_tool_call(
            "call_1",
            "run_command",
            "{\"command\":\"echo headless-ok\"}",
        ),
        completion_text("Command ran."),
    ]);
    let workdir = std::env::temp_dir().join(format!("sigit-headless-c-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).unwrap();

    let (code, events) = run_headless(&endpoint, &workdir, &[("SIGIT_PERMISSIONS", "allow")]);

    assert_eq!(code, 0, "events: {events:?}");
    let tool_result = events
        .iter()
        .find(|event| event["type"] == "tool_result")
        .expect("tool_result event");
    assert_eq!(tool_result["denied"], false);
    assert!(
        tool_result["output"]
            .as_str()
            .unwrap()
            .contains("headless-ok"),
        "tool output should carry the command's stdout: {tool_result}"
    );

    std::fs::remove_dir_all(&workdir).ok();
}
