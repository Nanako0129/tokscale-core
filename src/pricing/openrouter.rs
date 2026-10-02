use super::cache;
use super::litellm::ModelPricing;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Semaphore;

const CACHE_FILENAME: &str = "pricing-openrouter.json";
const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 200;
const MAX_CONCURRENT_REQUESTS: usize = 10;

/// Structs for `/api/v1/models` endpoint (list all models).

#[derive(Deserialize)]
struct ModelListPricing {
    prompt: String,
    completion: String,
}

#[derive(Deserialize)]
struct ModelListItem {
    id: String,
    pricing: Option<ModelListPricing>,
}

#[derive(Deserialize)]
struct ModelsListResponse {
    data: Vec<ModelListItem>,
}

/// Structs for `/api/v1/models/{id}/endpoints` endpoint (author pricing).

#[derive(Deserialize)]
struct EndpointPricing {
    prompt: String,
    completion: String,
    #[serde(default)]
    input_cache_read: Option<String>,
    #[serde(default)]
    input_cache_write: Option<String>,
}

#[derive(Deserialize)]
struct Endpoint {
    provider_name: String,
    pricing: EndpointPricing,
}

#[derive(Deserialize)]
struct EndpointData {
    #[allow(dead_code)]
    id: String,
    endpoints: Vec<Endpoint>,
}

#[derive(Deserialize)]
struct EndpointsResponse {
    data: EndpointData,
}

/// Model ID prefix to provider name mapping.
///
/// Translates model ID prefixes like `z-ai` to their corresponding
/// provider names in the endpoints API, such as `Z.AI`.
fn get_author_provider_name(model_id: &str) -> Option<&'static str> {
    let prefix = model_id.split('/').next()?;

    match prefix.to_lowercase().as_str() {
        "z-ai" => Some("Z.AI"),
        "x-ai" => Some("xAI"),
        "anthropic" => Some("Anthropic"),
        "openai" => Some("OpenAI"),
        "google" => Some("Google"),
        "meta-llama" => Some("Meta"),
        "mistralai" => Some("Mistral"),
        "deepseek" => Some("DeepSeek"),
        "qwen" => Some("Alibaba"),
        "cohere" => Some("Cohere"),
        "perplexity" => Some("Perplexity"),
        "moonshotai" => Some("Moonshot AI"),
        _ => None,
    }
}

pub fn load_cached() -> Option<HashMap<String, ModelPricing>> {
    cache::load_cache(CACHE_FILENAME)
}

pub fn load_cached_any_age() -> Option<HashMap<String, ModelPricing>> {
    cache::load_cache_any_age(CACHE_FILENAME)
}

pub(crate) fn load_cached_any_age_from_dir(
    cache_dir: &Path,
) -> Option<HashMap<String, ModelPricing>> {
    cache::load_cache_any_age_from_dir(cache_dir, CACHE_FILENAME)
}

fn parse_price(s: &str) -> Option<f64> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

async fn fetch_author_pricing(
    client: Arc<reqwest::Client>,
    model_id: String,
    semaphore: Arc<Semaphore>,
    fallback_pricing: Option<ModelPricing>,
) -> Option<(String, ModelPricing)> {
    let _permit = semaphore.acquire().await.ok()?;

    let author_name = match get_author_provider_name(&model_id) {
        Some(name) => name,
        None => return fallback_pricing.map(|p| (model_id, p)),
    };

    let url = format!("https://openrouter.ai/api/v1/models/{}/endpoints", model_id);

    let response = match client
        .get(&url)
        .header("Content-Type", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return fallback_pricing.map(|p| (model_id, p));
        }
    };

    if !response.status().is_success() {
        return fallback_pricing.map(|p| (model_id, p));
    }

    let data: EndpointsResponse = match response.json().await {
        Ok(d) => d,
        Err(_) => {
            return fallback_pricing.map(|p| (model_id, p));
        }
    };

    match select_author_endpoint_pricing(
        &data.data.endpoints,
        author_name,
        fallback_pricing.as_ref(),
    ) {
        Some(pricing) => Some((model_id, pricing)),
        None => fallback_pricing.map(|p| (model_id, p)),
    }
}

