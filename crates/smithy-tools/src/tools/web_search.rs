//! Search the web, through Brave.
//!
//! ## Why a search API and not a scraper
//!
//! Scraping a search engine's HTML is the version of this that works for a
//! fortnight. Brave's API is a documented endpoint with a free tier, returns
//! JSON, and does not break when someone changes a CSS class.
//!
//! ## Why this tool is absent rather than broken without a key
//!
//! [`crate::registry::Registry::core`] documents that the tool block must be
//! constant for the life of a session, because changing it changes the cached
//! prefix and forces a cold prefill. It says nothing about two *different*
//! sessions having different tool blocks, and that is the seam used here: the
//! app adds this tool at session construction when a key exists, and leaves it
//! out when none does.
//!
//! The alternative — always present, erroring on every call — costs a round trip
//! to discover something the app already knew, and teaches the model that tools
//! in its list may not work.
//!
//! ## Results are deliberately thin
//!
//! Title, URL, and Brave's own description. Not the page — that is `web_fetch`,
//! and the whole point of the split is that the model spends ten results' worth
//! of tokens choosing which single page to spend ten thousand on.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::registry::{Tool, ToolCtx};
use crate::schema::{arg_i64, arg_str, ToolDefinition, ToolOutput, ToolParameter};

const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

const DEFAULT_COUNT: i64 = 8;
const MAX_COUNT: i64 = 20;

const DESCRIPTION: &str =
    "Search the web and return titles, URLs and short descriptions. Use it when the \
             answer depends on something outside this repository — a library's current API, an \
             error message you do not recognise, a version's release notes. Do not use it for \
             anything about this codebase; `grep` and `read` are faster and authoritative. \
             Search returns descriptions, not pages: follow up with `web_fetch` on the one or \
             two URLs that look right. Two or three searches is usually enough; if that has not \
             answered it, say so rather than searching again.";

const RESEARCH_DESCRIPTION: &str =
    "Search the web and return titles, URLs and short descriptions. Use it when the \
             answer depends on something outside this repository. Do not use it for anything \
             about this codebase; `grep` and `read` are faster and authoritative. Search returns \
             descriptions, not pages: follow up with `web_fetch` on the URLs that look right. \
             Keep searching while a query would produce diagnostic evidence against a frozen \
             hypothesis. Stop when a pass adds none.";

/// How much of a description survives. Brave's are already short; this catches
/// the occasional one that is a whole paragraph.
const MAX_DESCRIPTION_CHARS: usize = 300;

/// How the tool obtains its API key.
///
/// [`KeySource::Deferred`] exists so session construction can register the tool
/// when a key is *known to be stored*, without unlocking the OS keychain yet —
/// unlocking every configured key at launch was prompting for the password once
/// per key. The keychain is touched on the first search instead.
pub enum KeySource {
    Ready(String),
    Deferred(Arc<dyn Fn() -> Option<String> + Send + Sync>),
}

pub struct WebSearch {
    http: reqwest::Client,
    key: KeySource,
    /// Research Sessions must not reuse the coding "two or three is enough" line.
    research: bool,
}

impl WebSearch {
    /// Build the tool around a key already in hand.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .unwrap_or_default(),
            key: KeySource::Ready(api_key.into()),
            research: false,
        }
    }

    /// Register the tool now; unlock the key on the first call.
    pub fn deferred(lookup: impl Fn() -> Option<String> + Send + Sync + 'static) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .unwrap_or_default(),
            key: KeySource::Deferred(Arc::new(lookup)),
            research: false,
        }
    }

    /// Description without the coding-session search budget. Frozen into the
    /// tool JSON for a Research Session.
    pub fn for_research(mut self) -> Self {
        self.research = true;
        self
    }

    fn resolve_key(&self) -> Result<String, String> {
        match &self.key {
            KeySource::Ready(k) => Ok(k.clone()),
            KeySource::Deferred(lookup) => lookup().ok_or_else(|| {
                "Brave Search key is no longer available. Add it under Settings → Agent."
                    .to_string()
            }),
        }
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "web_search",
            if self.research {
                RESEARCH_DESCRIPTION
            } else {
                DESCRIPTION
            },
            vec![
                ToolParameter::string(
                    "query",
                    "What to search for. Keywords work better than a sentence. Include the \
                     library and version when they matter.",
                    true,
                ),
                ToolParameter::integer(
                    "count",
                    "How many results to return (default 8, maximum 20).",
                    false,
                ),
            ],
        )
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutput {
        let query = match arg_str(args, "query") {
            Ok(q) => q.trim(),
            Err(e) => return ToolOutput::err(e),
        };
        if query.is_empty() {
            return ToolOutput::err("the query is empty");
        }
        let api_key = match self.resolve_key() {
            Ok(k) => k,
            Err(e) => return ToolOutput::err(e),
        };
        let count = arg_i64(args, "count")
            .unwrap_or(DEFAULT_COUNT)
            .clamp(1, MAX_COUNT);

        // Both bound as `&str` so the pair array has a single element type.
        let count_param = count.to_string();
        let response = match self
            .http
            .get(ENDPOINT)
            .header("X-Subscription-Token", &api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", count_param.as_str())])
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("could not reach Brave Search: {e}")),
        };

        let status = response.status();
        if !status.is_success() {
            return ToolOutput::err(describe_failure(status.as_u16()));
        }

        let body = match response.text().await {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("could not read the search response: {e}")),
        };
        let json: Value = match serde_json::from_str(&body) {
            Ok(j) => j,
            Err(e) => return ToolOutput::err(format!("could not parse the search response: {e}")),
        };

        match render(&json, query) {
            Some(text) => ToolOutput::ok(text),
            None => ToolOutput::ok(format!(
                "No results for `{query}`. Try different keywords, or drop a version number."
            )),
        }
    }
}

