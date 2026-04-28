# Changelog

## 1.0.0 — 2026

We've been running siGit in real smbCloud codebases for a while now. It holds up. Time to call it 1.0.

---

### The short version

siGit is a local coding agent. It runs a quantized LLM on your machine, talks to editors over ACP, and can read files, run commands, search the web, and write code, all without sending anything to a cloud API. You can install it with cargo, pip, npm, or Homebrew. Then you use it like any other tool on your machine.

---

### What's actually in here

**The editor integration** is the main thing. Zed and VSCode both pick it up as an ACP agent. Multi-turn sessions, tool calling, session forking, working directory context, all there. This was the original goal, and it feels solid now.

**The terminal UI** started as a bonus and ended up being genuinely useful. You get a full-screen ratatui chat, streaming tokens with a blinking cursor, a spinner while the model thinks, and a model picker you can open mid-session. It runs on macOS and Linux. Windows gets the editor integration for now. TUI support there is still on the list.

**Tool calling** is what makes it feel like an agent instead of a chat window. The loop runs up to 10 rounds per message:

- `read_file` / `write_file` / `delete_file`
- `list_directory` / `search_files`
- `run_command` — shell commands with optional working directory
- `read_website` — fetches a URL and strips it to readable text

The model decides which tools to call, sees the results, and keeps going until it has something useful to say.

**Model support** ended up broader than we expected for 1.0. Qwen 3 1.7B, 4B, 8B, and 14B. Qwen 2.5 1.5B and 3B. Qwen 2.5 Coder 1.5B, 3B, and 7B. DeepSeek Coder 6.7B. All GGUF, all pulled from HuggingFace on first run.

Qwen 3 is the interesting one. It uses extended thinking mode. The model reasons inside `<think>…</think>` blocks before answering. The TUI strips those out and renders them dimmed above the reply, so you can see what it was doing without cluttering the conversation. The 8B is the default on desktop. Mobile defaults to 1.7B because iOS gives apps about 2 to 3 GB, and we found out the hard way that 3B causes OOM on an iPhone 16e.

**The model picker** (`/models` in the TUI, or an agent config option in editors) shows what is cached locally, what is available to download, whether a model supports tool calling, and whether its local cache looks healthy. Switching models downloads and loads in the background. The UI stays alive the whole time, with a progress bar.

**smbCloud context** is baked into the system prompt. siGit knows the difference between platform user flows and tenant app auth flows, how `Project` / `FrontendApp` / `AuthApp` / GresIQ fit together, that Next.js SSR deploys are not the generic git-push path, and what workspace patterns smbCloud repos tend to follow. Outside smbCloud, it behaves like a normal coding agent and does not force platform-specific advice where it does not belong.

**Distribution** ended up being more work than the software itself, honestly. Pre-built binaries for macOS (arm64 + x64), Linux (arm64 + x64), and Windows (arm64 + x64). Four install methods: `cargo install sigit`, `pip install sigit-code`, `npm install -g @smbcloud/sigit`, `brew install sigit`. Getting all of that working across CI, crates.io, PyPI, npm, and Homebrew was basically its own project.

---

### What doesn't work yet

The Windows TUI. ACP and editor mode work fine on Windows. It is just the interactive terminal UI that is still missing. The underlying issue is Unix-specific terminal handling that we have not abstracted yet.

---

### Changes since 0.1.2

- Qwen 3 14B support
- Qwen 3 `<think>` block parsing, which strips and renders thinking content separately in the TUI
- Moved all TUI code into `#[cfg(unix)]`, which fixed a pile of dead-code errors on Windows CI that were blocking releases
- Live download progress bar during model switch, with cancellation support (Ctrl+C mid-download works)
- Model download and loading progress shown in the Zed agent config panel
- Animated spinner during model switching
- Qwen 2.5 Coder 7B
- Available-for-download models shown in the picker, not just locally cached ones
- Model selection persists across restarts
- Session working directory support
- Model picker logic moved to a platform-independent module so it compiles on Windows even without the TUI
- `/models N` shortcut for picking a model by number directly
- `read_website` tool
- Better `read_file` handling and empty reply detection
- Async tool execution
- Fixed CI cross-compilation for macOS, iOS, Linux, and Windows
- npm, PyPI, and Homebrew distribution added

---

*© 2026 [smbCloud](https://smbcloud.xyz/) (Splitfire AB).*