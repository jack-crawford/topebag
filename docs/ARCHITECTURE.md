# Topebag architecture

This document explains how Topebag is put together and why. It's meant for
anyone changing or extending the code. For usage, see the
[README](../README.md). For API-level docs, run `cargo doc --open`.

## The workflow

In the app, a link goes through **link → edit → format → preview → print**:

1. `pipeline::add_url` fetches the link and saves it as a document: the
   cleaned `article.html`, a **draft** (`document.json`) that splits it into
   blocks and records the default format, and a first PDF.
2. The editor (`gui/mod.rs`) loads the draft with `pipeline::load_draft`.
   Every edit or format change marks the document dirty.
3. About 450 ms after the last change, the draft goes to the worker's
   rebuild thread, which calls `pipeline::rebuild` (save the draft, convert
   the included blocks, typeset, write the PDF) and renders preview pages
   from the same layout.
4. Printing is a separate, explicit request (`pipeline::print_entry`) that
   prints the latest layout. The Print button is disabled while a rebuild is
   pending, so what you print is always what you saw.

The CLI uses the same functions: `add` (optionally `--print`) and
`retypeset`, which rebuilds from the draft.

## The pipeline

Adding a link runs one linear pipeline (`pipeline::add_url`):

```
                 ┌────────────┐   ┌─────────────┐   ┌──────────────┐
  URL ─────────► │  fetch.rs  │──►│ extract.rs  │──►│  convert.rs  │──┐
                 │ HTTP, meta │   │ Readability │   │ HTML → Typst │  │
                 │  refresh   │   │ + metadata  │   │   markup     │  │
                 └────────────┘   └─────────────┘   └──────┬───────┘  │
                                                           │ images   │
                                                    ┌──────▼───────┐  │
                                                    │  images.rs   │  │
                                                    │ download and │  │
                                                    │  normalise   │  │
                                                    └──────────────┘  │
   ┌──────────────────────────────────────────────────────────────────┘
   │
   ▼
 ┌────────────┐   ┌──────────────┐   ┌──────────────┐
 │ typeset.rs │──►│ document.pdf │   │  print/*.rs  │──► printer
 │   Typst    │   │ (library.rs) │   │  GDI / CUPS  │
 │  compiler  │───┼──────────────┼──►│              │
 └────────────┘   └──────────────┘   └──────────────┘
        │
        └──► page rasters ──► GUI preview
```

1. **Acquire** (`pipeline::acquire`, `fetch.rs`, `browser.rs`). One shared
   blocking `reqwest` client with a browser-like user agent, response-size
   caps, charset detection (HTTP header, then `<meta charset>`, then UTF-8)
   and up to three `<meta http-equiv="refresh">` redirects. A response whose
   bytes start with `%PDF-` is a **PDF link** and takes the PDF path (below).
   The headless browser is used instead of, or after, the plain download:
   - always, for sites listed under *Sites you've signed in to* (or when
     *Use the browser* is *Always*);
   - when the plain download is refused (401/403/429/451), since many sites
     block non-browser clients;
   - when the extracted article is thin (under 200 words, or no article
     found), which usually means a JavaScript-built page. The browser result
     is kept only if it's clearly better.
2. **Extract** (`extract.rs`). [`dom_smoothie`], a Rust port of Mozilla
   Readability, isolates the article and its metadata. If it finds nothing
   convincing (under 250 characters of text), Topebag falls back to
   `<main>`/`<article>`/`<body>` and lets the converter strip page chrome.
   Titles lose " | Site Name" suffixes, and dates are formatted for display.
3. **Convert** (`convert.rs`). The cleaned HTML is walked and turned into
   Typst markup. This is the most delicate module; see below.
4. **Images** (`images.rs`). While converting, every `<img>` goes through an
   `ImageResolver`. The `Downloader` fetches it, rejects icons and tracking
   pixels, downscales to at most 1800 px, optionally converts to grayscale,
   and re-encodes as JPEG (or PNG when it has transparency). It writes the
   result to `img/NNN.ext`. The list of stored images goes into `entry.json`
   so re-typesetting can replay them offline (`StoredImages`).
