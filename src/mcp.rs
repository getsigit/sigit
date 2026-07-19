//! Model Context Protocol (MCP) client for siGit Code.
//!
//! Implements the client half of the [Model Context Protocol](https://modelcontextprotocol.io):
//! siGit Code connects to one or more MCP servers, discovers the tools they
//! expose, and surfaces those tools to the model alongside its built-in ones.
//! When the model calls an MCP tool, the call is forwarded to the owning server
//! and the result fed back into the agent loop.
//!
//! Transports:
//!
//! - **Streamable HTTP** — a single HTTP endpoint the client POSTs JSON-RPC 2.0
//!   messages to. The server answers either with a single `application/json`
//!   body or a `text/event-stream` (SSE) stream that carries the JSON-RPC
//!   response. Both are handled here. Configured with `url` in `mcp.toml`.
//! - **stdio** — siGit spawns the server as a child process and exchanges
//!   newline-delimited JSON-RPC messages over its stdin/stdout (the server's
//!   stderr flows into siGit's own log stream). Configured with `command`
//!   (plus optional `args` and `[server.env]`) in `mcp.toml`. This is how most
//!   published MCP servers (filesystem, Playwright, GitHub, ...) are run.
//!
//! `url` and `command` are mutually exclusive; an entry with both, or neither,
//! is a config error that is logged and skipped.
//!
//! ## Baked-in servers
//!
//! siGit Code bakes in its official MCP server at `<cloud>/mcp` (default
//! `https://sigit.si/api/v1/mcp`, following `SIGIT_CLOUD_URL`). When the user is
//! signed in (`sigit login`) the cloud session token is sent as a bearer
//! credential.
//!
//! The smbCloud CLI's MCP server (`smb --mcp`, stdio) is also baked in, but
//! only when the `smb` binary is actually on `PATH` — no binary, no entry, no
//! error. Opt out with `smbcloud = false` in `mcp.toml` or
//! `SIGIT_MCP_SMBCLOUD=off`.
//!
//! Both baked-in entries yield to a user-defined `mcp.toml` server of the same
//! name, so either can be repointed or reconfigured without a special case.
//! Additional servers are configured in `mcp.toml` (see [`load_configs`]).
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
//! stdio children live for the sigit process. When a child dies (EOF or an I/O
//! error on its pipes) the server is marked dead and later calls return an
//! in-band error string the model can react to; there is no automatic restart.
//! `/reload` does *not* re-run discovery ([`init`] is once-per-process), so a
//! changed `mcp.toml` or a dead server needs a sigit restart. At process exit
//! children see EOF on their stdin and exit on their own.
//!
//! Tools are namespaced `mcp__<server>__<tool>` so they never collide with
//! built-in tools or with each other across servers. This mirrors the
//! convention used by other MCP-aware agents.
//!
//! Like the rest of the backend seam, MCP is wired up only through the
//! interactive client and the ACP agent loop. On non-Unix targets a few helpers
//! are unused, so the dead-code lint is suppressed there only.
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};

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

/// Name of the baked-in smbCloud CLI server; its tools are namespaced
/// `mcp__smbcloud__<tool>`. Like the official server, a user-defined `mcp.toml`
/// entry with this name overrides the baked-in command line.
const SMBCLOUD_SERVER_NAME: &str = "smbcloud";

/// The smbCloud CLI binary the baked-in stdio entry spawns (`smb --mcp`).
const SMBCLOUD_COMMAND: &str = "smb";

/// The bare tool name when `name` belongs to the smbCloud server
/// (`mcp__smbcloud__project_list` → `Some("project_list")`), else `None`.
pub fn smbcloud_tool_suffix(name: &str) -> Option<&str> {
    name.strip_prefix(MCP_PREFIX)?
        .strip_prefix(SMBCLOUD_SERVER_NAME)?
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
    /// Display endpoint for the `/mcp` listing: the URL for HTTP servers, the
    /// command line for stdio servers.
    endpoint: String,
    /// The live transport. `None` when a stdio server failed to even spawn.
    transport: Option<Transport>,
    /// Tools discovered at startup. Empty when the server failed to connect.
    tools: Vec<McpTool>,
    /// Connection error, if the handshake failed. Surfaced by `/mcp`.
    error: Option<String>,
}