fn endpoint_pricing(endpoint: &Endpoint) -> Option<ModelPricing> {
    Some(ModelPricing {
        input_cost_per_token: Some(parse_price(&endpoint.pricing.prompt)?),
        output_cost_per_token: Some(parse_price(&endpoint.pricing.completion)?),
        cache_read_input_token_cost: endpoint
            .pricing
            .input_cache_read
            .as_deref()
            .and_then(parse_price),
        cache_creation_input_token_cost: endpoint
            .pricing
            .input_cache_write
            .as_deref()
            .and_then(parse_price),
        ..Default::default()
    })
}

fn quotes_same_base_price(candidate: &ModelPricing, listed: &ModelPricing) -> bool {
    let same = |candidate: Option<f64>, listed: Option<f64>| match (candidate, listed) {
        (Some(candidate), Some(listed)) => (candidate - listed).abs() <= listed.abs() * 1e-9,
        _ => false,
    };
    same(candidate.input_cost_per_token, listed.input_cost_per_token)
        && same(
            candidate.output_cost_per_token,
            listed.output_cost_per_token,
        )
}

/// Price a model from its author's own OpenRouter endpoint.
///
/// One author can serve the same model from several endpoints that differ only
/// by service tier — `openai` against `openai/flex` and `openai/fast`,
/// `google-vertex/global` against `.../flex` and `.../priority` — and all of
/// them report the same `provider_name`. Taking the first match therefore made
/// the stored price depend on OpenRouter's response ordering, which currently
/// returns the discounted Flex tier first: on 2026-10-02 the stored price was
/// the Flex rate for 32 of 319 models. Flex is a real price but not the one
/// that ran: it has to be opted into per request, and Codex records
/// `"service_tier":"priority"` or `"default"`, never `flex`. The listed price
/// on `/models` is the standard tier for 31 of those 32, so it breaks the tie
/// between endpoints of one author. The exception is `openai/gpt-5.6-sol-pro`,
/// whose listed price equals its `openai/fast` endpoint, so it is stored at the
/// fast rate. `tag` is deliberately not used here: tier naming is not uniform
/// across authors, so it would need per-author rules where the listed price
/// needs none.
///
/// With no author endpoint this returns `None` and the caller keeps the listed
/// price, as before. With no listed price (missing or unparseable on
/// `/models`) there is nothing to break the tie with, so the first author
/// endpoint is kept, which may be a discounted tier.
fn select_author_endpoint_pricing(
    endpoints: &[Endpoint],
    author_name: &str,
    listed: Option<&ModelPricing>,
) -> Option<ModelPricing> {
    let author_endpoints: Vec<&Endpoint> = endpoints
        .iter()
        .filter(|e| e.provider_name == author_name)
        .collect();
    let first = author_endpoints.first()?;

    if let Some(listed) = listed {
        let standard_tier = fewest_unpriceable_buckets(
            author_endpoints
                .iter()
                .copied()
                .filter_map(endpoint_pricing)
                .filter(|pricing| quotes_same_base_price(pricing, listed)),
        );
        // The first match stands only when no author endpoint quotes the
        // listed price, so a model whose author price legitimately differs
        // from the listed one is left exactly as it was.
        if standard_tier.is_some() {
            return standard_tier;
        }
    }
    endpoint_pricing(first)
}

/// Reduce equally-priced candidates to the one that leaves the fewest token
/// buckets unpriceable.
///
/// Cache read and cache write are independent fields, so the endpoint
/// publishing the most of them wins. On an equal count, retain cache-read
/// pricing: it is the bucket required by Codex usage and must not be lost to
/// an earlier write-only endpoint.
fn fewest_unpriceable_buckets(
    candidates: impl Iterator<Item = ModelPricing>,
) -> Option<ModelPricing> {
    candidates.reduce(|best, candidate| {
        if published_cache_rates(&candidate) > published_cache_rates(&best)
            || (published_cache_rates(&candidate) == published_cache_rates(&best)
                && candidate.cache_read_input_token_cost.is_some()
                && best.cache_read_input_token_cost.is_none())
        {
            candidate
        } else {
            best
        }
    })
}

fn published_cache_rates(pricing: &ModelPricing) -> usize {
    usize::from(pricing.cache_read_input_token_cost.is_some())
        + usize::from(pricing.cache_creation_input_token_cost.is_some())
}

