# Distribution

There are two products under the siGit Code name, and they ship through different channels.

- **siGit Code** is the command-line agent in this repository. It runs in a terminal, in CI, or
  inside any ACP editor.
- **siGit Code & Deploy** is the desktop and mobile app. It embeds this same siGit Code as an
  in-process agent and adds a Git client, a code editor and deploy to smbCloud around it.

Every channel is listed, and kept current, at <https://getsigit.5mb.app/>.

## Command line

| Channel | Package | Platforms |
|---------|---------|-----------|
| [crates.io](https://crates.io/crates/sigit) | `sigit` | macOS, Linux, Windows |
| Homebrew | `getsigit/tap`, formula `sigit` | macOS |
| [npm](https://www.npmjs.com/package/@getsigit/sigit) | `@getsigit/sigit` | macOS, Linux, Windows |
| [PyPI](https://pypi.org/project/sigit-code/) | `sigit-code` | macOS, Linux, Windows |
| NuGet | `SiGit.Code` | macOS, Linux, Windows |
| winget | `getSigit.siGitCode` | Windows |
| Scoop | bucket `getsigit/scoop-bucket`, package `sigit` | Windows |
| `.deb` and `.rpm` | [GitHub releases](https://github.com/getsigit/sigit/releases) | Linux (arm64, x64) |

Each release is cut from this repository.

## Apps

| Store | Platforms | Link |
|-------|-----------|------|
| App Store | iOS, iPadOS, macOS | [siGit Code & Deploy](https://apps.apple.com/app/id6753018849) |
| Google Play | Android | [siGit Code & Deploy](https://play.google.com/store/apps/details?id=xyz.smbcloud.SiGit) |
| Microsoft Store | Windows | [siGit Code & Deploy](https://apps.microsoft.com/detail/9p69ph1k7nxx) |
| Snap Store | Linux (amd64) | [`sudo snap install sigit`](https://snapcraft.io/sigit) |
| Direct download | macOS, Windows, Linux | [siGit Code download page](https://getsigit.5mb.app/download) |

App versions are numbered separately from the CLI.

### The snap is the desktop app

`sudo snap install sigit` installs the desktop app, not the terminal agent. If you want the
`sigit` command, use one of the command-line channels above.

The snap uses strict confinement, so the agent inside it has no shell tool. It reads and edits
files in the folder you open and answers through siGit Code Cloud.

## Inference

The CLI runs models on-device through the [Onde Inference on-device inference SDK](https://ondeinference.com/sdk) by default, and
can use siGit Code Cloud when you choose it. The desktop and mobile apps currently use siGit Code
Cloud.
