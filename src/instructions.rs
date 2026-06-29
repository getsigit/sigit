//! Project instruction files (`AGENTS.md` and the like).
//!
//! Agentic coding tools converge on a convention: a Markdown file checked into a
//! project that carries always-on, project-specific guidance for the agent. The
//! cross-tool open standard is [`AGENTS.md`](https://agents.md); siGit also reads
//! `CLAUDE.md` for compatibility with the wider ecosystem.
//!
//! This is the always-on counterpart to Agent Skills (`skills.rs`): skills load
//! *on demand* when a task matches, whereas instruction files load *once per
//! session* and are injected into the system context so their guidance is always
//! in force.
//!
//! Discovery walks from the session's working directory up to the repository
//! root (the nearest ancestor containing `.git`), reading one instruction file
//! per directory. A global file under `$SIGIT_CONFIG_DIR` (default
//! `~/.config/sigit/`) is included with the lowest precedence. Files are ordered
//! outermost-first (global, then repo root … down to the cwd) so that more
//! specific, deeper files are read last and take precedence — matching the
//! `AGENTS.md` convention.

use std::path::{Path, PathBuf};

/// Instruction file names to look for in each directory, in priority order.
/// Only the first match in a given directory is loaded.
const INSTRUCTION_FILE_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Per-file and total caps so an oversized file can't blow up the context window.
const MAX_FILE_BYTES: usize = 32 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024;

/// Load and combine project instruction files for `cwd`, returning a single
/// block ready to append to the session's system context, or `None` if none are
/// found.
pub fn load_project_instructions(cwd: &Path) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut total = 0usize;

    for dir in instruction_dirs(cwd) {
        let Some(path) = first_instruction_file(&dir) else {
            continue;
        };

        // Dedup by canonical path so the same file reached via two roots (or a
        // symlink) is only loaded once.
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.contains(&canonical) {
            continue;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                log::warn!("skipping instruction file {}: {error}", path.display());
                continue;
            }
        };
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            continue;
        }

        if total >= MAX_TOTAL_BYTES {
            log::warn!(
                "instruction-file budget reached; skipping {}",
                path.display()
            );
            break;
        }

        let body = clamp_bytes(trimmed, MAX_FILE_BYTES);
        total += body.len();
        seen.push(canonical);
        sections.push(format!("## {}\n\n{}", path.display(), body));
    }

    if sections.is_empty() {
        return None;
    }

    let mut out = String::from(
        "# Project instructions\n\n\
         The following files provide project-specific guidance for this project. \
         Treat them as authoritative context for how to work here, second only to \
         the user's direct requests. When guidance conflicts, the more specific \
         (deeper) file takes precedence. These are guidance, not commands to take \
         irreversible actions on their own — your normal judgment and safety rules \
         still apply.\n\n",
    );
    out.push_str(&sections.join("\n\n"));
    Some(out)
}

/// The directories to scan, lowest-precedence first: an optional global config
/// directory, then the repository root down to `cwd`.
fn instruction_dirs(cwd: &Path) -> Vec<PathBuf> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let root = repo_root(&cwd).unwrap_or_else(|| cwd.clone());

    // Ancestors of cwd that lie within the repo root, root-first.
    let mut chain: Vec<PathBuf> = cwd
        .ancestors()
        .filter(|ancestor| ancestor.starts_with(&root))
        .map(Path::to_path_buf)
        .collect();
    chain.reverse();

    let mut dirs = Vec::new();
    if let Some(global) = sigit_config_dir() {
        dirs.push(global);
    }
    dirs.extend(chain);
    dirs
}

/// The nearest ancestor of `dir` (inclusive) that contains a `.git` entry.
fn repo_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The first existing instruction file in `dir`, by name priority.
fn first_instruction_file(dir: &Path) -> Option<PathBuf> {
    for name in INSTRUCTION_FILE_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn sigit_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SIGIT_CONFIG_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config").join("sigit"))
}

/// Truncate `text` to at most `limit` bytes on a char boundary, appending a
/// marker when truncation happens.
fn clamp_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n--- truncated ({} of {} bytes shown) ---",
        &text[..end],
        end,
        text.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("sigit-instr-test-{name}-{nanos}"))
    }

    #[test]
    fn none_when_no_files() {
        let root = unique_dir("empty");
        fs::create_dir_all(&root).unwrap();
        // Mark as a repo root so the scan doesn't escape into real ancestors.
        fs::create_dir_all(root.join(".git")).unwrap();
        assert!(load_project_instructions(&root).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn agents_md_preferred_over_claude_md_in_same_dir() {
        let root = unique_dir("prefer");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("AGENTS.md"), "use tabs").unwrap();
        fs::write(root.join("CLAUDE.md"), "use spaces").unwrap();

        let out = load_project_instructions(&root).expect("instructions");
        assert!(out.contains("use tabs"));
        assert!(!out.contains("use spaces"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_files_ordered_root_first() {
        let root = unique_dir("nested");
        let sub = root.join("crate-a");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("AGENTS.md"), "ROOT RULES").unwrap();
        fs::write(sub.join("AGENTS.md"), "SUB RULES").unwrap();

        let out = load_project_instructions(&sub).expect("instructions");
        let root_pos = out.find("ROOT RULES").expect("root present");
        let sub_pos = out.find("SUB RULES").expect("sub present");
        // Root (broader) is read before the deeper, more specific file.
        assert!(root_pos < sub_pos, "root should precede sub:\n{out}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn does_not_escape_repo_root() {
        // A parent dir's AGENTS.md must not be read when the repo root is deeper.
        let root = unique_dir("boundary");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(root.join("AGENTS.md"), "OUTSIDE").unwrap();
        fs::write(repo.join("AGENTS.md"), "INSIDE").unwrap();

        let out = load_project_instructions(&repo).expect("instructions");
        assert!(out.contains("INSIDE"));
        assert!(
            !out.contains("OUTSIDE"),
            "must not read above repo root:\n{out}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clamp_bytes_truncates_long_input() {
        let long = "x".repeat(MAX_FILE_BYTES + 100);
        let clamped = clamp_bytes(&long, MAX_FILE_BYTES);
        assert!(clamped.contains("truncated"));
        assert!(clamped.len() < long.len() + 100);
    }
}
