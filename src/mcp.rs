//! Model Context Protocol (MCP) client for siGit Code.
//!
//! Implements the client half of the [Model Context Protocol](https://modelcontextprotocol.io):
//! siGit Code connects to one or more MCP servers, discovers the tools they
//! expose, and surfaces those tools to the model alongside its built-in ones.
//! When the model calls an MCP tool, the call is forwarded to the owning server
//! and the result fed back into the agent loop.
//!
//! Transport: the modern **Streamable HTTP** transport — a single HTTP endpoint
//! the client POSTs JSON-RPC 2.0 messages to. The server answers either with a
//! single `application/json` body or a `text/event-stream` (SSE) stream that
//! carries the JSON-RPC response. Both are handled here. stdio transport is not
//! supported (siGit Code never spawns child processes for inference).
//!
//! ## The official server
//!
//! siGit Code bakes in its official MCP server at `<cloud>/mcp` (default
//! `https://sigit.si/api/v1/mcp`, following `SIGIT_CLOUD_URL`). When the user is
//! signed in (`sigit login`) the cloud session token is sent as a bearer
//! credential. Additional servers are configured in `mcp.toml` (see
//! [`load_configs`]).
//!
//! ## Lifecycle
//!
//! Discovery is best-effort and happens once at startup via [`init`]: each
//! configured server is contacted concurrently (with a per-server timeout),
//! runs the `initialize` handshake, and has its `tools/list` cached. A server
//! that fails to connect is recorded with its error and simply contributes no
//! tools — it never blocks startup or the rest of the agent. The result is
//! stored in a process-global so the synchronous tool-spec builders
//! ([`tool_specs`]) and the async dispatch ([`call_tool`]) can both read it.
//!
//! Tools are namespaced `mcp__<server>__<tool>` so they never collide with
//! built-in tools or with each other across servers. This mirrors the
//! convention used by other MCP-aware agents.
//!
//! Like the rest of the backend seam, MCP is wired up only through the
//! interactive client and the ACP agent loop. On non-Unix targets a few helpers
//! are unused, so the dead-code lint is suppressed there only.
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::backend::ToolSpec;

/// Prefix marking a tool as MCP-provided. The full name is
/// `mcp__<server>__<tool>`.
pub const MCP_PREFIX: &str = "mcp__";

/// Name of the baked-in official siGit Code server; its tools are namespaced
/// `mcp__sigit__<tool>`. A user-defined `mcp.toml` entry with this name
/// overrides the baked-in URL/headers but keeps the namespace, so callers of
/// [`official_tool_name`] reach whatever the user pointed `sigit` at.
pub const OFFICIAL_SERVER_NAME: &str = "sigit";

/// The full namespaced name of a tool on the official server, e.g.
/// `official_tool_name("list_issues")` → `mcp__sigit__list_issues`.
pub fn official_tool_name(tool: &str) -> String {
    format!("{MCP_PREFIX}{OFFICIAL_SERVER_NAME}__{tool}")
}

/// The bare tool name when `name` belongs to the official server
/// (`mcp__sigit__list_issues` → `Some("list_issues")`), else `None`.
pub fn official_tool_suffix(name: &str) -> Option<&str> {
    name.strip_prefix(MCP_PREFIX)?
        .strip_prefix(OFFICIAL_SERVER_NAME)?
        .strip_prefix("__")
}

/// JSON-RPC / MCP protocol version we advertise in the handshake.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Per-server budget for the connect + `initialize` + `tools/list` handshake at
/// startup. Bounds how long an unreachable server can delay startup; servers are
/// contacted concurrently, so this is the worst case for the whole set, not the
/// sum.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);

/// Overall request timeout for an individual `tools/call`. Generous, since an
/// MCP tool may do real work server-side.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Cap on the characters returned from a single tool call, so a chatty server
/// can't blow up the model's context. Matches the spirit of the file-read cap.
const RESULT_CHAR_LIMIT: usize = 30_000;

// ── Public types ──────────────────────────────────────────────────────────────

