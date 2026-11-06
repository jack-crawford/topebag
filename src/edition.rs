//! Editions: several articles printed together, newspaper style.
//!
//! Printing articles one by one wastes the blank space at the end of each.
//! An edition flows its stories one after another through newspaper columns
//! with no page breaks between them, so the only empty space is at the very
//! end. Page 1 opens with a nameplate ("The Topebag", the date, the number
//! of stories, reading time) and an "In this edition" list with page numbers.
//!
//! An edition is a library entry of kind [`EntryKind::Edition`]. Its folder
//! holds `edition.json` (the [`Edition`]: nameplate title, ordered stories,
//! format), the generated `main.typ`, copies of the images and QR codes it
//! uses (so it keeps printing even if an article is deleted later), and
//! `document.pdf`.
//!
//! Stories are built from each article's **current draft**, so edits made to
//! an article (dropped blocks, rewritten text, a new title) carry into the
//! edition the next time it's rebuilt. PDFs can't flow into columns and are
//! left out with a note.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Local;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::convert::{self, ConvertOptions, string_literal};
use crate::document::{BlockKind, Draft};
use crate::images::{GrayImages, ImageResolver, NoImages, StoredImage};
use crate::library::{Entry, EntryKind, Library};
use crate::pipeline::{self, Rebuilt};
use crate::settings::{Format, Settings, Typeface};
use crate::typeset;

fn yes() -> bool {
    true
}

/// One story slot in an edition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Library id of the article.
    pub id: String,
    #[serde(default = "yes")]
    pub included: bool,
}

/// An edition's settings and story list (`edition.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edition {
    /// The nameplate, e.g. "The Topebag".
    pub title: String,
    /// Stories in print order.
    pub members: Vec<Member>,
    /// Show an "In this edition" list with page numbers on the front page.
    #[serde(default = "yes")]
    pub contents: bool,
    #[serde(default)]
    pub format: Format,
}

impl Edition {
    /// A new edition of `ids`, formatted as a newspaper from `defaults`:
    /// three columns, 9.5 pt text (or smaller if that's the default), one page
    /// per sheet, spacing fitted to the fewest pages.
    pub fn new(ids: &[String], defaults: &Format) -> Edition {
        Edition {
            title: "The Topebag".into(),
            members: ids.iter().map(|id| Member { id: id.clone(), included: true }).collect(),
            contents: true,
            format: Format {
                columns: 3,
                font_size: defaults.font_size.min(9.5),
                side_by_side: false,
                fit_to_sheets: true,
                ..defaults.clone()
            },
        }
    }

    /// Move story `index` one place up or down. Returns its new index.
    pub fn move_member(&mut self, index: usize, up: bool) -> usize {
        let target = if up { index.checked_sub(1) } else { Some(index + 1).filter(|&i| i < self.members.len()) };
        match target {
            Some(t) if index < self.members.len() => {
                self.members.swap(index, t);
                t
            }
            _ => index,
        }
    }

