# Install

siGit Code is a single binary. Pick the channel you already use.

```sh
cargo install sigit
```

| Method | Command |
|--------|---------|
| Homebrew | `brew tap getsigit/tap && brew trust --tap getsigit/tap && brew install sigit` |
| pip | `pip install sigit-code` |
| uv | `uvx --from sigit-code sigit` |
| npm | `npm install -g @getsigit/sigit` |
| NuGet | `dotnet tool install --global SiGit.Code` |
| winget | `winget install getSigit.siGitCode` |
| Scoop | `scoop bucket add getsigit https://github.com/getsigit/scoop-bucket && scoop install sigit` |
| Debian/Ubuntu | download the `.deb` from [releases](https://github.com/getsigit/sigit/releases), then `sudo dpkg -i sigit_*.deb` |
| Fedora/RHEL | download the `.rpm` from [releases](https://github.com/getsigit/sigit/releases), then `sudo rpm -i sigit-*.rpm` |

See [distribution](distribution.md) for how each channel is fed, and for the desktop and mobile
apps, which are a separate product.

## First run

The first launch downloads a GGUF model from Hugging Face. Expect roughly 1 to 2 GB, depending on the model. After that, loads come from disk and are much faster.

On macOS, siGit Code shares its model cache with the desktop app through an App Group container. If the desktop app already downloaded the model, the CLI reuses it.

## Platform support

| Platform | Architecture |
|----------|-------------|
| macOS | arm64, x64 |
| Linux (glibc) | arm64, x64 |
| Windows | arm64, x64 |
