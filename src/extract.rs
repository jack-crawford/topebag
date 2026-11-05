//! Reader-mode extraction.
//!
//! Web pages are mostly chrome: navigation, ads, cookie banners, related
//! links, comments. [`extract`] isolates the main content using
//! [`dom_smoothie`], a Rust port of Mozilla's Readability (the engine behind
//! Firefox Reader View), and gathers metadata for the document header.
//!
//! When Readability cannot find an article (for example on a landing page
//! or a documentation index), Topebag falls back to a simple "strip the
//! obvious junk and keep `<main>` / `<body>`" strategy so that *something*
//! printable is always produced. [`Article::used_fallback`] records which
//! path was taken.

use anyhow::Result;
use dom_smoothie::{Config, Readability};
use scraper::{Html, Selector};
use url::Url;

/// The printable essence of a web page.
#[derive(Debug, Clone, Default)]
pub struct Article {
    pub title: String,
    pub byline: Option<String>,
    pub site_name: Option<String>,
    pub excerpt: Option<String>,
    /// Publication date, already formatted for display (e.g. "5 November 2026").
    pub published: Option<String>,
    /// BCP-47-ish language tag from the page (e.g. `en`, `de-AT`).
    pub lang: Option<String>,
    /// Cleaned HTML of the main content.
    pub content_html: String,
    pub word_count: usize,
    /// `true` when Readability found nothing and the whole page body was used.
    pub used_fallback: bool,
}

/// Minimum amount of text (in characters) for a Readability result to be
/// trusted. Below this the fallback usually gives a better page.
const MIN_TEXT_CHARS: usize = 250;

/// Extract the main content and metadata from `html`, which was served at
/// `url`.
pub fn extract(html: &str, url: &Url) -> Result<Article> {
    let page_lang = html_lang(html);

    // Keep classes: they carry code-block languages (`language-rust`), which
    // the converter uses for syntax highlighting.
    let config = Config { keep_classes: true, ..Config::default() };
    let readability = Readability::new(html, Some(url.as_str()), Some(config)).and_then(|mut r| r.parse());

    let mut article = match readability {
        Ok(found) if found.text_content.trim().chars().count() >= MIN_TEXT_CHARS => Article {
            title: clean(&found.title).unwrap_or_default(),
            byline: found.byline.as_deref().and_then(clean),
            site_name: found.site_name.as_deref().and_then(clean),
            excerpt: found.excerpt.as_deref().and_then(clean),
            published: found.published_time.as_deref().and_then(format_date),
            lang: found.lang.clone().or(page_lang),
            word_count: found.text_content.split_whitespace().count(),
            content_html: found.content.to_string(),
            used_fallback: false,
        },
        Ok(_) | Err(_) => fallback(html, page_lang),
    };

    if article.title.is_empty() {
        article.title = url
            .path_segments()
            .and_then(|mut s| s.rfind(|p| !p.is_empty()).map(str::to_string))
            .map(|slug| slug.replace(['-', '_'], " "))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| url.host_str().unwrap_or("Untitled").to_string());
    }
    if article.site_name.is_none() {
        article.site_name = url.host_str().map(|h| h.trim_start_matches("www.").to_string());
    }
    article.title = strip_site_suffix(&article.title, article.site_name.as_deref(), url.host_str());
    // Bylines sometimes repeat the site name or are absurdly long (bios).
    if article
        .byline
        .as_ref()
        .is_some_and(|b| b.chars().count() > 120 || Some(b) == article.site_name.as_ref())
    {
        article.byline = None;
    }
    Ok(article)
}

/// Remove a trailing " | Site Name" (or " - ", " — ", " · ") from a title
/// when the suffix is clearly the site's name.
pub fn strip_site_suffix(title: &str, site: Option<&str>, host: Option<&str>) -> String {
    let haystack = format!("{} {}", site.unwrap_or(""), host.unwrap_or("")).to_lowercase();
    for sep in [" | ", " — ", " – ", " - ", " · ", " :: "] {
        if let Some(idx) = title.rfind(sep) {
            let (head, tail) = (title[..idx].trim(), title[idx + sep.len()..].trim());
            let words: Vec<String> = tail
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty() && !matches!(w.to_lowercase().as_str(), "the" | "a" | "of"))
                .map(str::to_lowercase)
                .collect();
            // A short suffix whose distinctive words mostly appear in the site
            // name or host ("Rust Blog" on blog.rust-lang.org, "Project
            // Gutenberg" on gutenberg.org) is the site's name.
            let matching = words.iter().filter(|w| w.len() >= 3 && haystack.contains(w.as_str())).count();
            let is_site = !words.is_empty() && words.len() <= 4 && matching * 2 >= words.len();
            if is_site && head.split_whitespace().count() >= 2 {
                return head.to_string();
            }
        }
    }
    title.to_string()
}