/// How a connected server is reached.
enum Transport {
    Http(HttpConn),
    Stdio(StdioConn),
}

/// Streamable HTTP connection state.
struct HttpConn {
    /// Streamable HTTP endpoint (the single POST URL).
    url: String,
    /// Extra headers sent on every request (e.g. `Authorization`).
    headers: Vec<(String, String)>,
    /// Session id handed back by the server on `initialize`, echoed on every
    /// later request via the `Mcp-Session-Id` header.
    session_id: Mutex<Option<String>>,
}

/// stdio connection state: a child process speaking newline-delimited JSON-RPC
/// over its stdin/stdout.
struct StdioConn {
    /// The child's stdin. The mutex serializes writes so concurrent requests
    /// can't interleave bytes on the pipe; `None` once the pipe broke.
    writer: Mutex<Option<ChildStdin>>,
    /// State shared with the background reader task that owns the child's
    /// stdout.
    shared: Arc<StdioShared>,
    /// JSON-RPC id source. Ids are per-connection so the reader task can route
    /// each response to the request that carries its id.
    next_id: AtomicI64,
}

/// State shared between a [`StdioConn`] and its background reader task.
struct StdioShared {
    /// Server name, for log lines.
    name: String,
    /// In-flight requests awaiting a response, keyed by JSON-RPC id. Dropping
    /// a sender (when the connection dies) wakes the waiter with an error.
    pending: StdMutex<HashMap<i64, oneshot::Sender<Value>>>,
    /// Why the connection is unusable, once it is (EOF, I/O error, kill).
    dead: StdMutex<Option<String>>,
    /// The child handle, kept so a dead/failed connection can kill and reap
    /// the process. Taken on death.
    child: StdMutex<Option<Child>>,
}

impl StdioShared {
    fn dead_reason(&self) -> Option<String> {
        self.dead.lock().unwrap().clone()
    }