5. **Typeset** (`typeset.rs`). `main_source` builds `main.typ`, which imports
   the template and contains the converted body. The embedded Typst compiler
   lays it out into pages. If the rich markup ever fails to compile,
   `pipeline::build` retries with a plain-paragraph rendering so the user
   always gets a document, and records a warning.
6. **Save** (`library.rs`). The PDF and `entry.json` are written atomically
   into the entry's folder.
7. **Print** (`print/`). Optional. On Windows the laid-out pages are
   rasterised and sent through GDI. On macOS and Linux the PDF goes to CUPS.

**PDF links** skip steps 2–5. The original file is saved untouched as
`document.pdf`, `typeset::pdf_info` reads its page count, title and author
(via `hayro-syntax`, already part of Typst), and a three-line Typst wrapper
(`typeset::pdf_wrapper_source`) places each PDF page on a page of the same
size. Previews and printing then work exactly as for articles, because
Typst renders PDF pages as images.

The CLI and the GUI call the same `pipeline` functions, so they can't drift
apart.

## Module map

| Module | Responsibility | Key items |
|---|---|---|
| `lib.rs` | Crate docs and module list | |
| `fetch.rs` | HTTP and URL parsing | `parse_url`, `urls_in_text`, `fetch` → `Fetched::{Html, Pdf}`, `fetch_text`, `fetch_resource` |
| `browser.rs` | Headless Edge/Chrome | `find_browser`, `dump_dom`, `open_sign_in_window`, `profile_dir`, `matches_site` |
| `extract.rs` | Reader mode | `extract` → `Article` |
| `images.rs` | Image pipeline | `ImageResolver`, `Downloader`, `StoredImages`, `NoImages` |
| `document.rs` | Editable drafts | `Draft`, `Block`, `BlockKind`, `split_blocks`, `html_to_markdown`, `markdown_to_html` |
| `edition.rs` | Newspaper-style editions | `Edition`, `Member`, `create`, `rebuild`, `is_stale`, `can_join` |
| `convert.rs` | HTML → Typst | `html_to_typst`, `html_to_plain_typst`, `escape_text`, `string_literal` |
| `typeset.rs` | Typst integration | `main_source`, `compile`, `pdf`, `render_page`, fonts and `World` |
| `library.rs` | Storage | `Library`, `Entry`, `PrintRecord`, `data_dir` |
| `pipeline.rs` | Workflows | `add_url` (acquire → article or PDF), `load_draft`, `rebuild`, `print_entry`, `retypeset`, `render_previews` |
| `instapaper.rs` | Reading-queue feeds | `parse_feed`, `fetch_feed`, `QueueState` (`observe`, `unseen`, `mark_seen`) |
| `search.rs` | Library search | `parse_query`, `SearchIndex::matches` → `Hit` with snippet |
| `instance.rs` | Single instance | `claim` → `Role::{Primary, Secondary}`, `Primary::listen` |
| `autostart.rs` | Start at sign-in | `set_enabled`, `is_enabled` (Windows `Run` key; macOS `launch_agent`) |
| `print/` | Printing | `printers`, `default_printer`, `print` (Windows GDI / CUPS) |
| `settings.rs` | Preferences | `Settings`, per-document `Format`, enums |
| `main.rs`, `cli.rs` | Executable | GUI by default; subcommands for the CLI |
| `gui/` | Desktop app | `App` (UI state), `worker` (threads), `keys` (keyboard shortcuts), `tray`, `theme` |
| `assets/topebag.typ` | Document template | `tb-base` (shared type styles), `tb-article`, `tb-edition`, `tb-story`, `tb-figure`, `tb-table`, … |

## Key design decisions

### Typst as the typesetter

Typst is a modern typesetting system written in Rust and usable as a library.
It gives Topebag:

- **Book-quality typography** with no external tools: Knuth–Plass line
  breaking, hyphenation in many languages, smart quotes, ligatures, and
  footnotes.
- **One layout, three outputs.** The same laid-out document produces the
  saved PDF (`typst-pdf`), the on-screen preview and the print rasters
  (`typst-render`). What you see is exactly what prints.
- **A readable template.** All visual design lives in `assets/topebag.typ`,
  which is far easier to adjust than Rust drawing code.
- **Determinism.** The fonts it needs (Libertinus Serif, DejaVu Sans Mono)
  are bundled, so output doesn't depend on the machine. A curated list of
  Windows fonts adds Segoe UI and coverage for symbols, emoji, CJK and
  Indic scripts. They're loaded lazily, so unused fonts cost nothing.

`typeset::ProjectWorld` implements Typst's `World` trait: it serves
`main.typ` and the template from memory, reads other files (images,
`qr.svg`) from the entry folder, and refuses any path that tries to leave it.

### Converting HTML to Typst safely

Web content is untrusted text, and Typst markup is a programming language
(`#` starts code). The converter guarantees that no web text is ever
interpreted as Typst syntax:

- Every character with meaning in Typst markup (`\ # * _ ` $ < > @ [ ] = - +
  / ~ { } ( ) . ; :`) is backslash-escaped (`escape_text`). `'` and `"` are
  left alone on purpose, so Typst renders them as typographic quotes.
- Every value passed into Typst code (URLs, code blocks, paths) is a string
  literal (`string_literal`).
- Structure is always emitted in function-call form (`#heading(level: 2)[…]`,
  `#list([…], […])`, `#enum(start: 3, …)`). It never uses line-based
  shortcuts, so a paragraph that begins "1." or "-" can't turn into a list.

HTML is messy, so the walker works with **flows**: inside any container,
runs of inline content become paragraphs, and block elements are emitted
between them. Notable rules:

- Images inside inline elements (`<a><img></a>`) are *deferred* and placed
  after the current paragraph.
- Inline wrappers around real blocks (`<font><p>…</p></font>`, common on
  older sites) are treated as containers, so paragraphs survive.
- `<br><br>` becomes a paragraph break. A single `<br>` is a line break.
- Whitespace collapses as it does in HTML, except in `<pre>`.
- A first heading that repeats the title is dropped, because the masthead
  already shows it.
- One-column "layout tables" are flattened. Real tables keep header rows
  (repeated on each page), `colspan` and `rowspan`.
- Top-level figures **float** to the top or bottom of a page, as in books,
  so a tall image never leaves a half-empty page. Figures inside lists,
  tables or quotes stay in place.
- Images use their HTML `width`/`height` when those are smaller than the
  pixel size, because high-DPI sites serve 2× images.

### Drafts: editing by blocks

`document::split_blocks` walks the cleaned article HTML and returns
top-level blocks. It looks *through* wrapper elements (`div`, `section`,
`span`, `font`…) that contain block content. Loose inline text between
blocks becomes its own paragraph block. A paragraph or wrapper holding only
an image is an image block, and empty blocks are dropped. Each block keeps
its original HTML and a stable `id`.

**Sections.** Headings give the flat block list a hierarchy without storing
one: `Draft::section(i)` is a heading plus everything up to the next heading
of the same or a higher level (any other block is its own section), and
`Draft::depths()` gives each block's nesting for indentation. A heading's
checkbox calls `set_section_included`, and `section_inclusion` reports
`All`, `Partly` (shown as a dash) or `Nothing`. `move_section` rotates a
heading's section past the neighbouring section of the same level (or past
one block when there's none), so ↑ ↓ on a heading keep its contents with it.
Because the hierarchy is derived from heading levels every time, it can't go
stale after edits or moves. Folding is purely a view: the GUI keeps folded
heading ids per document in memory, and `section` decides what to hide.

A block can be **excluded**, **moved**, or **edited**. Edits round-trip
through light Markdown: `html_to_markdown` turns the block's inline HTML into
`**bold**`, `*italic*`, `` `code` ``, `[link](url)` (lists become `- item`
lines, quotes become paragraphs, code stays raw), and `markdown_to_html`
(pulldown-cmark) turns the edited text back into HTML of the same kind.
`Draft::html()` joins the included blocks in order, and that HTML goes
through the same safe `convert::html_to_typst` as before. So edited text can
never inject Typst syntax, and "Restore original" just re-splits
`article.html`.

The draft also stores the masthead (`Meta`: title, byline, date, plus the
original values) and the document's own `Format`. `Settings.format` is
flattened into `settings.json`, so settings files from before per-document
formatting still load and supply the defaults. Entries saved before drafts
existed get a draft made from their `article.html` the first time they're
opened.

**Images during edits.** Live rebuilds are offline: `RefillImages` only
replays stored images. The first rebuild after opening a document, and the
*Fetch missing images* button, also download images that aren't stored yet
(skipping URLs known to be icons, kept in `entry.rejected_images`).
Grayscale is per document: `GrayImages` makes `gray-NNN` copies on demand
from the colour originals, so switching it never re-downloads.

**The rebuild thread** (`gui/worker.rs`) coalesces requests: while it's
busy, newer drafts of the same document replace older ones, so a burst of
typing costs one rebuild. Pending edits of a document you switch away from
are still rebuilt and saved.

### Side-by-side spreads and the spacing optimizer

**Layout** (`assets/topebag.typ`): with `spread: true`, the page is the
landscape version of the chosen paper, and the body (masthead included)
flows through `columns(2)`. Each column is a "page": the gutter is twice the
inner margin, the header puts the title over the left half and the site over
the right, the footer numbers the halves `2n−1` and `2n`, and a small mark in
the `background` shows the fold. The masthead is placed as the first top
float, so floating figures always queue below it, never above the title.

**Density**: one parameter `density ∈ [0, 1]` interpolates every spacing
decision between a comfortable and a compact value: text size (down to
8.5 pt), leading, paragraph and block spacing, heading spacing, margins,
code and table padding, the masthead's title and QR code, and the maximum
figure height (shared with `tb-figure` through a Typst `state`). Above
density 0.3, paragraphs get a first-line indent so they stay distinct as the
gaps shrink. A single knob means the optimizer has one dimension to search,
and every intermediate layout is coherent.

**Optimizer** (`typeset::compile_fitted`): sheets as a function of density
is a decreasing step function. Topebag lays out at density 0 and 1. If
compact doesn't save a sheet, it keeps normal spacing. Otherwise it
binary-searches (6 steps, about 1.6 % precision, 20 s budget) for the
smallest density that still reaches the compact sheet count. The objective
is "fewest sheets, then least tightening". Typst's incremental compilation
keeps this cheap: about 0.3 s for a newsletter article, 0.7 s for a 19-page
essay, and 4.6 s for a 146-page book.

**Reuse**: the search lives in `typeset::fit_density(make_source, root, fit)`,
which takes any "density → Typst source" function. Articles and editions
both use it.

**Printing**: printers default to portrait, so `print/windows.rs` turns any
page whose orientation differs from the paper's a quarter turn
counter-clockwise (`rotate_ccw`) before blitting. Landscape PDF links
benefit too.

### Editions: newspaper-style batches

Printing articles one at a time wastes the end of every last page. An
edition (`edition.rs`) is a library entry of kind `Edition` whose stories flow
through **one** column layout, so the only blank space is at the very end.

**Model.** `edition.json` holds an `Edition`: the nameplate title, the
ordered `members` (article id plus an `included` flag), whether page 1 lists
the contents, and its own `Format` (three columns, 9.5 pt and fit-to-pages by
default; `side_by_side` is ignored). Only `EntryKind::Article` entries can
join (`can_join`), since PDFs can't be reflowed. `create` drops the others,
saves the entry and lays it out, and removes the half-made entry if layout
fails.

**Building** (`edition::rebuild`). For each included member, it loads the
article's **current draft** with `pipeline::load_draft` (edits, dropped
blocks and a changed title all carry over) and removes a short byline
paragraph near the top that the story header would repeat. It then converts
with `ConvertOptions { allow_floats: false, … }`, so figures stay inside
their story instead of floating to the top of a page that belongs to
another. Images go through a `CopyImages` resolver that copies each stored
image to `img/<story>-<file>`, so the edition is self-contained and still
prints after an article is deleted (that story is skipped with a warning).
Each story becomes one `#tb-story(title:, site:, byline:, date:, minutes:,
qr:, first:, lang:, region:)[body]` call, and `main.typ` wraps them all in
`#show: tb-edition.with(...)`. Layout goes through the same `fit_density`
search as spreads, minimising **pages** instead of sheets. The entry's title
(`<nameplate> · <date>`), page count, density and warnings are updated, and
`article.txt` gets the story titles for search.