/// Whole-page fallback for pages Readability rejects.
fn fallback(html: &str, lang: Option<String>) -> Article {
    let doc = Html::parse_document(html);
    let sel = |s: &str| Selector::parse(s).expect("static selector");

    let title = doc
        .select(&sel(r#"meta[property="og:title"]"#))
        .next()
        .and_then(|m| m.value().attr("content").map(str::to_string))
        .or_else(|| doc.select(&sel("title")).next().map(|t| t.text().collect()))
        .or_else(|| doc.select(&sel("h1")).next().map(|t| t.text().collect()))
        .and_then(|t: String| clean(&t))
        .unwrap_or_default();

    let site_name = doc
        .select(&sel(r#"meta[property="og:site_name"]"#))
        .next()
        .and_then(|m| m.value().attr("content"))
        .and_then(clean);

    let container = doc
        .select(&sel("main"))
        .next()
        .or_else(|| doc.select(&sel("article")).next())
        .or_else(|| doc.select(&sel("body")).next());

    let content_html = container.map(|c| c.inner_html()).unwrap_or_else(|| html.to_string());
    // Junk removal happens in the converter, which skips scripts, styles,
    // navigation, forms and so on (see `convert::SKIPPED_TAGS`).
    let word_count = container
        .map(|c| c.text().flat_map(str::split_whitespace).count())
        .unwrap_or(0);

    Article {
        title,
        site_name,
        lang,
        content_html,
        word_count,
        used_fallback: true,
        ..Article::default()
    }
}

/// The `lang` attribute of the `<html>` element, if any.
fn html_lang(html: &str) -> Option<String> {
    let head = &html[..html.len().min(8192)];
    let lower = head.to_ascii_lowercase();
    let start = lower.find("<html")?;
    let tag_end = lower[start..].find('>')? + start;
    let tag = &head[start..tag_end];
    let idx = tag.to_ascii_lowercase().find("lang=")?;
    let value = tag[idx + 5..].trim_start_matches(['"', '\'']);
    let end = value.find(['"', '\'', ' ', '>']).unwrap_or(value.len());
    let value = value[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Collapse whitespace; return `None` for empty strings.
fn clean(s: &str) -> Option<String> {
    let out = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!out.is_empty()).then_some(out)
}

/// Turn an ISO-8601-ish timestamp into "5 November 2026".
pub fn format_date(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let date = chrono::DateTime::parse_from_rfc3339(raw)
        .map(|d| d.date_naive())
        .or_else(|_| chrono::DateTime::parse_from_rfc2822(raw).map(|d| d.date_naive()))
        .or_else(|_| chrono::NaiveDate::parse_from_str(&raw[..raw.len().min(10)], "%Y-%m-%d"));
    match date {
        Ok(d) => Some(d.format("%-d %B %Y").to_string()),
        // Unknown format: show it verbatim if it is short and readable.
        Err(_) if raw.len() <= 40 => Some(raw.to_string()),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html lang="en-GB"><head>
        <title>The Quiet Joy of Paper | Example Times</title>
        <meta property="og:site_name" content="Example Times">
        <meta property="article:published_time" content="2026-03-14T09:00:00Z">
        </head><body>
        <nav><a href="/">Home</a><a href="/news">News</a></nav>
        <article>
          <h1>The Quiet Joy of Paper</h1>
          <p class="byline">By Ada Lovelace</p>
          <p>Paper never needs charging. It never pings you, never asks you to
          update, and it is perfectly happy to be scribbled on. This is a long
          enough paragraph to look like real article content to Readability,
          which wants a decent amount of text before it trusts a candidate.</p>
          <p>Second paragraph with <a href="/more">a relative link</a> and some
          more words so that the extraction heuristics are satisfied. Printing
          an article turns it into an object you can hold, annotate and share
          across the kitchen table without a single notification.</p>
        </article>
        <footer>© Example Times · Subscribe now!</footer>
        </body></html>"#;

    #[test]
    fn extracts_article_and_metadata() {
        let url = Url::parse("https://example.com/2026/paper").unwrap();
        let a = extract(PAGE, &url).unwrap();
        assert!(!a.used_fallback);
        assert!(a.title.contains("Quiet Joy of Paper"), "title: {}", a.title);
        assert_eq!(a.published.as_deref(), Some("14 March 2026"));
        assert_eq!(a.lang.as_deref(), Some("en-GB"));
        assert!(a.content_html.contains("never needs charging"));
        assert!(!a.content_html.contains("Subscribe now"), "footer removed");
        assert!(a.word_count > 50);
    }

    #[test]
    fn falls_back_for_non_articles() {
        let url = Url::parse("https://example.com/").unwrap();
        let a = extract("<html><title>Hi</title><body><main><p>Short.</p></main></body></html>", &url)
            .unwrap();
        assert!(a.used_fallback);
        assert_eq!(a.title, "Hi");
        assert!(a.content_html.contains("Short."));
        assert_eq!(a.site_name.as_deref(), Some("example.com"));
    }

    #[test]
    fn strips_site_name_from_titles() {
        let host = Some("blog.rust-lang.org");
        assert_eq!(strip_site_suffix("Announcing Rust 1.83.0 | Rust Blog", None, host), "Announcing Rust 1.83.0");
        assert_eq!(
            strip_site_suffix("Why We Print - The Example Review", Some("The Example Review"), None),
            "Why We Print"
        );
        assert_eq!(
            strip_site_suffix("A Modest Proposal | Project Gutenberg", Some("gutenberg.org"), Some("www.gutenberg.org")),
            "A Modest Proposal"
        );
        // Not a site name: keep it.
        assert_eq!(strip_site_suffix("Rust vs Go - A Comparison", None, host), "Rust vs Go - A Comparison");
    }

    #[test]
    fn formats_dates() {
        assert_eq!(format_date("2024-02-29").as_deref(), Some("29 February 2024"));
        assert_eq!(format_date("Tue, 1 Jul 2003 10:52:37 +0200").as_deref(), Some("1 July 2003"));
        assert_eq!(format_date("   "), None);
    }
}
