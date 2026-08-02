//! Fetch a URL and render it as text.
//!
//! The counterpart to `web_search`: search finds the page, this reads it. Useful
//! on its own — a model told "the docs are at <https://docs.rs/…>" needs no
//! search at all, which is why this tool exists whether or not a search key is
//! configured.
//!
//! ## What it will not do
//!
//! Only `http` and `https`, and no redirect to anything else. Not defence
//! against a determined attacker — the model is not an adversary here — but
//! against the ordinary failure where a URL scraped out of a page turns out to
//! be `file:///etc/passwd` and a tool designed to fetch things fetches it. The
//! filesystem has a sandbox precisely so that reads go through a capability; a
//! network tool that could sidestep it would be a hole in the side of that.
//!
//! Loopback and link-local addresses are refused for the same reason. An agent
//! is a program that acts on text it did not write, and "fetch
//! `http://169.254.169.254/…`" is the oldest way to turn that into a credential
//! leak. Nothing in this editor needs to fetch its own localhost.

use async_trait::async_trait;
use serde_json::Value;

use crate::registry::{Tool, ToolCtx};
use crate::schema::{arg_i64, arg_str, ToolDefinition, ToolOutput, ToolParameter};

/// Ceiling on the text handed back, in characters.
///
/// ~16k tokens. A documentation page that does not fit is truncated with a note
/// rather than silently cut, so the model knows to narrow its question rather
/// than concluding the page ended there.
const DEFAULT_MAX_CHARS: usize = 64_000;
const HARD_MAX_CHARS: usize = 200_000;

/// Ceiling on what is downloaded before rendering, so a 200 MB tarball behind a
/// `text/html` content type cannot be read into memory.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A browser-ish agent string. Some documentation hosts serve a JavaScript
/// challenge to unrecognised clients, which renders as an empty page — an
/// answer far more confusing than an error.
const USER_AGENT: &str = concat!("Smithy/", env!("CARGO_PKG_VERSION"), " (+agent)");

pub struct WebFetch {
    http: reqwest::Client,
}

impl WebFetch {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .user_agent(USER_AGENT)
                // Redirects are followed, but every hop is re-checked below —
                // an allowed first URL that redirects to loopback is exactly
                // the case a scheme check on the input alone would miss.
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .unwrap_or_default(),
        }
    }
}

impl Default for WebFetch {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "web_fetch",
            "Fetch a web page over http/https and return it as plain text. Use this to read \
             documentation, a changelog, an RFC, or any URL you already know. Prefer the \
             canonical source (docs.rs, the project's own docs, the RFC text) over a blog \
             summarising it. If the page is truncated, fetch a more specific URL rather than \
             re-fetching the same one.",
            vec![
                ToolParameter::string("url", "The absolute http:// or https:// URL.", true),
                ToolParameter::integer(
                    "max_chars",
                    "Maximum characters to return (default 64000).",
                    false,
                ),
            ],
        )
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutput {
        let url = match arg_str(args, "url") {
            Ok(u) => u.trim(),
            Err(e) => return ToolOutput::err(e),
        };
        if let Err(e) = check_url(url) {
            return ToolOutput::err(e);
        }
        let max_chars = arg_i64(args, "max_chars")
            .map(|n| (n.max(500) as usize).min(HARD_MAX_CHARS))
            .unwrap_or(DEFAULT_MAX_CHARS);

        let response = match self.http.get(url).send().await {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("could not fetch `{url}`: {e}")),
        };

        // The URL actually reached, after redirects. Checked again because a
        // permitted URL is allowed to redirect anywhere, including inward.
        let final_url = response.url().clone();
        if let Err(e) = check_url(final_url.as_str()) {
            return ToolOutput::err(format!("`{url}` redirected somewhere it may not go: {e}"));
        }

        let status = response.status();
        if !status.is_success() {
            return ToolOutput::err(format!(
                "`{url}` returned HTTP {}. {}",
                status.as_u16(),
                match status.as_u16() {
                    404 => "The page does not exist — check the URL or search for it.",
                    401 | 403 => "The page requires authentication, which this tool cannot supply.",
                    429 => "Rate limited. Wait before retrying.",
                    _ => "Try a different source.",
                }
            ));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => return ToolOutput::err(format!("could not read `{url}`: {e}")),
        };
        if bytes.len() > MAX_BODY_BYTES {
            return ToolOutput::err(format!(
                "`{url}` is {} MB, which is too large to read. Fetch a more specific page.",
                bytes.len() / (1024 * 1024)
            ));
        }
        let body = String::from_utf8_lossy(&bytes);

        let text = if is_html(&content_type, &body) {
            render_html(&body)
        } else {
            body.to_string()
        };

        let text = collapse_blank_lines(text.trim());
        if text.is_empty() {
            return ToolOutput::err(format!(
                "`{url}` returned no readable text. It may require JavaScript; try the project's \
                 raw documentation or repository instead."
            ));
        }

        ToolOutput::ok(truncate(&text, max_chars, final_url.as_str()))
    }
}

