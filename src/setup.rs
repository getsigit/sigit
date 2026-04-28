//! Model cache setup, local model discovery, and selected-model persistence.
//!
//! On macOS the CLI shares a HuggingFace cache with Onde desktop apps via an
//! App Group container (`~/Library/Group Containers/group.com.ondeinference.apps/models/`).
//! On other platforms it falls back to `~/.cache/huggingface/`.
//!
//! Must run before anything touches `ChatEngine` or `hf-hub` because they
//! read the env vars once at init.

use std::path::{Path, PathBuf};

/// shared across siGit, Rumi, GT8, etc.
#[cfg(target_os = "macos")]
const APP_GROUP_IDENTIFIER: &str = "group.com.ondeinference.apps";

/// point `HF_HOME` / `HF_HUB_CACHE` at the shared container. no-ops if
/// the user already set them.
pub fn setup_shared_model_cache() {
    if let Some(shared_dir) = resolve_shared_container() {
        let models_home = shared_dir.join("models");
        let model_hub = models_home.join("hub");

        if let Err(error) = std::fs::create_dir_all(&model_hub) {
            log::warn!(
                "Failed to create shared model cache at {}: {error} — falling back to default",
                model_hub.display()
            );
            return;
        }

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

        // mistral.rs reads HF_HUB_CACHE directly instead of deriving from HF_HOME
        if std::env::var("HF_HUB_CACHE").is_err() {
            // SAFETY: called once at startup before any threads are spawned.
            unsafe { std::env::set_var("HF_HUB_CACHE", &model_hub) };
            log::info!("HF_HUB_CACHE → shared App Group: {}", model_hub.display());
        }
    } else {
        log::debug!("Shared App Group container not available — using default HF cache");
    }
}

const SELECTED_MODEL_FILE_NAME: &str = "selected-model.txt";

/// persisted identifier for a selected model (model_id + gguf filename).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedModel {
    /// e.g. `bartowski/Qwen_Qwen3-4B-GGUF`
    pub model_id: String,

    pub gguf_file: String,
}

impl SelectedModel {
    fn from_discovered(model: &DiscoveredModel) -> Self {
        Self {
            model_id: model.model_id.clone(),
            gguf_file: model.gguf_file.clone(),
        }
    }

    fn matches(&self, model: &DiscoveredModel) -> bool {
        self.model_id == model.model_id && self.gguf_file == model.gguf_file
    }
}

/// what we know about the model before the full UI is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupModelSelection {
    /// shown in the loading screen
    pub display_name: String,

    pub selected_model: Option<SelectedModel>,
}

/// a GGUF model found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// e.g. `bartowski/Qwen_Qwen3-4B-GGUF`
    pub model_id: String,
    /// filename inside the snapshot dir
    pub gguf_file: String,

    pub display_name: String,

    pub snapshot_path: PathBuf,

    pub gguf_path: PathBuf,

    pub from_app_group: bool,

    pub cache_health: ModelCacheHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelCacheHealth {
    Complete,
    Incomplete,
    NotDownloaded,
}

