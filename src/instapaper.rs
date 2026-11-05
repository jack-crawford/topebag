//! Printing your Instapaper queue.
//!
//! Instapaper publishes a private RSS feed of your **Unread** folder (on
//! instapaper.com: *Unread → Downloads menu → RSS Feed*). Topebag reads that
//! feed, shows which saves haven't been printed yet, and fetches and prints
//! them like any pasted link. No password or API key is involved; the feed
//! URL itself is the secret, so keep it private.
//!
//! Any RSS 2.0 or Atom feed works the same way, so this also covers other
//! read-later services and blogs.
//!
//! ## State
//!
//! `instapaper.json` in the data directory remembers:
//!
//! * `seen` — items you printed or skipped, which never show up again;
//! * `known` — every item ever observed, so "print new saves automatically"
//!   only prints things saved *after* you turned it on, never your backlog.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::fetch;

/// One saved article from the feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedItem {
    /// Stable identifier (the item's `guid`, or its link).
    pub id: String,
    pub title: String,
    /// The original article URL.
    pub url: String,
    /// When it was saved, as given by the feed (display only).
    pub saved: Option<String>,
}

/// What Topebag remembers between checks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueState {
    pub seen: BTreeSet<String>,
    pub known: BTreeSet<String>,
    pub last_checked: Option<DateTime<Local>>,
}

/// Path of the state file inside the data directory.
pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("instapaper.json")
}

impl QueueState {
    pub fn load(path: &Path) -> QueueState {
        fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Items not yet printed or skipped, in feed order.
    pub fn unseen<'a>(&self, items: &'a [FeedItem]) -> Vec<&'a FeedItem> {
        items.iter().filter(|i| !self.seen.contains(&i.id)).collect()
    }

    /// Record a successful check. Returns the items that appeared since the
    /// previous check (empty on the very first check, so a backlog is never
    /// treated as "new").
    pub fn observe(&mut self, items: &[FeedItem]) -> Vec<FeedItem> {
        let first_check = self.last_checked.is_none();
        let fresh: Vec<FeedItem> = items
            .iter()
            .filter(|i| !self.known.contains(&i.id) && !self.seen.contains(&i.id))
            .cloned()
            .collect();
        self.known.extend(items.iter().map(|i| i.id.clone()));
        self.last_checked = Some(Local::now());
        if first_check { Vec::new() } else { fresh }
    }

    /// Mark items as handled (printed or skipped).
    pub fn mark_seen<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>) {
        for id in ids {
            self.seen.insert(id.to_string());
            self.known.insert(id.to_string());
        }
    }
}

/// Validate a feed URL entered by the user.
pub fn parse_feed_url(input: &str) -> Result<Url> {
    let url = fetch::parse_url(input)?;
    if url.host_str().is_some_and(|h| h.ends_with("instapaper.com")) && !url.path().contains("/rss") {
        bail!(
            "that is an Instapaper page, not its RSS feed — on instapaper.com open Unread, then the \
             Downloads menu, and copy the “RSS Feed” link"
        );
    }
    Ok(url)
}

/// Download and parse a feed.
pub fn fetch_feed(feed_url: &Url) -> Result<Vec<FeedItem>> {
    let xml = fetch::fetch_text(feed_url).context("could not download the feed")?;
    parse_feed(&xml)
}

