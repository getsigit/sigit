//! Agent tools: schema definitions + execution for siGit Code.

use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::backend::{InferenceBackend, ToolResult, ToolSpec};

const WEBSITE_READ_CHAR_LIMIT: usize = 20_000;
const WEBSITE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const WEBSITE_USER_AGENT: &str =
    "siGit/0.1 (+https://github.com/getsigit/sigit; website-reading tool)";

const READ_FILE_CHAR_LIMIT: usize = 10_000;
const SEARCH_FILES_MATCH_LIMIT: usize = 50;
/// Upper bound on `max_results` for `search_files` and the number of paths
/// returned by `glob`, so a broad pattern can't flood the context window.
const SEARCH_RESULTS_HARD_CAP: usize = 1_000;

// ── Tool schemas ─────────────────────────────────────────────────────────────

pub struct AgentTool {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters_schema: Value,
}

pub fn all_tools() -> Vec<AgentTool> {
    vec![
        AgentTool {
            name: "read_file",
            description: "Read the contents of a file at the given path. \
                           Prefer an absolute path when possible. Use start_line and \
                           end_line to read a specific range instead of the whole file — \
                           strongly prefer this when you already know which lines matter. \
                           Output is truncated to 10 000 characters.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file to read."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "First line to read (1-based, inclusive). Omit to start from the beginning."
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Last line to read (1-based, inclusive). Omit to read to the end."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "create_directory",
            description: "Create a directory at the given path. \
                           Prefer an absolute path when possible. Missing parent \
                           directories are created automatically. Use this before \
                           create_file when the parent path does not exist. Succeeds \
                           if the directory already exists.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the directory to create."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "list_directory",
            description: "List files and directories at the given path. \
                           Prefer an absolute path when possible. Each entry is \
                           prefixed with [DIR] or [FILE]. Directories are listed \
                           first, sorted alphabetically.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the directory to list."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "search_files",
            description: "Search for a regex pattern across files in a directory tree. \
                           Prefer an absolute root path when possible. Returns matching \
                           lines in `file:line_number: content` format. Skips binary \
                           files and hidden directories. Pass `file_glob` to restrict the \
                           search to files whose name matches a glob (e.g. \"*.rs\"), and \
                           `max_results` to raise or lower the default cap of 50 matches.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression pattern to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "Root directory to search in. Defaults to \".\" (current directory)."
                    },
                    "file_glob": {
                        "type": "string",
                        "description": "Optional glob on the file name (not the full path), e.g. \"*.rs\" or \"*.{ts,tsx}\". Only matching files are searched."
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of matching lines to return (default 50, capped at 1000)."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "read_website",
            description: "Fetch a web page and return readable text content. \
                           Use this when the user gives you a URL and asks you to read, \
                           summarize, inspect, or extract information from the page. \
                           Supports normal http and https URLs. Output is truncated if the \
                           page is very large.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute http or https URL to fetch."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "create_file",
            description: "Create a new file at the given path with the provided content. \
                           Prefer an absolute path when possible. Parent directories are \
                           created automatically if they do not exist. Fails if the file \
                           already exists — use edit_file to modify existing files.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path for the new file."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full text content to write into the new file."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "edit_file",
            description: "Edit an existing file by replacing an exact substring (old_text) with \
                           new text (new_text). Prefer an absolute path when possible. By \
                           default old_text must appear exactly once; set replace_all to true \
                           to replace every occurrence (useful for renaming a symbol). Use \
                           read_file first to see the current content and identify the exact \
                           text to replace. To append to a file, match the last few lines as \
                           old_text and include them plus the new content as new_text.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the existing file to edit."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "The exact text span to find and replace. Must match exactly once unless replace_all is true."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "The replacement text that will take the place of old_text."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence of old_text instead of requiring a unique match. Defaults to false."
                    }
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "delete_file",
            description: "Delete a file or empty directory at the given path. \
                           Prefer an absolute path when possible. Refuses to delete \
                           non-empty directories to prevent accidental data loss. \
                           Use read_file or list_directory first to confirm the target.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file or empty directory to delete."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "run_command",
            description: "Run a shell command and return its combined stdout and stderr output. \
                           The command runs in the given working directory (defaults to the \
                           user's home directory). Always use an absolute working directory \
                           path. Use this for build tools (cargo, npm, make), package managers, \
                           linters, test runners, and git commands, including git init, \
                           porcelain commands like status/add/commit/checkout, and plumbing \
                           commands like rev-parse, hash-object, update-ref, and cat-file. \
                           For `git clone`, always specify the full absolute destination path \
                           as the last argument (e.g. `git clone <url> /absolute/path/to/dir`) \
                           and set cwd to the parent directory. Never run `git clone` without \
                           an explicit destination. If the user asks for a new repo or scaffold, \
                           use this for `git clone`, `git init`, and normal repo setup steps. \
                           In smbCloud repos, prefer existing workspace commands, Rails \
                           conventions, and deploy flows over inventing new command sequences. \
                           Foreground commands are killed after 120 seconds — run servers, \
                           watchers, builds, test suites, and anything that may exceed two \
                           minutes with run_in_background set to true, then poll with \
                           command_output.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute (e.g. \"cargo update\", \"git status\", \"git rev-parse HEAD\")."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for the command. Defaults to \".\" (current directory)."
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "Start the command as a background task and return a task id immediately instead of waiting for it to finish. Set this to true for servers, watchers, builds, test suites, and anything that may run longer than two minutes (foreground commands are killed after 120 seconds). Poll the task's output and status with command_output, and stop it with kill_command. Background tasks are killed when sigit exits. Defaults to false."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "multi_edit",
            description: "Apply several exact-substring edits to a single file in one call. \
                           Edits are applied in order, each to the result of the previous one, \
                           and the whole batch is atomic — if any edit fails to match, the file \
                           is left untouched and an error explains which edit failed. Prefer \
                           this over multiple edit_file calls when changing several spots in the \
                           same file. Each edit has old_text (must match exactly once, or every \
                           time when replace_all is true) and new_text.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the existing file to edit."
                    },
                    "edits": {
                        "type": "array",
                        "description": "Ordered list of edits to apply to the file.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {
                                    "type": "string",
                                    "description": "The exact text span to find and replace."
                                },
                                "new_text": {
                                    "type": "string",
                                    "description": "The replacement text."
                                },
                                "replace_all": {
                                    "type": "boolean",
                                    "description": "Replace every occurrence instead of requiring a unique match. Defaults to false."
                                }
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "glob",
            description: "Find files by name using a glob pattern (e.g. \"**/*.rs\", \
                           \"src/**/*.{ts,tsx}\", \"Cargo.toml\"). Returns matching file paths, \
                           most-recently-modified first. Supports `*` (any run of non-separator \
                           characters), `**` (any number of directories), `?` (one character), \
                           and `{a,b}` alternation. Use this to locate files by name; use \
                           search_files to search file contents.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern matched against paths relative to the search root."
                    },
                    "path": {
                        "type": "string",
                        "description": "Root directory to search in. Defaults to \".\" (current directory)."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "write_todos",
            description: "Record or update a checklist of the steps for the current task. \
                           Use this for any multi-step task to plan the work and show progress: \
                           call it once up front with all the steps as `pending`, then call it \
                           again whenever a step's status changes. Mark exactly one step \
                           `in_progress` at a time and `completed` as soon as it is done. \
                           Keep the list short and outcome-focused.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The full, current checklist (replaces any previous list).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "Short imperative description of the step."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Current status of the step."
                                }
                            },
                            "required": ["content", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "remember",
            description: "Persist a durable note, preference, or convention by appending it to \
                           this project's instruction file (AGENTS.md / CLAUDE.md). Use this \
                           when the user asks you to remember something for next time, or states \
                           a lasting preference about how to work in this project. The note is \
                           written to the nearest existing instruction file, or a new CLAUDE.md \
                           at the repository root if none exists yet.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "note": {
                        "type": "string",
                        "description": "The fact or preference to remember, phrased as a standalone instruction."
                    }
                },
                "required": ["note"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "command_output",
            description: "Get the output a background task (started with run_command's \
                           run_in_background) has produced since your last check, plus its \
                           status: still running, or exited with an exit code. Poll this \
                           periodically to follow builds, test suites, and servers. Between \
                           polls output is buffered up to 50 000 bytes per task; older \
                           output beyond that is dropped and the truncation is noted.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "integer",
                        "description": "Id of the background task, as returned by run_command with run_in_background."
                    }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        },
        AgentTool {
            name: "kill_command",
            description: "Kill a background task started with run_command's run_in_background. \
                           Reports the tail of the task's unread output and confirms it was \
                           killed. Use this to stop servers or watchers you no longer need and \
                           runaway commands. Background tasks are also killed automatically \
                           when sigit exits.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "integer",
                        "description": "Id of the background task, as returned by run_command with run_in_background."
                    }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
        },
    ]
}

// ── Tool execution ───────────────────────────────────────────────────────────

pub async fn execute_tool(name: &str, arguments: &str) -> String {
    match name {
        "read_file" => exec_read_file(arguments),
        "list_directory" => exec_list_directory(arguments),
        "search_files" => exec_search_files(arguments),
        "read_website" => {
            // reqwest::blocking panics inside a tokio runtime, so run on the blocking pool.
            let args = arguments.to_owned();
            tokio::task::spawn_blocking(move || exec_read_website(&args))
                .await
                .unwrap_or_else(|err| format!("Error: read_website task failed: {err}"))
        }
        "create_directory" => exec_create_directory(arguments),
        "create_file" => exec_create_file(arguments),
        "edit_file" => exec_edit_file(arguments),
        "multi_edit" => exec_multi_edit(arguments),
        "glob" => exec_glob(arguments),
        "write_todos" => exec_write_todos(arguments),
        "remember" => exec_remember(arguments),
        "delete_file" => exec_delete_file(arguments),
        "run_command" => exec_run_command(arguments),
        "command_output" => exec_command_output(arguments),
        "kill_command" => exec_kill_command(arguments),
        "skill" => crate::skills::activate_skill(arguments),
        TASK_TOOL_NAME => exec_task(arguments).await,
        WEB_SEARCH_TOOL_NAME => exec_web_search(arguments).await,
        // Tools discovered from MCP servers are namespaced `mcp__<server>__<tool>`
        // and forwarded to the owning server.
        _ if crate::mcp::is_mcp_tool(name) => crate::mcp::call_tool(name, arguments).await,
        _ => format!("Unknown tool: {name}"),
    }
}

// ── web_search ───────────────────────────────────────────────────────────────
//
// siGit Code Cloud runs a Brave-Search-backed `web_search` tool on its official
// MCP server (`mcp.rs`'s baked-in "sigit" server, discovered as
// `mcp__sigit__web_search`) — the API key and billing live server-side, so
// there is no on-device/local equivalent. This is a thin native wrapper
// around that MCP tool rather than a second implementation of it, for two
// reasons: (1) `mcp.rs` already owns the auth, transport, and error handling,
// so re-doing that here would just be a second copy of the same HTTP/JSON-RPC
// code; (2) every `mcp__*` tool is classified `Mutating` by
// `permissions::classify` (an unknown external tool's side effects can't be
// assumed safe), which would put an "ask" prompt in front of a harmless
// read-only search on every call. Presenting it under the clean `web_search`
// name instead lets `permissions::classify` treat it as read-only, matching
// `read_website`'s zero-friction behavior, while `execute_tool_impl` still
// delegates the actual call to the exact same `mcp::call_tool` codepath as
// any other MCP tool.
//
// Advertised only when the official server has actually discovered a
// `web_search` tool (see `web_search_available`) — the same conditional
// pattern as `skill` and `task` — so a signed-out or MCP-disabled session
// never sees a capability it can't use.

pub const WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// The discovered MCP tool this delegates to: siGit Code Cloud's official
/// server is always registered under the name "sigit" (see `mcp.rs`), so its
/// tools are namespaced `mcp__sigit__<tool>`.
const WEB_SEARCH_MCP_DELEGATE: &str = "mcp__sigit__web_search";

