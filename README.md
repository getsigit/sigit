# siGit Code

<p align="center">
  <a href="https://crates.io/crates/sigit"><img src="https://img.shields.io/crates/v/sigit?style=flat-square&labelColor=17211D&color=235843" alt="Crates.io"></a>
  <a href="https://pypi.org/project/sigit-code/"><img src="https://img.shields.io/pypi/v/sigit-code?style=flat-square&labelColor=17211D&color=235843" alt="PyPI"></a>
  <a href="https://www.npmjs.com/package/@getsigit/sigit"><img src="https://img.shields.io/npm/v/@getsigit/sigit?style=flat-square&labelColor=17211D&color=235843" alt="npm"></a>
  <a href="https://sigit.si/code"><img src="https://img.shields.io/badge/sigit.si%2Fcode-235843?style=flat-square&labelColor=17211D" alt="Website"></a>
  <a href="https://github.com/getsigit/sigit/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-235843?style=flat-square&labelColor=17211D" alt="License"></a>
</p>

siGit Code is a local-first AI coding agent. It runs on your machine by default, with optional
cloud inference and remote automation when you choose them. No API key is required to start; the
on-device path needs none at all.

It is the agent runtime for [siGit Factory](https://factory.sigit.si/), the hosted version of the
same product. Run it yourself, as below, or use siGit Code Cloud there if you would rather not
host a model locally. [sigit.si](https://sigit.si) is Git hosting built for AI workflows.

It works in any codebase. In smbCloud repos it is more useful out of the box because it already understands the Rust workspace layout, deploy flows, auth boundaries, and GresIQ.

You can use it in two ways:

- **ACP mode:** Zed or another ACP-compatible editor starts it over stdio with the `--acp` argument
- **Terminal mode:** run `sigit` for the interactive chat UI

Every ACP client must pass `--acp`. It takes no other arguments, and it is what
tells the binary to speak the Agent Client Protocol instead of opening the
terminal UI.

| Platform | ACP mode | Terminal mode |
|----------|----------|---------------|
| macOS | ✓ | ✓ |
| Linux | ✓ | ✓ |
| Windows | ✓ | not yet |

## Install

```sh
cargo install sigit
```

| Method | Command |
|--------|---------|
| Homebrew | `brew tap getsigit/tap && brew trust --tap getsigit/tap && brew install sigit` |
| pip | `pip install sigit-code` |
| uv | `uvx --from sigit-code sigit` |
| npm | `npm install -g @getsigit/sigit` |
| NuGet | `dotnet tool install --global SiGit.Code` |
| winget | `winget install smbCloud.siGitCode` |
| Scoop | `scoop bucket add getsigit https://github.com/getsigit/scoop-bucket && scoop install sigit` |
| AUR | `yay -S sigit-bin` |
| Debian/Ubuntu | download the `.deb` from [releases](https://github.com/getsigit/sigit/releases), then `sudo dpkg -i sigit_*.deb` |
| Fedora/RHEL | download the `.rpm` from [releases](https://github.com/getsigit/sigit/releases), then `sudo rpm -i sigit-*.rpm` |

## First run

The first launch downloads a GGUF model from Hugging Face. Expect roughly 1 to 2 GB, depending on the model. After that, loads come from disk and are much faster.

On macOS, siGit Code shares its model cache with the desktop app through an App Group container. If the desktop app already downloaded the model, the CLI reuses it.

## Zed setup

Add this to `~/.config/zed/settings.json`:

```json
{
  "agent_servers": {
    "siGit Code": {
      "type": "custom",
      "command": "/absolute/path/to/sigit",
      "args": ["--acp"]
    }
  }
}
```

Use the full absolute path. `~` does not expand here.

## Xcode custom agent

In Xcode, open **Settings > Intelligence > Agents**, add a custom agent, and set:

- **Executable:** the absolute path to `sigit`
- **Arguments:** `--acp`

In Xcode the `--acp` mode also loads the selected on-device model on the first
prompt, so you do not need to send `/load` from the Xcode chat.

To let siGit use Xcode's build, test, and project tools, enable **Allow external
agents to use Xcode tools** in Xcode's Intelligence settings, keep the project
open, and add this to `~/.config/sigit/mcp.toml`:

```toml
[[server]]
name = "xcode"
command = "xcrun"
args = ["mcpbridge"]
```

## VS Code

### With siGit Code Extension

Install from the [Visual Studio Code Marketplace](https://marketplace.visualstudio.com/items?itemName=getsigit.sigit-code).

```jsonc
{
  "sigit.agents": {
    "sigit": {
      "name": "siGit (on-device)",
      "command": "sigit",
      "args": ["--acp"],
      "env": {}
    },
  },
  "sigit.agent.default": "sigit"
}
```

### With ACP Client

Install [ACP Client](https://marketplace.visualstudio.com/items?itemName=formulahendry.acp-client), then add:

```json
{
  "acp.agents": {
    "siGit Code": {
      "command": "sigit",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

## ACP support

siGit Code speaks [Agent Client Protocol](https://agentclientprotocol.com) v1 over stdio.
As of 1.7.0 it implements the whole stable v1 surface an agent can offer:

| Area | What is supported |
|------|-------------------|
| Sessions | `session/new`, `load`, `resume`, `fork`, `list` (paged, with a `cwd` filter), `close`, `delete`, and multi-root projects through `additionalDirectories` |
| Prompts | Text, images, audio, embedded resources and resource links. Stop reasons `end_turn`, `max_tokens`, `max_turn_requests`, `refusal` and `cancelled` |
| Updates | Message and thought chunks with a `messageId`, tool calls with diffs, locations and embedded terminals, plans, `usage_update`, `session_info_update`, slash commands |
| Cancellation | `session/cancel`, and `$/cancel_request` for a prompt or a sign-in |
| Permissions | `session/request_permission` with allow or deny, once or for the session. Manual, Auto and Plan as config options and as session modes |
| Client features | `fs/read_text_file` and `fs/write_text_file`, `terminal/*`, URL-mode elicitation and boolean config options, each used only when the client advertises it |
| MCP | stdio and Streamable HTTP servers passed in `mcpServers` |
| Auth | `authenticate` (browser sign-in) and `logout` |

`session/fork` is still marked unstable in the protocol. Draft v2 is not enabled.

## Terminal mode

Run `sigit` in a terminal and you get the same model and system prompt as the editor integration, just in a full-screen chat UI.

Terminal mode currently needs Unix terminal behavior, so it works on macOS and Linux only.

## Headless and CI mode

Use `sigit run` to execute the same agent from scripts, CI, or another siGit Factory client:

```sh
sigit run "Review this repository and run the focused tests" --cwd .
```

Every run receives a durable session ID. The ID is printed to stderr in text mode and included
in every event in JSONL mode. Resume the same conversation from a later command—or import it in
an ACP client—by passing that ID:

```sh
sigit run "Continue with the next issue" --resume <session-id>
```

For automation, JSONL output provides session, assistant-delta, tool-call, tool-result, result,
and error events on stdout. Logs remain on stderr.

```sh
sigit run "Run the checks" --output jsonl --allow-tool run_command
```

Mutating tools still follow siGit's permission policy. Because a headless run cannot answer an
interactive permission prompt, approve only the tools the run needs with repeatable
`--allow-tool` flags. The legacy `sigit -p "<prompt>"` form remains supported. See the
[headless execution reference](docs/headless.md) for the event contract and exit codes.

## Telemetry

siGit Code itself collects no usage analytics. On-device models run on
[Onde Inference](https://ondeinference.com), and Onde can report model timing to its own
dashboard. That reporting only switches on in builds where Onde's credentials were embedded at
compile time. When it is on, siGit Code sends:

- one event when an on-device model finishes loading: the model's Hugging Face repo ID, its
  display name, and how long the load took;
- one event per on-device response: the model ID, a per-process request ID, the response time,
  the time to first token when it was measured, and whether it succeeded, errored, or was
  cancelled.

Nothing from your conversation goes with them: no prompts, responses, file paths, or code.
siGit Code doesn't set a machine identifier, so every install reports as the same
`onde-unknown` edge, and it sends no location. Models you run through siGit Code Cloud or your
own endpoint send no Onde events.

To see whether your build reports, load an on-device model and check the log for
`ChatEngine: pulse telemetry enabled` or `ChatEngine: pulse telemetry disabled`. siGit Code logs
to stderr, which an editor shows in its agent log. Terminal mode writes it to
`$TMPDIR/sigit.log`.

To turn it off, set `ONDE_DISABLE_PULSE=1` in the environment siGit Code runs in. For an
editor, put it in the agent's `env` block:

```json
"env": { "ONDE_DISABLE_PULSE": "1" }
```

## Platform support

| Platform | Architecture |
|----------|-------------|
| macOS | arm64, x64 |
| Linux (glibc) | arm64, x64 |
| Windows | arm64, x64 |

## License

[Apache 2.0](https://github.com/getsigit/sigit/blob/main/LICENSE)

## Copyright

© 2026 [siGit Code & Deploy](https://sigit.si). See [NOTICE](https://github.com/getsigit/sigit/blob/main/NOTICE) for the rights holder and distributor.
