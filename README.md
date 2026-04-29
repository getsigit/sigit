# siGit Code

<p align="center">
  <a href="https://crates.io/crates/sigit"><img src="https://img.shields.io/crates/v/sigit?style=flat-square&labelColor=17211D&color=235843" alt="Crates.io"></a>
  <a href="https://pypi.org/project/sigit-code/"><img src="https://img.shields.io/pypi/v/sigit-code?style=flat-square&labelColor=17211D&color=235843" alt="PyPI"></a>
  <a href="https://www.npmjs.com/package/@smbcloud/sigit"><img src="https://img.shields.io/npm/v/@smbcloud/sigit?style=flat-square&labelColor=17211D&color=235843" alt="npm"></a>
  <a href="https://smbcloud.xyz"><img src="https://img.shields.io/badge/smbcloud.xyz-235843?style=flat-square&labelColor=17211D" alt="Website"></a>
  <a href="https://github.com/getsigit/sigit/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-235843?style=flat-square&labelColor=17211D" alt="License"></a>
</p>

siGit is a coding agent that runs on your machine. No API keys, no cloud round-trips, no subscription.

It works with any codebase. It knows smbCloud repos a bit better — the Rust workspace layout, deploy flows, auth boundaries, GresIQ — so you spend less time explaining the setup.

Two modes:

- **ACP mode** — Zed or another ACP-compatible editor starts it over stdio
- **Terminal mode** — run `sigit` for an interactive chat UI

| Platform | ACP mode | Terminal mode |
|----------|----------|---------------|
| macOS | ✓ | ✓ |
| Linux | ✓ | ✓ |
| Windows | ✓ | not yet |

## smbCloud context

In an smbCloud repo, siGit knows the terrain:

- platform user flows and tenant app auth flows are different things
- `Project` is the umbrella workspace; `FrontendApp`, `AuthApp`, and GresIQ are separate deployable units
- Next.js SSR deploys aren't the same as the git-push path
- existing workspace patterns and crate boundaries over new abstractions

In other repos it stays general and doesn't pretend otherwise.

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

On first launch siGit downloads a GGUF model from Hugging Face, usually 1–2 GB. After that it loads from disk in a few seconds.

On macOS, the model cache is shared with the siGit desktop app via an App Group container. If the desktop app already pulled the model, the CLI reuses it.

## Zed setup

Add to `~/.config/zed/settings.json`:

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

Use the full absolute path. `~` doesn't expand here.

## VSCode via ACP Client extension

Install [ACP client](https://marketplace.visualstudio.com/items?itemName=formulahendry.acp-client):

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

Run `sigit` in a terminal and you get an interactive chat UI. Same model and system prompt as the editor integration, just without Zed.

Terminal mode needs Unix terminal behavior, so macOS and Linux only for now.

## Platform support

| Platform | Architecture |
|----------|-------------|
| macOS | arm64, x64 |
| Linux (glibc) | arm64, x64 |
| Windows | arm64, x64 |

## License

[Apache 2.0](https://github.com/getsigit/sigit/blob/main/LICENSE)

## Copyright

© 2026 [smbCloud](https://smbcloud.xyz/) (Splitfire AB).
