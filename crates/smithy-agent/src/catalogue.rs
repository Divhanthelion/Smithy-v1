//! What models are available, and what they cost.
//!
//! ## Why this is not on the `Provider` trait
//!
//! [`crate::provider::Provider`] says of itself that it is "narrower than
//! forge's `AiProvider` on purpose: the loop needs exactly one operation". That
//! is still true — the loop does not browse a catalogue — so listing lives here
//! as free functions instead of widening the trait every session runs through.
//!
//! It also has to work *before* a provider can be built. You cannot pick a model
//! for an endpoint you have not configured yet, and requiring a valid
//! `OpenRouter` — which requires a key — to find out which models are free would
//! put the answer behind the question.
//!
//! ## The filter that actually matters
//!
//! Smithy's loop is entirely tool-driven: the system prompt describes tools, the
//! turn ends on a tool call or a final answer, and a model that cannot emit
//! `tool_calls` produces a turn that does nothing at all. So [`ModelEntry`]
//! carries [`ModelEntry::tool_capable`] and the UI defaults to hiding the rest.
//!
//! This is not hypothetical on either backend. Of OpenRouter's free models, some
//! are content-safety classifiers and audio models with no tool support; of a
//! typical LM Studio library, the TTS and ASR entries are typed `llm` and would
//! otherwise sit in the list looking like chat models.

use serde::{Deserialize, Serialize};

use crate::config::{Endpoint, ProviderChoice};

/// What one model costs to use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelTier {
    /// Hosted, and billed at nothing. Still needs an account and a key.
    Free,
    /// Hosted and metered. Prices are dollars per million tokens, which is how
    /// they are quoted everywhere except the API, where they arrive as dollars
    /// per single token in a string.
    Paid {
        prompt_per_mtok: f64,
        completion_per_mtok: f64,
    },
    /// Hosted, with a price that cannot be known ahead of time.
    ///
    /// OpenRouter's router models — `openrouter/auto` and friends — quote `-1`
    /// as a sentinel, because what you pay depends on which model the router
    /// picks for a given request. Multiplying that by a million and rendering it
    /// produced `$-1000000.00 per M`, which is how this variant came to exist.
    Variable,
    /// On this machine.
    Local {
        size_bytes: u64,
        /// Whether an instance is resident right now. A model can be downloaded
        /// and not loaded, and the difference is a minute of waiting on the
        /// first request.
        loaded: bool,
    },
}

impl ModelTier {
    pub fn is_free(&self) -> bool {
        matches!(self, ModelTier::Free)
    }
}

/// One model you could select.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// The identifier that goes in the model field and on the wire.
    pub id: String,
    /// The human name, when the backend offers one distinct from the id.
    pub label: String,
    pub context_length: Option<i64>,
    /// Whether this model can call tools. See the module docs — with this false,
    /// the agent cannot function at all.
    pub tool_capable: bool,
    pub tier: ModelTier,
}

impl ModelEntry {
    /// The short annotation shown beside the name.
    pub fn badge(&self) -> String {
        match &self.tier {
            ModelTier::Free => "free".to_string(),
            ModelTier::Paid {
                prompt_per_mtok,
                completion_per_mtok,
            } => format!("${prompt_per_mtok:.2}/${completion_per_mtok:.2} per M"),
            ModelTier::Variable => "variable pricing".to_string(),
            ModelTier::Local { size_bytes, loaded } => {
                let gb = *size_bytes as f64 / 1e9;
                if *loaded {
                    format!("{gb:.1} GB · loaded")
                } else {
                    format!("{gb:.1} GB")
                }
            }
        }
    }

    /// Context window, written the way people say it.
    pub fn context_label(&self) -> String {
        match self.context_length {
            Some(n) if n >= 1_000_000 => format!("{}M ctx", n / 1_000_000),
            Some(n) if n >= 1_000 => format!("{}k ctx", n / 1_000),
            Some(n) => format!("{n} ctx"),
            None => String::new(),
        }
    }

