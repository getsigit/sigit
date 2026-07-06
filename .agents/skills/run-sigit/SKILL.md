---
name: run-sigit
description: Build, launch, and drive the sigit AI coding agent — run the ACP server, screenshot the interactive TUI, smoke-test the CLI. Use when asked to run sigit, start the agent, screenshot the chat UI, or verify a change to the binary.
---

# Run sigit

`sigit` is a single Rust binary that picks its mode at startup from whether stdin
is a TTY:

- **ACP mode** (stdin not a TTY): newline-delimited JSON-RPC 2.0 over stdio — the
  Agent Client Protocol surface that Zed / VS Code drive. This is the primary
  programmatic handle. Drive it with **`.claude/skills/run-sigit/driver.mjs`**.
- **Interactive TUI** (stdin is a TTY): a full-screen ratatui chat, Unix-only.
  Drive it under tmux with **`.claude/skills/run-sigit/tui-smoke.sh`**.
- **CLI subcommands**: `sigit login | logout | whoami`, handled before the split.

Both drivers avoid on-device inference: `initialize`, `session/new`, and slash
commands (`/whoami`, `/help`, `/status`) answer **without** loading a multi-GB
GGUF model, so they work on a clean machine with nothing cached and no network.

Paths below are relative to the repo root (`<unit>/`).

## Prerequisites

- Rust toolchain (pinned in `rust-toolchain.toml`); `cargo` on PATH.
- Node ≥ 18 for the ACP driver (`driver.mjs`).
- `tmux` for the TUI smoke test only: `brew install tmux` (macOS) /
  `apt-get install -y tmux` (Linux).

## Build

```bash
cargo build                 # debug binary at target/debug/sigit
```

First build is slow (it compiles `onde` / mistralrs); incremental rebuilds are
sub-second. Use `cargo build --release` for `target/release/sigit` if you want
realistic inference speed — the drivers default to the debug binary.

## Run (agent path)

### ACP server — `driver.mjs`

Spawns the binary in ACP mode, runs `initialize` → `session/new` →
`session/prompt /whoami`, prints every frame, exits 0 on success:

```bash
node .claude/skills/run-sigit/driver.mjs
# SIGIT_BIN=target/release/sigit node .claude/skills/run-sigit/driver.mjs
```

Expected tail:

```
<-- notify session/update "Signed in to siGit Code Cloud as demo@sigit.si."
  "stopReason": "end_turn"
OK — ACP handshake, session, and /whoami round-tripped.
```

The `/whoami` reply arrives as an `agent_message_chunk` notification — the same
streaming surface a real prompt fans out across many chunks. To drive real
streamed inference, send a `session/prompt` with ordinary text instead of a
slash command (needs a cached local model or a signed-in cloud tier).

### Interactive TUI — `tui-smoke.sh`

Launches the TUI under tmux, types `/help`, writes the rendered screen to
`$TMPDIR/sigit-tui.txt` (the "screenshot" for a terminal app), then quits:

```bash
.claude/skills/run-sigit/tui-smoke.sh
cat "${TMPDIR:-/tmp}/sigit-tui.txt"      # view the captured screen
```

To poke it by hand, the same tmux moves the script automates:

```bash
tmux new-session -d -s sigit -x 120 -y 35
tmux send-keys -t sigit './target/debug/sigit' Enter
sleep 6
tmux capture-pane -t sigit -p            # read the screen
tmux send-keys -t sigit '/help' Enter
tmux send-keys -t sigit C-c              # Ctrl+C quits
tmux kill-session -t sigit
```

### CLI smoke

```bash
./target/debug/sigit whoami              # prints the signed-in account, exit 0
```

## Run (human path)

```bash
cargo run                                # stdin is your TTY → launches the TUI
```

A full-screen chat opens; type a message or `/help`, Ctrl+C to quit. Useless
headless or with stdin piped — that path falls through to ACP mode instead.

## Gotchas

- **The ACP server never exits on stdin EOF.** `printf '…' | sigit | head` hangs:
  the process stays alive holding stdout open, so `head` blocks waiting for bytes
  that only stop when you kill it. You must read the response frame and then
  `kill` the child — that's the whole reason `driver.mjs` exists instead of a
  one-line pipe.
- **Piping stdin forces ACP mode.** Any non-TTY stdin (a pipe, `</dev/null`)
  routes to the JSON-RPC server, not the TUI. The TUI needs a real PTY, hence
  tmux.
- **Handshake is intentionally model-free.** `initialize` / `session/new` defer
  GGUF loading to the first real prompt, so they're fast and need no network. A
  text `session/prompt` to an on-device model triggers a ~1–2 GB download on
  first use.
- **The default model depends on sign-in state.** On a signed-in machine the
  picker shows a cloud tier (e.g. `onde-cloud (onde-fast)`); logged out it
  defaults to an on-device model. `sigit whoami` shows which.
- **Logs go to different places per mode.** ACP mode → stderr (the driver prefixes
  them `[sigit]`). TUI mode redirects all stdout/stderr to `$TMPDIR/sigit.log` so
  the ratatui surface stays clean — tail that file to debug the TUI.
- **macOS model cache is shared with the desktop app**, under
  `~/Library/Group Containers/group.com.ondeinference.apps/models/`; other
  platforms use `~/.cache/huggingface/`.

## Troubleshooting

- `binary not found: target/debug/sigit` → run `cargo build` first.
- Driver hangs / times out on `initialize` → you're likely running a stale binary
  or one that crashed at startup; check the `[sigit]` stderr lines it echoes.
- `tmux not installed` from `tui-smoke.sh` → `brew install tmux`.
- TUI capture is blank → increase the `sleep` before `capture-pane`; the banner
  and (lazy) model selection take a few seconds on a cold start.