/// Whether `web_search` should be offered this turn: the official MCP server
/// has to have actually discovered a `web_search` tool, which in turn only
/// happens when the user is signed in to siGit Code Cloud (an unauthenticated
/// request to the official server fails during `mcp::init`'s discovery and
/// contributes no tools — see `mcp.rs`). No separate sign-in check is needed
/// here: this is exactly the same conditional-advertisement pattern as
/// `subagent_available`, just checking a tool name instead of a factory.
pub fn web_search_available() -> bool {
    crate::mcp::tool_specs()
        .iter()
        .any(|spec| spec.name == WEB_SEARCH_MCP_DELEGATE)
}

/// Whether `name` is the raw MCP tool [`web_search_tool_spec`] delegates to.
/// Callers assembling the full tool list should exclude this name from
/// whatever they append from `mcp::tool_specs()`, so the model sees the one
/// clean `web_search` option rather than that *and* the raw `mcp__*` name for
/// the same underlying tool.
pub fn is_web_search_delegate(name: &str) -> bool {
    name == WEB_SEARCH_MCP_DELEGATE
}

/// Spec for the `web_search` tool. Lives in the `*_as_specs`/`build_tool_specs`
/// layer (like `skill` and `task`), not in [`all_tools`], because it is only
/// offered when [`web_search_available`] is true. The schema mirrors
/// siGit Code Cloud's actual `web_search` MCP tool exactly (`query` + `count`)
/// so arguments pass through to [`exec_web_search`] unchanged.
pub fn web_search_tool_spec() -> ToolSpec {
    ToolSpec {
        name: WEB_SEARCH_TOOL_NAME.to_string(),
        description: "Search the public web and return matching pages as title, URL, and \
             snippet. Use this to find pages when you don't already know the URL — for \
             current events, library/API documentation, error messages, or anything else \
             outside this repository. Once you have a promising URL, use read_website to \
             fetch its full content. Requires a signed-in siGit Code Cloud account (the \
             search runs server-side); rate-limited per account."
            .to_string(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query."
                },
                "count": {
                    "type": "integer",
                    "description": "Number of results to return (default 5, max 10)."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
        .to_string(),
    }
}

/// The `web_search` tool entry point used by [`execute_tool`]: forward
/// verbatim to the discovered MCP tool. `mcp::call_tool` already handles a
/// missing/uninitialized server gracefully (an in-band error string, never a
/// panic), so there's nothing extra to do here — see the module doc above for
/// why this delegates instead of reimplementing the call.
async fn exec_web_search(arguments: &str) -> String {
    crate::mcp::call_tool(WEB_SEARCH_MCP_DELEGATE, arguments).await
}

// ── task (subagent) ──────────────────────────────────────────────────────────
//
// The `task` tool delegates a self-contained research question to a *nested*
// agent loop running in a fresh conversation, so the main thread receives only
// the final answer instead of every intermediate file read. The subagent's
// toolset is strictly read-only and never includes `task` itself (no
// recursion), so a delegated agent can research but not mutate state.
//
// This module is backend-agnostic and cannot construct a backend, so the
// surface that knows the active provider (`run_acp_server` / `run_interactive`
// in `main.rs`) registers a factory at startup. The factory returns `None`
// when inference runs on-device: onde has a single shared conversation
// history, so a second concurrent context is not possible yet.

pub const TASK_TOOL_NAME: &str = "task";

/// The tool names a subagent may call, filtered from [`all_tools`].
const SUBAGENT_TOOL_NAMES: &[&str] = &[
    "read_file",
    "list_directory",
    "search_files",
    "glob",
    "read_website",
];

/// System prompt seeding every subagent conversation.
pub const SUBAGENT_SYSTEM_PROMPT: &str = "You are a focused research subagent. \
You are given one self-contained task by a calling agent. Investigate it using \
the read-only tools available to you (read_file, list_directory, search_files, \
glob, read_website) and answer it thoroughly but concisely. You cannot modify \
files or run commands. Your final message is returned verbatim to the caller, \
so make it a complete, self-contained answer — include the concrete facts, \
paths, and code excerpts the caller needs, and nothing else.";

/// Rounds of tool calls a subagent may use before it is forced to answer.
const SUBAGENT_MAX_TOOL_ROUNDS: usize = 8;
/// Cap on the answer text returned to the caller.
const SUBAGENT_RESULT_CHAR_LIMIT: usize = 8_000;

/// Returned when `task` is called but no subagent backend can be built.
const SUBAGENT_UNAVAILABLE: &str = "The task tool is not available on-device \
yet: on-device inference has a single conversation context. Do the research \
yourself with the read-only tools.";

/// Builds a fresh backend for one subagent run, or `None` when the active
/// inference cannot host a second conversation (on-device).
pub type SubagentFactory = Box<dyn Fn() -> Option<Arc<dyn InferenceBackend>> + Send + Sync>;

static SUBAGENT_FACTORY: OnceLock<SubagentFactory> = OnceLock::new();

/// Register the process-wide subagent factory. Called once at startup by the
/// surface that resolved the inference provider; later calls are ignored.
pub fn set_subagent_factory(factory: SubagentFactory) {
    let _ = SUBAGENT_FACTORY.set(factory);
}

/// Whether a `task` call could run right now: a factory is registered and it
/// can build a backend. The spec builders (`agent_tools_as_specs` /
/// `build_tool_specs`) use this to offer the tool only when it works, the same
/// conditional pattern as the `skill` tool.
pub fn subagent_available() -> bool {
    SUBAGENT_FACTORY
        .get()
        .is_some_and(|factory| factory().is_some())
}

/// The read-only toolset offered to a subagent, filtered from [`all_tools`] by
/// name. Never contains `task` (recursion) or any mutating tool.
pub fn subagent_tool_specs() -> Vec<ToolSpec> {
    all_tools()
        .into_iter()
        .filter(|tool| SUBAGENT_TOOL_NAMES.contains(&tool.name))
        .map(|tool| ToolSpec {
            name: tool.name.to_string(),
            description: tool.description.to_string(),
            parameters_schema: tool.parameters_schema.to_string(),
        })
        .collect()
}

/// Spec for the `task` tool. Lives in the `*_as_specs`/`build_tool_specs`
/// layer (like `skill` and MCP tools), not in [`all_tools`], because it is
/// only offered when [`subagent_available`] is true.
pub fn task_tool_spec() -> ToolSpec {
    ToolSpec {
        name: TASK_TOOL_NAME.to_string(),
        description: "Delegate a self-contained research task to a subagent that \
             runs in a fresh conversation and returns only its final answer. The \
             subagent can read files, list directories, search, glob, and read \
             websites, but cannot modify anything. Use this for exploratory \
             questions whose intermediate file reads would otherwise clutter this \
             conversation (e.g. \"where is X implemented and how does it work?\"). \
             The subagent cannot see this conversation, so the prompt must be \
             fully self-contained: include absolute paths, symbol names, and \
             exactly what the answer should contain."
            .to_string(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "A short (3-5 word) summary of the task, for progress display."
                },
                "prompt": {
                    "type": "string",
                    "description": "The full task for the subagent. Must be self-contained: the subagent sees nothing of this conversation."
                }
            },
            "required": ["description", "prompt"],
            "additionalProperties": false
        })
        .to_string(),
    }
}

/// The `task` tool entry point used by [`execute_tool`].
async fn exec_task(arguments: &str) -> String {
    exec_task_with(arguments, SUBAGENT_FACTORY.get()).await
}

/// Core of the `task` tool, parameterized on the factory so tests can exercise
/// the unavailable path without touching the process-global `OnceLock`.
async fn exec_task_with(arguments: &str, factory: Option<&SubagentFactory>) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let prompt = match args.get("prompt").and_then(Value::as_str) {
        Some(p) if !p.trim().is_empty() => p,
        _ => return "Error: missing required parameter \"prompt\"".to_string(),
    };
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("(no description)");

    let Some(backend) = factory.and_then(|build| build()) else {
        return SUBAGENT_UNAVAILABLE.to_string();
    };

    log::info!("task: running subagent — {description}");
    run_subagent(backend.as_ref(), prompt).await
}

/// The nested agent loop: a fresh conversation, read-only tools, a round cap,
/// and only the final text returned. Mirrors the main loops in `main.rs` /
/// `chat.rs`: offer tools each round, and pass `tools = None` on the last
/// round to force a text answer.
async fn run_subagent(backend: &dyn InferenceBackend, prompt: &str) -> String {
    let specs = subagent_tool_specs();

    let mut result = match backend.send_message_with_tools(prompt, &specs, None).await {
        Ok(r) => r,
        Err(err) => return format!("Error: subagent inference failed: {err}"),
    };

    let mut round = 0;
    while !result.tool_calls.is_empty() && round < SUBAGENT_MAX_TOOL_ROUNDS {
        round += 1;
        log::info!(
            "task: subagent tool round {round} — {} call(s)",
            result.tool_calls.len()
        );

        let mut tool_results = Vec::with_capacity(result.tool_calls.len());
        for call in &result.tool_calls {
            // Hard gate, not just advertisement: even if the model asks for a
            // tool outside the offered set, only read-only tools execute here.
            let content = if SUBAGENT_TOOL_NAMES.contains(&call.name.as_str()) {
                // Boxed to break the async cycle: execute_tool → task →
                // run_subagent → execute_tool.
                Box::pin(execute_tool(&call.name, &call.arguments)).await
            } else {
                format!(
                    "Error: `{}` is not available to a subagent. Only these \
                     read-only tools are: {}.",
                    call.name,
                    SUBAGENT_TOOL_NAMES.join(", ")
                )
            };
            tool_results.push(ToolResult {
                tool_call_id: call.id.clone(),
                content,
            });
        }

        // On the last round, offer no tools so the model must produce text.
        let next_tools = if round < SUBAGENT_MAX_TOOL_ROUNDS {
            Some(specs.as_slice())
        } else {
            None
        };
        result = match backend
            .send_tool_results(tool_results, next_tools, None)
            .await
        {
            Ok(r) => r,
            Err(err) => return format!("Error: subagent inference failed: {err}"),
        };
    }

    let text = result.text.trim();
    if text.is_empty() {
        return "The subagent finished without a text answer.".to_string();
    }

    let total = text.chars().count();
    if total > SUBAGENT_RESULT_CHAR_LIMIT {
        let truncated: String = text.chars().take(SUBAGENT_RESULT_CHAR_LIMIT).collect();
        return format!(
            "{truncated}\n\n--- truncated (showing {SUBAGENT_RESULT_CHAR_LIMIT} of {total} \
             characters of the subagent's answer) ---"
        );
    }
    text.to_string()
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn absolute_path_string(path: &Path) -> String {
    absolute_path(path).display().to_string()
}

// ── read_file ────────────────────────────────────────────────────────────────

fn exec_read_file(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let path_str = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return "Error: missing required parameter \"path\"".to_string(),
    };

    let start_line = args
        .get("start_line")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let end_line = args
        .get("end_line")
        .and_then(Value::as_u64)
        .map(|n| n as usize);

    let path = Path::new(path_str);
    let absolute_path = absolute_path(path);
    let absolute_path_str = absolute_path.display().to_string();

    if !absolute_path.exists() {
        return format!("Error: path does not exist: {absolute_path_str}");
    }

    if !absolute_path.is_file() {
        return format!("Error: path is not a file: {absolute_path_str}");
    }

    match fs::read_to_string(&absolute_path) {
        Ok(contents) => {
            if start_line.is_some() || end_line.is_some() {
                let lines: Vec<&str> = contents.lines().collect();
                let total = lines.len();
                let start = start_line.unwrap_or(1).max(1);
                let end = end_line.unwrap_or(total).min(total);

                if start > total {
                    return format!(
                        "Error: start_line {start} is beyond end of file ({total} lines)"
                    );
                }

                let selected: Vec<&str> = lines[(start - 1)..end].to_vec();
                let range_text = selected.join("\n");
                format!("Lines {start}-{end} of {total} in {absolute_path_str}:\n{range_text}")
            } else if contents.len() > READ_FILE_CHAR_LIMIT {
                let truncated: String = contents.chars().take(READ_FILE_CHAR_LIMIT).collect();
                format!(
                    "{truncated}\n\n--- truncated (showing {READ_FILE_CHAR_LIMIT} of {} characters) ---",
                    contents.len()
                )
            } else {
                contents
            }
        }
        Err(err) => format!("Error: could not read file: {err}"),
    }
}

