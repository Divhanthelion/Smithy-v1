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

use std::collections::HashSet;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{StatusCode, Url};
use serde_json::Value;

use crate::registry::{ExecutionControl, Tool, ToolCtx};
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
const MAX_REDIRECTS: usize = 5;

/// A browser-ish agent string. Some documentation hosts serve a JavaScript
/// challenge to unrecognised clients, which renders as an empty page — an
/// answer far more confusing than an error.
const USER_AGENT: &str = concat!("Smithy/", env!("CARGO_PKG_VERSION"), " (+agent)");

pub struct WebFetch {
    resolver: Arc<dyn Resolver>,
}

impl WebFetch {
    pub fn new() -> Self {
        Self {
            resolver: Arc::new(SystemResolver),
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

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        self.run_controlled(args, ctx, &ExecutionControl::default())
            .await
    }

    async fn run_controlled(
        &self,
        args: &Value,
        _ctx: &ToolCtx,
        control: &ExecutionControl,
    ) -> ToolOutput {
        let input = match arg_str(args, "url") {
            Ok(u) => u.trim(),
            Err(e) => return ToolOutput::err(e),
        };
        let url = match parse_url(input) {
            Ok(url) => url,
            Err(error) => return ToolOutput::err(error),
        };
        let max_chars = arg_i64(args, "max_chars")
            .map(|n| (n.max(500) as usize).min(HARD_MAX_CHARS))
            .unwrap_or(DEFAULT_MAX_CHARS);

        let control = control.bounded_by(TIMEOUT);
        let (bytes, content_type, final_url) =
            match fetch_bytes(url.clone(), self.resolver.as_ref(), &control).await {
                Ok(result) => result,
                Err(error) => return ToolOutput::err(error),
            };
        let requested_url = url.to_string();
        match controlled_blocking(&control, move || {
            render_download(
                bytes,
                content_type,
                final_url,
                requested_url,
                max_chars,
            )
        })
        .await
        {
            Ok(text) => ToolOutput::ok(text),
            Err(error) => ToolOutput::err(error),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ResolvedAddress {
    validated: IpAddr,
    connect: SocketAddr,
}

type ResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<ResolvedAddress>, String>> + Send + 'a>>;

trait Resolver: Send + Sync {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a>;
}

struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a> {
        Box::pin(async move {
            tokio::net::lookup_host((host, port))
                .await
                .map_err(|error| format!("DNS lookup for `{host}` failed: {error}"))
                .map(|answers| {
                    answers
                        .map(|connect| ResolvedAddress {
                            validated: connect.ip(),
                            connect,
                        })
                        .collect()
                })
        })
    }
}

fn parse_url(input: &str) -> Result<Url, String> {
    let url = Url::parse(input).map_err(|error| format!("`{input}` is not a valid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "`{input}` is not an http:// or https:// URL. This tool only fetches web pages; use \
             `read` for files on disk."
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("`{input}` contains user information, which is not allowed."));
    }
    if url.host().is_none() {
        return Err(format!("`{input}` has no host"));
    }
    Ok(url)
}

pub fn check_url(input: &str) -> Result<(), String> {
    let url = parse_url(input)?;
    let host = url.host_str().expect("parse_url requires a host");
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".internal") {
        return Err(format!(
            "`{host}` is a private or loopback host, which this tool will not fetch."
        ));
    }
    let literal = host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host);
    match literal.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if !is_public_ipv4(ip) => private_address_error(ip.into()),
        Ok(IpAddr::V6(ip)) if !is_public_ipv6(ip) => private_address_error(ip.into()),
        _ => Ok(()),
    }
}

