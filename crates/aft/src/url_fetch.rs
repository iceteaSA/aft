use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use htmd::{
    element_handler::{HandlerResult, Handlers},
    Element, HtmlToMarkdown,
};
use reqwest::blocking::{Client, Response as HttpResponse};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION, USER_AGENT};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::parser::detect_language;

const MAX_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;
const CACHE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(30_000);
const BODY_CHUNK_TIMEOUT: Duration = Duration::from_millis(15_000);
const MAX_REDIRECTS: usize = 5;

/// Retry budget for transient connect/transport failures only. Agents
/// shouldn't have to retry manually for a single TCP/TLS hiccup. We cap
/// at 2 retries (= 3 total attempts) with short jittered backoff so a
/// genuinely-broken host fails fast instead of dragging the foreground
/// fetch out to many seconds.
///
/// We deliberately do NOT retry on:
///   - HTTP error status (4xx/5xx) — the server actually answered
///   - Redirect errors / SSRF rejections — those are deterministic
///   - Body read stalls — already handled by BODY_CHUNK_TIMEOUT
const TRANSIENT_RETRY_ATTEMPTS: usize = 2;
const TRANSIENT_RETRY_BACKOFFS_MS: [u64; TRANSIENT_RETRY_ATTEMPTS] = [200, 600];
const ACCEPT_HEADER: &str = "application/vnd.github.raw, text/markdown, text/x-markdown, text/html;q=0.9, application/json;q=0.8, text/plain;q=0.5";
const USER_AGENT_VALUE: &str = "aft-opencode-plugin";
const CONVERTED_MARKDOWN_CONTENT_TYPE: &str = "text/markdown; charset=utf-8";

#[derive(Clone, Default)]
pub struct UrlFetchOptions {
    pub allow_private: bool,
    /// Test hook: treat a hostname as resolving to these IPs during SSRF validation.
    /// Production callers leave this empty and use `std::net::ToSocketAddrs`.
    #[doc(hidden)]
    pub public_host_overrides: Vec<(String, Vec<IpAddr>)>,
    /// Test hook: force reqwest to connect a hostname to a local mock server while
    /// SSRF validation still sees `public_host_overrides` above.
    #[doc(hidden)]
    pub connect_overrides: Vec<(String, SocketAddr)>,
    /// Test hook: observes the temp path immediately before the atomic rename.
    #[doc(hidden)]
    pub atomic_write_observer: Option<Arc<dyn Fn(&Path, &Path) + Send + Sync>>,
}

#[derive(Debug, Clone)]
pub struct UrlFetchError {
    message: String,
}

impl UrlFetchError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for UrlFetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UrlFetchError {}

#[derive(Debug, Serialize, Deserialize)]
struct CacheMeta {
    url: String,
    #[serde(rename = "contentType")]
    content_type: String,
    extension: String,
    #[serde(rename = "fetchedAt")]
    fetched_at: u64,
}

pub fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