    /// Whether this entry matches a search box's contents.
    ///
    /// Matches id and label, case-insensitively, on every whitespace-separated
    /// term — so "gemma 26" finds `google/gemma-4-26b-a4b-it:free` without
    /// anyone having to type the punctuation.
    pub fn matches(&self, query: &str) -> bool {
        let haystack = format!("{} {}", self.id, self.label).to_lowercase();
        query
            .split_whitespace()
            .all(|term| haystack.contains(&term.to_lowercase()))
    }
}

/// Fetch the models a backend offers.
///
/// `api_key` is optional and only consulted by OpenRouter, whose catalogue is
/// public — which is deliberate to rely on: you can browse the free tier and
/// decide whether it is worth signing up before you have a key to browse it
/// with.
pub async fn list(
    provider: ProviderChoice,
    endpoint: &Endpoint,
    api_key: Option<&str>,
) -> Result<Vec<ModelEntry>, String> {
    match provider {
        ProviderChoice::OpenRouter => list_openrouter(&endpoint.base_url, api_key).await,
        ProviderChoice::LmStudio => list_lmstudio(&endpoint.base_url).await,
        ProviderChoice::DeepSeek => list_deepseek(&endpoint.base_url, api_key).await,
    }
}

/// DeepSeek's catalogue.
///
/// `/models` needs a key and returns ids and nothing else — no context window,
/// no pricing, no capability flags. So the ids come from the wire and everything
/// else from [`crate::providers::deepseek::KNOWN_MODELS`], which is a snapshot
/// and says so.
///
/// A model DeepSeek adds later still appears in the list, just without a context
/// window or a price. That is the right failure: an unfamiliar id you can select
/// beats a complete list you cannot see.
async fn list_deepseek(base_url: &str, api_key: Option<&str>) -> Result<Vec<ModelEntry>, String> {
    use crate::providers::deepseek;

    let Some(key) = api_key.filter(|k| !k.trim().is_empty()) else {
        // Worded for both callers: the dialog shows this above the key field,
        // and the `models` example prints it to a terminal where "below" would
        // mean nothing.
        return Err(
            "DeepSeek needs an API key before it will list models. Add one, then refresh."
                .to_string(),
        );
    };

    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let response = client(20)?
        .get(&url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("could not reach DeepSeek: {e}"))?;

    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err("DeepSeek rejected the API key.".to_string());
    }
    if !status.is_success() {
        return Err(format!(
            "DeepSeek returned HTTP {} when listing models",
            status.as_u16()
        ));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("could not parse DeepSeek's model list: {e}"))?;
    let data = body["data"]
        .as_array()
        .ok_or("DeepSeek's model list had no `data` array")?;

    let mut entries: Vec<ModelEntry> = data
        .iter()
        .filter_map(|m| m["id"].as_str())
        .map(|id| ModelEntry {
            id: id.to_string(),
            label: deepseek_label(id),
            context_length: deepseek::context_for(id),
            // Both current models do tool calls, and an unknown one is assumed
            // to as well — hiding a new model behind the tool filter would be a
            // worse error than listing one that turns out not to work.
            tool_capable: true,
            tier: match deepseek::pricing_for(id) {
                Some((prompt, completion)) => ModelTier::Paid {
                    prompt_per_mtok: prompt,
                    completion_per_mtok: completion,
                },
                None => ModelTier::Variable,
            },
        })
        .collect();

    entries.sort_by(|a, b| b.context_length.cmp(&a.context_length).then(a.id.cmp(&b.id)));
    Ok(entries)
}

/// A readable name for a DeepSeek id, since `/models` supplies none.
fn deepseek_label(id: &str) -> String {
    match id {
        "deepseek-v4-flash" => "DeepSeek V4 Flash — cheaper, faster".to_string(),
        "deepseek-v4-pro" => "DeepSeek V4 Pro — stronger, dearer".to_string(),
        other => other.to_string(),
    }
}

fn client(timeout: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))
}