fn private_address_error(ip: IpAddr) -> Result<(), String> {
    Err(format!(
        "`{ip}` is a special, private, or non-routable address, which this tool will not fetch."
    ))
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 100 && (64..=127).contains(&b)
        || a == 127
        || a == 169 && b == 254
        || a == 172 && (16..=31).contains(&b)
        // Blanket-denying 192/8 blocked ordinary public sites. These are the
        // IANA special-purpose allocations inside it; whole allocation blocks
        // are denied so future use of an anycast exception cannot become an
        // accidental route inward.
        || a == 192
            && ((b == 0 && (c == 0 || c == 2))
                || (b == 31 && c == 196)
                || (b == 52 && c == 193)
                || (b == 88 && c == 99)
                || b == 168
                || (b == 175 && c == 48))
        || a == 198 && (b == 18 || b == 19 || b == 51 && c == 100)
        || a == 203 && b == 0 && c == 113
        || a >= 224)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = embedded_ipv4(ip) {
        return is_public_ipv4(v4);
    }
    let segments = ip.segments();
    (segments[0] & 0xe000 == 0x2000)
        && !(ip.is_unspecified()
        || ip.is_loopback()
        || segments[0] & 0xfe00 == 0xfc00
        || segments[0] & 0xff00 == 0xfe00
        || segments[0] & 0xff00 == 0xff00
        || segments[0] == 0x0100 && segments[1..].iter().all(|segment| *segment == 0)
        || segments[0] == 0x0064 && segments[1] == 0xff9b
        || segments[0] == 0x2001 && segments[1] <= 0x01ff
        || segments[0] == 0x2001 && segments[1] == 0x0db8
        || segments[0] == 0x2002
        || segments[0] == 0x3fff && segments[1] & 0xf000 == 0)
}

fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let octets = ip.octets();
    if octets[..10] == [0; 10] && (octets[10..12] == [0, 0] || octets[10..12] == [0xff, 0xff]) {
        Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ))
    } else {
        None
    }
}

fn validate_answers(host: &str, answers: Vec<ResolvedAddress>) -> Result<Vec<SocketAddr>, String> {
    if answers.is_empty() {
        return Err(format!("DNS lookup for `{host}` returned no addresses."));
    }
    let mut public = Vec::new();
    let mut rejected = Vec::new();
    for answer in answers {
        let allowed = match answer.validated {
            IpAddr::V4(ip) => is_public_ipv4(ip),
            IpAddr::V6(ip) => is_public_ipv6(ip),
        };
        if allowed {
            public.push(answer.connect);
        } else {
            rejected.push(answer.validated);
        }
    }
    if !rejected.is_empty() {
        let kind = if public.is_empty() {
            "only non-public"
        } else {
            "a mixture of public and non-public"
        };
        return Err(format!(
            "DNS lookup for `{host}` returned {kind} addresses ({rejected:?}); refusing the entire \
             answer set."
        ));
    }
    public.sort_unstable();
    public.dedup();
    Ok(public)
}

async fn controlled<T>(
    control: &ExecutionControl,
    future: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        reason = control.cancelled() => Err(reason),
        result = future => result,
    }
}

async fn controlled_blocking<T: Send + 'static>(
    control: &ExecutionControl,
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    control.check()?;
    let task = tokio::task::spawn_blocking(job);
    tokio::select! {
        biased;
        reason = control.cancelled() => Err(reason),
        result = task => result
            .map_err(|error| format!("web rendering task failed: {error}"))?,
    }
}

fn render_download(
    bytes: Vec<u8>,
    content_type: String,
    final_url: Url,
    requested_url: String,
    max_chars: usize,
) -> Result<String, String> {
    let body = String::from_utf8_lossy(&bytes);
    let text = if is_html(&content_type, &body) {
        render_html(&body)
    } else {
        body.to_string()
    };
    let text = collapse_blank_lines(text.trim());
    if text.is_empty() {
        return Err(format!(
            "`{requested_url}` returned no readable text. It may require JavaScript; try the \
             project's raw documentation or repository instead."
        ));
    }
    Ok(truncate(&text, max_chars, final_url.as_str()))
}

