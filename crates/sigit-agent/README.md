# sigit-agent

siGit Code as a library, for apps that embed the agent instead of launching the
[`sigit`](https://crates.io/crates/sigit) binary.

It is an [`ed-acp`](https://crates.io/crates/ed-acp) agent profile. A host serves it in
process over an in-memory channel and talks to it with an
[Agent Client Protocol](https://agentclientprotocol.com) client on the other end. That is
what lets a sandboxed app (a Mac App Store build, an iOS app) run the agent without spawning
a subprocess.

## What is in the crate

- `SiGitCode`, the `ed_acp::Profile`: the system prompt, the workspace tools, the `/init`
  command and the sign-in. `SiGitCode::new()` includes `run_command`.
  `SiGitCode::without_shell()` is for a host that cannot start processes. It drops
  `run_command` and tells the model so.
- `SiGitAuth`, the profile's `AuthProvider`. It runs the browser sign-in (a public OAuth
  client with PKCE) and keeps the resulting session in the `credentials` module.
- `official_mcp_servers()`, the official siGit MCP server as a session server for the
  signed-in user. It is empty when signed out. Pass it in `mcpServers` when you open a
  session, so a sign-out does not leave a stale bearer behind in a shared server.
- `prompt`, which holds `SYSTEM_PROMPT` and `INIT_PROMPT`. The `sigit` binary uses the same
  ones.

Inference goes to siGit Code Cloud (`https://sigit.si/api/v1`, or `SIGIT_CLOUD_URL`) with the
stored session token as the bearer.

## Serving it

```rust
use std::sync::Arc;

use agent_client_protocol::Channel;
use ed_acp::ServeOptions;
use sigit_agent::SiGitCode;

let (agent_end, client_end) = Channel::duplex();
let opts = ServeOptions { yolo: false, surface: "app" };

tokio::spawn(ed_acp::serve(Arc::new(SiGitCode::without_shell()), opts, agent_end));
// Connect an ACP client to `client_end`, then initialize, authenticate and open a session.
```

`ed_acp::serve` is a future, so this runs inside the host's own async runtime. The example
assumes a `tokio` runtime, so the host needs `tokio` as a dependency.

`crates/sigit-agent/tests/embedded.rs` is a complete version of this against a mock
endpoint: it checks that the sign-in method is advertised, that a signed-out session is
refused, that a stored session reaches the endpoint as the bearer, and that signing out takes
it away again.

## Where the session lives

Call `credentials::use_config_dir` with your app's own data directory before anything else.
In a sandboxed app that directory is inside the container, so the app and the `sigit` CLI
never share or overwrite each other's session.

## Environment

| Variable | Effect |
| --- | --- |
| `SIGIT_CLOUD_URL` | Inference endpoint. Defaults to `https://sigit.si/api/v1`. |
| `SIGIT_CONFIG_DIR` | Where the session is stored. |
| `SIGIT_MCP_OFFICIAL` | `off` makes `official_mcp_servers()` return nothing. |
| `SIGIT_MCP_URL` | Overrides the official MCP server URL. |

## License

Apache-2.0.
