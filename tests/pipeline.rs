//! End-to-end tests for the URL → PDF → library → print pipeline.
//!
//! A tiny HTTP server on 127.0.0.1 serves a realistic article fixture (with
//! navigation, ads, a cookie banner, an image, a table and a code block), so
//! these tests need no internet access.

use std::io::{BufRead, BufReader, Cursor, Write};
use std::net::TcpListener;
#[cfg(windows)]
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use topebag::instapaper::{self, QueueState};
use topebag::library::{EntryKind, Library};
#[cfg(windows)]
use topebag::pipeline::PrintTarget;
use topebag::pipeline::{self, Stage};
use topebag::settings::{BrowserMode, Format, Paper, Settings};
use url::Url;

const ARTICLE: &str = include_str!("fixtures/article.html");

/// A page whose article only exists after JavaScript runs.
const JS_PAGE: &str = r#"<!doctype html><html lang="en"><head><title>Rendered by JavaScript</title></head>
<body><div id="app">Loading…</div>
<script>
  var sentence = "This paragraph was written into the page by a script, so a plain download sees nothing but a loading message. ";
  var html = "<article><h1>Rendered by JavaScript</h1>";
  for (var i = 0; i < 8; i++) { html += "<p>" + sentence.repeat(4) + "</p>"; }
  document.getElementById("app").innerHTML = html + "</article>";
</script></body></html>"#;

/// Serve fixtures on a random local port; returns the base URL.
///
/// * `/2026/why-print` — the article fixture
/// * `/images/press.png` — a generated image
/// * `/paper.pdf` — a generated two-page PDF
/// * `/js-article` — content built by JavaScript
/// * `/bot-wall` — 403 for Topebag's HTTP client, the article for browsers
/// * `/feed.xml` — an Instapaper-style RSS feed
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");

    let png = {
        let img = image::RgbImage::from_fn(1200, 600, |x, y| image::Rgb([(x / 5) as u8, (y / 3) as u8, 140]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    };
    let pdf = {
        let dir = tempfile::tempdir().unwrap();
        let source = "#set document(title: \"Quarterly Paper\", author: \"Ada\")\n#set page(paper: \"a5\")\n= First page\nHello.\n#pagebreak()\n= Second page\nBye.";
        let compiled = topebag::typeset::compile(source, dir.path()).unwrap();
        topebag::typeset::pdf(&compiled.document).unwrap()
    };
    let feed = format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Instapaper: Unread</title>
        <item><guid>11</guid><title>Why We Still Print Things</title><link>{base}/2026/why-print</link></item>
        <item><guid>12</guid><title>Quarterly Paper</title><link>{base}/paper.pdf</link></item>
        </channel></rss>"#
    );
    let assets = Arc::new((png, pdf, feed));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let assets = Arc::clone(&assets);
            // One thread per connection: browsers open speculative
            // connections that may never send a request.
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                let mut user_agent = String::new();
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok() && line.trim() != "" {
                    if line.to_ascii_lowercase().starts_with("user-agent:") {
                        user_agent = line.clone();
                    }
                    line.clear();
                }
                let (png, pdf, feed) = &*assets;
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                let (status, content_type, body): (&str, &str, &[u8]) = match path {
                    "/2026/why-print" => ("200 OK", "text/html; charset=utf-8", ARTICLE.as_bytes()),
                    "/images/press.png" => ("200 OK", "image/png", png),
                    "/paper.pdf" => ("200 OK", "application/pdf", pdf),
                    "/js-article" => ("200 OK", "text/html; charset=utf-8", JS_PAGE.as_bytes()),
                    "/bot-wall" if user_agent.contains("Topebag") => ("403 Forbidden", "text/plain", b"no robots"),
                    "/bot-wall" => ("200 OK", "text/html; charset=utf-8", ARTICLE.as_bytes()),
                    "/feed.xml" => ("200 OK", "application/rss+xml", feed.as_bytes()),
                    _ => ("404 Not Found", "text/plain", b"not found"),
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
            });
        }
    });
    base
}

/// Deterministic settings: no printing, no headless browser.
fn settings() -> Settings {
    Settings {
        format: Format { paper: Paper::Letter, ..Format::default() },
        browser_mode: BrowserMode::Never,
        ..Settings::default()
    }
}

