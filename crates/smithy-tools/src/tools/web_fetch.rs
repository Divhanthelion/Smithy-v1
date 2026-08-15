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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

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
                // Followed by hand so every hop is re-validated *before* it is
                // requested. Automatic following would hit a private address
                // and only refuse the body afterwards.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|e| {
                    panic!("web_fetch HTTP client could not be built: {e}");
                }),
        }
    }

    /// Follow at most 5 redirects, validating every hop *before* requesting it.
    ///
    /// `validate_first` is the production path. Tests set it false so a
    /// loopback fixture can be the first hop — check_url would otherwise
    /// refuse before we can observe whether the second hop is requested.
    async fn get_with_redirects(
        &self,
        start: &str,
        validate_first: bool,
    ) -> Result<(reqwest::Response, String), String> {
        let mut current = start.to_string();
        for hop in 0..=5 {
            if hop > 0 || validate_first {
                check_url(&current).map_err(|e| {
                    if hop == 0 {
                        e
                    } else {
                        format!("`{start}` redirected somewhere it may not go: {e}")
                    }
                })?;
                reject_resolved_host(&current).await.map_err(|e| {
                    if hop == 0 {
                        e
                    } else {
                        format!("`{start}` redirected somewhere it may not go: {e}")
                    }
                })?;
            }
            let hop_response = self
                .http
                .get(&current)
                .send()
                .await
                .map_err(|e| format!("could not fetch `{current}`: {e}"))?;
            if hop_response.status().is_redirection() {
                let loc = hop_response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| format!("`{current}` redirected without a Location header"))?;
                current = join_redirect(&current, loc)?;
                if hop == 5 {
                    return Err(format!("`{start}` redirected more than 5 times"));
                }
                continue;
            }
            return Ok((hop_response, current));
        }
        Err(format!("`{start}` redirected more than 5 times"))
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
            Ok(u) => u.trim().to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        let max_chars = arg_i64(args, "max_chars")
            .map(|n| (n.max(500) as usize).min(HARD_MAX_CHARS))
            .unwrap_or(DEFAULT_MAX_CHARS);

        let (response, current) = match self.get_with_redirects(&url, true).await {
            Ok(pair) => pair,
            Err(e) => return ToolOutput::err(e),
        };

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

        ToolOutput::ok(truncate(&text, max_chars, &current))
    }
}