async fn list_openrouter(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<ModelEntry>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = client(20)?.get(&url);
    if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        request = request.bearer_auth(key);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("could not reach OpenRouter: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "OpenRouter returned HTTP {} when listing models",
            response.status().as_u16()
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("could not parse OpenRouter's model list: {e}"))?;

    let data = body["data"]
        .as_array()
        .ok_or("OpenRouter's model list had no `data` array")?;

    let mut entries: Vec<ModelEntry> = data.iter().filter_map(parse_openrouter_entry).collect();

    // Free first, then by context window. The free tier is the reason most
    // people open this list, and burying it under three hundred paid entries
    // sorted by name would make it the hardest thing to find.
    entries.sort_by(|a, b| {
        b.tier
            .is_free()
            .cmp(&a.tier.is_free())
            .then(b.context_length.cmp(&a.context_length))
            .then(a.id.cmp(&b.id))
    });
    Ok(entries)
}

fn parse_openrouter_entry(model: &serde_json::Value) -> Option<ModelEntry> {
    let id = model["id"].as_str()?.to_string();

    // Prices arrive as decimal strings in dollars per token. A missing or
    // unparseable price is treated as paid-at-unknown rather than free: calling
    // something free when it is not is the expensive direction to be wrong in.
    let prompt = price(model, "prompt");
    let completion = price(model, "completion");
    let tier = match (prompt, completion) {
        // A negative price is a sentinel, not a discount.
        (Some(p), _) | (_, Some(p)) if p < 0.0 => ModelTier::Variable,
        (Some(p), Some(c)) if p == 0.0 && c == 0.0 => ModelTier::Free,
        (p, c) => ModelTier::Paid {
            prompt_per_mtok: p.unwrap_or(0.0) * 1e6,
            completion_per_mtok: c.unwrap_or(0.0) * 1e6,
        },
    };

    let tool_capable = model["supported_parameters"]
        .as_array()
        .map(|params| params.iter().any(|p| p.as_str() == Some("tools")))
        .unwrap_or(false);

    Some(ModelEntry {
        label: model["name"].as_str().unwrap_or(&id).to_string(),
        id,
        context_length: model["context_length"].as_i64(),
        tool_capable,
        tier,
    })
}

fn price(model: &serde_json::Value, field: &str) -> Option<f64> {
    model["pricing"][field].as_str()?.parse::<f64>().ok()
}

async fn list_lmstudio(base_url: &str) -> Result<Vec<ModelEntry>, String> {
    let url = native_models_url(base_url);
    let response = client(10)?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("could not reach LM Studio at {url}: {e}. Is the server running?"))?;
    if !response.status().is_success() {
        return Err(format!(
            "LM Studio returned HTTP {} when listing models. The native API needs a recent build.",
            response.status().as_u16()
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("could not parse LM Studio's model list: {e}"))?;

    let models = body["models"]
        .as_array()
        .ok_or("LM Studio's model list had no `models` array")?;

    let mut entries: Vec<ModelEntry> = models
        .iter()
        // Embedding models are not chat models, and LM Studio says which is
        // which. It does *not* reliably say so for every entry — the tool-use
        // capability below is what actually keeps TTS and ASR out of the list.
        .filter(|m| m["type"].as_str() != Some("embedding"))
        .filter_map(parse_lmstudio_entry)
        .collect();

    // Loaded first — a resident model answers immediately and everything else
    // costs a load — then the larger context, then by name.
    entries.sort_by(|a, b| {
        let loaded = |e: &ModelEntry| matches!(e.tier, ModelTier::Local { loaded: true, .. });
        loaded(b)
            .cmp(&loaded(a))
            .then(b.context_length.cmp(&a.context_length))
            .then(a.id.cmp(&b.id))
    });
    Ok(entries)
}

fn parse_lmstudio_entry(model: &serde_json::Value) -> Option<ModelEntry> {
    let id = model["key"].as_str()?.to_string();
    let loaded = model["loaded_instances"]
        .as_array()
        .map(|i| !i.is_empty())
        .unwrap_or(false);

    Some(ModelEntry {
        label: model["display_name"].as_str().unwrap_or(&id).to_string(),
        id,
        context_length: model["max_context_length"].as_i64(),
        // Absent means yes, matching `probe_model`'s existing default: an older
        // build that does not report capabilities should not have its whole
        // library hidden behind the tool-capable filter.
        tool_capable: model["capabilities"]["trained_for_tool_use"]
            .as_bool()
            .unwrap_or(true),
        tier: ModelTier::Local {
            size_bytes: model["size_bytes"].as_u64().unwrap_or(0),
            loaded,
        },
    })
}

/// LM Studio's native API root, derived from the OpenAI-compatible base URL.
///
/// Same derivation [`crate::providers::LmStudio`] uses, kept in step with it:
/// the native surface carries load state and capabilities that `/v1/models`
/// does not.
fn native_models_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match base.rfind("/v1") {
        Some(i) => format!("{}/api/v1/models", &base[..i]),
        None => format!("{base}/api/v1/models"),
    }
}