/// A tool discovered on an MCP server, in siGit's flattened form.
#[derive(Debug, Clone)]
struct McpTool {
    /// Namespaced name exposed to the model: `mcp__<server>__<tool>`.
    full_name: String,
    /// The tool's name as the server knows it (sent back in `tools/call`).
    remote_name: String,
    /// Human/model-facing description, prefixed with the server name.
    description: String,
    /// JSON Schema for the tool's arguments, encoded as a string.
    parameters_schema: String,
}

/// A configured MCP server and its live connection state.
struct ServerConn {
    /// Sanitized server name used in tool namespacing and the `/mcp` listing.
    name: String,
    /// Streamable HTTP endpoint (the single POST URL).
    url: String,
    /// Extra headers sent on every request (e.g. `Authorization`).
    headers: Vec<(String, String)>,
    /// Session id handed back by the server on `initialize`, echoed on every
    /// later request via the `Mcp-Session-Id` header.
    session_id: Mutex<Option<String>>,
    /// Tools discovered at startup. Empty when the server failed to connect.
    tools: Vec<McpTool>,
    /// Connection error, if the handshake failed. Surfaced by `/mcp`.
    error: Option<String>,
}

/// The process-global MCP state: a shared HTTP client plus every configured
/// server.
struct Mcp {
    http: reqwest::Client,
    servers: Vec<ServerConn>,
    next_id: AtomicI64,
}

static MCP: OnceLock<Mcp> = OnceLock::new();

// ── Configuration ───────────────────────────────────────────────────────────

/// Default endpoint of the official siGit Code MCP server, derived from the
/// cloud base URL so `SIGIT_CLOUD_URL` (dev) carries over.
fn official_url() -> String {
    format!(
        "{}/mcp",
        crate::provider::cloud_base_url().trim_end_matches('/')
    )
}

/// A server entry as written in `mcp.toml`.
#[derive(Debug, Deserialize)]
struct ServerEntry {
    name: String,
    url: String,
    /// Set `enabled = false` to keep an entry in the file but skip connecting.
    #[serde(default)]
    enabled: Option<bool>,
    /// Static headers, e.g. `Authorization = "Bearer ..."`.
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// The `mcp.toml` schema.
#[derive(Debug, Default, Deserialize)]
struct McpFile {
    /// Include the baked-in official server. Defaults to `true`; set `false` to
    /// opt out.
    #[serde(default)]
    official: Option<bool>,
    #[serde(default)]
    server: Vec<ServerEntry>,
}

/// A resolved server definition, before connecting.
#[derive(Debug, Clone)]
struct ServerDef {
    name: String,
    url: String,
    headers: Vec<(String, String)>,
}

/// Config files to read, in priority order (later wins on a name clash):
/// global `$SIGIT_CONFIG_DIR/mcp.toml`, then project-local `<cwd>/.sigit/mcp.toml`.
fn config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = sigit_config_dir() {
        paths.push(dir.join("mcp.toml"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join(".sigit").join("mcp.toml"));
    }
    paths
}

fn sigit_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SIGIT_CONFIG_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config").join("sigit"))
}

