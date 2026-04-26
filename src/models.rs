//! Platform-independent model picker types and item construction.
//!
//! This module is available on all target platforms (Windows, macOS, Linux).
//! The TUI rendering code in `chat.rs` (unix-only) re-uses these types
//! rather than defining them inline.

use onde::inference::GgufModelConfig;

use crate::setup::DiscoveredModel;

pub(crate) use crate::setup::ModelCacheHealth;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ModelSource {
    Onde,
    HuggingFace,
    /// Supported model that is not yet downloaded locally. When selected it
    /// will be downloaded into the Onde app-group cache automatically.
    Available,
    Fallback,
}

#[derive(Clone)]
pub(crate) struct ModelPickerItem {
    pub(crate) display_name: String,
    pub(crate) description: String,
    pub(crate) tool_calling: bool,
    pub(crate) max_tokens: u64,
    pub(crate) config: GgufModelConfig,
    pub(crate) source_label: String,

    pub(crate) source: ModelSource,
    pub(crate) cache_health: ModelCacheHealth,
}

// ── Model ID → GgufModelConfig mapping ────────────────────────────────────────

/// Map a HuggingFace model ID to the corresponding [`GgufModelConfig`]
/// constructor. Returns `None` for model IDs that siGit does not know how
/// to load.
pub(crate) fn model_id_to_config(model_id: &str) -> Option<GgufModelConfig> {
    Some(match model_id {
        "bartowski/Qwen_Qwen3-4B-GGUF" => GgufModelConfig::qwen3_4b(),
        "bartowski/Qwen_Qwen3-8B-GGUF" => GgufModelConfig::qwen3_8b(),
        "bartowski/Qwen_Qwen3-1.7B-GGUF" => GgufModelConfig::qwen3_1_7b(),
        "bartowski/Qwen2.5-3B-Instruct-GGUF" => GgufModelConfig::qwen25_3b(),
        "bartowski/Qwen2.5-1.5B-Instruct-GGUF" => GgufModelConfig::qwen25_1_5b(),
        "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF" => GgufModelConfig::qwen25_coder_3b(),
        "bartowski/Qwen2.5-Coder-1.5B-Instruct-GGUF" => GgufModelConfig::qwen25_coder_1_5b(),
        "bartowski/Qwen2.5-Coder-7B-Instruct-GGUF" => GgufModelConfig::qwen25_coder_7b(),
        "TheBloke/deepseek-coder-6.7B-instruct-GGUF" => GgufModelConfig::deepseek_coder_6_7b(),
        _ => return None,
    })
}

/// Whether a model ID supports tool calling (Qwen 3 family).
fn is_tool_calling(model_id: &str) -> bool {
    matches!(
        model_id,
        "bartowski/Qwen_Qwen3-4B-GGUF"
            | "bartowski/Qwen_Qwen3-8B-GGUF"
            | "bartowski/Qwen_Qwen3-1.7B-GGUF"
            | "bartowski/Qwen2.5-Coder-7B-Instruct-GGUF"
    )
}

/// Max tokens for a given model (tool-calling models need higher budgets
/// because the `<think>…</think>` block consumes tokens before the real
/// response).
fn max_tokens_for(model_id: &str) -> u64 {
    if is_tool_calling(model_id) { 4096 } else { 512 }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Build the full list of model picker items.
///
/// Items are sourced from:
/// 1. **Locally cached** models in the Onde app-group and HuggingFace caches.
/// 2. **All supported models** from [`onde::inference::models::SUPPORTED_MODEL_INFO`]
///    that are not yet downloaded locally — shown as `Available` so the user
///    can select them to trigger a download into the app-group cache.
///
/// If no models are discovered *and* no supported models are known, a single
/// fallback entry for the platform-default model is returned so the picker
/// is never empty.
///
/// Items are sorted: Onde first, then HuggingFace, then Available (not
/// downloaded), then Fallback, and alphabetically within each group.
pub(crate) fn build_model_picker_items() -> Vec<ModelPickerItem> {
    let mut items = Vec::new();

    // ── 1. Locally discovered models ─────────────────────────────────────
    for discovered in crate::setup::discover_local_models() {
        if let Some(item) = discovered_model_to_picker_item(discovered) {
            items.push(item);
        }
    }

    // ── 2. Supported models not yet downloaded ───────────────────────────
    //
    // Walk SUPPORTED_MODEL_INFO and add an entry for every model ID that
    // does not already appear in the local items list (by model_id).
    // These entries have `cache_health: NotDownloaded` and
    // `source: Available`. When the user selects one, `load_gguf_model`
    // will download the GGUF file from HuggingFace into the app-group
    // cache automatically.
    for info in onde::inference::models::SUPPORTED_MODEL_INFO {
        let already_present = items.iter().any(|item| item.config.model_id == info.id);
        if already_present {
            continue;
        }

        let config = match model_id_to_config(info.id) {
            Some(config) => config,
            None => continue,
        };

        let tool_calling = is_tool_calling(info.id);
        let max_tokens = max_tokens_for(info.id);

        items.push(ModelPickerItem {
            display_name: config.display_name.clone(),
            description: config.approx_memory.clone(),
            tool_calling,
            max_tokens,
            config,
            source_label: "Onde".to_string(),

            source: ModelSource::Available,
            cache_health: ModelCacheHealth::NotDownloaded,
        });
    }

    // ── 3. Fallback ──────────────────────────────────────────────────────
    if items.is_empty() {
        let config = GgufModelConfig::platform_default();
        let tool_calling = is_tool_calling(&config.model_id);
        let max_tokens = max_tokens_for(&config.model_id);

        items.push(ModelPickerItem {
            display_name: config.display_name.clone(),
            description: config.approx_memory.clone(),
            tool_calling,
            max_tokens,
            config,
            source_label: "Platform default".to_string(),

            source: ModelSource::Fallback,
            cache_health: ModelCacheHealth::Complete,
        });
    }

    items.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.display_name.cmp(&right.display_name))
    });

    items
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn discovered_model_to_picker_item(model: DiscoveredModel) -> Option<ModelPickerItem> {
    let source_label = if model.from_app_group {
        "Onde".to_string()
    } else {
        "HuggingFace".to_string()
    };

    let config = model_id_to_config(&model.model_id)?;

    let tool_calling = is_tool_calling(&model.model_id);
    let max_tokens = max_tokens_for(&model.model_id);

    Some(ModelPickerItem {
        display_name: config.display_name.clone(),
        description: config.approx_memory.clone(),
        tool_calling,
        max_tokens,
        config,
        source_label,

        source: if model.from_app_group {
            ModelSource::Onde
        } else {
            ModelSource::HuggingFace
        },
        cache_health: model.cache_health,
    })
}
