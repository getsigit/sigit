//! Inference provider configuration.
//!
//! Decides which backend serves inference. Resolution order, first match wins:
//!
//! 1. Override: `OPENAI_BASE_URL` + `OPENAI_API_KEY`, or the active profile in
//!    `~/.config/sigit/providers.toml`.
//! 2. siGit Code Cloud: used when the user is logged in (`sigit login`). The
//!    endpoint and tier are built in, and the session token is the credential.
//! 3. On-device: no login and no override, so inference runs locally.
//!
//! Provider resolution runs at startup in both modes: the interactive client
//! picks its whole backend from it, and `run_acp_server` applies the explicit
//! override (env or `providers.toml`) the same way. Some items are still wired
//! up only through `#[cfg(unix)]` interactive paths, so the dead-code lint
//! stays suppressed on non-Unix targets only — Unix builds keep full coverage.
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::PathBuf;

use serde::Deserialize;

/// Default siGit Code Cloud inference endpoint. This is the siGit platform API
/// (sigit.si), which authenticates the user and forwards to the inference
/// backend; the client never talks to the backend directly. Override with
/// `SIGIT_CLOUD_URL` (dev: `http://localhost:8088/api/v1`).
const DEFAULT_CLOUD_URL: &str = "https://sigit.si/api/v1";

/// The cloud quality tiers, in display order (each line ordered smallest to
/// largest). Always offered in `/models`; selecting one requires a signed-in
/// account.
///
/// The tiers group into several independent lines, and a tier is only routable
/// when the cloud has that line's family configured. Which family is which, and
/// what runs behind any of them, is deliberately not represented here: the
/// client knows tier names and nothing else.
///
/// This list mirrors onde-cloud's router catalogue (`src/router.rs`) and
/// sigit-si's `CloudCatalog`. A tier the cloud serves but this list omits is
/// simply invisible in `/models` and the editor picker, so all three sides have
/// to be updated together when a tier is added.
pub const CLOUD_TIERS: &[&str] = &[
    "fast", "balanced", "large", "mini", "air", "pro", "oke", "flux", "apex", "aura", "orbit",
    "nova", "flare", "zenith", "prism",
];

pub const DEFAULT_CLOUD_TIER: &str = "nova";

/// Cloud tiers that read images attached to a prompt.
///
/// This mirrors onde-cloud's `IMAGE_CANDIDATES` table, which is where the
/// per-provider facts live and where a request is actually accepted or
/// refused. A tier is listed here when at least one of its providers takes
/// images. The rest are text-only at the source: `nova` and `orbit` (GLM-5.3
/// and GLM-5.2), `apex` and `flux` (DeepSeek V4 Pro and V4 Flash).
///
/// The list can be hard-coded because the tier list itself is: a tier this
/// client does not know is not offered at all (see [`CLOUD_TIERS`]).
const IMAGE_TIERS: &[&str] = &[
    "fast", "balanced", "large", "mini", "air", "pro", "oke", "aura", "flare", "zenith", "prism",
];

/// Whether the cloud tier `tier` reads images.
pub fn tier_accepts_images(tier: &str) -> bool {
    IMAGE_TIERS.contains(&tier.trim().to_lowercase().as_str())
}

/// Whether the model id sent on the wire reads images.
///
/// An `onde-*` id is one of our tiers and is answered from [`IMAGE_TIERS`]. Any
/// other id belongs to an endpoint the user configured themselves
/// (`OPENAI_BASE_URL` or a provider profile). Nothing is known about those, so
/// the image is passed through and the endpoint gets to decide.
pub fn model_accepts_images(model: &str) -> bool {
    if !model.starts_with("onde-") {
        return true;
    }
    CLOUD_TIERS
        .iter()
        .any(|tier| tier_to_model(tier) == model && tier_accepts_images(tier))
}

/// Whether the model id sent on the wire takes audio.
///
/// No cloud tier does: onde-cloud drops `input_audio` parts when it parses a
/// request, so a clip sent to an `onde-*` id would vanish without a word. Any
/// other id is the user's own endpoint and, as with images, gets the audio and
/// decides for itself.
pub fn model_accepts_audio(model: &str) -> bool {
    !model.starts_with("onde-")
}