    /// Mark the connection unusable: record the reason (first one wins), fail
    /// every in-flight request, and kill + reap the child, best effort.
    fn mark_dead(&self, reason: &str) {
        {
            let mut dead = self.dead.lock().unwrap();
            if dead.is_none() {
                *dead = Some(reason.to_string());
            }
        }
        // Dropping the senders wakes every waiter with a recv error.
        self.pending.lock().unwrap().clear();
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
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

/// A server entry as written in `mcp.toml`. Exactly one of `url` (Streamable
/// HTTP) or `command` (stdio) selects the transport.
#[derive(Debug, Deserialize)]
struct ServerEntry {
    name: String,
    /// Streamable HTTP endpoint. Mutually exclusive with `command`.
    #[serde(default)]
    url: Option<String>,
    /// stdio server executable. Mutually exclusive with `url`.
    #[serde(default)]
    command: Option<String>,
    /// Arguments for `command`.
    #[serde(default)]
    args: Vec<String>,
    /// Extra environment variables for `command`, added on top of the
    /// inherited environment.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Set `enabled = false` to keep an entry in the file but skip connecting.
    #[serde(default)]
    enabled: Option<bool>,
    /// Static headers, e.g. `Authorization = "Bearer ..."`. HTTP only.
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

impl ServerEntry {
    /// Resolve the entry's transport. `url` and `command` are mutually
    /// exclusive and exactly one is required; anything else is a config error.
    fn transport_def(&self) -> Result<TransportDef, String> {
        let url = self.url.as_deref().map(str::trim).filter(|v| !v.is_empty());
        let command = self
            .command
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        match (url, command) {
            (Some(_), Some(_)) => {
                Err("has both `url` and `command`; a server uses exactly one transport".to_string())
            }
            (None, None) => {
                Err("needs either `url` (Streamable HTTP) or `command` (stdio)".to_string())
            }
            (Some(url), None) => Ok(TransportDef::Http {
                url: url.to_string(),
                headers: self
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            }),
            (None, Some(command)) => Ok(TransportDef::Stdio {
                command: command.to_string(),
                args: self.args.clone(),
                env: self
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            }),
        }
    }
}

/// The `mcp.toml` schema.
#[derive(Debug, Default, Deserialize)]
struct McpFile {
    /// Include the baked-in official server. Defaults to `true`; set `false` to
    /// opt out.
    #[serde(default)]
    official: Option<bool>,
    /// Include the baked-in smbCloud CLI server (`smb --mcp`). Defaults to
    /// `true`; set `false` to opt out. Moot when `smb` isn't installed.
    #[serde(default)]
    smbcloud: Option<bool>,
    #[serde(default)]
    server: Vec<ServerEntry>,
}

/// How to reach a configured server, before connecting.
#[derive(Debug, Clone)]
enum TransportDef {
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
}

impl TransportDef {
    /// Human-readable endpoint for logs and the `/mcp` listing: the URL for
    /// HTTP, the command line for stdio.
    fn endpoint(&self) -> String {
        match self {
            TransportDef::Http { url, .. } => url.clone(),
            TransportDef::Stdio { command, args, .. } => {
                let mut line = command.clone();
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                line
            }
        }
    }
}

/// A resolved server definition, before connecting.
#[derive(Debug, Clone)]
struct ServerDef {
    name: String,
    transport: TransportDef,
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
    let mut include_smbcloud = true;
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
        if let Some(smbcloud) = parsed.smbcloud {
            include_smbcloud = smbcloud;
        }
        for entry in parsed.server {
            if entry.enabled == Some(false) {
                continue;
            }
            let name = sanitize(&entry.name);
            if name.is_empty() {
                log::warn!("mcp: skipping server with empty name in {}", path.display());
                continue;
            }
            let transport = match entry.transport_def() {
                Ok(transport) => transport,
                Err(error) => {
                    log::warn!(
                        "mcp: skipping server '{name}' in {}: {error}",
                        path.display()
                    );
                    continue;
                }
            };
            upsert(&mut defs, ServerDef { name, transport });
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
            transport: TransportDef::Http {
                url: official_url(),
                headers,
            },
        });
    }

    // The smbCloud server can also be disabled with SIGIT_MCP_SMBCLOUD=off.
    if let Ok(value) = std::env::var("SIGIT_MCP_SMBCLOUD")
        && matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        )
    {
        include_smbcloud = false;
    }

    // Add the baked-in smbCloud CLI server, with the same never-clobber rule as
    // the official one. Only when `smb` is actually installed: a hardwired
    // entry for a binary most users don't have would surface a spawn failure
    // in `/mcp` instead of just staying out of the way.
    if include_smbcloud && !defs.iter().any(|d| d.name == SMBCLOUD_SERVER_NAME) {
        if on_path(SMBCLOUD_COMMAND) {
            defs.push(ServerDef {
                name: SMBCLOUD_SERVER_NAME.to_string(),
                transport: TransportDef::Stdio {
                    command: SMBCLOUD_COMMAND.to_string(),
                    args: vec!["--mcp".to_string()],
                    env: Vec::new(),
                },
            });
        } else {
            log::debug!("mcp: `{SMBCLOUD_COMMAND}` not on PATH; skipping the smbcloud server");
        }
    }

    defs
}