pub fn fetch_url_to_cache(
    url: &str,
    storage_dir: &Path,
    options: UrlFetchOptions,
) -> Result<PathBuf, UrlFetchError> {
    let parsed = Url::parse(url).map_err(|_| UrlFetchError::new(format!("Invalid URL: {url}")))?;
    validate_public_url(&parsed, &options)?;

    let dir = cache_dir(storage_dir);
    fs::create_dir_all(&dir).map_err(|error| {
        UrlFetchError::new(format!(
            "Failed to create URL cache directory {}: {error}",
            dir.display()
        ))
    })?;

    let hash = hash_url(url);
    let meta_file = meta_path(storage_dir, &hash);
    if let Some(cached) = fresh_cached_path(storage_dir, &hash, &meta_file, &parsed)? {
        return Ok(cached);
    }

    let response = fetch_with_redirects(&parsed, url, &options)?;
    if !response.status().is_success() {
        return Err(UrlFetchError::new(format!(
            "HTTP {} {} fetching {url}",
            response.status().as_u16(),
            response.status().canonical_reason().unwrap_or("")
        )));
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/plain")
        .to_string();
    let (extension, from_source_path) =
        resolve_fetch_extension(&parsed, &content_type).ok_or_else(|| {
            UrlFetchError::new(format!(
                "Unsupported content type '{content_type}' for {url}. Supported: text/html, text/markdown, application/json, text/plain; source files via URL path extension (e.g. .rs, .ts, .mjs)"
            ))
        })?;

    if let Some(length) = response.content_length() {
        if length > MAX_RESPONSE_BYTES {
            return Err(UrlFetchError::new(format!(
                "Response too large: {length} bytes (max {MAX_RESPONSE_BYTES})"
            )));
        }
    }

    let body = read_response_body(response, url)?;
    if from_source_path && body_contains_nul_in_prefix(&body) {
        return Err(UrlFetchError::new(format!(
            "Binary content detected for source URL {url}"
        )));
    }
    let (body, content_type, extension) = if extension == ".html" {
        (
            convert_html_body_to_markdown(&body, url)?,
            CONVERTED_MARKDOWN_CONTENT_TYPE.to_string(),
            ".md",
        )
    } else {
        (body, content_type, extension)
    };

    let content_file = content_path(storage_dir, &hash, extension);
    atomic_write(&content_file, &body, &options)?;

    let meta = CacheMeta {
        url: url.to_string(),
        content_type,
        extension: extension.to_string(),
        fetched_at: now_ms(),
    };
    let meta_bytes = serde_json::to_vec(&meta).map_err(|error| {
        UrlFetchError::new(format!("Failed to encode URL cache metadata: {error}"))
    })?;
    atomic_write(&meta_file, &meta_bytes, &options)?;

    Ok(content_file)
}

pub fn cleanup_url_cache(storage_dir: &Path) -> Result<usize, UrlFetchError> {
    let dir = cache_dir(storage_dir);
    if !dir.exists() {
        return Ok(0);
    }

    let entries = fs::read_dir(&dir).map_err(|error| {
        UrlFetchError::new(format!(
            "URL cache cleanup failed reading {}: {error}",
            dir.display()
        ))
    })?;
    let mut removed = 0usize;
    let now = now_ms();

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".meta.json") {
            continue;
        }

        let meta = fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str::<CacheMeta>(&content).ok());
        let Some(meta) = meta else {
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
            continue;
        };

        if now.saturating_sub(meta.fetched_at) <= CACHE_TTL_MS {
            continue;
        }

        let hash = name.trim_end_matches(".meta.json");
        let content = content_path(storage_dir, hash, &meta.extension);
        let _ = fs::remove_file(content);
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }

    Ok(removed)
}

#[doc(hidden)]
pub fn cache_content_path_for_url(storage_dir: &Path, url: &str, extension: &str) -> PathBuf {
    content_path(storage_dir, &hash_url(url), extension)
}

#[doc(hidden)]
pub fn cache_meta_path_for_url(storage_dir: &Path, url: &str) -> PathBuf {
    meta_path(storage_dir, &hash_url(url))
}

#[doc(hidden)]
pub fn is_private_ip_for_test(ip: IpAddr) -> bool {
    is_private_ip(ip)
}

fn cache_dir(storage_dir: &Path) -> PathBuf {
    storage_dir.join("url_cache")
}

fn hash_url(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    format!("{digest:x}").chars().take(16).collect()
}

fn meta_path(storage_dir: &Path, hash: &str) -> PathBuf {
    cache_dir(storage_dir).join(format!("{hash}.meta.json"))
}

fn content_path(storage_dir: &Path, hash: &str, extension: &str) -> PathBuf {
    cache_dir(storage_dir).join(format!("{hash}{extension}"))
}

