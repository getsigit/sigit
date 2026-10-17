//! Test-only MCP stdio server stub, used by `tests/mcp_stdio.rs`.
//!
//! Speaks the MCP stdio transport: newline-delimited JSON-RPC 2.0, one message
//! per line on stdin/stdout. It answers `initialize`, `tools/list` (a single
//! `echo` tool) and `tools/call` (echoes `text` back, prefixed with the
//! `STUB_PREFIX` env var so tests can verify env propagation).
//!
//! Flags that script failure modes:
//! - `--fail`: exit(1) immediately, before reading anything (a server whose
//!   process dies at spawn).
//! - `--exit-after-list`: exit(0) right after answering `tools/list` (a server
//!   that dies after discovery, exercising the dead-child call path).
//!
//! After the `initialize` response it also emits a server-initiated
//! notification the client must log and ignore.
//!
//! This binary is excluded from the published crate (see `exclude` in
//! `Cargo.toml`); it exists only for the integration tests.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--fail") {
        std::process::exit(1);
    }
    let exit_after_list = args.iter().any(|a| a == "--exit-after-list");

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let result = match method.as_str() {
            "initialize" => Some(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mcp-stdio-stub", "version": "0.0.0" }
            })),
            "tools/list" => Some(json!({
                "tools": [{
                    "name": "echo",
                    "description": "Echo the text back.",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                }]
            })),
            "tools/call" => {
                let text = message
                    .pointer("/params/arguments/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let prefix = std::env::var("STUB_PREFIX").unwrap_or_default();
                Some(json!({
                    "content": [{ "type": "text", "text": format!("{prefix}{text}") }]
                }))
            }
            // Notifications (`notifications/initialized`) and anything else
            // without a scripted answer fall through.
            _ => None,
        };

        let mut out = stdout.lock();
        match (id, result) {
            (Some(id), Some(result)) => {
                let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                writeln!(out, "{response}").ok();
            }
            (Some(id), None) => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" }
                });
                writeln!(out, "{response}").ok();
            }
            // A notification: nothing to answer.
            (None, _) => {}
        }
        if method == "initialize" {
            // Server-initiated traffic the client must ignore.
            let notification = json!({ "jsonrpc": "2.0", "method": "notifications/stub" });
            writeln!(out, "{notification}").ok();
        }
        out.flush().ok();

        if exit_after_list && method == "tools/list" {
            std::process::exit(0);
        }
    }
}