/// Whether `binary` resolves to an executable file on `PATH` (with the
/// platform's executable suffix, `.exe` on Windows).
fn on_path(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let file = format!("{binary}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&path).any(|dir| !dir.as_os_str().is_empty() && dir.join(&file).is_file())
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
    let endpoint = def.transport.endpoint();
    let transport = match &def.transport {
        TransportDef::Http { url, headers } => Transport::Http(HttpConn {
            url: url.clone(),
            headers: headers.clone(),
            session_id: Mutex::new(None),
        }),
        TransportDef::Stdio { command, args, env } => {
            match spawn_stdio(&def.name, command, args, env) {
                Ok(conn) => Transport::Stdio(conn),
                Err(error) => {
                    return ServerConn {
                        name: def.name,
                        endpoint,
                        transport: None,
                        tools: Vec::new(),
                        error: Some(error),
                    };
                }
            }
        }
    };

    let mut conn = ServerConn {
        name: def.name,
        endpoint,
        transport: Some(transport),
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

    // A stdio child that failed its handshake is useless — kill it rather than
    // leave it running for the rest of the process.
    if let Some(error) = conn.error.clone()
        && let Some(Transport::Stdio(stdio)) = &conn.transport
    {
        stdio.shared.mark_dead(&error);
    }

    conn
}

/// The `initialize` request: negotiate protocol version and (on HTTP) capture
/// the session id from the response headers (handled inside [`post_rpc`]).
async fn initialize(http: &reqwest::Client, conn: &ServerConn) -> Result<(), String> {
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "sigit", "version": env!("CARGO_PKG_VERSION") }
    });
    rpc_request(http, conn, "initialize", params, HANDSHAKE_TIMEOUT).await?;
    Ok(())
}

