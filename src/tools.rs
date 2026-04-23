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
//! - `create_file` — create a new file (fails if it already exists)
//! - `edit_file` — replace an exact old-text span with new text in an existing file
//! - `delete_file` — delete a file or empty directory at the given path
//!
//! # Shell Tools
//!
//! - `run_command` — run a shell command and return its combined stdout/stderr output

use regex::Regex;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;

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
                           Returns the file text, or an error message if the file cannot be read. \
                           Output is truncated to 10 000 characters.",
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
            name: "list_directory",
            description: "List files and directories at the given path. \
                           Each entry is prefixed with [DIR] or [FILE]. \
                           Directories are listed first, sorted alphabetically.",
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
                           Returns matching lines in `file:line_number: content` format. \
                           Skips binary files and hidden directories. \
                           Limited to the first 50 matches.",
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
            name: "create_file",
            description: "Create a new file at the given path with the provided content. \
                           Parent directories are created automatically if they do not exist. \
                           Fails if the file already exists — use edit_file to modify existing files.",
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
                           new text (new_text). The old_text must appear exactly once in the file. \
                           Use read_file first to see the current content and identify the exact \
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
                           Refuses to delete non-empty directories to prevent accidental data loss. \
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
                           Use this for build tools (cargo, npm, make), version control (git), \
                           package managers, linters, test runners, and other CLI tasks. \
                           Commands that run indefinitely (servers, watchers) will be killed \
                           after 120 seconds.",
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute (e.g. \"cargo update\", \"git status\")."
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
        "create_file" => exec_create_file(arguments),
        "edit_file" => exec_edit_file(arguments),
        "delete_file" => exec_delete_file(arguments),
        "run_command" => exec_run_command(arguments),
        _ => format!("Unknown tool: {name}"),
    }
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

    if !path.exists() {
        return format!("Error: path does not exist: {path_str}");
    }

    if !path.is_file() {
        return format!("Error: path is not a file: {path_str}");
    }

    match fs::read_to_string(path) {
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

    if !path.exists() {
        return format!("Error: path does not exist: {path_str}");
    }

    if !path.is_dir() {
        return format!("Error: path is not a directory: {path_str}");
    }

    let entries = match fs::read_dir(path) {
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
        return format!("(empty directory: {path_str})");
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

    if !root.exists() {
        return format!("Error: path does not exist: {root_str}");
    }

    if !root.is_dir() {
        return format!("Error: path is not a directory: {root_str}");
    }

    let mut matches: Vec<String> = Vec::new();
    walk_and_search(root, &re, &mut matches);

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

    let display_path = path.display();

    for (line_idx, line) in contents.lines().enumerate() {
        if re.is_match(line) {
            let line_number = line_idx + 1;
            matches.push(format!("{display_path}:{line_number}: {line}"));
        }
    }
}

// ── create_file ──────────────────────────────────────────────────────────────

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

    if path.exists() {
        return format!(
            "Error: file already exists: {path_str} — use edit_file to modify existing files"
        );
    }

    // Create parent directories if needed.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
        && let Err(err) = fs::create_dir_all(parent)
    {
        return format!("Error: could not create parent directories: {err}");
    }

    match fs::write(path, content) {
        Ok(()) => format!("Created file: {path_str} ({} bytes)", content.len()),
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

    if !path.exists() {
        return format!("Error: file does not exist: {path_str} — use create_file for new files");
    }

    if !path.is_file() {
        return format!("Error: path is not a file: {path_str}");
    }

    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => return format!("Error: could not read file: {err}"),
    };

    // Count occurrences to give a clear error message.
    let occurrences = contents.matches(old_text).count();

    if occurrences == 0 {
        return format!(
            "Error: old_text not found in {path_str}. \
             Use read_file to see the current content and copy the exact text to replace."
        );
    }

    if occurrences > 1 {
        return format!(
            "Error: old_text appears {occurrences} times in {path_str}. \
             Include more surrounding context in old_text so it matches exactly once."
        );
    }

    let updated = contents.replacen(old_text, new_text, 1);

    match fs::write(path, &updated) {
        Ok(()) => format!("Edited file: {path_str} ({} bytes written)", updated.len()),
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

    if !path.exists() {
        return format!("Error: path does not exist: {path_str}");
    }

    if path.is_dir() {
        match fs::remove_dir(path) {
            Ok(()) => format!("Deleted empty directory: {path_str}"),
            Err(err) => format!(
                "Error: could not delete directory: {err}. \
                 Only empty directories can be deleted."
            ),
        }
    } else {
        match fs::remove_file(path) {
            Ok(()) => format!("Deleted file: {path_str}"),
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

    let cwd_path = Path::new(cwd);
    if !cwd_path.exists() {
        return format!("Error: working directory does not exist: {cwd}");
    }

    log::info!("run_command: `{command_str}` in `{cwd}`");

    #[cfg(unix)]
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command_str)
        .current_dir(cwd_path)
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
        .current_dir(cwd_path)
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

        let args = format!(r#"{{"path": "{}"}}"#, file_path.display());
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

        let args = format!(r#"{{"path": "{}"}}"#, dir.display());
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

        let args = format!(r#"{{"pattern": "println", "path": "{}"}}"#, dir.display());
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

        let args = format!(
            r#"{{"pattern": "zzz_will_not_match_42", "path": "{}"}}"#,
            dir.display()
        );
        let result = exec_search_files(&args);
        assert!(result.contains("No matches found"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_all_tools_count() {
        let tools = all_tools();
        assert_eq!(tools.len(), 7);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[1].name, "list_directory");
        assert_eq!(tools[2].name, "search_files");
        assert_eq!(tools[3].name, "create_file");
        assert_eq!(tools[4].name, "edit_file");
        assert_eq!(tools[5].name, "delete_file");
        assert_eq!(tools[6].name, "run_command");
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
        let args = format!(
            r#"{{"path": "{}", "content": "hello world"}}"#,
            file_path.display()
        );

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

        let args = format!(
            r#"{{"path": "{}", "content": "overwrite attempt"}}"#,
            file_path.display()
        );

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

        let args = format!(
            r#"{{"path": "{}", "old_text": "println!(\"hello\")", "new_text": "println!(\"world\")"}}"#,
            file_path.display()
        );

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

        let args = format!(
            r#"{{"path": "{}", "old_text": "zzz", "new_text": "yyy"}}"#,
            file_path.display()
        );

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

        let args = format!(
            r#"{{"path": "{}", "old_text": "foo", "new_text": "baz"}}"#,
            file_path.display()
        );

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

        let args = format!(r#"{{"path": "{}"}}"#, file_path.display());
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

        let args = format!(r#"{{"path": "{}"}}"#, dir.display());
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

        let args = format!(r#"{{"path": "{}"}}"#, dir.display());
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
        let result = exec_run_command(r#"{"command": "false"}"#);
        assert!(result.contains("failed"), "got: {result}");
    }

    #[test]
    fn test_run_command_with_cwd() {
        let dir = std::env::temp_dir().join("sigit_test_run_cmd_cwd");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let args = format!(r#"{{"command": "pwd", "cwd": "{}"}}"#, dir.display());
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
        let result =
            exec_run_command(r#"{"command": "echo hi", "cwd": "/tmp/sigit_no_such_dir_xyz"}"#);
        assert!(result.contains("does not exist"), "got: {result}");
    }

    #[test]
    fn test_run_command_captures_stderr() {
        let result = exec_run_command(r#"{"command": "echo err >&2"}"#);
        assert!(result.contains("err"), "got: {result}");
    }
}