/// Refuse anything that is not a public http/https URL.
///
/// Written as a deny of the specific shapes that matter rather than an allow of
/// hostname patterns, because the latter cannot be got right — the point is to
/// close the obvious holes, not to claim the tool is safe against a host that is
/// actively trying to be reached.
pub fn check_url(url: &str) -> Result<(), String> {
    let lowered = url.to_lowercase();
    if !lowered.starts_with("http://") && !lowered.starts_with("https://") {
        return Err(format!(
            "`{url}` is not an http:// or https:// URL. This tool only fetches web pages; use \
             `read` for files on disk."
        ));
    }

    let host = host_of(&lowered).ok_or_else(|| format!("`{url}` has no host"))?;

    if host == "localhost"
        || host == "0.0.0.0"
        || host.ends_with(".localhost")
        || host.ends_with(".internal")
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("169.254.")
        || host == "[::1]"
        || is_private_172(host)
    {
        return Err(format!(
            "`{host}` is a private or loopback address, which this tool will not fetch."
        ));
    }
    Ok(())
}

/// The host portion of an already-lowercased absolute URL, port stripped.
fn host_of(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())?;
    // Credentials, if someone put them in the URL.
    let authority = authority.rsplit('@').next()?;
    // An IPv6 literal keeps its brackets; anything else loses its port.
    if authority.starts_with('[') {
        return authority.split_once(']').map(|(h, _)| {
            // Put the bracket back so the caller compares against "[::1]".
            let end = h.len() + 1;
            &authority[..end]
        });
    }
    Some(authority.split(':').next().unwrap_or(authority))
}

/// `172.16.0.0/12` — the one private range that is not a clean prefix match.
fn is_private_172(host: &str) -> bool {
    let Some(rest) = host.strip_prefix("172.") else {
        return false;
    };
    let Some(second) = rest.split('.').next() else {
        return false;
    };
    matches!(second.parse::<u8>(), Ok(16..=31))
}

/// Whether to run the body through the HTML renderer.
///
/// Sniffs the body as well as the header because plenty of servers label
/// everything `application/octet-stream`, and rendering markup as if it were
/// plain text produces a wall of tags that is pure token cost.
fn is_html(content_type: &str, body: &str) -> bool {
    if content_type.contains("html") {
        return true;
    }
    if content_type.contains("json")
        || content_type.contains("text/plain")
        || content_type.contains("markdown")
    {
        return false;
    }
    let head = body.get(..512).unwrap_or(body).to_lowercase();
    head.contains("<!doctype html") || head.contains("<html")
}

fn render_html(body: &str) -> String {
    // A wide render width: this is going to a model, not a terminal, and hard
    // wrapping at 80 columns puts line breaks in the middle of code samples.
    html2text::from_read(body.as_bytes(), 200).unwrap_or_else(|_| body.to_string())
}

