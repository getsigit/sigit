# The official siGit MCP server

siGit Code Cloud runs a hosted [Model Context Protocol](https://modelcontextprotocol.io)
server at **`https://sigit.si/api/v1/mcp`**. It's a stateless **Streamable-HTTP**
server (JSON-RPC 2.0 over a single POST endpoint) that exposes your
siGit-hosted git repositories — plus web search — as MCP tools, so any MCP
client can browse repos, read files, search code, and open or comment on pull
requests and issues on your behalf.

This page documents the server from a **client's** perspective: the siGit Code
CLI is its reference consumer, but the endpoint is a standard remote MCP server
that works with any compliant client. The server implementation itself is
closed-source (it's part of the hosted service); only this consumer-facing
description is public.

## Tools

| Tool | What it does |
| --- | --- |
| `list_repositories` | List the git repositories the authenticated user can access. Use this first to discover the `owner/name` to pass to other tools. |
| `get_file_contents` | Read a file's contents at a given ref (branch, tag, or commit SHA). |
| `search_code` | Search file contents across a repository; returns matching files with line snippets. |
| `list_issues` | List issues in a repository, optionally filtered by state or a title/body search. |
| `get_issue` | Fetch one issue by number, including its body and comments. |
| `create_issue` | Open a new issue in a repository. |
| `list_pull_requests` | List pull requests in a repository, optionally filtered by state. |
| `get_pull_request` | Fetch one pull request by number. |
| `create_pull_request` | Open a pull request from a head branch into a base branch. |
| `add_issue_comment` | Post a comment on an issue or pull request by its number. |
| `web_search` | Search the public web; returns matching pages as title, URL, and snippet. |

All repository tools take a repo in `owner/name` form and only ever see
repositories the authenticated user may access.

## Authentication

The server is a per-user protected resource. Every `tools/call` requires an
`Authorization: Bearer <token>` header carrying a siGit Code Cloud session
token. Unauthenticated requests get a `401` with a `WWW-Authenticate` header
advertising the OAuth protected-resource metadata, so MCP clients that support
OAuth can discover the sign-in flow automatically.

Being stateless, the server issues no `Mcp-Session-Id` and offers no
server→client SSE stream (a `GET` to the endpoint returns `405`) — a valid,
tool-only MCP transport mode.

## How the siGit Code CLI uses it

The CLI bakes this endpoint in as its **official** MCP server (default
`https://sigit.si/api/v1/mcp`, overridable via `SIGIT_CLOUD_URL`). When you're
signed in with `sigit login`, the CLI sends your cloud session token as the
bearer credential; when you're not, the server simply contributes no tools.
Its tools are namespaced `mcp__sigit__<tool>` inside the agent (see
[`src/mcp.rs`](../src/mcp.rs)). `web_search` is additionally re-exposed as a
first-class read-only tool rather than a raw `mcp__*` call.

A user-defined `mcp.toml` entry named `sigit` overrides the baked-in default,
so you can repoint the CLI at a different deployment without a code change.

## MCP Registry listing

The server is published to the
[official MCP Registry](https://registry.modelcontextprotocol.io) as
**`si.sigit/sigit`**, a remote Streamable-HTTP listing, so registry-aware
clients can add it in one click. The registry only stores metadata; the
`server.json` and publish workflow live in this repo, and namespace ownership is
proven by a DNS TXT record on `sigit.si`. The internal setup and release runbook
live with the server (private `sigit-si` repo).