/// Resolve the full set of servers to connect to: the baked-in official server
/// (unless opted out) plus any from `mcp.toml`. Project-local entries override
/// global ones, and a user entry named `sigit` overrides the official default.
fn load_configs() -> Vec<ServerDef> {
    // Global escape hatch: `SIGIT_MCP=off` disables MCP entirely.
    if let Ok(value) = std::env::var("SIGIT_MCP")
        && matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no" | "disabled"
        )
    {
        log::info!("mcp: disabled via SIGIT_MCP");
        return Vec::new();
    }

    let mut include_official = true;
    // De-duplicated by sanitized name; a later config file overrides an earlier
    // one for the same name (project-local wins over global).
    let mut defs: Vec<ServerDef> = Vec::new();

    for path in config_paths() {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed: McpFile = match toml::from_str(&contents) {
            Ok(parsed) => parsed,
            Err(error) => {
                log::warn!("mcp: ignoring {}: {error}", path.display());
                continue;
            }
        };
        if let Some(official) = parsed.official {
            include_official = official;
        }
        for entry in parsed.server {
            if entry.enabled == Some(false) {
                continue;
            }
            let name = sanitize(&entry.name);
            if name.is_empty() || entry.url.trim().is_empty() {
                log::warn!(
                    "mcp: skipping server with empty name/url in {}",
                    path.display()
                );
                continue;
            }
            let headers = entry.headers.into_iter().collect();
            upsert(
                &mut defs,
                ServerDef {
                    name,
                    url: entry.url.trim().to_string(),
                    headers,
                },
            );
        }
    }

    // The official server can also be disabled with SIGIT_MCP_OFFICIAL=off.
    if let Ok(value) = std::env::var("SIGIT_MCP_OFFICIAL")
        && matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        )
    {
        include_official = false;
    }

    // Add the baked-in official server, but never clobber a user-defined entry
    // named `sigit` — an explicit config (e.g. a custom URL or headers) wins.
    if include_official && !defs.iter().any(|d| d.name == OFFICIAL_SERVER_NAME) {
        let mut headers = Vec::new();
        if let Some(token) = crate::credentials::load_token() {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
        defs.push(ServerDef {
            name: OFFICIAL_SERVER_NAME.to_string(),
            url: official_url(),
            headers,
        });
    }

    defs
}

/// Insert `def`, replacing any existing entry with the same name.
fn upsert(defs: &mut Vec<ServerDef>, def: ServerDef) {
    if let Some(slot) = defs.iter_mut().find(|d| d.name == def.name) {
        *slot = def;
    } else {
        defs.push(def);
    }
}

/// Sanitize a name into the `[a-zA-Z0-9_-]` set tool names are restricted to,
/// collapsing anything else to `_`.
fn sanitize(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ── Startup / discovery ─────────────────────────────────────────────────────

/// Connect to every configured server and cache the tools they expose. Idempotent
/// and best-effort: a server that can't be reached is recorded with its error and
/// contributes no tools. Safe to call from either entry point; only the first
/// call does work.
pub async fn init() {
    if MCP.get().is_some() {
        return;
    }

    let defs = load_configs();
    let http = reqwest::Client::builder()
        .timeout(CALL_TIMEOUT)
        .user_agent(concat!(
            "sigit/",
            env!("CARGO_PKG_VERSION"),
            " (mcp-client)"
        ))
        .build()
        .unwrap_or_default();

    // Contact servers concurrently so one slow/unreachable host doesn't serialize
    // the rest. Each handshake is bounded by HANDSHAKE_TIMEOUT.
    let connects = defs.into_iter().map(|def| {
        let http = http.clone();
        async move { connect(&http, def).await }
    });
    let servers = futures::future::join_all(connects).await;

    for server in &servers {
        match &server.error {
            Some(error) => log::warn!("mcp: server '{}' unavailable: {error}", server.name),
            None => log::info!(
                "mcp: server '{}' ready, {} tool(s)",
                server.name,
                server.tools.len()
            ),
        }
    }

    let _ = MCP.set(Mcp {
        http,
        servers,
        next_id: AtomicI64::new(1),
    });
}

/// Run the handshake against one server and collect its tools. Always returns a
/// `ServerConn`; failures land in its `error` field rather than propagating.
async fn connect(http: &reqwest::Client, def: ServerDef) -> ServerConn {
    let mut conn = ServerConn {
        name: def.name.clone(),
        url: def.url.clone(),
        headers: def.headers.clone(),
        session_id: Mutex::new(None),
        tools: Vec::new(),
        error: None,
    };

    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        // initialize → notifications/initialized → tools/list
        initialize(http, &conn).await?;
        notify_initialized(http, &conn).await?;
        list_tools(http, &conn).await
    })
    .await;

    match handshake {
        Ok(Ok(tools)) => conn.tools = tools,
        Ok(Err(error)) => conn.error = Some(error),
        Err(_) => conn.error = Some(format!("timed out after {}s", HANDSHAKE_TIMEOUT.as_secs())),
    }

    conn
}