**Template** (`assets/topebag.typ`). `tb-base` holds the type styles shared
with articles (text, paragraphs, headings, code, quotes, tables, lists), so
an edition sets type exactly like a single article at the same density.
`tb-edition` sets up the page (running head from page 2, page numbers,
hairlines between columns drawn in the page `background`) and places the
**nameplate** as a top float spanning all columns (`scope: "parent"`):
heavy rule, title, dateline, story count and reading time, and the "In this
edition" line. That line is built in `context` by querying the `metadata`
marker each story emits, so its page numbers are always right.
`tb-story` emits that marker and then one **sticky, unbreakable header
block** (separator rule, kicker, headline, details, optional QR). Because it's
sticky, it moves to the next column together with the story's first lines
rather than sitting alone at a column's foot. The rule lives inside the same
block on purpose: Typst doesn't chain two sticky blocks. The body follows
directly in the column flow, with no container, so a story can break across
columns and pages anywhere, and headings inside a story are scaled down to
stay below its headline.

**Staleness.** `is_stale` compares the edition's `document.pdf` timestamp
with its stories' `document.json`. The GUI highlights **Update from
articles** when an article was edited after the edition was laid out.

**GUI.** The library keeps an ordered `picked` list next to the open
document. Ctrl+click toggles (starting from the open document, like a file
manager), Shift+click adds a range in display order, and **Select** mode
turns plain clicks into toggles. A batch bar offers **Combine into an
edition** (`Request::CreateEdition` on the job thread, reported as
`Event::Added`) or **Add to "…"** for the open edition. The editor's
`Editor::source()` yields either `Source::Draft` or `Source::Edition`, so the
debounced rebuild thread, the latest-wins coalescing and the live preview
work the same for both.