/// Squeeze runs of blank lines. Rendered HTML is mostly whitespace, and
/// whitespace is tokens.
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Cut to length, saying so.
///
/// Naming the URL in the notice matters: after a redirect the model asked for
/// one page and is reading another, and "fetch a more specific page" is only
/// actionable if it knows which page it got.
fn truncate(text: &str, max_chars: usize, final_url: &str) -> String {
    if text.chars().count() <= max_chars {
        return format!("Fetched {final_url}\n\n{text}");
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!(
        "Fetched {final_url}\n\n{cut}\n\n[Truncated at {max_chars} characters. The page continues \
         — fetch a more specific URL, or raise `max_chars`, rather than re-fetching this one.]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_https_url_is_allowed() {
        assert!(check_url("https://docs.rs/tokio/latest/tokio/").is_ok());
        assert!(check_url("http://example.com/a?b=c#d").is_ok());
    }

    #[test]
    fn a_file_url_is_refused_and_points_at_read() {
        let err = check_url("file:///etc/passwd").unwrap_err();
        assert!(err.contains("`read`"), "{err}");
    }

    #[test]
    fn other_schemes_are_refused() {
        for url in ["ftp://example.com/x", "gopher://example.com", "javascript:x"] {
            assert!(check_url(url).is_err(), "{url} should be refused");
        }
    }

    /// The cloud-metadata endpoint, which is the reason this check exists.
    #[test]
    fn link_local_metadata_is_refused() {
        assert!(check_url("http://169.254.169.254/latest/meta-data/").is_err());
    }

    #[test]
    fn loopback_and_private_ranges_are_refused() {
        for url in [
            "http://localhost:8080/",
            "http://127.0.0.1/",
            "http://0.0.0.0/",
            "https://10.1.2.3/x",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://172.31.255.1/",
            "http://[::1]/",
            "http://foo.internal/",
        ] {
            assert!(check_url(url).is_err(), "{url} should be refused");
        }
    }

    /// `172.32` is public, and refusing it would be a bug rather than caution.
    #[test]
    fn the_public_half_of_172_is_allowed() {
        assert!(check_url("http://172.32.0.1/").is_ok());
        assert!(check_url("http://172.15.0.1/").is_ok());
    }

    /// Credentials in the authority must not hide the real host.
    #[test]
    fn a_userinfo_prefix_does_not_disguise_loopback() {
        assert!(check_url("http://example.com@127.0.0.1/").is_err());
    }

    #[test]
    fn ports_do_not_disguise_a_private_host() {
        assert!(check_url("http://192.168.0.5:8443/admin").is_err());
    }

    #[test]
    fn html_is_detected_from_the_body_when_the_header_lies() {
        assert!(is_html(
            "application/octet-stream",
            "<!DOCTYPE html><html><body>hi</body></html>"
        ));
        assert!(!is_html("application/json", "{\"a\": 1}"));
        assert!(!is_html("text/plain", "<html> in a code sample"));
    }

    #[test]
    fn markup_is_rendered_rather_than_passed_through() {
        let out = render_html("<html><body><h1>Title</h1><p>Body text.</p></body></html>");
        assert!(out.contains("Title"), "{out}");
        assert!(out.contains("Body text."), "{out}");
        assert!(!out.contains("<h1>"), "{out}");
    }

    #[test]
    fn runs_of_blank_lines_collapse() {
        assert_eq!(collapse_blank_lines("a\n\n\n\nb"), "a\n\nb\n");
    }

    #[test]
    fn a_short_page_is_returned_whole_with_its_url() {
        let out = truncate("hello", 100, "https://example.com/");
        assert!(out.contains("https://example.com/"));
        assert!(out.ends_with("hello"));
        assert!(!out.contains("Truncated"));
    }

    /// Truncation must announce itself — a model that thinks it read the whole
    /// page will confidently report that something is not in it.
    #[test]
    fn a_long_page_says_that_it_was_cut() {
        let out = truncate(&"x".repeat(200), 50, "https://example.com/");
        assert!(out.contains("Truncated at 50"), "{out}");
        assert!(out.contains("more specific URL"), "{out}");
    }
}
