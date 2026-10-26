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
Each run rewrites that metadata with its own `--cwd` and `--add-dir` values, so a resume records
where the session last ran. Pass the same `--add-dir` flags again to keep extra roots attached.

Use repeatable `--add-dir <path>` flags for multi-root workspaces. Mutating tools that would ask
for interactive permission are denied in headless mode unless granted with
`--allow-tool <name>`. A repeatable `--deny-tool <name>` takes precedence over grants and
settings.

## Choosing the model

With no flag, a run uses the provider override (`OPENAI_BASE_URL` and `OPENAI_API_KEY`, or the
active profile in `providers.toml`) when there is one, and otherwise the default siGit Code Cloud
tier when you are signed in and local inference is off. `--model <tier>` picks a siGit Code
Cloud tier for the run instead:

```sh
sigit run "Run the focused tests" --model prism --allow-tool run_command
```

The value is a tier name from `/models` (`prism`) or the model id sent on the wire
(`onde-prism`). An unknown name is a usage error, exit code 2. The flag wins over the provider
override and the local-inference setting, and it needs a signed-in session.

## Tool rounds

A run makes at most 24 rounds of tool calls. `--max-tool-rounds <n>`, or the
`SIGIT_MAX_TOOL_ROUNDS` environment variable, sets a different cap from 1 to 500; the flag wins.
When the cap is reached, or the model otherwise ends without saying anything, siGit Code asks it
once for a final message, so stdout is not left empty by a run that was cut off mid-task.

## JSONL output

Pass `--output jsonl` for a machine-readable stdout stream:

```sh
sigit run "Run the focused tests" --output jsonl --allow-tool run_command
```

Each line is one JSON object. Every event includes `type` and `session_id`. A run that gets as far
as inference starts with a `session` event. A run that fails before that, because no remote
provider is configured or the `--resume` session doesn't exist, emits a single `error` event and
nothing else.

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