fn fresh_cached_path(
    storage_dir: &Path,
    hash: &str,
    meta_file: &Path,
    url: &Url,
) -> Result<Option<PathBuf>, UrlFetchError> {
    if !meta_file.exists() {
        return Ok(None);
    }

    let meta = match fs::read_to_string(meta_file)
        .ok()
        .and_then(|content| serde_json::from_str::<CacheMeta>(&content).ok())
    {
        Some(meta) => meta,
        None => return Ok(None),
    };
    let age = now_ms().saturating_sub(meta.fetched_at);
    if meta.extension == ".html" {
        return Ok(None);
    }

    let content_type = meta.content_type.as_str();
    let current = resolve_fetch_extension(url, content_type);
    let expected_ext = current.map(|(ext, _)| ext);
    if expected_ext != Some(meta.extension.as_str()) {
        return Ok(None);
    }

    let cached = content_path(storage_dir, hash, &meta.extension);
    if age < CACHE_TTL_MS && cached.exists() {
        return Ok(Some(cached));
    }
    Ok(None)
}

fn fetch_with_redirects(
    start_url: &Url,
    original_url: &str,
    options: &UrlFetchOptions,
) -> Result<HttpResponse, UrlFetchError> {
    let client = build_client(options)?;
    let mut current_url = start_url.clone();

    for redirect_count in 0..=MAX_REDIRECTS {
        validate_public_url(&current_url, options)?;
        let response = send_with_transient_retries(&client, &current_url)?;

        if !response.status().is_redirection() {
            return Ok(response);
        }
        if redirect_count == MAX_REDIRECTS {
            return Err(UrlFetchError::new(format!(
                "Too many redirects fetching {original_url}"
            )));
        }

        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                UrlFetchError::new(format!(
                    "Redirect from {} missing Location header",
                    current_url.as_str()
                ))
            })?;
        current_url = current_url.join(location).map_err(|error| {
            UrlFetchError::new(format!(
                "Invalid redirect Location '{location}' from {}: {error}",
                current_url.as_str()
            ))
        })?;
    }

    Err(UrlFetchError::new(format!(
        "Too many redirects fetching {original_url}"
    )))
}

/// Issue a single GET with the configured User-Agent + Accept headers and
/// transparently retry only on transient connect/transport failures.
///
/// Returns the response (including 4xx/5xx — caller decides how to treat
/// those). On a non-transient reqwest error (e.g. an HTTP-shaped reply that
/// reqwest still surfaces as Err, or a TLS handshake fault that doesn't read
/// as `is_connect`), the original error is returned immediately so the user
/// sees the real failure without an artificial 800ms-plus delay.
fn send_with_transient_retries(
    client: &Client,
    target: &Url,
) -> Result<HttpResponse, UrlFetchError> {
    let mut last_error: Option<reqwest::Error> = None;
    for attempt in 0..=TRANSIENT_RETRY_ATTEMPTS {
        let result = client
            .get(target.clone())
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, ACCEPT_HEADER)
            .send();
        match result {
            Ok(response) => return Ok(response),
            Err(error) => {
                if attempt < TRANSIENT_RETRY_ATTEMPTS && is_transient_reqwest_error(&error) {
                    thread::sleep(Duration::from_millis(TRANSIENT_RETRY_BACKOFFS_MS[attempt]));
                    last_error = Some(error);
                    continue;
                }
                return Err(UrlFetchError::new(format!(
                    "Failed to fetch {}: {}",
                    target.as_str(),
                    reqwest_error_detail(&error)
                )));
            }
        }
    }
    // Loop fell through after the last allowed retry exhausted — surface the
    // most recent transient error rather than swallowing it.
    Err(UrlFetchError::new(format!(
        "Failed to fetch {} after {} retries: {}",
        target.as_str(),
        TRANSIENT_RETRY_ATTEMPTS,
        last_error
            .as_ref()
            .map(reqwest_error_detail)
            .unwrap_or_else(|| "unknown transient error".to_string())
    )))
}

/// Classify a reqwest error as transient (worth a quick retry) vs terminal.
///
/// Transient: TCP connect failures, request-build/send TCP-level failures
/// that don't carry status, and timeouts. These typically clear on a single
/// retry — agents shouldn't have to ask twice for a momentary blip.
///
/// Terminal: anything where reqwest got far enough to decode an HTTP-shaped
/// reply (`is_status()`, `is_body()`, `is_decode()`). Retrying those would
/// just hammer a server that already answered.
fn is_transient_reqwest_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request()
}

