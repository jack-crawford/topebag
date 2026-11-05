//! HTTP downloading.
//!
//! A single blocking [`reqwest`] client is shared by the whole process. It
//! identifies as a regular desktop browser (plus a `Topebag` token) because
//! many sites serve a stripped-down or empty page to unknown user agents.
//!
//! Only `http` and `https` URLs are accepted, and response bodies are capped
//! (see [`MAX_PAGE_BYTES`] / [`MAX_IMAGE_BYTES`]) so a hostile or broken
//! server cannot exhaust memory.

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, HeaderMap, HeaderValue};
use url::Url;

/// Largest HTML document we are willing to download (8 MiB).
pub const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Largest single image we are willing to download (15 MiB).
pub const MAX_IMAGE_BYTES: u64 = 15 * 1024 * 1024;
/// Largest PDF we are willing to download (64 MiB).
pub const MAX_PDF_BYTES: u64 = 64 * 1024 * 1024;

const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/140.0 Safari/537.36 Topebag/",
    env!("CARGO_PKG_VERSION"),
);

/// A downloaded web page.
#[derive(Debug, Clone)]
pub struct Page {
    /// The URL after following redirects. Relative links resolve against it.
    pub final_url: Url,
    /// The decoded HTML.
    pub html: String,
}

/// A link that turned out to be a PDF document.
#[derive(Debug, Clone)]
pub struct PdfFile {
    pub final_url: Url,
    pub bytes: Vec<u8>,
    /// File name from `Content-Disposition` or the URL path, if any.
    pub filename: Option<String>,
}

/// What a link pointed to.
#[derive(Debug, Clone)]
pub enum Fetched {
    Html(Page),
    Pdf(PdfFile),
}

/// A downloaded binary resource (e.g. an image).
#[derive(Debug, Clone)]
pub struct Resource {
    pub bytes: Vec<u8>,
    /// The `Content-Type` header, lower-cased, without parameters.
    pub content_type: Option<String>,
}

fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9,*;q=0.5"));
        Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(45))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to build HTTP client")
    })
}

/// Parse and validate user input as an `http(s)` URL.
///
/// Bare domains such as `example.com/post` are accepted and upgraded to
/// `https://`.
pub fn parse_url(input: &str) -> Result<Url> {
    let trimmed = input.trim().trim_matches(|c| c == '<' || c == '>' || c == '"');
    if trimmed.is_empty() {
        bail!("empty URL");
    }
    let url = match Url::parse(trimmed) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase) => Url::parse(&format!("https://{trimmed}"))
            .with_context(|| format!("not a valid URL: {trimmed}"))?,
        Err(err) => return Err(anyhow!("not a valid URL: {trimmed} ({err})")),
    };
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("unsupported URL scheme `{other}` (only http and https work)"),
    }
    if url.host_str().is_none_or(|h| !h.contains('.') && h != "localhost" && h.parse::<std::net::IpAddr>().is_err()) {
        bail!("not a valid web address: {trimmed}");
    }
    Ok(url)
}

/// Pull every URL-looking token out of a blob of pasted text.
///
/// Splits on whitespace (and commas), so pasting a list of links — one per
/// line, or separated by spaces — queues each of them.
pub fn urls_in_text(text: &str) -> Vec<Url> {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| t.contains('.') || t.starts_with("http"))
        .filter_map(|t| parse_url(t).ok())
        .collect()
}

/// Download a link: an HTML page (following `<meta http-equiv="refresh">`
/// redirects, common when sites move content, up to three times) or a PDF.
pub fn fetch(url: &Url) -> Result<Fetched> {
    let mut fetched = fetch_once(url)?;
    for _ in 0..3 {
        let Fetched::Html(page) = &fetched else { break };
        match meta_refresh_target(&page.html, &page.final_url) {
            Some(next) if next != page.final_url => fetched = fetch_once(&next)?,
            _ => break,
        }
    }
    Ok(fetched)
}

/// Download an HTML page; PDFs are reported as an error.
pub fn fetch_page(url: &Url) -> Result<Page> {
    match fetch(url)? {
        Fetched::Html(page) => Ok(page),
        Fetched::Pdf(_) => bail!("this link is a PDF, not a web page"),
    }
}

