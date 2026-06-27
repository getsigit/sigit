#!/usr/bin/env bash
# Drive the interactive ratatui TUI under tmux: launch it, type /help, dump the
# rendered screen to a file ("screenshot" for a terminal app), then quit cleanly.
#
# The TUI only starts when stdin is a real TTY, so it must run inside a terminal
# multiplexer — tmux gives us one plus `capture-pane` to read what it drew.
# Requires tmux (`brew install tmux`). Unix only (the TUI is #[cfg(unix)]).
#
# Usage: .claude/skills/run-sigit/tui-smoke.sh [path-to-binary]
set -euo pipefail

BIN="${1:-${SIGIT_BIN:-target/debug/sigit}}"
SESSION="sigit-smoke-$$"
OUT="${TMPDIR:-/tmp}/sigit-tui.txt"

if [[ ! -x "$BIN" ]]; then
  echo "binary not found/executable: $BIN — run \`cargo build\` first" >&2
  exit 2
fi
command -v tmux >/dev/null || { echo "tmux not installed (brew install tmux)" >&2; exit 2; }

cleanup() { tmux kill-session -t "$SESSION" 2>/dev/null || true; }
trap cleanup EXIT

tmux new-session -d -s "$SESSION" -x 120 -y 35
tmux send-keys -t "$SESSION" "$BIN" Enter
sleep 6                                   # banner + (lazy) model selection
tmux send-keys -t "$SESSION" '/help' Enter
sleep 2
tmux capture-pane -t "$SESSION" -p > "$OUT"
tmux send-keys -t "$SESSION" C-c          # Ctrl+C quits
sleep 1

echo "Captured TUI screen -> $OUT"
if grep -q '/whoami' "$OUT"; then
  echo "OK — TUI launched and /help rendered."
else
  echo "FAILED — /help output not found in capture:" >&2
  sed '/^$/d' "$OUT" | head -40 >&2
  exit 1
fi
