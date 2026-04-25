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
    pub(crate) brand_mark: &'static str,
    pub(crate) source: ModelSource,
    pub(crate) cache_health: ModelCacheHealth,
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Build the full list of available model picker items from the local cache.
///
/// Items are sourced from:
/// 1. The Onde app-group model cache (macOS shared container).
/// 2. The HuggingFace hub cache (`HF_HUB_CACHE` / `HF_HOME` / `~/.cache/huggingface/hub`).
///
/// If no models are discovered at all, a single fallback entry for the
/// platform-default model is returned so the picker is never empty.
///
/// Items are sorted by source priority (Onde first, then HuggingFace, then
/// Fallback) and then alphabetically by display name within each group.
pub(crate) fn build_model_picker_items() -> Vec<ModelPickerItem> {
    let mut items = Vec::new();

    for discovered in crate::setup::discover_local_models() {
        if let Some(item) = discovered_model_to_picker_item(discovered) {
            items.push(item);
        }
    }

    if items.is_empty() {
        let config = GgufModelConfig::platform_default();
        let tool_calling = config.display_name == "Qwen 3 4B (Q4_K_M)";
        let max_tokens = if tool_calling { 4096 } else { 512 };

        items.push(ModelPickerItem {
            display_name: config.display_name.clone(),
            description: config.approx_memory.clone(),
            tool_calling,
            max_tokens,
            config,
            source_label: "Platform default".to_string(),
            brand_mark: "◎",
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

    let config = match model.model_id.as_str() {
        "bartowski/Qwen_Qwen3-4B-GGUF" => GgufModelConfig::qwen3_4b(),
        "bartowski/Qwen_Qwen3-8B-GGUF" => GgufModelConfig::qwen3_8b(),
        "bartowski/Qwen2.5-3B-Instruct-GGUF" => GgufModelConfig::qwen25_3b(),
        "bartowski/Qwen2.5-1.5B-Instruct-GGUF" => GgufModelConfig::qwen25_1_5b(),
        "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF" => GgufModelConfig::qwen25_coder_3b(),
        "bartowski/Qwen2.5-Coder-1.5B-Instruct-GGUF" => GgufModelConfig::qwen25_coder_1_5b(),
        _ => return None,
    };

    let tool_calling = model.model_id == "bartowski/Qwen_Qwen3-4B-GGUF"
        || model.model_id == "bartowski/Qwen_Qwen3-8B-GGUF";
    let max_tokens = if tool_calling { 4096 } else { 512 };

    Some(ModelPickerItem {
        display_name: config.display_name.clone(),
        description: config.approx_memory.clone(),
        tool_calling,
        max_tokens,
        config,
        source_label,
        brand_mark: if model.from_app_group { "◉" } else { "○" },
        source: if model.from_app_group {
            ModelSource::Onde
        } else {
            ModelSource::HuggingFace
        },
        cache_health: model.cache_health,
    })
}
