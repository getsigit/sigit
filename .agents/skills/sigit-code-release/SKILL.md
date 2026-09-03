---
name: sigit-code-release
description: Prepare and verify siGit Code releases. Use when bumping versions, checking Cargo, npm, or PyPI package metadata, validating release notes, confirming CI readiness, or assembling a release checklist for a new siGit Code version.
---

# siGit Code Release

Use this skill when preparing a release for this repository.

## Focus areas

- Keep the branded product name as `siGit Code` in prose and `sigit` for commands, crate names, repo paths, and package identifiers.
- Verify version consistency across release surfaces, but respect which files are real sources of truth versus generated or release-time rewritten artifacts.
- Check release-facing docs such as `README.md`, `CHANGELOG.md`, workflow notes, and package metadata for stale version references or incorrect naming.
- Validate the local release path pragmatically: formatting, linting, targeted tests, and any repo-specific release checks that matter for the requested version.

## Working approach

1. Read the files that define the release version and distribution metadata before proposing any changes.
2. Treat `Cargo.toml` as the primary release version source for the Rust crate and the PyPI package in this repository.
3. Check how the release workflows rewrite or derive versions before editing checked-in npm or Python metadata.
4. Prefer targeted verification commands that match CI or packaging workflows already present in the repository.
5. Report blockers clearly: failed checks, version mismatches, missing changelog entries, or unpublished packaging changes.

## Repo-specific release rules

- Bump the crate version in `Cargo.toml`.
- Update the root `sigit` package entry in `Cargo.lock` when the crate version changes.
- Add or update the top changelog entry in `CHANGELOG.md` for the release being cut.
- Do not treat `npm/sigit/package.json` `0.0.0-dev` as a bug by default. The npm release workflow rewrites it at publish time using `npm/scripts/render-main-package.cjs` and the release tag.
- Do not add a hardcoded version to `pypi/pyproject.toml` for normal releases. PyPI uses `maturin` with `dynamic = ["version"]` and derives the published package version from `Cargo.toml`.
- Release workflows are tag-driven. `release-github.yml`, `release-npm.yml`, `release-pypi.yml`, `release-crates.yml`, `release-homebrew.yml`, `release-nuget.yml`, and `release-mcp-registry.yml` all derive `RELEASE_VERSION` from a `v*.*.*` tag or a manually supplied tag input.
- The crate is published to crates.io (`release-crates.yml`) and the Homebrew tap is updated (`release-homebrew.yml`) as part of the tag-driven flow. Per the siGit release flow, Homebrew is auto-triggered — do not dispatch it manually.
- `release-github.yml` builds the binaries, attaches them (plus a `.deb`/`.rpm` per Linux target built with nfpm from `packaging/nfpm.yaml`) to the GitHub release, then dispatches `release-homebrew`, `release-scoop`, `release-winget`, and `release-aur`. A dispatch failure in any one of those four is logged as a warning, not a hard failure — an unconfigured channel doesn't block the rest of the release. `release-nuget.yml` and `release-mcp-registry.yml`, by contrast, are tag-triggered directly like the other language-registry workflows, not dispatched from `release-github.yml`.
- Every release asset carries a `.sha256` sidecar (not just the macOS Homebrew tarball). Scoop, winget, and the AUR PKGBUILD each consume the raw binaries plus their sidecar checksum, rather than the tarball.
- npm and NuGet publish via OIDC trusted publishing — neither has a stored token in the repo. On npm this is configured per package (each `@getsigit/*` name has a trusted publisher pointing at `release-npm.yml`), so a new package needs the same setup before its first publish will work. OIDC requires npm >= 11.5.1 and Node >= 22.14.0.
- The NuGet package (`SiGit.Code`, `dotnet tool install --global SiGit.Code`) bundles all six platform binaries under `native/<os>-<arch>/` behind a managed shim, rather than shipping one package per platform like npm/PyPI. Watch the nuget.org 250 MB package-size limit if it ever grows.
- The baked-in official MCP server is separately listed in the public MCP Registry as `si.sigit/sigit` (`release-mcp-registry.yml`, driven by `server.json` at the repo root). It's a remote Streamable-HTTP listing, so it's verified by a DNS TXT record on the `sigit.si` domain rather than the GitHub-OIDC scheme used for package listings.

## Git release flow

Releases are cut from `development` and shipped on `main`. The published tags (`v1.2.0`, `v1.2.1`) point at the `main`-side merge commit, never at a release branch or at `development`. Follow this order:

1. Branch `release/v<version>` off `development`.
2. Apply the version bump and changelog edits, then commit (e.g. `Release v<version>`).
3. Merge `release/v<version>` back into `development`.
4. Merge `development` into `main` (commit message: `Merge development into main for v<version> release`).
5. Tag `v<version>` on that `main` merge commit, then push `main` and the tag.

Step 4 is the only way commits are meant to reach `main`. A feature or fix PR targeting `main` breaks that: it puts work on the release branch that `development` does not have, so the next release is cut without it. Such PRs belong on `development` (see the pull request target rule in `AGENTS.md`).

Pushing the `v*.*.*` tag is what fires every release workflow, so create and push it only after the merge into `main` has landed. Do not commit, tag, or push until the user explicitly asks — confirm the version and that they want the release to go out first.

## Typical files to inspect

- `Cargo.toml`
- `Cargo.lock`
- `CHANGELOG.md`
- `README.md`
- `npm/sigit/package.json`
- `npm/scripts/render-main-package.cjs`
- `npm/`
- `pypi/`
- `nuget/sigit/` (bundled-binary `.NET` tool packaging)
- `packaging/` (`nfpm.yaml` for `.deb`/`.rpm`, plus `aur/` and `winget/` templates)
- `server.json` (MCP Registry listing)
- `.github/workflows/` (`release-github.yml`, `release-npm.yml`, `release-pypi.yml`, `release-crates.yml`, `release-homebrew.yml`, `release-scoop.yml`, `release-winget.yml`, `release-aur.yml`, `release-nuget.yml`, `release-mcp-registry.yml`)

## Release checklist

- Version bump is applied everywhere it needs to be.
- `Cargo.toml` and the root crate entry in `Cargo.lock` match.
- `CHANGELOG.md` has a correct top entry for the new release.
- `npm/sigit/package.json` is left at `0.0.0-dev` unless the packaging flow itself changed.
- `pypi/pyproject.toml` still uses dynamic versioning unless there is a deliberate packaging change.
- Release workflows still derive their version from the tag as expected.
- Git flow followed: bump committed on `release/v<version>`, merged back to `development`, then `development` merged into `main`, with `v<version>` tagged on the `main` merge commit.
- Release notes or changelog entries match the actual changes.
- CI-equivalent local checks pass for the relevant platform or target.
- Package names, install commands, and branding stay consistent across npm, PyPI, crates.io, Homebrew, NuGet, Scoop, winget, AUR, and the `.deb`/`.rpm` packages.
- Every release asset (including the new `.deb`/`.rpm`/raw binaries consumed by Scoop, winget, and AUR) has a matching `.sha256` sidecar.
- Any known release limitations are called out explicitly, including a dispatch failure in `release-scoop`/`release-winget`/`release-aur` (non-fatal — logged as a warning by `release-github.yml`) versus a hard failure in a directly tag-triggered workflow like `release-nuget` or `release-mcp-registry`.