// ── list_directory ───────────────────────────────────────────────────────────

fn exec_list_directory(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let path_str = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return "Error: missing required parameter \"path\"".to_string(),
    };

    let path = Path::new(path_str);
    let absolute_path = absolute_path(path);
    let absolute_path_str = absolute_path.display().to_string();

    if !absolute_path.exists() {
        return format!("Error: path does not exist: {absolute_path_str}");
    }

    if !absolute_path.is_dir() {
        return format!("Error: path is not a directory: {absolute_path_str}");
    }

    let entries = match fs::read_dir(&absolute_path) {
        Ok(rd) => rd,
        Err(err) => return format!("Error: could not read directory: {err}"),
    };

    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                files.push(format!("[ERR] {err}"));
                continue;
            }
        };

        let name = entry.file_name().to_string_lossy().to_string();

        let is_dir = match entry.file_type() {
            Ok(ft) => ft.is_dir(),
            Err(_) => false,
        };

        if is_dir {
            dirs.push(format!("[DIR]  {name}"));
        } else {
            files.push(format!("[FILE] {name}"));
        }
    }

    dirs.sort();
    files.sort();

    dirs.extend(files);

    if dirs.is_empty() {
        return format!("(empty directory: {absolute_path_str})");
    }

    dirs.join("\n")
}

// ── search_files ─────────────────────────────────────────────────────────────

fn exec_search_files(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let pattern_str = match args.get("pattern").and_then(Value::as_str) {
        Some(p) => p,
        None => return "Error: missing required parameter \"pattern\"".to_string(),
    };

    let root_str = args.get("path").and_then(Value::as_str).unwrap_or(".");

    let re = match Regex::new(pattern_str) {
        Ok(r) => r,
        Err(err) => return format!("Error: invalid regex pattern: {err}"),
    };

    // Optional file-name filter compiled from a glob (e.g. "*.rs").
    let name_filter = match args.get("file_glob").and_then(Value::as_str) {
        Some(glob) => match Regex::new(&glob_to_regex(glob)) {
            Ok(r) => Some(r),
            Err(err) => return format!("Error: invalid file_glob: {err}"),
        },
        None => None,
    };

    let limit = args
        .get("max_results")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, SEARCH_RESULTS_HARD_CAP))
        .unwrap_or(SEARCH_FILES_MATCH_LIMIT);

    let root = Path::new(root_str);
    let absolute_root = absolute_path(root);
    let absolute_root_str = absolute_root.display().to_string();

    if !absolute_root.exists() {
        return format!("Error: path does not exist: {absolute_root_str}");
    }

    if !absolute_root.is_dir() {
        return format!("Error: path is not a directory: {absolute_root_str}");
    }

    let mut matches: Vec<String> = Vec::new();
    walk_and_search(
        &absolute_root,
        &re,
        name_filter.as_ref(),
        limit,
        &mut matches,
    );

    if matches.is_empty() {
        return format!("No matches found for pattern: {pattern_str}");
    }

    let total = matches.len();
    if total > limit {
        matches.truncate(limit);
        matches.push(format!(
            "\n--- truncated (showing {limit} of {total}+ matches; raise max_results to see more) ---"
        ));
    }

    matches.join("\n")
}

/// Collects up to `limit + 1` matches (the extra signals truncation) so a broad
/// pattern can't walk an entire tree once enough hits are found.
fn walk_and_search(
    dir: &Path,
    re: &Regex,
    name_filter: Option<&Regex>,
    limit: usize,
    matches: &mut Vec<String>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    let mut sorted: Vec<fs::DirEntry> = entries.filter_map(Result::ok).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        if matches.len() > limit {
            return;
        }

        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            walk_and_search(&path, re, name_filter, limit, matches);
        } else if path.is_file() {
            if let Some(filter) = name_filter
                && !filter.is_match(&name_str)
            {
                continue;
            }
            search_file(&path, re, matches);
        }
    }
}

/// skips non-UTF-8 files (probably binary).
fn search_file(path: &Path, re: &Regex, matches: &mut Vec<String>) {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let display_path = absolute_path_string(path);

    for (line_idx, line) in contents.lines().enumerate() {
        if re.is_match(line) {
            let line_number = line_idx + 1;
            matches.push(format!("{display_path}:{line_number}: {line}"));
        }
    }
}

// ── read_website ─────────────────────────────────────────────────────────────

fn exec_read_website(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let url = match args.get("url").and_then(Value::as_str) {
        Some(u) => u,
        None => return "Error: missing required parameter \"url\"".to_string(),
    };

    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return format!("Error: url must start with http:// or https://: {url}");
    }

    let client = match reqwest::blocking::Client::builder()
        .timeout(WEBSITE_READ_TIMEOUT)
        .user_agent(WEBSITE_USER_AGENT)
        .build()
    {
        Ok(client) => client,
        Err(err) => return format!("Error: failed to build website client: {err}"),
    };

    let response = match client.get(url).send() {
        Ok(r) => r,
        Err(err) => return format!("Error: failed to fetch website: {err}"),
    };

    let final_url = response.url().to_string();
    let status = response.status();
    if !status.is_success() {
        return format!("Error: website returned HTTP {status} for {final_url}");
    }

    let body = match response.text() {
        Ok(text) => text,
        Err(err) => return format!("Error: failed to read website body: {err}"),
    };

    let title = Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
        .unwrap()
        .captures(&body)
        .and_then(|captures| captures.get(1))
        .map(|m| {
            Regex::new(r"\s+")
                .unwrap()
                .replace_all(m.as_str(), " ")
                .trim()
                .to_string()
        })
        .filter(|title| !title.is_empty());

    let with_block_breaks = Regex::new(
        r"(?is)</?(?:p|div|section|article|main|aside|header|footer|nav|li|ul|ol|h1|h2|h3|h4|h5|h6|br|tr|td|th)[^>]*>",
    )
    .unwrap()
    .replace_all(&body, "\n");
    let without_scripts = Regex::new(r"(?is)<script[^>]*>.*?</script>")
        .unwrap()
        .replace_all(&with_block_breaks, " ");
    let without_styles = Regex::new(r"(?is)<style[^>]*>.*?</style>")
        .unwrap()
        .replace_all(&without_scripts, " ");
    let without_tags = Regex::new(r"(?is)<[^>]+>")
        .unwrap()
        .replace_all(&without_styles, " ");
    let normalized_newlines = without_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let collapsed_lines = Regex::new(r"[ \t]+")
        .unwrap()
        .replace_all(&normalized_newlines, " ");
    let collapsed_breaks = Regex::new(r"\n\s*\n+")
        .unwrap()
        .replace_all(&collapsed_lines, "\n\n");
    let cleaned = collapsed_breaks
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    if cleaned.is_empty() {
        return format!("Fetched {url}, but no readable text content was found.");
    }

    let mut metadata = vec![format!("URL: {final_url}")];
    if let Some(title) = &title {
        metadata.push(format!("Title: {title}"));
    }

    let body_text = match title {
        Some(title) if !cleaned.starts_with(&title) => cleaned,
        _ => cleaned,
    };

    let output = format!("{}\n\n{}", metadata.join("\n"), body_text);

    if output.len() > WEBSITE_READ_CHAR_LIMIT {
        let truncated: String = output.chars().take(WEBSITE_READ_CHAR_LIMIT).collect();
        return format!(
            "{truncated}\n\n--- truncated (showing {WEBSITE_READ_CHAR_LIMIT} of {} characters) ---",
            output.len()
        );
    }

    output
}

// ── create_directory ─────────────────────────────────────────────────────────

fn exec_create_directory(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let path_str = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return "Error: missing required parameter \"path\"".to_string(),
    };

    let path = Path::new(path_str);
    let absolute_path = absolute_path(path);
    let absolute_path_str = absolute_path.display().to_string();

    if absolute_path.exists() {
        if absolute_path.is_dir() {
            return format!("Directory already exists: {absolute_path_str}");
        }
        return format!("Error: path exists and is not a directory: {absolute_path_str}");
    }

    match fs::create_dir_all(&absolute_path) {
        Ok(()) => format!("Created directory: {absolute_path_str}"),
        Err(err) => format!("Error: could not create directory: {err}"),
    }
}

/// fails if file exists so the LLM is forced to use `edit_file` for modifications.
fn exec_create_file(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let path_str = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return "Error: missing required parameter \"path\"".to_string(),
    };

    let content = match args.get("content").and_then(Value::as_str) {
        Some(c) => c,
        None => return "Error: missing required parameter \"content\"".to_string(),
    };

    let path = Path::new(path_str);
    let absolute_path = absolute_path(path);
    let absolute_path_str = absolute_path.display().to_string();

    if absolute_path.exists() {
        return format!(
            "Error: file already exists: {absolute_path_str} — use edit_file to modify existing files"
        );
    }

    if let Some(parent) = absolute_path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
        && let Err(err) = fs::create_dir_all(parent)
    {
        return format!("Error: could not create parent directories: {err}");
    }

    match fs::write(&absolute_path, content) {
        Ok(()) => format!(
            "Created file: {absolute_path_str} ({} bytes)",
            content.len()
        ),
        Err(err) => format!("Error: could not write file: {err}"),
    }
}

// ── edit_file / multi_edit ─────────────────────────────────────────────────

/// Apply one exact-substring replacement to `contents`. Returns the updated
/// string, or a human-readable explanation of why the match failed so the model
/// can correct itself in a single follow-up instead of guessing blindly.
fn apply_edit(
    contents: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old_text.is_empty() {
        return Err("old_text is empty; nothing to match".to_string());
    }
    if old_text == new_text {
        return Err("old_text and new_text are identical; no change to make".to_string());
    }

    let occurrences = contents.matches(old_text).count();

    if occurrences == 0 {
        return Err(format!(
            "old_text not found. Use read_file to copy the exact text \
             (including whitespace and indentation) to replace.{}",
            nearest_line_hint(contents, old_text)
        ));
    }

    if occurrences > 1 && !replace_all {
        return Err(format!(
            "old_text appears {occurrences} times; include more surrounding context so it \
             matches exactly once, or set replace_all to true to change every occurrence."
        ));
    }

    if replace_all {
        Ok(contents.replace(old_text, new_text))
    } else {
        Ok(contents.replacen(old_text, new_text, 1))
    }
}

/// When `old_text` doesn't match verbatim, point at the line whose trimmed text
/// equals the first trimmed line of `old_text` — the usual culprit is a
/// whitespace/indentation mismatch, and naming the line lets the model fix it.
fn nearest_line_hint(contents: &str, old_text: &str) -> String {
    let first = old_text.lines().find(|l| !l.trim().is_empty());
    let Some(first) = first.map(str::trim) else {
        return String::new();
    };
    for (idx, line) in contents.lines().enumerate() {
        if line.trim() == first {
            return format!(
                " (the first line of old_text appears at line {}, so the difference is likely \
                 whitespace or indentation)",
                idx + 1
            );
        }
    }
    String::new()
}

