//! The terminal UI's connection layer against the real binary.
//!
//! `acp_client` is what `sigit`'s TUI is built on: it starts `sigit` as an ACP
//! agent (`SIGIT_SURFACE=tui`) and reports what happens as events. This test
//! drives it with no terminal, against a scripted OpenAI-compatible endpoint
//! (the `OPENAI_BASE_URL` override), and checks the sequence a UI would render
//! for a turn with a `run_command` call, and that the commit trailer names the
//! `tui` surface.

// The module is the binary's own source, compiled here against the test's
// dependencies; the parts a test does not call are not dead in the binary.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ContentBlock, PermissionOptionKind, RequestPermissionOutcome, SelectedPermissionOutcome,
    SessionUpdate, StopReason, ToolCallStatus,
};
use serde_json::{Value, json};

/// `acp_client` names the one auth method by this id; the binary's root owns it.
const AUTH_METHOD_ID: &str = "sigit";

#[path = "../src/acp_client.rs"]
mod acp_client;

use acp_client::AgentEvent;

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

// ── Driving the connection ──────────────────────────────────────────────────

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sigit-tui-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// What a UI would have drawn, in order.
#[derive(Debug, PartialEq)]
enum Seen {
    ToolCall(ToolCallStatus),
    Permission,
    ToolUpdate(Option<ToolCallStatus>),
    Text(String),
    Done(StopReason),
}

/// Send `prompt` and collect events until the turn ends, answering any
/// permission request with "allow once" the way the TUI's `y` key does.
async fn run_turn(
    port: u16,
    config_dir: &std::path::Path,
    cwd: PathBuf,
    permissions: Option<&str>,
    prompt: &str,
) -> Vec<Seen> {
    let base_url = format!("http://127.0.0.1:{port}");
    let config_dir = config_dir.to_string_lossy().into_owned();
    let mut env = vec![
        ("OPENAI_BASE_URL", base_url.as_str()),
        ("OPENAI_API_KEY", "test-key"),
        ("SIGIT_MODEL", "scripted-model"),
        ("SIGIT_CONFIG_DIR", config_dir.as_str()),
        ("SIGIT_MCP", "off"),
    ];
    if let Some(mode) = permissions {
        env.push(("SIGIT_PERMISSIONS", mode));
    }

    let exe = PathBuf::from(env!("CARGO_BIN_EXE_sigit"));
    let prompt = prompt.to_string();
    tokio::time::timeout(
        TIMEOUT,
        acp_client::connect_to(
            exe,
            &env,
            vec![cwd],
            async move |agent, _started, mut rx| {
                agent.prompt(prompt);
                let mut seen = Vec::new();
                while let Some(event) = rx.recv().await {
                    match event {
                        AgentEvent::Update(session_id, update) => {
                            assert_eq!(session_id, agent.session_id);
                            match *update {
                                SessionUpdate::ToolCall(call) => {
                                    seen.push(Seen::ToolCall(call.status))
                                }
                                SessionUpdate::ToolCallUpdate(update) => {
                                    seen.push(Seen::ToolUpdate(update.fields.status));
                                }
                                SessionUpdate::AgentMessageChunk(chunk) => {
                                    if let ContentBlock::Text(text) = chunk.content {
                                        seen.push(Seen::Text(text.text));
                                    }
                                }
                                _ => {}
                            }
                        }
                        AgentEvent::Permission(request, answer) => {
                            seen.push(Seen::Permission);
                            let allow_once = request
                                .options
                                .iter()
                                .find(|o| o.kind == PermissionOptionKind::AllowOnce)
                                .expect("the agent offers allow-once");
                            let _ = answer.send(RequestPermissionOutcome::Selected(
                                SelectedPermissionOutcome::new(allow_once.option_id.clone()),
                            ));
                        }
                        AgentEvent::TurnDone(result) => {
                            seen.push(Seen::Done(result.expect("the turn succeeds")));
                            break;
                        }
                        _ => {}
                    }
                }
                Ok(seen)
            },
        ),
    )
    .await
    .expect("the turn ends in time")
    .expect("the connection works")
}

