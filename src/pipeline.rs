//! The end-to-end workflow, shared by the GUI and the CLI.
//!
//! * [`add_url`] — fetch → extract → draft (blocks) → images → convert →
//!   typeset → save PDF → (only if asked) print. This is what happens when
//!   you paste a link; the result is a draft you can edit, format and preview.
//! * [`load_draft`] / [`rebuild`] — the editor's loop: load a document's
//!   draft, and rebuild its PDF after edits or format changes.
//! * [`print_entry`] — print a saved document.
//! * [`retypeset`] — rebuild a saved document from its draft (used by the
//!   CLI), downloading any images that were missing.
//! * [`render_previews`] — rasterise pages for the on-screen preview.
//!
//! Progress is reported through a [`Stage`] callback so the UI can show
//! what is happening.

use std::fmt;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;
use url::Url;

use crate::browser;
use crate::convert::{self, ConvertOptions};
use crate::document::{Draft, Meta};
use crate::extract;
use crate::fetch;
use crate::images::{Downloader, GrayImages, ImageResolver, NoImages, RefillImages};
use crate::library::{Entry, EntryKind, Library, PrintRecord};
use crate::print::{self, PrintRequest};
use crate::settings::{BrowserMode, Format, Settings};
use crate::typeset::{self, Compiled, DocMeta, RasterPage};

/// A step in the pipeline, for progress reporting.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Fetching,
    /// Loading the page in the headless browser.
    Browser,
    Extracting,
    /// Downloading images: (attempted, stored).
    Images(usize, usize),
    Typesetting,
    Saving,
    Printing(String),
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stage::Fetching => write!(f, "Downloading page…"),
            Stage::Browser => write!(f, "Loading the page in the browser…"),
            Stage::Extracting => write!(f, "Finding the article…"),
            Stage::Images(0, _) => write!(f, "Fetching images…"),
            Stage::Images(tried, stored) => write!(f, "Fetching images… ({stored} of {tried})"),
            Stage::Typesetting => write!(f, "Typesetting…"),
            Stage::Saving => write!(f, "Saving PDF…"),
            Stage::Printing(p) => write!(f, "Sending to {p}…"),
        }
    }
}

/// Where a print job should go.
#[derive(Debug, Clone, Default)]
pub struct PrintTarget {
    /// Overrides the printer from settings.
    pub printer: Option<String>,
    /// Spool to a file instead (Windows: usually with "Microsoft Print to PDF").
    pub output_file: Option<std::path::PathBuf>,
}

/// Result of [`add_url`].
pub struct AddOutcome {
    pub entry: Entry,
    /// `Some(Ok)` if printed, `Some(Err)` if printing failed (the document is
    /// still saved), `None` if printing was not requested.
    pub print: Option<Result<PrintRecord>>,
}

/// Fetch a URL, turn it into a PDF in the library, and print it if a
/// print target is given. Web pages are converted and typeset; links that
/// are already PDFs are stored as-is.
pub fn add_url(
    library: &Library,
    settings: &Settings,
    url: &Url,
    print: Option<&PrintTarget>,
    progress: &mut (dyn FnMut(Stage) + Send),
) -> Result<AddOutcome> {
    match acquire(library, settings, url, progress)? {
        Acquired::Pdf(pdf) => add_pdf(library, settings, url, pdf, print, progress),
        Acquired::Article { final_url, article, via_browser, notes } => {
            add_article(library, settings, url, &final_url, article, via_browser, notes, print, progress)
        }
    }
}

/// What a link turned out to contain.
enum Acquired {
    Pdf(fetch::PdfFile),
    Article { final_url: Url, article: extract::Article, via_browser: bool, notes: Vec<String> },
}

/// Little text usually means a JavaScript-built page or a paywall.
fn looks_thin(article: &extract::Article) -> bool {
    article.used_fallback || article.word_count < 200
}

