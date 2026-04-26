<h1 align="center">siGit Code</h1>

<p align="center">
  <strong>AI coding agent powered by local LLM via <a href="https://ondeinference.com">Onde Inference</a>.</strong><br>
  ACP-compatible agent that runs entirely on your machine — no API keys, no cloud.
</p>

<p align="center">
  <a href="https://pypi.org/project/sigit-code/"><img src="https://img.shields.io/pypi/v/sigit-code?style=flat-square&labelColor=17211D&color=235843" alt="PyPI"></a>
  <a href="https://crates.io/crates/sigit"><img src="https://img.shields.io/crates/v/sigit?style=flat-square&labelColor=17211D&color=235843" alt="Crates.io"></a>
  <a href="https://www.npmjs.com/package/@smbcloud/sigit"><img src="https://img.shields.io/npm/v/@smbcloud/sigit?style=flat-square&labelColor=17211D&color=235843" alt="npm"></a>
  <a href="https://smbcloud.xyz"><img src="https://img.shields.io/badge/smbcloud.xyz-235843?style=flat-square&labelColor=17211D" alt="Website"></a>
  <a href="https://github.com/getsigit/sigit/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-235843?style=flat-square&labelColor=17211D" alt="License"></a>
</p>

<br>

---

## Install

Use `pip` or `uv`:

```sh
pip install sigit-code
uvx --from sigit-code sigit
```

Installs the native `sigit` binary for your platform — no compiler, no Node.js, no runtime dependencies.

## Quick start

### Interactive TUI

```sh
sigit
```

A terminal UI opens where you can chat with a local LLM coding agent directly.

### Zed editor (ACP agent)

siGit works as an [ACP-compatible](https://github.com/nicobailon/agent-client-protocol) agent in [Zed](https://zed.dev). Add this to your Zed settings:

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

Then select **sigit** as your agent profile in the Zed assistant panel.

## Other installation methods

| Method | Command |
|--------|---------|
| npm | `npm install -g @smbcloud/sigit` |
| Homebrew | `brew tap getsigit/tap && brew install sigit` |
| Cargo | `cargo install sigit` |

### From source

```sh
git clone https://github.com/getsigit/sigit
cd sigit
cargo build --release
./target/release/sigit
```

## Platform support

Pre-built native binaries ship for every major platform:

| Platform      | Architecture |
|---------------|--------------|
| macOS         | arm64, x64   |
| Linux (glibc) | arm64, x64   |
| Windows       | arm64, x64   |

## Source & issues

This package ships a pre-built native binary. Source lives at
[github.com/getsigit/sigit](https://github.com/getsigit/sigit) —
file bugs and feature requests there.

## License

Licensed under **Apache 2.0**.

- [Apache License 2.0](https://github.com/getsigit/sigit/blob/main/LICENSE)

---

## Copyright

© 2026 [smbCloud](https://smbcloud.xyz/) (Splitfire AB).