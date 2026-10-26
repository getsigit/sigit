<p align="center">
  <img src="docs/assets/logo.png" alt="siGit Code" width="96">
</p>

<h1 align="center">siGit Code</h1>

<p align="center"><strong>The coding agent that runs where your code lives.</strong></p>
<p align="center">A modular on-device inference LLM coding agent. Core component of siGit Factory.</p>

<p align="center">
  <a href="https://crates.io/crates/sigit"><img src="https://img.shields.io/crates/v/sigit?style=flat-square&labelColor=17211D&color=235843" alt="Crates.io"></a>
  <a href="https://pypi.org/project/sigit-code/"><img src="https://img.shields.io/pypi/v/sigit-code?style=flat-square&labelColor=17211D&color=235843" alt="PyPI"></a>
  <a href="https://www.npmjs.com/package/@getsigit/sigit"><img src="https://img.shields.io/npm/v/@getsigit/sigit?style=flat-square&labelColor=17211D&color=235843" alt="npm"></a>
  <a href="https://snapcraft.io/sigit"><img src="https://snapcraft.io/sigit/badge.svg" alt="sigit"></a>
  <a href="https://sigit.si/code"><img src="https://img.shields.io/badge/sigit.si%2Fcode-235843?style=flat-square&labelColor=17211D" alt="Website"></a>
  <a href="https://github.com/getsigit/sigit/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-235843?style=flat-square&labelColor=17211D" alt="License"></a>
</p>

<p align="center">
  <a href="https://www.nuget.org/packages/SiGit.Code"><img src="https://img.shields.io/nuget/v/SiGit.Code?style=flat-square&labelColor=17211D&color=235843&label=nuget" alt="NuGet"></a>
  <a href="https://github.com/microsoft/winget-pkgs/tree/master/manifests/g/getSigit/siGitCode"><img src="https://img.shields.io/winget/v/getSigit.siGitCode?style=flat-square&labelColor=17211D&color=235843" alt="winget"></a>
  <a href="https://marketplace.visualstudio.com/items?itemName=getsigit.sigit-code"><img src="https://img.shields.io/badge/VS%20Code-Marketplace-235843?style=flat-square&labelColor=17211D" alt="VS Code Marketplace"></a>
  <a href="https://pypi.org/project/sigit-code/"><img src="https://img.shields.io/pypi/dm/sigit-code?style=flat-square&labelColor=17211D&color=235843&label=pypi%20downloads" alt="PyPI downloads"></a>
  <a href="https://github.com/getsigit/sigit/releases"><img src="https://img.shields.io/github/v/release/getsigit/sigit?style=flat-square&labelColor=17211D&color=235843" alt="Release"></a>
  <a href="https://github.com/getsigit/sigit/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/getsigit/sigit/ci.yml?style=flat-square&labelColor=17211D&label=ci" alt="CI"></a>
  <a href="https://github.com/getsigit/sigit/releases"><img src="https://img.shields.io/github/downloads/getsigit/sigit/total?style=flat-square&labelColor=17211D&color=235843" alt="GitHub downloads"></a>
</p>

siGit Code runs on your machine and works in any codebase. Inference runs on-device by default through [Onde Inference](https://ondeinference.com), or on siGit Code Cloud when you choose it. You need no API key to start, and it works with any editor that speaks the [Agent Client Protocol](https://agentclientprotocol.com). The agent is Apache-2.0. siGit Code Cloud and [siGit Factory](https://factory.sigit.si/) are the hosted, paid side.

## Install

```sh
cargo install sigit
brew tap getsigit/tap && brew trust --tap getsigit/tap && brew install sigit
npm install -g @getsigit/sigit
```

More ways, including pip, NuGet, winget, Scoop and Linux packages, are in [docs/install.md](docs/install.md).

## Get siGit anywhere

The command-line agent and the desktop and mobile app are separate products. Each cell links to where it ships.

| Desktop and mobile apps | macOS | Windows | Linux | iOS | Android |
|---|:-:|:-:|:-:|:-:|:-:|
| [App Store](https://apps.apple.com/app/id6753018849) | [✓](https://apps.apple.com/app/id6753018849) | | | [✓](https://apps.apple.com/app/id6753018849) | |
| [Google Play](https://play.google.com/store/apps/details?id=xyz.smbcloud.SiGit) | | | | | [✓](https://play.google.com/store/apps/details?id=xyz.smbcloud.SiGit) |
| [Microsoft Store](https://apps.microsoft.com/detail/9p69ph1k7nxx) | | [✓](https://apps.microsoft.com/detail/9p69ph1k7nxx) | | | |
| [Snap Store](https://snapcraft.io/sigit) | | | [✓](https://snapcraft.io/sigit) | | |
| [Direct download](https://github.com/getsigit/sigit-app/releases) | [✓](https://github.com/getsigit/sigit-app/releases) | [✓](https://github.com/getsigit/sigit-app/releases) | [✓](https://github.com/getsigit/sigit-app/releases) | | |

| Command line | macOS | Windows | Linux |
|---|:-:|:-:|:-:|
| [crates.io](https://crates.io/crates/sigit) | ✓ | ✓ | ✓ |
| [Homebrew](docs/install.md) | ✓ | | |
| [npm](https://www.npmjs.com/package/@getsigit/sigit) | ✓ | ✓ | ✓ |
| [PyPI and uv](https://pypi.org/project/sigit-code/) | ✓ | ✓ | ✓ |
| [NuGet](https://www.nuget.org/packages/SiGit.Code) | ✓ | ✓ | ✓ |
| [winget](docs/install.md) | | ✓ | |
| [Scoop](https://github.com/getsigit/scoop-bucket) | | ✓ | |
| [deb and rpm](https://github.com/getsigit/sigit/releases) | | | ✓ |

`sudo snap install sigit` installs the desktop app, not the terminal agent. The app currently uses siGit Code Cloud for inference, while the command line runs models on-device. Details are in [docs/distribution.md](docs/distribution.md).

Every channel, kept current: https://getsigit.5mb.app/

## Use it your way

- **In your editor.** Any ACP client starts `sigit --acp`. See [ACP support](docs/acp.md).
- **In the terminal.** Run `sigit` for a full-screen chat UI. See [terminal mode](docs/terminal.md).
- **In scripts and CI.** `sigit run "<prompt>"` gives you JSONL events and exit codes. See [headless mode](docs/headless.md).

Setup guides for Zed, Xcode and VS Code are in [docs/editors.md](docs/editors.md).

## Docs

[Install](docs/install.md), [editors](docs/editors.md), [ACP](docs/acp.md), [terminal](docs/terminal.md), [headless](docs/headless.md), [hooks](docs/hooks.md), [MCP](docs/mcp.md), [telemetry](docs/telemetry.md), [distribution](docs/distribution.md). The index is [docs/README.md](docs/README.md).

## Contributing

Issues and pull requests are welcome. Open one against `development`.

## License

[Apache 2.0](https://github.com/getsigit/sigit/blob/main/LICENSE)

## Copyright

© 2026 [siGit Code & Deploy](https://sigit.si).
