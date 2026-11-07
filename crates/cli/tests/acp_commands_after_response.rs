//! The command list must follow the `session/new` and `session/fork` responses.
//!
//! Zed creates a thread only once it has the response, and drops
//! `available_commands_update` for a session id it does not know yet. Sent
//! before the response, the list never arrives and Zed rejects `/whoami`
//! unless the user types a leading space. This reads the raw wire in order.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

struct Wire {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
}

fn spawn(config_dir: &std::path::Path) -> Wire {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sigit"))
        .env("OPENAI_BASE_URL", "http://127.0.0.1:9")
        .env("OPENAI_API_KEY", "test-key")
        .env("SIGIT_MODEL", "scripted-model")
        .env("SIGIT_CONFIG_DIR", config_dir)
        .env("SIGIT_MCP", "off")
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

    fn next(&mut self, what: &str) -> Value {
        self.incoming
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// Everything on the wire up to and including the response to `id`.
    fn until_response(&mut self, id: u64) -> (Value, Vec<Value>) {
        let mut before = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "no response to request {id}");
            let message = self.next(&format!("the response to request {id}"));
            if message["id"] == id && message.get("method").is_none() {
                assert!(
                    message.get("error").is_none(),
                    "request {id} failed: {message}"
                );
                return (message, before);
            }
            before.push(message);
        }
    }

    /// The next `available_commands_update` for `session_id`.
    fn commands_update_for(&mut self, session_id: &str) -> Value {
        loop {
            let message = self.next("available_commands_update");
            if message["method"] == "session/update"
                && message["params"]["sessionId"] == session_id
                && message["params"]["update"]["sessionUpdate"] == "available_commands_update"
            {
                return message["params"]["update"].clone();
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

fn is_commands_update_for(message: &Value, session_id: &str) -> bool {
    message["method"] == "session/update"
        && message["params"]["sessionId"] == session_id
        && message["params"]["update"]["sessionUpdate"] == "available_commands_update"
}

fn command_names(update: &Value) -> Vec<&str> {
    update["availableCommands"]
        .as_array()
        .expect("availableCommands")
        .iter()
        .filter_map(|command| command["name"].as_str())
        .collect()
}

#[test]
fn commands_follow_the_new_and_fork_responses() {
    let scratch = std::env::temp_dir().join(format!("sigit_acp_cmds_{}", std::process::id()));
    let config_dir = scratch.join("config");
    let project = scratch.join("project");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    let mut wire = spawn(&config_dir);
    let id = wire.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    wire.until_response(id);

    let id = wire.request("session/new", json!({"cwd": project, "mcpServers": []}));
    let (response, before) = wire.until_response(id);
    let session_id = response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !before
            .iter()
            .any(|m| is_commands_update_for(m, &session_id)),
        "the command list must not precede the session/new response: {before:#?}"
    );
    let update = wire.commands_update_for(&session_id);
    assert!(command_names(&update).contains(&"whoami"), "{update}");

    let id = wire.request(
        "session/fork",
        json!({"sessionId": session_id, "cwd": project, "mcpServers": []}),
    );
    let (response, before) = wire.until_response(id);
    let forked = response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(forked, session_id);
    assert!(
        !before.iter().any(|m| is_commands_update_for(m, &forked)),
        "the command list must not precede the session/fork response: {before:#?}"
    );
    let update = wire.commands_update_for(&forked);
    assert!(command_names(&update).contains(&"whoami"), "{update}");

    drop(wire);
    let _ = std::fs::remove_dir_all(&scratch);
}