/// The `initialize` request: negotiate protocol version and capture the session
/// id from the response headers (handled inside [`post_rpc`]).
async fn initialize(http: &reqwest::Client, conn: &ServerConn) -> Result<(), String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "sigit", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    post_rpc(http, conn, &body, HANDSHAKE_TIMEOUT).await?;
    Ok(())
}

/// The `notifications/initialized` notification. Servers expect it before
/// fielding requests; it carries no id and yields a 202 with no body.
async fn notify_initialized(http: &reqwest::Client, conn: &ServerConn) -> Result<(), String> {
    let body = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    post_notification(http, conn, &body, HANDSHAKE_TIMEOUT).await
}

/// `tools/list`, following `nextCursor` pagination, mapped into [`McpTool`]s.
async fn list_tools(http: &reqwest::Client, conn: &ServerConn) -> Result<Vec<McpTool>, String> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let body = json!({ "jsonrpc": "2.0", "id": 0, "method": "tools/list", "params": params });
        let result = post_rpc(http, conn, &body, HANDSHAKE_TIMEOUT).await?;

        for tool in result
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(remote_name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            let full_name = format!("{MCP_PREFIX}{}__{}", conn.name, sanitize(remote_name));
            if full_name.chars().count() > 64 {
                log::warn!(
                    "mcp: tool name '{full_name}' exceeds 64 chars; some backends may reject it"
                );
            }
            let remote_desc = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let description = if remote_desc.is_empty() {
                format!("[MCP server '{}'] {remote_name}", conn.name)
            } else {
                format!("[MCP server '{}'] {remote_desc}", conn.name)
            };
            // `inputSchema` is a JSON Schema object; default to a permissive
            // object schema when a server omits it.
            let parameters_schema = tool
                .get("inputSchema")
                .filter(|schema| schema.is_object())
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }))
                .to_string();

            tools.push(McpTool {
                full_name,
                remote_name: remote_name.to_string(),
                description,
                parameters_schema,
            });
        }

        cursor = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }

    Ok(tools)
}

// ── Tool exposure + dispatch ────────────────────────────────────────────────

/// Whether a tool name belongs to MCP. The dispatch in `tools::execute_tool`
/// uses this to route a call here.
pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with(MCP_PREFIX)
}

/// All discovered MCP tools as agent [`ToolSpec`]s, ready to append to the
/// built-in tool list. Empty when MCP is uninitialized or no server exposed any.
pub fn tool_specs() -> Vec<ToolSpec> {
    let Some(mcp) = MCP.get() else {
        return Vec::new();
    };
    let mut specs = Vec::new();
    for server in &mcp.servers {
        for tool in &server.tools {
            specs.push(ToolSpec {
                name: tool.full_name.clone(),
                description: tool.description.clone(),
                parameters_schema: tool.parameters_schema.clone(),
            });
        }
    }
    specs
}

/// Execute an MCP tool call by name, returning text to feed back to the model.
/// Errors are returned as plain strings (never panics) so a failing tool degrades
/// to a message the model can react to, exactly like the built-in tools.
pub async fn call_tool(full_name: &str, arguments: &str) -> String {
    let Some(mcp) = MCP.get() else {
        return "Error: MCP is not initialized.".to_string();
    };

    let Some((server, tool)) = mcp.servers.iter().find_map(|s| {
        s.tools
            .iter()
            .find(|t| t.full_name == full_name)
            .map(|t| (s, t))
    }) else {
        return format!("Error: unknown MCP tool \"{full_name}\".");
    };

    // Arguments arrive as a JSON-encoded string; an empty/blank string means no
    // arguments. Anything that isn't a JSON object is a model mistake.
    let args: Value = if arguments.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(arguments) {
            Ok(value @ Value::Object(_)) => value,
            Ok(_) => return "Error: tool arguments must be a JSON object.".to_string(),
            Err(error) => return format!("Error: failed to parse arguments: {error}"),
        }
    };

    match mcp.call(server, &tool.remote_name, args).await {
        Ok(text) => truncate(text),
        Err(error) => format!("Error: {error}"),
    }
}

