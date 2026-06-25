# Changelog

## 1.2.1

Stabilizes the Zed/ACP integration and finishes the cloud-tier wiring on top of 1.2.0.

### What changed

- Fixed a Zed crash by keeping model-picker labels ASCII in the ACP model selector
- Wired ACP auth, cloud tiers, and slash commands into the Zed panel, including a `/reload` command to re-sync session state in place
- Fixed the TUI so the loaded-model checkmark appears once a download completes
- Synced bundled agent skills with the current code and added `CLAUDE.md`

## 1.2.0

Adds siGit Code Cloud — a hosted inference tier alongside on-device models.

### What changed

- Added siGit Code Cloud with cloud-tier routing, pointed at `sigit.si`
- Added account management slash commands and surfaced cloud tiers in `/models`
- Carried forward from 1.1.0: ACP SDK v0.13, refreshed dependencies and branding

## 1.1.0

Bumps the ACP SDK to v0.13 and pulls in updated dependencies.

### What changed

- Updated `agent-client-protocol` from v0.11 to v0.13
- Updated `onde` to 1.1.2
- Refreshed branding and skill metadata

## 1.0.4

This release tightens up the terminal experience and finishes a few release-facing cleanup items.

### What changed

- Added bold rich-text rendering in the TUI for assistant replies, so `**text**` now displays with terminal styling instead of raw markdown markers
- Refreshed the bundled skill metadata to follow the current Agent Skills `SKILL.md` format
- Synced the crate release metadata for the `1.0.4` cut

## 1.0.3

This is the cleanup release for the editor-side startup problems.

### What changed

- Fixed ACP sessions failing on the first real prompt because the server claimed the model was ready before anything had actually been loaded
- Changed ACP startup so the default model loads lazily on the first non-slash prompt instead of pretending it is already in memory
- Kept `initialize` and `session/new` lightweight while still sending proper progress updates once model loading begins
- Updated the Onde integration to `1.0.0`
- Removed a few dependencies we were no longer using

## 1.0.2

This release was supposed to fix the ACP auth breakage. It did fix the stdout pollution problem, but it turned out not to be the whole story.

### What changed

- Delayed model loading in ACP mode so startup diagnostics would not leak into protocol stdout during the auth handshake
- Tightened up the ACP startup path for editor integrations
- Refreshed some README wording while cutting `1.0.2`

## 1.0.1

The first patch after `1.0.0` was mostly about making model loading and model switching feel less opaque.

### What changed

- Added ToolCall-based progress UI for startup model loading and downloading
- Improved model-switch progress reporting in ACP clients
- Fixed model-load error handling so failed switches did not leave the UI in a weird state
- Cleaned up a few status messages and docs while the release was going out

## 1.0.0 (2026)

siGit Code has been living in real smbCloud repos for a while now. At some point it stopped feeling like an experiment, so we called it 1.0.

### What this release is

siGit Code is a local coding agent. It runs a quantized model on your machine, talks to editors over ACP, and can read files, run commands, fetch web pages, and write code without sending your project to a hosted API.

You can install it with Cargo, pip, npm, or Homebrew and use it like any other tool on your machine.

### What shipped in 1.0

#### Editor integration

This is the core of the project.

Zed and VS Code can talk to siGit Code over ACP. Multi-turn sessions work. Tool calling works. Session forking works. Working-directory context works. That was the original goal, and it feels solid now.

#### Terminal UI

The terminal UI started as a side quest and turned out to be useful. You get a full-screen ratatui chat, streaming tokens, a spinner while the model is busy, and a model picker you can open in the middle of a session.

It runs on macOS and Linux. Windows gets ACP and editor mode for now. The Windows terminal UI is still unfinished.

#### Tool calling

This is the part that makes siGit Code feel like an agent instead of a chat box. The loop can run up to 10 rounds per message.

Available tools:

- `read_file` / `write_file` / `delete_file`
- `list_directory` / `search_files`
- `run_command`, with an optional working directory
- `read_website`, which fetches a URL and strips it down to readable text

The model can call a tool, inspect the result, and keep going until it has a real answer.

#### Model support

The model list ended up wider than we expected for 1.0:

- Qwen 3 1.7B, 4B, 8B, and 14B
- Qwen 2.5 1.5B and 3B
- Qwen 2.5 Coder 1.5B, 3B, and 7B
- DeepSeek Coder 6.7B

They are all GGUF models and they all come from Hugging Face on first run.

Qwen 3 is the interesting one. It uses extended thinking mode. The model reasons inside `<think>...</think>` blocks before answering. The TUI strips those blocks out and renders them dimmed above the reply, so you can see what happened without turning the whole conversation into noise.

The 8B model is the desktop default. Mobile stays on 1.7B because iOS gives apps roughly 2 to 3 GB of memory, and we learned the hard way that 3B can blow up on an iPhone 16e.

#### Model picker

The model picker shows:

- what is already cached locally
- what can be downloaded
- which models support tool calling
- whether the local cache looks healthy

You can open it with `/models` in the TUI or through the editor config option. Switching models happens in the background and the UI stays alive while the download or load is in progress.

#### smbCloud context

siGit Code knows smbCloud repos better than a generic coding assistant does. It understands the difference between platform-user flows and tenant-app auth flows, how `Project`, `FrontendApp`, `AuthApp`, and GresIQ fit together, and why Next.js SSR deploys are not the same thing as the generic git-push path.

Outside smbCloud, it backs off and behaves like a normal coding agent.

#### Distribution

Distribution took an unreasonable amount of time, honestly.

There are prebuilt binaries for macOS, Linux, and Windows on both arm64 and x64 where relevant, plus install paths through Cargo, PyPI, npm, and Homebrew:

- `cargo install sigit`
- `pip install sigit-code`
- `npm install -g @smbcloud/sigit`
- `brew install sigit`

Getting that whole pipeline to behave across CI, crates.io, PyPI, npm, and Homebrew was basically its own project.

### What still does not work

The Windows terminal UI is still missing.

ACP and editor mode work on Windows. The part that is still missing is the interactive full-screen terminal UI. The blocker is Unix-specific terminal handling that has not been abstracted cleanly yet.

### Changes since 0.1.2

- Added Qwen 3 14B support
- Added Qwen 3 `<think>` block parsing and separate rendering in the TUI
- Moved all TUI code into `#[cfg(unix)]`, which fixed a pile of dead-code errors on Windows CI
- Added live download progress during model switches, including cancellation with Ctrl+C
- Added model download and loading progress in the Zed agent config panel
- Added an animated spinner during model switching
- Added Qwen 2.5 Coder 7B
- Added downloadable models to the picker, not just locally cached ones
- Made model selection persist across restarts
- Added session working-directory support
- Moved model picker logic into a platform-independent module so Windows can compile without the TUI
- Added the `/models N` shortcut for picking a model by number
- Added the `read_website` tool
- Improved `read_file` handling and empty-reply detection
- Added async tool execution
- Fixed CI cross-compilation for macOS, iOS, Linux, and Windows
- Added npm, PyPI, and Homebrew distribution

---

*© 2026 [smbCloud](https://smbcloud.xyz/) (Splitfire AB).*
