# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

## What this is

`sigit` ("siGit Code") is a single Rust binary: a local-first AI coding agent that runs LLM
inference on-device (via the `onde` crate / GGUF models) or against a hosted/OpenAI-compatible
endpoint. It exposes itself two ways from the *same* binary, chosen at startup by whether stdin
is a TTY:

- **ACP mode** (stdin not a TTY): speaks the Agent Client Protocol over stdio for editor
  integration (Zed, VS Code ACP Client). Cross-platform.
- **Interactive terminal mode** (stdin is a TTY): a full-screen ratatui chat UI. **Unix-only** —
  it relies on fd redirection to keep logs out of the TUI, so Windows gets ACP mode only.

Before the TTY/ACP split, `main` also dispatches the account subcommands `sigit login`,
`sigit logout`, `sigit whoami` (see `src/main.rs` `main()`).

## Working in this repo

**IMPORTANT — branch naming:** Prefix every working branch with `feature/` (new functionality)
or `fix/` (bug fixes) — never a tool- or agent-name prefix like `claude/`. Name the branch after
the *changes it contains*, as a short, descriptive, kebab-case slug that is self-explanatory
from the name alone (e.g. `feature/tool-permission-system`, `fix/glob-mtime-sort`), never after
a task, ticket, or session id (not `feature/task-q003hm`).

**IMPORTANT — pull request target:** Always open pull requests against the `development` branch,
never `main`. `main` is release-only; `development` is where day-to-day work integrates.

**IMPORTANT — branch off `development`:** Start every working branch from the latest
`origin/development`. Exception: when new work *depends on* a feature branch that has not merged
yet (e.g. it builds on tools or APIs that branch introduces), it may be stacked on top of that
branch instead. When stacking: merge the base PR into `development` first, then rebase the
stacked branch onto `development` before opening its pull request, so each PR shows only its own
commits.

**IMPORTANT — run CI before pushing:** Run the full CI gate locally and confirm it is green
*before* pushing a branch or opening a pull request — never push work that fails these:

```sh
cargo fmt -- --check                  # formatting
cargo clippy --tests -- -D warnings   # lint (warnings are errors)
cargo test --locked                   # tests
```

## Agent assets layout

`.agents/` is the canonical home for agent assets; every other agent path in
the repo is a symlink into it, never a copy:

- `.agents/AGENTS.md` — this file, the project instructions. The root
  `AGENTS.md` and `CLAUDE.md` are symlinks to it, so agents.md-standard tools,
  Claude Code, and sigit itself (via `src/instructions.rs`, which prefers
  `AGENTS.md` over `CLAUDE.md` in the same directory and therefore reads it
  once) all load the same content. Edit this file; never replace the root
  symlinks with real files.
- `.agents/skills/` — all project skills. `.claude/skills` is a symlink to it,
  which is how both Claude Code and sigit itself (via `src/skills.rs`) discover
  them. Add new skills under `.agents/skills/<name>/SKILL.md`, never as real
  files under `.claude/`.

## Build / test / lint

```sh
cargo build                 # debug build
cargo build --release       # release binary at target/release/sigit
cargo run                   # launches interactive TUI (stdin is a TTY)
cargo test                  # CI runs: cargo test --locked --target <target>
cargo clippy --tests -- -D warnings   # CI gate: clippy is -D warnings on all 4 targets
cargo fmt -- --check        # CI gate (edition 2024)
```

CI (`.github/workflows/ci.yml`) runs fmt + clippy + test across four targets:
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`,
`x86_64-pc-windows-msvc`. Clippy is `-D warnings`, so warnings fail the build.

Run a single test: `cargo test <test_name>`.

## Critical platform constraint: `#[cfg(unix)]` dead code

The interactive client is `#[cfg(unix)]`-only. The `InferenceBackend` seam (`backend.rs`) and
provider resolution (`provider.rs`) are consumed by both the interactive client and the ACP
server, but several of their items are reached only through the Unix-only interactive paths, so
the dead-code lint is suppressed *on non-Unix targets only*.

Consequence: code can pass clippy on macOS/Linux but fail on the Windows target (or vice versa).
When touching `backend.rs`, `provider.rs`, or the interactive path, keep the `cfg` gates intact —
don't "fix" an unused-warning by deleting code that's live on Unix.

## Architecture

The agent loop is backend-agnostic. The flow: a turn (messages + tool specs) goes to an
`InferenceBackend`, which returns assistant text and/or tool calls; the loop executes tools and
feeds results back. Neither the loop nor ACP/TUI surfaces depend on a concrete backend.

- **`src/main.rs`** — entry point, mode dispatch, the full ACP `Agent` impl (session lifecycle:
  new/load/fork/prompt/cancel, config options, slash-command advertisement), and the `SYSTEM_PROMPT`
  (note: it bakes in smbCloud-specific context the agent should use when the repo is clearly
  smbCloud, and stay general otherwise).
