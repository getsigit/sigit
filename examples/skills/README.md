# Example Agent Skills

siGit Code supports the open [Agent Skills](https://agentskills.io) format. A
skill is a folder containing a `SKILL.md` file — YAML frontmatter (`name` and
`description`, at minimum) followed by Markdown instructions. Skills can bundle
`scripts/`, `references/`, and `assets/` that the agent reads on demand.

## Installing a skill

Copy a skill folder into one of the directories siGit scans (in priority order):

- `.sigit/skills/` or `.claude/skills/` in your project (project-local)
- `~/.config/sigit/skills/` (honours `$SIGIT_CONFIG_DIR`)
- `~/.claude/skills/` (shared with the broader ecosystem)

For example, to install the `commit-message` skill here for the current project:

```sh
mkdir -p .sigit/skills
cp -R examples/skills/commit-message .sigit/skills/
```

The folder name must match the skill's `name` field.

## How siGit uses them

siGit follows the spec's *progressive disclosure*:

1. **Discovery** — at the start of each turn, siGit loads only each skill's
   `name` and `description` into the `skill` tool's description.
2. **Activation** — when your task matches a skill, the agent calls the `skill`
   tool with that name, which loads the full `SKILL.md` into context.
3. **Execution** — the agent follows the instructions, reading any bundled files
   from the skill's directory with its normal file and command tools.

Run `/skills` to list the skills siGit can see.
