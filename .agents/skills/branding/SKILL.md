---
name: branding
description: Keep siGit Code branding, naming, and package references consistent. Use when editing docs, release notes, UI copy, package metadata, setup guides, or any prose that mentions the product, CLI, company, or Onde Inference.
---

# Branding

## Overview

Use this file when writing docs, release notes, UI copy, package metadata, or setup guides for this repository.

The short version:

- **Product / brand name:** `siGit Code`
- **CLI command:** `sigit`
- **Rust crate:** `sigit`
- **npm package:** `@smbcloud/sigit`
- **PyPI package:** `sigit-code`
- **Company name:** `smbCloud`
- **LLM backend name:** `Onde Inference`

The most common mistake is mixing the product name with the command name.


---

# Branding: siGit Code naming and voice

Use this skill when you are writing docs, release notes, UI copy, package descriptions, setup guides, or anything else user-facing in this repo.

The main job is simple: keep the names straight.

## The name map

These names are case-sensitive.

- **Product / brand:** `siGit Code`
- **CLI command:** `sigit`
- **Rust crate:** `sigit`
- **Repository slug:** `getsigit/sigit`
- **npm package:** `@smbcloud/sigit`
- **PyPI package:** `sigit-code`
- **Company:** `smbCloud`
- **LLM backend in prose:** `Onde Inference`
- **Rust crate for the backend:** `onde`
- **Protocol acronym:** `ACP`
- **Long form when needed:** `Agent Client Protocol (ACP)`

## First rule

When you mean the product, write **`siGit Code`** exactly like that.

Correct:

- `siGit Code is a local coding agent.`
- `siGit Code works in Zed over ACP.`
- `siGit Code shares its model cache with the desktop app on macOS.`

Wrong:

- `Sigit Code`
- `SiGit Code`
- `siGit`
- `sigit Code`
- `SIGIT`

Do not shorten the product name to just `siGit` in docs or marketing copy unless there is a very specific reason and the sentence still reads clearly.

## Second rule

When you mean something users type, install, import, or clone, use the literal lowercase name.

That means:

- `sigit` for the command, crate, and repo slug
- `@smbcloud/sigit` for npm
- `sigit-code` for PyPI
- `onde` for the Rust crate

Examples:

- Run `sigit` in a terminal.
- Install with `cargo install sigit`.
- Install with `npm install -g @smbcloud/sigit`.
- Install with `pip install sigit-code`.
- The repository is `getsigit/sigit`.

A good gut-check:

> If this is the thing a user types or installs, keep the literal package or command name.
> If this is the thing you are describing, use the branded product name.

## Preferred wording

Keep the prose plain and direct.

Prefer:

- `siGit Code is a local coding agent.`
- `siGit Code runs on your machine.`
- `siGit Code works with any codebase.`
- `Install siGit Code with Cargo:`
- `To start siGit Code, run `sigit`.`

Avoid:

- `siGit is a local coding agent.`
- `sigit is a coding assistant.`
- `The siGit product...`
- inflated marketing language that makes the copy sound generic

If a sentence feels awkward because of the brand name, rewrite the sentence. Do not change the name.

## Editor setup rules

In UI-facing examples, the visible label should stay `siGit Code`.
The executable should stay `sigit`.

Example:

```/dev/null/branding-example.json#L1-8
{
  "agent_servers": {
    "siGit Code": {
      "type": "custom",
      "command": "/absolute/path/to/sigit"
    }
  }
}
```

The same rule applies in VS Code ACP examples, screenshots, panel labels, and release notes.

## Other names that must stay exact

### smbCloud

Always write `smbCloud` with lowercase `smb` and uppercase `C`.

Wrong:

- `SMBCloud`
- `SmbCloud`
- `smbcloud`

### Onde Inference

Use `Onde Inference` when you mean the product or project.
Use `onde` when you mean the Rust crate.

Examples:

- `siGit Code uses Onde Inference as its local LLM backend.`
- `The Rust dependency is `onde`.`

### ACP

Use `ACP` for the acronym.
Use `Agent Client Protocol (ACP)` on first mention when the long form helps.

## Humanizing without breaking branding

If you are also cleaning up AI-ish writing, preserve every case-sensitive name exactly as written.

That includes:

- `siGit Code`
- `smbCloud`
- `Onde Inference`
- `ACP`
- `sigit`
- `@smbcloud/sigit`
- `sigit-code`
- `onde`

Do not "smooth out" a brand name. Do not re-case package names to make a sentence look nicer. Rewrite around them.

## Quick checklist

Before you finish any doc or release-note edit, check these:

1. Did you use `siGit Code` when referring to the product?
2. Did you keep `sigit` lowercase for commands, crates, and repo references?
3. Did you keep `@smbcloud/sigit` and `sigit-code` exact?
4. Did you keep `smbCloud` and `Onde Inference` cased correctly?
5. In setup examples, does the visible editor label say `siGit Code` while the command stays `sigit`?

## Fast replacements

Common fixes:

- `siGit is` -> `siGit Code is`
- `siGit works` -> `siGit Code works`
- `siGit knows` -> `siGit Code knows`
- `On macOS, siGit` -> `On macOS, siGit Code`
- `Sigit` -> usually `siGit Code` or `sigit`, depending on context

When in doubt, ask one question:

> Am I talking about the product, or the literal thing a user types?

If it is the product, use `siGit Code`.
If it is the command or package, use the exact lowercase name.