fn exec_edit_file(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let path_str = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return "Error: missing required parameter \"path\"".to_string(),
    };

    let old_text = match args.get("old_text").and_then(Value::as_str) {
        Some(t) => t,
        None => return "Error: missing required parameter \"old_text\"".to_string(),
    };

    let new_text = match args.get("new_text").and_then(Value::as_str) {
        Some(t) => t,
        None => return "Error: missing required parameter \"new_text\"".to_string(),
    };

    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let path = Path::new(path_str);
    let absolute_path = absolute_path(path);
    let absolute_path_str = absolute_path.display().to_string();

    if !absolute_path.exists() {
        return format!(
            "Error: file does not exist: {absolute_path_str} — use create_file for new files"
        );
    }

    if !absolute_path.is_file() {
        return format!("Error: path is not a file: {absolute_path_str}");
    }

    let contents = match fs::read_to_string(&absolute_path) {
        Ok(c) => c,
        Err(err) => return format!("Error: could not read file: {err}"),
    };

    let updated = match apply_edit(&contents, old_text, new_text, replace_all) {
        Ok(updated) => updated,
        Err(why) => return format!("Error: {why} (in {absolute_path_str})"),
    };

    match fs::write(&absolute_path, &updated) {
        Ok(()) => format!(
            "Edited file: {absolute_path_str} ({} bytes written)",
            updated.len()
        ),
        Err(err) => format!("Error: could not write file: {err}"),
    }
}

/// Apply a batch of edits to one file atomically: each edit is applied to the
/// result of the previous one, and the file is only written if *every* edit
/// matches. A failure leaves the file untouched.
fn exec_multi_edit(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let path_str = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return "Error: missing required parameter \"path\"".to_string(),
    };

    let edits = match args.get("edits").and_then(Value::as_array) {
        Some(e) if !e.is_empty() => e,
        Some(_) => return "Error: \"edits\" must contain at least one edit".to_string(),
        None => return "Error: missing required parameter \"edits\"".to_string(),
    };

    let path = Path::new(path_str);
    let absolute_path = absolute_path(path);
    let absolute_path_str = absolute_path.display().to_string();

    if !absolute_path.exists() {
        return format!(
            "Error: file does not exist: {absolute_path_str} — use create_file for new files"
        );
    }

    if !absolute_path.is_file() {
        return format!("Error: path is not a file: {absolute_path_str}");
    }

    let mut working = match fs::read_to_string(&absolute_path) {
        Ok(c) => c,
        Err(err) => return format!("Error: could not read file: {err}"),
    };

    for (idx, edit) in edits.iter().enumerate() {
        let old_text = match edit.get("old_text").and_then(Value::as_str) {
            Some(t) => t,
            None => return format!("Error: edit #{} is missing \"old_text\"", idx + 1),
        };
        let new_text = match edit.get("new_text").and_then(Value::as_str) {
            Some(t) => t,
            None => return format!("Error: edit #{} is missing \"new_text\"", idx + 1),
        };
        let replace_all = edit
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        match apply_edit(&working, old_text, new_text, replace_all) {
            Ok(updated) => working = updated,
            Err(why) => {
                return format!(
                    "Error: edit #{} failed: {why}. No changes were written to {absolute_path_str}.",
                    idx + 1
                );
            }
        }
    }

    match fs::write(&absolute_path, &working) {
        Ok(()) => format!(
            "Applied {} edits to {absolute_path_str} ({} bytes written)",
            edits.len(),
            working.len()
        ),
        Err(err) => format!("Error: could not write file: {err}"),
    }
}

// ── glob ─────────────────────────────────────────────────────────────────────

/// Translate a shell-style glob into an anchored regex. Supports `*`
/// (non-separator run), `**` (any number of directories), `?` (one
/// non-separator), and `{a,b}` alternation; everything else is matched
/// literally. Used by the `glob` tool (against relative paths), by
/// `search_files`' `file_glob` filter (against bare file names), and by
/// `crate::permissions` rule patterns (which re-anchor the result).
pub(crate) fn glob_to_regex(glob: &str) -> String {
    let chars: Vec<char> = glob.chars().collect();
    let mut re = String::from("^");
    let mut brace_depth = 0usize;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    i += 1; // consume the second '*'
                    if i + 1 < chars.len() && chars[i + 1] == '/' {
                        // `**/` matches zero or more leading directories.
                        re.push_str("(?:.*/)?");
                        i += 1; // consume the '/'
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '{' => {
                brace_depth += 1;
                re.push_str("(?:");
            }
            '}' if brace_depth > 0 => {
                brace_depth -= 1;
                re.push(')');
            }
            ',' if brace_depth > 0 => re.push('|'),
            // Escape regex metacharacters so they match literally. (`{` is always
            // consumed by the brace arm above; an unmatched `}` lands here.)
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '\\' | '[' | ']' | '}' => {
                re.push('\\');
                re.push(c);
            }
            other => re.push(other),
        }
        i += 1;
    }

    re.push('$');
    re
}

fn exec_glob(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let pattern = match args.get("pattern").and_then(Value::as_str) {
        Some(p) => p,
        None => return "Error: missing required parameter \"pattern\"".to_string(),
    };

    let re = match Regex::new(&glob_to_regex(pattern)) {
        Ok(r) => r,
        Err(err) => return format!("Error: invalid glob pattern: {err}"),
    };

    let root_str = args.get("path").and_then(Value::as_str).unwrap_or(".");
    let absolute_root = absolute_path(Path::new(root_str));
    let absolute_root_str = absolute_root.display().to_string();

    if !absolute_root.exists() {
        return format!("Error: path does not exist: {absolute_root_str}");
    }
    if !absolute_root.is_dir() {
        return format!("Error: path is not a directory: {absolute_root_str}");
    }

    let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
    glob_walk(&absolute_root, &absolute_root, &re, &mut found);

    if found.is_empty() {
        return format!("No files match glob: {pattern}");
    }

    // Most-recently-modified first.
    found.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));

    let total = found.len();
    let mut paths: Vec<String> = found.into_iter().map(|(_, p)| p).collect();
    if total > SEARCH_RESULTS_HARD_CAP {
        paths.truncate(SEARCH_RESULTS_HARD_CAP);
        paths.push(format!(
            "\n--- truncated (showing {SEARCH_RESULTS_HARD_CAP} of {total} files) ---"
        ));
    }

    paths.join("\n")
}

fn glob_walk(root: &Path, dir: &Path, re: &Regex, out: &mut Vec<(std::time::SystemTime, String)>) {
    if out.len() > SEARCH_RESULTS_HARD_CAP {
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    let mut sorted: Vec<fs::DirEntry> = entries.filter_map(Result::ok).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        if out.len() > SEARCH_RESULTS_HARD_CAP {
            return;
        }

        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }

        if path.is_dir() {
            glob_walk(root, &path, re, out);
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if re.is_match(&relative) {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                out.push((mtime, absolute_path_string(&path)));
            }
        }
    }
}

// ── write_todos ──────────────────────────────────────────────────────────────

/// Renders the model's task checklist back as the tool result so the surface
/// (TUI / ACP client) can show live progress. Pure presentation — the list is
/// owned by the model, not persisted here.
fn exec_write_todos(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let todos = match args.get("todos").and_then(Value::as_array) {
        Some(t) if !t.is_empty() => t,
        Some(_) => return "Error: \"todos\" must contain at least one item".to_string(),
        None => return "Error: missing required parameter \"todos\"".to_string(),
    };

    let mut lines = Vec::with_capacity(todos.len());
    let mut completed = 0usize;

    for (idx, todo) in todos.iter().enumerate() {
        let content = match todo.get("content").and_then(Value::as_str) {
            Some(c) => c.trim(),
            None => return format!("Error: todo #{} is missing \"content\"", idx + 1),
        };
        let status = todo
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");

        let marker = match status {
            "completed" => {
                completed += 1;
                "[x]"
            }
            "in_progress" => "[~]",
            _ => "[ ]",
        };
        lines.push(format!("{marker} {content}"));
    }

    format!(
        "Task list updated ({completed}/{} done):\n{}",
        todos.len(),
        lines.join("\n")
    )
}

// ── remember ─────────────────────────────────────────────────────────────────

