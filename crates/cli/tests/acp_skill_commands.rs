//! Skills are slash commands: `/<skill-name> [task]` activates the skill and
//! runs a turn, and the name shows up in the client's command list.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

/// Answers every chat completion with a short reply and reports each request
/// body on the returned channel.
fn start_fake_endpoint() -> (u16, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake endpoint");
    let port = listener.local_addr().unwrap().port();
    let (bodies, received): (Sender<String>, Receiver<String>) = channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(clone) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(clone);
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
            let _ = bodies.send(String::from_utf8_lossy(&body).into_owned());
            let payload = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": {"content": "Done."}}]})
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, received)
}

struct Wire {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
}

fn spawn(port: u16, config_dir: &std::path::Path) -> Wire {
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
    let (tx, incoming) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && tx.send(message).is_err()
            {
                break;
            }
        }
    });
    let stdin = child.stdin.take().unwrap();
    Wire {
        child,
        stdin,
        incoming,
        next_id: 0,
    }
}

impl Wire {
    fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let mut line =
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
        id
    }

    /// The response to `id` and every `session/update` seen on the way.
    fn finish(&mut self, id: u64) -> (Value, Vec<Value>) {
        let mut updates = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(message) = self.incoming.recv_timeout(remaining) else {
                panic!("timed out waiting for the response to request {id}");
            };
            if message["method"] == "session/update" {
                updates.push(message["params"]["update"].clone());
            }
            if message["id"] == id && message.get("method").is_none() {
                assert!(
                    message.get("error").is_none(),
                    "request {id} failed: {message}"
                );
                return (message, updates);
            }
        }
    }

    fn next_commands_update(&mut self) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = self
                .incoming
                .recv_timeout(remaining)
                .expect("timed out waiting for available_commands_update");
            let update = &message["params"]["update"];
            if message["method"] == "session/update"
                && update["sessionUpdate"] == "available_commands_update"
            {
                return update.clone();
            }
        }
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_skill_is_advertised_and_runs_as_a_slash_command() {
    let scratch = std::env::temp_dir().join(format!("sigit_acp_skill_cmd_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let project = scratch.join("project");
    let skill_dir = config_dir.join("skills").join("zz-shelf-sorter");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    // A description past 100 characters, over two lines.
    let description = format!("Sort shelves.\n  {}", "Use it when asked. ".repeat(10));
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: zz-shelf-sorter\ndescription: \"{}\"\n---\n\nAlphabetise the shelves first.\n",
            description.replace('\n', " ")
        ),
    )
    .unwrap();

    let (port, requests) = start_fake_endpoint();
    let mut wire = spawn(port, &config_dir);
    let id = wire.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    wire.finish(id);

    let id = wire.request("session/new", json!({"cwd": project, "mcpServers": []}));
    let (response, _) = wire.finish(id);
    let session_id = response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let update = wire.next_commands_update();
    let commands = update["availableCommands"].as_array().unwrap();
    let skill = commands
        .iter()
        .find(|c| c["name"] == "zz-shelf-sorter")
        .unwrap_or_else(|| panic!("the skill must be advertised: {update}"));
    assert_eq!(skill["input"]["hint"], "task (optional)", "{skill}");
    let shown = skill["description"].as_str().unwrap();
    assert!(
        shown.chars().count() <= 100 && !shown.contains('\n'),
        "description must be one short line: {shown:?}"
    );

    let id = wire.request(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "/zz-shelf-sorter do the garage"}],
        }),
    );
    wire.finish(id);

    let body = requests
        .recv_timeout(TIMEOUT)
        .expect("the skill command should run a turn against the model endpoint");
    assert!(body.contains("Alphabetise the shelves first."), "{body}");
    assert!(body.contains("Task: do the garage"), "{body}");

    drop(wire);
    let _ = std::fs::remove_dir_all(&scratch);
}
