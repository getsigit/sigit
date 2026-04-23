# siGit Code

[![Crates.io Version](https://img.shields.io/crates/v/sigit)](https://crates.io/crates/sigit)

A coding agent for [smbCloud](https://smbcloud.xyz/) that runs entirely on your machine. No API keys. No cloud round-trips. The model lives in your local HuggingFace cache.

Two modes: an ACP agent that Zed (and other ACP-compatible editors) spawns over stdio, or an interactive terminal chat when you run it directly.

## Install

```sh
cargo install sigit
```

## First run

The first time siGit starts, it downloads a GGUF model (~1–2 GB) from HuggingFace. Subsequent starts load from disk in a few seconds.

On macOS, siGit shares the model cache with the siGit desktop app via an App Group container — if you've already downloaded it there, the CLI picks it up automatically.

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

Use the absolute path — `~` expansion won't work here.

## Terminal mode

Running `sigit` directly in a terminal opens an interactive chat UI. Same model, same system prompt — handy for quick questions without opening an editor.

## Copyright

2026 smbCloud (Splitfire AB).
