//! Model picker types and item construction, shared across platforms.
//! The unix-only TUI in `chat.rs` pulls from here.

use onde::inference::GgufModelConfig;

use crate::setup::DiscoveredModel;

pub(crate) use crate::setup::ModelCacheHealth;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ModelSource {
    Onde,
    HuggingFace,
    /// not downloaded yet — selecting it triggers a download into the app-group cache.
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

/// map a HF model ID to its config constructor, or `None` if we don't support it.
pub(crate) fn model_id_to_config(model_id: &str) -> Option<GgufModelConfig> {
    Some(match model_id {
        "bartowski/Qwen_Qwen3-4B-GGUF" => GgufModelConfig::qwen3_4b(),
        "bartowski/Qwen_Qwen3-8B-GGUF" => GgufModelConfig::qwen3_8b(),
        "bartowski/Qwen_Qwen3-14B-GGUF" => GgufModelConfig::qwen3_14b(),
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

fn is_tool_calling(model_id: &str) -> bool {
    matches!(
        model_id,
        "bartowski/Qwen_Qwen3-4B-GGUF"
            | "bartowski/Qwen_Qwen3-8B-GGUF"
            | "bartowski/Qwen_Qwen3-14B-GGUF"
            | "bartowski/Qwen_Qwen3-1.7B-GGUF"
            | "bartowski/Qwen2.5-Coder-7B-Instruct-GGUF"
    )
}

/// tool-calling models get more tokens because `<think>` blocks eat into the budget.
fn max_tokens_for(model_id: &str) -> u64 {
    if is_tool_calling(model_id) { 4096 } else { 512 }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// collect every model the picker should show: local cache, remote available, fallback.
/// sorted by source (Onde > HF > Available > Fallback), then alphabetically.
pub(crate) fn build_model_picker_items() -> Vec<ModelPickerItem> {
    let mut items = Vec::new();

    // ── 1. Locally discovered models ─────────────────────────────────────
    for discovered in crate::setup::discover_local_models() {
        if let Some(item) = discovered_model_to_picker_item(discovered) {
            items.push(item);
        }
    }

    // ── 2. Supported models not yet downloaded ───────────────────────────
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