fn build_client(options: &UrlFetchOptions) -> Result<Client, UrlFetchError> {
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(CONNECT_TIMEOUT);

    for (host, address) in &options.connect_overrides {
        builder = builder.resolve(host, *address);
    }

    builder
        .build()
        .map_err(|error| UrlFetchError::new(format!("Failed to build URL fetch client: {error}")))
}

fn validate_public_url(url: &Url, options: &UrlFetchOptions) -> Result<(), UrlFetchError> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(UrlFetchError::new(format!(
            "Only http:// and https:// URLs are supported, got: {}:",
            url.scheme()
        )));
    }
    if options.allow_private {
        return Ok(());
    }

    let host = url
        .host_str()
        .ok_or_else(|| UrlFetchError::new(format!("URL missing host: {url}")))?;
    let host_for_parse = host
        .trim_matches(['[', ']'])
        .split('%')
        .next()
        .unwrap_or(host);

    if let Ok(ip) = host_for_parse.parse::<IpAddr>() {
        reject_private_ip(host, ip)?;
        return Ok(());
    }
    if host_for_parse.contains(':') {
        return Err(UrlFetchError::new(format!(
            "Blocked private URL host {host} ({host_for_parse})"
        )));
    }

    let addresses = resolve_host_ips(host_for_parse, url.port_or_known_default(), options)?;
    if addresses.is_empty() {
        return Err(UrlFetchError::new(format!(
            "Failed to resolve URL host {host}"
        )));
    }
    for ip in addresses {
        reject_private_ip(host, ip)?;
    }

    // We validate all resolved addresses before issuing the request. Reqwest's
    // default resolver runs again during TCP connect, leaving the same small
    // DNS-rebinding window the old Bun fallback accepted. A custom per-request
    // resolver hook would close that window but adds complexity for marginal
    // value in this opt-in agent-tooling surface.
    Ok(())
}

fn resolve_host_ips(
    host: &str,
    port: Option<u16>,
    options: &UrlFetchOptions,
) -> Result<Vec<IpAddr>, UrlFetchError> {
    if let Some((_, ips)) = options
        .public_host_overrides
        .iter()
        .find(|(override_host, _)| override_host == host)
    {
        return Ok(ips.clone());
    }

    let port = port.unwrap_or(80);
    let addrs = (host, port).to_socket_addrs().map_err(|error| {
        UrlFetchError::new(format!("Failed to resolve URL host {host}: {error}"))
    })?;
    Ok(addrs.map(|addr| addr.ip()).collect())
}

fn reject_private_ip(host: &str, ip: IpAddr) -> Result<(), UrlFetchError> {
    if is_private_ip(ip) {
        return Err(UrlFetchError::new(format!(
            "Blocked private URL host {host} ({ip})"
        )));
    }
    Ok(())
}

/// True for any private/link-local/CGNAT/benchmark/multicast/reserved/loopback
/// address (the full set this module refuses to fetch). Exposed so the semantic
/// embedding SSRF guard shares one authoritative range list instead of keeping a
/// drifting copy. Note: this INCLUDES loopback — callers that intentionally
/// allow loopback (e.g. a local Ollama endpoint) must exclude it themselves.
pub fn is_private_or_reserved_ip(ip: IpAddr) -> bool {
    is_private_ip(ip)
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_private_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_private_ipv6(ipv6),
    }
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 169 && b == 254)
        // RFC 6598 Shared Address Space (CGNAT): 100.64.0.0/10. Not globally
        // routable; used for provider/VPC-internal endpoints — must not be
        // reachable via SSRF.
        || (a == 100 && (64..=127).contains(&b))
        // RFC 2544 benchmark subnet: 198.18.0.0/15. Reserved, non-routable.
        || (a == 198 && (18..=19).contains(&b))
        || a >= 224
}