fn position(seen: &[Seen], wanted: impl Fn(&Seen) -> bool, what: &str) -> usize {
    seen.iter()
        .position(wanted)
        .unwrap_or_else(|| panic!("no {what} in {seen:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_turn_arrives_as_call_permission_update_text_done() {
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "run_command",
            r#"{"command":"echo from-the-tui"}"#,
        ),
        sse_text("all done"),
    ]);
    let temp = scratch_dir("turn");
    let cwd = temp.join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    let seen = run_turn(endpoint.port, &temp.join("config"), cwd, None, "run it").await;

    let call = position(&seen, |s| matches!(s, Seen::ToolCall(_)), "tool call");
    let permission = position(&seen, |s| *s == Seen::Permission, "permission request");
    let completed = position(
        &seen,
        |s| *s == Seen::ToolUpdate(Some(ToolCallStatus::Completed)),
        "completed tool update",
    );
    let text = position(&seen, |s| *s == Seen::Text("all done".into()), "final text");
    let done = position(&seen, |s| matches!(s, Seen::Done(_)), "turn end");

    assert!(
        call < permission && permission < completed && completed < text && text < done,
        "wrong order: {seen:?}"
    );
    assert_eq!(seen[done], Seen::Done(StopReason::EndTurn));
    // The call that will ask starts out waiting for approval.
    assert_eq!(seen[call], Seen::ToolCall(ToolCallStatus::Pending));

    // The command really ran and its output went back to the model.
    let requests = endpoint.requests.lock().unwrap();
    let second = serde_json::to_string(&requests[1]).unwrap();
    assert!(second.contains("from-the-tui"), "{second}");
    drop(requests);
    std::fs::remove_dir_all(&temp).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_made_in_the_tui_names_the_tui_surface() {
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git is installed");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let temp = scratch_dir("commit");
    let repo = temp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    // In the repo's own config, since the agent amends the trailer in with a
    // plain `git commit --amend` of its own.
    git(&repo, &["config", "user.name", "tui-test"]);
    git(&repo, &["config", "user.email", "tui@example.com"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    // The watch compares HEAD before and after, so there has to be a HEAD.
    git(&repo, &["commit", "--allow-empty", "-q", "-m", "initial"]);

    // A real change: git refuses to amend an empty commit.
    std::fs::write(repo.join("note.txt"), "hello\n").unwrap();
    let command = "git add note.txt && git commit -q -m from-the-tui";
    let endpoint = start_fake_endpoint(vec![
        sse_tool_call(
            "call_1",
            "run_command",
            // `run_command` defaults to the home directory, not the session's.
            &json!({ "command": command, "cwd": repo.display().to_string() }).to_string(),
        ),
        sse_text("committed"),
    ]);

    // Pre-approved, so the turn needs no answer from this test.
    let seen = run_turn(
        endpoint.port,
        &temp.join("config"),
        repo.clone(),
        Some("allow"),
        "commit",
    )
    .await;
    assert!(
        matches!(seen.last(), Some(Seen::Done(StopReason::EndTurn))),
        "{seen:?}"
    );

    let message = git(&repo, &["log", "-1", "--format=%B"]);
    assert!(
        message.contains("Co-Authored-By: siGit Code v") && message.contains("-tui"),
        "trailer should name the tui surface: {message}"
    );
    assert!(!message.contains("-acp"), "{message}");
    std::fs::remove_dir_all(&temp).ok();
}

// ── Hybrid: one conversation, two worlds ────────────────────────────────────

/// Read events until the turn ends; the stop reason, or the error text.
async fn until_turn_done(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
) -> Result<StopReason, String> {
    while let Some(event) = rx.recv().await {
        if let AgentEvent::TurnDone(result) = event {
            return result.map_err(|e| e.message);
        }
    }
    Err("the connection closed".to_string())
}

/// Switching from the hosted-endpoint override to a siGit Code Cloud tier in
/// the middle of a thread carries the conversation with it: the request to the
/// second endpoint holds the turn the first one served. Endpoint A is the
/// `OPENAI_BASE_URL` override, B is "siGit Code Cloud" (`SIGIT_CLOUD_URL`, with
/// a stored session), and the switch is the picker's own `set_config_option`.
#[tokio::test(flavor = "multi_thread")]
async fn switching_between_endpoints_keeps_the_conversation() {
    let a = start_fake_endpoint(vec![sse_text("noted, the code word is PELICAN")]);
    let b = start_fake_endpoint(vec![sse_text("it was PELICAN")]);
    let temp = scratch_dir("hybrid");
    let config = temp.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("credentials.toml"),
        "access_token = \"cloud-token\"\n",
    )
    .unwrap();
    let cwd = temp.join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    let a_url = format!("http://127.0.0.1:{}", a.port);
    let b_url = format!("http://127.0.0.1:{}", b.port);
    let config_dir = config.to_string_lossy().into_owned();
    let env = [
        ("OPENAI_BASE_URL", a_url.as_str()),
        ("OPENAI_API_KEY", "test-key"),
        ("SIGIT_MODEL", "scripted-model"),
        ("SIGIT_CLOUD_URL", b_url.as_str()),
        ("SIGIT_CONFIG_DIR", config_dir.as_str()),
        ("SIGIT_MCP", "off"),
    ];

    let exe = PathBuf::from(env!("CARGO_BIN_EXE_sigit"));
    let current_after_switch = tokio::time::timeout(
        TIMEOUT,
        acp_client::connect_to(
            exe,
            &env,
            vec![cwd],
            async move |agent, _started, mut rx| {
                agent.prompt("remember: the code word is PELICAN".to_string());
                until_turn_done(&mut rx).await.map_err(anyhow::Error::msg)?;

                agent.set_config(
                    "sigit-model",
                    acp_client::ConfigValue::Id("sigit-cloud:balanced".to_string()),
                );
                let options = loop {
                    match rx.recv().await {
                        Some(AgentEvent::ConfigSet(result)) => {
                            break result.map_err(anyhow::Error::msg)?;
                        }
                        Some(_) => {}
                        None => anyhow::bail!("the connection closed"),
                    }
                };
                let current = options
                    .iter()
                    .find(|option| option.id.0.as_ref() == "sigit-model")
                    .and_then(|option| match &option.kind {
                        agent_client_protocol::schema::v1::SessionConfigKind::Select(select) => {
                            Some(select.current_value.0.to_string())
                        }
                        _ => None,
                    })
                    .unwrap_or_default();

                agent.prompt("what was the code word?".to_string());
                until_turn_done(&mut rx).await.map_err(anyhow::Error::msg)?;
                Ok(current)
            },
        ),
    )
    .await
    .expect("the thread finishes in time")
    .expect("the connection works");

    assert_eq!(current_after_switch, "sigit-cloud:balanced");

    // A served the first turn only; B the second.
    assert_eq!(a.requests.lock().unwrap().len(), 1);
    let b_requests = b.requests.lock().unwrap();
    assert_eq!(b_requests.len(), 1);
    let body = serde_json::to_string(&b_requests[0]["messages"]).unwrap();
    assert!(
        body.contains("remember: the code word is PELICAN"),
        "B must see A's user turn: {body}"
    );
    assert!(
        body.contains("noted, the code word is PELICAN"),
        "B must see A's reply: {body}"
    );
    assert!(body.contains("what was the code word?"), "{body}");
    drop(b_requests);
    std::fs::remove_dir_all(&temp).ok();
}
