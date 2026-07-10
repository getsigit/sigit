# siGit Code Hooks

Hooks allow you to run custom scripts at key moments in the agent lifecycle:
- **SessionStart**: When a session begins (new/load/fork)
- **PreToolUse**: Before a tool is executed
- **PostToolUse**: After a tool is executed

## Configuration

Hooks are configured in `~/.config/sigit/settings.toml` under the `[hooks]` section:

```toml
[hooks]
session_start = ["echo 'Starting siGit in {cwd}'"]
pre_tool_use = ["echo 'About to run {tool_name} with {tool_args_len} bytes of args'"]
post_tool_use = ["echo 'Finished {tool_name}, result was {tool_result_len} bytes'"]
```

Each hook is a list of shell commands. Commands run in the session working directory and inherit the environment.

## Variable Substitution

Hooks support substitution for context variables:

### SessionStart
- `{cwd}` - The session working directory
- `{session_id}` - The unique session identifier

### PreToolUse
- `{tool_name}` - The name of the tool being called
- `{tool_args_len}` - Length of the tool arguments in bytes
- `{cwd}` - The session working directory

### PostToolUse
- `{tool_name}` - The name of the tool that ran
- `{tool_result_len}` - Length of the tool result in bytes
- `{cwd}` - The session working directory

Every `{var}` value is quote-escaped for the platform shell before it's spliced
into the command line, so a directory name or an MCP-server-supplied tool name
that happens to contain shell metacharacters is always treated as literal text,
never executed. Windows `cmd.exe` quoting is best-effort (`cmd` has no fully
safe quoting story); the same values are also exported as environment
variables — `SIGIT_HOOK_CWD`, `SIGIT_HOOK_SESSION_ID`, `SIGIT_HOOK_TOOL_NAME`,
`SIGIT_HOOK_TOOL_ARGS_LEN`, `SIGIT_HOOK_TOOL_RESULT_LEN` — and reading those
in your script instead of `{var}` sidesteps quoting entirely on any platform.

## Examples

### Log all tool usage to a file

```toml
[hooks]
pre_tool_use = ["echo '[{tool_name}]' >> /tmp/sigit-tools.log"]
post_tool_use = ["echo '  → {tool_result_len} bytes' >> /tmp/sigit-tools.log"]
```

### Send notifications

```toml
[hooks]
session_start = ["notify-send 'siGit started in {cwd}'"]
```

### Build on session start

```toml
[hooks]
session_start = ["cd {cwd} && cargo build"]
```

### Track metrics

```toml
[hooks]
pre_tool_use = ["echo {tool_name} | tee -a ~/.sigit/tool_usage.txt"]
post_tool_use = ["wc -c <<< {tool_result_len} >> ~/.sigit/result_sizes.txt"]
```

## Notes

- Hooks are optional. When none are configured, each tool call still does one
  lightweight settings-file read to check for that; there is no shell process
  spawned and no other measurable overhead.
- Hook failures (non-zero exit codes) are logged as warnings but do not interrupt the session.
- Hooks are run synchronously, so slow hooks will impact agent responsiveness — this applies
  in the interactive TUI too, not just ACP/headless mode.
- `PreToolUse` and `PostToolUse` hooks fire in every mode (TUI, ACP, headless) and for every
  call to the shared tool-execution path — including tool calls made by a `task` subagent, not
  just the top-level agent.
- `SessionStart` hooks currently only fire on ACP session lifecycle events (new/load/fork
  session). The interactive TUI does not have an equivalent session-start hook point yet.