/// The `notifications/initialized` notification. Servers expect it before
/// fielding requests; it carries no id and no response.
async fn notify_initialized(http: &reqwest::Client, conn: &ServerConn) -> Result<(), String> {
    rpc_notify(http, conn, "notifications/initialized", HANDSHAKE_TIMEOUT).await
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
        let result = rpc_request(http, conn, "tools/list", params, HANDSHAKE_TIMEOUT).await?;

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
    /// Send a `tools/call` and render the result into text. On HTTP, retries
    /// once after a re-`initialize` if the session was dropped (HTTP 404),
    /// which is how Streamable HTTP signals an expired session.
    async fn call(
        &self,
        server: &ServerConn,
        remote_name: &str,
        args: Value,
    ) -> Result<String, String> {
        let params = json!({ "name": remote_name, "arguments": args });
        let result = match &server.transport {
            None => return Err(format!("server '{}' is not connected", server.name)),
            Some(Transport::Stdio(stdio)) => {
                stdio.request("tools/call", params, CALL_TIMEOUT).await?
            }
            Some(Transport::Http(http_conn)) => {
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": 0,
                    "method": "tools/call",
                    "params": params
                });
                match post_rpc(&self.http, &server.name, http_conn, &body, CALL_TIMEOUT).await {
                    Ok(result) => result,
                    Err(error) if error.contains("returned 404") => {
                        // Session expired — drop it, re-handshake, and retry once.
                        *http_conn.session_id.lock().await = None;
                        initialize(&self.http, server).await?;
                        notify_initialized(&self.http, server).await?;
                        post_rpc(&self.http, &server.name, http_conn, &body, CALL_TIMEOUT).await?
                    }
                    Err(error) => return Err(error),
                }
            }
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

// ── Transport-generic JSON-RPC dispatch ─────────────────────────────────────

/// Send a JSON-RPC request over whichever transport the server uses and return
/// its `result`.
async fn rpc_request(
    http: &reqwest::Client,
    conn: &ServerConn,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    match &conn.transport {
        None => Err(format!("server '{}' is not connected", conn.name)),
        Some(Transport::Http(http_conn)) => {
            let body = json!({ "jsonrpc": "2.0", "id": 0, "method": method, "params": params });
            post_rpc(http, &conn.name, http_conn, &body, timeout).await
        }
        Some(Transport::Stdio(stdio)) => stdio.request(method, params, timeout).await,
    }
}

/// Send a JSON-RPC notification (no id, no response expected).
async fn rpc_notify(
    http: &reqwest::Client,
    conn: &ServerConn,
    method: &str,
    timeout: Duration,
) -> Result<(), String> {
    match &conn.transport {
        None => Err(format!("server '{}' is not connected", conn.name)),
        Some(Transport::Http(http_conn)) => {
            let body = json!({ "jsonrpc": "2.0", "method": method });
            post_notification(http, &conn.name, http_conn, &body, timeout).await
        }
        Some(Transport::Stdio(stdio)) => stdio.notify(method).await,
    }
}

// ── stdio JSON-RPC plumbing ─────────────────────────────────────────────────

/// Spawn a stdio MCP server and start its background reader task. The child's
/// stderr is inherited so it lands in sigit's own log stream; the given env
/// vars are added on top of the inherited environment.
fn spawn_stdio(
    name: &str,
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<StdioConn, String> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("failed to spawn `{command}`: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "child stdin was not captured".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "child stdout was not captured".to_string())?;

    let shared = Arc::new(StdioShared {
        name: name.to_string(),
        pending: StdMutex::new(HashMap::new()),
        dead: StdMutex::new(None),
        child: StdMutex::new(Some(child)),
    });
    tokio::spawn(stdio_reader(BufReader::new(stdout), Arc::clone(&shared)));

    Ok(StdioConn {
        writer: Mutex::new(Some(stdin)),
        shared,
        next_id: AtomicI64::new(1),
    })
}

/// Background task owning a stdio child's stdout: parses one JSON-RPC message
/// per line and routes each response to the pending request that carries its
/// id. Server-initiated requests and notifications (anything with a `method`)
/// are logged and ignored — siGit doesn't support server→client calls. On EOF
/// or a read error the connection is marked dead, which fails every in-flight
/// request and reaps the child.
async fn stdio_reader(mut stdout: BufReader<ChildStdout>, shared: Arc<StdioShared>) {
    let mut line = String::new();
    loop {
        line.clear();
        match stdout.read_line(&mut line).await {
            Ok(0) => {
                shared.mark_dead("server closed its stdout (process exited)");
                return;
            }
            Ok(_) => {}
            Err(error) => {
                shared.mark_dead(&format!("read error: {error}"));
                return;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("mcp: '{}' sent a non-JSON line: {error}", shared.name);
                continue;
            }
        };
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            log::debug!(
                "mcp: ignoring server-initiated '{method}' from '{}'",
                shared.name
            );
            continue;
        }
        let Some(id) = message.get("id").and_then(Value::as_i64) else {
            log::warn!(
                "mcp: '{}' sent a response without a usable id; ignoring",
                shared.name
            );
            continue;
        };
        let waiter = shared.pending.lock().unwrap().remove(&id);
        match waiter {
            Some(sender) => {
                let _ = sender.send(message);
            }
            None => log::debug!(
                "mcp: '{}' answered unknown/expired request id {id}; ignoring",
                shared.name
            ),
        }
    }
}

impl StdioConn {
    /// Send a JSON-RPC request and await its response, correlated by id. Fails
    /// fast (in-band, never panicking) when the child has died.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let name = &self.shared.name;
        if let Some(reason) = self.shared.dead_reason() {
            return Err(format!("stdio server '{name}' is not running: {reason}"));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (sender, receiver) = oneshot::channel();
        self.shared.pending.lock().unwrap().insert(id, sender);

        if let Err(error) = self.write_line(&body).await {
            self.shared.pending.lock().unwrap().remove(&id);
            self.shared.mark_dead(&error);
            return Err(format!("stdio server '{name}': {error}"));
        }

        let message = match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(message)) => message,
            // Our sender was dropped: the connection died mid-request.
            Ok(Err(_)) => {
                let reason = self
                    .shared
                    .dead_reason()
                    .unwrap_or_else(|| "connection closed".to_string());
                return Err(format!("stdio server '{name}' is not running: {reason}"));
            }
            Err(_) => {
                self.shared.pending.lock().unwrap().remove(&id);
                return Err(format!(
                    "request to stdio server '{name}' timed out after {}s",
                    timeout.as_secs()
                ));
            }
        };

        if let Some(error) = message.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let msg = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("'{name}' JSON-RPC error {code}: {msg}"));
        }
        message
            .get("result")
            .cloned()
            .ok_or_else(|| format!("response from '{name}' had no result"))
    }

    /// Send a JSON-RPC notification (no id, no response).
    async fn notify(&self, method: &str) -> Result<(), String> {
        let body = json!({ "jsonrpc": "2.0", "method": method });
        if let Err(error) = self.write_line(&body).await {
            self.shared.mark_dead(&error);
            return Err(format!("stdio server '{}': {error}", self.shared.name));
        }
        Ok(())
    }

    /// Write one newline-delimited JSON-RPC message. The writer mutex keeps
    /// concurrent requests from interleaving bytes on the pipe.
    async fn write_line(&self, body: &Value) -> Result<(), String> {
        let mut guard = self.writer.lock().await;
        let Some(writer) = guard.as_mut() else {
            return Err("stdin already closed".to_string());
        };
        let mut line = body.to_string();
        line.push('\n');
        let result = async {
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await
        }
        .await;
        if let Err(error) = result {
            // A broken pipe is unrecoverable; drop the writer so later calls
            // fail fast.
            *guard = None;
            return Err(format!("write failed: {error}"));
        }
        Ok(())
    }
}