### Printing by rasterising

The usual ways to print a PDF on Windows depend on a third-party PDF reader
being installed and registered (`ShellExecute "print"`, SumatraPDF, Adobe).
Topebag avoids that: Typst rasterises each page at the printer's resolution
(capped at 300 or 600 dpi), and the bitmap goes to the printer through Win32
GDI (`CreateDC` → `StartDoc` → `StretchDIBits` per page → `EndDoc`). Every
Windows printer driver supports this, and the job appears in the normal
print queue.

- Pages are positioned on the **physical** sheet (accounting for the
  printer's unprintable margins), so the document's own margins are kept.
  If the paper is smaller than the document, the page is scaled to fit.
- Bitmaps are 24-bit, bottom-up DIBs, the most widely supported format.
  `HALFTONE` stretching smooths output when the printer's resolution is
  higher than the raster's.
- `DOCINFO.lpszOutput` allows printing to a file. With the built-in
  *Microsoft Print to PDF* printer this tests the real spooler path without
  paper (`--print-to-file`, and the ignored integration test).

The trade-off is that text reaches the printer as pixels rather than
vectors. At 300–600 dpi this is visually indistinguishable on paper, and
it's simple and reliable. A future vector path (for example XPS via the
Windows Print API) could be added behind the same `print::print` interface.