/// Appends a durable note to the project's instruction file so it persists
/// across sessions (the always-on counterpart to a one-off chat message).
fn exec_remember(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let note = match args.get("note").and_then(Value::as_str) {
        Some(n) if !n.trim().is_empty() => n.trim(),
        Some(_) => return "Error: \"note\" must not be empty".to_string(),
        None => return "Error: missing required parameter \"note\"".to_string(),
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    remember_at(&cwd, note)
}

/// Core of `remember`, parameterized on the working directory so it can be
/// tested without mutating the process-global current directory.
fn remember_at(cwd: &Path, note: &str) -> String {
    let target = crate::instructions::memory_file(cwd);
    let target_str = target.display().to_string();

    let existed = target.exists();
    let mut body = if existed {
        match fs::read_to_string(&target) {
            Ok(c) => c,
            Err(err) => return format!("Error: could not read {target_str}: {err}"),
        }
    } else {
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
            && let Err(err) = fs::create_dir_all(parent)
        {
            return format!("Error: could not create parent directories: {err}");
        }
        String::new()
    };

    // Keep remembered notes grouped under one heading so the file stays tidy.
    const HEADING: &str = "## Remembered notes";
    if !body.contains(HEADING) {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(HEADING);
        body.push('\n');
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str("- ");
    body.push_str(note);
    body.push('\n');

    match fs::write(&target, &body) {
        Ok(()) => {
            let verb = if existed { "Appended to" } else { "Created" };
            format!("{verb} {target_str}: remembered \"{note}\"")
        }
        Err(err) => format!("Error: could not write {target_str}: {err}"),
    }
}

// ── delete_file ──────────────────────────────────────────────────────────────

/// only removes files or *empty* directories — no recursive deletes.
fn exec_delete_file(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let path_str = match args.get("path").and_then(Value::as_str) {
        Some(p) => p,
        None => return "Error: missing required parameter \"path\"".to_string(),
    };

    let path = Path::new(path_str);
    let absolute_path = absolute_path(path);
    let absolute_path_str = absolute_path.display().to_string();

    if !absolute_path.exists() {
        return format!("Error: path does not exist: {absolute_path_str}");
    }

    if absolute_path.is_dir() {
        match fs::remove_dir(&absolute_path) {
            Ok(()) => format!("Deleted empty directory: {absolute_path_str}"),
            Err(err) => format!(
                "Error: could not delete directory: {err}. \
                 Only empty directories can be deleted."
            ),
        }
    } else {
        match fs::remove_file(&absolute_path) {
            Ok(()) => format!("Deleted file: {absolute_path_str}"),
            Err(err) => format!("Error: could not delete file: {err}"),
        }
    }
}

// ── run_command ──────────────────────────────────────────────────────────────

const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const COMMAND_OUTPUT_LIMIT: usize = 50_000;

/// Spawn `command_str` through the platform shell with piped stdout/stderr.
/// Shared by the foreground and background paths of `run_command`.
fn spawn_shell(command_str: &str, cwd_path: &Path) -> std::io::Result<std::process::Child> {
    #[cfg(unix)]
    let (shell, flag) = ("sh", "-c");
    #[cfg(windows)]
    let (shell, flag) = ("cmd", "/C");

    Command::new(shell)
        .arg(flag)
        .arg(command_str)
        .current_dir(cwd_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
}

/// Trailer identifying siGit Code as the co-author of commits it creates.
/// GitHub detects `Co-authored-by:` trailers on the last lines of a commit
/// message (separated from the body by a blank line) and lists the agent
/// alongside the human author; `297239231+sigitc@users.noreply.github.com`
/// is the noreply address for the <https://github.com/sigitc> account
/// ("siGit Code"), so the co-author is rendered with that account's avatar
/// and profile link. The system prompt asks the model to add this itself;
/// [`ensure_commit_co_author`] is the safety net when it forgets.
pub const COMMIT_CO_AUTHOR_TRAILER: &str =
    "Co-Authored-By: siGit Code <297239231+sigitc@users.noreply.github.com>";

/// Run `git <args>` in `cwd`, returning trimmed stdout on success.
fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_head(cwd: &Path) -> Option<String> {
    git_stdout(cwd, &["rev-parse", "HEAD"])
}

/// Deterministic co-author attribution: if the commit at HEAD lacks the
/// siGit Code trailer, amend it in (via `git commit --amend --trailer`, which
/// places it after a blank line — the format GitHub detects). Never rewrites
/// a commit that is already on a remote. Returns a note describing the amend
/// so the model and user can see it happened.
fn ensure_commit_co_author(cwd: &Path) -> Option<String> {
    let message = git_stdout(cwd, &["log", "-1", "--format=%B"])?;
    if message
        .to_lowercase()
        .contains("co-authored-by: sigit code")
    {
        return None;
    }
    // Amending changes the commit id; a commit that any remote ref already
    // contains must be left alone or the branch diverges from its upstream.
    match git_stdout(cwd, &["branch", "-r", "--contains", "HEAD"]) {
        Some(remotes) if remotes.is_empty() => {}
        _ => return None,
    }
    let amend = Command::new("git")
        .args(["commit", "--amend", "--no-edit", "--trailer"])
        .arg(COMMIT_CO_AUTHOR_TRAILER)
        .current_dir(cwd)
        .output()
        .ok()?;
    if amend.status.success() {
        log::info!(
            "appended co-author trailer to the new commit in {}",
            cwd.display()
        );
        Some(format!(
            "[siGit Code] The new commit was amended to append the co-author trailer \
             \"{COMMIT_CO_AUTHOR_TRAILER}\" (its hash changed)."
        ))
    } else {
        log::warn!(
            "could not append co-author trailer in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&amend.stderr).trim()
        );
        None
    }
}

/// runs via `sh -c` / `cmd /C`; killed after COMMAND_TIMEOUT unless
/// `run_in_background` is set, in which case the child is registered as a
/// background task and polled with `command_output` / stopped with
/// `kill_command`.
fn exec_run_command(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let command_str = match args.get("command").and_then(Value::as_str) {
        Some(c) => c,
        None => return "Error: missing required parameter \"command\"".to_string(),
    };

    let default_cwd = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let cwd = args
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or(&default_cwd);
    let cwd_path = absolute_path(Path::new(cwd));
    let cwd_str = cwd_path.display().to_string();

    if !cwd_path.exists() {
        return format!("Error: working directory does not exist: {cwd_str}");
    }

    let run_in_background = args
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    log::info!("run_command: `{command_str}` in `{cwd_str}` (background: {run_in_background})");

    if run_in_background {
        return start_background_task(command_str, &cwd_path);
    }

    // Co-author attribution: note where HEAD is before a command that looks
    // like it may commit, so a new commit can be detected afterwards. The
    // string check is only a cheap trigger — a false positive costs one
    // `git rev-parse` and nothing else. Background tasks skip the gate: their
    // commits finish after the tool returns, when there is nothing to amend
    // from.
    let may_commit = command_str.contains("git") && command_str.contains("commit");
    let head_before = if may_commit {
        git_head(&cwd_path)
    } else {
        None
    };

    let mut child = match spawn_shell(command_str, &cwd_path) {
        Ok(c) => c,
        Err(err) => return format!("Error: failed to spawn command: {err}"),
    };

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed() >= COMMAND_TIMEOUT {
                    let _ = child.kill();
                    return format!(
                        "Error: command timed out after {} seconds and was killed.",
                        COMMAND_TIMEOUT.as_secs()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(err) => return format!("Error: failed to wait on command: {err}"),
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(err) => return format!("Error: failed to read command output: {err}"),
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    combined.push_str(&String::from_utf8_lossy(&output.stderr));

    // A new commit appeared under this command: make sure it carries the
    // siGit co-author trailer (see `ensure_commit_co_author`).
    if may_commit {
        let head_after = git_head(&cwd_path);
        if head_after.is_some()
            && head_after != head_before
            && let Some(note) = ensure_commit_co_author(&cwd_path)
        {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&note);
        }
    }

    let truncated = if combined.len() > COMMAND_OUTPUT_LIMIT {
        let truncated_str = &combined[..COMMAND_OUTPUT_LIMIT];
        format!("{truncated_str}\n\n… (output truncated at {COMMAND_OUTPUT_LIMIT} bytes)")
    } else {
        combined
    };

    if output.status.success() {
        if truncated.is_empty() {
            format!("Command succeeded (exit code {exit_code}) with no output.")
        } else {
            format!("Exit code {exit_code}:\n{truncated}")
        }
    } else {
        format!("Command failed (exit code {exit_code}):\n{truncated}")
    }
}

// ── background tasks (command_output / kill_command) ─────────────────────────

/// A command started with `run_in_background`. The child is never detached,
/// so background tasks die with the sigit process.
struct BackgroundTask {
    command: String,
    child: std::process::Child,
    /// Combined stdout+stderr drained by the reader threads, shared with them.
    output: Arc<Mutex<TaskOutput>>,
    /// Exit code cached once observed; `-1` when there is no code (killed by
    /// a signal). `None` while the task is still running.
    exit_code: Option<i32>,
    /// Set when the task was stopped via `kill_command`.
    killed: bool,
}

/// Output accumulated for a background task since the last poll.
#[derive(Default)]
struct TaskOutput {
    buf: String,
    /// Whether the oldest output was dropped because `buf` hit the cap.
    dropped: bool,
}

/// Process-global background task table (same pattern as `mcp::MCP`).
fn tasks() -> &'static Mutex<HashMap<u64, BackgroundTask>> {
    static TASKS: OnceLock<Mutex<HashMap<u64, BackgroundTask>>> = OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_tasks() -> std::sync::MutexGuard<'static, HashMap<u64, BackgroundTask>> {
    tasks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn next_task_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Drain a child's stream into the task's shared buffer on a plain std thread,
/// capping the buffer at COMMAND_OUTPUT_LIMIT by dropping the oldest output.
fn spawn_output_reader<R: std::io::Read + Send + 'static>(
    mut stream: R,
    output: Arc<Mutex<TaskOutput>>,
) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&chunk[..n]).into_owned();
                    let mut out = output
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    out.buf.push_str(&text);
                    if out.buf.len() > COMMAND_OUTPUT_LIMIT {
                        let mut cut = out.buf.len() - COMMAND_OUTPUT_LIMIT;
                        while !out.buf.is_char_boundary(cut) {
                            cut += 1;
                        }
                        out.buf.drain(..cut);
                        out.dropped = true;
                    }
                }
            }
        }
    });
}

/// Background branch of `run_command`: spawn, register, return immediately.
fn start_background_task(command_str: &str, cwd_path: &Path) -> String {
    let mut child = match spawn_shell(command_str, cwd_path) {
        Ok(c) => c,
        Err(err) => return format!("Error: failed to spawn command: {err}"),
    };

    let output = Arc::new(Mutex::new(TaskOutput::default()));
    if let Some(stdout) = child.stdout.take() {
        spawn_output_reader(stdout, Arc::clone(&output));
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_output_reader(stderr, Arc::clone(&output));
    }

    let task_id = next_task_id();
    lock_tasks().insert(
        task_id,
        BackgroundTask {
            command: command_str.to_string(),
            child,
            output,
            exit_code: None,
            killed: false,
        },
    );

    format!(
        "Started background task {task_id}: `{command_str}`. Poll its output and status \
         with command_output (task_id: {task_id}); stop it with kill_command. The task is \
         killed when sigit exits."
    )
}

/// Check (and cache) whether a task's child has exited. Returns the exit code,
/// or `None` while it is still running.
fn poll_exit_code(task: &mut BackgroundTask) -> Option<i32> {
    if task.exit_code.is_none()
        && let Ok(Some(status)) = task.child.try_wait()
    {
        task.exit_code = Some(status.code().unwrap_or(-1));
    }
    task.exit_code
}

/// Take everything the task has printed since the last poll, plus whether
/// older output was dropped at the buffer cap.
fn drain_task_output(task: &BackgroundTask) -> (String, bool) {
    let mut out = task
        .output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    (
        std::mem::take(&mut out.buf),
        std::mem::take(&mut out.dropped),
    )
}

fn parse_task_id(arguments: &str) -> Result<u64, String> {
    let args: Value = serde_json::from_str(arguments)
        .map_err(|err| format!("Error: failed to parse arguments: {err}"))?;
    args.get("task_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Error: missing required parameter \"task_id\"".to_string())
}

fn unknown_task(task_id: u64) -> String {
    format!(
        "Error: no background task with id {task_id}. Start one with run_command and \
         run_in_background set to true."
    )
}

fn dropped_note(dropped: bool) -> &'static str {
    if dropped {
        "\n(note: earlier output was dropped after exceeding the 50000-byte buffer)"
    } else {
        ""
    }
}

