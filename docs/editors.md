# Editor setup

siGit Code runs inside any [ACP](acp.md) editor. Every client must pass `--acp`. Guides for Zed, Xcode, VS Code, the JetBrains IDEs and Android Studio follow, and any other client is covered at the end.

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

The JetBrains IDEs run ACP agents through AI Assistant. We tested siGit Code in RustRover and
RubyMine. In the **AI Chat** window, choose **Add Custom Agent**. That opens
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

## Android Studio

Android Studio can connect ACP agents through Bring Your Own Agent (BYOA). It is a preview in the
Canary channel, starting with Android Studio Rabbit 2, so you need the latest Canary build. See the
Android Developers Blog post
[Build your way: Use any AI agent of your choice in Android Studio](https://android-developers.googleblog.com/2026/09/build-your-way-use-any-ai-agent-in-android-studio.html).

siGit Code is in the agent registry, which you find under **Settings > Tools > AI > Agents**
(**Android Studio > Settings** on macOS). Install **siGit Code** there and sign in. The registry
entry launches siGit through `npx`. We tested siGit Code in Android Studio this way. The
`~/.jetbrains/acp.json` file from the JetBrains section is not part of this setup.

## Other editors

Any other client that speaks ACP works the same way: run `sigit` with `--acp`. The protocol site
keeps a list of [ACP clients](https://agentclientprotocol.com/get-started/clients), including
Emacs, Neovim, Sublime Text, [marimo](https://github.com/marimo-team/marimo) and VS Code compatible IDEs such as Cursor and Windsurf.