/// Money left on a hosted account.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    /// ISO code, e.g. `USD`. Reported rather than assumed — DeepSeek bills some
    /// accounts in CNY.
    pub currency: String,
    /// What is left to spend: granted credit plus what was topped up.
    pub total: f64,
    /// Whether the provider considers the account usable at all.
    pub available: bool,
}

impl Balance {
    pub fn render(&self) -> String {
        let symbol = match self.currency.as_str() {
            "USD" => "$",
            "CNY" => "¥",
            _ => "",
        };
        if symbol.is_empty() {
            format!("{:.2} {}", self.total, self.currency)
        } else {
            format!("{symbol}{:.2}", self.total)
        }
    }
}

/// Ask DeepSeek what is left on the account.
///
/// The only provider here with a balance endpoint. OpenRouter has one too but
/// reports a credit *limit* rather than a remaining balance for most keys, and
/// LM Studio is a local server with nothing to bill — so this is deliberately
/// DeepSeek-shaped rather than a general abstraction over one implementation.
pub async fn deepseek_balance(base_url: &str, api_key: &str) -> Result<Balance, String> {
    let url = format!("{}/user/balance", base_url.trim_end_matches('/'));
    let response = client(15)?
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("could not reach DeepSeek: {e}"))?;

    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err("DeepSeek rejected the API key.".to_string());
    }
    if !status.is_success() {
        return Err(format!("DeepSeek returned HTTP {}", status.as_u16()));
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("could not parse the balance response: {e}"))?;

    parse_balance(&body).ok_or_else(|| "DeepSeek's balance response had no usable figures".into())
}

fn parse_balance(body: &serde_json::Value) -> Option<Balance> {
    let available = body["is_available"].as_bool().unwrap_or(false);
    // `balance_infos` is an array — one entry per currency the account holds.
    let first = body["balance_infos"].as_array()?.first()?;
    Some(Balance {
        currency: first["currency"].as_str().unwrap_or("USD").to_string(),
        // Strings, not numbers, in DeepSeek's response.
        total: first["total_balance"].as_str()?.parse().ok()?,
        available,
    })
}

/// What happened when a local model was asked to load.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub model: String,
    pub seconds: f64,
}

/// Load a model into LM Studio.
///
/// **This blocks for as long as the load takes**, which for a thirty-gigabyte
/// model is tens of seconds to minutes. The caller must be on a worker and must
/// tell the user something is happening.
///
/// Strictly speaking it is optional: LM Studio's JIT loader will pull in an
/// unloaded model on the first completion request. But that turns "press Send"
/// into a minute of apparent hang with no explanation, which is exactly the
/// confusion the explicit button exists to remove.
pub async fn load_lmstudio_model(
    base_url: &str,
    model: &str,
    context_length: Option<i64>,
) -> Result<Loaded, String> {
    let base = base_url.trim_end_matches('/');
    let url = match base.rfind("/v1") {
        Some(i) => format!("{}/api/v1/models/load", &base[..i]),
        None => format!("{base}/api/v1/models/load"),
    };

    let mut body = serde_json::json!({ "model": model });
    if let Some(context) = context_length {
        body["context_length"] = serde_json::json!(context);
    }

    // No timeout: a large model on a cold cache legitimately takes minutes, and
    // a client timeout here would report failure for a load that is still
    // proceeding — leaving LM Studio holding a model the UI says it does not
    // have.
    let response = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("could not reach LM Studio: {e}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "LM Studio could not load `{model}`: {}",
            explain_error(&text, status.as_u16())
        ));
    }

    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    Ok(Loaded {
        model: model.to_string(),
        seconds: parsed["load_time_seconds"].as_f64().unwrap_or(0.0),
    })
}

