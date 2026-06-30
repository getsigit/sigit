# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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

The interactive client, the `InferenceBackend` seam (`backend.rs`), and provider resolution
(`provider.rs`) are wired up **only** through `#[cfg(unix)]` code paths. On Windows the binary
runs ACP-only and drives `onde` directly, so much of `backend.rs` and `provider.rs` is
legitimately unused there and the dead-code lint is suppressed *on non-Unix targets only*.

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
  `list_directory`, `search_files`, `read_website`, `create_file`, `edit_file`, `delete_file`,
  `run_command`. Add a tool in both the spec list and the execute `match`.
- **`src/skills.rs`** — [Agent Skills](https://agentskills.io) support. Discovers skill
  folders (each with a `SKILL.md`: YAML frontmatter `name` + `description`, then Markdown
  instructions) from `.sigit/skills/` and `.claude/skills/` in the cwd, `$SIGIT_CONFIG_DIR/skills/`,
  and `~/.claude/skills/`. Progressive disclosure: the discovery list (name + description) is
  baked into the dynamically-built `skill` tool's description, and activating a skill (the model
  calls `skill` with a name) loads the full `SKILL.md` body. The `skill` tool is appended in the
  `*_as_specs`/`build_tool_specs` layer (not in `all_tools()`) so its description can be dynamic,
  and only when at least one skill exists.
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

Slash commands (`/help`, `/models`, `/skills`, `/login`, `/logout`, `/whoami`, `/reload`,
`/clear`, `/status`) are advertised via `advertise_commands` in `main.rs` and handled in both the
TUI and ACP sessions.

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
`SIGIT_MODEL`, `HF_HOME` / `HF_HUB_CACHE`, `RUST_LOG`.

## Releasing

Version lives in `Cargo.toml`. The binary is published to five registries via separate workflows
(`release-crates`, `release-github`, `release-homebrew`, `release-npm`, `release-pypi`); the
`npm/` and `pypi/` dirs hold the wrapper-package templates. Update `CHANGELOG.md` for releases.
