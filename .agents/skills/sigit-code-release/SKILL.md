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
2. Treat `[workspace.package].version` in the root `Cargo.toml` as the primary release version source for the Rust crate and the PyPI package in this repository.
3. Check how the release workflows rewrite or derive versions before editing checked-in npm or Python metadata.
4. Prefer targeted verification commands that match CI or packaging workflows already present in the repository.
5. Report blockers clearly: failed checks, version mismatches, missing changelog entries, or unpublished packaging changes.

## Repo-specific release rules

- Bump `[workspace.package].version` in the root `Cargo.toml`; every workspace crate inherits it.
- Update the root `sigit` package entry in `Cargo.lock` when the crate version changes.
- Add or update the top changelog entry in `CHANGELOG.md` for the release being cut.
- Do not treat `npm/sigit/package.json` `0.0.0-dev` as a bug by default. The npm release workflow rewrites it at publish time using `npm/scripts/render-main-package.cjs` and the release tag.
- Every npm package publishes twice, under `@getsigit` and under `@smbcloud`. `@smbcloud` is the pre-1.5.5 name, mirrored so an install predating the rename keeps updating; `@getsigit` is canonical and is what the README, the Homebrew tap, and the ACP registry entry point at. All fourteen names (two base packages, twelve platform packages) publish over OIDC and each needs its own Trusted Publisher on npm pointing at `release-npm.yml`.
- The scope is a render-time argument, not a second copy of the packaging: `render-platform-package.cjs` takes it as a fifth argument, and the base package picks it up from `package-main.json.tmpl` versus `package-compat.json.tmpl`. `npm/sigit/src/index.ts` is built once and shipped under both names — it reads its own `package.json` at runtime to decide which scope to resolve the platform binary from, and prints the migration notice only when it finds itself running as `@smbcloud/sigit`.
- Do not add a hardcoded version to `pypi/pyproject.toml` for normal releases. PyPI uses `maturin` with `dynamic = ["version"]` and derives the published package version from `crates/cli/Cargo.toml`, which inherits the workspace version.
- Release workflows are tag-driven. `release-github.yml`, `release-npm.yml`, `release-pypi.yml`, `release-crates.yml`, `release-homebrew.yml`, `release-nuget.yml`, and `release-mcp-registry.yml` all derive `RELEASE_VERSION` from a `v*.*.*` tag or a manually supplied tag input.
- The crate is published to crates.io (`release-crates.yml`) and the Homebrew tap is updated (`release-homebrew.yml`) as part of the tag-driven flow. Per the siGit release flow, Homebrew is auto-triggered — do not dispatch it manually.
- `release-github.yml` builds the binaries, attaches them (plus a `.deb`/`.rpm` per Linux target built with nfpm from `packaging/nfpm.yaml`) to the GitHub release, then dispatches `release-homebrew`, `release-scoop`, `release-winget`, and `release-aur`. A dispatch failure in any one of those four is logged as a warning, not a hard failure — an unconfigured channel doesn't block the rest of the release. `release-nuget.yml` and `release-mcp-registry.yml`, by contrast, are tag-triggered directly like the other language-registry workflows, not dispatched from `release-github.yml`.
- Every release asset carries a `.sha256` sidecar (not just the macOS Homebrew tarball). Scoop, winget, and the AUR PKGBUILD each consume the raw binaries plus their sidecar checksum, rather than the tarball.
- The GitHub release body is generated, not hand-written: `release-github.yml`'s release job checks out the tag, awk-extracts the `## <version>` section from `CHANGELOG.md` (a missing section is a hard failure), appends a full-changelog link and a `sigit.si/code` footnote, and publishes it via `body_path: release_notes.md`. The only file a release-prep change needs to touch is `CHANGELOG.md` — never edit release notes in the GitHub UI, because the next tag push would overwrite them.
- npm and NuGet publish via OIDC trusted publishing — neither has a stored token in the repo. On npm this is configured per package (each `@getsigit/*` name has a trusted publisher pointing at `release-npm.yml`), so a new package needs the same setup before its first publish will work. OIDC requires npm >= 11.5.1 and Node >= 22.14.0.
- The NuGet package (`SiGit.Code`, `dotnet tool install --global SiGit.Code`) bundles all six platform binaries under `native/<os>-<arch>/` behind a managed shim, rather than shipping one package per platform like npm/PyPI. Watch the nuget.org 250 MB package-size limit if it ever grows.
- The winget channel (`release-winget.yml`) files a Package Request issue first and then opens the manifest PR that resolves it, authored by **two separate classic PATs** (not fine-grained) — `WINGET_ISSUE_TOKEN` for the issue and `WINGET_PR_TOKEN` for the PR, replacing the old single `WINGET_TOKEN`. `WINGET_ISSUE_TOKEN` needs `public_repo`. `WINGET_PR_TOKEN` needs `public_repo` **plus `read:org` and `read:discussion`**: wingetcreate forks `microsoft/winget-pkgs` and opens the PR (fork needs `public_repo`; a fine-grained PAT scoped to `getsigit/sigit` can't fork an outside repo and 403/404s), and the "Set the winget PR body" step then runs `gh pr edit`, whose GraphQL query resolves org/team and discussion fields and fails with a scope error on a `public_repo`-only token (CI run 38262301933). The PR token is passed as `WINGET_CREATE_GITHUB_TOKEN` (not `--token`) so it doesn't leak into logs. `wingetcreate update` only works on a package that already exists in `microsoft/winget-pkgs`, so the workflow checks `manifests/g/getSigit/siGitCode` first and falls back to `wingetcreate submit` with `packaging/winget/*.yaml.in` for the first release; every later release takes the `update` path. If a winget release fails on the fork step, check the token type; if it fails on the `gh pr edit` step with a scope error, check `WINGET_PR_TOKEN`'s scopes before re-dispatching.
- The baked-in official MCP server is separately listed in the public MCP Registry as `si.sigit/sigit` (`release-mcp-registry.yml`, driven by `server.json` at the repo root). It's a remote Streamable-HTTP listing, so it's verified by a DNS TXT record on the `sigit.si` domain rather than the GitHub-OIDC scheme used for package listings.

## Git release flow

Releases are shipped on `main`, and the published tags (`v1.2.0`, `v1.2.1`) point at the
`main`-side merge commit, never at a release branch or at `development`. There are two flows,
and picking the wrong one ships the wrong set of commits. Decide first which this is.

### Full release (minor/major — ships everything on `development`)

Cut from `development` when the release is meant to carry all the work that has accumulated there.

1. Branch `release/v<version>` off `development`.
2. Apply the version bump and changelog edits, then commit (e.g. `Release v<version>`).
3. Merge `release/v<version>` back into `development`.
4. Merge `development` into `main` (commit message: `Merge development into main for v<version> release`).
5. Tag `v<version>` on that `main` merge commit, then push `main` and the tag.

For a full release, step 4 is the only way commits are meant to reach `main`. A feature or fix
PR targeting `main` breaks that: it puts work on the release branch that `development` does not
have, so the next release is cut without it. Such PRs belong on `development` (see the pull
request target rule in `AGENTS.md`).

### Scoped patch (hotfix — ships only specific fixes on top of the last tag)

Use this when the patch must ship **only** a named set of fixes on top of the last released tag,
**not** the unreleased work sitting on `development` (e.g. a patch over `v1.6.7` that must carry
just a winget/Windows hotfix while ACP features stay unreleased). The giveaway is wording like
"scoped patch off `v<prev>`" or "hotfix" — if `development` is far ahead, a `development → main`
merge would drag all of it out under a patch tag, which is wrong.

1. Branch `release/v<version>` off the **last released tag** (`v<prev>`), not `development`.
2. Cherry-pick (or commit) only the in-scope fixes, then the version bump + changelog, and commit
   (e.g. `Release v<version>`).
3. Merge `release/v<version>` back into `development` so the bump and changelog carry forward.
4. Merge `release/v<version>` **directly into `main`** (commit message: `Release v<version>`) —
   **never `development → main` for a scoped patch**, since that would pull `development`'s
   unreleased work onto `main`.
5. Tag `v<version>` on that `main` merge commit, then push `main` and the tag.

Before tagging a scoped patch, audit the scope: `git log --oneline v<prev>^{commit}..main` and
`git diff --stat v<prev>^{commit}..main` must show only the intended fixes and the release
metadata files (`Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`), nothing from `development`.

Pushing the `v*.*.*` tag is what fires every release workflow, so create and push it only after the merge into `main` has landed. Do not commit, tag, or push until the user explicitly asks — confirm the version and that they want the release to go out first.

Decide up front which flow you are in and confirm the exact scope with the user. If in doubt about
whether a release ships everything on `development` or only a scoped set of fixes, ask — the two
flows start from different base commits and are not interchangeable.

## Typical files to inspect

- `Cargo.toml`
- `Cargo.lock`
- `CHANGELOG.md`
- `README.md`
- `npm/sigit/package.json`
- `npm/package-main.json.tmpl`, `npm/package-compat.json.tmpl`, `npm/package.json.tmpl` (publish-time package metadata for both scopes)
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
- Correct git flow for the release type followed, with `v<version>` tagged on the `main` merge commit either way:
  - **Full release:** bump committed on `release/v<version>` (off `development`), merged back to `development`, then `development` merged into `main`.
  - **Scoped patch:** `release/v<version>` branched off the last tag `v<prev>`, in-scope fixes + bump committed, merged back to `development`, then `release/v<version>` merged **directly into `main`** (never `development → main`), and the scope audited (`git log --oneline v<prev>^{commit}..main` / `git diff --stat v<prev>^{commit}..main`) to show only the intended fixes plus release metadata.
- Release notes or changelog entries match the actual changes.
- CI-equivalent local checks pass for the relevant platform or target.
- Package names, install commands, and branding stay consistent across npm, PyPI, crates.io, Homebrew, NuGet, Scoop, winget, AUR, and the `.deb`/`.rpm` packages.
- Every release asset (including the new `.deb`/`.rpm`/raw binaries consumed by Scoop, winget, and AUR) has a matching `.sha256` sidecar.
- Any known release limitations are called out explicitly, including a dispatch failure in `release-scoop`/`release-winget`/`release-aur` (non-fatal — logged as a warning by `release-github.yml`) versus a hard failure in a directly tag-triggered workflow like `release-nuget` or `release-mcp-registry`.
- The winget channel uses two classic PATs (not fine-grained): `WINGET_ISSUE_TOKEN` (`public_repo`) for the Package Request issue and `WINGET_PR_TOKEN` (`public_repo` + `read:org` + `read:discussion`) for the manifest PR, so wingetcreate can fork `microsoft/winget-pkgs` and `gh pr edit` can set the PR body. If a winget release fails on the fork step, verify the token type; if it fails on the `gh pr edit` step, verify `WINGET_PR_TOKEN`'s scopes before re-dispatching.
