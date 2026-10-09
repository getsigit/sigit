//! Model Context Protocol (MCP) client for siGit Code.
//!
//! siGit Code connects to one or more [MCP](https://modelcontextprotocol.io)
//! servers, discovers the tools they expose, and surfaces those tools to the
//! model alongside its built-in ones. When the model calls an MCP tool, the
//! call is forwarded to the owning server and the result fed back into the
//! agent loop.
//!
//! The protocol itself is [`ed_mcp`]'s, which runs `rmcp`, the official Rust
//! SDK. This module decides *which* servers to connect and how their tools are
//! named and shown. Transports:
//!
//! - **Streamable HTTP**, configured with `url` in `mcp.toml`.
//! - **stdio**: siGit spawns the server as a child process (its stderr flows
//!   into siGit's own log stream). Configured with `command` (plus optional
//!   `args` and `[server.env]`) in `mcp.toml`. This is how most published MCP
//!   servers (filesystem, Playwright, GitHub, ...) are run.
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
//! stdio children live for the sigit process. When a child dies, later calls
//! return an in-band error string the model can react to; there is no
//! automatic restart. An HTTP server that stops answering (a restart, an
//! expired session) is reconnected once and the call retried.
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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ed_mcp::{ClientInfo, Connection, ServerSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

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

/// Per-server budget for the connect + `initialize` + `tools/list` handshake at
/// startup. Bounds how long an unreachable server can delay startup; servers are
/// contacted concurrently, so this is the worst case for the whole set, not the
/// sum.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);

/// Overall request timeout for an individual `tools/call`. Generous for build
/// and test tools, while the Xcode bridge gets a shorter bound below because
/// it otherwise leaves an ACP prompt looking permanently busy when Xcode
/// cannot service the request.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const XCODE_CALL_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// How it was reached, kept so an HTTP server can be reconnected.
    def: ServerDef,
    /// The live connection. `None` when the server failed to connect.
    conn: RwLock<Option<Arc<Connection>>>,
    /// Tools discovered at startup. Empty when the server failed to connect.
    tools: Vec<McpTool>,
    /// Connection error, if the handshake failed. Surfaced by `/mcp`.
    error: Option<String>,
}

/// The process-global MCP state: every configured server.
struct Mcp {
    servers: Vec<ServerConn>,
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

impl ServerDef {
    /// The `ed-mcp` spec for this server. An `Authorization: Bearer` header is
    /// passed as the bearer token, which is how `rmcp` wants it.
    fn spec(&self) -> ServerSpec {
        match &self.transport {
            TransportDef::Http { url, headers } => {
                headers
                    .iter()
                    .fold(
                        ServerSpec::http(&self.name, url),
                        |spec, (key, value)| match value.strip_prefix("Bearer ") {
                            Some(token) if key.eq_ignore_ascii_case("authorization") => {
                                spec.with_bearer(token)
                            }
                            _ => spec.with_header(key, value),
                        },
                    )
            }
            TransportDef::Stdio { command, args, env } => {
                ServerSpec::stdio(&self.name, command, args.clone(), env.clone())
            }
        }
    }

    fn is_stdio(&self) -> bool {
        matches!(self.transport, TransportDef::Stdio { .. })
    }
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

/// Global escape hatch: `SIGIT_MCP=off` disables MCP entirely, including
/// servers an ACP client supplies per session.
fn disabled_by_env() -> bool {
    std::env::var("SIGIT_MCP").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no" | "disabled"
        )
    })
}

