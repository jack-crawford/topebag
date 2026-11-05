//! The on-disk library of saved documents.
//!
//! Everything lives under one data directory (see [`data_dir`]):
//!
//! ```text
//! <data dir>/
//!   settings.json
//!   library/
//!     2026-11-05_143012_the-quiet-joy-of-paper/
//!       entry.json      metadata (this module's [`Entry`])
//!       article.html    cleaned article HTML, kept for re-typesetting
//!       main.typ        generated Typst source
//!       img/…           downloaded images
//!       qr.svg          QR code (optional)
//!       document.pdf    the print-ready PDF
//! ```
//!
//! Each entry is self-contained, so the library can be backed up, synced or
//! inspected with ordinary file tools. There is no database to corrupt: the
//! list of entries is simply the list of folders with a valid `entry.json`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::images::StoredImage;

/// Environment variable that overrides the data directory.
pub const HOME_ENV: &str = "TOPEBAG_HOME";

/// The Topebag data directory.
///
/// `%LOCALAPPDATA%\Topebag` on Windows (local, not roaming, because PDFs
/// and images add up), `~/Library/Application Support/Topebag` on macOS and
/// `~/.local/share/topebag` on Linux — unless `TOPEBAG_HOME` is set, which
/// is handy for demos and tests.
pub fn data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(home);
    }
    let name = if cfg!(target_os = "linux") { "topebag" } else { "Topebag" };
    directories::BaseDirs::new()
        .map(|d| d.data_local_dir().join(name))
        .unwrap_or_else(|| PathBuf::from("topebag-data"))
}

/// Path of the settings file inside `data_dir`.
pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

/// One completed print job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrintRecord {
    pub at: DateTime<Local>,
    pub printer: String,
    pub pages: usize,
    pub copies: u16,
}

/// What kind of document an entry holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    /// A web page converted and typeset by Topebag.
    #[default]
    Article,
    /// A PDF downloaded as-is (printed page by page).
    Pdf,
}

/// Metadata for one saved document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Folder name; stable identifier.
    pub id: String,
    /// The URL the user pasted.
    pub url: String,
    /// The URL after redirects (used to resolve relative links).
    pub final_url: String,
    pub title: String,
    #[serde(default)]
    pub byline: Option<String>,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub excerpt: Option<String>,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    pub saved_at: DateTime<Local>,
    #[serde(default)]
    pub word_count: usize,
    /// Number of pages in the current PDF.
    #[serde(default)]
    pub pages: usize,
    /// `true` if Readability failed and the whole page body was used.
    #[serde(default)]
    pub used_fallback: bool,
    /// Images downloaded for this entry (for offline re-typesetting).
    #[serde(default)]
    pub images: Vec<StoredImage>,
    #[serde(default)]
    pub prints: Vec<PrintRecord>,
    /// Non-fatal problems worth surfacing (skipped images, font fallbacks…).
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub kind: EntryKind,
    /// User-assigned tags, lower-case, sorted.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The page was loaded with the headless browser.
    #[serde(default)]
    pub via_browser: bool,
    /// Laid out as side-by-side spreads (each PDF page is one sheet).
    #[serde(default)]
    pub spread: bool,
    /// How much spacing was tightened to save sheets (0 = none, 1 = most).
    #[serde(default)]
    pub density: f64,
}

impl Entry {
    /// Add a tag (normalised to lower case). Returns `false` if it was
    /// empty or already present.
    pub fn add_tag(&mut self, tag: &str) -> bool {
        let tag = normalize_tag(tag);
        if tag.is_empty() || self.tags.contains(&tag) {
            return false;
        }
        self.tags.push(tag);
        self.tags.sort();
        true
    }

    /// Remove a tag. Returns `true` if it was present.
    pub fn remove_tag(&mut self, tag: &str) -> bool {
        let tag = normalize_tag(tag);
        let before = self.tags.len();
        self.tags.retain(|t| t != &tag);
        before != self.tags.len()
    }

    /// Number of times this entry has been printed.
    pub fn print_count(&self) -> usize {
        self.prints.len()
    }

    /// The host name of the original URL, without `www.`.
    pub fn domain(&self) -> String {
        url::Url::parse(&self.url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()))
            .unwrap_or_default()
    }
}