/// Parse RSS 2.0 or Atom into items (in feed order, duplicates removed).
pub fn parse_feed(xml: &str) -> Result<Vec<FeedItem>> {
    let xml = xml.trim_start_matches('\u{feff}');
    // DTDs stay disabled (roxmltree's default): feeds are untrusted input
    // and entity expansion is a classic denial-of-service vector.
    let doc = roxmltree::Document::parse(xml).map_err(|e| anyhow!("this is not a valid RSS feed ({e})"))?;
    let root = doc.root_element();
    let child_text = |node: roxmltree::Node, name: &str| -> Option<String> {
        node.children()
            .find(|c| c.is_element() && c.tag_name().name() == name)
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    };

    let mut items = Vec::new();
    match root.tag_name().name() {
        "rss" | "RDF" => {
            for item in root.descendants().filter(|n| n.is_element() && n.tag_name().name() == "item") {
                let Some(url) = child_text(item, "link") else { continue };
                let id = child_text(item, "guid").unwrap_or_else(|| url.clone());
                let title = child_text(item, "title").unwrap_or_else(|| url.clone());
                let saved = child_text(item, "pubDate").or_else(|| child_text(item, "date"));
                items.push(FeedItem { id, title, url, saved });
            }
        }
        "feed" => {
            for entry in root.children().filter(|n| n.is_element() && n.tag_name().name() == "entry") {
                let link = entry
                    .children()
                    .filter(|c| c.is_element() && c.tag_name().name() == "link")
                    .find(|c| c.attribute("rel").is_none_or(|r| r == "alternate"))
                    .and_then(|c| c.attribute("href"))
                    .map(str::to_string);
                let Some(url) = link else { continue };
                let id = child_text(entry, "id").unwrap_or_else(|| url.clone());
                let title = child_text(entry, "title").unwrap_or_else(|| url.clone());
                let saved = child_text(entry, "updated").or_else(|| child_text(entry, "published"));
                items.push(FeedItem { id, title, url, saved });
            }
        }
        other => bail!("this is not an RSS or Atom feed (root element <{other}>)"),
    }

    let mut ids = BTreeSet::new();
    items.retain(|i| fetch::parse_url(&i.url).is_ok() && ids.insert(i.id.clone()));
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel>
  <title>Instapaper: Unread</title>
  <link>https://www.instapaper.com/u</link>
  <item>
    <guid isPermaLink="false">1001</guid>
    <title>Why We Still Print Things</title>
    <link>https://example.com/print</link>
    <pubDate>Tue, 04 Nov 2026 18:00:00 GMT</pubDate>
  </item>
  <item>
    <title>No guid here</title>
    <link>https://example.org/second</link>
  </item>
  <item><title>Broken</title><link>javascript:alert(1)</link></item>
  <item><guid>1001</guid><title>Duplicate</title><link>https://example.com/print</link></item>
</channel></rss>"#;

    #[test]
    fn parses_rss() {
        let items = parse_feed(RSS).unwrap();
        assert_eq!(items.len(), 2, "invalid links and duplicates are dropped");
        assert_eq!(items[0].id, "1001");
        assert_eq!(items[0].title, "Why We Still Print Things");
        assert_eq!(items[0].saved.as_deref(), Some("Tue, 04 Nov 2026 18:00:00 GMT"));
        assert_eq!(items[1].id, "https://example.org/second");
    }

    #[test]
    fn parses_atom() {
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>x</title>
            <entry><id>tag:1</id><title>Atom post</title>
              <link rel="alternate" href="https://example.com/atom"/><updated>2026-11-01T00:00:00Z</updated></entry>
            </feed>"#;
        let items = parse_feed(atom).unwrap();
        assert_eq!(items[0].url, "https://example.com/atom");
        assert_eq!(items[0].title, "Atom post");
    }

    #[test]
    fn rejects_non_feeds() {
        assert!(parse_feed("<html><body>hi</body></html>").is_err());
        assert!(parse_feed("not xml").is_err());
        assert!(parse_feed_url("https://www.instapaper.com/u").is_err());
        assert!(parse_feed_url("https://www.instapaper.com/rss/123/abcDEF").is_ok());
    }

    #[test]
    fn backlog_is_not_new_but_later_saves_are() {
        let items = parse_feed(RSS).unwrap();
        let mut state = QueueState::default();
        assert!(state.observe(&items).is_empty(), "first check: backlog only");
        assert_eq!(state.unseen(&items).len(), 2, "but it is still in the queue");

        let mut more = items.clone();
        more.insert(0, FeedItem { id: "2002".into(), title: "New".into(), url: "https://example.net/new".into(), saved: None });
        let fresh = state.observe(&more);
        assert_eq!(fresh.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["2002"]);

        state.mark_seen(["2002", "1001"]);
        assert_eq!(state.unseen(&more).len(), 1);

        let dir = tempfile::tempdir().unwrap();
        let path = state_path(dir.path());
        state.save(&path).unwrap();
        assert_eq!(QueueState::load(&path), state);
    }
}