- **`src/backend.rs`** — the `InferenceBackend` trait and neutral types (`ToolSpec`, `ToolCall`,
  `ToolResult`, `TurnResult`). Two impls: `LocalBackend` (on-device via `onde::ChatEngine`) and
  `OpenAiBackend` (any OpenAI-compatible HTTP endpoint).
- **`src/provider.rs`** — decides *which* backend serves inference. Resolution order, first match
  wins: (1) override via `OPENAI_BASE_URL`+`OPENAI_API_KEY` or active profile in
  `~/.config/sigit/providers.toml`; (2) siGit Code Cloud when logged in; (3) on-device.
- **`src/tools.rs`** — agent tool schemas + execution: `read_file`, `create_directory`,
  `list_directory`, `search_files`, `glob`, `read_website`, `create_file`, `edit_file`,
  `multi_edit`, `delete_file`, `run_command`, `write_todos`, `remember`. Add a tool in both the
  spec list (`all_tools`) and the execute `match` (`execute_tool`). `run_command` also enforces
  commit attribution: when a command creates a new commit that lacks the
  `Co-Authored-By: siGit Code` trailer (`COMMIT_CO_AUTHOR_TRAILER`), it amends the trailer in —
  unless the commit already exists on a remote, which is never rewritten. Also owns the `task`
  tool: a nested agent loop in a fresh conversation, offered only when `subagent_available()`
  (a subagent factory is registered — see `register_subagent_factory_for` in `main.rs`; on-device
  registers a `None`-returning factory since onde has a single shared history). A subagent's
  toolset is a hard-gated read-only allow-list (`SUBAGENT_TOOL_NAMES`) that is never expanded by a
  configurable subagent type (see `src/subagents.rs`) — only ever narrowed — so a `.sigit/agents/*.md`
  file can't grant itself `edit_file`/`run_command` and bypass the permission system.
- **`src/skills.rs`** — [Agent Skills](https://agentskills.io) support. Discovers skill
  folders (each with a `SKILL.md`: YAML frontmatter `name` + `description`, then Markdown
  instructions) from `.sigit/skills/` and `.claude/skills/` in the cwd, `$SIGIT_CONFIG_DIR/skills/`,
  and `~/.claude/skills/`. Progressive disclosure: the discovery list (name + description) is
  baked into the dynamically-built `skill` tool's description, and activating a skill (the model
  calls `skill` with a name) loads the full `SKILL.md` body. The `skill` tool is appended in the
  `*_as_specs`/`build_tool_specs` layer (not in `all_tools()`) so its description can be dynamic,
  and only when at least one skill exists.