On macOS and Linux, CUPS accepts PDF natively, so `print/cups.rs` just runs
`lp`.

### The library is plain folders

Each entry is a self-contained folder. `Library::entries()` lists folders
that have a readable `entry.json` and logs (never deletes) broken ones.
Writes are atomic (temporary file, then rename). Entry IDs are
`<timestamp>_<slug>`, and every ID is validated before use so it can't
escape the library folder. A failed import removes its half-created folder.

### The headless browser and the sign-in profile

`browser::dump_dom` runs Edge (or Chrome) with `--headless=new --dump-dom`,
waits up to 45 s, and reads the rendered DOM from stdout. Both pipes are
drained on helper threads so a chatty browser can't deadlock the call.

It always uses **Topebag's own profile folder** (`<data dir>/browser-profile`),
never the user's everyday profile. That has three benefits: it works while
the user's Edge is open (two browsers can't share a profile), it never
touches the user's real cookies or passwords (no cookie decryption or
keychain access), and signing in once in that profile
(`open_sign_in_window`) gives headless fetches the user's subscriptions.

The trade-off is that Edge can't use a profile that's already open, so the
sign-in window must be closed before fetching. The error message says so.

A quirk worth knowing: in some sandboxed environments (certain terminals, CI
runners) Chromium relaunches itself detached and its output is lost. The
integration tests probe for this and skip the browser tests with a clear
message, rather than failing.

### The Instapaper queue

Instapaper's full API needs a developer key, but every account has a private
RSS feed of the Unread folder. `instapaper.rs` parses RSS 2.0 and Atom
(`roxmltree`, with DTDs disabled since feeds are untrusted input) and keeps
`instapaper.json`:

- **`seen`**: items printed or skipped. They're never offered again.
- **`known`**: every item ever observed. `QueueState::observe` returns only
  items that weren't known at the previous check, and nothing on the very
  first check. That's what makes "print new saves automatically" safe: it
  never prints the existing backlog, which stays available in the queue
  window instead.

The GUI checks the feed on startup and every *N* minutes (default 15) from
`App::logic`, so checks continue while the app is hidden in the tray. Queued
items go through the normal `add_url` pipeline, so PDFs, JavaScript pages
and sign-in sites all work for Instapaper saves too.

### Tags and full-text search

Tags live in `entry.json` (lower-cased and sorted; `tag:long-reads` also
matches "long reads"). For full text, each article's plain text is saved as
`article.txt`. Older entries get it generated from `article.html` on first
use. `SearchIndex` keeps the lower-cased text of every entry in memory: a
few MB for hundreds of articles. A linear scan per keystroke is instant, so
no search engine or database is needed. The GUI builds the index on a
background thread at startup and updates it as documents are added or
deleted.

### Tray, background mode and a single instance

- **Tray** (`gui/tray.rs`, Windows): `tray-icon` with a small menu. Menu and
  click events arrive on the UI thread's message loop and are forwarded over
  a channel, with `request_repaint()` to wake the app.
- **Hidden window**: closing the window sends `CancelClose` plus
  `Visible(false)`. eframe keeps calling `App::logic` (not `App::ui`) while
  the window is hidden, so all event handling, timers and tray actions live
  in `logic`. A heartbeat thread wakes the app every 30 s so timers keep
  running. eframe always shows the window after its first frame, so
  `--background` hides it again on the second frame.
- **Single instance** (`instance.rs`): the first instance listens on a random
  127.0.0.1 port and writes the port and a random token to `instance.json`.
  Later launches connect, prove they know the token, pass "show" and any
  links, and exit. A stale `instance.json` is harmless: the connection fails
  and the new launch becomes the primary.
