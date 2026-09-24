# Headless execution

`sigit run` exposes the siGit Code agent runtime to scripts, CI, and siGit Factory clients.
It uses the same provider selection, project instructions, skills, tools, MCP servers, and
permission policy as the interactive and ACP surfaces.

## Run and resume

```sh
sigit run "Review this repository" --cwd /path/to/repository
```

A new run receives a UUID session ID and saves its conversation under the siGit configuration
directory. Text mode prints the ID to stderr as `Session: <id>`. Resume it later with:

```sh
sigit run "Continue with the fixes" --resume <id>
```

The saved metadata includes the primary working directory, additional project roots, and model.
This makes the same session discoverable through ACP `session/list` and loadable by an editor.

Use repeatable `--add-dir <path>` flags for multi-root workspaces. Mutating tools that would ask
for interactive permission are denied in headless mode unless granted with
`--allow-tool <name>`. A repeatable `--deny-tool <name>` takes precedence over grants and
settings.

## JSONL output

Pass `--output jsonl` for a machine-readable stdout stream:

```sh
sigit run "Run the focused tests" --output jsonl --allow-tool run_command
```

Each line is one JSON object. Every event includes `type` and `session_id`.

| Type | Additional fields | Meaning |
|---|---|---|
| `session` | `resumed` | Identifies the run before inference starts. |
| `assistant_delta` | `text` | One visible streamed response fragment. |
| `tool_call` | `tool_call_id`, `name`, `arguments` | A tool requested by the model. `arguments` is the tool's JSON string. |
| `tool_result` | `tool_call_id`, `name`, `content` | The executed result or permission denial returned to the model. |
| `result` | `text`, `tool_rounds` | The completed turn and its final visible text. |
| `error` | `message` | A provider, session, or inference failure. |

Logs and diagnostics stay on stderr. `--quiet` applies only to text output and cannot be combined
with JSONL output.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The turn completed, including turns where a tool was denied by policy. |
| `1` | Provider resolution, session restoration, inference, or tool-loop failure. |
| `2` | Invalid command-line arguments or working directories. |

The earlier `sigit -p "<prompt>"` syntax remains an alias for headless execution and accepts the
same flags.