/// Handle to the library folder.
#[derive(Debug, Clone)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    /// Open (and create if necessary) the library inside `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Library> {
        let root = data_dir.join("library");
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Library { root })
    }

    /// The library folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The Topebag data directory that contains the library.
    pub fn data_dir(&self) -> &Path {
        self.root.parent().unwrap_or(&self.root)
    }

    /// Plain text of an entry, for full-text search. Uses `article.txt`,
    /// creating it from `article.html` for entries saved before it existed.
    pub fn text(&self, id: &str) -> Option<String> {
        let dir = self.dir(id).ok()?;
        if let Ok(text) = fs::read_to_string(dir.join("article.txt")) {
            return Some(text);
        }
        let html = fs::read_to_string(dir.join("article.html")).ok()?;
        let text = crate::convert::html_to_plain_text(&html);
        let _ = fs::write(dir.join("article.txt"), &text);
        Some(text)
    }

    /// All entries, newest first. Folders with a missing or corrupt
    /// `entry.json` are skipped (and logged), never deleted.
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.root)? {
            let item = item?;
            if !item.file_type()?.is_dir() {
                continue;
            }
            let meta = item.path().join("entry.json");
            if !meta.exists() {
                continue;
            }
            match fs::read_to_string(&meta).map_err(anyhow::Error::from).and_then(|t| Ok(serde_json::from_str::<Entry>(t.trim_start_matches('\u{feff}'))?)) {
                Ok(entry) => entries.push(entry),
                Err(err) => log::warn!("ignoring unreadable entry {}: {err:#}", meta.display()),
            }
        }
        entries.sort_by(|a, b| b.saved_at.cmp(&a.saved_at));
        Ok(entries)
    }

    /// Load a single entry.
    pub fn entry(&self, id: &str) -> Result<Entry> {
        let path = self.dir(id)?.join("entry.json");
        let text = fs::read_to_string(&path).with_context(|| format!("no saved document with id `{id}`"))?;
        Ok(serde_json::from_str(text.trim_start_matches('\u{feff}'))?)
    }

    /// Folder for entry `id`. Rejects ids that could escape the library.
    pub fn dir(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty() || id.contains(['/', '\\', ':']) || id.starts_with('.') {
            bail!("invalid entry id `{id}`");
        }
        Ok(self.root.join(id))
    }

    /// Path of the entry's PDF.
    pub fn pdf_path(&self, id: &str) -> Result<PathBuf> {
        Ok(self.dir(id)?.join("document.pdf"))
    }

    /// Reserve a new, unique entry folder named after `title`.
    pub fn create(&self, title: &str) -> Result<(String, PathBuf)> {
        let stamp = Local::now().format("%Y-%m-%d_%H%M%S");
        let base = format!("{stamp}_{}", slug(title));
        for n in 1.. {
            let id = if n == 1 { base.clone() } else { format!("{base}-{n}") };
            let dir = self.root.join(&id);
            match fs::create_dir(&dir) {
                Ok(()) => return Ok((id, dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
            }
        }
        unreachable!()
    }

    /// Write `entry.json` (atomically).
    pub fn save(&self, entry: &Entry) -> Result<()> {
        let dir = self.dir(&entry.id)?;
        fs::create_dir_all(&dir)?;
        let tmp = dir.join("entry.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(entry)?)?;
        fs::rename(&tmp, dir.join("entry.json"))?;
        Ok(())
    }

    /// Delete an entry and all its files.
    pub fn delete(&self, id: &str) -> Result<()> {
        let dir = self.dir(id)?;
        if dir.exists() {
            fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", dir.display()))?;
        }
        Ok(())
    }

    /// Remove a half-created entry folder after a failed import.
    pub fn discard(&self, id: &str) {
        if let Ok(dir) = self.dir(id) {
            if !dir.join("entry.json").exists() {
                let _ = fs::remove_dir_all(dir);
            }
        }
    }
}

/// Tags are trimmed, lower-cased, and use single spaces.
pub fn normalize_tag(tag: &str) -> String {
    tag.trim().trim_start_matches('#').split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A short, filesystem-safe slug: lowercase ASCII letters, digits, dashes.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "untitled".to_string() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, minutes_ago: i64) -> Entry {
        Entry {
            id: id.into(),
            url: "https://www.example.com/a".into(),
            final_url: "https://www.example.com/a".into(),
            title: "T".into(),
            byline: None,
            site_name: None,
            excerpt: None,
            published: None,
            lang: None,
            saved_at: Local::now() - chrono::Duration::minutes(minutes_ago),
            word_count: 0,
            pages: 1,
            used_fallback: false,
            images: vec![],
            prints: vec![],
            warnings: vec![],
            ..Entry::default()
        }
    }

    #[test]
    fn create_save_list_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();

        let (id_a, _) = lib.create("Hello, World!").unwrap();
        let (id_b, _) = lib.create("Hello, World!").unwrap();
        assert_ne!(id_a, id_b, "ids are unique even for identical titles");
        assert!(id_a.ends_with("hello-world"));

        lib.save(&entry(&id_a, 10)).unwrap();
        lib.save(&entry(&id_b, 1)).unwrap();
        let list = lib.entries().unwrap();
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), [&id_b, &id_a]);
        assert_eq!(list[0].domain(), "example.com");

        lib.delete(&id_a).unwrap();
        assert_eq!(lib.entries().unwrap().len(), 1);
    }

    #[test]
    fn rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        assert!(lib.dir("../etc").is_err());
        assert!(lib.dir("a/b").is_err());
        assert!(lib.delete("..").is_err());
    }

    #[test]
    fn tags_are_normalised() {
        let mut e = entry("x", 0);
        assert!(e.add_tag("  #Long  Reads "));
        assert!(!e.add_tag("long reads"), "duplicates are ignored");
        assert!(e.add_tag("ai"));
        assert_eq!(e.tags, ["ai", "long reads"]);
        assert!(e.remove_tag("LONG READS"));
        assert_eq!(e.tags, ["ai"]);
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("  Ünïcode & Friends: Part 2 "), "n-code-friends-part-2");
        assert_eq!(slug("???"), "untitled");
    }
}