/// Download a text document such as an RSS feed.
pub fn fetch_text(url: &Url) -> Result<String> {
    let response = client()
        .get(url.clone())
        .header(ACCEPT, "application/rss+xml,application/atom+xml,application/xml,text/xml;q=0.9,*/*;q=0.5")
        .send()
        .with_context(|| format!("could not connect to {}", url.host_str().unwrap_or("site")))?;
    let status = response.status();
    if !status.is_success() {
        bail!("the site answered {status} for {url}");
    }
    let bytes = read_capped(response, MAX_PAGE_BYTES)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The target of an immediate (≤ 5 s) `<meta http-equiv="refresh">`, if any.
pub fn meta_refresh_target(html: &str, base: &Url) -> Option<Url> {
    let head = &html[..html.len().min(16 * 1024)];
    let lower = head.to_ascii_lowercase();
    let mut search = 0;
    while let Some(pos) = lower[search..].find("<meta") {
        let start = search + pos;
        let end = start + lower[start..].find('>')?;
        let tag = &lower[start..end];
        search = end;
        if !(tag.contains("http-equiv=\"refresh\"") || tag.contains("http-equiv='refresh'") || tag.contains("http-equiv=refresh")) {
            continue;
        }
        let content_at = tag.find("content=")? + 8;
        let original = &head[start + content_at..end];
        let quote = original.chars().next().filter(|c| *c == '"' || *c == '\'');
        let value = match quote {
            Some(q) => original[1..].split(q).next().unwrap_or(""),
            None => original.split_whitespace().next().unwrap_or(""),
        };
        let (delay, rest) = value.split_once(';').unwrap_or((value, ""));
        if delay.trim().parse::<f64>().map_or(true, |d| d > 5.0) {
            return None;
        }
        let rest = rest.trim();
        let target = rest
            .strip_prefix("url=")
            .or_else(|| rest.strip_prefix("URL="))
            .or_else(|| rest.strip_prefix("Url="))
            .unwrap_or(rest)
            .trim()
            .trim_matches(['"', '\'']);
        if target.is_empty() {
            return None;
        }
        return base.join(target).ok().filter(|u| matches!(u.scheme(), "http" | "https"));
    }
    None
}

fn fetch_once(url: &Url) -> Result<Fetched> {
    let response = client()
        .get(url.clone())
        .header(ACCEPT, "text/html,application/xhtml+xml;q=0.9,application/pdf;q=0.8,*/*;q=0.7")
        .send()
        .with_context(|| format!("could not connect to {}", url.host_str().unwrap_or("site")))?;

    let status = response.status();
    if !status.is_success() {
        bail!("the site answered {status} for {url}");
    }
    let final_url = response.url().clone();
    let content_type = content_type(response.headers());
    let looks_like_pdf = matches!(content_type.as_deref(), Some("application/pdf" | "application/x-pdf"))
        || final_url.path().to_ascii_lowercase().ends_with(".pdf");
    if let Some(ct) = &content_type {
        let textual = ct.contains("html") || ct.contains("xml") || ct.starts_with("text/");
        if !textual && !looks_like_pdf && ct != "application/octet-stream" {
            bail!("this link is not a web page or PDF (content type `{ct}`)");
        }
    }

    let header = |name: reqwest::header::HeaderName| {
        response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
    };
    let charset = header(CONTENT_TYPE)
        .and_then(|v| v.split("charset=").nth(1).map(str::to_string))
        .map(|c| c.trim_matches(|ch| ch == '"' || ch == ' ').to_ascii_lowercase());
    let disposition = header(reqwest::header::CONTENT_DISPOSITION);

    let cap = if looks_like_pdf || content_type.as_deref() == Some("application/octet-stream") {
        MAX_PDF_BYTES
    } else {
        MAX_PAGE_BYTES
    };
    let bytes = read_capped(response, cap)?;

    // Trust the bytes over the headers: some servers label PDFs as HTML or
    // octet-stream.
    if bytes.starts_with(b"%PDF-") {
        let filename = disposition.as_deref().and_then(disposition_filename).or_else(|| {
            final_url
                .path_segments()
                .and_then(|mut s| s.rfind(|p| !p.is_empty()).map(str::to_string))
                .filter(|s| s.contains('.'))
        });
        return Ok(Fetched::Pdf(PdfFile { final_url, bytes, filename }));
    }
    if looks_like_pdf && content_type.as_deref() != Some("text/html") {
        bail!("the link claims to be a PDF but the download is not a valid PDF");
    }
    if content_type.as_deref() == Some("application/octet-stream") {
        bail!("this link is a download, not a web page");
    }
    let html = decode_html(&bytes, charset.as_deref());
    Ok(Fetched::Html(Page { final_url, html }))
}

/// `attachment; filename="report.pdf"` -> `report.pdf`.
fn disposition_filename(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let idx = lower.find("filename=")?;
    let rest = value[idx + 9..].trim_start();
    let name = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split(';').next()?.trim(),
    };
    let name = name.rsplit(['/', '\\']).next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Download an arbitrary resource such as an image.
pub fn fetch_resource(url: &Url, referer: Option<&Url>) -> Result<Resource> {
    let mut request = client()
        .get(url.clone())
        .header(ACCEPT, "image/avif,image/webp,image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5");
    if let Some(referer) = referer {
        request = request.header(reqwest::header::REFERER, referer.as_str());
    }
    let response = request.send().with_context(|| format!("downloading {url}"))?;
    if !response.status().is_success() {
        bail!("{} for {url}", response.status());
    }
    let content_type = content_type(response.headers());
    let bytes = read_capped(response, MAX_IMAGE_BYTES)?;
    Ok(Resource { bytes, content_type })
}

fn content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

fn read_capped(response: reqwest::blocking::Response, cap: u64) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|len| len > cap) {
        bail!("response is larger than {} MiB", cap / 1024 / 1024);
    }
    let mut bytes = Vec::new();
    response
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .context("connection dropped while downloading")?;
    if bytes.len() as u64 > cap {
        bail!("response is larger than {} MiB", cap / 1024 / 1024);
    }
    Ok(bytes)
}

