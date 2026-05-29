---
name: sigit-code-release
description: Prepare and verify siGit Code releases. Use when bumping versions, checking Cargo, npm, or PyPI package metadata, validating release notes, confirming CI readiness, or assembling a release checklist for a new siGit Code version.
---

# siGit Code Release

Use this skill when preparing a release for this repository.

## Focus areas

- Keep the branded product name as `siGit Code` in prose and `sigit` for commands, crate names, repo paths, and package identifiers.
- Verify version consistency across release surfaces such as `Cargo.toml`, lockfiles if applicable, npm packaging files, Python packaging files, and user-facing install docs.
- Check release-facing docs such as `README.md`, `CHANGELOG.md`, workflow notes, and package metadata for stale version references or incorrect naming.
- Validate the local release path pragmatically: formatting, linting, targeted tests, and any repo-specific release checks that matter for the requested version.

## Working approach

1. Read the files that define the release version and distribution metadata before proposing any changes.
2. Compare version strings across Rust, npm, Python, and docs instead of assuming they stay in sync automatically.
3. Prefer targeted verification commands that match CI or packaging workflows already present in the repository.
4. Report blockers clearly: failed checks, version mismatches, missing changelog entries, or unpublished packaging changes.

## Typical files to inspect

- `Cargo.toml`
- `Cargo.lock`
- `CHANGELOG.md`
- `README.md`
- `npm/`
- `pypi/`
- `.github/workflows/`

## Release checklist

- Version bump is applied everywhere it needs to be.
- Release notes or changelog entries match the actual changes.
- CI-equivalent local checks pass for the relevant platform or target.
- Package names, install commands, and branding stay consistent.
- Any known release limitations are called out explicitly.