/// `command_output` tool: output since the last poll + running/exited status.
fn exec_command_output(arguments: &str) -> String {
    let task_id = match parse_task_id(arguments) {
        Ok(id) => id,
        Err(err) => return err,
    };

    let mut map = lock_tasks();
    let Some(task) = map.get_mut(&task_id) else {
        return unknown_task(task_id);
    };

    let was_running = task.exit_code.is_none();
    let exit_code = poll_exit_code(task);
    if was_running && exit_code.is_some() {
        // The child just exited; give the reader threads a moment to flush
        // the final output through the pipes before draining the buffer.
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let (new_output, dropped) = drain_task_output(task);

    let command = &task.command;
    let status = match exit_code {
        None => format!("Task {task_id} (`{command}`) is still running."),
        Some(_) if task.killed => format!("Task {task_id} (`{command}`) was killed."),
        Some(code) => format!("Task {task_id} (`{command}`) exited with code {code}."),
    };

    if new_output.is_empty() {
        format!(
            "{status} No new output since the last check.{}",
            dropped_note(dropped)
        )
    } else {
        format!(
            "{status} New output since the last check:{}\n{new_output}",
            dropped_note(dropped)
        )
    }
}

/// `kill_command` tool: stop a background task and report its output tail.
fn exec_kill_command(arguments: &str) -> String {
    let task_id = match parse_task_id(arguments) {
        Ok(id) => id,
        Err(err) => return err,
    };

    let mut map = lock_tasks();
    let Some(task) = map.get_mut(&task_id) else {
        return unknown_task(task_id);
    };

    if let Some(code) = poll_exit_code(task) {
        let (new_output, dropped) = drain_task_output(task);
        let command = &task.command;
        let tail = if new_output.is_empty() {
            String::new()
        } else {
            format!(" Unread output:\n{new_output}")
        };
        return format!(
            "Task {task_id} (`{command}`) had already exited with code {code}; nothing to \
             kill.{}{tail}",
            dropped_note(dropped)
        );
    }

    if let Err(err) = task.child.kill() {
        return format!("Error: failed to kill task {task_id}: {err}");
    }
    match task.child.wait() {
        Ok(status) => task.exit_code = Some(status.code().unwrap_or(-1)),
        Err(_) => task.exit_code = Some(-1),
    }
    task.killed = true;

    // Let the reader threads flush whatever was in flight before reporting.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let (new_output, dropped) = drain_task_output(task);

    let command = &task.command;
    let tail = if new_output.is_empty() {
        " It produced no unread output.".to_string()
    } else {
        format!(" Last output:\n{new_output}")
    };
    format!(
        "Killed task {task_id} (`{command}`).{}{tail}",
        dropped_note(dropped)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn test_execute_unknown_tool() {
        let result = execute_tool("nonexistent", "{}").await;
        assert!(result.starts_with("Unknown tool:"));
    }

    #[test]
    fn test_read_file_missing_path_param() {
        let result = exec_read_file("{}");
        assert!(result.contains("missing required parameter"));
    }

    #[test]
    fn test_read_file_nonexistent() {
        let result = exec_read_file(r#"{"path": "/tmp/__sigit_no_such_file_42__"}"#);
        assert!(result.contains("does not exist"));
    }

    #[test]
    fn test_read_file_success() {
        let dir = std::env::temp_dir().join("sigit_test_read_file");
        let _ = fs::create_dir_all(&dir);
        let file_path = dir.join("hello.txt");
        fs::write(&file_path, "hello world").unwrap();

        let args = serde_json::json!({ "path": file_path }).to_string();
        let result = exec_read_file(&args);
        assert_eq!(result, "hello world");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_directory_missing_path_param() {
        let result = exec_list_directory("{}");
        assert!(result.contains("missing required parameter"));
    }

    #[test]
    fn test_list_directory_success() {
        let dir = std::env::temp_dir().join("sigit_test_list_dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("subdir")).unwrap();
        fs::write(dir.join("aaa.txt"), "").unwrap();
        fs::write(dir.join("bbb.rs"), "").unwrap();

        let args = serde_json::json!({ "path": dir }).to_string();
        let result = exec_list_directory(&args);

        assert!(result.contains("[DIR]  subdir"));
        assert!(result.contains("[FILE] aaa.txt"));
        assert!(result.contains("[FILE] bbb.rs"));

        // Directories should appear before files.
        let dir_pos = result.find("[DIR]").unwrap();
        let file_pos = result.find("[FILE]").unwrap();
        assert!(dir_pos < file_pos);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_search_files_invalid_regex() {
        let result = exec_search_files(r#"{"pattern": "[invalid", "path": "."}"#);
        assert!(result.contains("invalid regex"));
    }

    #[test]
    fn test_search_files_success() {
        let dir = std::env::temp_dir().join("sigit_test_search");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("code.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        fs::write(dir.join("other.txt"), "no match here\n").unwrap();

        let args = serde_json::json!({
            "pattern": "println",
            "path": dir
        })
        .to_string();
        let result = exec_search_files(&args);

        assert!(result.contains("code.rs:2:"));
        assert!(result.contains("println"));
        assert!(!result.contains("other.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_search_files_no_matches() {
        let dir = std::env::temp_dir().join("sigit_test_search_none");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("empty.txt"), "nothing special").unwrap();

        let args = serde_json::json!({
            "pattern": "zzz_will_not_match_42",
            "path": dir
        })
        .to_string();
        let result = exec_search_files(&args);
        assert!(result.contains("No matches found"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_all_tools_count() {
        let tools = all_tools();
        assert_eq!(tools.len(), 15);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[1].name, "create_directory");
        assert_eq!(tools[2].name, "list_directory");
        assert_eq!(tools[3].name, "search_files");
        assert_eq!(tools[4].name, "read_website");
        assert_eq!(tools[5].name, "create_file");
        assert_eq!(tools[6].name, "edit_file");
        assert_eq!(tools[7].name, "delete_file");
        assert_eq!(tools[8].name, "run_command");
        assert_eq!(tools[9].name, "multi_edit");
        assert_eq!(tools[10].name, "glob");
        assert_eq!(tools[11].name, "write_todos");
        assert_eq!(tools[12].name, "remember");
        assert_eq!(tools[13].name, "command_output");
        assert_eq!(tools[14].name, "kill_command");
    }

    #[test]
    fn test_edit_file_replace_all() {
        let dir = std::env::temp_dir().join("sigit_test_edit_replace_all");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        fs::write(&file, "foo foo foo").unwrap();

        // Without replace_all an ambiguous match is rejected.
        let args =
            serde_json::json!({ "path": &file, "old_text": "foo", "new_text": "bar" }).to_string();
        let result = exec_edit_file(&args);
        assert!(result.contains("appears 3 times"), "{result}");

        // With replace_all every occurrence is changed.
        let args = serde_json::json!({
            "path": &file, "old_text": "foo", "new_text": "bar", "replace_all": true
        })
        .to_string();
        let result = exec_edit_file(&args);
        assert!(result.starts_with("Edited file:"), "{result}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "bar bar bar");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_edit_file_whitespace_hint() {
        let dir = std::env::temp_dir().join("sigit_test_edit_hint");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        fs::write(&file, "line one\n    indented\nline three\n").unwrap();

        // old_text has more indentation than the file, so it isn't a substring,
        // but its trimmed content still locates the intended line.
        let args = serde_json::json!({
            "path": &file, "old_text": "        indented", "new_text": "x"
        })
        .to_string();
        let result = exec_edit_file(&args);
        assert!(result.contains("line 2"), "{result}");
        assert!(result.contains("whitespace"), "{result}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_multi_edit_atomic_on_failure() {
        let dir = std::env::temp_dir().join("sigit_test_multi_edit");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        fs::write(&file, "alpha beta gamma").unwrap();

        // Second edit can't match -> nothing should be written.
        let args = serde_json::json!({
            "path": &file,
            "edits": [
                { "old_text": "alpha", "new_text": "ALPHA" },
                { "old_text": "nope", "new_text": "x" }
            ]
        })
        .to_string();
        let result = exec_multi_edit(&args);
        assert!(result.contains("edit #2 failed"), "{result}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "alpha beta gamma");

        // All-matching batch applies in sequence.
        let args = serde_json::json!({
            "path": &file,
            "edits": [
                { "old_text": "alpha", "new_text": "ALPHA" },
                { "old_text": "gamma", "new_text": "GAMMA" }
            ]
        })
        .to_string();
        let result = exec_multi_edit(&args);
        assert!(result.contains("Applied 2 edits"), "{result}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "ALPHA beta GAMMA");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_glob_to_regex() {
        let re = Regex::new(&glob_to_regex("**/*.rs")).unwrap();
        assert!(re.is_match("src/tools.rs"));
        assert!(re.is_match("main.rs")); // `**/` matches zero directories too
        assert!(!re.is_match("src/tools.txt"));

        let re = Regex::new(&glob_to_regex("*.{ts,tsx}")).unwrap();
        assert!(re.is_match("app.ts"));
        assert!(re.is_match("app.tsx"));
        assert!(!re.is_match("app.js"));
    }

    #[test]
    fn test_glob_tool_success() {
        let dir = std::env::temp_dir().join("sigit_test_glob");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), "").unwrap();
        fs::write(dir.join("src/main.rs"), "").unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();

        let args = serde_json::json!({ "pattern": "**/*.rs", "path": &dir }).to_string();
        let result = exec_glob(&args);
        assert!(result.contains("main.rs"), "{result}");
        assert!(result.contains("lib.rs"), "{result}");
        assert!(!result.contains("Cargo.toml"), "{result}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_search_files_file_glob_filter() {
        let dir = std::env::temp_dir().join("sigit_test_search_glob");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("code.rs"), "needle here\n").unwrap();
        fs::write(dir.join("notes.txt"), "needle here\n").unwrap();

        let args = serde_json::json!({
            "pattern": "needle", "path": &dir, "file_glob": "*.rs"
        })
        .to_string();
        let result = exec_search_files(&args);
        assert!(result.contains("code.rs"), "{result}");
        assert!(!result.contains("notes.txt"), "{result}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_todos_renders_checklist() {
        let args = serde_json::json!({
            "todos": [
                { "content": "Read code", "status": "completed" },
                { "content": "Make change", "status": "in_progress" },
                { "content": "Run tests", "status": "pending" }
            ]
        })
        .to_string();
        let result = exec_write_todos(&args);
        assert!(result.contains("1/3 done"), "{result}");
        assert!(result.contains("[x] Read code"), "{result}");
        assert!(result.contains("[~] Make change"), "{result}");
        assert!(result.contains("[ ] Run tests"), "{result}");
    }

    #[test]
    fn test_remember_appends_to_instruction_file() {
        let dir = std::env::temp_dir().join("sigit_test_remember");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();
        let claude_md = dir.join("CLAUDE.md");
        fs::write(&claude_md, "# Project\n").unwrap();

        let target = crate::instructions::memory_file(&dir);
        // Should pick the existing CLAUDE.md at the repo root.
        assert_eq!(
            target.canonicalize().unwrap(),
            claude_md.canonicalize().unwrap()
        );

        let result = remember_at(&dir, "remembered text");
        assert!(result.contains("remembered"), "{result}");

        let updated = fs::read_to_string(&claude_md).unwrap();
        assert!(updated.contains("## Remembered notes"), "{updated}");
        assert!(updated.contains("- remembered text"), "{updated}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_all_tools_schemas_are_valid_json_objects() {
        for tool in all_tools() {
            assert!(
                tool.parameters_schema.is_object(),
                "schema for {} is not an object",
                tool.name
            );
            let obj = tool.parameters_schema.as_object().unwrap();
            assert!(obj.contains_key("type"));
            assert!(obj.contains_key("properties"));
            assert!(obj.contains_key("required"));
        }
    }

    // ── read_website tests ───────────────────────────────────────────────

    #[test]
    fn test_read_website_missing_url() {
        let result = exec_read_website("{}");
        assert!(result.contains("missing required parameter"));
    }

    #[test]
    fn test_read_website_invalid_scheme() {
        let result = exec_read_website(r#"{"url": "file:///tmp/test.html"}"#);
        assert!(result.contains("url must start with http:// or https://"));
    }

    #[test]
    fn test_read_website_extracts_title_from_html() {
        let body = r#"
            <html>
                <head>
                    <title>Qwen 3.6 27B</title>
                </head>
                <body>
                    <h1>Model card</h1>
                    <p>Large language model.</p>
                </body>
            </html>
        "#;

        let title = Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
            .unwrap()
            .captures(body)
            .and_then(|captures| captures.get(1))
            .map(|m| {
                Regex::new(r"\s+")
                    .unwrap()
                    .replace_all(m.as_str(), " ")
                    .trim()
                    .to_string()
            })
            .filter(|title| !title.is_empty());

        assert_eq!(title.as_deref(), Some("Qwen 3.6 27B"));
    }

    #[test]
    fn test_read_website_metadata_includes_final_url_header() {
        let final_url = "https://huggingface.co/Qwen/Qwen3.6-27B";
        let title = Some("Qwen 3.6 27B".to_string());
        let cleaned = "Model card\nLarge language model.".to_string();

        let mut metadata = vec![format!("URL: {final_url}")];
        if let Some(title) = &title {
            metadata.push(format!("Title: {title}"));
        }

        let body_text = match title {
            Some(_) => cleaned,
            None => cleaned,
        };

        let output = format!("{}\n\n{}", metadata.join("\n"), body_text);

        assert!(output.starts_with("URL: https://huggingface.co/Qwen/Qwen3.6-27B"));
        assert!(output.contains("\nTitle: Qwen 3.6 27B\n\n"));
    }

    // ── create_directory tests ───────────────────────────────────────────

    #[test]
    fn test_create_directory_missing_path() {
        let result = exec_create_directory("{}");
        assert!(result.contains("missing required parameter"));
    }

    #[test]
    fn test_create_directory_success() {
        let dir = std::env::temp_dir()
            .join("sigit_test_create_directory")
            .join("nested")
            .join("child");
        let _ = fs::remove_dir_all(dir.parent().unwrap());

        let args = serde_json::json!({ "path": dir }).to_string();
        let result = exec_create_directory(&args);
        assert!(result.starts_with("Created directory:"), "got: {result}");
        assert!(dir.exists());
        assert!(dir.is_dir());

        let _ = fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn test_create_directory_already_exists() {
        let dir = std::env::temp_dir().join("sigit_test_create_directory_exists");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let args = serde_json::json!({ "path": dir }).to_string();
        let result = exec_create_directory(&args);
        assert!(result.contains("Directory already exists"), "got: {result}");

        let _ = fs::remove_dir_all(&dir);
    }

    // ── create_file tests ────────────────────────────────────────────────

    #[test]
    fn test_create_file_missing_path() {
        let result = exec_create_file(r#"{"content": "hello"}"#);
        assert!(result.contains("missing required parameter"));
    }

    #[test]
    fn test_create_file_missing_content() {
        let result = exec_create_file(r#"{"path": "/tmp/sigit_test_nope.txt"}"#);
        assert!(result.contains("missing required parameter"));
    }

    #[test]
    fn test_create_file_success() {
        let dir = std::env::temp_dir().join("sigit_test_create_file");
        let _ = fs::remove_dir_all(&dir);

        let file_path = dir.join("sub").join("new_file.txt");
        let args = serde_json::json!({
            "path": file_path,
            "content": "hello world"
        })
        .to_string();

        let result = exec_create_file(&args);
        assert!(result.starts_with("Created file:"), "got: {result}");
        assert!(file_path.exists());
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "hello world");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_create_file_already_exists() {
        let dir = std::env::temp_dir().join("sigit_test_create_exists");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file_path = dir.join("existing.txt");
        fs::write(&file_path, "original").unwrap();

        let args = serde_json::json!({
            "path": file_path,
            "content": "overwrite attempt"
        })
        .to_string();

        let result = exec_create_file(&args);
        assert!(result.contains("already exists"), "got: {result}");
        // Original content untouched.
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "original");

        let _ = fs::remove_dir_all(&dir);
    }

    // ── edit_file tests ──────────────────────────────────────────────────

    #[test]
    fn test_edit_file_missing_params() {
        let result = exec_edit_file(r#"{"path": "x"}"#);
        assert!(result.contains("missing required parameter"));

        let result = exec_edit_file(r#"{"path": "x", "old_text": "a"}"#);
        assert!(result.contains("missing required parameter"));
    }

    #[test]
    fn test_edit_file_nonexistent() {
        let result = exec_edit_file(
            r#"{"path": "/tmp/__sigit_no_such__", "old_text": "a", "new_text": "b"}"#,
        );
        assert!(result.contains("does not exist"));
    }

    #[test]
    fn test_edit_file_success() {
        let dir = std::env::temp_dir().join("sigit_test_edit_file");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file_path = dir.join("code.rs");
        fs::write(&file_path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

        let args = serde_json::json!({
            "path": file_path,
            "old_text": "println!(\"hello\")",
            "new_text": "println!(\"world\")"
        })
        .to_string();

        let result = exec_edit_file(&args);
        assert!(result.starts_with("Edited file:"), "got: {result}");

        let updated = fs::read_to_string(&file_path).unwrap();
        assert!(updated.contains("println!(\"world\")"));
        assert!(!updated.contains("println!(\"hello\")"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_edit_file_old_text_not_found() {
        let dir = std::env::temp_dir().join("sigit_test_edit_notfound");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file_path = dir.join("data.txt");
        fs::write(&file_path, "aaa bbb ccc").unwrap();

        let args = serde_json::json!({
            "path": file_path,
            "old_text": "zzz",
            "new_text": "yyy"
        })
        .to_string();

        let result = exec_edit_file(&args);
        assert!(result.contains("old_text not found"), "got: {result}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_edit_file_ambiguous_match() {
        let dir = std::env::temp_dir().join("sigit_test_edit_ambiguous");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file_path = dir.join("repeat.txt");
        fs::write(&file_path, "foo bar foo bar foo").unwrap();

        let args = serde_json::json!({
            "path": file_path,
            "old_text": "foo",
            "new_text": "baz"
        })
        .to_string();

        let result = exec_edit_file(&args);
        assert!(result.contains("appears 3 times"), "got: {result}");
        // File should be unchanged.
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "foo bar foo bar foo"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // ── delete_file tests ────────────────────────────────────────────────

    #[test]
    fn test_delete_file_missing_path() {
        let result = exec_delete_file("{}");
        assert!(
            result.contains("missing required parameter"),
            "got: {result}"
        );
    }

    #[test]
    fn test_delete_file_nonexistent() {
        let result = exec_delete_file(r#"{"path": "/tmp/sigit_test_no_such_file_xyz"}"#);
        assert!(result.contains("does not exist"), "got: {result}");
    }

    #[test]
    fn test_delete_file_success() {
        let dir = std::env::temp_dir().join("sigit_test_delete_file");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let file_path = dir.join("to_delete.txt");
        fs::write(&file_path, "bye").unwrap();
        assert!(file_path.exists());

        let args = serde_json::json!({ "path": file_path }).to_string();
        let result = exec_delete_file(&args);
        assert!(result.contains("Deleted file"), "got: {result}");
        assert!(!file_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_delete_empty_directory() {
        let dir = std::env::temp_dir().join("sigit_test_delete_empty_dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let args = serde_json::json!({ "path": dir }).to_string();
        let result = exec_delete_file(&args);
        assert!(result.contains("Deleted empty directory"), "got: {result}");
        assert!(!dir.exists());
    }

    #[test]
    fn test_delete_nonempty_directory() {
        let dir = std::env::temp_dir().join("sigit_test_delete_nonempty_dir");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("child.txt"), "content").unwrap();

        let args = serde_json::json!({ "path": dir }).to_string();
        let result = exec_delete_file(&args);
        assert!(result.contains("Error"), "got: {result}");
        assert!(dir.exists(), "directory should not have been deleted");

        let _ = fs::remove_dir_all(&dir);
    }

    // ── run_command tests ────────────────────────────────────────────────

    #[test]
    fn test_run_command_missing_command() {
        let result = exec_run_command("{}");
        assert!(
            result.contains("missing required parameter"),
            "got: {result}"
        );
    }

    #[test]
    fn test_run_command_success() {
        let result = exec_run_command(r#"{"command": "echo hello"}"#);
        assert!(result.contains("hello"), "got: {result}");
        assert!(result.contains("Exit code 0"), "got: {result}");
    }

    /// Fresh git repo with one commit, test identity, and signing off (the
    /// developer's global gpgsign must not leak into sandbox commits).
    fn init_test_repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sigit_test_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        test_git(&dir, &["init", "-q", "-b", "main"]);
        test_git(&dir, &["config", "user.name", "Test User"]);
        test_git(&dir, &["config", "user.email", "test@example.com"]);
        test_git(&dir, &["config", "commit.gpgsign", "false"]);
        fs::write(dir.join("file.txt"), "one\n").unwrap();
        test_git(&dir, &["add", "file.txt"]);
        test_git(&dir, &["commit", "-q", "-m", "Initial"]);
        dir
    }

    fn test_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn run_command_appends_co_author_trailer_to_new_commits() {
        let dir = init_test_repo("coauthor_append");
        fs::write(dir.join("file.txt"), "two\n").unwrap();
        // Quote-free command: `cmd /C` does not strip double quotes the way
        // `sh -c` does, so quoted arguments would break on Windows.
        let args = serde_json::json!({
            "command": "git add file.txt && git commit -m Update",
            "cwd": dir.display().to_string(),
        })
        .to_string();

        let result = exec_run_command(&args);
        assert!(result.contains("co-author trailer"), "got: {result}");

        let message = git_stdout(&dir, &["log", "-1", "--format=%B"]).unwrap();
        assert!(
            message.ends_with(COMMIT_CO_AUTHOR_TRAILER),
            "trailer must be the last line: {message:?}"
        );
        assert!(
            message.contains(&format!("\n\n{COMMIT_CO_AUTHOR_TRAILER}")),
            "trailer needs a blank line before it for GitHub to detect it: {message:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_command_keeps_existing_co_author_trailer() {
        let dir = init_test_repo("coauthor_present");
        fs::write(dir.join("file.txt"), "two\n").unwrap();
        // The trailer contains spaces and angle brackets, which `cmd /C`
        // mis-tokenizes (`<` is redirection), so create the trailer-carrying
        // commit with direct git args and let run_command amend it without
        // editing: HEAD changes, the message already has the trailer, and the
        // gate must leave it alone.
        test_git(&dir, &["add", "file.txt"]);
        test_git(
            &dir,
            &[
                "commit",
                "-q",
                "-m",
                &format!("Update file\n\n{COMMIT_CO_AUTHOR_TRAILER}"),
            ],
        );
        let args = serde_json::json!({
            "command": "git commit --amend --no-edit",
            "cwd": dir.display().to_string(),
        })
        .to_string();

        let result = exec_run_command(&args);
        assert!(
            !result.contains("[siGit Code]"),
            "no amend expected: {result}"
        );

        let message = git_stdout(&dir, &["log", "-1", "--format=%B"]).unwrap();
        assert_eq!(
            message.matches("Co-Authored-By: siGit Code").count(),
            1,
            "trailer must not be duplicated: {message:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_command_never_amends_pushed_commits() {
        let dir = init_test_repo("coauthor_pushed");
        let remote = std::env::temp_dir().join(format!(
            "sigit_test_coauthor_remote_{}.git",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&remote);
        fs::create_dir_all(&remote).unwrap();
        test_git(&remote, &["init", "-q", "--bare"]);
        test_git(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);

        fs::write(dir.join("file.txt"), "two\n").unwrap();
        let args = serde_json::json!({
            "command": "git add file.txt && git commit -m Update && git push -q origin main",
            "cwd": dir.display().to_string(),
        })
        .to_string();

        let result = exec_run_command(&args);
        assert!(
            !result.contains("[siGit Code]"),
            "no amend expected: {result}"
        );

        // Already on the remote when the gate ran, so it must be untouched.
        let message = git_stdout(&dir, &["log", "-1", "--format=%B"]).unwrap();
        assert!(
            !message.contains("Co-Authored-By"),
            "pushed commit must not be rewritten: {message:?}"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&remote);
    }

    #[test]
    fn test_run_command_failure() {
        #[cfg(unix)]
        let command = "false";
        #[cfg(windows)]
        let command = "exit /b 1";

        let args = serde_json::json!({ "command": command }).to_string();
        let result = exec_run_command(&args);
        assert!(result.contains("failed"), "got: {result}");
    }

    #[test]
    fn test_run_command_with_cwd() {
        let dir = std::env::temp_dir().join("sigit_test_run_cmd_cwd");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        #[cfg(unix)]
        let command = "pwd";
        #[cfg(windows)]
        let command = "cd";

        let args = serde_json::json!({
            "command": command,
            "cwd": dir
        })
        .to_string();
        let result = exec_run_command(&args);
        // The output should contain the temp dir path.
        assert!(
            result.contains(&dir.to_string_lossy().to_string()),
            "got: {result}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_command_bad_cwd() {
        let missing_dir = std::env::temp_dir().join("sigit_no_such_dir_xyz");
        let _ = fs::remove_dir_all(&missing_dir);

        let args = serde_json::json!({
            "command": "echo hi",
            "cwd": missing_dir
        })
        .to_string();
        let result = exec_run_command(&args);
        assert!(result.contains("does not exist"), "got: {result}");
    }

    #[test]
    fn test_run_command_captures_stderr() {
        #[cfg(unix)]
        let command = "echo err >&2";
        #[cfg(windows)]
        let command = "echo err 1>&2";

        let args = serde_json::json!({ "command": command }).to_string();
        let result = exec_run_command(&args);
        assert!(result.contains("err"), "got: {result}");
    }

    // ── background command tests ─────────────────────────────────────────

    /// Extract the task id from a "Started background task {id}: ..." result.
    fn background_task_id(result: &str) -> u64 {
        let digits: String = result
            .strip_prefix("Started background task ")
            .unwrap_or_else(|| panic!("unexpected spawn result: {result}"))
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().expect("task id")
    }

    /// Whether the task's child has exited, without draining its output.
    fn background_task_exited(task_id: u64) -> bool {
        let mut map = lock_tasks();
        match map.get_mut(&task_id) {
            Some(task) => poll_exit_code(task).is_some(),
            None => true,
        }
    }

    /// Wait (bounded) for a background task's child to exit.
    fn wait_for_background_exit(task_id: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !background_task_exited(task_id) {
            assert!(
                std::time::Instant::now() < deadline,
                "task {task_id} did not exit in time"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn test_run_command_background_lifecycle() {
        #[cfg(unix)]
        let command = "echo start; sleep 2; echo done";
        #[cfg(windows)]
        let command = "echo start&& ping -n 3 127.0.0.1 > nul&& echo done";

        let args = serde_json::json!({
            "command": command,
            "cwd": std::env::temp_dir(),
            "run_in_background": true
        })
        .to_string();
        let spawned = std::time::Instant::now();
        let result = exec_run_command(&args);
        // Spawning must return immediately, not wait the ~2s the command takes.
        assert!(
            spawned.elapsed() < std::time::Duration::from_secs(1),
            "background spawn blocked for {:?}",
            spawned.elapsed()
        );
        assert!(result.contains("command_output"), "got: {result}");
        let task_id = background_task_id(&result);

        // Polling while the command is still sleeping reports it as running.
        let poll_args = serde_json::json!({ "task_id": task_id }).to_string();
        let poll = exec_command_output(&poll_args);
        assert!(poll.contains("still running"), "got: {poll}");
        let mut combined = poll;

        wait_for_background_exit(task_id);
        // Grace period so the reader threads finish draining the pipes.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let final_poll = exec_command_output(&poll_args);
        assert!(
            final_poll.contains("exited with code 0"),
            "got: {final_poll}"
        );
        combined.push_str(&final_poll);

        // Across the polls, all output the command printed was delivered.
        assert!(combined.contains("start"), "got: {combined}");
        assert!(combined.contains("done"), "got: {combined}");
    }

    #[test]
    fn test_kill_command_stops_background_task() {
        #[cfg(unix)]
        let command = "sleep 30";
        #[cfg(windows)]
        let command = "ping -n 31 127.0.0.1 > nul";

        let args = serde_json::json!({
            "command": command,
            "cwd": std::env::temp_dir(),
            "run_in_background": true
        })
        .to_string();
        let result = exec_run_command(&args);
        let task_id = background_task_id(&result);

        let kill_args = serde_json::json!({ "task_id": task_id }).to_string();
        let killed_at = std::time::Instant::now();
        let kill_result = exec_kill_command(&kill_args);
        assert!(
            kill_result.contains(&format!("Killed task {task_id}")),
            "got: {kill_result}"
        );
        // The kill must not wait out the 30s sleep.
        assert!(
            killed_at.elapsed() < std::time::Duration::from_secs(5),
            "kill blocked for {:?}",
            killed_at.elapsed()
        );

        // A later poll reports the task as killed, not still running.
        let poll = exec_command_output(&serde_json::json!({ "task_id": task_id }).to_string());
        assert!(poll.contains("was killed"), "got: {poll}");
    }

    #[test]
    fn test_command_output_unknown_task() {
        let result = exec_command_output(r#"{"task_id": 9999999}"#);
        assert!(
            result.contains("no background task with id"),
            "got: {result}"
        );
    }

    // ── task (subagent) tests ────────────────────────────────────────────

    #[test]
    fn test_subagent_toolset_is_read_only() {
        let specs = subagent_tool_specs();
        let mut names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "glob",
                "list_directory",
                "read_file",
                "read_website",
                "search_files"
            ]
        );

        // No recursion and no mutating tools, ever.
        assert!(!names.contains(&TASK_TOOL_NAME));
        for banned in [
            "create_file",
            "create_directory",
            "edit_file",
            "multi_edit",
            "delete_file",
            "run_command",
            "write_todos",
            "remember",
        ] {
            assert!(!names.contains(&banned), "{banned} leaked into subagent");
        }

        // Every spec came from `all_tools()` (schemas intact).
        for spec in &specs {
            assert!(
                serde_json::from_str::<Value>(&spec.parameters_schema)
                    .unwrap()
                    .is_object()
            );
        }
    }

    #[tokio::test]
    async fn test_task_reports_unavailable_without_factory() {
        let args = serde_json::json!({
            "description": "look around",
            "prompt": "What is in the current directory?"
        })
        .to_string();
        // `None` is exactly what `exec_task` passes before any surface has
        // registered the process-global factory.
        let result = exec_task_with(&args, None).await;
        assert!(
            result.contains("not available on-device yet"),
            "got: {result}"
        );
    }

    #[test]
    fn test_kill_command_unknown_task() {
        let result = exec_kill_command(r#"{"task_id": 9999999}"#);
        assert!(
            result.contains("no background task with id"),
            "got: {result}"
        );
    }

    #[tokio::test]
    async fn test_task_missing_prompt() {
        let result = exec_task_with(r#"{"description": "x"}"#, None).await;
        assert!(
            result.contains("missing required parameter \"prompt\""),
            "got: {result}"
        );
    }

    // ── web_search tests ─────────────────────────────────────────────────

    #[test]
    fn test_web_search_tool_spec_matches_the_cloud_mcp_tools_contract() {
        let spec = web_search_tool_spec();
        assert_eq!(spec.name, "web_search");
        let schema: Value = serde_json::from_str(&spec.parameters_schema).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["query"]));
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["count"].is_object());
    }

    #[test]
    fn test_is_web_search_delegate() {
        assert!(is_web_search_delegate("mcp__sigit__web_search"));
        assert!(!is_web_search_delegate("web_search"));
        assert!(!is_web_search_delegate("mcp__other__web_search"));
    }

    #[test]
    fn test_web_search_unavailable_without_mcp_init() {
        // `mcp::init()` is never called anywhere in this test binary (it does
        // real network I/O and is a one-shot `OnceLock`, same constraint as
        // `SUBAGENT_FACTORY`), so the official server's tools — including
        // web_search — are never discovered here. This proves the
        // conditional-advertisement gate fails closed, matching a
        // signed-out/MCP-disabled session, rather than defaulting to "always
        // offered".
        assert!(!web_search_available());
    }

    #[tokio::test]
    async fn test_web_search_delegates_to_mcp_and_degrades_gracefully_when_uninitialized() {
        // With MCP uninitialized (see the test above), `mcp::call_tool`
        // returns its own in-band error string rather than panicking — proves
        // execute_tool's WEB_SEARCH_TOOL_NAME arm actually reaches
        // mcp::call_tool(WEB_SEARCH_MCP_DELEGATE, ...) instead of, say, silently
        // no-op'ing or hitting the `Unknown tool` fallback.
        let args = serde_json::json!({ "query": "siGit Code" }).to_string();
        let result = execute_tool(WEB_SEARCH_TOOL_NAME, &args).await;
        assert_eq!(result, "Error: MCP is not initialized.");
    }

    #[test]
    fn test_command_output_missing_task_id() {
        let result = exec_command_output("{}");
        assert!(
            result.contains("missing required parameter"),
            "got: {result}"
        );
    }

    #[test]
    fn test_background_output_is_capped() {
        // Print well over COMMAND_OUTPUT_LIMIT (50 000) bytes without polling,
        // so the buffer must drop the oldest output and note the truncation.
        #[cfg(unix)]
        let command = r#"i=0; while [ $i -lt 200 ]; do printf 'line-%04d %s\n' "$i" \
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; \
             i=$((i+1)); done"#;
        // Quote-free on purpose: `cmd /C` does not strip double quotes the
        // way `sh -c` does, so a quoted PowerShell one-liner gets mangled.
        // A cmd-native for /L loop needs no quoting at all.
        #[cfg(windows)]
        let command = &format!(
            "for /L %i in (1,1,200) do @echo line-%i-end {}",
            "a".repeat(330)
        );

        let args = serde_json::json!({
            "command": command,
            "cwd": std::env::temp_dir(),
            "run_in_background": true
        })
        .to_string();
        let result = exec_run_command(&args);
        let task_id = background_task_id(&result);

        wait_for_background_exit(task_id);
        // Grace period so the reader threads finish draining the pipes.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let poll = exec_command_output(&serde_json::json!({ "task_id": task_id }).to_string());
        assert!(poll.contains("exited with code 0"), "got: {poll}");
        assert!(
            poll.contains("earlier output was dropped"),
            "expected truncation note, got: {poll}"
        );
        // The oldest lines were dropped; the newest survived. The two
        // platforms number lines differently (printf %04d vs cmd's %i).
        #[cfg(unix)]
        let (oldest, newest) = ("line-0000", "line-0199");
        #[cfg(windows)]
        let (oldest, newest) = ("line-1-end", "line-200-end");
        assert!(!poll.contains(oldest), "oldest output not dropped");
        assert!(poll.contains(newest), "newest output missing");
        // Buffer stayed within the cap, plus the framing: the status line
        // (which quotes the command) and the truncation note.
        assert!(
            poll.len() <= COMMAND_OUTPUT_LIMIT + command.len() + 500,
            "poll result too large: {} bytes",
            poll.len()
        );
    }

    // ── task (subagent) end-to-end against a scripted endpoint ──────────

    /// A completion whose assistant message requests one tool call.
    fn completion_tool_call(id: &str, name: &str, arguments: &str) -> String {
        serde_json::json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": serde_json::Value::Null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }],
            }}]
        })
        .to_string()
    }

    /// A completion whose assistant message is a plain text answer.
    fn completion_text(text: &str) -> String {
        serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": text}}]
        })
        .to_string()
    }

    /// Minimal scripted OpenAI-compatible endpoint (same pattern as
    /// `tests/acp_permissions.rs`): serves one canned chat-completion JSON body
    /// per request and records each request body. The subagent loop passes
    /// `sink: None`, so the backend takes the non-streaming path and expects
    /// plain JSON rather than SSE.
    fn start_scripted_endpoint(responses: Vec<String>) -> (u16, Arc<std::sync::Mutex<Vec<Value>>>) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind endpoint");
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
        let recorded = Arc::clone(&requests);
        let queue = std::sync::Mutex::new(std::collections::VecDeque::from(responses));

        std::thread::spawn(move || {
            // `connection: close` means one request per connection, matching
            // the backend's serial completion requests.
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(clone) => clone,
                    Err(_) => continue,
                });
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        content_length = length.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                if let Ok(request) = serde_json::from_slice::<Value>(&body) {
                    recorded.lock().unwrap().push(request);
                }
                let payload = queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| completion_text("out of scripted responses"));
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        (port, requests)
    }

    #[tokio::test]
    async fn test_task_runs_subagent_end_to_end() {
        // A file only the subagent's read_file call can surface.
        let dir = std::env::temp_dir().join("sigit_test_subagent_e2e");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        fs::write(&file, "subagent secret: 4217").unwrap();

        // Script: one read_file tool call, then a final text answer.
        let (port, requests) = start_scripted_endpoint(vec![
            completion_tool_call(
                "call_1",
                "read_file",
                &serde_json::json!({ "path": file }).to_string(),
            ),
            completion_text("The file contains the number 4217."),
        ]);

        // Register the real process-global factory, pointing a fresh
        // OpenAiBackend at the scripted endpoint — exactly what the surfaces
        // do at startup. This is the only test that touches the OnceLock.
        set_subagent_factory(Box::new(move || {
            Some(Arc::new(crate::backend::OpenAiBackend::new(
                format!("http://127.0.0.1:{port}"),
                "test-key",
                "scripted-model",
                Some(SUBAGENT_SYSTEM_PROMPT.to_string()),
            )) as Arc<dyn InferenceBackend>)
        }));
        assert!(subagent_available());

        let args = serde_json::json!({
            "description": "read the notes file",
            "prompt": format!("What number is recorded in {}?", file.display()),
        })
        .to_string();
        let result = execute_tool(TASK_TOOL_NAME, &args).await;

        // Only the subagent's final text comes back.
        assert_eq!(result, "The file contains the number 4217.");

        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 2, "expected exactly two completions");

        // The subagent conversation is fresh (subagent system prompt) and was
        // offered only the read-only toolset.
        let first = &recorded[0];
        assert_eq!(first["messages"][0]["role"], "system");
        assert!(
            first["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("research subagent")
        );
        let offered: Vec<&str> = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        for name in &offered {
            assert!(
                SUBAGENT_TOOL_NAMES.contains(name),
                "non-read-only tool offered: {name}"
            );
        }
        assert!(!offered.contains(&TASK_TOOL_NAME));

        // The read-only tool actually executed: its output travelled back to
        // the endpoint as a tool result on the second request.
        let messages = recorded[1]["messages"].as_array().unwrap();
        let tool_message = messages
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("no tool result in second request");
        assert_eq!(tool_message["tool_call_id"], "call_1");
        assert!(
            tool_message["content"]
                .as_str()
                .unwrap()
                .contains("subagent secret: 4217")
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