    pub fn load(path: &Path) -> Result<Edition> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_str(text.trim_start_matches('\u{feff}'))?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// A new edition of `ids` using the default formatting from `settings`.
pub fn new_from_settings(ids: &[String], settings: &Settings) -> Edition {
    Edition::new(ids, &settings.format)
}

/// Path of an edition's `edition.json`.
pub fn edition_path(library: &Library, id: &str) -> Result<PathBuf> {
    Ok(library.dir(id)?.join("edition.json"))
}

/// Load an edition entry's settings.
pub fn load(library: &Library, id: &str) -> Result<Edition> {
    Edition::load(&edition_path(library, id)?)
}

/// Were any of the edition's stories edited after it was last laid out?
pub fn is_stale(library: &Library, id: &str, edition: &Edition) -> bool {
    let modified = |path: Result<PathBuf>| path.ok().and_then(|p| fs::metadata(p).ok()).and_then(|m| m.modified().ok());
    let Some(built) = modified(library.pdf_path(id)) else { return false };
    edition
        .members
        .iter()
        .filter(|m| m.included)
        .any(|m| modified(library.draft_path(&m.id)).is_some_and(|t| t > built))
}

/// Can this entry be a story in an edition?
pub fn can_join(entry: &Entry) -> bool {
    entry.kind == EntryKind::Article
}

/// Combine articles into a new edition entry and lay it out. Members that
/// aren't web articles (PDFs, other editions) are dropped. Start from
/// [`Edition::new`] (or [`new_from_settings`]) and adjust it first if needed.
pub fn create(library: &Library, mut edition: Edition) -> Result<Rebuilt> {
    edition.members.retain(|m| library.entry(&m.id).is_ok_and(|e| can_join(&e)));
    if edition.members.is_empty() {
        bail!("only web articles can be combined into an edition (PDFs can't flow into columns)");
    }
    let now = Local::now();
    let (id, _dir) = library.create(&format!("edition {}", now.format("%-d %b %Y")))?;
    let entry = Entry {
        id: id.clone(),
        title: format!("{} · {}", edition.title, now.format("%-d %b %Y")),
        site_name: Some("Topebag edition".into()),
        saved_at: now,
        kind: EntryKind::Edition,
        ..Entry::default()
    };
    let result = library.save(&entry).and_then(|()| rebuild(library, &id, &edition));
    if result.is_err() {
        let _ = library.delete(&id);
    }
    result
}

/// Lay out an edition from its stories' current drafts.
pub fn rebuild(library: &Library, id: &str, edition: &Edition) -> Result<Rebuilt> {
    let mut entry = library.entry(id)?;
    if entry.kind != EntryKind::Edition {
        bail!("“{}” is not an edition", entry.title);
    }
    let dir = library.dir(id)?;
    // Fresh copies of the images and QR codes every time.
    for sub in ["img", "qr"] {
        let _ = fs::remove_dir_all(dir.join(sub));
        fs::create_dir_all(dir.join(sub))?;
    }

    let format = &edition.format;
    let mut stories: Vec<String> = Vec::new();
    let mut titles = Vec::new();
    let mut warnings = Vec::new();
    let mut words_total = 0usize;
    let mut first_lang: Option<String> = None;

    for member in edition.members.iter().filter(|m| m.included) {
        let Ok(article) = library.entry(&member.id) else {
            warnings.push("A story was deleted from the library and was left out.".to_string());
            continue;
        };
        if !can_join(&article) {
            warnings.push(format!("“{}” is a PDF and can't flow into columns, so it was left out.", article.title));
            continue;
        }
        let Some(draft) = pipeline::load_draft(library, &member.id, format)? else { continue };
        let html = without_repeated_byline(&draft).html();
        let base = Url::parse(&article.final_url).or_else(|_| Url::parse(&article.url))?;
        let number = stories.len() + 1;

        let options = ConvertOptions {
            link_style: format.link_style,
            include_images: format.include_images,
            title: Some(draft.meta.title.clone()),
            strip_chrome: article.used_fallback,
            allow_floats: false,
        };
        let mut copier =
            CopyImages::new(&article.images, library.dir(&member.id)?.join("img"), dir.join("img"), format!("{number}-"));
        let body = if !format.include_images {
            convert::html_to_typst(&html, &base, &options, &mut NoImages)
        } else if format.grayscale_images {
            let mut gray = GrayImages::new(&mut copier, dir.join("img"));
            convert::html_to_typst(&html, &base, &options, &mut gray)
        } else {
            convert::html_to_typst(&html, &base, &options, &mut copier)
        };

        let words = convert::html_to_plain_text(&html).split_whitespace().count();
        words_total += words;
        let qr = if format.qr_code && !article.url.is_empty() && article.url.len() < 1500 {
            typeset::qr_svg(&article.url)
                .ok()
                .and_then(|svg| fs::write(dir.join("qr").join(format!("{number}.svg")), svg).ok())
                .map(|()| format!("/qr/{number}.svg"))
        } else {
            None
        };
        if first_lang.is_none() {
            first_lang = article.lang.clone();
        }
        titles.push(draft.meta.title.clone());
        stories.push(story_markup(&draft.meta, &article, words, qr.as_deref(), number == 1, &body));
    }
    if stories.is_empty() {
        bail!("this edition has no stories that can be printed (only web articles can be combined)");
    }

    let dateline = entry.saved_at.format("%A, %-d %B %Y").to_string();
    let minutes = ((words_total as f64) / 230.0).round().max(1.0) as usize;
    let body = stories.join("\n\n");
    let source = |density: f64| {
        edition_source(edition, &dateline, stories.len(), minutes, first_lang.as_deref(), &body, density)
    };
    let fitted = typeset::fit_density(source, &dir, format.fit_to_sheets)?;
    fs::write(dir.join("main.typ"), &fitted.source)?;
    pipeline::write_pdf(&dir, &fitted.compiled)?;

    let title = if edition.title.trim().is_empty() { "The Topebag" } else { edition.title.trim() };
    entry.title = format!("{title} · {}", entry.saved_at.format("%-d %b %Y"));
    // The library marks editions with a badge, so this just counts stories.
    entry.site_name = Some(match stories.len() {
        1 => "1 story".to_string(),
        n => format!("{n} stories"),
    });
    entry.pages = fitted.compiled.page_count();
    entry.density = fitted.density;
    entry.spread = false;
    entry.word_count = words_total;
    entry.warnings = warnings;
    edition.save(&dir.join("edition.json"))?;
    fs::write(dir.join("article.txt"), titles.join("\n"))?;
    library.save(&entry)?;
    Ok(Rebuilt { entry, compiled: fitted.compiled })
}

/// The draft's HTML minus a short byline paragraph near the top ("By Ada
/// Lovelace · 4 May"), which a story's header already shows. Articles keep
/// it; in a newspaper it would just repeat the line above.
fn without_repeated_byline(draft: &Draft) -> Draft {
    let mut draft = draft.clone();
    let byline = draft.meta.byline.as_deref().map(|b| b.trim().to_lowercase()).filter(|b| !b.is_empty());
    for block in draft.blocks.iter_mut().filter(|b| b.included).take(3) {
        if block.kind != BlockKind::Paragraph {
            continue;
        }
        let text = block.preview().to_lowercase();
        let length = text.chars().count();
        // "By Ada Lovelace · 4 May" names the author, or at least reads like
        // a credit line rather than a sentence.
        let names_author = length <= 140 && byline.as_ref().is_some_and(|b| text.contains(b.as_str()));
        let credit_line = length <= 80 && text.starts_with("by ") && !text.ends_with(['.', '!', '?']);
        if names_author || credit_line {
            block.included = false;
            break;
        }
    }
    draft
}

/// One `#tb-story(...)[...]` call.
fn story_markup(
    meta: &crate::document::Meta,
    article: &Entry,
    words: usize,
    qr: Option<&str>,
    first: bool,
    body: &str,
) -> String {
    let opt = |v: Option<&str>| {
        v.map(str::trim).filter(|s| !s.is_empty()).map(string_literal).unwrap_or_else(|| "none".into())
    };
    let (lang, region) = typeset::split_lang(article.lang.as_deref());
    let minutes = if words >= 150 { ((words as f64) / 230.0).round().max(1.0) as usize } else { 0 };
    format!(
        "#tb-story(title: {title}, site: {site}, byline: {byline}, date: {date}, minutes: {minutes}, qr: {qr}, first: {first}, lang: {lang}, region: {region})[\n{body}\n]",
        title = string_literal(meta.title.trim()),
        site = opt(article.site_name.as_deref()),
        byline = opt(meta.byline.as_deref()),
        date = opt(meta.published.as_deref()),
        qr = qr.map(string_literal).unwrap_or_else(|| "none".into()),
        lang = string_literal(&lang),
        region = region.as_deref().map(string_literal).unwrap_or_else(|| "none".into()),
    )
}

/// The complete `main.typ` of an edition.
fn edition_source(
    edition: &Edition,
    dateline: &str,
    stories: usize,
    minutes: usize,
    lang: Option<&str>,
    body: &str,
    density: f64,
) -> String {
    let f = &edition.format;
    let (lang, _) = typeset::split_lang(lang);
    let title = if edition.title.trim().is_empty() { "The Topebag" } else { edition.title.trim() };
    format!(
        r#"// Generated by Topebag {version}: an edition of {stories} stories. Rebuilding the edition regenerates this file.
#import "/topebag.typ": *
#show: tb-edition.with(
  title: {title},
  dateline: {dateline},
  stories: {stories},
  minutes: {minutes},
  contents: {contents},
  lang: {lang},
  paper: {paper},
  font-size: {size}pt,
  typeface: {typeface},
  cols: {cols},
  justify: {justify},
  density: {density:.3},
)

{body}
"#,
        version = env!("CARGO_PKG_VERSION"),
        title = string_literal(title),
        dateline = string_literal(dateline),
        contents = edition.contents,
        lang = string_literal(&lang),
        paper = string_literal(f.paper.typst_name()),
        size = f.font_size,
        typeface = string_literal(match f.typeface {
            Typeface::Serif => "serif",
            Typeface::Sans => "sans",
        }),
        cols = f.columns.clamp(1, 3),
        justify = f.justify,
        density = density.clamp(0.0, 1.0),
    )
}