fn is_private_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let top_six_zero = segments[..6].iter().all(|segment| *segment == 0);
    let is_mapped = segments[..5].iter().all(|segment| *segment == 0) && segments[5] == 0xffff;
    if is_mapped || top_six_zero {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            (segments[6] & 0xff) as u8,
            (segments[7] >> 8) as u8,
            (segments[7] & 0xff) as u8,
        );
        return is_private_ipv4(embedded);
    }

    let first = segments[0];
    (0xfe80..=0xfebf).contains(&first) || (0xfc00..=0xfdff).contains(&first) || first >= 0xff00
}

const BINARY_SNIFF_PREFIX: usize = 8 * 1024;

fn body_contains_nul_in_prefix(body: &[u8]) -> bool {
    let end = body.len().min(BINARY_SNIFF_PREFIX);
    body[..end].contains(&0)
}

/// Resolve cache extension: URL path (parsed, percent-decoded) first for source
/// languages AFT parses; otherwise content-type mapping.
fn resolve_fetch_extension(url: &Url, content_type: &str) -> Option<(&'static str, bool)> {
    if let Some(ext) = extension_from_url_path(url) {
        return Some((ext, true));
    }
    resolve_extension_from_content_type(content_type).map(|ext| (ext, false))
}

fn extension_from_url_path(url: &Url) -> Option<&'static str> {
    let path = url.path();
    if path.is_empty() || path == "/" {
        return None;
    }
    let segment = path.rsplit('/').next().unwrap_or(path);
    let file_name = percent_decode_path_segment(segment);
    let dot = file_name.rfind('.')?;
    let ext = &file_name[dot + 1..];
    if ext.is_empty() {
        return None;
    }
    let probe = Path::new("file").with_extension(ext);
    if detect_language(&probe).is_some() {
        static_extension_for_lang_ext(ext)
    } else {
        None
    }
}

fn percent_decode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let bytes = segment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h1), Some(h2)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push(char::from(h1 << 4 | h2));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Dotted cache extension for a path extension that `detect_language` accepts.
fn static_extension_for_lang_ext(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => Some(".ts"),
        "tsx" => Some(".tsx"),
        "js" => Some(".js"),
        "jsx" => Some(".jsx"),
        "mjs" => Some(".mjs"),
        "cjs" => Some(".cjs"),
        "py" | "pyi" => Some(".py"),
        "rs" => Some(".rs"),
        "go" => Some(".go"),
        "c" | "h" => Some(".c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(".cpp"),
        "zig" => Some(".zig"),
        "cs" => Some(".cs"),
        "sh" | "bash" | "zsh" => Some(".sh"),
        "html" | "htm" => Some(".html"),
        "md" | "markdown" | "mdx" => Some(".md"),
        "sol" => Some(".sol"),
        "scss" => Some(".scss"),
        "vue" => Some(".vue"),
        "json" | "jsonc" => Some(".json"),
        "scala" | "sc" => Some(".scala"),
        "java" => Some(".java"),
        "rb" => Some(".rb"),
        "kt" | "kts" => Some(".kt"),
        "swift" => Some(".swift"),
        "inc" | "php" => Some(".php"),
        "lua" => Some(".lua"),
        "pl" | "pm" | "t" => Some(".pl"),
        "yaml" | "yml" => Some(".yaml"),
        _ => None,
    }
}

fn resolve_extension_from_content_type(content_type: &str) -> Option<&'static str> {
    let lower = content_type.to_ascii_lowercase();
    let media_type = lower
        .split(';')
        .next()
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .trim();

    match media_type {
        "text/html"
        | "application/xhtml+xml"
        | "application/vnd.github.html"
        | "application/vnd.github+html" => Some(".html"),
        "text/markdown"
        | "text/x-markdown"
        | "application/markdown"
        | "application/vnd.github.raw"
        | "application/vnd.github+raw"
        | "application/vnd.github.v3.raw"
        | "text/plain" => Some(".md"),
        "application/json" | "application/ld+json" => Some(".json"),
        other if other.ends_with("+json") => Some(".json"),
        "text/javascript" | "application/javascript" | "application/ecmascript" => Some(".js"),
        "text/typescript" | "application/typescript" => Some(".ts"),
        _ => None,
    }
}

