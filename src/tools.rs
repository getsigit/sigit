//! Tool definitions and execution for the siGit Code.
//!
//! Each tool has:
//! - A schema (JSON Schema) that describes its parameters for the LLM
//! - An execution function that runs the tool and returns a string result
//!
//! # Dependencies
//!
//! This module requires `serde_json` and `regex` crates in `Cargo.toml`:
//! ```toml
//! serde_json = "1"
//! regex = "1"
//! ```
//!
//! # Write Tools
//!
//! - `create_directory` — create a directory and any missing parent directories
//! - `create_file` — create a new file (fails if it already exists)
//! - `edit_file` — replace an exact old-text span with new text in an existing file
//! - `delete_file` — delete a file or empty directory at the given path
//!
//! # Web Tools
//!
//! - `read_website` — fetch a web page and return readable text content
//!
//! # Shell Tools
//!
//! - `run_command` — run shell commands, including git porcelain and plumbing commands

use regex::Regex;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const WEBSITE_READ_CHAR_LIMIT: usize = 20_000;
const WEBSITE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const WEBSITE_USER_AGENT: &str =
    "siGit/0.1 (+https://github.com/getsigit/sigit; website-reading tool)";

/// Maximum characters returned from `read_file` before truncation.
const READ_FILE_CHAR_LIMIT: usize = 10_000;

/// Maximum number of matching lines returned from `search_files`.
const SEARCH_FILES_MATCH_LIMIT: usize = 50;

// ── Tool schemas ─────────────────────────────────────────────────────────────

/// A tool definition with its JSON Schema and metadata for the LLM.
pub struct AgentTool {
    /// Machine-readable tool name (e.g. `"read_file"`).
    pub name: &'static str,
    /// Human-readable description shown to the LLM.
    pub description: &'static str,
    /// JSON Schema describing the tool's parameters.
    pub parameters_schema: Value,
}