/// Can a headless browser run here? Needs Edge/Chrome, and an environment
/// that lets it write to our pipe (some sandboxes make Chromium relaunch
/// itself detached, which loses its output).
fn browser_available(base: &str) -> bool {
    if topebag::browser::find_browser().is_none() {
        eprintln!("skipping: no Edge/Chrome installed");
        return false;
    }
    let profile = tempfile::tempdir().unwrap();
    let probe = Url::parse(&format!("{base}/2026/why-print")).unwrap();
    match topebag::browser::dump_dom(&probe, profile.path(), Duration::from_secs(30)) {
        Ok(html) if html.contains("Why We Still Print") => true,
        other => {
            eprintln!("skipping: headless browser cannot run in this environment ({:?})", other.err());
            false
        }
    }
}

#[test]
fn url_to_pdf_to_library_and_back() {
    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/2026/why-print")).unwrap();

    let mut stages = Vec::new();
    let mut progress = |s: Stage| stages.push(s);
    let outcome = pipeline::add_url(&library, &settings(), &url, None, &mut progress).unwrap();
    let entry = outcome.entry;
    assert!(outcome.print.is_none(), "printing was not requested");

    // Metadata
    assert_eq!(entry.title, "Why We Still Print Things");
    assert_eq!(entry.site_name.as_deref(), Some("The Example Review"));
    assert_eq!(entry.published.as_deref(), Some("28 October 2026"));
    assert!(!entry.used_fallback);
    assert!(entry.pages >= 1);
    assert_eq!(entry.images.len(), 1, "the press photo was downloaded: {:?}", entry.warnings);
    assert!(
        !entry.warnings.iter().any(|w| w.contains("simplified")),
        "rich typesetting succeeded: {:?}",
        entry.warnings
    );
    assert!(stages.contains(&Stage::Fetching) && stages.contains(&Stage::Typesetting));

    // Files on disk
    let dir = library.dir(&entry.id).unwrap();
    let pdf = std::fs::read(dir.join("document.pdf")).unwrap();
    assert!(pdf.starts_with(b"%PDF"));
    assert!(dir.join("img").join(&entry.images[0].file).exists());
    assert!(dir.join("qr.svg").exists());
    assert!(!dir.join("main.rich-failed.typ").exists());

    // The generated markup kept the structure and dropped the junk.
    let main = std::fs::read_to_string(dir.join("main.typ")).unwrap();
    for expected in [
        "#tb-figure(\"/img/001.jpg\"",
        "#tb-table(columns: 3",
        "#raw(block: true, lang: \"rust\"",
        "#heading(level: 2)[What makes a page print\\-friendly]",
        "#quote(block: true)",
        "#enum(",
        "#link(\"https://example.org/study?id=7&ref=print\")",
        "日本語",
    ] {
        assert!(main.contains(expected), "main.typ should contain {expected:?}\n---\n{main}");
    }
    for junk in ["BUY NOW", "Accept all", "© 2026", "window.tracking"] {
        assert!(!main.contains(junk), "main.typ should not contain {junk:?}");
    }

    // The library lists it.
    let entries = library.entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, entry.id);

    // Previews render one raster per page.
    let previews = pipeline::render_previews(&library, &entry.id, 30.0).unwrap();
    assert_eq!(previews.len(), entry.pages);

    // Re-format this document (its own format, not the defaults) offline.
    let mut draft = pipeline::load_draft(&library, &entry.id, &settings().format).unwrap().unwrap();
    draft.format = Format { paper: Paper::A4, columns: 2, include_images: false, ..draft.format };
    let updated = pipeline::rebuild(&library, &entry.id, &draft, false).unwrap().entry;
    let main = std::fs::read_to_string(dir.join("main.typ")).unwrap();
    assert!(main.contains("paper: \"a4\""));
    assert!(main.contains("cols: 2"));
    assert!(!main.contains("#tb-figure("));
    assert!(updated.pages >= 1);

    // Deleting removes everything.
    pipeline::delete_entry(&library, &entry.id).unwrap();
    assert!(!dir.exists());
    assert!(library.entries().unwrap().is_empty());
}

#[test]
fn bad_urls_fail_cleanly_without_leaving_files() {
    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/missing")).unwrap();
    let err = pipeline::add_url(&library, &settings(), &url, None, &mut |_| {}).err().unwrap();
    assert!(format!("{err:#}").contains("404"), "{err:#}");
    assert!(library.entries().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(library.root()).unwrap().count(), 0, "no half-created folders");
}

