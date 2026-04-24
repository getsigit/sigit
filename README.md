# siGit Code

[![Crates.io Version](https://img.shields.io/crates/v/sigit)](https://crates.io/crates/sigit)

A coding agent for [smbCloud](https://smbcloud.xyz/) that runs entirely on your machine. No API keys. No cloud round-trips. The model lives in your local HuggingFace cache.

siGit is meant to be a general coding agent, but it is especially good in smbCloud codebases. It already knows the rough shape of the platform: Rust workspaces with focused crates, Rails services, deploy flows, auth boundaries, and platform-managed services like GresIQ. In smbCloud repos, that means it can usually give more grounded answers with less back-and-forth.

siGit has two modes:

- ACP mode, where Zed or another ACP-compatible editor starts it over stdio
- an interactive terminal chat when you run `sigit` yourself

Current platform support:

- macOS: ACP mode and interactive terminal mode
- Linux: ACP mode and interactive terminal mode
- Windows: ACP mode only for now

## What siGit knows about smbCloud

When siGit is working in an smbCloud repo, it should lean on platform context instead of treating everything like a generic cloud app. That includes things like:

- the difference between platform user flows and tenant app auth flows
- the fact that `Project` is the umbrella workspace, while app-like resources such as `FrontendApp`, `AuthApp`, and GresIQ are separate deployable units
- the fact that Next.js SSR deploys are not the same as the generic git-push path
- the fact that smbCloud repos usually prefer existing workspace patterns and crate boundaries over new abstractions

Outside smbCloud, it should still behave like a normal coding agent and not force platform-specific advice where it does not belong.

## Install

```sh
cargo install sigit
```

## First run

The first time siGit starts, it downloads a GGUF model (~1–2 GB) from HuggingFace. Subsequent starts load from disk in a few seconds.

On macOS, siGit shares its model cache with the siGit desktop app through an App Group container. If the desktop app already downloaded the model, the CLI will reuse it.

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

Use the full absolute path. `~` will not be expanded here.

## Terminal mode

If you run `sigit` directly in a terminal, it opens an interactive chat UI. It uses the same model and system prompt as the editor integration, so it is useful for quick questions when you do not want to open Zed first.

That terminal mode currently depends on Unix terminal behavior, so it works on macOS and Linux. On Windows, siGit supports ACP/editor mode only right now.

## Copyright

2026 smbCloud (Splitfire AB).