/// Pull the human sentence out of an error body.
///
/// LM Studio answers with `{"error": {"type": …, "message": …}}` — pretty
/// printed, so the naive "first line of the body" this replaces rendered every
/// failure as a single `{`. The fallbacks descend deliberately: the nested
/// message, then a flat `error` string, then the raw first line, then the status
/// code on its own. Something legible at every level beats a clean parse that
/// yields nothing when the shape changes.
fn explain_error(text: &str, status: u16) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(message) = value["error"]["message"].as_str() {
            return truncate(message);
        }
        if let Some(message) = value["error"].as_str() {
            return truncate(message);
        }
        if let Some(message) = value["message"].as_str() {
            return truncate(message);
        }
    }
    let line = text.trim().lines().next().unwrap_or("").trim();
    if line.is_empty() || line == "{" {
        format!("HTTP {status}")
    } else {
        truncate(line)
    }
}

fn truncate(text: &str) -> String {
    if text.chars().count() > 200 {
        format!("{}…", text.chars().take(200).collect::<String>())
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- OpenRouter ---

    fn openrouter_model(id: &str, prompt: &str, completion: &str, tools: bool) -> serde_json::Value {
        json!({
            "id": id,
            "name": format!("Name of {id}"),
            "context_length": 128000,
            "pricing": { "prompt": prompt, "completion": completion },
            "supported_parameters": if tools { json!(["tools", "temperature"]) } else { json!(["temperature"]) }
        })
    }

    #[test]
    fn a_zero_priced_model_is_free() {
        let entry = parse_openrouter_entry(&openrouter_model("a:free", "0", "0", true)).unwrap();
        assert_eq!(entry.tier, ModelTier::Free);
        assert_eq!(entry.badge(), "free");
    }

    /// Prices arrive per *token*; everyone quotes per million. Getting this
    /// wrong by six orders of magnitude would render every price as `$0.00`.
    #[test]
    fn prices_are_converted_to_dollars_per_million_tokens() {
        let entry =
            parse_openrouter_entry(&openrouter_model("a", "0.00000009", "0.00000018", true))
                .unwrap();
        match entry.tier {
            ModelTier::Paid {
                prompt_per_mtok,
                completion_per_mtok,
            } => {
                assert!((prompt_per_mtok - 0.09).abs() < 1e-9, "{prompt_per_mtok}");
                assert!((completion_per_mtok - 0.18).abs() < 1e-9);
            }
            other => panic!("expected paid, got {other:?}"),
        }
    }

    /// `openrouter/auto` quotes `-1` because the router decides what you pay.
    /// Read as a number it rendered as `$-1000000.00 per M`.
    #[test]
    fn a_negative_sentinel_price_is_variable_not_a_huge_discount() {
        let entry = parse_openrouter_entry(&openrouter_model("auto", "-1", "-1", true)).unwrap();
        assert_eq!(entry.tier, ModelTier::Variable);
        assert_eq!(entry.badge(), "variable pricing");
        assert!(!entry.tier.is_free(), "variable must not read as free");
    }

    /// Being wrong about "free" costs money, so an unparseable price must not
    /// resolve that way.
    #[test]
    fn a_missing_price_is_treated_as_paid_not_free() {
        let entry = parse_openrouter_entry(&json!({
            "id": "mystery", "context_length": 1000, "supported_parameters": ["tools"]
        }))
        .unwrap();
        assert!(!entry.tier.is_free(), "{:?}", entry.tier);
    }

    /// The filter the whole module exists for: a model without `tools` cannot
    /// drive this agent at all.
    #[test]
    fn tool_support_is_read_from_supported_parameters() {
        assert!(
            parse_openrouter_entry(&openrouter_model("a", "0", "0", true))
                .unwrap()
                .tool_capable
        );
        assert!(
            !parse_openrouter_entry(&openrouter_model("b", "0", "0", false))
                .unwrap()
                .tool_capable
        );
    }

    #[test]
    fn free_models_sort_ahead_of_paid_ones() {
        let body = json!({"data": [
            openrouter_model("paid", "0.001", "0.002", true),
            openrouter_model("free", "0", "0", true),
        ]});
        let data = body["data"].as_array().unwrap();
        let mut entries: Vec<ModelEntry> =
            data.iter().filter_map(parse_openrouter_entry).collect();
        entries.sort_by(|a, b| {
            b.tier
                .is_free()
                .cmp(&a.tier.is_free())
                .then(b.context_length.cmp(&a.context_length))
                .then(a.id.cmp(&b.id))
        });
        assert_eq!(entries[0].id, "free");
    }

    // --- LM Studio ---

    fn lmstudio_model(key: &str, loaded: bool, tools: bool, kind: &str) -> serde_json::Value {
        json!({
            "type": kind,
            "key": key,
            "display_name": "Display Name",
            "max_context_length": 262144,
            "size_bytes": 33795587616u64,
            "loaded_instances": if loaded { json!([{"config": {}}]) } else { json!([]) },
            "capabilities": { "trained_for_tool_use": tools }
        })
    }

    #[test]
    fn a_local_model_reports_its_size_and_load_state() {
        let entry = parse_lmstudio_entry(&lmstudio_model("m", true, true, "llm")).unwrap();
        assert_eq!(
            entry.tier,
            ModelTier::Local {
                size_bytes: 33795587616,
                loaded: true
            }
        );
        assert!(entry.badge().contains("loaded"), "{}", entry.badge());
        assert!(entry.badge().contains("33.8 GB"), "{}", entry.badge());
    }

    #[test]
    fn an_unloaded_local_model_says_only_its_size() {
        let entry = parse_lmstudio_entry(&lmstudio_model("m", false, true, "llm")).unwrap();
        assert!(!entry.badge().contains("loaded"), "{}", entry.badge());
    }

    /// TTS and ASR entries are typed `llm` by LM Studio and are the reason the
    /// tool-capability filter is the load-bearing one.
    #[test]
    fn a_local_model_without_tool_training_is_marked() {
        let entry = parse_lmstudio_entry(&lmstudio_model("tts", false, false, "llm")).unwrap();
        assert!(!entry.tool_capable);
    }

    /// An older build reporting no capabilities must not have its whole library
    /// hidden by a filter that defaults to on.
    #[test]
    fn a_local_model_with_no_capability_block_is_assumed_capable() {
        let entry = parse_lmstudio_entry(&json!({
            "type": "llm", "key": "old", "max_context_length": 4096, "loaded_instances": []
        }))
        .unwrap();
        assert!(entry.tool_capable);
    }

    // --- shared ---

    #[test]
    fn the_native_url_is_derived_from_the_openai_base() {
        assert_eq!(
            native_models_url("http://localhost:1234/v1"),
            "http://localhost:1234/api/v1/models"
        );
        assert_eq!(
            native_models_url("http://localhost:1234/v1/"),
            "http://localhost:1234/api/v1/models"
        );
        assert_eq!(
            native_models_url("http://host:9999"),
            "http://host:9999/api/v1/models"
        );
    }

    /// The real body LM Studio returns for an unknown model. Pretty-printed, so
    /// "first line of the body" — which this replaced — rendered it as `{`.
    #[test]
    fn a_load_failure_reports_lm_studios_own_sentence() {
        let body = "{\n  \"error\": {\n    \"type\": \"model_not_found\",\n    \
                    \"message\": \"Model foo not found in downloaded models\"\n  }\n}";
        assert_eq!(
            explain_error(body, 404),
            "Model foo not found in downloaded models"
        );
    }

    #[test]
    fn a_flat_error_string_is_also_understood() {
        assert_eq!(explain_error(r#"{"error":"out of memory"}"#, 500), "out of memory");
        assert_eq!(explain_error(r#"{"message":"nope"}"#, 500), "nope");
    }

    /// An empty or unparseable body must still say something, and never the
    /// bare `{` that started this.
    #[test]
    fn an_unusable_body_falls_back_to_the_status_code() {
        assert_eq!(explain_error("", 503), "HTTP 503");
        assert_eq!(explain_error("{", 500), "HTTP 500");
        assert_eq!(explain_error("plain text failure", 500), "plain text failure");
    }

    #[test]
    fn a_very_long_error_is_cut_rather_than_filling_the_dialog() {
        let long = format!("{{\"error\":{{\"message\":\"{}\"}}}}", "x".repeat(500));
        let out = explain_error(&long, 500);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 201, "{}", out.chars().count());
    }

    /// The exact body a live account returns. Note the figures are *strings*.
    #[test]
    fn a_balance_is_parsed_from_deepseeks_actual_response() {
        let body = json!({
            "is_available": true,
            "balance_infos": [{
                "currency": "USD",
                "total_balance": "9.93",
                "granted_balance": "0.00",
                "topped_up_balance": "9.93"
            }]
        });
        let balance = parse_balance(&body).expect("parsed");
        assert_eq!(balance.currency, "USD");
        assert!((balance.total - 9.93).abs() < 1e-9);
        assert!(balance.available);
        assert_eq!(balance.render(), "$9.93");
    }

    /// An account billed in yuan must not be rendered with a dollar sign.
    #[test]
    fn a_non_dollar_currency_is_not_silently_relabelled() {
        let body = json!({
            "is_available": true,
            "balance_infos": [{ "currency": "CNY", "total_balance": "42.50" }]
        });
        assert_eq!(parse_balance(&body).unwrap().render(), "¥42.50");

        let body = json!({
            "is_available": true,
            "balance_infos": [{ "currency": "GBP", "total_balance": "7.00" }]
        });
        assert_eq!(parse_balance(&body).unwrap().render(), "7.00 GBP");
    }

    /// A shape we do not recognise must yield nothing rather than a confident
    /// zero — "you have $0.00 left" is an alarming thing to invent.
    #[test]
    fn an_unusable_balance_response_is_none_not_zero() {
        assert!(parse_balance(&json!({})).is_none());
        assert!(parse_balance(&json!({"balance_infos": []})).is_none());
        assert!(parse_balance(&json!({"balance_infos": [{"currency": "USD"}]})).is_none());
    }

    #[test]
    fn context_windows_are_written_the_way_people_say_them() {
        let entry = |n: Option<i64>| ModelEntry {
            id: "x".into(),
            label: "x".into(),
            context_length: n,
            tool_capable: true,
            tier: ModelTier::Free,
        };
        assert_eq!(entry(Some(1_000_000)).context_label(), "1M ctx");
        assert_eq!(entry(Some(262_144)).context_label(), "262k ctx");
        assert_eq!(entry(Some(900)).context_label(), "900 ctx");
        assert_eq!(entry(None).context_label(), "");
    }

    /// Search has to work on the words people remember, not the punctuation the
    /// id happens to use.
    #[test]
    fn search_matches_every_term_across_id_and_label() {
        let entry = ModelEntry {
            id: "google/gemma-4-26b-a4b-it:free".into(),
            label: "Google: Gemma 4 26B A4B (free)".into(),
            context_length: Some(262144),
            tool_capable: true,
            tier: ModelTier::Free,
        };
        assert!(entry.matches("gemma 26"));
        assert!(entry.matches("GOOGLE"));
        assert!(entry.matches(""));
        assert!(!entry.matches("gemma llama"));
    }
}
