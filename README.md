# siGit Code

<p align="center">
  <a href="https://crates.io/crates/sigit"><img src="https://img.shields.io/crates/v/sigit?style=flat-square&labelColor=17211D&color=235843" alt="Crates.io"></a>
  <a href="https://pypi.org/project/sigit-code/"><img src="https://img.shields.io/pypi/v/sigit-code?style=flat-square&labelColor=17211D&color=235843" alt="PyPI"></a>
  <a href="https://www.npmjs.com/package/@smbcloud/sigit"><img src="https://img.shields.io/npm/v/@smbcloud/sigit?style=flat-square&labelColor=17211D&color=235843" alt="npm"></a>
  <a href="https://smbcloud.xyz"><img src="https://img.shields.io/badge/smbcloud.xyz-235843?style=flat-square&labelColor=17211D" alt="Website"></a>
  <a href="https://github.com/getsigit/sigit/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-235843?style=flat-square&labelColor=17211D" alt="License"></a>
</p>

siGit Code is a local coding agent. It runs on your machine, not someone else's. No API keys, no cloud round-trips, no subscription.

Its home is [code.sigit.si](https://code.sigit.si). You can run it yourself, as below, or use the hosted version (siGit Code Cloud) there if you would rather not run a model locally. [sigit.si](https://sigit.si) is Git hosting built for AI workflows.

It works in any codebase. In smbCloud repos it is more useful out of the box because it already understands the Rust workspace layout, deploy flows, auth boundaries, and GresIQ.

You can use it in two ways:

- **ACP mode:** Zed or another ACP-compatible editor starts it over stdio
- **Terminal mode:** run `sigit` for the interactive chat UI

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
| Homebrew | `brew tap getsigit/tap && brew install sigit` |
| pip | `pip install sigit-code` |
| uv | `uvx --from sigit-code sigit` |
| npm | `npm install -g @smbcloud/sigit` |

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
      "command": "/absolute/path/to/sigit"
    }
  }
}
```

Use the full absolute path. `~` does not expand here.

## VS Code

### With siGit Code Extension

Install from the [Visual Studio Code Marketplace](https://marketplace.visualstudio.com/items?itemName=getsigit.sigit-code).

```jsonc
{
  "sigit.agents": {
    "sigit": {
      "name": "siGit (on-device)",
      "command": "sigit",
      "args": [],
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
      "args": [],
      "env": {}
    }
  }
}
```

## Terminal mode

Run `sigit` in a terminal and you get the same model and system prompt as the editor integration, just in a full-screen chat UI.

Terminal mode currently needs Unix terminal behavior, so it works on macOS and Linux only.

## Platform support

| Platform | Architecture |
|----------|-------------|
| macOS | arm64, x64 |
| Linux (glibc) | arm64, x64 |
| Windows | arm64, x64 |

## License

[Apache 2.0](https://github.com/getsigit/sigit/blob/main/LICENSE)

## Copyright

© 2026 [Splitfire AB](https://5mb.app) ([siGit Code & Deploy](https://sigit.si)).