/// Refuse anything that is not a public http/https URL.
///
/// Syntax and literal-IP checks only. Hostnames are resolved separately
/// ([`reject_resolved_host`]) so a name that maps to loopback is also refused.
/// That closes casual DNS rebinding; a determined attacker with a TTL-flipping
/// nameserver can still race the lookup against the connect.
pub fn check_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| {
        format!(
            "`{url}` is not an http:// or https:// URL. This tool only fetches web pages; use \
             `read` for files on disk."
        )
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(format!(
            "`{url}` is not an http:// or https:// URL. This tool only fetches web pages; use \
             `read` for files on disk."
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("`{url}` has no host"))?;
    if forbidden_hostname(host) {
        return Err(format!(
            "`{host}` is a private or loopback address, which this tool will not fetch."
        ));
    }
    if let Some(ip) = parse_ip_host(host) {
        if ip_is_forbidden(ip) {
            return Err(format!(
                "`{host}` is a private or loopback address, which this tool will not fetch."
            ));
        }
    }
    Ok(())
}

fn join_redirect(current: &str, location: &str) -> Result<String, String> {
    let base = url::Url::parse(current)
        .map_err(|e| format!("could not parse `{current}` as a URL: {e}"))?;
    base.join(location)
        .map(|u| u.to_string())
        .map_err(|e| format!("redirect Location `{location}` is not a URL: {e}"))
}

/// Resolve `host` and refuse if any address is private/loopback.
///
/// Not a boundary against a TTL-flipping DNS server — the lookup and the
/// connect are separate. It does stop the ordinary case of a public name that
/// points at 127.0.0.1.
async fn reject_resolved_host(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("`{url}` is not a URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("`{url}` has no host"))?
        .to_string();
    if parse_ip_host(&host).is_some() {
        return Ok(());
    }
    let host_for_lookup = host.clone();
    let addrs = tokio::task::spawn_blocking(move || {
        (host_for_lookup.as_str(), 0u16)
            .to_socket_addrs()
            .map(|i| i.map(|a| a.ip()).collect::<Vec<_>>())
    })
    .await
    .map_err(|e| format!("could not resolve `{host}`: {e}"))?
    .map_err(|e| format!("could not resolve `{host}`: {e}"))?;
    for ip in addrs {
        if ip_is_forbidden(ip) {
            return Err(format!(
                "`{host}` resolves to {ip}, a private or loopback address, which this tool \
                 will not fetch."
            ));
        }
    }
    Ok(())
}

fn forbidden_hostname(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost" || h == "localhost." || h.ends_with(".localhost") || h.ends_with(".internal")
}

fn parse_ip_host(host: &str) -> Option<IpAddr> {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(unwrap_mapped(ip));
    }
    parse_weird_ipv4(host).map(IpAddr::V4)
}

/// Decimal dword (`2130706433`), octal (`0177.0.0.1`), hex (`0x7f.0.0.1`).
fn parse_weird_ipv4(host: &str) -> Option<Ipv4Addr> {
    if let Ok(n) = host.parse::<u32>() {
        return Some(Ipv4Addr::from(n));
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut nums = [0u32; 4];
    for (i, part) in parts.iter().enumerate() {
        nums[i] = parse_ipv4_octet(part)?;
    }
    match parts.len() {
        1 => Some(Ipv4Addr::from(nums[0])),
        2 => {
            let a = u8::try_from(nums[0]).ok()?;
            let rest = (nums[1] <= 0x00ff_ffff).then_some(nums[1])?;
            Some(Ipv4Addr::from((u32::from(a) << 24) | rest))
        }
        3 => {
            let a = u8::try_from(nums[0]).ok()?;
            let b = u8::try_from(nums[1]).ok()?;
            let rest = (nums[2] <= 0xffff).then_some(nums[2])?;
            Some(Ipv4Addr::from(
                (u32::from(a) << 24) | (u32::from(b) << 16) | rest,
            ))
        }
        4 => {
            let octs = [
                u8::try_from(nums[0]).ok()?,
                u8::try_from(nums[1]).ok()?,
                u8::try_from(nums[2]).ok()?,
                u8::try_from(nums[3]).ok()?,
            ];
            Some(Ipv4Addr::from(octs))
        }
        _ => None,
    }
}

fn parse_ipv4_octet(part: &str) -> Option<u32> {
    if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16).ok();
    }
    if part.len() > 1 && part.starts_with('0') && part.bytes().all(|b| b.is_ascii_digit()) {
        return u32::from_str_radix(part, 8).ok();
    }
    part.parse().ok()
}

fn unwrap_mapped(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        other => other,
    }
}

fn ip_is_forbidden(ip: IpAddr) -> bool {
    match unwrap_mapped(ip) {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || is_ula(v6)
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Unique local addresses `fc00::/7`.
fn is_ula(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
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
        for url in [
            "ftp://example.com/x",
            "gopher://example.com",
            "javascript:x",
        ] {
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

    #[test]
    fn numeric_hex_and_octal_loopback_are_refused() {
        for url in [
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:7f00:1]/",
        ] {
            assert!(check_url(url).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn ipv6_ula_and_link_local_are_refused() {
        assert!(check_url("http://[fc00::1]/").is_err());
        assert!(check_url("http://[fe80::1]/").is_err());
    }

    #[tokio::test]
    async fn a_hostname_that_resolves_to_loopback_is_refused() {
        let err = reject_resolved_host("http://localhost/")
            .await
            .expect_err("localhost must not resolve through");
        assert!(err.contains("loopback") || err.contains("private"), "{err}");
    }

    /// A 302 to loopback must never be requested. Two listeners: the first
    /// redirects, the second records whether anyone connected.
    #[tokio::test]
    async fn a_redirect_to_loopback_is_not_followed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let private_port = private.local_addr().unwrap().port();
        let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hit_flag = hit.clone();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = private.accept().await {
                hit_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = sock.read(&mut [0u8; 64]).await;
            }
        });

        let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let public_port = public.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = public.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let loc = format!("http://127.0.0.1:{private_port}/secret");
            let resp =
                format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\nContent-Length: 0\r\n\r\n");
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let fetch = WebFetch::new();
        let start = format!("http://127.0.0.1:{public_port}/");
        let result = fetch.get_with_redirects(&start, false).await;
        assert!(
            result.is_err(),
            "following a 302 to loopback must fail, got {result:?}"
        );

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !hit.load(std::sync::atomic::Ordering::SeqCst),
            "the loopback listener received a request — the redirect was followed"
        );
    }
}
