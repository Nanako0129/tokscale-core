use once_cell::sync::Lazy;
use std::collections::HashMap;

static CURSOR_PRICING_ALIASES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut aliases = HashMap::new();
    for tier in [
        "cursor-grok-4.6-high",
        "cursor-grok-4.6-high-fast",
        "cursor-grok-4.6-low",
        "cursor-grok-4.6-low-fast",
        "cursor-grok-4.6-medium",
        "cursor-grok-4.6-medium-fast",
        "cursor-grok-4.6-xhigh",
    ] {
        aliases.insert(tier, "grok-4.6");
    }
    aliases.insert("grok-composer-2.5", "composer-2.5");
    aliases.insert("grok-composer-2.5-fast", "composer-2.5-fast");
    aliases
});

static MODEL_ALIASES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("big-pickle", "glm-4.7");
    m.insert("big pickle", "glm-4.7");
    m.insert("bigpickle", "glm-4.7");
    m.insert("k2p5", "kimi-k2-thinking");
    m.insert("k2-p5", "kimi-k2-thinking");
    m.insert("k2p6", "kimi-k2.6");
    m.insert("k2-p6", "kimi-k2.6");
    m.insert("kimi-k2p6", "kimi-k2.6");
    m.insert("kimi-k2.5-thinking", "kimi-k2-thinking");
    // Kimi CLI reports `kimi-for-coding` for Kimi K2.7 Code (its config sets
    // `display_name = "K2.7 Coding"`). It was previously aliased to k2.5, which
    // priced it about a third under the real rate.
    m.insert("kimi-for-coding", "kimi-k2.7-code");
    m.insert("kimi-for-coding-highspeed", "kimi-k2.7-code-highspeed");
    m.insert("k3", "kimi-k3");
    // The long-context spelling must resolve to the same row as bare `k3`.
    m.insert("k3-256k", "kimi-k3");
    // Kimi Work (the Kimi desktop app's agent mode) embeds the same kimi-code
    // kernel and writes the same wire protocol, but reports its own ids.
    // Unaliased they fuzzy-match badly: upstream measured `k2d6-agent` landing
    // on `xai/grok-4.20-multi-agent-beta-0309`.
    m.insert("k2d6-agent", "kimi-k2.6");
    m.insert("k3-agent", "kimi-k3");
    m.insert("k3-agent-swarm", "kimi-k3");

    m.insert("model_placeholder_m26", "claude-opus-4-6");
    m.insert("model_placeholder_m35", "claude-sonnet-4-6");
    m.insert("model_placeholder_m36", "gemini-3.1-pro");
    m.insert("model_placeholder_m37", "gemini-3.1-pro");
    // Antigravity uses opaque placeholder IDs in IDE metadata and shorter
    // responseModel aliases in CLI conversation protobufs. Keep these as
    // machine-ID aliases rather than display labels because labels may be
    // renamed or localized.
    //
    // M133/`gemini-3-flash-b`, `gemini-3-flash-a`, and M187/raw
    // `gemini-3.5-flash-low` are source-verified exceptions to the obvious
    // mapping: M133 and both response aliases are the High tier; the raw
    // `gemini-3.5-flash-low` wire value is the Medium tier; M187 is the true
    // Low tier with its distinct machine id.
    m.insert("model_placeholder_m16", "gemini-3.1-pro");
    m.insert("model_placeholder_m18", "gemini-3-flash-preview");
    m.insert("model_placeholder_m84", "gemini-3-flash-preview");
    m.insert("model_placeholder_m132", "gemini-3.5-flash-high");
    m.insert("model_placeholder_m133", "gemini-3.5-flash-high");
    m.insert("model_placeholder_m187", "gemini-3.5-flash-extra-low");
    m.insert("model_placeholder_m20", "gemini-3.5-flash-medium");
    m.insert("gemini-pro-default", "gemini-3.1-pro");
    m.insert("gemini-pro-agent", "gemini-3.1-pro");
    m.insert("gemini-3-flash-agent", "gemini-3.5-flash-high");
    m.insert("gemini-3-flash-b", "gemini-3.5-flash-high");
    m.insert("gemini-3.5-flash-low", "gemini-3.5-flash-medium");
    m.insert("model_placeholder_m47", "gemini-3-flash-preview");
    m.insert("model_openai_gpt_oss_120b_medium", "gpt-oss-120b-medium");
    m.insert("claude-opus-4-6-thinking", "claude-opus-4-6");
    m.insert("claude-sonnet-4-6-thinking", "claude-sonnet-4-6");
    m.insert("claude-opus-4.6-thinking", "claude-opus-4-6");
    m.insert("claude-sonnet-4.6-thinking", "claude-sonnet-4-6");
    m.insert("claude-opus-4-6", "claude-opus-4-6");
    m.insert("claude-sonnet-4-6", "claude-sonnet-4-6");
    m.insert("claude-haiku-4-6", "claude-haiku-4-6");
    m.insert("claude-opus-4.6", "claude-opus-4-6");
    m.insert("claude-sonnet-4.6", "claude-sonnet-4-6");
    m.insert("claude-haiku-4.6", "claude-haiku-4-6");
    // GitHub Copilot reports Claude 4.1 without the separator; resolve it the
    // same way github_copilot/gpt-4o already resolves to gpt-4o.
    // Deliberately opus-only: `claude-sonnet-4-1` resolves cross-vendor to
    // `databricks/databricks-claude-sonnet-4-1`, so aliasing the Copilot
    // spelling onto it would route Sonnet 4.1 usage to Databricks rates.
    m.insert("claude-opus-41", "claude-opus-4-1");
    // Anthropic's "-0" suffix is their documented moving alias for the latest
    // snapshot of a model line. Upstream also aliases `claude-opus-4-0` to the
    // bare `claude-opus-4`; that one is not taken here because, without a
    // provider hint, bare `claude-opus-4` resolves in this tree to an Opus 4.5
    // reseller row at a third of the Opus 4 rate.
    m.insert("claude-sonnet-4-0", "claude-sonnet-4");
    m.insert("anthropic/claude-4-5-opus", "claude-opus-4-5");
    m.insert("anthropic/claude-4-5-sonnet", "claude-sonnet-4-5");
    m.insert("anthropic/claude-4-5-haiku", "claude-haiku-4-5");
    m.insert("anthropic/claude-4-6-opus", "claude-opus-4-6");
    m.insert("anthropic/claude-4-6-sonnet", "claude-sonnet-4-6");
    m.insert("anthropic/claude-4-6-haiku", "claude-haiku-4-6");
    m.insert("gemini-3.1-pro-high", "gemini-3.1-pro");
    m.insert("gemini-3.1-pro-low", "gemini-3.1-pro");
    m.insert("gemini-3-pro-high", "gemini-3-pro");
    m.insert("gemini-3-pro-low", "gemini-3-pro");
    m.insert("gemini-3-flash", "gemini-3-flash-preview");
    m.insert("gemini-3-flash-c", "gemini-3-flash-preview");
    m.insert("gemini-3-flash-a", "gemini-3.5-flash-high");
    // OpenAI documents the API spelling below as a moving alias for
    // `gpt-5.6-sol`; Codex records the same alias with its `gpt-` prefix.
    // Keep the API, Codex, and provider-qualified spellings pinned to the
    // currently documented target so the upstream GPT-5.6 Sol row supplies
    // all token-bucket rates. The qualified form must be explicit because
    // provider-prefix stripping does not run alias resolution a second time.
    // Sources (accessed 2026-08-17):
    // https://developers.openai.com/api/docs/guides/safety-checks/cybersecurity
    // https://developers.openai.com/api/docs/pricing
    m.insert("daybreak-blue-latest", "gpt-5.6-sol");
    m.insert("gpt-daybreak-blue-latest", "gpt-5.6-sol");
    m.insert("openai/gpt-daybreak-blue-latest", "gpt-5.6-sol");
    m.insert("openai/daybreak-blue-latest", "gpt-5.6-sol");

    // Synthetic model variants (only where resolver needs help)
    m.insert("kimi-k2.5-nvfp4", "kimi-k2.5"); // Quantization variant → base model pricing
    m.insert("kimi-k2-instruct-0905", "kimi-k2.5"); // Specific version → base (avoids reseller)
    m
});

