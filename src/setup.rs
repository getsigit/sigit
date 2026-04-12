//! Shared model cache setup.
//!
//! On macOS, siGit desktop and other Onde apps keep their HuggingFace models
//! in a shared App Group container at:
//!
//!   `~/Library/Group Containers/group.com.ondeinference.apps/models/`
//!
//! This module points `HF_HOME` / `HF_HUB_CACHE` there so the CLI reuses
//! whatever the desktop app already downloaded (and vice versa). On Linux
//! and Windows the default `~/.cache/huggingface/` path is used.
//!
//! Call this before anything touches `ChatEngine` or `hf-hub` — they read
//! the env vars once at init and never check again.

use std::path::PathBuf;

/// App Group ID shared across all Onde apps (siGit, Rumi, GT8, …).
const APP_GROUP_IDENTIFIER: &str = "group.com.ondeinference.apps";

/// Find the shared container and set `HF_HOME` / `HF_HUB_CACHE` to point
/// there. Skips any var the user already set.
pub fn setup_shared_model_cache() {
    if let Some(shared_dir) = resolve_shared_container() {
        let models_home = shared_dir.join("models");
        let model_hub = models_home.join("hub");

        // Make sure the dirs exist.
        if let Err(error) = std::fs::create_dir_all(&model_hub) {
            log::warn!(
                "Failed to create shared model cache at {}: {error} — falling back to default",
                model_hub.display()
            );
            return;
        }

        // hf-hub derives all its paths from HF_HOME.
        if std::env::var("HF_HOME").is_err() {
            // SAFETY: called once at startup before any threads are spawned.
            unsafe { std::env::set_var("HF_HOME", &models_home) };
            log::info!("HF_HOME → shared App Group: {}", models_home.display());
        } else {
            log::debug!(
                "HF_HOME already set by user: {}",
                std::env::var("HF_HOME").unwrap_or_default()
            );
        }

        // Some mistral.rs code paths read HF_HUB_CACHE directly instead
        // of deriving it from HF_HOME, so we set both.
        if std::env::var("HF_HUB_CACHE").is_err() {
            // SAFETY: called once at startup before any threads are spawned.
            unsafe { std::env::set_var("HF_HUB_CACHE", &model_hub) };
            log::info!("HF_HUB_CACHE → shared App Group: {}", model_hub.display());
        }
    } else {
        log::debug!("Shared App Group container not available — using default HF cache");
    }
}

/// Look for the App Group container on disk. macOS creates it the first time
/// a signed app in the group accesses it, so it only exists if the user has
/// launched siGit desktop (or another Onde app) at least once. A plain CLI
/// binary can read/write there without extra entitlements.
#[cfg(target_os = "macos")]
fn resolve_shared_container() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let container = PathBuf::from(home)
        .join("Library")
        .join("Group Containers")
        .join(APP_GROUP_IDENTIFIER);

    if container.is_dir() {
        log::debug!("App Group container found: {}", container.display());
        Some(container)
    } else {
        log::debug!(
            "App Group container does not exist at {} — \
             has siGit desktop been launched at least once?",
            container.display()
        );
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn resolve_shared_container() -> Option<PathBuf> {
    None
}