/// Download `url`, using the headless browser when settings ask for it or
/// when a plain download comes back (nearly) empty.
fn acquire(
    library: &Library,
    settings: &Settings,
    url: &Url,
    progress: &mut (dyn FnMut(Stage) + Send),
) -> Result<Acquired> {
    let browser_ok = settings.browser_mode != BrowserMode::Never && browser::find_browser().is_some();
    let profile = browser::profile_dir(library.data_dir());
    let via_browser = |progress: &mut (dyn FnMut(Stage) + Send), target: &Url| -> Result<extract::Article> {
        progress(Stage::Browser);
        let html = browser::dump_dom(target, &profile, browser::DEFAULT_TIMEOUT)?;
        progress(Stage::Extracting);
        extract::extract(&html, target)
    };

    let is_pdf_path = url.path().to_ascii_lowercase().ends_with(".pdf");
    let wants_browser = settings.browser_mode == BrowserMode::Always || browser::matches_site(url, &settings.browser_sites);
    if browser_ok && wants_browser && !is_pdf_path {
        let article = via_browser(progress, url)?;
        return Ok(Acquired::Article { final_url: url.clone(), article, via_browser: true, notes: Vec::new() });
    }

    progress(Stage::Fetching);
    let page = match fetch::fetch(url) {
        Ok(fetch::Fetched::Pdf(pdf)) => return Ok(Acquired::Pdf(pdf)),
        Ok(fetch::Fetched::Html(page)) => page,
        // Many sites refuse non-browser clients outright; a real browser
        // usually gets through.
        Err(err) if browser_ok && is_refusal(&err) => {
            let article = via_browser(progress, url).map_err(|browser_err| {
                anyhow::anyhow!("{err:#} (and the browser could not load it either: {browser_err:#})")
            })?;
            return Ok(Acquired::Article { final_url: url.clone(), article, via_browser: true, notes: Vec::new() });
        }
        Err(err) => return Err(err),
    };

    progress(Stage::Extracting);
    let article = extract::extract(&page.html, &page.final_url)?;
    if !(browser_ok && settings.browser_mode == BrowserMode::Auto && looks_thin(&article)) {
        return Ok(Acquired::Article { final_url: page.final_url, article, via_browser: false, notes: Vec::new() });
    }

    // Thin result: try again with the browser and keep whichever is better.
    match via_browser(progress, &page.final_url) {
        Ok(rendered) if rendered.word_count > article.word_count + 50 || (article.used_fallback && !rendered.used_fallback) => {
            Ok(Acquired::Article { final_url: page.final_url, article: rendered, via_browser: true, notes: Vec::new() })
        }
        Ok(_) => Ok(Acquired::Article { final_url: page.final_url, article, via_browser: false, notes: Vec::new() }),
        Err(err) => {
            log::info!("browser fallback failed: {err:#}");
            let note = format!("Loading the page in the browser did not help: {err:#}");
            Ok(Acquired::Article { final_url: page.final_url, article, via_browser: false, notes: vec![note] })
        }
    }
}

/// HTTP answers that mean "not for robots" rather than "doesn't exist".
fn is_refusal(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    ["401 ", "403 ", "429 ", "451 "].iter().any(|code| msg.contains(code))
}

