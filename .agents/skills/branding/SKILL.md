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

## Primary rule

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

Do not shorten the product name to just `siGit` in documentation or marketing copy unless there is a very specific reason and the surrounding text makes it unmistakable.

---

## Use lowercase names for commands and packages

Use lowercase `sigit` when you mean the executable, crate, repo slug, or package name.

Examples:

- Run `sigit` in a terminal.
- Install with `cargo install sigit`.
- Install with `npm install -g @smbcloud/sigit`.
- Install with `pip install sigit-code`.
- The repository is `getsigit/sigit`.

This distinction matters:

- **Brand / product:** `siGit Code`
- **Command / package / repo:** `sigit`

---

## Recommended wording patterns

### Product description

Prefer:

- `siGit Code is a local coding agent.`
- `siGit Code runs on your machine.`
- `siGit Code works with any codebase.`

Avoid:

- `siGit is a local coding agent.`
- `sigit is a coding assistant.`
- `The siGit product...`

### Install instructions

Prefer:

- `Install siGit Code with Cargo:`
- `Install siGit Code from npm:`
- To start siGit Code, run `sigit`.

That keeps the brand name in prose and the command name in code.

### Editor setup

In UI-facing examples, keep the displayed agent name as `siGit Code`.

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

The visible name is `siGit Code`. The executable path is `sigit`.

---

## Other names that should stay consistent

### smbCloud

Always write `smbCloud` with a lowercase `smb` and uppercase `C`.

Correct:

- `smbCloud`

Wrong:

- `SMBCloud`
- `SmbCloud`
- `smbcloud`

### Onde Inference

Use `Onde Inference` when referring to the product or project.

Use `onde` when referring to the Rust crate.

Examples:

- `siGit Code uses Onde Inference as its local LLM backend.`
- The Rust dependency is `onde`.

### ACP

Use `ACP` for the protocol acronym.

Preferred long form:

- `Agent Client Protocol (ACP)` on first mention when useful

---

## Copy checklist

Before you finish any docs or release-note edit, check these quickly:

1. Did you use `siGit Code` for the product name?
2. Did you keep `sigit` lowercase for commands and package names?
3. Did you keep `smbCloud` cased correctly?
4. Did you keep `Onde Inference` cased correctly in prose?
5. In setup examples, does the visible editor label say `siGit Code` while the command stays `sigit`?

---

## Fast replacements

Common fixes:

- `siGit is` -> `siGit Code is`
- `siGit works` -> `siGit Code works`
- `siGit knows` -> `siGit Code knows`
- `On macOS, siGit` -> `On macOS, siGit Code`
- `Sigit` -> usually `siGit Code` or `sigit`, depending on context

When in doubt, ask:

> Am I talking about the branded product, or the literal command/package name?

If it is the product, use `siGit Code`.
If it is something users type or install, use `sigit`.