/// Exercises the real Windows print path (GDI → spooler) by printing to a
/// file through the built-in "Microsoft Print to PDF" printer. Skipped when
/// that printer is not installed.
///
/// Ignored by default: if Windows is set to "Let Windows manage my default
/// printer", printing anything makes that printer the new default — a
/// surprising side effect for a test. Run it explicitly with
/// `cargo test -- --ignored`.
#[cfg(windows)]
#[test]
#[ignore = "changes the default printer when Windows manages it; run with --ignored"]
fn prints_through_the_windows_spooler_to_a_file() {
    let installed = topebag::print::printers().unwrap();
    if !installed.iter().any(|p| p.name == topebag::print::PRINT_TO_PDF_PRINTER) {
        eprintln!("skipping: Microsoft Print to PDF is not installed");
        return;
    }
    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/2026/why-print")).unwrap();
    let out = home.path().join("printed.pdf");
    let target = PrintTarget {
        printer: Some(topebag::print::PRINT_TO_PDF_PRINTER.into()),
        output_file: Some(out.clone()),
    };

    let outcome = pipeline::add_url(&library, &settings(), &url, Some(&target), &mut |_| {}).unwrap();
    let record = outcome.print.expect("printing was requested").expect("print job succeeded");
    assert_eq!(record.printer, topebag::print::PRINT_TO_PDF_PRINTER);
    assert_eq!(record.pages, outcome.entry.pages);
    assert_eq!(library.entry(&outcome.entry.id).unwrap().print_count(), 1, "print recorded");

    wait_for_file(&out);
    let bytes = std::fs::read(&out).unwrap();
    assert!(bytes.starts_with(b"%PDF"), "spooler produced a PDF");
}

#[cfg(windows)]
fn wait_for_file(path: &Path) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false) {
            // Give the driver a moment to finish writing.
            std::thread::sleep(Duration::from_millis(500));
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{} was not written by the print spooler", path.display());
}

#[test]
fn pdf_links_are_stored_as_is_and_previewable() {
    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/paper.pdf")).unwrap();

    let entry = pipeline::add_url(&library, &settings(), &url, None, &mut |_| {}).unwrap().entry;
    assert_eq!(entry.kind, EntryKind::Pdf);
    assert_eq!(entry.title, "Quarterly Paper", "title comes from the PDF metadata");
    assert_eq!(entry.byline.as_deref(), Some("Ada"));
    assert_eq!(entry.pages, 2);

    // The original bytes are kept untouched.
    let stored = std::fs::read(library.pdf_path(&entry.id).unwrap()).unwrap();
    assert!(stored.starts_with(b"%PDF"));
    assert_eq!(topebag::typeset::pdf_info(&stored).unwrap().pages, 2);

    // Preview and re-typeset work (re-typeset is a no-op for PDFs).
    assert_eq!(pipeline::render_previews(&library, &entry.id, 20.0).unwrap().len(), 2);
    assert_eq!(pipeline::retypeset(&library, &entry.id, &settings()).unwrap().pages, 2);
}

#[test]
fn instapaper_feed_drives_the_queue() {
    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let feed = instapaper::parse_feed_url(&format!("{base}/feed.xml")).unwrap();

    let items = instapaper::fetch_feed(&feed).unwrap();
    assert_eq!(items.len(), 2);
    let mut state = QueueState::default();
    assert!(state.observe(&items).is_empty(), "backlog is never auto-printed");
    let queue: Vec<_> = state.unseen(&items).into_iter().cloned().collect();
    assert_eq!(queue.len(), 2);

    for item in &queue {
        let url = Url::parse(&item.url).unwrap();
        pipeline::add_url(&library, &settings(), &url, None, &mut |_| {}).unwrap();
        state.mark_seen([item.id.as_str()]);
    }
    assert!(state.unseen(&items).is_empty());
    let kinds: Vec<EntryKind> = library.entries().unwrap().iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&EntryKind::Pdf) && kinds.contains(&EntryKind::Article));
}