/// Copies an article's stored images into the edition's folder (prefixed
/// with the story number) as they're used.
struct CopyImages {
    by_url: HashMap<String, StoredImage>,
    source: PathBuf,
    dest: PathBuf,
    prefix: String,
}

impl CopyImages {
    fn new(images: &[StoredImage], source: PathBuf, dest: PathBuf, prefix: String) -> Self {
        Self { by_url: images.iter().map(|i| (i.source_url.clone(), i.clone())).collect(), source, dest, prefix }
    }
}

impl ImageResolver for CopyImages {
    fn resolve(&mut self, url: &Url) -> Option<StoredImage> {
        let stored = self.by_url.get(url.as_str())?;
        let file = format!("{}{}", self.prefix, stored.file);
        let target = self.dest.join(&file);
        if !target.exists() {
            if let Err(err) = fs::copy(self.source.join(&stored.file), &target) {
                log::warn!("could not copy image {} into the edition: {err}", stored.file);
                return None;
            }
        }
        Some(StoredImage { file, ..stored.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn story(n: usize, paragraphs: usize) -> String {
        let para = "Newspapers flow stories from column to column without leaving gaps. ".repeat(8);
        let body = (0..paragraphs).map(|_| para.clone()).collect::<Vec<_>>().join("\n\n");
        format!(
            "#tb-story(title: \"Story {n}\", site: \"Example\", byline: \"Ada\", date: none, minutes: 3, qr: none, first: {}, lang: \"en\", region: none)[\n#heading(level: 2)[Inner heading]\n\n{body}\n]",
            n == 1
        )
    }

    #[test]
    fn stories_flow_without_page_breaks_and_list_their_pages() {
        let dir = tempfile::tempdir().unwrap();
        let edition = Edition::new(&["a".into(), "b".into(), "c".into()], &Format::default());
        let body = (1..=3).map(|n| story(n, 6)).collect::<Vec<_>>().join("\n\n");
        let source = edition_source(&edition, "Thursday, 5 November 2026", 3, 9, Some("en"), &body, 0.0);
        let compiled = typeset::compile(&source, dir.path()).unwrap();
        // Three 6-paragraph stories in three columns share pages: far fewer
        // pages than three separately printed articles would take.
        assert!(compiled.page_count() <= 2, "got {} pages", compiled.page_count());

        // Fitting never makes it longer.
        let fitted = typeset::fit_density(
            |d| edition_source(&edition, "Thursday", 3, 9, None, &body, d),
            dir.path(),
            true,
        )
        .unwrap();
        assert!(fitted.compiled.page_count() <= compiled.page_count());
    }

    #[test]
    fn editions_compile_in_one_and_two_columns_without_contents() {
        let dir = tempfile::tempdir().unwrap();
        for cols in [1, 2] {
            let mut edition = Edition::new(&["a".into()], &Format::default());
            edition.format.columns = cols;
            edition.contents = false;
            let source = edition_source(&edition, "Today", 1, 2, None, &story(1, 2), 0.0);
            assert!(typeset::compile(&source, dir.path()).is_ok(), "{cols} column(s)");
        }
    }

    #[test]
    fn a_repeated_byline_is_left_to_the_story_header() {
        let html = "<p>By Ada Lovelace · 4 May</p><p>The engine weaves algebraic patterns.</p><p>By the way, it works.</p>";
        let meta = crate::document::Meta { title: "Notes".into(), byline: Some("Ada Lovelace".into()), ..Default::default() };
        let mut draft = Draft::from_article(html, meta, Format::default());
        let html = without_repeated_byline(&draft).html();
        assert!(!html.contains("By Ada Lovelace") && html.contains("algebraic") && html.contains("By the way"), "{html}");
        // Long paragraphs that happen to start with "By" stay.
        draft.blocks.remove(0);
        draft.blocks[0].edited = Some(format!("By {}", "and large this is a long opening paragraph. ".repeat(6)));
        let html = without_repeated_byline(&draft).html();
        assert!(html.contains("By and large") && html.contains("By the way"), "{html}");
    }

    #[test]
    fn members_move_and_round_trip() {
        let mut edition = Edition::new(&["a".into(), "b".into()], &Format::default());
        assert_eq!(edition.format.columns, 3);
        assert!(edition.format.font_size <= 9.5);
        assert_eq!(edition.move_member(1, true), 0);
        assert_eq!(edition.members[0].id, "b");
        assert_eq!(edition.move_member(0, true), 0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edition.json");
        edition.save(&path).unwrap();
        assert_eq!(Edition::load(&path).unwrap(), edition);
    }
}
