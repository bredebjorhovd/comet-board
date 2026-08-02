//! Model catalog + reasoning-effort mapping for opencode, mirroring
//! `crates/harness/src/codex/catalog.rs`.
//!
//! opencode serves a live model catalog over its HTTP server
//! (`GET /config/providers`): each provider's models carry a `variants` map
//! whose keys are the reasoning-effort options the model accepts (`low`,
//! `medium`, `high`, `max`, …). [`OpencodeHarness::models`] introspects that
//! endpoint through a short-lived server; [`static_models`] is the offline
//! fallback when introspection isn't reachable (and the seam live discovery is
//! spliced in at, exactly like codex's `model/list` seam).

use comet_proto::{Model, ReasoningLevel};

/// The unified reasoning ladder opencode models advertise across their
/// `variants` maps (the display set; the wire clamps per model — see
/// [`to_effort`]). Mirrored by the engine registry's lazy descriptor, so the
/// catalog entry never changes after the first resolve.
pub(crate) const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultra,
];

/// The variant name a [`ReasoningLevel`] maps to in a model's `variants` map.
/// Claude-only modes (ultracode, ultrathink) clamp to the nearest effort
/// opencode speaks.
pub(crate) fn to_effort(reasoning: Option<ReasoningLevel>) -> Option<&'static str> {
    Some(match reasoning? {
        ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultracode | ReasoningLevel::Ultrathink => "xhigh",
        ReasoningLevel::Max => "max",
        ReasoningLevel::Ultra => "ultra",
    })
}

/// The [`ReasoningLevel`] a variant key maps to, for live-catalog derivation.
/// Unknown keys are dropped.
pub(crate) fn level_for_variant(variant: &str) -> Option<ReasoningLevel> {
    Some(match variant {
        "minimal" => ReasoningLevel::Minimal,
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        "xhigh" => ReasoningLevel::XHigh,
        "max" => ReasoningLevel::Max,
        "ultra" => ReasoningLevel::Ultra,
        _ => return None,
    })
}

const DEFAULT_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::Max,
];

const FULL_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultra,
];

const MAX_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];

fn model(id: &str, label: &str, description: &str, ladder: &[ReasoningLevel]) -> Model {
    Model {
        id: id.into(),
        label: label.into(),
        description: (!description.is_empty()).then(|| description.into()),
        reasoning_levels: ladder.to_vec(),
        options: Vec::new(),
    }
}

/// The offline fallback catalog: a small snapshot of common opencode models
/// (id is `provider/model`, the CLI's own spelling). Used when a live server
/// can't be reached; introspection is the primary path.
pub(crate) fn static_models() -> Vec<Model> {
    vec![
        model(
            "opencode/big-pickle",
            "Big Pickle",
            "OpenCode's flagship model",
            FULL_LADDER,
        ),
        model(
            "deepseek/deepseek-v4-pro",
            "DeepSeek V4 Pro",
            "Frontier reasoning model",
            FULL_LADDER,
        ),
        model(
            "deepseek/deepseek-v4-flash",
            "DeepSeek V4 Flash",
            "Fast, capable everyday model",
            MAX_LADDER,
        ),
        model("openai/gpt-5.5", "GPT-5.5", "Frontier coding model", FULL_LADDER),
        model("openai/gpt-5.4", "GPT-5.4", "Reliable general coding", MAX_LADDER),
        model(
            "openai/gpt-5.4-mini",
            "GPT-5.4 Mini",
            "Small, fast and capable",
            DEFAULT_LADDER,
        ),
        model("anthropic/claude-sonnet-5", "Claude Sonnet 5", "Balanced agent work", FULL_LADDER),
        model("anthropic/claude-haiku-4-5", "Claude Haiku 4.5", "Fast everyday tasks", DEFAULT_LADDER),
    ]
}

/// Derive a comet [`Model`] from one opencode `/config/providers` model entry.
/// The display id is `provider/model` (round-trips through the harness, which
/// splits it on the run wire); reasoning levels come from the model's
/// `variants` keys when present, else the default ladder.
pub(crate) fn model_from_entry(provider_id: &str, value: &serde_json::Value) -> Option<Model> {
    let id = value.get("id").and_then(serde_json::Value::as_str)?;
    let label = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(id);
    let description = value
        .get("family")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let reasoning_levels: Vec<ReasoningLevel> = value
        .get("variants")
        .and_then(serde_json::Value::as_object)
        .map(|variants| {
            let mut levels: Vec<ReasoningLevel> = variants
                .keys()
                .filter_map(|k| level_for_variant(k))
                .collect();
            levels.sort_by_key(|l| *l);
            levels
        })
        .filter(|levels| !levels.is_empty())
        .unwrap_or_else(|| DEFAULT_LADDER.to_vec());
    Some(Model {
        id: format!("{provider_id}/{id}"),
        label: label.to_string(),
        description,
        reasoning_levels,
        options: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_maps_unified_levels() {
        assert_eq!(to_effort(None), None);
        assert_eq!(to_effort(Some(ReasoningLevel::Minimal)), Some("low"));
        assert_eq!(to_effort(Some(ReasoningLevel::Medium)), Some("medium"));
        assert_eq!(to_effort(Some(ReasoningLevel::High)), Some("high"));
        assert_eq!(to_effort(Some(ReasoningLevel::XHigh)), Some("xhigh"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultracode)), Some("xhigh"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultrathink)), Some("xhigh"));
        assert_eq!(to_effort(Some(ReasoningLevel::Max)), Some("max"));
        assert_eq!(to_effort(Some(ReasoningLevel::Ultra)), Some("ultra"));
    }

    #[test]
    fn variant_keys_map_to_levels() {
        assert_eq!(level_for_variant("high"), Some(ReasoningLevel::High));
        assert_eq!(level_for_variant("xhigh"), Some(ReasoningLevel::XHigh));
        assert_eq!(level_for_variant("bogus"), None);
    }

    #[test]
    fn live_entry_derives_reasoning_from_variants() {
        let value = serde_json::json!({
            "id": "deepseek-v4-flash",
            "providerID": "deepseek",
            "name": "DeepSeek V4 Flash",
            "family": "deepseek",
            "variants": { "high": {}, "max": {} },
        });
        let m = model_from_entry("deepseek", &value).unwrap();
        assert_eq!(m.id, "deepseek/deepseek-v4-flash");
        assert_eq!(m.label, "DeepSeek V4 Flash");
        assert_eq!(
            m.reasoning_levels,
            vec![ReasoningLevel::High, ReasoningLevel::Max]
        );
        assert_eq!(m.description.as_deref(), Some("deepseek"));
    }

    #[test]
    fn entry_without_variants_gets_default_ladder() {
        let value = serde_json::json!({ "id": "big-pickle", "name": "Big Pickle" });
        let m = model_from_entry("opencode", &value).unwrap();
        assert_eq!(m.id, "opencode/big-pickle");
        assert_eq!(m.reasoning_levels, DEFAULT_LADDER);
    }
}