#[test]
fn tags_and_full_text_search_find_saved_articles() {
    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/2026/why-print")).unwrap();
    let mut entry = pipeline::add_url(&library, &settings(), &url, None, &mut |_| {}).unwrap().entry;
    entry.add_tag("Long reads");
    library.save(&entry).unwrap();

    let entries = library.entries().unwrap();
    let index = topebag::search::SearchIndex::build(&library, &entries);
    let find = |q: &str| index.matches(&entries[0], &topebag::search::parse_query(q));
    let hit = find("comprehension").expect("word from the article body");
    assert!(hit.snippet.unwrap().contains("comprehension"));
    assert!(find("tag:long-reads").is_some());
    assert!(find("tag:cooking").is_none());
    assert!(find("\"infinite scroll\"").is_some());
}

#[test]
fn javascript_pages_fall_back_to_the_browser() {
    let base = serve();
    if !browser_available(&base) {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/js-article")).unwrap();
    let auto = Settings { browser_mode: BrowserMode::Auto, ..settings() };

    let mut stages = Vec::new();
    let entry = pipeline::add_url(&library, &auto, &url, None, &mut |s| stages.push(s)).unwrap().entry;
    assert!(stages.contains(&Stage::Browser), "the browser was used: {stages:?}");
    assert!(entry.via_browser);
    assert!(entry.word_count > 300, "rendered text was extracted ({} words)", entry.word_count);
    assert!(!entry.used_fallback);
    assert!(home.path().join("browser-profile").exists(), "Topebag's own profile was used");
}

#[test]
fn sites_that_refuse_robots_are_loaded_with_the_browser() {
    let base = serve();
    if !browser_available(&base) {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/bot-wall")).unwrap();

    // With the browser disabled, the 403 is reported.
    let err = pipeline::add_url(&library, &settings(), &url, None, &mut |_| {}).err().unwrap();
    assert!(format!("{err:#}").contains("403"));

    let auto = Settings { browser_mode: BrowserMode::Auto, ..settings() };
    let entry = pipeline::add_url(&library, &auto, &url, None, &mut |_| {}).unwrap().entry;
    assert!(entry.via_browser);
    assert_eq!(entry.title, "Why We Still Print Things");
}

#[test]
fn side_by_side_spreads_are_landscape_and_fitted() {
    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/2026/why-print")).unwrap();
    let spread = Settings { format: Format { side_by_side: true, fit_to_sheets: true, ..settings().format }, ..settings() };

    let entry = pipeline::add_url(&library, &spread, &url, None, &mut |_| {}).unwrap().entry;
    assert!(entry.spread);
    let main = std::fs::read_to_string(library.dir(&entry.id).unwrap().join("main.typ")).unwrap();
    assert!(main.contains("spread: true"));

    let previews = pipeline::render_previews(&library, &entry.id, 20.0).unwrap();
    assert_eq!(previews.len(), entry.pages);
    assert!(previews[0].width > previews[0].height, "each sheet is landscape");

    // Switching this document back to one page per sheet gives portrait pages.
    let mut draft = pipeline::load_draft(&library, &entry.id, &settings().format).unwrap().unwrap();
    assert!(draft.format.side_by_side, "the document remembers its own format");
    draft.format.side_by_side = false;
    let portrait = pipeline::rebuild(&library, &entry.id, &draft, false).unwrap().entry;
    assert!(!portrait.spread);
    assert_eq!(portrait.density, 0.0);
    let previews = pipeline::render_previews(&library, &entry.id, 20.0).unwrap();
    assert!(previews[0].width < previews[0].height);
}

#[test]
fn edit_format_preview_then_print_flow() {
    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/2026/why-print")).unwrap();

    // Adding a link makes an editable draft and prints nothing.
    let outcome = pipeline::add_url(&library, &settings(), &url, None, &mut |_| {}).unwrap();
    assert!(outcome.print.is_none());
    let id = outcome.entry.id.clone();
    assert_eq!(library.entry(&id).unwrap().print_count(), 0);
    let dir = library.dir(&id).unwrap();
    assert!(dir.join("document.json").exists());

    let mut draft = pipeline::load_draft(&library, &id, &settings().format).unwrap().unwrap();
    assert!(draft.blocks.len() >= 8, "article split into blocks: {}", draft.blocks.len());
    let find = |draft: &topebag::document::Draft, needle: &str| {
        draft.blocks.iter().position(|b| b.preview().contains(needle)).unwrap_or_else(|| panic!("no block with {needle:?}"))
    };

    // Edit: drop the table and the duplicated byline, rewrite a heading,
    // move the code section up, and rename the document.
    let table = find(&draft, "Minutes read");
    draft.blocks[table].included = false;
    let byline = find(&draft, "By Grace Hopper");
    draft.blocks[byline].included = false;
    let heading = find(&draft, "A tiny bit of code");
    draft.blocks[heading].edited = Some("Counting words, *quickly*".into());
    draft.move_block(heading, true);
    draft.meta.title = "Why Paper Still Wins".into();

    let rebuilt = pipeline::rebuild(&library, &id, &draft, false).unwrap();
    let main = std::fs::read_to_string(dir.join("main.typ")).unwrap();
    assert!(!main.contains("Minutes read"), "dropped table is gone");
    assert!(!main.contains("By Grace Hopper\n"), "dropped byline paragraph is gone");
    assert!(main.contains("Counting words, #emph[quickly]"), "edited heading is typeset:\n{main}");
    assert!(main.contains("title: \"Why Paper Still Wins\""));
    assert_eq!(rebuilt.entry.title, "Why Paper Still Wins", "library shows the new title");
    assert_eq!(rebuilt.compiled.page_count(), rebuilt.entry.pages);
    let text = std::fs::read_to_string(dir.join("article.txt")).unwrap();
    assert!(text.contains("Counting words") && !text.contains("Minutes read"), "search sees the edited text");

    // The draft survives a reload.
    let reloaded = pipeline::load_draft(&library, &id, &settings().format).unwrap().unwrap();
    assert_eq!(reloaded, draft);

    // Missing images: offline rebuilds skip them; refilling downloads them.
    let mut entry = library.entry(&id).unwrap();
    entry.images.clear();
    library.save(&entry).unwrap();
    std::fs::remove_dir_all(dir.join("img")).unwrap();
    let offline = pipeline::rebuild(&library, &id, &draft, false).unwrap();
    assert!(offline.entry.images.is_empty());
    assert!(!std::fs::read_to_string(dir.join("main.typ")).unwrap().contains("#tb-figure("));
    let refilled = pipeline::rebuild(&library, &id, &draft, true).unwrap();
    assert_eq!(refilled.entry.images.len(), 1, "missing image fetched again");
    assert!(std::fs::read_to_string(dir.join("main.typ")).unwrap().contains("#tb-figure("));

    // Grayscale is per document and doesn't re-download.
    draft.format.grayscale_images = true;
    pipeline::rebuild(&library, &id, &draft, false).unwrap();
    assert!(std::fs::read_to_string(dir.join("main.typ")).unwrap().contains("/img/gray-"));

    // Restore original brings everything back (formatting is kept).
    draft.restore_original(&pipeline::original_html(&library, &id).unwrap());
    let restored = pipeline::rebuild(&library, &id, &draft, false).unwrap();
    let main = std::fs::read_to_string(dir.join("main.typ")).unwrap();
    assert!(main.contains("Minutes read") && main.contains("A tiny bit of code"));
    assert_eq!(restored.entry.title, "Why We Still Print Things");
    assert!(draft.format.grayscale_images);
}

#[test]
fn pdfs_have_no_draft() {
    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/paper.pdf")).unwrap();
    let id = pipeline::add_url(&library, &settings(), &url, None, &mut |_| {}).unwrap().entry.id;
    assert!(pipeline::load_draft(&library, &id, &settings().format).unwrap().is_none());
}

#[test]
fn articles_combine_into_a_newspaper_edition() {
    use topebag::edition;

    let base = serve();
    let home = tempfile::tempdir().unwrap();
    let library = Library::open(home.path()).unwrap();
    let url = Url::parse(&format!("{base}/2026/why-print")).unwrap();

    // Three articles, each retitled like a reader might, printed separately.
    let mut ids = Vec::new();
    let mut separate_pages = 0;
    for n in 1..=3 {
        let id = pipeline::add_url(&library, &settings(), &url, None, &mut |_| {}).unwrap().entry.id;
        let mut draft = pipeline::load_draft(&library, &id, &settings().format).unwrap().unwrap();
        draft.meta.title = format!("Story number {n}");
        separate_pages += pipeline::rebuild(&library, &id, &draft, false).unwrap().entry.pages;
        ids.push(id);
    }
    // A PDF can't flow into columns, so it's left out.
    let pdf_url = Url::parse(&format!("{base}/paper.pdf")).unwrap();
    let pdf = pipeline::add_url(&library, &settings(), &pdf_url, None, &mut |_| {}).unwrap().entry.id;
    let picked = vec![ids[0].clone(), pdf.clone(), ids[1].clone(), ids[2].clone()];

    let rebuilt = edition::create(&library, edition::new_from_settings(&picked, &settings())).unwrap();
    let entry = &rebuilt.entry;
    let id = entry.id.clone();
    assert_eq!(entry.kind, EntryKind::Edition);
    assert!(entry.url.is_empty());
    assert_eq!(entry.pages, rebuilt.compiled.page_count());
    assert!(
        entry.pages < separate_pages,
        "stories share pages instead of each ending on its own: {} pages vs {separate_pages} printed separately",
        entry.pages
    );
    assert_eq!(entry.print_count(), 0, "nothing prints by itself");

    let dir = library.dir(&id).unwrap();
    let main = std::fs::read_to_string(dir.join("main.typ")).unwrap();
    assert_eq!(main.matches("#tb-story(").count(), 3, "the PDF was skipped:\n{main}");
    let at = |needle: &str| main.find(needle).unwrap_or_else(|| panic!("{needle:?} missing"));
    assert!(at("Story number 1") < at("Story number 2") && at("Story number 2") < at("Story number 3"), "picked order");
    assert!(!main.contains("float: true"), "pictures stay inside their own story");
    assert!(main.contains("cols: 3"));

    // Each story's images are copied in, so the edition outlives its articles.
    assert!(main.contains("\"/img/1-") && main.contains("\"/img/3-"), "story images:\n{main}");
    assert_eq!(std::fs::read_dir(dir.join("img")).unwrap().count(), 3);

    // Saved model, search text, and preview/print from what's on disk.
    let mut ed = edition::load(&library, &id).unwrap();
    assert_eq!(ed.members.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), ids);
    assert!(library.text(&id).unwrap().contains("Story number 2"));
    assert_eq!(pipeline::render_previews(&library, &id, 30.0).unwrap().len(), entry.pages);

    // Edits to an article carry into the edition when it's rebuilt; stories
    // can be reordered and left out.
    let mut draft = pipeline::load_draft(&library, &ids[2], &settings().format).unwrap().unwrap();
    draft.meta.title = "Story three, revised".into();
    pipeline::rebuild(&library, &ids[2], &draft, false).unwrap();
    assert!(edition::is_stale(&library, &id, &ed), "the edition knows a story changed");
    ed.move_member(2, true); // 1, 3, 2
    ed.members[0].included = false;
    ed.title = "The Weekend Paper".into();
    let again = edition::rebuild(&library, &id, &ed).unwrap();
    let main = std::fs::read_to_string(dir.join("main.typ")).unwrap();
    assert!(!main.contains("Story number 1"));
    assert!(main.find("Story three, revised").unwrap() < main.find("Story number 2").unwrap());
    assert!(main.contains("title: \"The Weekend Paper\""));
    assert!(again.entry.title.starts_with("The Weekend Paper"));
    assert_eq!(again.entry.site_name.as_deref(), Some("2 stories"));
    assert!(!edition::is_stale(&library, &id, &ed));

    // Retypeset (the CLI path) understands editions too.
    let retypeset = pipeline::retypeset(&library, &id, &settings()).unwrap();
    assert_eq!(retypeset.kind, EntryKind::Edition);
    assert!(pipeline::load_draft(&library, &id, &settings().format).unwrap().is_none(), "editions have no block draft");

    // Deleting a story's article leaves a note instead of breaking the edition.
    pipeline::delete_entry(&library, &ids[1]).unwrap();
    let after = edition::rebuild(&library, &id, &ed).unwrap();
    assert!(after.entry.warnings.iter().any(|w| w.contains("deleted")), "{:?}", after.entry.warnings);

    // Only PDFs picked: nothing to lay out, and no half-made entry is left.
    let before = library.entries().unwrap().len();
    assert!(edition::create(&library, edition::new_from_settings(std::slice::from_ref(&pdf), &settings())).is_err());
    assert_eq!(library.entries().unwrap().len(), before);
}