/// Base URL of the siGit Code Cloud inference endpoint. Override with
/// `SIGIT_CLOUD_URL` (dev: `http://localhost:8090/v1`).
pub fn cloud_base_url() -> String {
    std::env::var("SIGIT_CLOUD_URL").unwrap_or_else(|_| DEFAULT_CLOUD_URL.to_string())
}

/// Display label for a tier, e.g. `siGit Code Cloud · Balanced`.
pub fn cloud_tier_label(tier: &str) -> String {
    format!("siGit Code Cloud · {}", tier_title(tier))
}

/// Build a siGit Code Cloud provider for `tier`, if the user is signed in.
/// Returns `None` when there is no stored session, so the caller can prompt for
/// login rather than silently failing.
pub fn cloud_tier_provider(tier: &str) -> Option<ProviderConfig> {
    let token = crate::credentials::load_token()?;
    Some(ProviderConfig {
        display_name: cloud_tier_label(tier),
        base_url: cloud_base_url(),
        api_key: token,
        model: tier_to_model(tier),
    })
}

/// Map a neutral tier name to the model id sent on the wire. Unknown values pass
/// through unchanged so an explicit model id still works.
fn tier_to_model(tier: &str) -> String {
    match tier.trim().to_lowercase().as_str() {
        "fast" => "onde-fast",
        "balanced" => "onde-balanced",
        "large" => "onde-large",
        "mini" => "onde-mini",
        "air" => "onde-air",
        "pro" => "onde-pro",
        "oke" => "onde-kkk",
        "flux" => "onde-flux",
        "apex" => "onde-apex",
        "aura" => "onde-aura",
        "orbit" => "onde-orbit",
        "nova" => "onde-nova",
        "flare" => "onde-flare",
        "zenith" => "onde-zenith",
        "prism" => "onde-prism",
        other => other,
    }
    .to_string()
}

/// A resolved inference provider: everything needed to build an OpenAI-compatible
/// client. Deliberately free of any Onde/smbCloud-specific fields.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Human-facing name shown in the UI (e.g. `siGit Code Cloud · Balanced`).
    pub display_name: String,
    /// API root, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: String,
    /// Model id sent to the endpoint, e.g. `onde-balanced` or `gpt-4o-mini`.
    pub model: String,
}

/// Title-case a tier name for display (`balanced` → `Balanced`).
pub fn tier_title(tier: &str) -> String {
    let tier = tier.trim();
    let mut chars = tier.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Balanced".to_string(),
    }
}

/// Default model id used when an environment-configured provider omits one.
const DEFAULT_ENV_MODEL: &str = "onde-large";

/// Resolve the startup provider, or `None` to run on-device.
///
/// This is only the explicit override (env or `providers.toml`), for power users
/// and BYO endpoints. Being signed in does **not** auto-select the cloud: model
/// choice is separate from identity, and the user picks a model or tier in
/// `/models`. With no override, siGit Code is local-first.
pub fn active_provider() -> Option<ProviderConfig> {
    if let Some(config) = from_env() {
        return Some(config);
    }
    match from_config_file() {
        Ok(config) => config,
        Err(error) => {
            log::warn!("provider: ignoring providers.toml: {error}");
            None
        }
    }
}

/// Provider from environment variables, if both URL and key are present.
fn from_env() -> Option<ProviderConfig> {
    let base_url = non_empty(std::env::var("OPENAI_BASE_URL").ok())?;
    // A base URL with no key is almost always a mistake. Warn instead of
    // silently falling back to on-device, which looks like the cloud failed.
    let Some(api_key) = non_empty(std::env::var("OPENAI_API_KEY").ok()) else {
        log::warn!(
            "provider: OPENAI_BASE_URL is set but OPENAI_API_KEY is empty/missing; \
             staying on-device. Set OPENAI_API_KEY to use the remote provider."
        );
        return None;
    };
    let model = non_empty(std::env::var("SIGIT_MODEL").ok())
        .unwrap_or_else(|| DEFAULT_ENV_MODEL.to_string());
    Some(ProviderConfig {
        display_name: format!("{model} (custom endpoint)"),
        base_url,
        api_key,
        model,
    })
}

// ── providers.toml ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProvidersFile {
    /// Name of the profile to use.
    active: Option<String>,
    #[serde(default)]
    provider: Vec<ProviderEntry>,
}