/// Return all available agent tools.
pub fn all_tools() -> Vec<AgentTool> {
    vec![
        AgentTool {
            name: "read_file",
            description: "Read the contents of a file at the given path. \
                           Prefer an absolute path when possible. Returns the file text, \
                           or an error message if the file cannot be read. Output is \
                           truncated to 10 000 characters.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file to read."
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
                           files and hidden directories. Limited to the first 50 matches.",
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
                           new text (new_text). Prefer an absolute path when possible. The \
                           old_text must appear exactly once in the file. Use read_file first \
                           to see the current content and identify the exact text to replace. \
                           To append to a file, match the last few lines as old_text and \
                           include them plus the new content as new_text.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the existing file to edit."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "The exact text span to find and replace. Must match exactly once."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "The replacement text that will take the place of old_text."
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
                           The command runs in the given working directory (defaults to \".\"). \
                           Prefer an absolute working directory when possible. Use this for \
                           build tools (cargo, npm, make), package managers, linters, test \
                           runners, and git commands, including git init, porcelain commands \
                           like status/add/commit/checkout, and plumbing commands like \
                           rev-parse, hash-object, update-ref, and cat-file. If the user asks \
                           for a new repo or scaffold, it is fine to use this for `git init` \
                           and normal repo setup steps. In smbCloud repos, prefer existing \
                           workspace commands, Rails conventions, and deploy flows over \
                           inventing new command sequences. Commands that run indefinitely \
                           (servers, watchers) will be killed after 120 seconds.",
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
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
    ]
}

// ── Tool execution ───────────────────────────────────────────────────────────

/// Execute a tool by name with the given JSON arguments string.
///
/// Returns the tool output as a human-readable string. Errors are returned as
/// descriptive strings rather than panicking.
pub fn execute_tool(name: &str, arguments: &str) -> String {
    match name {
        "read_file" => exec_read_file(arguments),
        "list_directory" => exec_list_directory(arguments),
        "search_files" => exec_search_files(arguments),
        "read_website" => exec_read_website(arguments),
        "create_directory" => exec_create_directory(arguments),
        "create_file" => exec_create_file(arguments),
        "edit_file" => exec_edit_file(arguments),
        "delete_file" => exec_delete_file(arguments),
        "run_command" => exec_run_command(arguments),
        _ => format!("Unknown tool: {name}"),
    }
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

/// Read the contents of a single file, truncating at [`READ_FILE_CHAR_LIMIT`].
fn exec_read_file(arguments: &str) -> String {
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

    if !absolute_path.is_file() {
        return format!("Error: path is not a file: {absolute_path_str}");
    }

    match fs::read_to_string(&absolute_path) {
        Ok(contents) => {
            if contents.len() > READ_FILE_CHAR_LIMIT {
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

/// List directory entries, directories first, sorted alphabetically.
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

    // Directories first, then files.
    dirs.extend(files);

    if dirs.is_empty() {
        return format!("(empty directory: {absolute_path_str})");
    }

    dirs.join("\n")
}

// ── search_files ─────────────────────────────────────────────────────────────

/// Recursively search files for a regex pattern, returning matching lines.
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
    walk_and_search(&absolute_root, &re, &mut matches);

    if matches.is_empty() {
        return format!("No matches found for pattern: {pattern_str}");
    }

    let total = matches.len();
    if total > SEARCH_FILES_MATCH_LIMIT {
        matches.truncate(SEARCH_FILES_MATCH_LIMIT);
        matches.push(format!(
            "\n--- truncated (showing {SEARCH_FILES_MATCH_LIMIT} of {total} matches) ---"
        ));
    }

    matches.join("\n")
}

/// Recursively walk a directory and collect regex matches.
///
/// Skips hidden directories (names starting with `.`) and binary files.
/// Stops collecting once the match list reaches a generous internal cap (2×
/// the public limit) to avoid unbounded work.
fn walk_and_search(dir: &Path, re: &Regex, matches: &mut Vec<String>) {
    // Internal cap to avoid scanning the entire filesystem.
    const WALK_CAP: usize = SEARCH_FILES_MATCH_LIMIT * 2;

    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    // Collect and sort for deterministic output.
    let mut sorted: Vec<fs::DirEntry> = entries.filter_map(Result::ok).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        if matches.len() >= WALK_CAP {
            return;
        }

        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden entries.
        if name_str.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            walk_and_search(&path, re, matches);
        } else if path.is_file() {
            search_file(&path, re, matches);
        }
    }
}

/// Search a single file line-by-line for the regex pattern.
///
/// Skips files that cannot be read as UTF-8 (assumed binary).
fn search_file(path: &Path, re: &Regex, matches: &mut Vec<String>) {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        // Skip binary / unreadable files silently.
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

/// ── read_website ─────────────────────────────────────────────────────────────

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

/// ── create_directory ─────────────────────────────────────────────────────────

/// Create a directory and any missing parent directories.
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

/// ── create_file ──────────────────────────────────────────────────────────────

/// Create a new file with the provided content.
///
/// Parent directories are created automatically. Fails if the file already
/// exists to prevent accidental overwrites — the LLM should use `edit_file`
/// for existing files.
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

    // Create parent directories if needed.
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

// ── edit_file ────────────────────────────────────────────────────────────────

/// Edit an existing file by replacing an exact occurrence of `old_text` with
/// `new_text`.
///
/// The `old_text` must appear **exactly once** in the file. This prevents
/// ambiguous edits and forces the LLM to read the file first to get the exact
/// text span.
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

    // Count occurrences to give a clear error message.
    let occurrences = contents.matches(old_text).count();

    if occurrences == 0 {
        return format!(
            "Error: old_text not found in {absolute_path_str}. \
             Use read_file to see the current content and copy the exact text to replace."
        );
    }

    if occurrences > 1 {
        return format!(
            "Error: old_text appears {occurrences} times in {absolute_path_str}. \
             Include more surrounding context in old_text so it matches exactly once."
        );
    }

    let updated = contents.replacen(old_text, new_text, 1);

    match fs::write(&absolute_path, &updated) {
        Ok(()) => format!(
            "Edited file: {absolute_path_str} ({} bytes written)",
            updated.len()
        ),
        Err(err) => format!("Error: could not write file: {err}"),
    }
}

// ── delete_file ──────────────────────────────────────────────────────────────

/// Delete a file or empty directory at the given path.
///
/// Refuses to remove non-empty directories to guard against accidental
/// recursive deletes.
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

/// Maximum time a command is allowed to run before being killed.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Maximum bytes of combined output returned from a command.
const COMMAND_OUTPUT_LIMIT: usize = 50_000;

/// Run a shell command and return its combined stdout + stderr output.
///
/// The command is executed via `sh -c` (Unix) or `cmd /C` (Windows) so shell
/// features like pipes, redirects, and chaining work out of the box.
///
/// Long-running commands are killed after [`COMMAND_TIMEOUT`] seconds.
fn exec_run_command(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let command_str = match args.get("command").and_then(Value::as_str) {
        Some(c) => c,
        None => return "Error: missing required parameter \"command\"".to_string(),
    };

    let cwd = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let cwd_path = absolute_path(Path::new(cwd));
    let cwd_str = cwd_path.display().to_string();

    if !cwd_path.exists() {
        return format!("Error: working directory does not exist: {cwd_str}");
    }

    log::info!("run_command: `{command_str}` in `{cwd_str}`");

    #[cfg(unix)]
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command_str)
        .current_dir(&cwd_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => return format!("Error: failed to spawn command: {err}"),
    };

    #[cfg(windows)]
    let mut child = match Command::new("cmd")
        .arg("/C")
        .arg(command_str)
        .current_dir(&cwd_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => return format!("Error: failed to spawn command: {err}"),
    };

    // Wait with a timeout.
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

    // Truncate if output is huge.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_execute_unknown_tool() {
        let result = execute_tool("nonexistent", "{}");
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
        assert_eq!(tools.len(), 9);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[1].name, "create_directory");
        assert_eq!(tools[2].name, "list_directory");
        assert_eq!(tools[3].name, "search_files");
        assert_eq!(tools[4].name, "read_website");
        assert_eq!(tools[5].name, "create_file");
        assert_eq!(tools[6].name, "edit_file");
        assert_eq!(tools[7].name, "delete_file");
        assert_eq!(tools[8].name, "run_command");
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
}
