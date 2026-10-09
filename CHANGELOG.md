# Changelog

## Unreleased

## 1.6.6

### Added

- **A tool call carries its programmatic tool name.** The `tool_call` an editor
  gets for a running tool now includes `name` (`read_file`, `run_command`,
  `mcp__…`) next to the human title, sent on the first report of the call only
  as the protocol asks. A client can tell a command from a file edit without
  parsing the title, and a replayed thread from `session/load` carries the same
  name (#196).

### Changed

- **MCP runs on `rmcp`, the official Rust SDK, through `ed-mcp`.** siGit Code's
  own JSON-RPC client for stdio and Streamable HTTP servers (about 600 lines)
  is gone; `ed-mcp` is the layer every Onde agent shares. `mcp.toml`, the
  baked-in `sigit` and `smbcloud` servers, tool names and `/mcp` are unchanged.
  An HTTP server that stops answering is now reconnected and the call retried
  once for any failure but a timeout, not only on a 404.

### Fixed

- **A reply that degenerates into a repetition loop is cut off.** After a tool
  call, a model could fall into thousands of words from a tiny vocabulary
  (`find: list: check: get: …`) with no tool call and no end, and the junk went
  into history for the next turn. siGit Code now watches a streamed reply for a
  window of 200 words with very few distinct ones (code blocks excepted), stops
  reading, keeps only the text before the loop, tells you it stopped, and asks
  the model once to make its call or answer briefly (#123).
- **A linked selection written as `L10-L20` or `L10` reads just those lines.**
  siGit Code read the line range of a `resource_link` only as `L10:20` or
  `L10-20`, so `L10-L20` (the GitHub style) and a single line `L10` sent the
  whole file instead of the selection. It now reads the same forms Zed's own
  mention parser does, and a range that starts at line 0 or runs backwards
  counts as no range.

## 1.6.5

### Added

- **An `@file` mention can carry unsaved edits.** siGit Code now advertises
  `promptCapabilities.embeddedContext`, so a client can put a resource's
  contents in the prompt instead of sending a link to it. Before, a client
  that follows the spec only sent `resource_link`, and siGit Code read the
  file from disk, missing whatever the user had not saved yet (#141).
- **A prompt can carry audio.** siGit Code now advertises
  `promptCapabilities.audio` and sends an attached clip to the model as an
  OpenAI `input_audio` part. It works the way image attachments do: a model on
  an endpoint you configure yourself gets the clip and decides for itself,
  while siGit Code Cloud tiers and on-device models, none of which take audio
  today, get a note in its place and the editor says the clip was left out
  (#143).
- **The README says what telemetry is sent.** A new Telemetry section covers
  the model-timing events Onde Inference can report for on-device models,
  what they leave out, and how to turn them off with `ONDE_DISABLE_PULSE=1`.

### Changed

- **A turn that cannot get its session back stops.** A turn on an HTTP backend
  gives up the process-wide working directory while it waits and reinstalls
  its session before a tool runs. If the reinstall failed, the turn logged a
  warning and carried on in whichever directory was current. Nothing makes it
  fail today, since a close waits for the turn, but the turn now ends as
  cancelled instead of depending on that.
- **Dependencies match the rest of the Onde stack.** Onde 1.2.2 to 1.3.1,
  reqwest 0.12 to 0.13 and ratatui 0.29 to 0.30, the versions Ed, OndeCode and
  SplitFire Agent use. ratatui is built with only its crossterm backend, which
  drops the second copy of crossterm (0.28) from the build.

### Fixed

- **Tags a model makes up no longer show in the reply.** After a long run of
  tool rounds, a model could start writing markup that looked like it came
  from siGit Code: a `<system_warning>` block telling the user the previous
  turn was injected and should be ignored, or a question wrapped in
  `<Option_Picker>`. Neither tag means anything to siGit Code or the editor,
  so both rendered as raw text. A `system_*` block is now dropped from the
  reply and from the history, and other made-up tags are removed while the
  text inside them stays. Code blocks are left alone. The system prompt also
  tells the model to write plain Markdown and to list choices as plain text
  (#122).
- **Closing a thread that is waiting at a permission prompt no longer hangs.**
  `session/close` cancels the session's running turn and waits for it to end,
  but a turn stopped at a permission prompt only went on when the client
  answered the request. A client that closed the thread without answering
  left the close, and every later request for that thread, waiting for good.
  The same held for `session/cancel` with a client that did not send the
  cancelled outcome the protocol asks for. The permission wait now ends on
  the cancellation as well, and an approval that arrives after the turn was
  cancelled runs nothing.

- **A closed thread no longer leaves its request lock behind.** The process
  kept one lock per session id for as long as it ran, for closed threads and
  for ids no session ever had. The entry now goes with the last request that
  used it.

- **A tool call that did not run no longer shows as completed.** When policy
  or plan mode denied a call, when the user picked Deny at the permission
  prompt, or when the repeat guard skipped a call the model made three times,
  the editor's card still ended `completed`. Those cards now end `failed`, the
  same way a call cancelled at the prompt already did. A call denied at the
  prompt also gets its own title back instead of keeping the permission
  request's (#139).

- **On-device inference no longer panics on some non-English text.** siGit Code
  now builds on Onde 1.3.1, which fixes a panic when a message longer than 100
  bytes had a multi-byte character (an accented letter, CJK, or an emoji) at
  byte 100. Onde 1.3 also reports Pulse events for streaming inference, which
  is the path on-device turns take, so those turns are now reported alongside
  model loads. `ONDE_DISABLE_PULSE=1` turns Pulse off.

## 1.6.4

### Added

- **DeepSeek DSML tool calls written into the reply text are recovered.** A
  DeepSeek-family endpoint can render a tool call as literal
  `<｜DSML｜tool_calls>` text instead of the structured `tool_calls` field,
  which streamed the tag into the editor and ended the turn with nothing run.
  The inline tool-call scanner that already recovered the GLM XML and Kimi K3
  shapes now recovers the DSML shape too, including a marker split across
  stream chunks. A block that fails to parse is still kept out of the reply
  and the history, and the model is told the call did not run (#181).

### Fixed

- **The round-cap stop message no longer carries a stray run of spaces.**
  When the tool-round cap ended a turn, the closing message could collapse a
  space run into the wrong spot, which the editor then rendered verbatim
  (#177).

## 1.6.3

### Added

- **A client can resume a session and close one.** siGit Code now advertises
  `sessionCapabilities.resume` and `sessionCapabilities.close`.
  `session/resume` restores a saved thread the way `session/load` does, but
  sends none of it back: it is for a client that still has the thread on
  screen, after an agent restart for example. `session/close` cancels the
  session's running turn and drops what the process held for it, which is its
  state, its permission grants and plan mode, the MCP servers the client named
  for it, and the background commands it started. The saved history is left
  alone, so a closed thread still lists and reopens (#149).

- **The permission modes are also offered as ACP session modes.** Manual, Auto
  and Plan were only reachable as a config option, which is what the protocol
  now recommends and what Zed reads. A client that draws the older mode
  selector saw nothing. `session/new`, `session/load` and `session/fork` now
  return the same three in `modes`, `session/set_mode` switches between them,
  and a change made any other way (the config option, `/plan`, `/clear`,
  `/reload`) is announced with `current_mode_update`. A client that reads
  config options ignores `modes`, so nothing changes there (#148).

- **An editor can sign out of siGit Code.** The `initialize` reply carried an
  empty `auth` object, so a client drew no sign out button, and typing
  `/logout` was the only way out of an account. The reply now advertises
  `agentCapabilities.auth.logout`, and a `logout` request ends the account
  session through the method `/logout` calls. The request names no session,
  so it moves every open thread that used the account, not only the live one:
  a thread parked on a cloud tier goes on-device with the live thread instead
  of failing to restore its tier on its next prompt, and every thread that
  moved gets a fresh model picker. Local inference is switched back on if it
  was off, so a thread opened after signing out asks for no cloud tier
  (#150).

### Changed

- **Two threads can run a turn at the same time.** An editor runs one siGit
  Code process for every thread it has open, and a single lock ordered every
  turn-affecting request in it, so a second prompt waited for the first
  thread's whole turn and two prompts never overlapped. The lock held what a
  session installs process-wide — the working directory, the workspace roots,
  the MCP servers the client named for it, the live backend — none of which a
  turn touches while it waits on an HTTP endpoint, which is most of its time.
  A session on an HTTP backend now keeps a backend of its own, so its
  conversation is no longer swapped in and out of a shared one, and a turn
  releases the process-wide lock while it waits on the endpoint or on a
  permission answer and takes it back, reinstalling its session, before any
  tool runs. On-device turns are unchanged, since the engine keeps one
  conversation, and tool execution still runs one session at a time (#176).

### Fixed

- **A prompt whose inference failed is kept through a model switch.** A turn
  that ended in an endpoint error stayed in the client's thread, but the
  carryover into a new model dropped its trailing user message, and the error
  path never saved the session. Only a cancelled prompt should be taken back
  out, so cancellation now removes it from the live history, and the error
  path persists the session (#124).

## 1.6.2

### Added

- **`sigit --version` prints the release.** `sigit --version` and `sigit -V`
  print `sigit <version>` on stdout and exit 0. Before, the flag was not
  recognized and siGit Code started a session, which left a script that asked
  for the version waiting.

- **File tools read and write through the editor when it offers to.** An ACP
  client that advertises `fs.readTextFile` or `fs.writeTextFile` serves those
  requests from its open buffers. siGit Code always used the disk, so
  `read_file` missed unsaved changes and `edit_file` wrote underneath a buffer
  the user might have modified. `read_file`, `create_file`, `edit_file` and
  `multi_edit` now go through the client for paths inside the session's roots.
  The two capabilities are independent, and the disk is still the fallback:
  for a path outside the roots, and for a request the client fails or leaves
  unanswered for 30 seconds. `SIGIT_CLIENT_FS=off` keeps the file tools on
  disk (#147).

### Fixed

- **A file attached from the editor is found when its path needs escaping.**
  A `resource_link` was read by stripping `file://` and using the rest as the
  path, so a space arrived as `%20`, a non-ASCII name failed, and a Windows
  URI kept its leading slash. A `#` in a file name was also taken for the
  line-range fragment. The URI is now parsed properly (#170).

## 1.6.1

### Added

- **Images can be attached to a prompt in the editor.** siGit Code now
  advertises `promptCapabilities.image`, so an ACP client offers image
  attachments. The image goes to the model when the active model reads images.
  The model picker marks those models with "reads images". The on-device
  models and four cloud tiers (`nova`, `orbit`, `apex`, `flux`) are text-only:
  with one of them active the image is left out, a note in the thread says so,
  and the model is told an image was withheld. Switching a thread that already
  holds images to a text-only model flags that once as well. Cloud tiers need
  siGit Code Cloud to accept image parts, which is rolling out separately
  (#134).

- **The tool-round cap of a headless run can be set.** `--max-tool-rounds <n>`
  or `SIGIT_MAX_TOOL_ROUNDS` (1 to 500) replaces the built-in 24 for
  `sigit run` and `sigit -p`. The flag wins over the variable.

### Fixed

- **A prompt turn now reports why it stopped.** ACP defines five stop reasons
  and siGit Code only ever sent `end_turn` and `cancelled`. A turn that runs
  into the tool-round cap now ends with `max_turn_requests`, a reply cut off
  at the token limit with `max_tokens`, and a reply the endpoint withheld
  (`content_filter`) with `refusal`. A refused turn is also taken out of the
  conversation, which is what the protocol tells the client to expect (#140).

- **A tool call that needs approval is one card, not two.** The permission
  request carried an id of its own, so an ACP client showed an approval card
  beside a call that already claimed to be running. The call is now announced
  as `pending`, the permission request names that same call, and it moves to
  `in_progress` once approved. Calls allowed by policy still start out
  `in_progress` (#138).

- **A turn that runs out of tool rounds says so.** In an editor, a long task
  could stop on a line like "Now commit:" with nothing after it. The turn had
  used all 24 tool rounds, the model spent its last reply announcing the next
  step, and the call behind it was dropped. siGit Code now tells the model
  that its rounds are used up before that last reply, and ends the turn with a
  message saying why it stopped and that "continue" picks it back up.
  `SIGIT_MAX_TOOL_ROUNDS` now sets the cap for editor sessions too (#120).

- **A headless run no longer ends silently with no final message.** When the
  last round came back empty, which is what happens when a run is cut off at
  the tool-round cap mid-task, stdout was empty and the exit status was 0, so
  a caller could not tell it from a run with nothing to report. siGit Code now
  asks the model once for its final message before finishing.

## 1.6.0

### Added

- **Editors can pass HTTP MCP servers to a session.** siGit Code now
  advertises `mcpCapabilities.http`, so an ACP client may list Streamable HTTP
  servers in `mcpServers` next to stdio ones. They are connected with the URL
  and headers the client sent and are scoped to that session, like the stdio
  ones. SSE is still not advertised, since the MCP spec deprecated it (#151).

- **The plan panel survives a failed turn.** The last `write_todos` list is
  now kept by siGit Code itself. When a turn ends in an endpoint error before
  the model gets to call `write_todos` again, the saved list is sent to the
  editor again, so the plan stays on screen where it used to go blank (#132).

### Changed

- **The ACP Rust SDK is now 2.2** (`agent-client-protocol`, up from 1.3).
  siGit Code still speaks ACP protocol v1 to the editor, so nothing changes on
  the wire. The `unstable_auth_methods` feature is gone because agent auth
  methods are stable in 2.x.

### Fixed

- **MCP servers the editor passes to a session are connected.** ACP clients
  can name MCP servers in `mcpServers` when they open a session, and agents
  are required to connect to the stdio ones. siGit Code accepted the field and
  ignored it. The servers are now spawned when the session opens, their tools
  are offered to that session only, and `/mcp` lists them as coming from the
  editor. `SIGIT_MCP=off` still turns all of it off (#135).

- **A relative `cwd` is rejected when a session starts.** ACP requires the
  working directory and every `additionalDirectories` entry to be absolute.
  `session/new`, `session/load` and `session/fork` used to accept a relative
  one and resolve it against wherever the editor spawned the process. They now
  answer with an invalid-params error (#136).

- **Loading a session that does not exist is an error.** `session/load` with
  an id that was never saved used to succeed and open an empty session under
  that id, so the editor showed a blank thread that looked restored. It now
  answers "not found" (`-32002`). A thread opened in the running process but
  not spoken in yet still loads (#137).

- **Compaction no longer dead-ends a long session.** The summarization request
  used to carry the whole conversation, which by then was already over the
  model's window, so siGit Code Cloud timed out with a 504 and the session
  stopped with "start a new thread". The transcript sent for summarizing is
  now capped, cut from the middle so the opening request and the latest state
  are kept. If summarizing still fails, the oldest messages are dropped until
  the history fits, with whole tool rounds removed together. A failed attempt
  leaves the conversation as it was (#125).

- **Gateway error pages stay out of the chat.** When an endpoint answers with
  an HTML error page, the error shown is the status line, not the page's
  markup (#125).

- **Issue and pull request work no longer assumes GitHub.** The system prompt
  treated forge features as if every repository lived on GitHub, while the
  built-in `mcp__sigit__*` tools only work for repositories hosted on
  sigit.si. The agent now checks the remote first, uses those tools only for
  sigit.si repositories, and reaches for the forge's own CLI (`gh`, `glab`,
  `tea`) elsewhere (#127).

## 1.5.13

### Added

- **Three more siGit Code Cloud tiers: `flare`, `zenith` and `prism`.** They
  show up in `/models` and in the editor's model picker alongside the existing
  tiers. Selecting one needs a signed-in account, as before.

## 1.5.12

### Fixed

- **The Windows binaries start on machines without the Visual C++
  Redistributable.** They used to import `VCRUNTIME140.dll` and the UCRT
  `api-ms-win-crt-*` DLLs, so a clean Windows install refused to load them
  with `STATUS_DLL_NOT_FOUND` (0xC0000135). That is what failed winget's
  post-install check. The C runtime is now linked statically, and CI and the
  release build fail if a Windows exe picks up that dependency again.

- **Watching a slow background command no longer ends the turn early.**
  Polling `command_output` on the same task id three times tripped the
  repeated-tool-call guard, which refused the call and dropped tools for the
  rest of the turn. `command_output` now long-polls — it waits up to
  `wait_seconds` (default 10, max 30) for the task to exit, then returns
  everything printed in the meantime — and the repeat guard exempts it, since
  polling the same task is the whole point of the tool.

- **A turn that ends with no visible text says why.** When the
  repeated-tool-call guard blocked a call and the forced no-tools round came
  back empty, sigit sent nothing more — the turn just stopped with no answer
  and no question. When the last round of a tool-using turn produces no
  visible text, sigit now sends a short closing message naming the repeated
  tool and asking you to reply "continue" or redirect.

## 1.5.11

### Added

- **`sigit run` drives the agent headlessly with resumable sessions.** A new
  non-interactive subcommand runs a prompt through the same agent loop as the
  editor and terminal surfaces, for scripts, CI, and Factory clients. Every run
  gets a UUID session id, writes a JSON-lines history file under
  `~/.config/sigit/sessions/`, and can be restored with `--resume <id>`.
  `--output jsonl` emits a structured session/delta/tool/result stream on
  stdout while logs stay on stderr, and `--allow-tool` pre-approves a tool so
  permission prompts collapse to denial in a non-interactive context. The
  legacy `-p` form still works. Each run also rewrites the session metadata
  with its own cwd and extra roots, so resuming keeps multi-root projects

- **Saved threads can be imported into the editor.** siGit Code now advertises
  ACP's `sessionCapabilities.list` and answers `session/list`, so Zed's "Import
  Threads" picker offers the conversations stored under
  `~/.config/sigit/sessions/` instead of reporting that the agent doesn't
  support the capability. A saved session gets a sidecar recording the project
  directory it ran in — listing reports that `cwd`, the extra roots of a
  multi-root project, an ISO 8601 last-activity timestamp, and a title taken
  from the first user message — and a request may filter on `cwd` so one
  project is never offered another's threads. Threads saved before this have no
  sidecar and are not listed; they still reopen by id through `session/load`

### Changed

- **The commit trailer names the siGit Code version and surface.** Commits
  siGit Code makes now end with
  `Co-Authored-By: siGit Code v<version>-<surface> <noreply@sigit.si>` instead
  of the bare name, where the surface is `acp` (an editor), `tui` (the
  terminal UI), or `headless` (`sigit run`). The model changes from session to
  session; the version and surface tell you which build of the agent wrote the
  commit and how it was driven. GitHub still credits the co-author, since it
  matches on the address. A commit that already carries a siGit Code trailer
  from an older version is left alone

- **The terminal UI's model knows which directory the project is in.** The
  editor (ACP) and headless paths put the working directory in the system
  prompt, but the terminal UI only added the project's instruction files, so
  on `/init` a small on-device model invented a `AGENTS.md` path and failed.
  All three places the terminal UI builds a prompt now go through the same
  session context path the ACP sessions get. The cloud tier switch had also
  been dropping the instruction files; that's fixed too

- **An empty `write_todos` list clears the plan.** `write_todos` rejected an
  empty list, so the model had no way to clear a finished checklist and the
  last plan stayed in the editor until the session ended. An empty list now
  returns "Task list cleared." and goes to the client as an ACP plan with no
  entries. The tool description tells the model it can do this

- **`sigit run` gives clearer usage errors.** A second positional prompt says
  "unexpected extra argument", a bare `sigit run` says "missing prompt", and a
  mistyped flag like `--quite` is rejected instead of becoming the prompt

### Fixed

- **Each ACP session keeps its own roots, conversation, and tasks.** Zed runs
  one siGit process for every open thread, but the process had a single cwd,
  workspace root list, backend conversation, and background task table. The
  most recent session owned all of them, so a prompt from an older thread ran
  against another repo with another thread's history. A `SessionState` is now
  kept per session id; before a prompt or config change runs, the live
  conversation is parked and the requested session's cwd, extra roots, and
  conversation are installed, rebuilding the system prompt from its roots.
  `new`/`load`/`fork` share a single session-opening path, and fork carries the
  source thread's conversation. Background tasks are scoped to the session that
  started them, opening a session no longer wipes every other session's
  permission grants, and model selection is routed per session

- **New sessions no longer carry the previous thread's history.** Opening a
  new thread in Zed showed a `[Conversation summary]` from an earlier session.
  The session handlers cleared the on-device engine but not the history a cloud
  backend keeps for itself, and the startup routing to the cloud tier then
  carried that history into the installed backend. A provider override had the
  same leak. All session entry points now share a path that strips stale
  history too, and unknown session ids are rejected instead of borrowing the
  live thread's state

- **Unparseable tool-call markup is hidden and retried, not shown as text.**
  Some models write tool calls into the reply as text. Well-formed blocks were
  already recovered and run, but a block that failed to parse or never closed
  was printed to the editor as-is and stayed in history, so the model later
  read back a call it had "made" with no result and began writing invented
  `<function_results>`. The scanner now reports those blocks as malformed; the
  backend drops them from the reply and history, and when the only tool call
  was malformed the model gets one retry telling it the call didn't run. A
  block that merely *mentions* a tool-call marker (a backticked `<tool_call>` or an
  unclosed XTML `<|open|>tools`) is now kept as prose, so a reply explaining
  the scanner no longer loses everything after the first marker. The
  non-streamed path now strips broken blocks beside structured calls and runs
  any inline calls that did parse, matching the streaming path

- **The co-author trailer ends a conflicted merge commit.** Finishing a
  conflicted merge with `git commit --no-edit` keeps git's `# Conflicts:` list
  in the message, since no editor runs to strip it. When siGit Code then added
  its trailer, the trailer went in above that list, so it was no longer the last
  paragraph and GitHub didn't credit the co-author. The amend now drops the
  leftover comment block and puts the trailer last. It also fixes a commit
  that already has the trailer but still ends in that block

## 1.5.10

### What changed

- **Xcode's context no longer buries slash commands.** Xcode sends the user's
  prompt as the last of several text blocks, with project context in the
  blocks before it, so `/models` and friends sat in the middle of joined text
  and were handed to the model instead of dispatching locally. Slash-command
  parsing now searches the prompt's text blocks from the end, so a standalone
  command in the final user block dispatches no matter what context the client
  prepends
- **The nova tier is the default cloud engine.** When local inference is off
  and no explicit provider override is set, the cloud tier now defaults to
  `nova` (the `onde-nova` model) instead of the balanced tier, in both the
  interactive and headless paths

### Fixed

- **Malformed Chinese-model tool calls are handled end-to-end in ACP.** The
  inline-call recovery that 1.5.9 added for Kimi K3's XTML protocol and GLM's
  mis-tagged blocks only applied to the interactive and headless surfaces;
  the ACP path never ran it, so an editor session still surfaced raw protocol
  text and dropped the calls. Recovery now runs in the ACP prompt loop too,
  covered by integration tests, and the tool names are checked against the
  turn's offered tools before anything executes
- **Suppressed tool-call log spam is gone.** The guard that logs when a
  structured call is dropped for arriving as forced text fired once per
  streamed argument fragment — one malformed call could log a warning per
  chunk. The check now fires once per turn with the count and names of what
  was dropped, and the non-streaming path reports the same
- **A history-replay test no longer races on the process cwd.** One ACP test
  built its expected path from `std::env::current_dir()` while other tests
  briefly swap that process-global cwd, so on CI it could read a temp
  directory mid-swap and compare a truncated title against an untruncated
  expectation. It now uses a fixed path it never needed to derive

## 1.5.9

### Fixed

- **Kimi K3 tool calls no longer arrive as raw protocol text.** Kimi K3 renders
  a call in Moonshot's XTML protocol — pipe-delimited open/sep/close tokens
  wrapping named blocks — and when the serving stack doesn't parse that back
  into structured tool calls, the whole block surfaced in the editor as literal
  text and the turn ended as though the model had answered in prose. The
  inline-call recovery now speaks that protocol alongside the legacy XML tags:
  tools blocks are parsed into real calls (several per block, argument values
  decoded and typed from the block's own type attribute or, when it is
  missing, the turn's tool schemas), response blocks are unwrapped so their
  text still renders, and think blocks are dropped as private reasoning rather
  than shown. The streaming scanner watches all the opening markers, so a
  block split across chunk boundaries still recovers
- **GLM's mis-tagged calls recover too.** GLM sometimes precedes the legacy
  block's first argument key with another tool-call opening tag instead of the
  arg-key opening tag the format calls for, and the recovery rejected the
  whole block when it did — the call surfaced as text and whatever it was
  trying to do was dropped. The parser now accepts that alias for the first
  argument only — later arguments stay strict, so text that merely resembles a
  call cannot be recovered as one — in both the whole-blob and streaming
  paths, and the scanner tolerates the malformed marker split across chunk
  boundaries. Recovery in both protocols also checks the call's name against
  the tools actually offered in the turn: a tag naming a tool that was never
  in the turn's spec stays on screen instead of being executed

## 1.5.8

### What changed

- **Editor panels can switch permission mode without a slash command.** ACP
  clients now get a Permissions selector next to the model controls with
  Manual, Auto, and Plan choices for the current session. Manual keeps the
  existing approval prompts, Auto lets mutating tools run unattended while
  still respecting explicit deny rules in `settings.toml`, and Plan keeps the
  agent in research-only mode. The selector follows `/plan` and `/clear`, and
  it is deliberately session-scoped so a risky Auto choice does not persist
  into the next task
- **Tool calls in the editor are worth opening now.** Zed only drew the
  disclosure arrow on the handful of cards siGit set `content` on — the model
  download and switch spinners — because everything else carried nothing but
  `rawInput` and `rawOutput`, fields ACP gives clients no display guidance for.
  Every tool call now gets a fenced content block: the pretty-printed arguments
  while it runs, then the tool's output once it finishes, with a `(no output)`
  placeholder for a silent command like `git add`. Cards are titled
  `<tool> · <arg>` rather than the bare tool name, path-bearing tools set
  `locations` so the editor can follow along, and a replayed session gets the
  same treatment as a live one
- **The context window is visible before compaction fires.** Both model pickers
  show each model's window, and the TUI title bar has a gauge for how much of
  it the conversation is using. Cloud tiers report their own window instead of
  the compaction budget — the budget is when siGit Code summarizes history, not
  how much the model can hold, and labelling one with the other understated the
  window by an order of magnitude
- **`write_todos` renders as a plan, not a tool card.** Zed and other clients
  have real progress UI for `session/update` plans, so the model's todo list
  goes there
- **Picker changes read as status rather than chat.** Switching model or
  inference backend is UI state, so it renders as a completed think-kind tool
  call. The sign-in prompt stays an assistant message, since it needs the user
  to act on it

### Fixed

- **A reopened thread comes back with its history.** Clicking a saved thread in
  Zed sends `session/load`, and the client draws the thread purely from the
  `session/update` notifications the agent streams while that request is in
  flight. siGit restored the saved history into the backend, which is what
  makes the model remember, but sent the client nothing — so the thread opened
  empty and looked like a brand new conversation. The snapshot is now turned
  into updates before it is restored: user and assistant text as message chunks
  with reasoning stripped, each tool call completed with its result folded in,
  and `write_todos` as a plan the way it renders live. System messages stay
  out, since they seeded the model and were never on screen
- **Compaction no longer fails in every session that ran a tool.**
  `compact_history` asked for the summary through `complete(None, None)`, which
  sends no tools array but left the live history in place. That history is
  thick with assistant `tool_calls` and `role: "tool"` messages, and an
  endpoint handed tool shapes with no schema to check them against rejects the
  request — Anthropic answers 400. So compaction failed on every attempt in any
  session that had run a single tool, however small the history was, and then
  retried on every prompt and tool round while that history kept growing. The
  conversation now goes as a flattened transcript in one user message, which
  keeps what the summary needs and drops the shapes that only mean anything
  next to a tool schema
- **A tool call emitted as text is no longer dropped.** Qwen 3, GLM and
  DeepSeek write a call as `<tool_call>NAME<arg_key>…` in their chat template
  and rely on the serving stack to parse it back into `tool_calls`. When that
  doesn't happen the tag arrives as ordinary content, so it was rendered
  verbatim in the editor and the turn ended as though the model had chosen to
  answer in prose. Both the streaming and non-streaming paths now scan content
  for those blocks and turn well-formed ones back into real calls, typing
  argument values from the turn's own tool schemas. The streaming scanner holds
  back only enough text to catch a tag straddling a chunk boundary, so ordinary
  answers still stream token by token. A block that doesn't match the expected
  shape is left in the text untouched: reissuing a `run_command` is cheap, but
  guessing wrong at a half-parsed `edit_file` would write the wrong change to a
  file
- **Tool-call content survives awkward output.** Whitespace-only output is
  preserved rather than collapsed, malformed arguments no longer get a
  misleading JSON fence, and fence language identifiers are sanitized

## 1.5.7

### What changed

- **`@smbcloud/sigit` gets releases again.** The npm scope moved to
  `@getsigit` in 1.5.5 and the old package was left sitting on the registry at
  1.5.2, so an install made before the rename went quiet — `npm update` had
  nothing to give it and nothing said why. All seven packages now publish under
  both scopes from the same build, so `@smbcloud/sigit` is a full install again
  rather than a stub, with its own platform binaries and no dependency on the
  new scope. It prints a line on stderr saying where the package moved to;
  `SIGIT_SUPPRESS_SCOPE_NOTICE=1` turns that off. This is a migration path, not
  a second home: `@getsigit/sigit` is what the docs, the Homebrew tap, and the
  ACP registry entry point at, and it is the name to move to
- **Multi-root projects now work in the editor.** When a client opens several
  directories in one project, ACP sends the extra ones as
  `additionalDirectories` — but only to an agent that says it wants them, which
  siGit never did, so Zed kept the first root and showed a banner saying
  multi-root workspaces were unsupported. The capability is now advertised at
  `initialize`, and the directories that arrive are no longer logged and thrown
  away. All the roots are now part of the session: the model is told about each
  one, every root's `AGENTS.md` / `CLAUDE.md` is loaded, and skills, slash
  commands, and subagent types are discovered from all of them (primary root
  first, so it still wins name collisions). `/status` lists the roots when
  there is more than one, and headless runs take the same thing as repeatable
  `--add-dir <dir>` flags. MCP servers are the exception: discovery happens
  once at startup, before a session exists, so a second root's `.sigit/mcp.toml`
  is not picked up

### Fixed

- **A shell command can no longer wedge the editor session.** Tool commands
  inherited siGit's own stdin, which in ACP mode is the JSON-RPC pipe from the
  editor. A command that read stdin — a signing passphrase prompt, a pager, a
  `git` credential ask — blocked on it and consumed the client's next request
  out of the pipe, so nothing typed afterwards ever reached the agent and the
  session stayed dead past the command timeout. Commands, hooks, and the
  co-author `git` helpers now run with stdin closed, so a command that wants
  input gets EOF and reports an error instead
- **A command that prints a lot no longer stalls for two minutes.**
  `run_command` waited for the command to exit before reading its output, so
  anything past the pipe buffer (a build, a verbose test run) blocked writing
  while siGit blocked waiting, until the 120-second timeout killed it. Output
  is now drained while the command runs
- **The command timeout now stops the whole command.** It killed only the
  shell, leaving anything that shell had started running and unreaped; it now
  kills the process group
- **A refused turn now shows the endpoint's own message.** siGit pasted the
  status line and the raw JSON body into the editor's error banner, so a
  billing or allowance message arrived wrapped in
  `endpoint returned 429 Too Many Requests: {"error":{"message":…`. The message
  the endpoint wrote is what the banner shows now; the status is only used when
  there is no message to show
- **An error that arrives mid-stream is no longer silent.** An endpoint that
  fails after the response is already open reports it as a frame in the stream.
  That frame carries no `choices`, so siGit parsed it as an empty chunk and
  skipped it, and the turn ended as though the model had answered with nothing

## 1.5.6

### What changed

- **Switching models mid-conversation no longer restarts it.** Both switching
  to a local GGUF model and picking a different cloud tier now carry the live
  conversation history across, repairing any half-finished tool call so
  strict OpenAI-compatible endpoints don't reject the session
- **Long-running tools now show progress in ACP clients.** Synchronous tools
  (shell commands, file operations, `read_website`, ...) run on a blocking
  thread pool instead of the ACP connection's own task, so a slow command's
  `in_progress` update reaches the client — and Zed's spinner — while the
  command is still running instead of arriving with the result
- **Text from consecutive tool rounds no longer runs together.** ACP and
  headless clients concatenate consecutive message chunks, so a reply that
  continued after a tool call used to glue onto the previous round's last
  sentence with no space. A blank line now opens the next round's text
- **New onde-cloud model tiers.** The model picker (`/models` in the TUI, the
  config panel in editors) now offers five additional siGit Code Cloud tiers —
  `flux`, `apex`, `aura`, `orbit`, and `nova` — alongside the existing Fast,
  Balanced, Large, Mini, Air, Pro, and KKK options. The tier list mirrors
  onde-cloud's router catalogue, so all three sides (the picker, the router,
  and the cloud catalogue) have to be updated together when a tier is added
- **MCP Registry listing moved to `getsigit/si`.** The `server.json` manifest
  and its `release-mcp-registry.yml` publish workflow now live in the
  [`getsigit/si`](https://github.com/getsigit/si) repo alongside the si CLI's
  other code, since the registry entry describes the si platform rather than
  the coding agent

## 1.5.5

A distribution release. siGit Code itself is unchanged. What moved is how it
reaches you on npm and on Windows.

### What changed

- **The npm package is now `@getsigit/sigit`.** The six platform packages moved
  with it. `@smbcloud/sigit` stays on the registry but stops receiving updates,
  so upgrading means switching names:
  `npm uninstall -g @smbcloud/sigit && npm install -g @getsigit/sigit`. The old
  package had been stuck at 1.5.2, so this picks up 1.5.3 and 1.5.4 as well.
- **npm publishes over OIDC.** Every `@getsigit/*` package has a trusted
  publisher pointing at the release workflow, so no npm token is stored in the
  repository. The release workflow moves to Node 22, which trusted publishing
  requires.
- **winget releases actually go out.** The workflow could only update a package
  already present in `microsoft/winget-pkgs`, and siGit Code had never been
  submitted there, so every dispatch failed. It now submits the package the
  first time and updates it on later releases. The identifier is
  `getSigit.siGitCode`.

## 1.5.4

- **CI**: nfpm config expanded to include package version and binary path

## 1.5.3

Adds browser-based sign-in, expands distribution channels to Windows (winget,
Scoop), Linux (AUR, deb, rpm), and .NET (NuGet), gives ACP clients live
visibility into tool activity, adds the KKK and Moonshot AI cloud tiers, and
fixes the co-authored-by trailer email.

### What changed

- **Browser sign-in:** `sigit login` now opens a browser OAuth flow (PKCE S256
  against the siGit Code Cloud authorization server) so you can sign in without
  a password. A loopback listener catches the redirect automatically; a
  paste-back fallback handles environments where a loopback isn't reachable.
  `--paste` forces the paste path, `--password` keeps the old email/password
  prompt. Zed's "Sign in to siGit Code" button uses the loopback path via ACP
  `authenticate`
- **New release channels:** siGit Code is now published to winget
  (`getsigit.siGitCode`), Scoop (`getsigit/scoop-bucket`), the AUR
  (`sigit-bin`), and as `.deb`/`.rpm` packages alongside the existing
  Homebrew, npm, PyPI, crates.io, and NuGet channels
- **NuGet .NET tool:** `dotnet tool install --global SiGit.Code` now works on
  Windows, macOS, and Linux — the package bundles all six platform binaries and
  a managed shim that execs the right one, leaving stdin/stdout/stderr
  unredirected so TUI vs ACP mode detection still works
- **New cloud tiers:** the model picker (`/models` in the TUI, the config
  panel in Zed) now offers the `onde-kkk` (KKK) and Moonshot AI (`oke`) tiers
  alongside the existing Fast, Balanced, Large, Mini, Air, and Pro options
- **ACP tool visibility:** the agent loop now emits `ToolCall(InProgress)`
  before each tool runs and `ToolCallUpdate(Completed/Failed)` after it
  returns, so clients like Zed gain live visibility into tool activity between
  inference rounds. `tool_choice: "auto"` is also sent explicitly whenever
  tools are present, fixing models that describe actions in prose rather than
  issuing a tool call when the field is absent
- **Malformed tool argument logging:** invalid tool argument JSON is now logged
  at three sites (TUI pretty-printer, ACP tool-call event, permission request)
  instead of silently falling back to a string value, making malformed payloads
  visible in logs while preserving backward-compatible behaviour
- **Co-authored-by trailer fix:** the `Co-Authored-By` trailer email was
  updated to `noreply@sigit.si` (platform-neutral); the commit attribution
  check was tightened to use idiomatic ownership
- **NuGet trusted publishing:** the NuGet release workflow now exchanges a
  GitHub OIDC token for a short-lived NuGet API key (via `NuGet/login`) instead
  of relying on a long-lived `NUGET_API_KEY` secret, so there is no stored
  credential to rotate or leak
- **AUR dispatch temporarily disabled:** the AUR release workflow is no longer
  dispatched from `release-github` while new AUR account registration is
  closed; the `sigit-bin` channel returns once registration reopens
- Legal and branding copy updated

## 1.5.2

Brings the latest siGit Code Cloud tiers to the picker and aligns the MCP
registry metadata with the server release.

### What changed

- `/models` now lists the Pro, Air, and Mini siGit Code Cloud tiers alongside
  Fast, Balanced, and Large
- Updated the MCP registry manifest to version 1.2.5, matching the siGit Code
  Cloud server release
- Updated the Agent Client Protocol SDK to the latest v1 release

## 1.5.1

Makes siGit Code work as an Xcode custom agent, and updates the Onde SDK.

### What changed

- Xcode can now run siGit Code as a custom agent. Point it at the binary with
  `--acp` as the argument, and that explicit mode loads the selected on-device
  model on the first prompt, since Xcode's chat has no equivalent of `/load`.
  The README covers the setup, including wiring up `xcrun mcpbridge` in
  `mcp.toml` so the agent can use Xcode's build, test, and project tools
- MCP calls to the `xcode` server time out after 30 seconds instead of 120, so
  a request Xcode cannot service no longer leaves the prompt looking busy for
  two minutes. String JSON-RPC ids from the bridge are handled too
- The agent stops repeating itself: a tool call made three times with identical
  arguments now forces a text reply instead of looping
- Onde SDK bumped to 1.2.2
- The registry listing in `server.json` matches what sigit.si publishes

## 1.5.0

Opens the agent up to user extension: lifecycle hooks, custom slash commands,
and configurable subagent types. Also adds web search, a Repo tab in the TUI,
and a baked-in smbCloud MCP server.

### What changed

- Hooks let you run shell commands at three points in the agent's lifecycle:
  `session_start`, `pre_tool_use`, and `post_tool_use`, configured under
  `[hooks]` in `settings.toml`. Commands get context through variable
  substitution (`{cwd}`, `{tool_name}`, `{tool_result_len}`) and run in the
  session working directory. A hook that fails is logged and the session
  continues; a hook that hangs is killed on a timeout, along with its whole
  process group
- User-defined slash commands: drop a Markdown file in `.sigit/commands/` or
  `.claude/commands/` (or the personal `$SIGIT_CONFIG_DIR/commands/` and
  `~/.claude/commands/`) with optional `description` and `argument-hint`
  frontmatter, and the body becomes a prompt template. `$ARGUMENTS` takes the
  whole argument string and `$1`..`$9` the positional words. A subdirectory
  namespaces the command with `:`, so `.sigit/commands/git/commit.md` becomes
  `/git:commit`. They run as ordinary agent turns through the usual tools and
  permission checks, and are advertised to ACP clients like Zed. `/commands`
  lists what was discovered
- Configurable subagent types for the `task` tool: a Markdown file in
  `.sigit/agents/` or `.claude/agents/` with `name`, `description`, and an
  optional `tools:` allow-list, whose body becomes that subagent's system
  prompt. Pass its name as `subagent_type` to swap the prompt in. The
  `tools:` list can only narrow the subagent's read-only ceiling, never widen
  it, so a config file cannot grant itself `edit_file` or `run_command` and
  route around the permission system. `/agents` lists the discovered types
- New `web_search` tool, backed by the Brave-Search-backed search on siGit
  Code Cloud's MCP server. It is offered only when you are signed in, and it
  is classified read-only, so searching never triggers a permission prompt
- The TUI gains a Repo tab, shown when the session's `origin` remote points at
  the sigit.si host. It lists issues and pull requests fetched through the
  official MCP server: Up and Down select, Enter opens a scrollable detail,
  `i`, `p`, Left and Right switch sections, `r` refreshes. The tab is hidden
  and skipped in the cycle for any other remote
- The smbCloud CLI's stdio MCP server (`smb --mcp`) is now baked in, so
  smbCloud project and deployment tools work with no `mcp.toml` setup. It is
  added only when the `smb` binary is on `PATH`, its read-only tools (`me`,
  `deployments`, `project_list`, `project_show`) skip permission prompts, and
  you can opt out with `smbcloud = false` in `mcp.toml` or
  `SIGIT_MCP_SMBCLOUD=off`
- siGit Code Cloud's MCP server is now listed in the public MCP Registry as
  `si.sigit/sigit`, published from `server.json` at the repository root

### Fixes

- An explicit `deny` rule now applies to read-only first-party tools instead
  of being skipped
- A subagent type whose `tools:` list resolves to an empty set is rejected
  rather than producing a subagent with no tools
- Command templates render in a single substitution pass, so an argument that
  contains something like `$1` is no longer re-substituted
- Frontmatter is stripped with the same leniency it is parsed with
- The Repo tab's detail view no longer shows stale data after a refresh

## 1.4.1

Two small fixes: correct co-author attribution and a Homebrew tap fix.

### What changed

- The `Co-Authored-By` trailer siGit Code adds to commits now uses the GitHub
  noreply address for the [sigitc](https://github.com/sigitc) account instead
  of `sigit@sigit.si`, so GitHub reliably attributes co-authored commits to
  the siGit Code profile
- Fixed the Homebrew tap install docs to trust the tap with `brew trust`
  instead of a `brew tap --force` flag

## 1.4.0

Adds a headless one-shot mode, fine-grained permission rules, stdio transport
for MCP servers, an `/init` command, and a set of TUI upgrades: tabs, a live
thinking display, and collapsible tool calls.

### What changed

- New headless mode: `sigit -p "prompt"` runs one agent turn with tools and exits — 0 on a completed turn, 1 on inference errors, 2 on bad invocations. Assistant text streams to stdout while logs and tool progress stay on stderr; `--cwd` sets the working directory and `--quiet` prints only the final message. Nobody can answer a permission prompt in a headless run, so `ask` collapses to a denial: grant tools for the run with `--allow-tool`, block them with `--deny-tool`, or use `SIGIT_PERMISSIONS=allow`
- Permission rules make autonomy granular: `[permissions.rules]` in `settings.toml` holds ordered `allow` and `deny` lists of `tool_name(pattern)` entries, e.g. `run_command(git *)` or `edit_file(src/*)`. Patterns match the command string for `run_command` and the path for file tools; deny always beats allow, and unparseable patterns fail closed, so a bad rule can only narrow access. Always-allow on a `run_command` prompt now grants that command family (its first two tokens), not the whole shell
- The MCP client speaks stdio: `mcp.toml` server entries can give a `command`, `args`, and an `env` map instead of a `url`, so the stdio-first majority of published MCP servers plugs in. Same handshake, timeouts, tool namespacing, and output caps as HTTP; `/mcp` shows the command line for stdio servers
- New `/init` command (TUI and ACP): runs a normal agent turn that explores the repository and writes an `AGENTS.md` — or improves an existing `AGENTS.md`/`CLAUDE.md` in place — going through the ordinary tools and permission checks
- The TUI gains tabs, cycled with the Tab key: Session is the chat, History lists saved sessions (restore with Enter, delete with a confirmed double-`d`, refresh with `r`), and Cloud shows the signed-in account, inference mode, current model and engine state, permission policy, and config dir, with a live Local Inference toggle
- The TUI shows model thinking: while a reply streams, the last three lines of the reasoning appear live under the spinner, dim and italic; finished replies collapse to a one-line indicator with the line count, and `/thinking` expands the full reasoning block above each reply. Display only — nothing changes in what is sent to the model, saved to sessions, or streamed over ACP
- Tool calls in the TUI transcript collapse to single dim title lines (the command for `run_command`, the path for file tools, the pattern for search and glob); `/tools` expands every entry to show the pretty-printed arguments and a capped tail of the result. Permission prompts keep showing the full command, and the model still receives full outputs
- When `AGENTS.md` and `CLAUDE.md` sit in the same directory, the instructions loader now prefers `AGENTS.md`, so shared content is injected once

## 1.3.2

Adds a tool permission system with plan mode, durable sessions with context
compaction, background command execution, a subagent research tool, and commit
co-author attribution.

### What changed

- Every tool call now passes a permission policy before executing. Read-only tools always run; mutating tools (and all MCP or unknown tools) are governed by, in order: plan mode, session grants, per-tool overrides, and a default mode from `[permissions]` in `settings.toml` (`allow`/`ask`/`deny`, default `ask`). On `ask`, editors get a native ACP permission dialog (allow once / allow for this session / deny) and the TUI pauses on a y/a/n prompt showing the tool and its arguments. `SIGIT_PERMISSIONS` overrides the default mode for headless runs and clients without permission support
- New `/plan [on|off]` command: plan mode blocks mutating tools and asks the model to present a plan while research tools keep working. `/permissions` prints the effective policy
- Sessions are durable: conversation history is saved per session under `~/.config/sigit/sessions/` after every turn. ACP `session/load` actually restores it, the TUI gets `/resume`, and `/clear` deletes the saved file
- Context compaction: `/compact` compresses the conversation on demand, and the agent compacts automatically once the history approaches a 24k-token budget, summarizing older turns and keeping the recent ones. The tool-round cap rises from 10 to 24 now that long sessions have a defense other than the cap
- `run_command` can run work in the background: pass `run_in_background` and the tool returns a task id immediately, so builds, test suites, and dev servers are no longer killed by the 120 second foreground timeout. Poll with the new `command_output` tool (read-only, never prompts) and stop with `kill_command`
- New `task` tool: delegate research to a fresh subagent conversation that only gets the read-only tools and returns its final answer, keeping the main context small. Available on OpenAI-compatible backends; on-device returns a clear fallback until onde supports a second context
- Commits created by the agent are co-authored: commit messages end with `Co-Authored-By: siGit Code <sigit@sigit.si>` (the [sigitc](https://github.com/sigitc) account), which GitHub renders next to the human author. If the model forgets the trailer, siGit amends it in, never rewriting commits that already exist on a remote
- ACP mode now honors the `OPENAI_BASE_URL`/`OPENAI_API_KEY` provider override at startup, matching the interactive client

## 1.3.1

Adds [Model Context Protocol](https://modelcontextprotocol.io) (MCP) client
support with the official siGit Code MCP server baked in, a set of agent tools
that close parity gaps in the tool layer, and refreshed branding and licensing.

### What changed

- siGit Code is now an MCP client: it connects to MCP servers over the Streamable HTTP transport (a single JSON-RPC endpoint), discovers the tools they expose, and offers them to the model alongside the built-in tools. When the model calls one, the call is forwarded to the owning server and the result fed back into the agent loop
- Bakes in the official siGit Code MCP server at `https://sigit.si/api/v1/mcp` (follows `SIGIT_CLOUD_URL`). When you are signed in (`sigit login`), the cloud session token is sent as the bearer credential
- Configure additional servers in `mcp.toml` — global (`~/.config/sigit/mcp.toml`) or project-local (`.sigit/mcp.toml`). Each `[[server]]` has a `name`, `url`, optional `enabled`, and optional `[server.headers]`; set `official = false` to opt out of the baked-in server
- MCP tools are namespaced `mcp__<server>__<tool>` so they never collide with built-in tools or across servers; tool output is capped to protect the model's context
- Discovery is best-effort at startup and bounded by a per-server timeout, so an unreachable server never blocks startup — it just contributes no tools
- Added a `/mcp` slash command (TUI and ACP) that lists configured servers, their connection status, and the tools each exposes
- Disable MCP entirely with `SIGIT_MCP=off`, or just the official server with `SIGIT_MCP_OFFICIAL=off`
- New agent tools that close parity gaps in the tool layer: `multi_edit` (apply a batch of exact-substring edits to one file atomically — written only if every edit matches), `glob` (locate files by name pattern with `**`/`*`/`?`/`{a,b}`, most-recently-modified first), `write_todos` (render a live task checklist through the tool result for multi-step work), and `remember` (append durable notes to the nearest `AGENTS.md`/`CLAUDE.md`)
- `edit_file` now supports `replace_all` and returns actionable failure context — naming the line whose trimmed text matches when only whitespace differs — so the model self-corrects in one round
- `search_files` gained a `file_glob` filter and a `max_results` cap (default 50, hard-capped at 1000) that also bounds the directory walk
- Refreshed branding and legal: updated `LICENSE`, `README`, and the npm/PyPI package descriptions

## 1.3.0

Adds a Local Inference on/off toggle, the open [Agent Skills](https://agentskills.io)
format, and support for project instruction files (`AGENTS.md` and the like).

### What changed

- Added a Local Inference on/off setting that is the explicit local-vs-cloud mode switch. It is persisted in `~/.config/sigit/settings.toml` (default on, local-first) and can be overridden with `SIGIT_LOCAL_INFERENCE`
- Toggle it with the `/local [on|off]` command (TUI and ACP); ACP clients without slash-command support get an equivalent "Local Inference" On/Off control in the session config panel
- `/models` now groups models by nature — Local vs siGit Code Cloud — and highlights the active mode's group while still showing the other, so the cloud tiers stay discoverable
- Discovers Agent Skills (folders with a `SKILL.md`) from `.sigit/skills/` and `.claude/skills/` in the project, `~/.config/sigit/skills/`, and `~/.claude/skills/`
- Follows the spec's progressive disclosure: each skill's name and description are advertised up front via a new `skill` tool, and the full instructions load only when the agent activates one
- Added a `/skills` slash command (TUI and ACP) that lists the discovered skills
- Reads project instruction files at session start: `AGENTS.md` (the cross-tool standard) and `CLAUDE.md`, walking from the working directory up to the repository root, plus a global file under `~/.config/sigit/`, and injects them into the session's system context so their guidance is always in force
- Nested instruction files are ordered outermost-first so the closest, most specific file takes precedence; the scan never reads above the repository root
- On-device models are no longer loaded implicitly. The chat UI and ACP sessions come up immediately, and the local model is brought into memory only when you run the `/load` command (or pick one in `/models`). Prompts sent before a model is loaded now return a hint instead of blocking on a multi-minute download.

## 1.2.2

Streams assistant tokens as they arrive, on-device and over the cloud.

### What changed

- Streamed assistant tokens live in the TUI and ACP sessions, both on-device and through siGit Code Cloud
- On-device inference streams only when a turn offers no tools, since `onde` can't stream and detect tool calls in the same pass; tool-capable turns still resolve in one shot
- Fixed the TUI so the latest message stays visible in long chats
- Put the cloud model-switch confirmation on its own line in ACP

## 1.2.1

Stabilizes the Zed/ACP integration and finishes the cloud-tier wiring on top of 1.2.0.

### What changed

- Fixed a Zed crash by keeping model-picker labels ASCII in the ACP model selector
- Wired ACP auth, cloud tiers, and slash commands into the Zed panel, including a `/reload` command to re-sync session state in place
- Fixed the TUI so the loaded-model checkmark appears once a download completes
- Synced bundled agent skills with the current code and added `CLAUDE.md`

## 1.2.0

Adds siGit Code Cloud — a hosted inference tier alongside on-device models.

### What changed

- Added siGit Code Cloud with cloud-tier routing, pointed at `sigit.si`
- Added account management slash commands and surfaced cloud tiers in `/models`
- Carried forward from 1.1.0: ACP SDK v0.13, refreshed dependencies and branding

## 1.1.0

Bumps the ACP SDK to v0.13 and pulls in updated dependencies.

### What changed

- Updated `agent-client-protocol` from v0.11 to v0.13
- Updated `onde` to 1.1.2
- Refreshed branding and skill metadata

## 1.0.4

This release tightens up the terminal experience and finishes a few release-facing cleanup items.

### What changed

- Added bold rich-text rendering in the TUI for assistant replies, so `**text**` now displays with terminal styling instead of raw markdown markers
- Refreshed the bundled skill metadata to follow the current Agent Skills `SKILL.md` format
- Synced the crate release metadata for the `1.0.4` cut

## 1.0.3

This is the cleanup release for the editor-side startup problems.

### What changed

- Fixed ACP sessions failing on the first real prompt because the server claimed the model was ready before anything had actually been loaded
- Changed ACP startup so the default model loads lazily on the first non-slash prompt instead of pretending it is already in memory
- Kept `initialize` and `session/new` lightweight while still sending proper progress updates once model loading begins
- Updated the Onde integration to `1.0.0`
- Removed a few dependencies we were no longer using

## 1.0.2

This release was supposed to fix the ACP auth breakage. It did fix the stdout pollution problem, but it turned out not to be the whole story.

### What changed

- Delayed model loading in ACP mode so startup diagnostics would not leak into protocol stdout during the auth handshake
- Tightened up the ACP startup path for editor integrations
- Refreshed some README wording while cutting `1.0.2`

## 1.0.1

The first patch after `1.0.0` was mostly about making model loading and model switching feel less opaque.

### What changed

- Added ToolCall-based progress UI for startup model loading and downloading
- Improved model-switch progress reporting in ACP clients
- Fixed model-load error handling so failed switches did not leave the UI in a weird state
- Cleaned up a few status messages and docs while the release was going out

## 1.0.0 (2026)

siGit Code has been living in real smbCloud repos for a while now. At some point it stopped feeling like an experiment, so we called it 1.0.

### What this release is

siGit Code is a local coding agent. It runs a quantized model on your machine, talks to editors over ACP, and can read files, run commands, fetch web pages, and write code without sending your project to a hosted API.

You can install it with Cargo, pip, npm, or Homebrew and use it like any other tool on your machine.

### What shipped in 1.0

#### Editor integration

This is the core of the project.

Zed and VS Code can talk to siGit Code over ACP. Multi-turn sessions work. Tool calling works. Session forking works. Working-directory context works. That was the original goal, and it feels solid now.

#### Terminal UI

The terminal UI started as a side quest and turned out to be useful. You get a full-screen ratatui chat, streaming tokens, a spinner while the model is busy, and a model picker you can open in the middle of a session.

It runs on macOS and Linux. Windows gets ACP and editor mode for now. The Windows terminal UI is still unfinished.

#### Tool calling

This is the part that makes siGit Code feel like an agent instead of a chat box. The loop can run up to 10 rounds per message.

Available tools:

- `read_file` / `write_file` / `delete_file`
- `list_directory` / `search_files`
- `run_command`, with an optional working directory
- `read_website`, which fetches a URL and strips it down to readable text

The model can call a tool, inspect the result, and keep going until it has a real answer.

#### Model support

The model list ended up wider than we expected for 1.0:

- Qwen 3 1.7B, 4B, 8B, and 14B
- Qwen 2.5 1.5B and 3B
- Qwen 2.5 Coder 1.5B, 3B, and 7B
- DeepSeek Coder 6.7B

They are all GGUF models and they all come from Hugging Face on first run.

Qwen 3 is the interesting one. It uses extended thinking mode. The model reasons inside `<think>...</think>` blocks before answering. The TUI strips those blocks out and renders them dimmed above the reply, so you can see what happened without turning the whole conversation into noise.

The 8B model is the desktop default. Mobile stays on 1.7B because iOS gives apps roughly 2 to 3 GB of memory, and we learned the hard way that 3B can blow up on an iPhone 16e.

#### Model picker

The model picker shows:

- what is already cached locally
- what can be downloaded
- which models support tool calling
- whether the local cache looks healthy

You can open it with `/models` in the TUI or through the editor config option. Switching models happens in the background and the UI stays alive while the download or load is in progress.

#### smbCloud context

siGit Code knows smbCloud repos better than a generic coding assistant does. It understands the difference between platform-user flows and tenant-app auth flows, how `Project`, `FrontendApp`, `AuthApp`, and GresIQ fit together, and why Next.js SSR deploys are not the same thing as the generic git-push path.

Outside smbCloud, it backs off and behaves like a normal coding agent.

#### Distribution

Distribution took an unreasonable amount of time, honestly.

There are prebuilt binaries for macOS, Linux, and Windows on both arm64 and x64 where relevant, plus install paths through Cargo, PyPI, npm, and Homebrew:

- `cargo install sigit`
- `pip install sigit-code`
- `npm install -g @smbcloud/sigit`
- `brew install sigit`

Getting that whole pipeline to behave across CI, crates.io, PyPI, npm, and Homebrew was basically its own project.

### What still does not work

The Windows terminal UI is still missing.

ACP and editor mode work on Windows. The part that is still missing is the interactive full-screen terminal UI. The blocker is Unix-specific terminal handling that has not been abstracted cleanly yet.

### Changes since 0.1.2

- Added Qwen 3 14B support
- Added Qwen 3 `<think>` block parsing and separate rendering in the TUI
- Moved all TUI code into `#[cfg(unix)]`, which fixed a pile of dead-code errors on Windows CI
- Added live download progress during model switches, including cancellation with Ctrl+C
- Added model download and loading progress in the Zed agent config panel
- Added an animated spinner during model switching
- Added Qwen 2.5 Coder 7B
- Added downloadable models to the picker, not just locally cached ones
- Made model selection persist across restarts
- Added session working-directory support
- Moved model picker logic into a platform-independent module so Windows can compile without the TUI
- Added the `/models N` shortcut for picking a model by number
- Added the `read_website` tool
- Improved `read_file` handling and empty-reply detection
- Added async tool execution
- Fixed CI cross-compilation for macOS, iOS, Linux, and Windows
- Added npm, PyPI, and Homebrew distribution

---

*© 2026 PT Sigit Mitra Bangun ([siGit Code & Deploy](https://sigit.si)), distributed by [Splitfire AB](https://5mb.app).*