// ── Streamable HTTP JSON-RPC plumbing ───────────────────────────────────────

/// POST a JSON-RPC request and return its `result`. Handles both an
/// `application/json` body and a `text/event-stream` (SSE) reply, captures the
/// session id from the response headers, and maps a JSON-RPC `error` to `Err`.
async fn post_rpc(
    http: &reqwest::Client,
    name: &str,
    conn: &HttpConn,
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
            "server '{name}' returned {}: {detail}",
            status.as_u16()
        ));
    }

    let text = response
        .text()
        .await
        .map_err(|error| format!("reading response from '{name}': {error}"))?;

    let message = if content_type.contains("text/event-stream") {
        parse_sse_response(&text)
            .ok_or_else(|| format!("no JSON-RPC message in SSE reply from '{name}'"))?
    } else {
        serde_json::from_str::<Value>(&text)
            .map_err(|error| format!("parsing response from '{name}': {error}"))?
    };

    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("'{name}' JSON-RPC error {code}: {msg}"));
    }

    message
        .get("result")
        .cloned()
        .ok_or_else(|| format!("response from '{name}' had no result"))
}

/// POST a JSON-RPC notification (no id, no response expected). A non-success
/// status is an error; an empty 202 body is the normal case.
async fn post_notification(
    http: &reqwest::Client,
    name: &str,
    conn: &HttpConn,
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
            "server '{name}' rejected notification: {}",
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
    conn: &HttpConn,
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
/// `/mcp` slash command. Shows the URL for HTTP servers and the command line
/// for stdio servers.
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
                server.name, server.endpoint
            )),
            None => {
                lines.push(format!(
                    "- {} ({}) — {} tool(s)",
                    server.name,
                    server.endpoint,
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
    fn smbcloud_tool_suffix_strips_only_the_smbcloud_namespace() {
        assert_eq!(
            smbcloud_tool_suffix("mcp__smbcloud__project_list"),
            Some("project_list")
        );
        assert_eq!(smbcloud_tool_suffix("mcp__sigit__project_list"), None);
        // `smbcloud` must be the whole server name, not a prefix of it.
        assert_eq!(smbcloud_tool_suffix("mcp__smbcloudx__project_list"), None);
        assert_eq!(smbcloud_tool_suffix("project_list"), None);
        assert_eq!(smbcloud_tool_suffix("mcp__smbcloud__"), Some(""));
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
        assert_eq!(parsed.smbcloud, None);
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
    fn parses_smbcloud_opt_out_flag() {
        let parsed: McpFile = toml::from_str("smbcloud = false").unwrap();
        assert_eq!(parsed.smbcloud, Some(false));
        // Absent means "include" (the default stays true in load_configs).
        let parsed: McpFile = toml::from_str("").unwrap();
        assert_eq!(parsed.smbcloud, None);
    }

    #[test]
    fn on_path_finds_real_binaries_only() {
        assert!(!on_path("definitely-not-a-real-binary-xyzzy"));
        #[cfg(unix)]
        assert!(on_path("sh"));
    }

    #[test]
    fn parses_stdio_server_with_args_and_env() {
        let toml = r#"
            [[server]]
            name = "fs"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

            [server.env]
            LOG_LEVEL = "debug"
            TOKEN = "abc"
        "#;
        let parsed: McpFile = toml::from_str(toml).unwrap();
        assert_eq!(parsed.server.len(), 1);
        let entry = &parsed.server[0];
        assert_eq!(entry.command.as_deref(), Some("npx"));
        assert_eq!(entry.args.len(), 3);
        assert_eq!(
            entry.env.get("LOG_LEVEL").map(String::as_str),
            Some("debug")
        );
        assert_eq!(entry.env.get("TOKEN").map(String::as_str), Some("abc"));

        let def = entry.transport_def().expect("valid stdio entry");
        match def {
            TransportDef::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args[0], "-y");
                assert_eq!(env.len(), 2);
            }
            TransportDef::Http { .. } => panic!("expected a stdio transport"),
        }
    }

    #[test]
    fn entry_with_url_and_command_is_a_config_error() {
        let toml = r#"
            [[server]]
            name = "confused"
            url = "https://example.com/mcp"
            command = "npx"
        "#;
        let parsed: McpFile = toml::from_str(toml).unwrap();
        let error = parsed.server[0].transport_def().unwrap_err();
        assert!(error.contains("both"), "unexpected error: {error}");
    }

    #[test]
    fn entry_with_neither_url_nor_command_is_a_config_error() {
        let toml = r#"
            [[server]]
            name = "empty"
        "#;
        let parsed: McpFile = toml::from_str(toml).unwrap();
        let error = parsed.server[0].transport_def().unwrap_err();
        assert!(error.contains("needs"), "unexpected error: {error}");
    }

    #[test]
    fn blank_url_or_command_counts_as_absent() {
        let toml = r#"
            [[server]]
            name = "blank"
            url = "  "
            command = "server-bin"
        "#;
        let parsed: McpFile = toml::from_str(toml).unwrap();
        // A blank url is treated as absent, so this resolves to stdio.
        match parsed.server[0].transport_def().expect("stdio") {
            TransportDef::Stdio { command, .. } => assert_eq!(command, "server-bin"),
            TransportDef::Http { .. } => panic!("expected stdio"),
        }
    }

    #[test]
    fn endpoint_renders_url_or_command_line() {
        let http = TransportDef::Http {
            url: "https://example.com/mcp".into(),
            headers: vec![],
        };
        assert_eq!(http.endpoint(), "https://example.com/mcp");

        let stdio = TransportDef::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "server-fs".into()],
            env: vec![],
        };
        assert_eq!(stdio.endpoint(), "npx -y server-fs");
    }

    #[test]
    fn upsert_replaces_same_name() {
        let mut defs = vec![ServerDef {
            name: "a".into(),
            transport: TransportDef::Http {
                url: "u1".into(),
                headers: vec![],
            },
        }];
        upsert(
            &mut defs,
            ServerDef {
                name: "a".into(),
                transport: TransportDef::Http {
                    url: "u2".into(),
                    headers: vec![],
                },
            },
        );
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].transport.endpoint(), "u2");
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
