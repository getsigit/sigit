# Editor setup

siGit Code runs inside any [ACP](acp.md) editor. Every client must pass `--acp`. Guides for Zed, Xcode, VS Code and the JetBrains IDEs follow, and any other client is covered at the end.

## Zed setup

Add this to `~/.config/zed/settings.json`:

```json
{
  "agent_servers": {
    "siGit Code": {
      "type": "custom",
      "command": "/absolute/path/to/sigit",
      "args": ["--acp"]
    }
  }
}
```

Use the full absolute path. `~` does not expand here.

## Xcode custom agent

In Xcode, open **Settings > Intelligence > Agents**, add a custom agent, and set:

- **Executable:** the absolute path to `sigit`
- **Arguments:** `--acp`

In Xcode the `--acp` mode also loads the selected on-device model on the first
prompt, so you do not need to send `/load` from the Xcode chat.

To let siGit use Xcode's build, test, and project tools, enable **Allow external
agents to use Xcode tools** in Xcode's Intelligence settings, keep the project
open, and add this to `~/.config/sigit/mcp.toml`:

```toml
[[server]]
name = "xcode"
command = "xcrun"
args = ["mcpbridge"]
```

## VS Code

### With siGit Code Extension

Install from the [Visual Studio Code Marketplace](https://marketplace.visualstudio.com/items?itemName=getsigit.sigit-code).

```jsonc
{
  "sigit.agents": {
    "sigit": {
      "name": "siGit (on-device)",
      "command": "sigit",
      "args": ["--acp"],
      "env": {}
    },
  },
  "sigit.agent.default": "sigit"
}
```

### With ACP Client

Install [ACP Client](https://marketplace.visualstudio.com/items?itemName=formulahendry.acp-client), then add:

```json
{
  "acp.agents": {
    "siGit Code": {
      "command": "sigit",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

## JetBrains IDEs

RustRover, RubyMine, IntelliJ IDEA, Android Studio and the other JetBrains IDEs run ACP agents
through AI Assistant. In the **AI Chat** window, choose **Add Custom Agent**. That opens
`~/.jetbrains/acp.json`. Add:

```json
{
  "agent_servers": {
    "siGit Code": {
      "command": "/absolute/path/to/sigit",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

Use the full path to `sigit` as `command`. See JetBrains' [ACP documentation](https://www.jetbrains.com/help/ai-assistant/acp.html).

## Other editors

Any other client that speaks ACP works the same way: run `sigit` with `--acp`. The protocol site
keeps a list of [ACP clients](https://agentclientprotocol.com/get-started/clients), including
Emacs, Neovim, Sublime Text and VS Code compatible IDEs such as Cursor and Windsurf.