/// find all GGUF models on disk. checks Onde app group first, then HF cache.
pub fn discover_local_models() -> Vec<DiscoveredModel> {
    let mut models = Vec::new();
    let mut seen_roots = Vec::new();

    if let Some(app_group_models) = app_group_models_root() {
        seen_roots.push(app_group_models.clone());
        collect_models_from_cache_root(&app_group_models, true, &mut models);
    }

    if let Some(hf_cache) = hf_cache_root()
        && !seen_roots.iter().any(|root| root == &hf_cache)
    {
        seen_roots.push(hf_cache.clone());
        collect_models_from_cache_root(&hf_cache, false, &mut models);
    }

    if let Some(default_hf_cache) = default_hf_cache_root()
        && !seen_roots.iter().any(|root| root == &default_hf_cache)
    {
        collect_models_from_cache_root(&default_hf_cache, false, &mut models);
    }

    models.sort_by(|left, right| {
        right
            .from_app_group
            .cmp(&left.from_app_group)
            .then_with(|| {
                left.display_name
                    .to_lowercase()
                    .cmp(&right.display_name.to_lowercase())
            })
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

            let mut gguf_files = Vec::new();

            for file in files.flatten() {
                let file_path = file.path();
                if !file_path.is_file() {
                    continue;
                }

                let file_name = match file.file_name().to_str() {
                    Some(name) => name.to_string(),
                    None => continue,
                };

                let extension = file_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or_default();

                if extension.eq_ignore_ascii_case("gguf") {
                    gguf_files.push((file_name, file_path));
                }
            }

            if gguf_files.is_empty() {
                // snapshot dir exists but no .gguf yet (download in progress or
                // only metadata). mark incomplete so the picker can show it disabled.
                models.push(DiscoveredModel {
                    display_name: display_name_for_model(&model_id, ""),
                    model_id: model_id.clone(),
                    gguf_file: String::new(),
                    snapshot_path: snapshot_path.clone(),
                    // unused for loading; incomplete models are filtered out before config
                    gguf_path: snapshot_path.clone(),
                    from_app_group,
                    cache_health: ModelCacheHealth::Incomplete,
                });
            } else {
                for (gguf_file, file_path) in gguf_files {
                    models.push(DiscoveredModel {
                        display_name: display_name_for_model(&model_id, &gguf_file),
                        model_id: model_id.clone(),
                        gguf_file,
                        snapshot_path: snapshot_path.clone(),
                        gguf_path: file_path,
                        from_app_group,
                        cache_health: ModelCacheHealth::Complete,
                    });
                }
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

    None
}

fn default_hf_cache_root() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home)
        .join(".cache")
        .join("huggingface")
        .join("hub");

    path.is_dir().then_some(path)
}

pub fn load_selected_model() -> Option<SelectedModel> {
    let path = selected_model_file_path()?;
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.splitn(2, '\n');
    let model_id = parts.next()?.trim();
    let gguf_file = parts.next()?.trim();

    if model_id.is_empty() || gguf_file.is_empty() {
        return None;
    }

    Some(SelectedModel {
        model_id: model_id.to_string(),
        gguf_file: gguf_file.to_string(),
    })
}

#[allow(dead_code)]
pub fn load_selected_model_name() -> Option<String> {
    let selected = load_selected_model()?;
    discover_local_models()
        .into_iter()
        .find(|model| selected.matches(model))
        .map(|model| model.display_name)
}

/// pick a model for startup: saved selection > first local model > none.
/// if we fall back to a local model, persist it so ACP and TUI agree next time.
pub fn startup_model_selection() -> Option<StartupModelSelection> {
    let discovered = discover_local_models();

    if let Some(saved_model) = load_selected_model()
        && let Some(model) = discovered.iter().find(|model| {
            saved_model.matches(model) && model.cache_health == ModelCacheHealth::Complete
        })
    {
        return Some(StartupModelSelection {
            display_name: model.display_name.clone(),
            selected_model: Some(saved_model),
        });
    }

    discovered
        .into_iter()
        .find(|model| model.cache_health == ModelCacheHealth::Complete)
        .map(|model| {
            let selected_model = SelectedModel::from_discovered(&model);
            let _ = save_selected_model(&selected_model);
            StartupModelSelection {
                display_name: model.display_name.clone(),
                selected_model: Some(selected_model),
            }
        })
}

pub fn save_selected_model(selected_model: &SelectedModel) -> Result<(), String> {
    let path = selected_model_file_path()
        .ok_or_else(|| "Could not determine where to store the selected model.".to_string())?;

    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create preferences directory: {error}"))?;
    }

    let contents = format!(
        "{}\n{}\n",
        selected_model.model_id, selected_model.gguf_file
    );

    std::fs::write(&path, contents)
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

/// macOS only creates this dir when a signed app in the group first runs,
/// so it won't exist until the user has launched siGit desktop or another Onde app.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        std::env::temp_dir().join(format!("sigit-setup-tests-{name}-{nanos}"))
    }

    fn create_snapshot(
        cache_root: &Path,
        model_id: &str,
        snapshot_name: &str,
        gguf_file: &str,
        complete: bool,
    ) -> PathBuf {
        let repo_dir = cache_root.join(format!("models--{}", model_id.replace('/', "--")));
        let snapshot_dir = repo_dir.join("snapshots").join(snapshot_name);
        std::fs::create_dir_all(&snapshot_dir).expect("create snapshot dir");

        // Health is determined solely by the presence of a .gguf file.
        // A complete snapshot has one; an incomplete snapshot has none
        // (e.g. a partial download where only metadata files arrived).
        if complete {
            std::fs::write(snapshot_dir.join(gguf_file), b"gguf placeholder").expect("write gguf");
        } else {
            // Simulate a snapshot directory that exists but has no GGUF yet.
            std::fs::write(snapshot_dir.join("config.json"), b"{}")
                .expect("write config placeholder");
        }

        snapshot_dir
    }

    fn with_test_env<T>(hf_hub_cache: &Path, hf_home: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = env_lock().lock().expect("lock env");

        let old_hf_hub_cache = std::env::var_os("HF_HUB_CACHE");
        let old_hf_home = std::env::var_os("HF_HOME");
        let old_home = std::env::var_os("HOME");

        // SAFETY: tests serialize environment mutation with a process-wide mutex.
        unsafe {
            std::env::set_var("HF_HUB_CACHE", hf_hub_cache);
            std::env::set_var("HF_HOME", hf_home);
            std::env::set_var("HOME", hf_home);
        }

        let result = f();

        // SAFETY: tests serialize environment mutation with a process-wide mutex.
        unsafe {
            match old_hf_hub_cache {
                Some(value) => std::env::set_var("HF_HUB_CACHE", value),
                None => std::env::remove_var("HF_HUB_CACHE"),
            }
            match old_hf_home {
                Some(value) => std::env::set_var("HF_HOME", value),
                None => std::env::remove_var("HF_HOME"),
            }
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }

        result
    }

    #[test]
    fn discover_local_models_marks_complete_and_incomplete_snapshots() {
        let root = unique_temp_dir("discover-health");
        let cache_root = root.join("hf-cache");
        let hf_home = root.join("hf-home");
        std::fs::create_dir_all(&cache_root).expect("create cache root");
        std::fs::create_dir_all(&hf_home).expect("create hf home");

        create_snapshot(
            &cache_root,
            "bartowski/Qwen_Qwen3-4B-GGUF",
            "complete",
            "Qwen_Qwen3-4B-Q4_K_M.gguf",
            true,
        );
        create_snapshot(
            &cache_root,
            "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF",
            "incomplete",
            "Qwen2.5-Coder-3B-Instruct-Q4_K_M.gguf",
            false,
        );

        let models = with_test_env(&cache_root, &hf_home, discover_local_models);

        assert_eq!(models.len(), 2);

        let complete = models
            .iter()
            .find(|model| model.model_id == "bartowski/Qwen_Qwen3-4B-GGUF")
            .expect("complete model discovered");
        assert_eq!(complete.cache_health, ModelCacheHealth::Complete);
        assert!(!complete.from_app_group);

        let incomplete = models
            .iter()
            .find(|model| model.model_id == "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF")
            .expect("incomplete model discovered");
        assert_eq!(incomplete.cache_health, ModelCacheHealth::Incomplete);
        assert!(!incomplete.from_app_group);

        std::fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn startup_model_selection_skips_saved_incomplete_model_and_picks_complete_one() {
        let root = unique_temp_dir("startup-selection");
        let cache_root = root.join("hf-cache");
        let hf_home = root.join("hf-home");
        std::fs::create_dir_all(&cache_root).expect("create cache root");
        std::fs::create_dir_all(&hf_home).expect("create hf home");

        create_snapshot(
            &cache_root,
            "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF",
            "broken",
            "Qwen2.5-Coder-3B-Instruct-Q4_K_M.gguf",
            false,
        );
        create_snapshot(
            &cache_root,
            "bartowski/Qwen_Qwen3-4B-GGUF",
            "ready",
            "Qwen_Qwen3-4B-Q4_K_M.gguf",
            true,
        );

        let selection = with_test_env(&cache_root, &hf_home, || {
            let selected_path = selected_model_file_path().expect("selected model path");
            if let Some(parent) = selected_path.parent() {
                std::fs::create_dir_all(parent).expect("create selected model parent");
            }

            std::fs::write(
                &selected_path,
                "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF\nQwen2.5-Coder-3B-Instruct-Q4_K_M.gguf\n",
            )
            .expect("write selected model");

            startup_model_selection().expect("startup selection")
        });

        assert_eq!(
            selection.display_name,
            "Qwen Qwen3-4B-GGUF — Qwen_Qwen3-4B-Q4_K_M"
        );
        let selected = selection.selected_model.expect("selected model");
        assert_eq!(selected.model_id, "bartowski/Qwen_Qwen3-4B-GGUF");
        assert_eq!(selected.gguf_file, "Qwen_Qwen3-4B-Q4_K_M.gguf");

        std::fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn discover_empty_cache_returns_no_models() {
        let root = unique_temp_dir("discover-empty");
        let cache_root = root.join("hf-cache");
        let hf_home = root.join("hf-home");
        std::fs::create_dir_all(&cache_root).expect("create cache root");
        std::fs::create_dir_all(&hf_home).expect("create hf home");

        let models = with_test_env(&cache_root, &hf_home, discover_local_models);
        assert!(models.is_empty());

        std::fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn complete_models_sort_before_incomplete() {
        let root = unique_temp_dir("sort-order");
        let cache_root = root.join("hf-cache");
        let hf_home = root.join("hf-home");
        std::fs::create_dir_all(&cache_root).expect("create cache root");
        std::fs::create_dir_all(&hf_home).expect("create hf home");

        create_snapshot(
            &cache_root,
            "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF",
            "snap1",
            "Qwen2.5-Coder-3B-Instruct-Q4_K_M.gguf",
            false,
        );
        create_snapshot(
            &cache_root,
            "bartowski/Qwen_Qwen3-4B-GGUF",
            "snap2",
            "Qwen_Qwen3-4B-Q4_K_M.gguf",
            true,
        );

        let models = with_test_env(&cache_root, &hf_home, discover_local_models);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].cache_health, ModelCacheHealth::Complete);
        assert_eq!(models[1].cache_health, ModelCacheHealth::Incomplete);

        std::fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn load_selected_model_roundtrip() {
        let root = unique_temp_dir("persistence-roundtrip");
        let hf_home = root.join("hf-home");
        std::fs::create_dir_all(&hf_home).expect("create hf home");

        with_test_env(&hf_home, &hf_home, || {
            let original = SelectedModel {
                model_id: "bartowski/Qwen_Qwen3-4B-GGUF".to_string(),
                gguf_file: "Qwen_Qwen3-4B-Q4_K_M.gguf".to_string(),
            };

            save_selected_model(&original).expect("save");
            let loaded = load_selected_model().expect("load");

            assert_eq!(loaded.model_id, original.model_id);
            assert_eq!(loaded.gguf_file, original.gguf_file);
        });

        std::fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn load_selected_model_empty_file_returns_none() {
        let root = unique_temp_dir("persistence-empty");
        let hf_home = root.join("hf-home");
        std::fs::create_dir_all(&hf_home).expect("create hf home");

        with_test_env(&hf_home, &hf_home, || {
            let path = selected_model_file_path().expect("path");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(&path, "").expect("write empty");

            assert!(load_selected_model().is_none());
        });

        std::fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn display_name_deduplicates_repo_and_file_name() {
        // When the file name contains the repo name, only the repo name is shown.
        let name = display_name_for_model(
            "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF",
            "Qwen2.5-Coder-3B-Instruct-GGUF-Q4_K_M.gguf",
        );
        assert_eq!(name, "Qwen2.5-Coder-3B-Instruct-GGUF");

        // When the file name does NOT contain the repo name, both are shown.
        let name = display_name_for_model(
            "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF",
            "Qwen2.5-Coder-3B-Instruct-Q4_K_M.gguf",
        );
        assert_eq!(
            name,
            "Qwen2.5-Coder-3B-Instruct-GGUF — Qwen2.5-Coder-3B-Instruct-Q4_K_M"
        );
    }

    #[test]
    fn display_name_includes_file_when_different() {
        let name =
            display_name_for_model("bartowski/Qwen_Qwen3-4B-GGUF", "Qwen_Qwen3-4B-Q4_K_M.gguf");
        assert_eq!(name, "Qwen Qwen3-4B-GGUF — Qwen_Qwen3-4B-Q4_K_M");
    }
}
