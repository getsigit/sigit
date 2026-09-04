//! The directories the current session treats as project roots.
//!
//! An editor can open more than one directory in a single project — Zed calls
//! it a multi-root project — and ACP carries the extra ones as
//! `additional_directories` on every session request. The process still has a
//! single working directory (the primary `cwd` it `set_current_dir`s to), so
//! the extra roots live here, in a process-global that project-local discovery
//! reads alongside `std::env::current_dir()`: skills (`skills.rs`), slash
//! commands (`commands.rs`), subagent types (`subagents.rs`) and instruction
//! files (`instructions.rs`).
//!
//! MCP servers are deliberately not on that list. Discovery runs once at
//! startup (`mcp::init`), before any session exists, so a second root's
//! `.sigit/mcp.toml` has nobody to tell.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// The session's roots beyond `cwd`. Empty for an ordinary single-root project.
static ADDITIONAL_ROOTS: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// Record the session's extra roots, returning the ones that were kept.
///
/// Entries that aren't directories, repeat, or just name `cwd` again are
/// dropped — an editor is free to send any of those, and every consumer here
/// would either scan nothing or do the same work twice.
pub fn set_additional_roots(cwd: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = vec![canonical(cwd)];
    let mut kept: Vec<PathBuf> = Vec::new();

    for root in roots {
        if !root.is_dir() {
            log::warn!(
                "ignoring workspace root {} (not a directory)",
                root.display()
            );
            continue;
        }
        let key = canonical(root);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        kept.push(root.clone());
    }

    if let Ok(mut guard) = ADDITIONAL_ROOTS.write() {
        *guard = kept.clone();
    }
    kept
}

/// The extra roots recorded for the session.
pub fn additional_roots() -> Vec<PathBuf> {
    ADDITIONAL_ROOTS
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Every directory the session treats as a project root: the process working
/// directory first, then the extra roots. Project-local discovery scans these
/// in order, so the primary root keeps winning name collisions.
pub fn project_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    dirs.extend(additional_roots());
    dirs
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One process-global under test, so the cases share a lock and a cleanup.
    fn with_roots<T>(cwd: &Path, roots: &[PathBuf], test: impl FnOnce(Vec<PathBuf>) -> T) -> T {
        static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = GUARD.lock().unwrap_or_else(|poison| poison.into_inner());
        let kept = set_additional_roots(cwd, roots);
        let out = test(kept);
        set_additional_roots(cwd, &[]);
        out
    }

    #[test]
    fn keeps_real_directories_and_drops_the_rest() {
        let temp = std::env::temp_dir().join(format!("sigit-ws-{}", uuid::Uuid::new_v4()));
        let cwd = temp.join("primary");
        let extra = temp.join("extra");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        let missing = temp.join("gone");

        with_roots(
            &cwd,
            &[extra.clone(), extra.clone(), cwd.clone(), missing],
            |kept| {
                assert_eq!(kept, vec![extra.clone()]);
                assert_eq!(additional_roots(), vec![extra.clone()]);
            },
        );

        assert!(additional_roots().is_empty());
        std::fs::remove_dir_all(&temp).ok();
    }
}