#[derive(Debug, Deserialize)]
struct ProviderEntry {
    name: String,
    base_url: String,
    api_key: String,
    model: String,
}

/// Path to the providers file: `$SIGIT_CONFIG_DIR` or `~/.config/sigit/providers.toml`.
fn config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SIGIT_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("providers.toml"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/sigit/providers.toml"))
}

/// Load the active profile from `providers.toml`, if the file exists and names one.
fn from_config_file() -> Result<Option<ProviderConfig>, String> {
    let Some(path) = config_path() else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }

    let contents =
        std::fs::read_to_string(&path).map_err(|error| format!("read {path:?}: {error}"))?;
    let parsed: ProvidersFile =
        toml::from_str(&contents).map_err(|error| format!("parse {path:?}: {error}"))?;

    let Some(active) = parsed.active else {
        return Ok(None);
    };

    let entry = parsed
        .provider
        .into_iter()
        .find(|entry| entry.name == active)
        .ok_or_else(|| format!("active profile {active:?} not found"))?;

    Ok(Some(ProviderConfig {
        display_name: format!("{} ({})", entry.name, entry.model),
        base_url: entry.base_url,
        api_key: entry.api_key,
        model: entry.model,
    }))
}

/// Treat an empty/whitespace string as absent.
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|string| string.trim().to_string())
        .filter(|string| !string.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_active_profile_from_toml() {
        let toml = r#"
            active = "primary"

            [[provider]]
            name = "primary"
            base_url = "https://api.example.com/v1"
            api_key = "sk-test"
            model = "model-a"

            [[provider]]
            name = "openai"
            base_url = "https://api.openai.com/v1"
            api_key = "sk-other"
            model = "gpt-4o-mini"
        "#;
        let parsed: ProvidersFile = toml::from_str(toml).unwrap();
        let active = parsed.active.unwrap();
        let entry = parsed
            .provider
            .into_iter()
            .find(|entry| entry.name == active)
            .unwrap();
        assert_eq!(entry.base_url, "https://api.example.com/v1");
        assert_eq!(entry.model, "model-a");
    }

    #[test]
    fn non_empty_filters_blanks() {
        assert_eq!(non_empty(Some("  ".to_string())), None);
        assert_eq!(non_empty(Some(" x ".to_string())), Some("x".to_string()));
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn maps_default_cloud_tier_to_wire_model_id() {
        assert_eq!(DEFAULT_CLOUD_TIER, "nova");
        assert_eq!(tier_to_model(DEFAULT_CLOUD_TIER), "onde-nova");
    }

    #[test]
    fn maps_every_cloud_tier_to_its_own_onde_model_id() {
        for tier in CLOUD_TIERS {
            // `oke` is the one tier whose wire id is not its own name.
            let expected = match *tier {
                "oke" => "onde-kkk".to_string(),
                name => format!("onde-{name}"),
            };
            assert_eq!(tier_to_model(tier), expected);
        }
    }

    #[test]
    fn image_support_follows_the_tier_table() {
        assert!(tier_accepts_images("large"));
        assert!(tier_accepts_images("oke"));
        // The default tier is text-only at the source.
        assert!(!tier_accepts_images(DEFAULT_CLOUD_TIER));
        assert!(!tier_accepts_images("apex"));

        assert!(model_accepts_images("onde-large"));
        // `oke` is the one tier whose wire id is not `onde-<tier>`.
        assert!(model_accepts_images("onde-kkk"));
        assert!(!model_accepts_images("onde-nova"));
        // An onde id this client has never heard of is not assumed to work.
        assert!(!model_accepts_images("onde-something-new"));
        // A model on the user's own endpoint is theirs to judge.
        assert!(model_accepts_images("gpt-4o-mini"));

        for tier in IMAGE_TIERS {
            assert!(CLOUD_TIERS.contains(tier), "{tier} is not a cloud tier");
        }
    }

    #[test]
    fn audio_goes_only_to_the_users_own_endpoint() {
        for tier in CLOUD_TIERS {
            assert!(!model_accepts_audio(&tier_to_model(tier)), "{tier}");
        }
        assert!(model_accepts_audio("gpt-4o-audio-preview"));
    }
}
