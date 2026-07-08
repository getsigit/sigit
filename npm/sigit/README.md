<h1 align="center">siGit Code</h1>

<p align="center">
  A local coding agent powered by <a href="https://ondeinference.com">Onde Inference</a>.<br>
  Runs on your machine. No API keys. No cloud round-trips.
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/@smbcloud/sigit"><img src="https://img.shields.io/npm/v/@smbcloud/sigit?style=flat-square&labelColor=17211D&color=235843" alt="npm"></a>
  <a href="https://crates.io/crates/sigit"><img src="https://img.shields.io/crates/v/sigit?style=flat-square&labelColor=17211D&color=235843" alt="Crates.io"></a>
  <a href="https://pypi.org/project/sigit-code/"><img src="https://img.shields.io/pypi/v/sigit-code?style=flat-square&labelColor=17211D&color=235843" alt="PyPI"></a>
  <a href="https://smbcloud.xyz"><img src="https://img.shields.io/badge/smbcloud.xyz-235843?style=flat-square&labelColor=17211D" alt="Website"></a>
  <a href="https://github.com/getsigit/sigit/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-235843?style=flat-square&labelColor=17211D" alt="License"></a>
</p>

## Install

```sh
npm install -g @smbcloud/sigit
```

npm pulls in the right binary for your platform automatically.

Supported targets:

- macOS, Apple Silicon and Intel
- Linux, x64 and arm64
- Windows, x64 and arm64

## Other install methods

| Method | Command |
|---|---|
| Homebrew | `brew tap --force getsigit/tap && brew install sigit` |
| pip | `pip install sigit-code` |
| uv | `uvx --from sigit-code sigit` |
| Cargo | `cargo install sigit` |

## Usage

```sh
sigit
```

That starts the local terminal UI.

### Zed

Add this to `~/.config/zed/settings.json`:

```json
{
  "agent_servers": {
    "siGit Code": {
      "type": "custom",
      "command": "sigit"
    }
  }
}
```

Then select **siGit Code** in the Zed assistant panel.

### VS Code with ACP Client

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

## Platform support

| Platform | Architecture | Package |
|---|---|---|
| macOS | Apple Silicon (arm64) | [`@smbcloud/sigit-darwin-arm64`](https://www.npmjs.com/package/@smbcloud/sigit-darwin-arm64) |
| macOS | Intel (x64) | [`@smbcloud/sigit-darwin-x64`](https://www.npmjs.com/package/@smbcloud/sigit-darwin-x64) |
| Linux | x64 | [`@smbcloud/sigit-linux-x64`](https://www.npmjs.com/package/@smbcloud/sigit-linux-x64) |
| Linux | arm64 | [`@smbcloud/sigit-linux-arm64`](https://www.npmjs.com/package/@smbcloud/sigit-linux-arm64) |
| Windows | x64 | [`@smbcloud/sigit-windows-x64`](https://www.npmjs.com/package/@smbcloud/sigit-windows-x64) |
| Windows | arm64 | [`@smbcloud/sigit-windows-arm64`](https://www.npmjs.com/package/@smbcloud/sigit-windows-arm64) |

## Links

- [smbCloud](https://smbcloud.xyz/)
- [Source code](https://github.com/getsigit/sigit)
- [Issues](https://github.com/getsigit/sigit/issues)
- [Onde Inference](https://ondeinference.com)

## License

[Apache-2.0](https://github.com/getsigit/sigit/blob/main/LICENSE)

## Copyright

© 2026 [Splitfire AB](https://5mb.app) ([siGit Code & Deploy](https://sigit.si)).