/// Decode HTML bytes into a `String`.
///
/// Uses the charset from the HTTP header if present, then a `<meta charset>`
/// sniff of the first few KiB, and finally assumes UTF-8 (lossily).
pub fn decode_html(bytes: &[u8], header_charset: Option<&str>) -> String {
    let sniffed = || {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_ascii_lowercase();
        let idx = head.find("charset=")?;
        let rest = &head[idx + 8..];
        let rest = rest.trim_start_matches(['"', '\'']);
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    };
    let label = header_charset.map(str::to_string).or_else(sniffed);
    if let Some(label) = label {
        if let Some(encoding) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            let (text, _, _) = encoding.decode(bytes);
            return text.into_owned();
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        assert_eq!(parse_url("example.com/a").unwrap().as_str(), "https://example.com/a");
        assert!(parse_url("ftp://example.com").is_err());
        assert!(parse_url("hello").is_err());
        assert!(parse_url("http://localhost:8080/x").is_ok());
        assert!(parse_url("http://127.0.0.1:9/x").is_ok());
    }

    #[test]
    fn finds_urls_in_pasted_text() {
        let urls = urls_in_text("read https://a.com/x and\nhttps://b.org/y, thanks");
        let urls: Vec<_> = urls.iter().map(Url::as_str).collect();
        assert_eq!(urls, ["https://a.com/x", "https://b.org/y"]);
    }

    #[test]
    fn parses_content_disposition() {
        assert_eq!(disposition_filename(r#"attachment; filename="Q3 report.pdf""#).as_deref(), Some("Q3 report.pdf"));
        assert_eq!(disposition_filename("inline; filename=paper.pdf; size=3").as_deref(), Some("paper.pdf"));
        assert_eq!(disposition_filename("inline"), None);
    }

    #[test]
    fn follows_meta_refresh() {
        let base = Url::parse("https://blog.example.com/2024/old.html").unwrap();
        let html = r#"<html><head><meta http-equiv="refresh" content="0; url=/releases/new.html"></head></html>"#;
        assert_eq!(
            meta_refresh_target(html, &base).unwrap().as_str(),
            "https://blog.example.com/releases/new.html"
        );
        let slow = r#"<meta http-equiv="refresh" content="60; url=/elsewhere">"#;
        assert!(meta_refresh_target(slow, &base).is_none(), "slow refreshes are ignored");
        assert!(meta_refresh_target("<p>no refresh</p>", &base).is_none());
    }

    #[test]
    fn decodes_legacy_charsets() {
        let latin1 = b"<meta charset=\"iso-8859-1\"><p>caf\xe9</p>";
        assert!(decode_html(latin1, None).contains("café"));
        assert!(decode_html("naïve".as_bytes(), Some("utf-8")).contains("naïve"));
    }
}