fn convert_html_body_to_markdown(body: &[u8], url: &str) -> Result<Vec<u8>, UrlFetchError> {
    let html = String::from_utf8_lossy(body);
    let mut markdown = html_to_markdown_converter()
        .convert(&html)
        .map_err(|error| {
            UrlFetchError::new(format!(
                "Failed to convert HTML from {url} to Markdown: {error}"
            ))
        })?;
    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }
    Ok(markdown.into_bytes())
}

fn html_to_markdown_converter() -> HtmlToMarkdown {
    HtmlToMarkdown::builder()
        .skip_tags(vec![
            "head", "script", "style", "nav", "footer", "aside", "noscript",
        ])
        .add_handler(
            vec!["a"],
            |handlers: &dyn Handlers, element: Element| -> Option<HandlerResult> {
                if is_permalink_anchor(&element) {
                    None
                } else {
                    handlers.fallback(element)
                }
            },
        )
        .add_handler(
            vec!["header"],
            |handlers: &dyn Handlers, element: Element| -> Option<HandlerResult> {
                if should_skip_header(&element) {
                    None
                } else {
                    handlers.fallback(element)
                }
            },
        )
        .add_handler(
            vec!["span"],
            |handlers: &dyn Handlers, element: Element| -> Option<HandlerResult> {
                if element_has_class_token(&element, "token-line") {
                    let mut content = handlers.walk_children(element.node).content;
                    content.push('\n');
                    Some(content.into())
                } else {
                    handlers.fallback(element)
                }
            },
        )
        .build()
}

fn is_permalink_anchor(element: &Element<'_>) -> bool {
    element_has_class_token(element, "hash-link")
        || element_attr_value(element, "aria-label")
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("direct link to"))
}

fn should_skip_header(element: &Element<'_>) -> bool {
    element_has_class_token(element, "navbar")
        || element_has_class_token(element, "site-header")
        || element_has_class_token(element, "site-nav")
        || element_has_class_token(element, "topbar")
        || element_attr_value(element, "role")
            .is_some_and(|value| value.eq_ignore_ascii_case("banner"))
        || element_attr_value(element, "id").is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("navbar") || value.contains("site-header") || value.contains("site-nav")
        })
}

fn element_has_class_token(element: &Element<'_>, token: &str) -> bool {
    element_attr_value(element, "class")
        .is_some_and(|value| value.split_ascii_whitespace().any(|class| class == token))
}

fn element_attr_value<'a>(element: &'a Element<'_>, name: &str) -> Option<&'a str> {
    element
        .attrs
        .iter()
        .find(|attr| attr.name.local.as_ref() == name)
        .map(|attr| attr.value.as_ref())
}

enum BodyReadEvent {
    Chunk(Vec<u8>),
    Done,
    Error(io::ErrorKind, String),
}

fn read_response_body(mut response: HttpResponse, url: &str) -> Result<Vec<u8>, UrlFetchError> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match response.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.send(BodyReadEvent::Done);
                    break;
                }
                Ok(n) => {
                    if tx.send(BodyReadEvent::Chunk(buffer[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let kind = error.kind();
                    let message = error.to_string();
                    let _ = tx.send(BodyReadEvent::Error(kind, message));
                    break;
                }
            }
        }
    });

    let mut chunks = Vec::new();
    let mut total = 0u64;
    loop {
        match rx.recv_timeout(BODY_CHUNK_TIMEOUT) {
            Ok(BodyReadEvent::Chunk(chunk)) => {
                total += chunk.len() as u64;
                if total > MAX_RESPONSE_BYTES {
                    return Err(UrlFetchError::new(format!(
                        "Response exceeded {MAX_RESPONSE_BYTES} bytes, aborted"
                    )));
                }
                chunks.extend_from_slice(&chunk);
            }
            Ok(BodyReadEvent::Done) => return Ok(chunks),
            Ok(BodyReadEvent::Error(kind, _message)) if is_body_stall_kind(kind) => {
                return Err(body_stall_error(url));
            }
            Ok(BodyReadEvent::Error(_, message)) => {
                return Err(UrlFetchError::new(format!(
                    "Failed to read response body for {url}: {message}"
                )));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(body_stall_error(url)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(UrlFetchError::new(format!(
                    "Failed to read response body for {url}: body reader stopped unexpectedly"
                )));
            }
        }
    }
}

