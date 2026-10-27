# ACP support

Zed or another ACP-compatible editor starts siGit Code over stdio with the `--acp` argument.
Every ACP client must pass `--acp`. It takes no other arguments, and it is what
tells the binary to speak the Agent Client Protocol instead of opening the
terminal UI. See [editor setup](editors.md) for client configuration.

| Platform | ACP mode | Terminal mode |
|----------|----------|---------------|
| macOS | ✓ | ✓ |
| Linux | ✓ | ✓ |
| Windows | ✓ | not yet |

siGit Code speaks [Agent Client Protocol](https://agentclientprotocol.com) v1 over stdio.
As of 1.7.0 it implements the whole stable v1 surface an agent can offer:

| Area | What is supported |
|------|-------------------|
| Sessions | `session/new`, `load`, `resume`, `fork`, `list` (paged, with a `cwd` filter), `close`, `delete`, and multi-root projects through `additionalDirectories` |
| Prompts | Text, images, audio, embedded resources and resource links. Stop reasons `end_turn`, `max_tokens`, `max_turn_requests`, `refusal` and `cancelled` |
| Updates | Message and thought chunks with a `messageId`, tool calls with diffs, locations and embedded terminals, plans, `usage_update`, `session_info_update`, slash commands |
| Cancellation | `session/cancel`, and `$/cancel_request` for a prompt or a sign-in |
| Permissions | `session/request_permission` with allow or deny, once or for the session. Manual, Auto and Plan as config options and as session modes |
| Client features | `fs/read_text_file` and `fs/write_text_file`, `terminal/*`, URL-mode elicitation and boolean config options, each used only when the client advertises it |
| MCP | stdio and Streamable HTTP servers passed in `mcpServers` |
| Auth | `authenticate` (browser sign-in) and `logout` |

`session/fork` is still marked unstable in the protocol. Draft v2 is not enabled.
