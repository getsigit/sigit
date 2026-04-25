<h1 align="center">siGit Code</h1>

<p align="center">
  AI coding agent powered by local LLM via Onde Inference.
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/@smbcloud/sigit"><img src="https://img.shields.io/npm/v/@smbcloud/sigit?style=flat-square&labelColor=17211D&color=235843" alt="npm"></a>
  <a href="https://crates.io/crates/sigit"><img src="https://img.shields.io/crates/v/sigit?style=flat-square&labelColor=17211D&color=235843" alt="crates.io"></a>
  <a href="https://pypi.org/project/sigit/"><img src="https://img.shields.io/pypi/v/sigit?style=flat-square&labelColor=17211D&color=235843" alt="PyPI"></a>
</p>

---

## Install

```sh
npm install -g @smbcloud/sigit
```

The right binary for your platform gets pulled in automatically. Works on macOS (Apple Silicon and Intel), Linux (x64 and arm64), and Windows (x64 and arm64).

### Other ways to install

| Method | Command |
|---|---|
| **Homebrew** | `brew tap getsigit/tap && brew install sigit` |
| **pip** | `pip install sigit-code` |
| **uv** | `uvx --from sigit-code sigit` |
| **Cargo** | `cargo install sigit` |

---

## Usage

```sh
sigit
```

Opens a TUI coding agent that runs entirely on your device using a local LLM.

### Zed ACP (Agent Control Protocol)

Add siGit as an agent in Zed by adding this to your settings:

```json
{
  "agent": {
    "profiles": {
      "sigit": {
        "provider": "acp",
        "binary": "sigit",
        "args": ["--acp"]
      }
    }
  }
}
```

---

## Platform support

| Platform | Architecture | Package |
|---|---|---|
| macOS | Apple Silicon (arm64) | `@smbcloud/sigit-darwin-arm64` |
| macOS | Intel (x64) | `@smbcloud/sigit-darwin-x64` |
| Linux | x64 | `@smbcloud/sigit-linux-x64` |
| Linux | arm64 | `@smbcloud/sigit-linux-arm64` |
| Windows | x64 | `@smbcloud/sigit-windows-x64` |
| Windows | arm64 | `@smbcloud/sigit-windows-arm64` |

---

## Links

- [Source code](https://github.com/getsigit/sigit)
- [Issues](https://github.com/getsigit/sigit/issues)

## License

[Apache-2.0](https://github.com/getsigit/sigit/blob/main/LICENSE)

## Copyright

2026 smbCloud (Splitfire AB).