impl Mcp {
    /// Send a `tools/call` and render the result into text. Retries once after a
    /// re-`initialize` if the session was dropped (HTTP 404), which is how
    /// Streamable HTTP signals an expired session.
    async fn call(
        &self,
        server: &ServerConn,
        remote_name: &str,
        args: Value,
    ) -> Result<String, String> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "tools/call",
            "params": { "name": remote_name, "arguments": args }
        });

        let result = match post_rpc(&self.http, server, &body, CALL_TIMEOUT).await {
            Ok(result) => result,
            Err(error) if error.contains("returned 404") => {
                // Session expired — drop it, re-handshake, and retry once.
                *server.session_id.lock().await = None;
                initialize(&self.http, server).await?;
                notify_initialized(&self.http, server).await?;
                post_rpc(&self.http, server, &body, CALL_TIMEOUT).await?
            }
            Err(error) => return Err(error),
        };

        Ok(render_tool_result(&result))
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Flatten an MCP `tools/call` result into text. Joins text content blocks;
/// notes non-text blocks; honors `isError`.
fn render_tool_result(result: &Value) -> String {
    let mut out = String::new();
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                }
                Some(other) => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&format!("[{other} content omitted]"));
                }
                None => {}
            }
        }
    }

    // Some servers return only `structuredContent`; surface it if there was no
    // textual content.
    if out.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        out = structured.to_string();
    }

    if out.is_empty() {
        out = "(tool returned no content)".to_string();
    }

    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        format!("Tool reported an error:\n{out}")
    } else {
        out
    }
}

/// Truncate tool output to the context-protecting limit, with a trailing note.
fn truncate(text: String) -> String {
    if text.chars().count() <= RESULT_CHAR_LIMIT {
        return text;
    }
    let kept: String = text.chars().take(RESULT_CHAR_LIMIT).collect();
    format!("{kept}\n\n[output truncated to {RESULT_CHAR_LIMIT} characters]")
}

// ── Streamable HTTP JSON-RPC plumbing ───────────────────────────────────────

/// POST a JSON-RPC request and return its `result`. Handles both an
/// `application/json` body and a `text/event-stream` (SSE) reply, captures the
/// session id from the response headers, and maps a JSON-RPC `error` to `Err`.
async fn post_rpc(
    http: &reqwest::Client,
    conn: &ServerConn,
    body: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    // Give every outbound request a fresh id; the on-the-wire id in `body` is a
    // placeholder we overwrite so callers don't have to thread a counter.
    let mut body = body.clone();
    if body.get("id").is_some()
        && let Some(mcp) = MCP.get()
    {
        body["id"] = json!(mcp.next_id());
    }

    let response = build_request(http, conn, &body, timeout)
        .await
        .send()
        .await
        .map_err(|error| format!("request to {} failed: {error}", conn.url))?;

    // Persist the session id the server assigns on initialize.
    if let Some(session) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
    {
        *conn.session_id.lock().await = Some(session);
    }

    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();

    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(500).collect();
        return Err(format!(
            "server '{}' returned {}: {detail}",
            conn.name,
            status.as_u16()
        ));
    }

    let text = response
        .text()
        .await
        .map_err(|error| format!("reading response from '{}': {error}", conn.name))?;

    let message = if content_type.contains("text/event-stream") {
        parse_sse_response(&text)
            .ok_or_else(|| format!("no JSON-RPC message in SSE reply from '{}'", conn.name))?
    } else {
        serde_json::from_str::<Value>(&text)
            .map_err(|error| format!("parsing response from '{}': {error}", conn.name))?
    };

    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("'{}' JSON-RPC error {code}: {msg}", conn.name));
    }

    message
        .get("result")
        .cloned()
        .ok_or_else(|| format!("response from '{}' had no result", conn.name))
}