/// Resolve the full set of servers to connect to: the baked-in official server
/// (unless opted out) plus any from `mcp.toml`. Project-local entries override
/// global ones, and a user entry named `sigit` overrides the official default.
fn load_configs() -> Vec<ServerDef> {
    if disabled_by_env() {
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

    // Contact servers concurrently so one slow/unreachable host doesn't serialize
    // the rest. Each handshake is bounded by HANDSHAKE_TIMEOUT.
    let servers = futures::future::join_all(load_configs().into_iter().map(connect)).await;

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

    let _ = MCP.set(Mcp { servers });
}

/// Sent to every server as `clientInfo`.
fn client_info() -> ClientInfo {
    ClientInfo::new("sigit", env!("CARGO_PKG_VERSION"))
}

/// Run the handshake against one server and collect its tools. Always returns a
/// `ServerConn`; failures land in its `error` field rather than propagating.
async fn connect(def: ServerDef) -> ServerConn {
    let endpoint = def.transport.endpoint();
    let (conn, tools, error) =
        match Connection::connect(&def.spec(), &client_info(), HANDSHAKE_TIMEOUT).await {
            Ok(conn) => {
                let tools = server_tools(&def.name, &conn);
                (Some(Arc::new(conn)), tools, None)
            }
            Err(error) => (None, Vec::new(), Some(format!("{error:#}"))),
        };
    ServerConn {
        name: def.name.clone(),
        endpoint,
        def,
        conn: RwLock::new(conn),
        tools,
        error,
    }
}

/// The tools `conn` listed, in siGit's flattened form.
fn server_tools(server: &str, conn: &Connection) -> Vec<McpTool> {
    conn.tools()
        .iter()
        .map(|tool| {
            let remote_name = tool.name.to_string();
            let full_name = format!("{MCP_PREFIX}{server}__{}", sanitize(&remote_name));
            if full_name.chars().count() > 64 {
                log::warn!(
                    "mcp: tool name '{full_name}' exceeds 64 chars; some backends may reject it"
                );
            }
            let remote_desc = tool.description.as_deref().unwrap_or("").trim();
            let description = if remote_desc.is_empty() {
                format!("[MCP server '{server}'] {remote_name}")
            } else {
                format!("[MCP server '{server}'] {remote_desc}")
            };
            let mut schema = Value::Object((*tool.input_schema).clone());
            if schema.get("type").is_none() {
                schema["type"] = json!("object");
            }
            McpTool {
                full_name,
                remote_name,
                description,
                parameters_schema: schema.to_string(),
            }
        })
        .collect()
}

// ── Client-supplied servers (per session) ───────────────────────────────────

/// A server an ACP client passed in `mcpServers` on a session request.
#[derive(Debug, Clone)]
pub struct ClientServer {
    pub name: String,
    pub transport: ClientTransport,
}

/// How a client-supplied server is reached. stdio is the transport every ACP
/// agent must support; Streamable HTTP is the optional one siGit Code
/// advertises through `mcpCapabilities.http`.
#[derive(Debug, Clone)]
pub enum ClientTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// The MCP servers one ACP session brought with it, connected.
///
/// Unlike the startup servers in [`MCP`], these belong to a session: the
/// client names them on `session/new` (or load/fork) and they are offered to
/// that session only. Cheap to clone; the connections are shared. When the
/// last clone goes away the connections close, which stops stdio children.
#[derive(Clone, Default)]
pub struct SessionServers(Arc<Vec<ServerConn>>);

impl std::fmt::Debug for SessionServers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|server| &server.name))
            .finish()
    }
}

/// The live session's client-supplied servers. One process serves every
/// thread the editor has open, so like the roots in `workspace.rs` this
/// always belongs to the session `main.rs` has installed.
static SESSION_SERVERS: StdMutex<Option<SessionServers>> = StdMutex::new(None);

/// Connect to the servers a client supplied for one session. Best effort, like
/// [`init`]: a server that fails its handshake is kept with its error so `/mcp`
/// can show it, and contributes no tools.
///
/// A name already taken by a startup server (or by an earlier entry in the same
/// list) is not connected. Its tools would share the `mcp__<server>__` prefix
/// with the other server's, and a call could not be routed.
pub async fn connect_session_servers(servers: Vec<ClientServer>) -> SessionServers {
    if servers.is_empty() {
        return SessionServers::default();
    }
    if disabled_by_env() {
        log::info!(
            "mcp: ignoring {} client-supplied server(s), disabled via SIGIT_MCP",
            servers.len()
        );
        return SessionServers::default();
    }

    let mut taken: Vec<String> = MCP
        .get()
        .map(|mcp| mcp.servers.iter().map(|s| s.name.clone()).collect())
        .unwrap_or_default();

    let mut clashes = Vec::new();
    let mut defs = Vec::new();
    for server in servers {
        let name = sanitize(&server.name);
        let transport = match server.transport {
            ClientTransport::Stdio { command, args, env } => {
                TransportDef::Stdio { command, args, env }
            }
            ClientTransport::Http { url, headers } => TransportDef::Http { url, headers },
        };
        if name.is_empty() || taken.contains(&name) {
            clashes.push(ServerConn {
                endpoint: transport.endpoint(),
                error: Some(if name.is_empty() {
                    "the client sent a server with no name".to_string()
                } else {
                    format!("another MCP server is already named '{name}'")
                }),
                def: ServerDef {
                    name: name.clone(),
                    transport,
                },
                conn: RwLock::new(None),
                tools: Vec::new(),
                name,
            });
            continue;
        }
        taken.push(name.clone());
        defs.push(ServerDef { name, transport });
    }

    let mut connected = futures::future::join_all(defs.into_iter().map(connect)).await;
    connected.extend(clashes);

    for server in &connected {
        match &server.error {
            Some(error) => log::warn!(
                "mcp: client-supplied server '{}' unavailable: {error}",
                server.name
            ),
            None => log::info!(
                "mcp: client-supplied server '{}' ready, {} tool(s)",
                server.name,
                server.tools.len()
            ),
        }
    }

    SessionServers(Arc::new(connected))
}

