---
name: commit-message
description: Write a clear git commit message from staged changes. Use when the user asks to commit, write a commit message, or describe staged changes.
license: Apache-2.0
metadata:
  author: sigit
  version: "1.0"
---

# Commit message

Write a concise, conventional commit message that describes *why* a change was
made, not just what changed.

## Steps

1. Inspect what is staged: run `git diff --cached` (and `git status` for context).
2. Group the changes into a single logical intent. If they span unrelated
   concerns, say so and suggest splitting the commit.
3. Write the message:
   - **Subject line**: imperative mood, lowercase after the type, no trailing
     period, ≤ 50 characters. Prefix with a type when it fits the repo's
     convention (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`).
   - **Body** (optional): wrap at 72 columns. Explain the motivation and any
     non-obvious tradeoffs. Reference issues if relevant.
4. Show the message to the user before committing. Only run `git commit` if they
   confirm.

## Examples

Good subject lines:

```
fix: stop the picker from reloading a working model on /reload
refactor: extract skill discovery into its own module
```

Avoid:

```
Update files
fixed bug
WIP
```