async fn fetch_bytes(
    mut url: Url,
    resolver: &dyn Resolver,
    control: &ExecutionControl,
) -> Result<(Vec<u8>, String, Url), String> {
    let original = url.clone();
    let mut visited = HashSet::new();
    let mut redirects = 0;
    loop {
        control.check()?;
        check_url(url.as_str())?;
        let mut loop_key = url.clone();
        loop_key.set_fragment(None);
        if !visited.insert(loop_key.to_string()) {
            return Err(format!("redirect loop detected at `{url}`."));
        }

        let raw_host = url
            .host_str()
            .ok_or_else(|| format!("`{url}` has no host"))?;
        let host = raw_host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(raw_host)
            .to_string();
        let port = url
            .port_or_known_default()
            .ok_or_else(|| format!("`{url}` has no usable port"))?;
        let literal = host.parse::<IpAddr>().ok();
        let answers = if let Some(ip) = literal {
            vec![ResolvedAddress {
                validated: ip,
                connect: SocketAddr::new(ip, port),
            }]
        } else {
            controlled(control, resolver.resolve(&host, port)).await?
        };
        let addresses = validate_answers(&host, answers)?;

        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if literal.is_none() {
            builder = builder.resolve_to_addrs(&host, &addresses);
        }
        let client = builder
            .build()
            .map_err(|error| format!("could not prepare request for `{url}`: {error}"))?;
        let request = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        let mut response = controlled(control, async {
            request
                .send()
                .await
                .map_err(|error| format!("could not fetch `{url}`: {error}"))
        })
        .await?;
        let status = response.status();

        if is_redirect(status) {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| format!("`{url}` returned HTTP {status} without a Location header."))?
                .to_str()
                .map_err(|_| format!("`{url}` returned a non-text Location header."))?;
            if redirects == MAX_REDIRECTS {
                return Err(format!(
                    "`{original}` exceeded the limit of {MAX_REDIRECTS} redirects."
                ));
            }
            let next = url
                .join(location)
                .map_err(|error| format!("`{url}` returned an invalid redirect `{location}`: {error}"))?;
            parse_url(next.as_str())?;
            url = next;
            redirects += 1;
            continue;
        }

        if !status.is_success() {
            return Err(format!(
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

        if let Some(encoding) = response.headers().get(reqwest::header::CONTENT_ENCODING) {
            let encoding = encoding
                .to_str()
                .map_err(|_| format!("`{url}` returned an invalid Content-Encoding header."))?;
            if !encoding.trim().is_empty() && !encoding.eq_ignore_ascii_case("identity") {
                return Err(format!(
                    "`{url}` returned unsupported Content-Encoding `{encoding}`; only identity is \
                     accepted."
                ));
            }
        }
        if response.content_length().is_some_and(|length| length > MAX_BODY_BYTES as u64) {
            return Err(too_large(&url, response.content_length().unwrap()));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        let mut bytes = Vec::new();
        while let Some(chunk) = controlled(control, async {
            response
                .chunk()
                .await
                .map_err(|error| format!("could not read `{url}`: {error}"))
        })
        .await?
        {
            if chunk.len() > MAX_BODY_BYTES - bytes.len() {
                return Err(too_large(&url, bytes.len() as u64 + chunk.len() as u64));
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok((bytes, content_type, url));
    }
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn too_large(url: &Url, bytes: u64) -> String {
    format!(
        "`{url}` is larger than the {} MiB download limit ({bytes} bytes reported or received). \
         Fetch a more specific page.",
        MAX_BODY_BYTES / (1024 * 1024)
    )
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
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct FakeResolver {
        answers: Arc<Mutex<VecDeque<Vec<ResolvedAddress>>>>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeResolver {
        fn new(answers: Vec<Vec<ResolvedAddress>>) -> Self {
            Self {
                answers: Arc::new(Mutex::new(answers.into())),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Resolver for FakeResolver {
        fn resolve<'a>(&'a self, host: &'a str, _port: u16) -> ResolveFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let answer = self.answers.lock().unwrap().pop_front();
            Box::pin(async move {
                answer.ok_or_else(|| format!("fake DNS had no answer left for `{host}`"))
            })
        }
    }

    struct Reply {
        bytes: Vec<u8>,
        delay: std::time::Duration,
    }

    impl Reply {
        fn immediate(bytes: impl Into<Vec<u8>>) -> Self {
            Self {
                bytes: bytes.into(),
                delay: std::time::Duration::ZERO,
            }
        }
    }

    fn server(
        make_replies: impl FnOnce(u16) -> Vec<Reply>,
    ) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let replies = make_replies(address.port());
        let thread = std::thread::spawn(move || {
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut chunk).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                }
                std::thread::sleep(reply.delay);
                let _ = stream.write_all(&reply.bytes);
            }
        });
        (address, thread)
    }

    fn mapped(public: IpAddr, connect: SocketAddr) -> ResolvedAddress {
        ResolvedAddress {
            validated: public,
            connect,
        }
    }

    fn public_v4() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))
    }

    fn ok_response(body: &[u8]) -> Vec<u8> {
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn chunked_response(body: &[u8], encoding: Option<&str>) -> Vec<u8> {
        let encoding = encoding
            .map(|value| format!("Content-Encoding: {value}\r\n"))
            .unwrap_or_default();
        let mut response =
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n{encoding}\r\n").into_bytes();
        for chunk in body.chunks(8191) {
            response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(chunk);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        response
    }

    async fn fetch_for_test(
        url: &str,
        resolver: &FakeResolver,
        control: &ExecutionControl,
    ) -> Result<(Vec<u8>, String, Url), String> {
        fetch_bytes(parse_url(url).unwrap(), resolver, control).await
    }

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
    fn user_information_is_rejected_even_when_the_host_is_public() {
        assert!(check_url("https://user:secret@example.com/").is_err());
    }

    #[test]
    fn ports_do_not_disguise_a_private_host() {
        assert!(check_url("http://192.168.0.5:8443/admin").is_err());
    }

    /// The URL parser, rather than string prefix matching, must expose legacy
    /// numeric IPv4 forms and IPv4 embedded in IPv6 before policy runs.
    #[test]
    fn disguised_ipv4_and_ipv6_loopback_are_refused() {
        for url in [
            "http://2130706433/",
            "http://0177.0.0.1/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::127.0.0.1]/",
        ] {
            assert!(check_url(url).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn private_and_mixed_dns_answers_are_refused_whole() {
        let connect = SocketAddr::from((Ipv4Addr::LOCALHOST, 80));
        let private = mapped(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), connect);
        let public = mapped(public_v4(), connect);
        let private_error = validate_answers("private.test", vec![private]).unwrap_err();
        assert!(private_error.contains("only non-public"), "{private_error}");
        let mixed_error = validate_answers("mixed.test", vec![public, private]).unwrap_err();
        assert!(mixed_error.contains("mixture"), "{mixed_error}");
    }

    /// Missing one IANA special-purpose family reopens SSRF through a spelling
    /// that looks unlike the familiar RFC 1918 and `::1` examples.
    #[test]
    fn every_non_public_address_family_is_refused() {
        for address in [
            "0.1.2.3",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.2",
            "169.254.1.2",
            "172.20.1.2",
            "192.0.0.9",
            "192.168.1.2",
            "198.18.0.1",
            "198.51.100.2",
            "203.0.113.2",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:10.0.0.1",
            "64:ff9b::1",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "fc00::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "4000::1",
        ] {
            let ip: IpAddr = address.parse().unwrap();
            let allowed = match ip {
                IpAddr::V4(ip) => is_public_ipv4(ip),
                IpAddr::V6(ip) => is_public_ipv6(ip),
            };
            assert!(!allowed, "{address} should be refused");
        }
        for address in ["1.1.1.1", "172.32.0.1", "2001:4860:4860::8888", "2606:4700::1111"] {
            let ip: IpAddr = address.parse().unwrap();
            let allowed = match ip {
                IpAddr::V4(ip) => is_public_ipv4(ip),
                IpAddr::V6(ip) => is_public_ipv6(ip),
            };
            assert!(allowed, "{address} should remain public");
        }
    }

    /// Denying all of 192/8 fixed SSRF by breaking public 192.x sites. Keep the
    /// IANA allocations closed while proving both sides of each block remain
    /// reachable.
    #[test]
    fn only_explicit_special_purpose_192_ranges_are_refused() {
        for address in [
            "192.0.0.0",
            "192.0.0.255",
            "192.0.2.0",
            "192.0.2.255",
            "192.31.196.0",
            "192.31.196.255",
            "192.52.193.0",
            "192.52.193.255",
            "192.88.99.0",
            "192.88.99.255",
            "192.168.0.0",
            "192.168.255.255",
            "192.175.48.0",
            "192.175.48.255",
        ] {
            assert!(
                !is_public_ipv4(address.parse().unwrap()),
                "{address} should be refused"
            );
        }
        for address in [
            "192.0.1.255",
            "192.0.3.0",
            "192.31.195.255",
            "192.31.197.0",
            "192.52.192.255",
            "192.52.194.0",
            "192.88.98.255",
            "192.88.100.0",
            "192.167.255.255",
            "192.169.0.0",
            "192.175.47.255",
            "192.175.49.0",
            "192.200.1.1",
        ] {
            assert!(
                is_public_ipv4(address.parse().unwrap()),
                "{address} should remain public"
            );
        }
    }

    /// The validated answer must be the address used by the connector. This
    /// fake separates its policy address from the local test socket so a success
    /// proves the hostname override, not ambient DNS, made the connection.
    #[tokio::test]
    async fn validated_dns_answers_are_pinned_for_the_connection() {
        let (address, server) = server(|_| vec![Reply::immediate(ok_response(b"pinned"))]);
        let resolver = FakeResolver::new(vec![vec![mapped(public_v4(), address)]]);
        let result = fetch_for_test(
            "http://does-not-exist.invalid/",
            &resolver,
            &ExecutionControl::default().bounded_by(TIMEOUT),
        )
        .await
        .unwrap();
        assert_eq!(result.0, b"pinned");
        server.join().unwrap();
    }

    /// A redirect target is resolved and rejected before any request builder can
    /// connect to it; checking only the eventual response URL is too late.
    #[tokio::test]
    async fn a_redirect_to_a_private_dns_answer_is_rejected_before_connect() {
        let (address, server) = server(|_| {
            vec![Reply::immediate(
                b"HTTP/1.1 302 Found\r\nLocation: http://inside.test/secret\r\nContent-Length: 0\r\n\r\n"
                    .to_vec(),
            )]
        });
        let resolver = FakeResolver::new(vec![
            vec![mapped(public_v4(), address)],
            vec![mapped(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
            )],
        ]);
        let error = fetch_for_test(
            "http://outside.test/",
            &resolver,
            &ExecutionControl::default().bounded_by(TIMEOUT),
        )
        .await
        .unwrap_err();
        assert!(error.contains("only non-public"), "{error}");
        assert_eq!(resolver.calls(), 2);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn relative_redirects_re_resolve_the_host_on_every_hop() {
        let (address, server) = server(|_| {
            vec![
                Reply::immediate(
                    b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n"
                        .to_vec(),
                ),
                Reply::immediate(ok_response(b"arrived")),
            ]
        });
        let resolver = FakeResolver::new(vec![
            vec![mapped(public_v4(), address)],
            vec![mapped(public_v4(), address)],
        ]);
        let result = fetch_for_test(
            "http://redirect.test/start",
            &resolver,
            &ExecutionControl::default().bounded_by(TIMEOUT),
        )
        .await
        .unwrap();
        assert_eq!(result.0, b"arrived");
        assert_eq!(result.2.path(), "/final");
        assert_eq!(resolver.calls(), 2);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn redirect_loops_and_a_sixth_hop_are_rejected() {
        let (loop_address, loop_server) = server(|_| {
            vec![Reply::immediate(
                b"HTTP/1.1 302 Found\r\nLocation: /same\r\nContent-Length: 0\r\n\r\n".to_vec(),
            )]
        });
        let loop_resolver =
            FakeResolver::new(vec![vec![mapped(public_v4(), loop_address)]]);
        let error = fetch_for_test(
            "http://loop.test/same",
            &loop_resolver,
            &ExecutionControl::default().bounded_by(TIMEOUT),
        )
        .await
        .unwrap_err();
        assert!(error.contains("redirect loop"), "{error}");
        loop_server.join().unwrap();

        let (chain_address, chain_server) = server(|_| {
            (1..=6)
                .map(|next| {
                    Reply::immediate(
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: /{next}\r\nContent-Length: 0\r\n\r\n"
                        )
                        .into_bytes(),
                    )
                })
                .collect()
        });
        let chain_resolver = FakeResolver::new(
            (0..6)
                .map(|_| vec![mapped(public_v4(), chain_address)])
                .collect(),
        );
        let error = fetch_for_test(
            "http://chain.test/0",
            &chain_resolver,
            &ExecutionControl::default().bounded_by(TIMEOUT),
        )
        .await
        .unwrap_err();
        assert!(error.contains("limit of 5 redirects"), "{error}");
        assert_eq!(chain_resolver.calls(), 6);
        chain_server.join().unwrap();
    }

    #[tokio::test]
    async fn exact_and_oversized_chunked_bodies_enforce_the_byte_ceiling() {
        let exact = vec![b'x'; MAX_BODY_BYTES];
        let oversized = vec![b'y'; MAX_BODY_BYTES + 1];
        let (address, server) = server(|_| {
            vec![
                Reply::immediate(chunked_response(&exact, None)),
                Reply::immediate(chunked_response(&oversized, None)),
            ]
        });
        let resolver = FakeResolver::new(vec![
            vec![mapped(public_v4(), address)],
            vec![mapped(public_v4(), address)],
        ]);
        let control = ExecutionControl::default().bounded_by(TIMEOUT);
        let result = fetch_for_test("http://body.test/exact", &resolver, &control)
            .await
            .unwrap();
        assert_eq!(result.0.len(), MAX_BODY_BYTES);
        let error = fetch_for_test("http://body.test/over", &resolver, &control)
            .await
            .unwrap_err();
        assert!(error.contains("8 MiB"), "{error}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn known_oversize_and_non_identity_encoding_are_rejected_without_bodies() {
        let (address, server) = server(|_| {
            vec![
                Reply::immediate(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                        MAX_BODY_BYTES + 1
                    )
                    .into_bytes(),
                ),
                Reply::immediate(chunked_response(b"not really gzip", Some("gzip"))),
            ]
        });
        let resolver = FakeResolver::new(vec![
            vec![mapped(public_v4(), address)],
            vec![mapped(public_v4(), address)],
        ]);
        let control = ExecutionControl::default().bounded_by(TIMEOUT);
        let size_error = fetch_for_test("http://headers.test/size", &resolver, &control)
            .await
            .unwrap_err();
        assert!(size_error.contains("8 MiB"), "{size_error}");
        let encoding_error = fetch_for_test("http://headers.test/encoding", &resolver, &control)
            .await
            .unwrap_err();
        assert!(
            encoding_error.contains("unsupported Content-Encoding `gzip`"),
            "{encoding_error}"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cancellation_and_one_absolute_deadline_interrupt_body_reads() {
        let delayed = || Reply {
            bytes: ok_response(b"late"),
            delay: std::time::Duration::from_secs(2),
        };
        let (address, server) = server(|_| vec![delayed(), delayed()]);
        let resolver = FakeResolver::new(vec![
            vec![mapped(public_v4(), address)],
            vec![mapped(public_v4(), address)],
        ]);

        let (control, stopper) = ExecutionControl::for_turn(
            crate::ExecutionToken::new(1, 1),
            std::time::Duration::from_secs(10),
        );
        let stop = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            stopper.stop();
        });
        let error = fetch_for_test("http://slow.test/stop", &resolver, &control)
            .await
            .unwrap_err();
        assert_eq!(error, "stopped by user");
        stop.await.unwrap();

        let (deadline, _) = ExecutionControl::with_deadline(
            crate::ExecutionToken::new(2, 1),
            tokio::time::Instant::now() + std::time::Duration::from_millis(50),
        );
        let error = fetch_for_test("http://slow.test/deadline", &resolver, &deadline)
            .await
            .unwrap_err();
        assert_eq!(error, "turn deadline reached");
        server.join().unwrap();
    }

    /// Download completion is not permission to spend unbounded CPU. A late
    /// blocking renderer may finish because Tokio cannot cancel a running
    /// blocking closure, but its value must stay detached after the deadline.
    #[tokio::test]
    async fn cpu_rendering_obeys_control_and_quarantines_its_late_result() {
        let (control, _) = ExecutionControl::with_deadline(
            crate::ExecutionToken::new(3, 1),
            tokio::time::Instant::now() + std::time::Duration::from_millis(30),
        );
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let job_finished = finished.clone();
        let started = tokio::time::Instant::now();
        let error = controlled_blocking(&control, move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            job_finished.store(true, Ordering::SeqCst);
            Ok("late")
        })
        .await
        .unwrap_err();

        assert_eq!(error, "turn deadline reached");
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        assert!(!finished.load(Ordering::SeqCst));
        tokio::time::sleep(std::time::Duration::from_millis(175)).await;
        assert!(finished.load(Ordering::SeqCst));

        let (control, stopper) = ExecutionControl::for_turn(
            crate::ExecutionToken::new(4, 1),
            std::time::Duration::from_secs(10),
        );
        stopper.stop();
        let error = controlled_blocking(&control, || Ok("must not start"))
            .await
            .unwrap_err();
        assert_eq!(error, "stopped by user");
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
