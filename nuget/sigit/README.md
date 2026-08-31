# siGit Code for .NET

`sigit` is the command-line interface for [siGit Code](https://sigit.si), an AI coding agent that
runs on your machine.

## Install

```sh
dotnet tool install --global SiGit.Code
```

## Update

```sh
dotnet tool update --global SiGit.Code
```

## Run

```sh
sigit
```

In a terminal that opens the chat UI. When stdin is a pipe, the same binary speaks the Agent
Client Protocol (ACP) over stdio instead, which is how editors such as Zed and VS Code drive it.

First run downloads a GGUF model, so expect a wait of a gigabyte or two before the first reply.
On macOS the model cache is shared with the siGit Code desktop app, so a model either app has
already fetched is reused.

## Platform support

This .NET tool bundles native `sigit` binaries for:

- macOS `arm64`, `x64`
- Linux `arm64`, `x64` (glibc)
- Windows `arm64`, `x64`

The terminal chat UI is Unix-only. On Windows the binary runs in ACP mode, so use it through an
editor rather than directly.

Because all six binaries ship in one package, the install is large. If that matters, the Homebrew,
npm, and PyPI packages each download only the binary for your platform.

## Other installation methods

- Cargo: `cargo install sigit`
- npm: `npm install -g @getsigit/sigit`
- pip: `pip install sigit-code`
- Homebrew: `brew tap getsigit/tap && brew trust --tap getsigit/tap && brew install sigit`
- GitHub Releases: <https://github.com/getsigit/sigit/releases>

## Source

- Repository: <https://github.com/getsigit/sigit>
- Website: <https://sigit.si>
- Issues: <https://github.com/getsigit/sigit/issues>

## License

[Apache 2.0](https://github.com/getsigit/sigit/blob/main/LICENSE)

## Copyright

© 2026 PT Sigit Mitra Bangun ([siGit Code & Deploy](https://sigit.si)).