#[allow(clippy::too_many_arguments)]
fn add_article(
    library: &Library,
    settings: &Settings,
    url: &Url,
    final_url: &Url,
    article: extract::Article,
    via_browser: bool,
    notes: Vec<String>,
    print: Option<&PrintTarget>,
    progress: &mut (dyn FnMut(Stage) + Send),
) -> Result<AddOutcome> {
    let (id, dir) = library.create(&article.title)?;
    let result = (|| -> Result<AddOutcome> {
        fs::write(dir.join("article.html"), &article.content_html)?;
        fs::write(dir.join("article.txt"), convert::html_to_plain_text(&article.content_html))?;

        let mut entry = Entry {
            id: id.clone(),
            url: url.to_string(),
            final_url: final_url.to_string(),
            title: article.title.clone(),
            byline: article.byline.clone(),
            site_name: article.site_name.clone(),
            excerpt: article.excerpt.clone(),
            published: article.published.clone(),
            lang: article.lang.clone(),
            saved_at: Local::now(),
            word_count: article.word_count,
            used_fallback: article.used_fallback,
            via_browser,
            warnings: notes,
            ..Entry::default()
        };
        if article.used_fallback {
            entry.warnings.push("No article was detected on this page, so the whole page body was used.".into());
        }

        let meta = Meta {
            title: article.title.clone(),
            byline: article.byline.clone(),
            published: article.published.clone(),
        };
        let draft = Draft::from_article(&article.content_html, meta, settings.format.clone());

        // Images are downloaded lazily while the HTML is converted, then the
        // document is typeset; both report progress through one callback.
        let compiled = if draft.format.include_images {
            progress(Stage::Images(0, 0));
            let shared = std::cell::RefCell::new(&mut *progress);
            let mut downloader = Downloader::new(dir.join("img"), Some(final_url.clone()))
                .on_progress(|tried, stored| (shared.borrow_mut())(Stage::Images(tried, stored)));
            let compiled =
                typeset_draft(&dir, &mut entry, &draft, &mut downloader, &mut |stage| (shared.borrow_mut())(stage))?;
            if !downloader.skipped.is_empty() {
                entry.warnings.push(format!("{} image(s) could not be included.", downloader.skipped.len()));
            }
            entry.images = std::mem::take(&mut downloader.stored);
            entry.rejected_images = std::mem::take(&mut downloader.rejected);
            compiled
        } else {
            typeset_draft(&dir, &mut entry, &draft, &mut NoImages, &mut |stage| progress(stage))?
        };
        // Font fallbacks are expected (not every system has every font), so
        // they are logged rather than shown to the user.
        for warning in &compiled.warnings {
            log::info!("typst: {warning}");
        }
        entry
            .warnings
            .extend(compiled.warnings.iter().filter(|w| !w.contains("unknown font family")).take(5).cloned());

        progress(Stage::Saving);
        library.save(&entry)?;

        let print = print.map(|target| print_new_entry(library, settings, &mut entry, &compiled, target, progress));
        Ok(AddOutcome { entry, print })
    })();

    if result.is_err() {
        library.discard(&id);
    }
    result
}

/// Store a downloaded PDF as an entry. The original file is kept untouched
/// as `document.pdf`; a small Typst wrapper places its pages for previewing
/// and printing.
fn add_pdf(
    library: &Library,
    settings: &Settings,
    url: &Url,
    pdf: fetch::PdfFile,
    print: Option<&PrintTarget>,
    progress: &mut (dyn FnMut(Stage) + Send),
) -> Result<AddOutcome> {
    progress(Stage::Typesetting);
    let info = typeset::pdf_info(&pdf.bytes)?;
    let host = pdf.final_url.host_str().map(|h| h.trim_start_matches("www.").to_string());
    let title = info
        .title
        .clone()
        .or_else(|| pdf.filename.as_deref().map(title_from_filename))
        .filter(|t| !t.is_empty())
        .or_else(|| host.clone())
        .unwrap_or_else(|| "PDF document".into());

    let (id, dir) = library.create(&title)?;
    let result = (|| -> Result<AddOutcome> {
        fs::write(dir.join("document.pdf"), &pdf.bytes)?;
        let source = typeset::pdf_wrapper_source(info.pages);
        fs::write(dir.join("main.typ"), &source)?;
        let compiled = typeset::compile(&source, &dir)?;

        let mut entry = Entry {
            id: id.clone(),
            url: url.to_string(),
            final_url: pdf.final_url.to_string(),
            title: title.clone(),
            byline: info.author.clone(),
            site_name: host.clone(),
            saved_at: Local::now(),
            pages: compiled.page_count(),
            kind: EntryKind::Pdf,
            ..Entry::default()
        };
        progress(Stage::Saving);
        library.save(&entry)?;

        let print = print.map(|target| print_new_entry(library, settings, &mut entry, &compiled, target, progress));
        Ok(AddOutcome { entry, print })
    })();

    if result.is_err() {
        library.discard(&id);
    }
    result
}