/// Turn Brave's HTTP status into something the model can act on.
///
/// The distinction that matters is "your key is wrong" versus "you are going too
/// fast": one is worth reporting and giving up on, the other is worth waiting
/// for. A generic "search failed" leaves the model to guess, and it guesses
/// retry.
fn describe_failure(status: u16) -> String {
    match status {
        401 | 403 => "Brave Search rejected the API key. Check it under Settings → Agent. Do not \
                      retry this search."
            .to_string(),
        422 => "Brave Search rejected the query as malformed. Rephrase it.".to_string(),
        429 => "Brave Search is rate limiting. Wait before searching again, or answer from what \
                you already have."
            .to_string(),
        500..=599 => {
            "Brave Search is having trouble. Try once more, then proceed without it.".to_string()
        }
        other => format!("Brave Search returned HTTP {other}."),
    }
}

/// Render `web.results` into a compact list.
///
/// `None` when there is nothing to show, so the caller can say so in words
/// rather than returning an empty success — a blank tool result reads to a model
/// as a broken tool.
fn render(json: &Value, query: &str) -> Option<String> {
    let results = json.get("web")?.get("results")?.as_array()?;
    if results.is_empty() {
        return None;
    }

    let mut out = format!("Results for `{query}`:\n\n");
    for (i, result) in results.iter().enumerate() {
        let title = result
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(untitled)");
        let url = result.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if url.is_empty() {
            continue;
        }
        let description = result
            .get("description")
            .and_then(|v| v.as_str())
            .map(strip_markup)
            .unwrap_or_default();

        out.push_str(&format!("{}. {}\n   {}\n", i + 1, strip_markup(title), url));
        if !description.is_empty() {
            out.push_str(&format!("   {}\n", shorten(&description)));
        }
        out.push('\n');
    }

    out.push_str("Use `web_fetch` on the URLs worth reading in full.");
    Some(out)
}

/// Brave marks matched terms with `<strong>`. Left in, those tags are noise the
/// model has to read past on every result.
fn strip_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

fn shorten(text: &str) -> String {
    if text.chars().count() <= MAX_DESCRIPTION_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_DESCRIPTION_CHARS).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "web": {
                "results": [
                    {
                        "title": "tokio::<strong>spawn</strong> — Rust",
                        "url": "https://docs.rs/tokio/latest/tokio/fn.spawn.html",
                        "description": "Spawns a new <strong>asynchronous</strong> task."
                    },
                    {
                        "title": "Second",
                        "url": "https://example.com/second"
                    }
                ]
            }
        })
    }

    #[test]
    fn results_render_as_a_numbered_list_with_urls() {
        let out = render(&sample(), "tokio spawn").unwrap();
        assert!(out.contains("1. tokio::spawn — Rust"), "{out}");
        assert!(
            out.contains("https://docs.rs/tokio/latest/tokio/fn.spawn.html"),
            "{out}"
        );
        assert!(out.contains("2. Second"), "{out}");
    }

    /// The tags Brave wraps matched terms in are noise on every single result.
    #[test]
    fn highlight_markup_is_stripped_from_titles_and_descriptions() {
        let out = render(&sample(), "q").unwrap();
        assert!(!out.contains("<strong>"), "{out}");
        assert!(out.contains("Spawns a new asynchronous task."), "{out}");
    }

    /// The point of the search/fetch split, said in the result itself.
    #[test]
    fn the_result_points_at_web_fetch() {
        let out = render(&sample(), "q").unwrap();
        assert!(out.contains("web_fetch"), "{out}");
    }

    #[test]
    fn a_result_without_a_url_is_skipped_rather_than_rendered_blank() {
        let json = json!({"web": {"results": [{"title": "No link"}]}});
        let out = render(&json, "q").unwrap();
        assert!(!out.contains("No link"), "{out}");
    }

    #[test]
    fn no_results_is_none_rather_than_an_empty_success() {
        assert!(render(&json!({"web": {"results": []}}), "q").is_none());
        assert!(render(&json!({}), "q").is_none());
    }

    #[test]
    fn a_very_long_description_is_shortened() {
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 50);
        assert!(shorten(&long).ends_with('…'));
        assert_eq!(shorten("short"), "short");
    }

    #[test]
    fn a_research_description_does_not_cap_searches_at_two_or_three() {
        let coding = WebSearch::new("k").definition().description;
        let research = WebSearch::new("k").for_research().definition().description;
        assert!(coding.contains("Two or three searches is usually enough"));
        assert!(!research.contains("Two or three"));
        assert!(research.contains("diagnostic evidence"));
    }
    #[test]
    fn failures_are_distinguished_by_what_to_do_about_them() {
        assert!(describe_failure(401).contains("Do not retry"));
        assert!(describe_failure(429).contains("Wait"));
        assert!(describe_failure(503).contains("Try once more"));
        assert!(describe_failure(418).contains("418"));
    }
}