pub fn resolve_alias(model_id: &str) -> Option<&'static str> {
    let lowered = model_id.to_lowercase();
    if let Some(target) = MODEL_ALIASES.get(lowered.as_str()) {
        return Some(target);
    }
    if let Some(target) = CURSOR_PRICING_ALIASES.get(lowered.as_str()) {
        return Some(target);
    }
    // kimi-code reports some rows as `kimi-code/<id>`. The Kimi parser strips
    // that prefix before pricing, but any other path reaching pricing with the
    // qualified form would otherwise miss every alias above, so the qualified
    // and bare spellings of the same model would disagree.
    let bare = lowered.strip_prefix("kimi-code/")?;
    MODEL_ALIASES.get(bare).copied()
}

pub fn uses_cursor_pricing(model_id: &str) -> bool {
    CURSOR_PRICING_ALIASES.contains_key(model_id.to_lowercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::{resolve_alias, uses_cursor_pricing};
    use std::collections::HashMap;

    #[test]
    fn resolves_antigravity_placeholders() {
        let cases = [
            ("MODEL_PLACEHOLDER_M26", "claude-opus-4-6"),
            ("model_placeholder_m37", "gemini-3.1-pro"),
            ("model_placeholder_m16", "gemini-3.1-pro"),
            ("model_placeholder_m18", "gemini-3-flash-preview"),
            ("MODEL_PLACEHOLDER_M84", "gemini-3-flash-preview"),
            ("model_placeholder_m132", "gemini-3.5-flash-high"),
            ("model_placeholder_m133", "gemini-3.5-flash-high"),
            ("model_placeholder_m187", "gemini-3.5-flash-extra-low"),
            ("model_placeholder_m20", "gemini-3.5-flash-medium"),
            ("gemini-pro-default", "gemini-3.1-pro"),
            ("gemini-pro-agent", "gemini-3.1-pro"),
            ("gemini-3-flash-agent", "gemini-3.5-flash-high"),
            ("gemini-3-flash-b", "gemini-3.5-flash-high"),
            ("gemini-3.5-flash-low", "gemini-3.5-flash-medium"),
            ("MODEL_OPENAI_GPT_OSS_120B_MEDIUM", "gpt-oss-120b-medium"),
            ("gemini-3-flash-c", "gemini-3-flash-preview"),
            ("gemini-3-flash-a", "gemini-3.5-flash-high"),
            ("claude-opus-4.6-thinking", "claude-opus-4-6"),
            ("anthropic/claude-4-5-haiku", "claude-haiku-4-5"),
            ("anthropic/claude-4-6-sonnet", "claude-sonnet-4-6"),
        ];

        for (raw, expected) in cases {
            assert_eq!(resolve_alias(raw), Some(expected), "raw model: {raw}");
        }
    }

    #[test]
    fn resolves_kimi_k2p6_aliases_without_regressing_k2p5() {
        assert_eq!(resolve_alias("k2p6"), Some("kimi-k2.6"));
        assert_eq!(resolve_alias("k2-p6"), Some("kimi-k2.6"));
        assert_eq!(resolve_alias("kimi-k2p6"), Some("kimi-k2.6"));
        assert_eq!(resolve_alias("KIMI-K2P6"), Some("kimi-k2.6"));

        assert_eq!(resolve_alias("k2p5"), Some("kimi-k2-thinking"));
        assert_eq!(resolve_alias("k2-p5"), Some("kimi-k2-thinking"));
    }

    #[test]
    fn resolves_openai_daybreak_blue_aliases_to_gpt_5_6_sol() {
        for model_id in [
            "daybreak-blue-latest",
            "gpt-daybreak-blue-latest",
            "GPT-DAYBREAK-BLUE-LATEST",
            // Prefix stripping does not re-run alias resolution, so neither
            // qualified spelling can fall back to its bare form.
            "openai/daybreak-blue-latest",
            "openai/gpt-daybreak-blue-latest",
        ] {
            assert_eq!(resolve_alias(model_id), Some("gpt-5.6-sol"), "{model_id}");
        }
    }

    #[test]
    fn codex_daybreak_blue_usage_uses_the_underlying_openai_price() {
        let pricing = super::super::litellm::ModelPricing {
            input_cost_per_token: Some(5e-6),
            output_cost_per_token: Some(30e-6),
            cache_read_input_token_cost: Some(0.5e-6),
            cache_creation_input_token_cost: Some(6.25e-6),
            ..Default::default()
        };
        let service = super::super::PricingService::new(
            HashMap::from([("gpt-5.6-sol".to_string(), pricing)]),
            HashMap::new(),
        );
        let usage = crate::TokenBreakdown {
            input: 1_000,
            output: 100,
            cache_read: 500,
            cache_write: 200,
            ..Default::default()
        };

        let expected = 1_000.0 * 5e-6 + 100.0 * 30e-6 + 500.0 * 0.5e-6 + 200.0 * 6.25e-6;
        for model_id in [
            "daybreak-blue-latest",
            "gpt-daybreak-blue-latest",
            "openai/daybreak-blue-latest",
            "openai/gpt-daybreak-blue-latest",
        ] {
            let result = service
                .lookup_with_source_and_provider(model_id, None, Some("openai"))
                .expect("the Codex alias must resolve to the GPT-5.6 Sol row");
            assert_eq!(result.source, "LiteLLM");
            assert_eq!(result.matched_key, "gpt-5.6-sol");

            let cost = service.calculate_cost_with_provider(model_id, Some("openai"), &usage);
            assert!(
                (cost - expected).abs() < 1e-12,
                "unexpected cost for {model_id}: {cost}"
            );
        }
    }

    #[test]
    fn resolves_kimi_coding_plan_and_work_ids_to_their_underlying_models() {
        assert_eq!(resolve_alias("k3"), Some("kimi-k3"));
        assert_eq!(resolve_alias("k3-256k"), Some("kimi-k3"));
        assert_eq!(resolve_alias("k2d6-agent"), Some("kimi-k2.6"));
        assert_eq!(resolve_alias("k3-agent"), Some("kimi-k3"));
        assert_eq!(resolve_alias("k3-agent-swarm"), Some("kimi-k3"));
        assert_eq!(resolve_alias("K3-AGENT-SWARM"), Some("kimi-k3"));
    }

    #[test]
    fn kimi_for_coding_prices_as_k2p7_code_not_k2p5() {
        // Kimi CLI's own config names this "K2.7 Coding"; the previous k2.5
        // target priced it about a third under the real rate.
        assert_eq!(resolve_alias("kimi-for-coding"), Some("kimi-k2.7-code"));
        assert_eq!(
            resolve_alias("kimi-for-coding-highspeed"),
            Some("kimi-k2.7-code-highspeed")
        );
    }

    #[test]
    fn qualified_kimi_code_ids_resolve_like_their_bare_form() {
        for bare in ["k3", "kimi-for-coding", "k2d6-agent", "k3-agent"] {
            let qualified = format!("kimi-code/{bare}");
            assert_eq!(
                resolve_alias(&qualified),
                resolve_alias(bare),
                "qualified id {qualified} must resolve like {bare}"
            );
            assert!(resolve_alias(&qualified).is_some());
        }
        assert_eq!(resolve_alias("kimi-code/not-a-real-model"), None);
    }

    #[test]
    fn cursor_grok_reasoning_tiers_resolve_to_the_base_model() {
        for tier in [
            "cursor-grok-4.6-high",
            "cursor-grok-4.6-high-fast",
            "cursor-grok-4.6-low",
            "cursor-grok-4.6-low-fast",
            "cursor-grok-4.6-medium",
            "cursor-grok-4.6-medium-fast",
            "cursor-grok-4.6-xhigh",
        ] {
            assert_eq!(resolve_alias(tier), Some("grok-4.6"), "tier: {tier}");
            assert!(uses_cursor_pricing(tier), "tier: {tier}");
        }
        assert_eq!(resolve_alias("grok-composer-2.5"), Some("composer-2.5"));
        assert!(uses_cursor_pricing("grok-composer-2.5-fast"));
        assert!(!uses_cursor_pricing("grok-4.6"));
    }

    #[test]
    fn cursor_pricing_alias_keys_stay_disjoint_from_model_aliases() {
        // `resolve_alias` consults MODEL_ALIASES first, so a key present in
        // both maps would resolve through MODEL_ALIASES while
        // `uses_cursor_pricing` still forced the Cursor catalog for it.
        for key in super::CURSOR_PRICING_ALIASES.keys() {
            assert!(
                !super::MODEL_ALIASES.contains_key(key),
                "{key} is in both CURSOR_PRICING_ALIASES and MODEL_ALIASES"
            );
        }
    }

    #[test]
    fn copilot_opus_41_and_sonnet_4_0_resolve_and_the_deferred_aliases_stay_absent() {
        assert_eq!(resolve_alias("claude-opus-41"), Some("claude-opus-4-1"));
        assert_eq!(resolve_alias("Claude-Opus-41"), Some("claude-opus-4-1"));
        assert_eq!(resolve_alias("claude-sonnet-4-0"), Some("claude-sonnet-4"));
        assert_eq!(resolve_alias("Claude-Sonnet-4-0"), Some("claude-sonnet-4"));
        // `claude-sonnet-4-1` resolves cross-vendor today, so no Copilot alias.
        assert_eq!(resolve_alias("claude-sonnet-41"), None);
        // Deferred until bare `claude-opus-4` resolves to the right model
        // without a hint.
        assert_eq!(resolve_alias("claude-opus-4-0"), None);
    }

    #[test]
    fn antigravity_low_and_medium_aliases_remain_distinct() {
        let low = resolve_alias("model_placeholder_m187").unwrap();
        let medium = resolve_alias("model_placeholder_m20").unwrap();
        let cli_medium = resolve_alias("gemini-3.5-flash-low").unwrap();

        assert_eq!(low, "gemini-3.5-flash-extra-low");
        assert_eq!(medium, "gemini-3.5-flash-medium");
        assert_ne!(low, medium);
        assert_eq!(cli_medium, medium);
    }
}