/// POST a JSON-RPC notification (no id, no response expected). A non-success
/// status is an error; an empty 202 body is the normal case.
async fn post_notification(
    http: &reqwest::Client,
    conn: &ServerConn,
    body: &Value,
    timeout: Duration,
) -> Result<(), String> {
    let response = build_request(http, conn, body, timeout)
        .await
        .send()
        .await
        .map_err(|error| format!("notification to {} failed: {error}", conn.url))?;
    if !response.status().is_success() {
        return Err(format!(
            "server '{}' rejected notification: {}",
            conn.name,
            response.status().as_u16()
        ));
    }
    Ok(())
}

/// Build a request carrying the MCP headers: the dual `Accept`, the JSON body,
/// the configured static headers, the negotiated protocol version, and the
/// session id once we have one.
async fn build_request(
    http: &reqwest::Client,
    conn: &ServerConn,
    body: &Value,
    timeout: Duration,
) -> reqwest::RequestBuilder {
    let mut request = http
        .post(&conn.url)
        .timeout(timeout)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .json(body);

    for (key, value) in &conn.headers {
        request = request.header(key.as_str(), value.as_str());
    }
    if let Some(session) = conn.session_id.lock().await.as_ref() {
        request = request.header("Mcp-Session-Id", session.as_str());
    }
    request
}

/// Extract the first JSON-RPC message from an SSE body. SSE frames are separated
/// by blank lines; each `data:` line contributes to the frame's payload. For a
/// single request/response exchange the server sends one `message` event whose
/// data is the JSON-RPC response.
fn parse_sse_response(body: &str) -> Option<Value> {
    let mut data = String::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.trim().is_empty() && !data.is_empty() {
            // End of an event — try to parse it as a JSON-RPC message.
            if let Ok(value) = serde_json::from_str::<Value>(&data)
                && (value.get("result").is_some() || value.get("error").is_some())
            {
                return Some(value);
            }
            data.clear();
        }
    }
    // Trailing event without a closing blank line.
    if !data.is_empty()
        && let Ok(value) = serde_json::from_str::<Value>(&data)
        && (value.get("result").is_some() || value.get("error").is_some())
    {
        return Some(value);
    }
    None
}

// ── Status reporting (`/mcp`) ────────────────────────────────────────────────