fn body_stall_error(url: &str) -> UrlFetchError {
    UrlFetchError::new(format!(
        "Body read stalled (no data for {}ms) fetching {url}",
        BODY_CHUNK_TIMEOUT.as_millis()
    ))
}

fn is_body_stall_kind(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

fn atomic_write(
    final_path: &Path,
    bytes: &[u8],
    options: &UrlFetchOptions,
) -> Result<(), UrlFetchError> {
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        UrlFetchError::new(format!(
            "Failed to create URL cache parent {}: {error}",
            parent.display()
        ))
    })?;

    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            UrlFetchError::new(format!("Invalid cache path: {}", final_path.display()))
        })?;
    let tmp_path = final_path.with_file_name(format!(
        "{file_name}.tmp-{}-{}",
        std::process::id(),
        random_nonce()
    ));

    let write_result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(UrlFetchError::new(format!(
            "Failed to write URL cache temp file {}: {error}",
            tmp_path.display()
        )));
    }

    if let Some(observer) = &options.atomic_write_observer {
        observer(&tmp_path, final_path);
    }

    fs::rename(&tmp_path, final_path).map_err(|error| {
        let _ = fs::remove_file(&tmp_path);
        UrlFetchError::new(format!(
            "Failed to finalize URL cache file {}: {error}",
            final_path.display()
        ))
    })
}

fn random_nonce() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let fallback = now_ms() ^ u64::from(std::process::id());
        bytes = fallback.to_le_bytes();
    }
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn reqwest_error_detail(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return format!("timeout: {error}");
    }
    if let Some(source) = error.source() {
        return format!("{source}");
    }
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn extension_from_path_uses_parser_mapping() {
        let url = Url::parse("https://example.com/pkg/index.mjs").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/javascript").unwrap();
        assert_eq!(ext, ".mjs");
        assert!(from_path);
    }

    #[test]
    fn text_plain_rs_url_ignores_content_type_gate() {
        let url = Url::parse("https://raw.githubusercontent.com/o/r/main/lib.rs").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".rs");
        assert!(from_path);
    }

    #[test]
    fn extensionless_javascript_maps_to_js() {
        let url = Url::parse("https://cdn.example/bundle").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/javascript").unwrap();
        assert_eq!(ext, ".js");
        assert!(!from_path);
    }

    #[test]
    fn extensionless_plain_stays_md() {
        let url = Url::parse("https://example.com/readme").unwrap();
        let (ext, _) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".md");
    }

    #[test]
    fn query_and_fragment_do_not_break_path_extension() {
        let url = Url::parse("https://example.com/src/file.ts?v=2#L10").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".ts");
        assert!(from_path);
    }

    #[test]
    fn percent_encoded_path_segment() {
        let url = Url::parse("https://example.com/foo%2Fbar.rs").unwrap();
        // path is /foo%2Fbar.rs - segment is foo%2Fbar.rs -> decode to foo/bar.rs -> ext rs
        let (ext, _) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".rs");
    }

    #[test]
    fn binary_sniff_detects_nul() {
        let mut body = vec![b'f', b'n', 0, b' '];
        assert!(body_contains_nul_in_prefix(&body));
        body = vec![b'h'; 9000];
        assert!(!body_contains_nul_in_prefix(&body));
    }

    #[test]
    fn unsupported_pdf_still_errors_via_resolve() {
        let url = Url::parse("https://example.com/doc.pdf").unwrap();
        assert!(resolve_fetch_extension(&url, "application/pdf").is_none());
    }
}
