<h1 align="center">siGit Code</h1>

<p align="center">
  A local coding agent powered by <a href="https://ondeinference.com">Onde Inference</a>.<br>
  Runs on your machine. No API keys. No cloud round-trips.
</p>

<p align="center">
  <a href="https://pypi.org/project/sigit-code/"><img src="https://img.shields.io/pypi/v/sigit-code?style=flat-square&labelColor=17211D&color=235843" alt="PyPI"></a>
  <a href="https://crates.io/crates/sigit"><img src="https://img.shields.io/crates/v/sigit?style=flat-square&labelColor=17211D&color=235843" alt="Crates.io"></a>
  <a href="https://www.npmjs.com/package/@smbcloud/sigit"><img src="https://img.shields.io/npm/v/@smbcloud/sigit?style=flat-square&labelColor=17211D&color=235843" alt="npm"></a>
  <a href="https://smbcloud.xyz"><img src="https://img.shields.io/badge/smbcloud.xyz-235843?style=flat-square&labelColor=17211D" alt="Website"></a>
  <a href="https://github.com/getsigit/sigit/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-235843?style=flat-square&labelColor=17211D" alt="License"></a>
</p>

## Install

```sh
pip install sigit-code
uvx --from sigit-code sigit
```

This installs a native `sigit` binary for your platform. You do not need a compiler or extra runtime setup.

## Quick start

### Terminal mode

```sh
sigit
```

That opens the local chat UI.

### Zed

siGit Code works as an [ACP-compatible](https://github.com/nicobailon/agent-client-protocol) agent in [Zed](https://zed.dev). Add this to your Zed settings:

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

Then pick **siGit Code** in the assistant panel.

## Other install methods

| Method | Command |
|--------|---------|
| npm | `npm install -g @smbcloud/sigit` |
| Homebrew | `brew tap --force-auto-update getsigit/tap && brew install sigit` |
| Cargo | `cargo install sigit` |

### From source

```sh
git clone https://github.com/getsigit/sigit
cd sigit
cargo build --release
./target/release/sigit
```

## Platform support

| Platform | Architecture |
|----------|--------------|
| macOS | arm64, x64 |
| Linux (glibc) | arm64, x64 |
| Windows | arm64, x64 |

## Source and issues

This package ships a prebuilt binary. The source code lives at [github.com/getsigit/sigit](https://github.com/getsigit/sigit). If something breaks, file the issue there.

## License

[Apache 2.0](https://github.com/getsigit/sigit/blob/main/LICENSE)

## Copyright

© 2026 [Splitfire AB](https://5mb.app) ([siGit Code & Deploy](https://sigit.si)).