/// Human-readable summary of configured MCP servers and their tools, for the
/// `/mcp` slash command.
pub fn status_summary() -> String {
    let Some(mcp) = MCP.get() else {
        return "MCP is not initialized.".to_string();
    };
    if mcp.servers.is_empty() {
        return "No MCP servers configured. Add one in ~/.config/sigit/mcp.toml \
                or .sigit/mcp.toml. See https://modelcontextprotocol.io."
            .to_string();
    }

    let total_tools: usize = mcp.servers.iter().map(|s| s.tools.len()).sum();
    let mut lines = vec![format!(
        "{} MCP server(s), {total_tools} tool(s) available:",
        mcp.servers.len()
    )];
    for server in &mcp.servers {
        match &server.error {
            Some(error) => lines.push(format!(
                "- {} ({}) — unavailable: {error}",
                server.name, server.url
            )),
            None => {
                lines.push(format!(
                    "- {} ({}) — {} tool(s)",
                    server.name,
                    server.url,
                    server.tools.len()
                ));
                for tool in &server.tools {
                    lines.push(format!("    • {}", tool.full_name));
                }
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_mcp_tool_detects_prefix() {
        assert!(is_mcp_tool("mcp__sigit__search"));
        assert!(!is_mcp_tool("read_file"));
        assert!(!is_mcp_tool("skill"));
    }

    #[test]
    fn official_tool_name_matches_the_namespacing_convention() {
        assert_eq!(official_tool_name("list_issues"), "mcp__sigit__list_issues");
        assert_eq!(
            official_tool_name("get_pull_request"),
            "mcp__sigit__get_pull_request"
        );
    }

    #[test]
    fn official_tool_suffix_strips_only_the_official_namespace() {
        assert_eq!(
            official_tool_suffix("mcp__sigit__list_issues"),
            Some("list_issues")
        );
        assert_eq!(official_tool_suffix("mcp__other__list_issues"), None);
        // `sigit` must be the whole server name, not a prefix of it.
        assert_eq!(official_tool_suffix("mcp__sigitx__list_issues"), None);
        assert_eq!(official_tool_suffix("list_issues"), None);
        assert_eq!(official_tool_suffix("mcp__sigit__"), Some(""));
    }

    #[test]
    fn sanitize_collapses_invalid_chars() {
        assert_eq!(sanitize("github"), "github");
        assert_eq!(sanitize("my server"), "my_server");
        assert_eq!(sanitize("a.b/c:d"), "a_b_c_d");
        assert_eq!(sanitize("keep-_ok9"), "keep-_ok9");
    }

    #[test]
    fn parses_mcp_file_with_servers() {
        let toml = r#"
            official = false

            [[server]]
            name = "github"
            url = "https://api.example.com/mcp"

            [[server]]
            name = "disabled-one"
            url = "https://nope.example.com/mcp"
            enabled = false

            [server.headers]
            Authorization = "Bearer xyz"
        "#;
        let parsed: McpFile = toml::from_str(toml).unwrap();
        assert_eq!(parsed.official, Some(false));
        assert_eq!(parsed.server.len(), 2);
        assert_eq!(parsed.server[0].name, "github");
        assert_eq!(parsed.server[1].enabled, Some(false));
        assert_eq!(
            parsed.server[1]
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer xyz")
        );
    }

    #[test]
    fn upsert_replaces_same_name() {
        let mut defs = vec![ServerDef {
            name: "a".into(),
            url: "u1".into(),
            headers: vec![],
        }];
        upsert(
            &mut defs,
            ServerDef {
                name: "a".into(),
                url: "u2".into(),
                headers: vec![],
            },
        );
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].url, "u2");
    }

    #[test]
    fn parse_sse_extracts_jsonrpc_response() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let value = parse_sse_response(body).expect("a message");
        assert_eq!(value["result"]["ok"], json!(true));
    }

    #[test]
    fn parse_sse_handles_no_trailing_blank_line() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
        assert!(parse_sse_response(body).is_some());
    }

    #[test]
    fn parse_sse_ignores_non_response_frames() {
        // A lone notification (no result/error) shouldn't be mistaken for the response.
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\n";
        assert!(parse_sse_response(body).is_none());
    }

    #[test]
    fn render_result_joins_text_blocks() {
        let result = json!({
            "content": [
                { "type": "text", "text": "line one" },
                { "type": "text", "text": "line two" }
            ]
        });
        assert_eq!(render_tool_result(&result), "line one\nline two");
    }

    #[test]
    fn render_result_marks_errors_and_non_text() {
        let result = json!({
            "isError": true,
            "content": [
                { "type": "text", "text": "boom" },
                { "type": "image", "data": "..." }
            ]
        });
        let rendered = render_tool_result(&result);
        assert!(rendered.starts_with("Tool reported an error:"));
        assert!(rendered.contains("boom"));
        assert!(rendered.contains("[image content omitted]"));
    }

    #[test]
    fn render_result_falls_back_to_structured_content() {
        let result = json!({ "structuredContent": { "value": 42 } });
        assert!(render_tool_result(&result).contains("42"));
    }

    #[test]
    fn truncate_caps_long_output() {
        let long = "x".repeat(RESULT_CHAR_LIMIT + 100);
        let out = truncate(long);
        assert!(out.contains("[output truncated"));
    }

    #[test]
    fn tool_specs_empty_before_init() {
        // Without init() the global is unset; this must not panic.
        assert!(super::tool_specs().is_empty() || MCP.get().is_some());
    }
}
