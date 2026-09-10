use std::collections::HashMap;
use tokscale_core::pricing::{lookup::PricingLookup, ModelPricing};

#[test]
fn rebuilding_identical_price_catalogs_preserves_fallback_cost() {
    // Neither catalog has the bare model. Equally long dated candidates must
    // not turn HashMap's random seed into a different rate after a refresh.
    let candidates = [
        ("openai/gpt-5.2-2026-01-01", 0.000010),
        ("openai/gpt-5.2-2026-02-01", 0.000008),
    ];
    for source in ["litellm", "openrouter"] {
        for _ in 0..128 {
            let prices: HashMap<_, _> = candidates
                .iter()
                .map(|(key, rate)| {
                    (
                        (*key).to_owned(),
                        ModelPricing {
                            input_cost_per_token: Some(*rate),
                            output_cost_per_token: Some(*rate),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            let (litellm, openrouter) = if source == "litellm" {
                (prices, HashMap::new())
            } else {
                (HashMap::new(), prices)
            };
            let lookup = PricingLookup::new(litellm, openrouter, HashMap::new());
            let matched = lookup
                .lookup_with_source_and_provider("gpt-5.2", Some(source), Some("openai"))
                .expect("dated fallback is priced");
            assert_eq!(matched.matched_key, candidates[0].0, "{source}");
            assert_eq!(
                lookup.calculate_cost("gpt-5.2", 1_000_000_000, 0, 0, 0, 0),
                10_000.0,
                "{source} must preserve the same cost across snapshot rebuilds"
            );
        }
    }
}