/// `annual-report_2026.pdf` -> `Annual report 2026`.
fn title_from_filename(name: &str) -> String {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let decoded = url::form_urlencoded::parse(format!("x={stem}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| stem.to_string());
    let words = decoded.replace(['-', '_', '+'], " ");
    let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn print_new_entry(
    library: &Library,
    settings: &Settings,
    entry: &mut Entry,
    compiled: &Compiled,
    target: &PrintTarget,
    progress: &mut (dyn FnMut(Stage) + Send),
) -> Result<PrintRecord> {
    let printer_name = target
        .printer
        .clone()
        .or_else(|| settings.printer.clone())
        .or_else(print::default_printer)
        .unwrap_or_else(|| "printer".into());
    progress(Stage::Printing(printer_name));
    let record = send_to_printer(library, entry, compiled, settings, target)?;
    entry.prints.push(record.clone());
    if let Err(err) = library.save(entry) {
        log::warn!("could not record print job: {err:#}");
    }
    Ok(record)
}

/// Print a saved document.
pub fn print_entry(library: &Library, id: &str, settings: &Settings, target: &PrintTarget) -> Result<PrintRecord> {
    let mut entry = library.entry(id)?;
    let compiled = typeset::compile_dir(&library.dir(id)?)?;
    let record = send_to_printer(library, &entry, &compiled, settings, target)?;
    entry.prints.push(record.clone());
    library.save(&entry)?;
    Ok(record)
}

/// Load a document's draft for editing.
///
/// Returns `None` for PDF entries (they print as they are). Entries saved
/// before drafts existed get one made from their stored article HTML, with
/// `defaults` as formatting (keeping their side-by-side layout if they had
/// one). Nothing is written until the draft is rebuilt.
pub fn load_draft(library: &Library, id: &str, defaults: &Format) -> Result<Option<Draft>> {
    let entry = library.entry(id)?;
    if entry.kind == EntryKind::Pdf {
        return Ok(None);
    }
    let path = library.draft_path(id)?;
    if path.exists() {
        return Draft::load(&path).map(Some);
    }
    let html = fs::read_to_string(library.dir(id)?.join("article.html")).context("this entry has no stored article HTML")?;
    let meta = Meta { title: entry.title.clone(), byline: entry.byline.clone(), published: entry.published.clone() };
    let format = Format { side_by_side: entry.spread || defaults.side_by_side, ..defaults.clone() };
    Ok(Some(Draft::from_article(&html, meta, format)))
}

/// The article HTML a draft started from (for "Restore original").
pub fn original_html(library: &Library, id: &str) -> Result<String> {
    fs::read_to_string(library.dir(id)?.join("article.html")).context("this entry has no stored article HTML")
}

/// Result of [`rebuild`].
pub struct Rebuilt {
    pub entry: Entry,
    pub compiled: Compiled,
}

/// Save `draft` and rebuild the document's PDF from it.
///
/// With `refill_images`, images that aren't stored yet (for example ones an
/// older version failed to fetch) are downloaded; otherwise this works
/// entirely offline, which keeps the editor's live preview fast.
pub fn rebuild(library: &Library, id: &str, draft: &Draft, refill_images: bool) -> Result<Rebuilt> {
    let mut entry = library.entry(id)?;
    let dir = library.dir(id)?;
    if entry.kind == EntryKind::Pdf {
        let compiled = typeset::compile_dir(&dir)?;
        return Ok(Rebuilt { entry, compiled });
    }
    let base = Url::parse(&entry.final_url).or_else(|_| Url::parse(&entry.url))?;
    let compiled = if draft.format.include_images {
        let referer = refill_images.then(|| base.clone());
        let mut resolver = RefillImages::new(&entry.images, &entry.rejected_images, dir.join("img"), referer);
        let compiled = if refill_images {
            typeset_draft(&dir, &mut entry, draft, &mut resolver, &mut |_| {})?
        } else {
            let mut offline = OfflineImages(&mut resolver);
            typeset_draft(&dir, &mut entry, draft, &mut offline, &mut |_| {})?
        };
        let (images, rejected, still_missing) = resolver.finish();
        entry.images = images;
        entry.rejected_images = rejected;
        if refill_images {
            entry.warnings.retain(|w| !w.contains("image(s) could not be included"));
            if still_missing > 0 {
                entry.warnings.push(format!("{still_missing} image(s) could not be included."));
            }
        }
        compiled
    } else {
        typeset_draft(&dir, &mut entry, draft, &mut NoImages, &mut |_| {})?
    };
    library.save(&entry)?;
    Ok(Rebuilt { entry, compiled })
}

/// Only stored images; never touches the network.
struct OfflineImages<'a, 'b>(&'a mut RefillImages<'b>);

impl ImageResolver for OfflineImages<'_, '_> {
    fn resolve(&mut self, url: &Url) -> Option<crate::images::StoredImage> {
        self.0.stored(url)
    }
}

/// Rebuild a saved document from its draft (making one with `settings`'
/// formatting if it has none), downloading any missing images.
pub fn retypeset(library: &Library, id: &str, settings: &Settings) -> Result<Entry> {
    match load_draft(library, id, &settings.format)? {
        Some(draft) => Ok(rebuild(library, id, &draft, true)?.entry),
        None => library.entry(id), // PDFs print as they are
    }
}

/// Lay out `draft` into `dir`: sync the masthead into the entry, convert the
/// included blocks, typeset (fitting spreads), write `main.typ`,
/// `document.pdf` and `document.json`.
fn typeset_draft(
    dir: &Path,
    entry: &mut Entry,
    draft: &Draft,
    images: &mut dyn ImageResolver,
    progress: &mut dyn FnMut(Stage),
) -> Result<Compiled> {
    entry.title = if draft.meta.title.trim().is_empty() { "Untitled".into() } else { draft.meta.title.trim().to_string() };
    entry.byline = draft.meta.byline.clone().filter(|b| !b.trim().is_empty());
    entry.published = draft.meta.published.clone().filter(|p| !p.trim().is_empty());

    let base = Url::parse(&entry.final_url).or_else(|_| Url::parse(&entry.url))?;
    let html = draft.html();
    let format = &draft.format;
    let body = if format.include_images && format.grayscale_images {
        let mut gray = GrayImages::new(images, dir.join("img"));
        convert_body(entry, format, &base, &html, &mut gray)
    } else {
        convert_body(entry, format, &base, &html, images)
    };

    progress(Stage::Typesetting);
    let (compiled, density) = build(dir, entry, format, &body, &html)?;
    entry.spread = format.side_by_side;
    entry.density = density;
    entry.pages = compiled.page_count();
    write_pdf(dir, &compiled)?;
    fs::write(dir.join("article.txt"), convert::html_to_plain_text(&html))?;
    draft.save(&dir.join("document.json"))?;
    Ok(compiled)
}

/// Rasterise every page of a saved document for previewing.
pub fn render_previews(library: &Library, id: &str, dpi: f32) -> Result<Vec<RasterPage>> {
    let compiled = typeset::compile_dir(&library.dir(id)?)?;
    Ok((0..compiled.page_count()).filter_map(|i| typeset::render_page(&compiled.document, i, dpi)).collect())
}

/// Delete a saved document.
pub fn delete_entry(library: &Library, id: &str) -> Result<()> {
    library.delete(id)
}

fn convert_body(
    entry: &Entry,
    format: &Format,
    base: &Url,
    html: &str,
    images: &mut dyn ImageResolver,
) -> String {
    let options = ConvertOptions {
        link_style: format.link_style,
        include_images: format.include_images,
        title: Some(entry.title.clone()),
        strip_chrome: entry.used_fallback,
    };
    convert::html_to_typst(html, base, &options, images)
}

/// Write `main.typ` (and the QR code) and compile, fitting spacing to the
/// fewest sheets when side-by-side spreads ask for it. Returns the layout
/// and the spacing density used. If the rich markup fails to compile —
/// which should not happen, but the web is endlessly creative — fall back to
/// a plain-text rendering so the user still gets a document.
fn build(dir: &Path, entry: &Entry, format: &Format, body: &str, html: &str) -> Result<(Compiled, f64)> {
    let has_qr = format.qr_code && entry.url.len() < 1500 && {
        match typeset::qr_svg(&entry.url) {
            Ok(svg) => fs::write(dir.join("qr.svg"), svg).is_ok(),
            Err(err) => {
                log::warn!("QR code failed: {err:#}");
                false
            }
        }
    };
    let meta = DocMeta {
        title: entry.title.clone(),
        site_name: entry.site_name.clone(),
        byline: entry.byline.clone(),
        published: entry.published.clone(),
        url: Some(entry.url.clone()),
        lang: entry.lang.clone(),
        word_count: entry.word_count,
        saved: Some(entry.saved_at.format("%-d %B %Y, %H:%M").to_string()),
        has_qr,
    };

    match typeset::compile_fitted(&meta, format, body, dir) {
        Ok(fitted) => {
            fs::write(dir.join("main.typ"), &fitted.source)?;
            Ok((fitted.compiled, fitted.density))
        }
        Err(err) => {
            log::warn!("rich typesetting failed, falling back to plain text: {err:#}");
            fs::write(dir.join("main.rich-failed.typ"), typeset::main_source(&meta, format, body, 0.0))?;
            let plain = typeset::main_source(&meta, format, &convert::html_to_plain_typst(html), 0.0);
            fs::write(dir.join("main.typ"), &plain)?;
            let mut compiled = typeset::compile(&plain, dir).context("typesetting failed even in plain-text mode")?;
            compiled
                .warnings
                .insert(0, "Formatting was simplified because the page's structure could not be typeset.".into());
            Ok((compiled, 0.0))
        }
    }
}

fn write_pdf(dir: &Path, compiled: &Compiled) -> Result<()> {
    let bytes = typeset::pdf(&compiled.document)?;
    let tmp = dir.join("document.pdf.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, dir.join("document.pdf"))?;
    Ok(())
}

fn send_to_printer(
    library: &Library,
    entry: &Entry,
    compiled: &Compiled,
    settings: &Settings,
    target: &PrintTarget,
) -> Result<PrintRecord> {
    let printer = target.printer.as_deref().or(settings.printer.as_deref());
    let pdf_path = library.pdf_path(&entry.id)?;
    let outcome = print::print(&PrintRequest {
        document: &compiled.document,
        pdf_path: &pdf_path,
        job_name: &format!("{} — Topebag", entry.title),
        printer,
        copies: settings.copies,
        dpi: settings.print_quality.dpi(),
        output_file: target.output_file.as_deref(),
    })?;
    Ok(PrintRecord {
        at: Local::now(),
        printer: outcome.printer,
        pages: outcome.sheets,
        copies: settings.copies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_from_filenames() {
        assert_eq!(title_from_filename("annual-report_2026.pdf"), "Annual report 2026");
        assert_eq!(title_from_filename("Q3%20results.PDF"), "Q3 results");
        assert_eq!(title_from_filename(".pdf"), "");
    }
}
