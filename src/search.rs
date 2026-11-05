//! Library search: titles, sites, tags and the full text of articles.
//!
//! Query syntax (case-insensitive):
//!
//! * `paper typography` — every word must appear somewhere (title, site,
//!   byline, tags or article text);
//! * `"exact phrase"` — the phrase must appear as written;
//! * `tag:long-reads` — only entries with that tag (`tag:"long reads"` works too);
//! * `-word` — exclude entries containing the word.
//!
//! The index holds the lower-cased text of every entry in memory. That's a
//! few MB for hundreds of articles, small enough that a linear scan per
//! keystroke is instant and no database is needed.

use std::collections::HashMap;

use crate::library::{Entry, Library};

/// One search term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Word(String),
    Tag(String),
    Not(String),
}

/// Parse a query string into terms.
pub fn parse_query(query: &str) -> Vec<Term> {
    let mut terms = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut token = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c == '"' {
                quoted = !quoted;
                chars.next();
                continue;
            }
            if c.is_whitespace() && !quoted {
                break;
            }
            token.push(c);
            chars.next();
        }
        let token = token.to_lowercase();
        if let Some(tag) = token.strip_prefix("tag:") {
            if !tag.is_empty() {
                terms.push(Term::Tag(crate::library::normalize_tag(tag)));
            }
        } else if let Some(word) = token.strip_prefix('-').filter(|w| !w.is_empty()) {
            terms.push(Term::Not(word.to_string()));
        } else if !token.is_empty() {
            terms.push(Term::Word(token));
        }
    }
    terms
}

/// Why an entry matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// A short excerpt around the first match in the article text, when the
    /// match came from the body rather than the title or metadata.
    pub snippet: Option<String>,
}

/// Lower-cased article texts, keyed by entry id.
#[derive(Debug, Default, Clone)]
pub struct SearchIndex {
    texts: HashMap<String, String>,
}

impl SearchIndex {
    /// Load the text of every entry (reads `article.txt` files).
    pub fn build(library: &Library, entries: &[Entry]) -> SearchIndex {
        let mut index = SearchIndex::default();
        for entry in entries {
            index.add(library, entry);
        }
        index
    }

    /// Add or refresh one entry.
    pub fn add(&mut self, library: &Library, entry: &Entry) {
        if let Some(text) = library.text(&entry.id) {
            self.texts.insert(entry.id.clone(), text.to_lowercase());
        }
    }

    /// Insert text directly (used by tests).
    pub fn insert(&mut self, id: &str, text: &str) {
        self.texts.insert(id.to_string(), text.to_lowercase());
    }

    pub fn remove(&mut self, id: &str) {
        self.texts.remove(id);
    }

    pub fn len(&self) -> usize {
        self.texts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }

    /// Does `entry` match `terms`? An empty query matches everything.
    pub fn matches(&self, entry: &Entry, terms: &[Term]) -> Option<Hit> {
        let meta = format!(
            "{} {} {} {} {} {}",
            entry.title,
            entry.site_name.as_deref().unwrap_or(""),
            entry.domain(),
            entry.byline.as_deref().unwrap_or(""),
            entry.tags.join(" "),
            entry.url,
        )
        .to_lowercase();
        let body = self.texts.get(&entry.id).map(String::as_str).unwrap_or("");

        let mut snippet_at: Option<(usize, usize)> = None;
        for term in terms {
            match term {
                Term::Tag(tag) => {
                    // `tag:long-reads` also finds the tag "long reads".
                    if !entry.tags.iter().any(|t| t == tag || t.replace(' ', "-") == *tag) {
                        return None;
                    }
                }
                Term::Not(word) => {
                    if meta.contains(word.as_str()) || body.contains(word.as_str()) {
                        return None;
                    }
                }
                Term::Word(word) => {
                    if meta.contains(word.as_str()) {
                        continue;
                    }
                    let pos = body.find(word.as_str())?;
                    snippet_at.get_or_insert((pos, word.len()));
                }
            }
        }
        Some(Hit { snippet: snippet_at.map(|(pos, len)| snippet(body, pos, len)) })
    }
}

/// About 140 characters of context around `body[pos..pos+len]`.
fn snippet(body: &str, pos: usize, len: usize) -> String {
    let start = floor_char(body, pos.saturating_sub(60));
    let end = floor_char(body, (pos + len + 80).min(body.len()));
    let mut s: String = body[start..end].split_whitespace().collect::<Vec<_>>().join(" ");
    if start > 0 {
        s.insert(0, '…');
    }
    if end < body.len() {
        s.push('…');
    }
    s
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, title: &str, tags: &[&str]) -> Entry {
        Entry {
            id: id.into(),
            title: title.into(),
            url: format!("https://example.com/{id}"),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Entry::default()
        }
    }

    #[test]
    fn parses_queries() {
        assert_eq!(
            parse_query(r#"Paper "smart quotes" tag:long-reads -ads"#),
            vec![
                Term::Word("paper".into()),
                Term::Word("smart quotes".into()),
                Term::Tag("long-reads".into()),
                Term::Not("ads".into()),
            ]
        );
        assert!(parse_query("   ").is_empty());
    }

    #[test]
    fn searches_metadata_tags_and_full_text() {
        let mut index = SearchIndex::default();
        let a = entry("a", "Why we print", &["paper"]);
        let b = entry("b", "Rust release notes", &[]);
        index.insert("a", "Paper never needs charging. The Gutenberg press changed everything.");
        index.insert("b", "Const generics and other improvements.");

        let q = |s: &str| parse_query(s);
        assert!(index.matches(&a, &q("print")).unwrap().snippet.is_none(), "title match has no snippet");
        let hit = index.matches(&a, &q("gutenberg")).unwrap();
        assert!(hit.snippet.unwrap().contains("gutenberg press"));
        assert!(index.matches(&b, &q("gutenberg")).is_none());
        assert!(index.matches(&a, &q("tag:paper")).is_some());
        let c = entry("c", "Notes", &["long reads", "covid-19"]);
        assert!(index.matches(&c, &q("tag:long-reads")).is_some());
        assert!(index.matches(&c, &q("tag:covid-19")).is_some());
        assert!(index.matches(&b, &q("tag:paper")).is_none());
        assert!(index.matches(&b, &q("const -generics")).is_none());
        assert!(index.matches(&b, &q("")).is_some());
        assert!(index.matches(&a, &q("\"never needs\"")).is_some());
    }
}