- **Start with Windows** (`autostart.rs`): one `REG_SZ` value under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` that runs
  `topebag.exe gui --background`. No admin rights, and it's removed when the
  setting is turned off. On macOS the same setting writes a LaunchAgent
  plist (`launch_agent`) instead.

### Threads in the GUI

The egui UI thread never blocks:

- **One job thread** (`gui/worker.rs`) runs add, print and re-typeset jobs in
  order. Sequential jobs keep print order predictable and are polite to the
  sites being fetched.
- **Preview threads** are spawned per request, so browsing the library stays
  instant even while a long job runs. The search index is built the same way.
- **A heartbeat thread** wakes the app every 30 s so feed checks run while
  the window is hidden.
- **The instance listener** accepts hand-offs from later launches.
- Results come back as `Event`s over a channel. Each send calls
  `request_repaint()`, so the UI wakes up and applies them in
  `App::poll_events`.

### Keyboard

`gui/keys.rs` decodes each frame's key presses into `Command`s (`keys::read`)
and `App::run_command` carries them out, so the decoding is tested against a
bare `egui::Context` without any UI. Three rules keep shortcuts from fighting
egui. First, while a text field has focus (or had it last frame, so the Esc
that leaves a field doesn't also untick the library), only Ctrl shortcuts and
F-keys are read. Second, after Tab focuses a widget, arrows and Space are left
to egui's own widget navigation. Third, an open confirmation dialog gets every
key: Enter confirms (only after the dialog has been on screen for a frame, so
the key press that opened it can't confirm it too), and Esc cancels through
`Modal::should_close`. Shifted bindings are checked before plain ones because
egui's `consume_key` ignores an extra Shift. The same `keys::GROUPS` table
drives the F1 window, so the help can't drift from the bindings.

Moving through the library opens each document, but its preview starts only
after a 150 ms pause (`preview_due`), so holding ↓ doesn't start a render for
every entry passed.

### Errors and resilience

- Network and HTTP errors fail the job with a readable message, and nothing
  is left on disk.
- An image failure skips the image and adds a note. Icons are skipped
  silently.
- A Typst failure falls back to plain text and records a warning.
  `main.rich-failed.typ` is kept for debugging.
- A printer failure keeps the document saved. The job reports
  "Saved, but printing failed: …".
- Typst warnings about font fallbacks are logged, not shown.

## Extending Topebag

**Add a setting.** Add a field to `Settings` (with a default; `#[serde(default)]`
keeps old settings files working), clamp it in `sanitize`, add a control in
`gui/mod.rs::settings_window`, and use it in `typeset::main_source` (layout)
or `convert::ConvertOptions` (content). If it changes layout, add it to the
`layout_changed` check in `App::save_settings` so the user is reminded to
re-typeset.

**Change the look.** Edit `assets/topebag.typ`, rebuild, and run
`topebag retypeset <id>`. Then render the pages with
`cargo run --example render_pages -- <entry-folder> <out-folder>` to
inspect them as PNGs.

**Support a new kind of content** (for example links that are already PDFs).
Teach `fetch::fetch_page` to return the content type, then branch in
`pipeline::add_url` before extraction.

**Add a print backend.** Implement `printers`, `default_printer` and `print`
in a new `print/` submodule and select it with `cfg` in `print/mod.rs`.

## Portability (Windows and macOS)

Everything platform-specific is behind `cfg` in a few places:

| Concern | Windows | macOS (and Linux) |
|---|---|---|
| Printing | `print/windows.rs`: GDI, rasterised pages | `print/cups.rs`: the PDF goes to `lp`; `lpstat` lists printers |
| Start at sign-in | Registry `Run` value | LaunchAgent plist (Linux: not supported) |
| Tray icon | `tray-icon` (a Windows-only dependency) | None yet, so closing the window quits |
| System fonts (PDFs) | Segoe UI, Georgia, Consolas, symbols, CJK | Helvetica Neue, Georgia, Menlo, Apple Symbols, Hiragino, Apple SD Gothic Neo, Arial Unicode |
| UI fonts | Segoe UI | Helvetica Neue (faces picked from the `.ttc` by weight) and Apple Symbols |
| Paper default | Locale API | `LC_PAPER`/`LANG`, else the macOS `AppleLocale` setting |
| Browser for JS pages | Edge, Chrome under Program Files | Edge, Chrome in `/Applications` |
| Data folder | `%LOCALAPPDATA%\Topebag` | `~/Library/Application Support/Topebag` |
| Shortcut labels | Ctrl | `keys::display` turns Ctrl+ into ⌘ (egui's COMMAND modifier is ⌘ on a Mac) |

The template lists Windows and macOS families side by side (for example
`("Segoe UI", "Helvetica Neue", "Arial", …)`), and Typst uses the first one
installed. Its "unknown font family" warnings for the others are filtered
out before they reach the UI. Body text and code use fonts compiled in from
`typst-assets`, so documents look the same on both systems.

Code that only runs on a Mac can't be type-checked from Windows. So the Mac
logic lives in ordinary functions compiled everywhere (`launch_agent::*`,
`add_mac_fonts`, `face_index`, `Paper::for_locale`, `keys::display`) and is
unit-tested on every platform. Only one-line dispatches sit behind
`#[cfg(...)]`. `.github/workflows/ci.yml` then builds and runs the whole test
suite on `macos-latest` and `windows-latest` for every push.

## Testing

- **Unit tests** sit next to the code (`cargo test`). They cover escaping,
  whitespace, lists, tables, links, images, code blocks, charset decoding,
  URL parsing, meta refresh, title cleanup, settings round-trips, library
  operations and path-traversal checks, Typst compilation (PDF and raster),
  error reporting, and DIB conversion.
- **Integration tests** (`tests/pipeline.rs`) start a local HTTP server
  that serves a realistic article fixture with navigation, ads, a cookie
  banner, an image, a table and code. They run the full add → library →
  preview → re-typeset → delete cycle, plus a 404 case. They also cover a
  generated two-page **PDF link**, an **Instapaper-style feed** driving the
  queue, **tags and full-text search** on a saved article, a page built
  entirely by **JavaScript** (headless-browser fallback), and a site that
  returns **403 to robots** but serves browsers. The browser tests skip
  themselves if Edge/Chrome is missing or can't run in the environment.
- **Editing tests** cover block splitting through wrappers, the Markdown
  round trip (inline formatting and escaping), moving and restoring drafts,
  and an end-to-end flow: add → drop blocks → rewrite and move a heading →
  rename → rebuild (checking the Typst, the library title and the search
  text) → refill missing images → per-document grayscale → restore original.
- **Edition tests** check that three stories in columns take fewer pages than
  printed separately and still compile in one and two columns; that a PDF is
  skipped; that the stories keep the picked order and their images are copied
  in; that no figure floats; preview and search text; staleness after an
  article edit; reordering, leaving out and renaming; the CLI re-typeset
  path; a deleted article turning into a note; and that a byline repeated in
  the body is dropped (but a sentence starting with "By" isn't).
- **Section and keyboard tests** cover section ranges, depths, inclusion
  states and section moves (past siblings, out of a parent, at the edges);
  library moves at the ends and with nothing open; ranges in either
  direction; and key decoding: Shift before plain, Ctrl shortcuts while
  typing, and plain keys left alone while typing, with a widget focused, or
  with a window or dialog open.
- **Spread tests** check that side-by-side sheets are landscape, that the
  optimizer reaches the compact sheet count with near-minimal tightening
  (and leaves one-sheet articles alone), that re-typesetting switches between
  spreads and portrait, and that landscape rasters rotate the right way.
- **Instance and autostart tests** check the hand-off between two launches,
  rejection of a wrong token, and the registry helpers (on a throwaway
  `HKCU\Software\TopebagTest` key that's removed afterwards, never the
  real startup list).
- **The real spooler test** prints through *Microsoft Print to PDF* to a
  file. It's `#[ignore]`d by default because, when Windows manages the
  default printer, printing makes that printer the new default. Run it with
  `cargo test -- --ignored`.

[`dom_smoothie`]: https://crates.io/crates/dom_smoothie
