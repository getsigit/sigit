//! Shared model cache setup, local model discovery, and lightweight local
//! preferences.
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
//! It also exposes helpers for finding locally available models. Discovery
//! checks the Onde app group first on macOS, then falls back to the normal
//! Hugging Face cache layout.
//!
//! The selected model name is persisted in a small local preferences file so
//! the interactive UI can restore the last choice on the next launch.
//!
//! Call this before anything touches `ChatEngine` or `hf-hub` — they read
//! the env vars once at init and never check again.

use std::path::{Path, PathBuf};

/// App Group ID shared across all Onde apps (siGit, Rumi, GT8, …).
#[cfg(target_os = "macos")]
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

/// Preference key used to remember the last selected model.
const SELECTED_MODEL_FILE_NAME: &str = "selected-model.txt";

/// Minimal startup model selection info used before the full UI is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupModelSelection {
    /// Human-friendly model name shown in the loading UI.
    pub display_name: String,
    /// The saved model name if one was found.
    pub selected_name: Option<String>,
}

/// A locally discovered GGUF model candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// Hugging Face repo ID, e.g. `bartowski/Qwen_Qwen3-4B-GGUF`.
    pub model_id: String,
    /// GGUF filename inside the snapshot.
    pub gguf_file: String,
    /// Human-friendly label shown in model pickers.
    pub display_name: String,
    /// Absolute path to the snapshot directory that contains the GGUF file.
    pub snapshot_path: PathBuf,
    /// Absolute path to the GGUF file itself.
    pub gguf_path: PathBuf,
    /// True when the model came from the Onde app group cache.
    pub from_app_group: bool,
}

/// Return all locally discovered GGUF models.
///
/// Search order:
/// 1. Onde app group cache on macOS
/// 2. Standard Hugging Face cache
pub fn discover_local_models() -> Vec<DiscoveredModel> {
    let mut models = Vec::new();

    if let Some(app_group_models) = app_group_models_root() {
        collect_models_from_cache_root(&app_group_models, true, &mut models);
    }

    if let Some(hf_cache) = hf_cache_root() {
        collect_models_from_cache_root(&hf_cache, false, &mut models);
    }

    models.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
            .then_with(|| left.model_id.cmp(&right.model_id))
            .then_with(|| left.gguf_file.cmp(&right.gguf_file))
    });

    models.dedup_by(|left, right| left.gguf_path == right.gguf_path);
    models
}

fn collect_models_from_cache_root(
    cache_root: &Path,
    from_app_group: bool,
    models: &mut Vec<DiscoveredModel>,
) {
    let entries = match std::fs::read_dir(cache_root) {
        Ok(entries) => entries,
        Err(error) => {
            log::debug!(
                "Skipping unreadable model cache root {}: {error}",
                cache_root.display()
            );
            return;
        }
    };

    for entry in entries.flatten() {
        let repo_dir = entry.path();
        if !repo_dir.is_dir() {
            continue;
        }

        let dir_name = match entry.file_name().to_str() {
            Some(name) => name.to_string(),
            None => continue,
        };

        if !dir_name.starts_with("models--") {
            continue;
        }

        let model_id = dir_name["models--".len()..].replace("--", "/");
        let snapshots_dir = repo_dir.join("snapshots");
        let snapshots = match std::fs::read_dir(&snapshots_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for snapshot in snapshots.flatten() {
            let snapshot_path = snapshot.path();
            if !snapshot_path.is_dir() {
                continue;
            }

            let files = match std::fs::read_dir(&snapshot_path) {
                Ok(entries) => entries,
                Err(_) => continue,
            };

            for file in files.flatten() {
                let file_path = file.path();
                if !file_path.is_file() {
                    continue;
                }

                let extension = file_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or_default();

                if !extension.eq_ignore_ascii_case("gguf") {
                    continue;
                }

                let gguf_file = match file.file_name().to_str() {
                    Some(name) => name.to_string(),
                    None => continue,
                };

                models.push(DiscoveredModel {
                    display_name: display_name_for_model(&model_id, &gguf_file),
                    model_id: model_id.clone(),
                    gguf_file,
                    snapshot_path: snapshot_path.clone(),
                    gguf_path: file_path,
                    from_app_group,
                });
            }
        }
    }
}

fn display_name_for_model(model_id: &str, gguf_file: &str) -> String {
    let repo_name = model_id
        .rsplit('/')
        .next()
        .unwrap_or(model_id)
        .replace('_', " ");

    let file_name = gguf_file.strip_suffix(".gguf").unwrap_or(gguf_file);

    if file_name.contains(&repo_name.replace(' ', "_")) || file_name.contains(&repo_name) {
        repo_name
    } else {
        format!("{repo_name} — {file_name}")
    }
}

fn app_group_models_root() -> Option<PathBuf> {
    resolve_shared_container().map(|dir| dir.join("models").join("hub"))
}

fn hf_cache_root() -> Option<PathBuf> {
    if let Ok(cache) = std::env::var("HF_HUB_CACHE") {
        let path = PathBuf::from(cache);
        if path.is_dir() {
            return Some(path);
        }
    }

    if let Ok(home) = std::env::var("HF_HOME") {
        let path = PathBuf::from(home).join("hub");
        if path.is_dir() {
            return Some(path);
        }
    }

    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home)
        .join(".cache")
        .join("huggingface")
        .join("hub");

    path.is_dir().then_some(path)
}

pub fn load_selected_model_name() -> Option<String> {
    let path = selected_model_file_path()?;
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Pick the model name siGit should try to load at startup.
///
/// Order:
/// 1. saved selection, if it still exists locally
/// 2. first discovered local model (Onde app group first, then HF cache)
/// 3. no selection
///
/// If there is no saved selection but a local model is discovered, persist that
/// fallback choice so ACP mode and the interactive TUI converge on the same
/// startup model on the next launch too.
pub fn startup_model_selection() -> Option<StartupModelSelection> {
    let discovered = discover_local_models();

    if let Some(saved_name) = load_selected_model_name() {
        if discovered
            .iter()
            .any(|model| model.display_name == saved_name)
        {
            return Some(StartupModelSelection {
                display_name: saved_name.clone(),
                selected_name: Some(saved_name),
            });
        }
    }

    discovered.into_iter().next().map(|model| {
        let _ = save_selected_model_name(&model.display_name);
        StartupModelSelection {
            display_name: model.display_name.clone(),
            selected_name: Some(model.display_name),
        }
    })
}

pub fn save_selected_model_name(model_name: &str) -> Result<(), String> {
    let path = selected_model_file_path()
        .ok_or_else(|| "Could not determine where to store the selected model.".to_string())?;

    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create preferences directory: {error}"))?;
    }

    std::fs::write(&path, model_name)
        .map_err(|error| format!("Could not save selected model: {error}"))
}

fn selected_model_file_path() -> Option<PathBuf> {
    if let Some(shared_dir) = resolve_shared_container() {
        return Some(shared_dir.join(SELECTED_MODEL_FILE_NAME));
    }

    if let Ok(home) = std::env::var("HF_HOME") {
        let path = PathBuf::from(home);
        if path.is_dir() || path.parent().is_some() {
            return Some(path.join(SELECTED_MODEL_FILE_NAME));
        }
    }

    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("sigit")
            .join(SELECTED_MODEL_FILE_NAME),
    )
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