/// Fetch all models and get author pricing for each
pub async fn fetch_all_models() -> HashMap<String, ModelPricing> {
    if let Some(cached) = load_cached() {
        return cached;
    }

    let client = Arc::new(
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default(),
    );

    let mut last_error: Option<String> = None;

    let models_with_fallback: Vec<(String, Option<ModelPricing>)> = 'retry: {
        for attempt in 0..MAX_RETRIES {
            let response = match client
                .get(MODELS_URL)
                .header("Content-Type", "application/json")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_error = Some(format!("network error: {}", e));
                    if attempt < MAX_RETRIES - 1 {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            INITIAL_BACKOFF_MS * (1 << attempt),
                        ))
                        .await;
                    }
                    continue;
                }
            };

            let status = response.status();
            if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                last_error = Some(format!("HTTP {}", status));
                let _ = response.bytes().await;
                if attempt < MAX_RETRIES - 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        INITIAL_BACKOFF_MS * (1 << attempt),
                    ))
                    .await;
                }
                continue;
            }

            if !status.is_success() {
                eprintln!("[tokscale] OpenRouter models API returned {}", status);
                break 'retry Vec::new();
            }

            let data: ModelsListResponse = match response.json().await {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[tokscale] OpenRouter models JSON parse failed: {}", e);
                    break 'retry Vec::new();
                }
            };

            break 'retry data
                .data
                .into_iter()
                .map(|m| {
                    let fallback = m.pricing.and_then(|p| {
                        let input = parse_price(&p.prompt)?;
                        let output = parse_price(&p.completion)?;
                        Some(ModelPricing {
                            input_cost_per_token: Some(input),
                            output_cost_per_token: Some(output),
                            cache_read_input_token_cost: None,
                            cache_creation_input_token_cost: None,
                            ..Default::default()
                        })
                    });
                    (m.id, fallback)
                })
                .collect();
        }

        if let Some(err) = &last_error {
            eprintln!(
                "[tokscale] OpenRouter fetch failed after {} retries: {}",
                MAX_RETRIES, err
            );
        }
        Vec::new()
    };

    if models_with_fallback.is_empty() {
        return HashMap::new();
    }

    let models_with_authors: Vec<(String, Option<ModelPricing>)> = models_with_fallback
        .into_iter()
        .filter(|(id, _)| get_author_provider_name(id).is_some())
        .collect();

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));

    let mut handles = Vec::with_capacity(models_with_authors.len());

    for (model_id, fallback) in models_with_authors {
        let client = Arc::clone(&client);
        let sem = Arc::clone(&semaphore);

        let handle =
            tokio::spawn(
                async move { fetch_author_pricing(client, model_id, sem, fallback).await },
            );

        handles.push(handle);
    }

    // Collect results
    let mut result = HashMap::new();

    for handle in handles {
        if let Ok(Some((model_id, pricing))) = handle.await {
            result.insert(model_id, pricing);
        }
    }

    if !result.is_empty() {
        if let Err(e) = cache::save_cache(CACHE_FILENAME, &result) {
            eprintln!(
                "[tokscale] Warning: Failed to cache OpenRouter pricing at {}: {}",
                cache::get_cache_path(CACHE_FILENAME).display(),
                e
            );
        }
    }

    result
}

