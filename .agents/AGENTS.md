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
never `main`. `main` is release-only; `development` is where day-to-day work integrates, and the
release merge (see the `sigit-code-release` skill) is the one thing that ever puts commits on
`main`.

This rule needs help to hold, because the repository's default branch is `main`: both
`gh pr create` and the GitHub web form pre-select it, so a PR lands on the wrong base unless the
base is passed explicitly. Pass it every time, and check that it took:

```sh
gh pr create --base development --head <branch>
gh pr view <number> --json baseRefName
```

Retargeting a PR that was opened against `main` costs nothing before it merges:
`gh pr edit <number> --base development`. After it merges it costs real work — the change sits on
`main` while `development`, which the next release is cut from, does not have it, and someone has
to carry it back by hand. PR #65 went in that way.

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
  new/load/fork/prompt/cancel, config options, slash-command advertisement), and the `SYSTEM_PROMPT`.
  ACP session state owns its roots, conversation, and selected model even though the process has
  one working directory and one live backend slot; activating a thread parks and restores all
  three. A thread on an HTTP backend keeps a backend of its own (`InferenceBackend::fresh`,
  `SessionState::remote`), so its turn can wait on the endpoint while another thread is
  installed. Unknown session ids are rejected instead of silently borrowing the active thread's
  cwd. Prompt cancellation takes no lock, which lets a client cancel a turn whatever it holds.
  `session/load` and `session/resume` share `restore_session`; the only difference is that
  resume must not replay the history as `session/update`. `session/close` is the one place a
  `SessionState` is dropped. It runs in two halves: `begin_close` signals the session's turn
  from the dispatch loop (the turn holds the session's lock, so the signal cannot wait for it) and
  marks the id in `closing_sessions`, which makes `handle_prompt` cancel a prompt that was
  still queued; `handle_close_session` then runs under the session and workspace locks and
  removes the state, the permission grants and the background commands
  (`tools::kill_session_tasks`). It leaves `session_store` alone, so a closed thread still lists
  and reopens. `session/delete` is the one that removes it from disk: it runs the same two halves
  (`begin_close`, then `handle_close_session` from `handle_delete_session`) and then
  `session_store::delete`, and succeeds for an id that is already gone. The
  `SYSTEM_PROMPT` bakes in smbCloud-specific context the agent should use when the repo is clearly
  smbCloud, and stay general otherwise.
- **`src/backend.rs`** — the `InferenceBackend` trait and neutral types (`ToolSpec`, `ToolCall`,
  `ToolResult`, `TurnResult`). Two impls: `LocalBackend` (on-device via `onde::ChatEngine`) and
  `OpenAiBackend` (any OpenAI-compatible HTTP endpoint). A `BackendError` is user-facing: the
  ACP prompt handler passes it to the client verbatim and the editor puts it in an error banner,
  so `describe_api_error` unwraps the OpenAI `{"error":{"message":…}}` envelope and shows that
  message on its own, falling back to the status only when there is nothing to unwrap. An
  endpoint can also fail *after* the response is open, reporting it as a `data:` frame holding
  the same envelope; that frame has no `choices`, so `consume_stream` has to check for it
  explicitly or it parses as an empty chunk and the turn ends looking like an empty answer.
  `TurnResult::finish` carries the endpoint's `finish_reason` up to the loop, and
  `stop_reason_for` in `main.rs` turns it into the ACP stop reason: the tool-round cap is
  `max_turn_requests`, `length` is `max_tokens`, `content_filter` is `refusal`. ACP defines a
  refusal as a turn the next prompt will not include, so `handle_prompt` restores the history
  it snapshotted before the turn instead of only relabelling the response.
  The round that reaches the cap (24, or `SIGIT_MAX_TOOL_ROUNDS`) is followed by one forced
  text reply with no tools. The model cannot see that the tools are gone, so
  `round_cap_note` is appended to that round's last tool result, and the turn always closes
  with `round_cap_stop_message`. Without both, the thread stops on the model announcing a
  step that never runs (issue #120).
  Some models write tool calls into content as text; `src/inline_tool_calls.rs` recovers the
  well-formed ones. A block that doesn't parse (or never closes) is dropped from both the reply
  and history, and `OpenAiBackend::complete` retries once with a note telling the model the call
  didn't run. Leaving the raw block in history makes the model invent `<function_results>` later.
  What that scanner lets through then goes through `src/harness_markup.rs`, which removes
  markup the model makes up in the harness's shape (issue #122): a `<system_warning>`-style
  block (any `system_*`/`system-*` tag) is dropped whole, from the reply and the history, and
  other underscore-named tags like `<Option_Picker>` lose the tags but keep their text. Code is
  never touched, and an opening tag only counts at the start of a line, since prose writes
  placeholders like `<repo_url>` the same way. It has to run after the inline-call scanner,
  because `<tool_call>` matches its underscore rule.
  `src/repetition.rs` guards the same stream against a reply that degenerates into a loop over a
  tiny vocabulary (issue #123): a window of 200 words with at most 40 distinct ones, fenced code
  excepted, makes `consume_stream` stop reading, truncate the text to where the loop began (so
  history never holds the junk), tell the user, and finish with `FinishReason::Repetition`
  (`EndTurn` in ACP). `complete` then retries once with `REPETITION_RETRY`, like the malformed
  tool call retry. Only the streaming path is guarded; `consume_json` and `LocalBackend` are not.
  Image attachments: ACP fixes `promptCapabilities.image` for the whole connection, while the
  model can change on any turn, so the capability is always advertised and the decision is made
  per prompt. `InferenceBackend::accepts_images` answers for the active model (on-device: no;
  remote: `provider::model_accepts_images`, which reads the hard-coded `IMAGE_TIERS` for
  `onde-*` ids and says yes for a user's own endpoint). `images_for_turn` in `main.rs` either
  passes the images to `send_message_with_images` or drops them with a note to the user and a
  bracketed note to the model. A message with images is stored in history as OpenAI content
  parts and stays that way; `OpenAiBackend` strips the images per request (`without_images`)
  when its model cannot read them, which is what makes a mid-thread switch to a text-only tier
  safe. Read history text through `backend::message_text`, never `content.as_str()`.
  `IMAGE_TIERS` mirrors onde-cloud's `IMAGE_CANDIDATES`; update both together.
  Audio attachments take the same path (`promptCapabilities.audio`, `accepts_audio`,
  `audio_for_turn`, `without_audio`), sent as OpenAI `input_audio` parts. No cloud tier
  takes audio, because onde-cloud drops `input_audio` parts when it parses a request, so
  `provider::model_accepts_audio` says no for every `onde-*` id and yes for a user's own
  endpoint. If onde-cloud ever routes audio, give it a tier table like `IMAGE_TIERS`.
- **`src/provider.rs`** — decides *which* backend serves inference. Resolution order, first match
  wins: (1) override via `OPENAI_BASE_URL`+`OPENAI_API_KEY` or active profile in
  `~/.config/sigit/providers.toml`; (2) siGit Code Cloud when logged in; (3) on-device.
- **`src/tools.rs`** — agent tool schemas + execution: `read_file`, `create_directory`,
  `list_directory`, `search_files`, `glob`, `read_website`, `create_file`, `edit_file`,
  `multi_edit`, `delete_file`, `run_command`, `write_todos`, `remember`. Add a tool in both the
  spec list (`all_tools`) and the execute `match` (`execute_tool`). `run_command` also enforces
  commit attribution: when a command creates a new commit that lacks the
  `Co-Authored-By: siGit Code` trailer (`commit_co_author_trailer()`, which reads
  `siGit Code v<version>-<acp|tui|headless>` from the surface `main` sets via `set_surface`), it amends the trailer in —
  unless the commit already exists on a remote, which is never rewritten. Every child process it
  spawns (`spawn_shell`, the `git` helpers, and `hooks.rs`) sets `stdin` to null and never
  inherits it: in ACP mode sigit's stdin is the JSON-RPC pipe from the editor, so a command that
  reads stdin both blocks forever and eats the client's next request, wedging the session past
  any timeout. On Unix the shell also leads its own process group so the timeout kills the whole
  tree, and stdout/stderr are drained on threads while the command runs — waiting first and
  reading after deadlocks as soon as the output outgrows the pipe buffer. Also owns the `task`
  tool: a nested agent loop in a fresh conversation, offered only when `subagent_available()`
  (a subagent factory is registered — see `register_subagent_factory_for` in `main.rs`; on-device
  registers a `None`-returning factory since onde has a single shared history). A subagent's
  toolset is a hard-gated read-only allow-list (`SUBAGENT_TOOL_NAMES`) that is never expanded by a
  configurable subagent type (see `src/subagents.rs`) — only ever narrowed — so a `.sigit/agents/*.md`
  file can't grant itself `edit_file`/`run_command` and bypass the permission system. Also owns
  `web_search`: a thin native wrapper around the official MCP server's Brave-Search-backed
  `web_search` tool (`mcp__sigit__web_search`, implemented server-side in `sigit-si`'s
  `WebSearchService` + `Mcp::Tools::WebSearch`), offered only when that MCP tool was actually
  discovered (i.e. the user is signed in to siGit Code Cloud — see `web_search_available`).
  Wrapping it natively rather than leaving it as a raw `mcp__*` tool matters for two reasons:
  `permissions::classify` treats every `mcp__*` tool as mutating (an "ask" prompt on every call),
  while `web_search` is read-only like `read_website`; and the raw delegate name is filtered out
  of the assembled tool list (`is_web_search_delegate`) so the model sees one clean option, not
  two names for the same tool. Execution still forwards verbatim to `mcp::call_tool` — no second
  HTTP/JSON-RPC implementation.
- **`src/skills.rs`** — [Agent Skills](https://agentskills.io) support. Discovers skill
  folders (each with a `SKILL.md`: YAML frontmatter `name` + `description`, then Markdown
  instructions) from `.sigit/skills/` and `.claude/skills/` in every project root (see `src/workspace.rs`),
  `$SIGIT_CONFIG_DIR/skills/`,
  and `~/.claude/skills/`. Progressive disclosure: the discovery list (name + description) is
  baked into the dynamically-built `skill` tool's description, and activating a skill (the model
  calls `skill` with a name) loads the full `SKILL.md` body. The `skill` tool is appended in the
  `*_as_specs`/`build_tool_specs` layer (not in `all_tools()`) so its description can be dynamic,
  and only when at least one skill exists.
- **`src/commands.rs`** — user-defined slash commands. Discovers Markdown files (each an
  optional YAML frontmatter block — `description`, `argument-hint` — followed by a prompt-template
  body) from `.sigit/commands/` and `.claude/commands/` in every project root, `$SIGIT_CONFIG_DIR/commands/`,
  and `~/.claude/commands/`. A subdirectory namespaces the command with `:`
  (`.sigit/commands/git/commit.md` → `/git:commit`). Unlike skills there's no tool-call
  indirection: invoking one works exactly like the built-in `/init` — `commands::render`
  substitutes `$ARGUMENTS`/`$1..$9` in the body against whatever followed the command on the
  input line, and the result is fed to the model as a normal turn through the ordinary tools and
  permission checks. Resolution happens where each surface (`main.rs`/`chat.rs`) already
  special-cases `/init`: an unrecognized slash command is tried against `commands::resolve_command`
  before falling back to "unknown command". A custom command sharing a name with a built-in is
  unreachable (built-ins match first in `parse_slash`) and is skipped when advertised to ACP
  clients, with a warning logged.
- **`src/subagents.rs`** — configurable subagent types for the `task` tool. Discovers Markdown
  files (YAML frontmatter `name` + `description`, optional comma-separated `tools:` allow-list,
  then a Markdown body that becomes the subagent's system prompt) from `.sigit/agents/` and
  `.claude/agents/` in every project root, `$SIGIT_CONFIG_DIR/agents/`, and `~/.claude/agents/`. Passing a
  type's `name` as `task`'s `subagent_type` argument swaps in that system prompt and, if `tools:`
  is set, narrows the offered toolset to its *intersection* with `SUBAGENT_TOOL_NAMES` — the
  security-relevant narrowing logic lives in `tools.rs` next to that constant, not here; this
  module only discovers and parses files. `SubagentFactory` (in `tools.rs`) takes the resolved
  system prompt per call rather than baking one in at registration, so a single registered
  factory serves both the default research subagent and every configured type.
- **`src/frontmatter.rs`** — shared "YAML frontmatter + Markdown body" parsing used by both
  `src/skills.rs` (`SKILL.md`) and `src/commands.rs` (`.sigit/commands/*.md`).
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
  layer and routed in `tools::execute_tool` via `mcp::is_mcp_tool`. Two servers are baked in:
  the official server (`<cloud>/mcp`, default `https://sigit.si/api/v1/mcp`, always HTTP, authed
  with the cloud session token) and the smbCloud CLI server (`smb --mcp`, stdio, added only when
  the `smb` binary is on `PATH`; opt out with `smbcloud = false` in `mcp.toml` or
  `SIGIT_MCP_SMBCLOUD=off`). A user-defined entry named `sigit` or `smbcloud` overrides the
  corresponding baked-in one. Extra servers live in `mcp.toml` (global
  `$SIGIT_CONFIG_DIR/mcp.toml` and project-local `.sigit/mcp.toml`). The stdio path is covered by
  `tests/mcp_stdio.rs`, driven by the test-only `src/bin/mcp_stdio_stub.rs` helper binary
  (excluded from the published crate via `exclude` in `Cargo.toml`). The baked-in official
  server is *also* listed in the public MCP Registry as `si.sigit/sigit` — a **remote**
  Streamable-HTTP listing owned by the [`getsigit/si`](https://github.com/getsigit/si) repo
  (`server.json` at its root, published by its `release-mcp-registry.yml`), not this one. Because
  it's a remote server, the registry's URL-match rule forces a domain namespace (`si.sigit` ↔
  `sigit.si`) verified by a DNS TXT record, not the GitHub-OIDC scheme `smbcloud-cli` uses for its
  package listing.
  Separate from all of the above are the servers an ACP client passes in `mcpServers` on
  `session/new`, `session/load` and `session/fork`. ACP requires agents to connect to the stdio
  ones, and they belong to the session that named them, so they cannot live in the startup
  global: `connect_session_servers` returns a `SessionServers` that `main.rs` keeps on
  `SessionState` and installs with `set_session_servers` whenever a session becomes live, the
  same way it swaps the roots. `tool_specs`, `call_tool` and `/mcp` read the startup servers and
  then the live session's. A client-supplied server whose name is already taken by a startup
  server is not connected, since both would claim the same `mcp__<server>__` prefix. Streamable
  HTTP entries are connected too, with the URL and headers the client sent, because
  `handle_initialize` advertises `mcpCapabilities.http`. SSE is not advertised (the MCP spec
  deprecated it), so a conforming client never sends one and a stray entry is skipped.
- **`src/permissions.rs`** — tool permission policy. Every tool call passes through
  `decision_for` before executing: read-only tools always run; mutating tools (and all
  `mcp__*`/unknown tools) are governed by, in order: per-session plan mode (`/plan` — deny all
  mutating tools with a present-a-plan message), session "always deny" and "always allow"
  choices (deny checked first, both scoped by `session_grant_rule` and dropped with the session),
  per-tool overrides and the default mode from `[permissions]` in `settings.toml`
  (`allow`/`ask`/`deny`, default `ask`; `SIGIT_PERMISSIONS` env overrides the default). On `ask`,
  the ACP path sends `session/request_permission` (allow once / allow for session / deny / deny
  for session) and the TUI pauses the inference task on a y/a/n/d prompt. On the ACP path the decision is taken *before* the call is
  announced, because it sets the announced status: a call that will ask starts `pending`, the
  permission request carries that call's own id, and an `in_progress` update follows approval.
  That update also puts the card's title back, since the permission request overwrites it
  with the full arguments. The Manual/Auto/Plan choice reaches an ACP client twice: as the
  `sigit-permission-mode` config option, and as session `modes` with `session/set_mode`, which
  the spec is retiring but some clients still render instead. Both are built from
  `PERMISSION_MODES` in `main.rs` and read the same state, so whatever changes the mode has to
  refresh both (`ConfigOptionUpdate` and `send_current_mode`). Note: ACP turn-affecting handlers run in `cx.spawn`ed tasks
  so the dispatch loop can route the client's permission answer mid-turn — don't move them back
  inline, and don't await client requests from inline handlers (deadlock). Two locks order them.
  A per-session lock (`SiGitAgent::session_lock`) is held for a whole request, so requests on one
  thread keep their order. `SiGitAgent::workspace_lock` guards what a session installs
  process-wide (cwd, workspace roots, session MCP servers, the live backend slot, the engine's
  conversation); lifecycle and config handlers hold it throughout, and `handle_prompt` holds it
  through a `WorkspaceHold` that it releases while waiting on an HTTP endpoint or a permission
  answer and retakes (reinstalling its session via `resume_workspace`) before any tool runs. An
  on-device turn never releases it. Take the session lock first, never the other way round. Tool
  execution, subagents included, still runs one session at a time.
- **`src/instructions.rs`** — project instruction files, the always-on counterpart to skills.
  Reads `AGENTS.md` (the cross-tool [agents.md](https://agents.md) standard) and `CLAUDE.md`,
  walking from the session cwd up to the repo root (nearest ancestor with `.git`, never above it),
  plus a global file under `$SIGIT_CONFIG_DIR`. Files are ordered outermost-first so the deepest
  (most specific) wins. In a multi-root project that walk is repeated for every root
  (`load_workspace_instructions`). The combined block is injected via `session_context_message`
  in `main.rs` — pushed as a system message at every ACP session entry point (new/load/fork +
  model switch) and appended to the system prompt on the cloud and TUI-startup paths.
- **`src/workspace.rs`** — the directories the session treats as project roots. An editor can
  open several at once (Zed calls it a multi-root project) and ACP carries the extras as
  `additional_directories` on every session request; the headless CLI takes them as repeatable
  `--add-dir` flags. A client only sends those extras to an agent that advertises
  `sessionCapabilities.additionalDirectories` in its `initialize` reply, so that capability in
  `handle_initialize` is what makes the rest of this reachable — without it Zed keeps the first
  root, drops the others, and shows "This agent doesn't currently support multi-root workspaces".
  The process still has one working directory, so the extra roots live in a
  process-global here and `project_dirs()` returns cwd-first, extras after.
  One process also serves every thread the editor has open, so that global
  (with the cwd, the backend conversation, and the background-task owner in
  `tools.rs`) always belongs to the *live* session: `main.rs` keeps a
  `SessionState` per session id and `activate_session` parks the live one and
  installs the requested one before any prompt or config change runs. Don't
  read another session's roots from the global. Project-local
  discovery reads it: skills, slash commands, subagent types, and instruction files all scan
  every root. MCP is deliberately not on that list — `mcp::init` runs once at startup, before
  any session exists, so a second root's `.sigit/mcp.toml` has nobody to tell. (The servers a
  client names in `mcpServers` are a different thing and are per session; see `src/mcp.rs`.)
- **`src/client_fs.rs`** — file reads and writes through the ACP client. A client that
  advertises `fs.readTextFile` / `fs.writeTextFile` in `initialize` serves `fs/read_text_file`
  and `fs/write_text_file` from its buffers, so `read_file` sees unsaved changes and
  `edit_file` lands in the open buffer instead of underneath it. `tools.rs` has no access to the
  connection, so this is a seam like the one `mcp::call_tool` gives MCP: `handle_initialize`
  registers a `ClientFileSystem` (`AcpClientFs` in `main.rs`, holding the connection), and
  `execute_tool_impl` asks `route_for` before it dispatches `read_file`, `create_file`,
  `edit_file` or `multi_edit`. Each of those tools is split into a parse step and a pure
  render/apply step (`ReadFileCall`, `CreateFileCall`, `EditCall`) that the disk path and the
  client path share, so the model gets the same result text either way. The writing tools also
  hand back the file before and after (`tools::FileChange`, via `execute_tool_with_change`), which
  `handle_prompt` sends as ACP `diff` content ahead of the result text. A permission request for
  one of them carries the same diff, worked out by `preview_file_change` without writing anything
  and before the workspace is released, since a relative path resolves against the session's cwd. The two capabilities are
  independent: a client that only reads still gets its edits written to disk. The disk is the
  fallback throughout: nothing is registered in the TUI or headless modes, a path outside the
  session's roots is not routed, and a request the client fails or leaves unanswered for 30
  seconds is retried on disk with a warning in the log. The client is only ever asked from
  inside a spawned prompt turn, never from `handle_initialize` itself (deadlock, same as
  permission requests). Existence checks and `create_file`'s parent directories still use the
  disk. `SIGIT_CLIENT_FS=off` turns the routing off. Covered by `tests/acp_client_fs.rs`.
- **`src/client_terminal.rs`** — `run_command` in the ACP client's terminal. A client that
  advertises `terminal` in `initialize` runs a foreground command itself (`terminal/create`, with
  the platform shell as `command` and the whole command line as one argument), and
  `exec_run_command_via_client` embeds the terminal in the tool call as `terminal` content, so the
  user watches the output live and can stop the command from the editor. The seam is
  `client_fs`'s: `handle_initialize` registers `AcpClientTerminal`, and `execute_tool_impl` asks
  `client_terminal_route` before the blocking path. Embedding needs the tool call's id, which is
  why `execute_tool_with_change` takes one. What `run_command` promises stays put: the 120 s
  timeout ends in `terminal/kill`, the output is capped, and `CommitWatch` still amends the
  co-author trailer, checking HEAD on disk before and after. Background commands stay local
  (`command_output` and `kill_command` read their pipes), and so does a command whose directory
  is outside the session's roots, since nothing says the editor's machine can see it. A
  `terminal/create` the client fails means nothing ran, so the command runs locally; a failure
  after that is reported as the tool's error instead, because running it again could repeat its
  effects. The finished card keeps the terminal and drops the result text, which still goes out
  as `raw_output`. `SIGIT_CLIENT_TERMINAL=off` turns the routing off. Covered by
  `tests/acp_client_terminal.rs`.
- **`src/chat.rs`** — the Unix-only ratatui TUI. Loading-spinner phase then chat; uses
  `tokio::select!` to multiplex terminal events with streaming tokens.
- **`src/headless.rs`** — non-interactive `sigit run` execution for scripts, CI, and Factory
  clients. The legacy `-p` form reaches the same parser. Every new run gets a UUID session id;
  `--resume <id>` restores that session through `session_store`, and `--output jsonl` emits a
  structured session/delta/tool/result stream while logs stay on stderr. Headless permission
  prompts collapse to denial unless the tool was pre-approved with `--allow-tool`.
- **`src/session_store.rs`** — durable conversations: one JSON-lines history file per session at
  `$SIGIT_CONFIG_DIR/sessions/<id>.jsonl`, written atomically, restorable into either backend.
  Each save also writes a `<id>.meta.json` sidecar naming the session's `cwd` and the extra roots
  of a multi-root project. That sidecar is what makes a thread *listable*: ACP's `session/list`
  (advertised as `sessionCapabilities.list`, handled by `handle_list_sessions` in `main.rs` — the
  editor's "Import Threads" picker) must report an absolute `cwd` per session and may filter on
  it, so a session without one is skipped there while still reopening by id through
  `session/load`. Sidecars are written at save time, not at session start, so a thread nobody
  spoke in leaves nothing behind. Each ACP save (`persist_session` in `main.rs`) is also sent to
  the client as a `session_info_update` carrying a fresh `updatedAt`, plus the title when it
  differs from the last one sent (`announced_titles`), so a new thread gets its title in the
  sidebar without waiting for the next `session/list`.
- **`src/setup.rs`** — model cache location, local model discovery, selected-model persistence.
  Must run (`setup_shared_model_cache`) *before* anything touches `ChatEngine`/`hf-hub`, since
  those read env vars once at init.
- **`src/account.rs`** — siGit Code Cloud auth (`/login`, `/logout`, `/whoami`); authenticates
  against the account API and stores a session token. Performs no console I/O.
  ACP `logout` (the editor's sign out button, shown only because `handle_initialize` advertises
  `agentCapabilities.auth.logout`) and `/logout` both go through `SiGitAgent::sign_out` in
  `main.rs`. The request names no session, so `leave_cloud` moves the parked threads off their
  cloud tier along with the live one and each of them gets a fresh model picker.
- **`src/browser_auth.rs`** — browser sign-in, the path the editor's "Sign in to siGit Code"
  button takes. sigit is a public OAuth client (client id `sigit-code-cli`, no secret, PKCE
  S256) against the authorization server at `$SIGIT_API_URL/oauth`. Two ways the code gets
  back: a loopback listener on `127.0.0.1:0` (the registration lists `http://127.0.0.1/callback`
  with no port, and RFC 8252 §7.3 matching ignores the port, so one registration covers every
  run), or the out-of-band URN, where the server renders the code and the user pastes it. The
  resulting token goes into `credentials` like any other, and is served by `/api/v1/user` rather
  than the deprecated `/api/v1/me` — OAuth tokens are rejected there. The scopes requested are
  `user:read code:agent`; `code:agent` is what reaches `chat/completions` and the official MCP
  server. Surfaces: ACP `authenticate` (loopback only — nowhere to show a code), a bare `/login`
  in either chat surface, and `sigit login` (which adds the paste fallback, plus `--paste` to
  force it and `--password` for the old email/password prompt).
- **`src/credentials.rs`** — local session-token store (TOML, `0600` on Unix).
- **`src/models.rs`** — model-picker types shared across platforms.

Slash commands (`/help`, `/models`, `/skills`, `/agents`, `/commands`, `/mcp`, `/login`, `/logout`,
`/whoami`, `/reload`, `/plan`, `/permissions`, `/init`, `/clear`, `/status`) are advertised via
`advertise_commands` in `main.rs` and handled in both the TUI and ACP sessions. `/init` is
special: instead of replying directly it substitutes `instructions::INIT_PROMPT` for the user
text and runs a normal agent turn that explores the repo and writes (or improves) `AGENTS.md`
through the ordinary tools and permission checks. User-defined commands (see `src/commands.rs`)
get the same treatment via the `SlashCommand::Unknown` fallback path, so anyone can add their own
`/name` commands without touching the built-in command list.

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
`SIGIT_MODEL`, `SIGIT_MAX_TOOL_ROUNDS` (1 to 500, default 24; the tool-round cap of a
headless run or an ACP prompt turn), `SIGIT_MCP` (`off` disables MCP), `SIGIT_CLIENT_FS` (`off` keeps the file tools on disk even when the
ACP client offers `fs/read_text_file` / `fs/write_text_file`), `SIGIT_CLIENT_TERMINAL` (`off` runs
`run_command` locally even when the ACP client offers a terminal), `SIGIT_MCP_SMBCLOUD` (`off` drops the baked-in
smbCloud CLI server), `SIGIT_MCP_OFFICIAL` (`off` drops the baked-in
server), `SIGIT_PERMISSIONS` (`allow`/`ask`/`deny` — overrides the default permission mode for
mutating tools; the escape hatch for clients without permission-request support),
`HF_HOME` / `HF_HUB_CACHE`, `RUST_LOG`.

## Releasing

Version lives in `Cargo.toml`. Update `CHANGELOG.md` for releases.

Publishing splits into two groups. **Language registries** each get their own tag-triggered
workflow: `release-crates`, `release-npm`, `release-nuget`, `release-pypi`. The `npm/`, `nuget/`,
and `pypi/` dirs hold their wrapper-package templates.

npm and NuGet authenticate with OIDC (Trusted Publishing), so neither needs a stored token. On
npm that is configured per package, not per org: all seven `@getsigit/*` names carry a trusted
publisher pointing at `release-npm.yml`, and a package added later needs the same setup or its
publish step will fail. OIDC also requires npm >= 11.5.1 and Node >= 22.14.0, which is why the
workflow upgrades npm and why `.nvmrc` cannot drop below 22.

**OS package managers** all publish somewhere outside this repo and all need checksums from the
GitHub release, so `release-github` builds the binaries, attaches the assets, and then dispatches
`release-homebrew`, `release-scoop`, `release-winget`, and `release-aur`. A dispatch failure in one
is logged as a warning rather than failing the others, so an unconfigured channel does not block a
release. Their inputs live in `packaging/`.

The Windows binaries link the MSVC C runtime statically (`+crt-static` in `.cargo/config.toml`).
winget, Scoop, npm, PyPI and NuGet all ship the bare exe with no installer, so a dynamic build
that imports `VCRUNTIME140.dll` fails to load on a machine without the VC++ Redistributable
(`0xC0000135`, which is what failed winget's first validation).
`packaging/windows/check-static-crt.ps1` runs in CI and in `release-github` and fails the build if
a Windows exe imports the runtime again. Don't set a `RUSTFLAGS` env var in those workflows: it
replaces the config rustflags instead of adding to them.

Every release asset now carries a `.sha256` sidecar, not just the macOS Homebrew tarball. Scoop,
winget, and the AUR PKGBUILD each need one, and they consume the raw binaries rather than the
tarball. `release-github` also builds a `.deb` and `.rpm` per Linux target with nfpm
(`packaging/nfpm.yaml`), packaging the already-built binary rather than re-invoking cargo.

Re-pushing a tag fires every release workflow a second time, so each one is grouped by tag in a
`concurrency` block with `cancel-in-progress: false` — the duplicate queues behind the original
rather than interrupting a publish that is halfway through uploading. What keeps the queued run
from going red is the "is this version already published?" check each publishing workflow does
before it uploads. Those checks are load-bearing, not belt-and-braces: v1.5.6 published twice
because the crates.io one asked the API with curl's default User-Agent, which crates.io answers
with a 403, which read as "not published yet".

Three of these need credentials or a one-time manual step before they work:

- Scoop needs a `getsigit/scoop-bucket` repo and a `SCOOP_BUCKET_TOKEN` secret, mirroring the
  Homebrew tap setup.
- winget needs a `WINGET_TOKEN` (PAT with `public_repo` scope) so `wingetcreate` can fork
  `microsoft/winget-pkgs` and open the manifest PR. It is passed as
  `WINGET_CREATE_GITHUB_TOKEN` rather than `--token`, which wingetcreate warns can leak the
  token into logs. `wingetcreate update` only works on a package that already exists in
  `microsoft/winget-pkgs`, so the workflow checks the `manifests/g/getSigit/siGitCode` path
  first and falls back to rendering `packaging/winget/*.yaml.in` and running
  `wingetcreate submit` for a first submission. That fallback runs once, then every later
  release takes the `update` path. Two non-obvious things in the update path: the `--urls`
  arguments carry a trailing `|x64` / `|arm64` suffix that tells wingetcreate which installer
  entry each URL replaces — without it, wingetcreate guesses from the file name, and
  "sigit-win-amd64.exe" is not a spelling it recognises; and the first-submission render
  uppercases the SHA256 (`tr '[:lower:]' '[:upper:]'`) to match the community validation
  pipeline, which writes 64-hex checksums uppercase. The update path also passes
  `--release-date`, `--release-notes-url`, `--submit`, and `--no-open` explicitly.
- The AUR needs `AUR_USERNAME`, `AUR_EMAIL`, and `AUR_SSH_PRIVATE_KEY`. It publishes `sigit-bin`
  (a prebuilt binary) so Arch users are not compiling the on-device inference stack to install a
  CLI.

`[profile.release]` sets `strip = "symbols"` because binary size is a distribution constraint, not
just a nicety. See the NuGet note below.

The NuGet package (`SiGit.Code`, installed with `dotnet tool install --global SiGit.Code`) is the
odd one out among the language registries: npm and PyPI publish one artifact per platform, but a
.NET tool is a single package, so `nuget/sigit/` bundles all six binaries under
`native/<os>-<arch>/` and a small managed shim (`Program.cs`) execs the right one. That shim leaves
stdin/stdout/stderr unredirected on purpose, since siGit Code chooses TUI or ACP mode by testing
whether stdin is a TTY. It also sets `RollForward=Major`: it targets `net8.0`, and without that a
machine carrying only the .NET 10 runtime installs a tool that refuses to start.

Bundling every target means the package is large, so `release-nuget.yml` fails the pack job if the
`.nupkg` crosses nuget.org's 250 MB limit. At v1.5.1 it lands around 160 MB. If a future release
trips that check, the fix is not to drop targets but to split into RID-specific tool packages
(.NET 10's `DotnetToolRidPackage`), which ship one binary per platform the way npm already does.
That also needs a fallback package for pre-.NET-10 SDKs.