- **`src/subagents.rs`** — configurable subagent types for the `task` tool. Discovers Markdown
  files (YAML frontmatter `name` + `description`, optional comma-separated `tools:` allow-list,
  then a Markdown body that becomes the subagent's system prompt) from `.sigit/agents/` and
  `.claude/agents/` in the cwd, `$SIGIT_CONFIG_DIR/agents/`, and `~/.claude/agents/`. Passing a
  type's `name` as `task`'s `subagent_type` argument swaps in that system prompt and, if `tools:`
  is set, narrows the offered toolset to its *intersection* with `SUBAGENT_TOOL_NAMES` — the
  security-relevant narrowing logic lives in `tools.rs` next to that constant, not here; this
  module only discovers and parses files. `SubagentFactory` (in `tools.rs`) takes the resolved
  system prompt per call rather than baking one in at registration, so a single registered
  factory serves both the default research subagent and every configured type.
- **`src/mcp.rs`** — [Model Context Protocol](https://modelcontextprotocol.io) *client*. Two
  transports: **Streamable HTTP** (one JSON-RPC POST endpoint, `url` in `mcp.toml`; replies are
  `application/json` or SSE) and **stdio** (`command` + optional `args`/`[server.env]` in
  `mcp.toml`; sigit spawns the server and speaks newline-delimited JSON-RPC over its
  stdin/stdout, stderr inherited into sigit's log). `url` and `command` are mutually exclusive —
  both or neither is a config error, logged and skipped. Both transports run the same
  `initialize`/`tools/list` handshake and forward `tools/call`. Discovery is best-effort at
  startup (`mcp::init`, called from both branches of `main()`) and cached in a process-global so
  the synchronous spec builders (`mcp::tool_specs`) and the async dispatch (`mcp::call_tool`) can
  both read it; `/reload` does *not* re-run it, so config changes need a restart. stdio children
  live for the process; a dead child fails calls with an in-band error string (no auto-restart).
  Tools are namespaced `mcp__<server>__<tool>`, appended in the `*_as_specs`/`build_tool_specs`
  layer and routed in `tools::execute_tool` via `mcp::is_mcp_tool`. The official server
  (`<cloud>/mcp`, default `https://sigit.si/api/v1/mcp`) is baked in (always HTTP) and authed
  with the cloud session token; extra servers live in `mcp.toml` (global
  `$SIGIT_CONFIG_DIR/mcp.toml` and project-local `.sigit/mcp.toml`). The stdio path is covered by
  `tests/mcp_stdio.rs`, driven by the test-only `src/bin/mcp_stdio_stub.rs` helper binary
  (excluded from the published crate via `exclude` in `Cargo.toml`).
- **`src/permissions.rs`** — tool permission policy. Every tool call passes through
  `decision_for` before executing: read-only tools always run; mutating tools (and all
  `mcp__*`/unknown tools) are governed by, in order: per-session plan mode (`/plan` — deny all
  mutating tools with a present-a-plan message), session "always allow" grants, per-tool
  overrides and the default mode from `[permissions]` in `settings.toml` (`allow`/`ask`/`deny`,
  default `ask`; `SIGIT_PERMISSIONS` env overrides the default). On `ask`, the ACP path sends
  `session/request_permission` (allow once / allow for session / deny) and the TUI pauses the
  inference task on a y/a/n prompt. Note: ACP turn-affecting handlers run in `cx.spawn`ed tasks
  serialized by `SiGitAgent::turn_lock` so the dispatch loop can route the client's permission
  answer mid-turn — don't move them back inline, and don't await client requests from inline
  handlers (deadlock).
- **`src/instructions.rs`** — project instruction files, the always-on counterpart to skills.
  Reads `AGENTS.md` (the cross-tool [agents.md](https://agents.md) standard) and `CLAUDE.md`,
  walking from the session cwd up to the repo root (nearest ancestor with `.git`, never above it),
  plus a global file under `$SIGIT_CONFIG_DIR`. Files are ordered outermost-first so the deepest
  (most specific) wins. The combined block is injected via `session_context_message` in `main.rs`
  — pushed as a system message at every ACP session entry point (new/load/fork + model switch)
  and appended to the system prompt on the cloud and TUI-startup paths.
- **`src/chat.rs`** — the Unix-only ratatui TUI. Loading-spinner phase then chat; uses
  `tokio::select!` to multiplex terminal events with streaming tokens.
- **`src/setup.rs`** — model cache location, local model discovery, selected-model persistence.
  Must run (`setup_shared_model_cache`) *before* anything touches `ChatEngine`/`hf-hub`, since
  those read env vars once at init.
- **`src/account.rs`** — siGit Code Cloud auth (`/login`, `/logout`, `/whoami`); authenticates
  against the account API and stores a session token. Performs no console I/O.
- **`src/credentials.rs`** — local session-token store (TOML, `0600` on Unix).
- **`src/models.rs`** — model-picker types shared across platforms.

Slash commands (`/help`, `/models`, `/skills`, `/agents`, `/mcp`, `/login`, `/logout`, `/whoami`,
`/reload`, `/plan`, `/permissions`, `/init`, `/clear`, `/status`) are advertised via `advertise_commands` in
`main.rs` and handled in both the TUI and ACP sessions. `/init` is special: instead of replying
directly it substitutes `instructions::INIT_PROMPT` for the user text and runs a normal agent
turn that explores the repo and writes (or improves) `AGENTS.md` through the ordinary tools and
permission checks.

## Model cache (macOS)

On macOS the HF model cache lives in an App Group container shared with the siGit desktop app:
`~/Library/Group Containers/group.com.ondeinference.apps/models/`. Other platforms fall back to
`~/.cache/huggingface/`. The CLI reuses a model the desktop app already downloaded. First run
downloads a GGUF model (~1–2 GB) from Hugging Face.

## Logging

In TTY (interactive) mode, *all* output — `log`, `tracing`, stray `println!` — is redirected to
`$TMPDIR/sigit.log` so the ratatui surface stays clean; the TUI holds a separate fd to the real
terminal. In ACP mode, stdout is reserved for protocol JSON and logs go to stderr. Control
verbosity with `RUST_LOG`.

## Relevant env vars

`OPENAI_BASE_URL` / `OPENAI_API_KEY` (provider override), `SIGIT_API_URL` (account API base,
default `https://sigit.si`), `SIGIT_CLOUD_URL`, `SIGIT_CONFIG_DIR` (default `~/.config/sigit`),
`SIGIT_MODEL`, `SIGIT_MCP` (`off` disables MCP), `SIGIT_MCP_OFFICIAL` (`off` drops the baked-in
server), `SIGIT_PERMISSIONS` (`allow`/`ask`/`deny` — overrides the default permission mode for
mutating tools; the escape hatch for clients without permission-request support),
`HF_HOME` / `HF_HUB_CACHE`, `RUST_LOG`.

## Releasing

Version lives in `Cargo.toml`. The binary is published to five registries via separate workflows
(`release-crates`, `release-github`, `release-homebrew`, `release-npm`, `release-pypi`); the
`npm/` and `pypi/` dirs hold the wrapper-package templates. Update `CHANGELOG.md` for releases.