pub async fn fetch_all_mapped() -> HashMap<String, ModelPricing> {
    fetch_all_models().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(
        provider_name: &str,
        prompt: &str,
        completion: &str,
        input_cache_read: Option<&str>,
    ) -> Endpoint {
        Endpoint {
            provider_name: provider_name.to_string(),
            pricing: EndpointPricing {
                prompt: prompt.to_string(),
                completion: completion.to_string(),
                input_cache_read: input_cache_read.map(str::to_string),
                input_cache_write: None,
            },
        }
    }

    fn listed(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input_cost_per_token: Some(input),
            output_cost_per_token: Some(output),
            ..Default::default()
        }
    }

    // OpenRouter serves `openai/gpt-6-astra` from three `OpenAI` endpoints that
    // differ only by service tier, and returns the discounted `openai/flex` one
    // first. Selecting by `provider_name` alone therefore stored $5/$25 for a
    // model whose standard rate — and whose `/models` listed price — is
    // $10/$50, halving every reported cost for it.
    #[test]
    fn a_discounted_tier_does_not_outrank_the_standard_author_endpoint() {
        let endpoints = vec![
            endpoint("OpenAI", "0.000005", "0.000025", Some("0.0000005")),
            endpoint("Azure", "0.00001", "0.00005", Some("0.000001")),
            endpoint("OpenAI", "0.00001", "0.00005", Some("0.000001")),
        ];

        let pricing =
            select_author_endpoint_pricing(&endpoints, "OpenAI", Some(&listed(1e-5, 5e-5)))
                .unwrap();

        assert_eq!(pricing.input_cost_per_token, Some(1e-5));
        assert_eq!(pricing.output_cost_per_token, Some(5e-5));
        assert_eq!(pricing.cache_read_input_token_cost, Some(1e-6));
    }

    // The error is not one-directional: `openai/fast` is 2x the standard rate,
    // so the same ordering dependency overcharges if OpenRouter ever returns
    // that tier first.
    #[test]
    fn a_premium_tier_does_not_outrank_the_standard_author_endpoint() {
        let endpoints = vec![
            endpoint("OpenAI", "0.00002", "0.0001", Some("0.000002")),
            endpoint("OpenAI", "0.00001", "0.00005", Some("0.000001")),
        ];

        let pricing =
            select_author_endpoint_pricing(&endpoints, "OpenAI", Some(&listed(1e-5, 5e-5)))
                .unwrap();

        assert_eq!(pricing.input_cost_per_token, Some(1e-5));
        assert_eq!(pricing.output_cost_per_token, Some(5e-5));
    }

    // Tiering is not an OpenAI shape. Google serves `gemini-3.6-flash` from
    // `google-vertex/global/flex` before `google-vertex/global`, both as
    // `Google`.
    #[test]
    fn the_tier_rule_is_not_specific_to_one_author() {
        let endpoints = vec![
            endpoint("Google", "0.000000375", "0.000001875", Some("0.0000000375")),
            endpoint("Google AI Studio", "0.00000075", "0.00000375", None),
            endpoint("Google", "0.00000075", "0.00000375", Some("0.000000075")),
        ];

        let pricing =
            select_author_endpoint_pricing(&endpoints, "Google", Some(&listed(7.5e-7, 3.75e-6)))
                .unwrap();

        assert_eq!(pricing.input_cost_per_token, Some(7.5e-7));
        assert_eq!(pricing.output_cost_per_token, Some(3.75e-6));
        assert_eq!(pricing.cache_read_input_token_cost, Some(7.5e-8));
    }

    // Z.AI quantization tiers (`z-ai/fp4`, `z-ai/fp8`) quote prices that match
    // no listed price, and the listed price there is a reseller's. Those models
    // must keep resolving exactly as before, so the tier rule stays additive.
    #[test]
    fn author_endpoint_still_wins_when_none_quotes_the_listed_price() {
        let endpoints = vec![
            endpoint("Z.AI", "0.0000006", "0.0000022", None),
            endpoint("Z.AI", "0.0000008", "0.0000029", None),
        ];

        let pricing =
            select_author_endpoint_pricing(&endpoints, "Z.AI", Some(&listed(4e-7, 2e-6))).unwrap();

        assert_eq!(pricing.input_cost_per_token, Some(6e-7));
        assert_eq!(pricing.output_cost_per_token, Some(2.2e-6));
    }

    // Two author endpoints quote the listed price; the one that publishes a
    // cache-read rate wins over the one that does not, whatever the order.
    #[test]
    fn equally_priced_author_endpoints_keep_the_one_with_a_cache_read_rate() {
        let endpoints = vec![
            endpoint("OpenAI", "0.00001", "0.00005", None),
            endpoint("OpenAI", "0.00001", "0.00005", Some("0.000001")),
        ];

        let pricing =
            select_author_endpoint_pricing(&endpoints, "OpenAI", Some(&listed(1e-5, 5e-5)))
                .unwrap();

        assert_eq!(pricing.cache_read_input_token_cost, Some(1e-6));
    }

    // Upstream #1028 also picks a same-priced non-author endpoint when the
    // author serves none; this tree keeps the listed price there instead.
    #[test]
    fn no_author_endpoint_leaves_the_listed_price_to_the_caller() {
        let endpoints = vec![endpoint("Azure", "0.00001", "0.00005", Some("0.000001"))];

        assert!(
            select_author_endpoint_pricing(&endpoints, "OpenAI", Some(&listed(1e-5, 5e-5)))
                .is_none()
        );
    }
}