/// Make `servers` the live session's set. Called whenever `main.rs` installs a
/// session, so a thread never sees another thread's servers.
pub fn set_session_servers(servers: SessionServers) {
    *SESSION_SERVERS.lock().unwrap() = Some(servers);
}

fn session_servers() -> SessionServers {
    SESSION_SERVERS.lock().unwrap().clone().unwrap_or_default()
}

// ── Tool exposure + dispatch ────────────────────────────────────────────────

/// Whether a tool name belongs to MCP. The dispatch in `tools::execute_tool`
/// uses this to route a call here.
pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with(MCP_PREFIX)
}

/// All discovered MCP tools as agent [`ToolSpec`]s, ready to append to the
/// built-in tool list: the startup servers' tools, then the ones the live
/// session's client supplied. Empty when no server exposed any.
pub fn tool_specs() -> Vec<ToolSpec> {
    let session = session_servers();
    let startup = MCP.get().map(|mcp| mcp.servers.as_slice()).unwrap_or(&[]);
    let mut specs = Vec::new();
    for server in startup.iter().chain(session.0.iter()) {
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
    let session = session_servers();
    let startup = MCP.get().map(|mcp| mcp.servers.as_slice()).unwrap_or(&[]);
    if MCP.get().is_none() && session.0.is_empty() {
        return "Error: MCP is not initialized.".to_string();
    }

    let Some((server, tool)) = startup.iter().chain(session.0.iter()).find_map(|s| {
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

    match call_server(server, &tool.remote_name, args).await {
        Ok(text) => truncate(text),
        Err(error) => format!("Error: {error}"),
    }
}

/// Send a `tools/call` and render the result into text. An HTTP server whose
/// call fails for any reason but a timeout is reconnected and the call retried
/// once: that is how a restarted server or an expired session recovers.
async fn call_server(
    server: &ServerConn,
    remote_name: &str,
    args: Value,
) -> Result<String, String> {
    let timeout = if server.name == "xcode" {
        XCODE_CALL_TIMEOUT
    } else {
        CALL_TIMEOUT
    };
    let kind = if server.def.is_stdio() {
        "stdio server"
    } else {
        "server"
    };
    let Some(conn) = server.conn.read().await.clone() else {
        return Err(format!("{kind} '{}' is not connected", server.name));
    };
    let result = match conn.call_tool(remote_name, args.clone(), timeout).await {
        Ok(result) => result,
        Err(error) if server.def.is_stdio() || error.to_string().contains("timed out") => {
            return Err(format!("{kind} '{}': {error:#}", server.name));
        }
        Err(error) => {
            log::info!("mcp: '{}' failed ({error:#}); reconnecting", server.name);
            let fresh = Connection::connect(&server.def.spec(), &client_info(), HANDSHAKE_TIMEOUT)
                .await
                .map(Arc::new)
                .map_err(|again| {
                    format!(
                        "server '{}': {error:#}; reconnecting failed: {again:#}",
                        server.name
                    )
                })?;
            *server.conn.write().await = Some(fresh.clone());
            fresh
                .call_tool(remote_name, args, timeout)
                .await
                .map_err(|error| format!("server '{}': {error:#}", server.name))?
        }
    };
    Ok(render_tool_result(&result))
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

// ── Status reporting (`/mcp`) ────────────────────────────────────────────────

/// Human-readable summary of configured MCP servers and their tools, for the
/// `/mcp` slash command. Shows the URL for HTTP servers and the command line
/// for stdio servers.
pub fn status_summary() -> String {
    let session = session_servers();
    let Some(mcp) = MCP.get() else {
        if session.0.is_empty() {
            return "MCP is not initialized.".to_string();
        }
        return summarize_servers(&[], &session.0);
    };
    if mcp.servers.is_empty() && session.0.is_empty() {
        return "No MCP servers configured. Add one in ~/.config/sigit/mcp.toml \
                or .sigit/mcp.toml. See https://modelcontextprotocol.io."
            .to_string();
    }
    summarize_servers(&mcp.servers, &session.0)
}

/// The `/mcp` listing for the startup servers followed by the ones the live
/// session's client supplied, which are marked as such.
fn summarize_servers(startup: &[ServerConn], session: &[ServerConn]) -> String {
    let all = || startup.iter().chain(session.iter());
    let total_tools: usize = all().map(|s| s.tools.len()).sum();
    let mut lines = vec![format!(
        "{} MCP server(s), {total_tools} tool(s) available:",
        all().count()
    )];
    for (index, server) in all().enumerate() {
        let origin = if index >= startup.len() {
            ", from the editor"
        } else {
            ""
        };
        match &server.error {
            Some(error) => lines.push(format!(
                "- {} ({}{origin}) — unavailable: {error}",
                server.name, server.endpoint
            )),
            None => {
                lines.push(format!(
                    "- {} ({}{origin}) — {} tool(s)",
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
