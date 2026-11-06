//! The desktop app (egui / eframe).
//!
//! The flow is **link → edit → format → preview → print**:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │ Topebag   [ paste a link…                       ] [Fetch] Instapaper ⚙│
//! ├────────────┬──────────────────────────────┬──────────────────────────┤
//! │ Library    │ Title · source · tags                                   │
//! │ [search]   │ [✎ Edit] [Aa Format]          │ Preview      2 sheets    │
//! │ #tag       │ Title  [               ]      │ ┌──────┬──────┐          │
//! │ ▸ doc      │ [x] H2  Section heading  ↑↓✎  │ │page 1│page 2│          │
//! │ ▸ doc      │ [x] ¶   Paragraph…       ↑↓✎  │ └──────┴──────┘          │
//! │            │ [ ] ¶   Subscribe now…   ↑↓✎  │ 🖨 Brother ▾  ×1 [Print]  │
//! ├────────────┴──────────────────────────────┴──────────────────────────┤
//! │ Activity                                                              │
//! └──────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Select several articles (Ctrl+click, Shift+click, or **Select**) and
//! **Combine into an edition** to print them together like a newspaper: the
//! stories run on through shared columns instead of each starting a fresh
//! page. An edition opens in the same workspace, with a **Stories** tab
//! (nameplate, order, which stories print) instead of the block editor.
//!
//! Everything is reachable from the keyboard too: ↑ ↓ (or J K) move through
//! the library, Space and Shift+arrows tick documents, Ctrl+E combines them,
//! and F1 lists every shortcut. The keys are decoded in [`keys`].
//!
//! Pasting a link creates a document and opens it. Every edit or format
//! change marks the document *dirty*; after a short pause the draft is sent
//! to the worker's rebuild thread, and the preview updates when the fresh
//! pages arrive. Nothing is printed until you click **Print**.
//!
//! All slow work goes through [`worker::Worker`]; this module keeps UI state
//! and reacts to [`worker::Event`]s. Event handling, timers, tray actions and
//! messages from later launches are processed in `App::logic`, which eframe
//! also runs while the window is hidden in the tray.

mod keys;
mod theme;
mod tray;
mod worker;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use eframe::egui::{
    self, Align, Color32, Frame, Layout, Margin, RichText, Sense, Stroke, TextureHandle, Ui, ViewportCommand,
};
use topebag::document::{BlockKind, Draft, Inclusion};
use topebag::edition::{self, Edition};
use topebag::instapaper::{self, FeedItem, QueueState};
use topebag::instance::{self, Message, Role};
use topebag::library::{self, Entry, EntryKind, Library};
use topebag::print::PrinterInfo;
use topebag::search::{SearchIndex, parse_query};
use topebag::settings::{BrowserMode, Format, LinkStyle, Paper, PrintQuality, Settings, Typeface};
use topebag::{autostart, browser, fetch, pipeline};

use self::keys::Command;
use self::theme::{ACCENT, ACCENT_DARK, CARD, CARD_SELECTED, DANGER, HAIRLINE, MUTED, SUCCESS};
use self::tray::TrayAction;
use self::worker::{Event, JobId, Rebuild, Request, Source, Worker};

/// Pause after the last edit before the preview is rebuilt.
const REBUILD_DELAY: Duration = Duration::from_millis(450);
/// Pause on a newly opened document before rendering its preview, so holding
/// ↓ through the library doesn't start a render for every entry passed.
const PREVIEW_DELAY: Duration = Duration::from_millis(150);

/// Launch the desktop app, queueing `initial_urls`. With `background`, it
/// starts hidden in the system tray. If Topebag is already running, the
/// links are handed to that instance instead. Blocks until the app quits.
pub fn run(initial_urls: Vec<String>, background: bool) -> anyhow::Result<()> {
    let data_dir = library::data_dir();

    let mut messages: Vec<Message> = initial_urls.iter().cloned().map(Message::Add).collect();
    if !background {
        messages.insert(0, Message::Show);
    }
    let primary = match instance::claim(&data_dir, &messages) {
        Ok(Role::Secondary) => return Ok(()),
        Ok(Role::Primary(primary)) => Some(primary),
        Err(err) => {
            log::warn!("single-instance check failed, continuing: {err:#}");
            None
        }
    };

    let library = Library::open(&data_dir)?;
    let (settings, settings_error) = match Settings::load(&library::settings_path(&data_dir)) {
        Ok(s) => (s, None),
        Err(e) => (Settings::default(), Some(format!("Settings could not be read and were reset: {e:#}"))),
    };
    topebag::typeset::warm_up();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Topebag")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([960.0, 560.0])
            .with_visible(!background)
            .with_icon(theme::icon()),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Topebag",
        options,
        Box::new(move |cc| {
            let (instance_tx, instance_rx) = channel();
            if let Some(primary) = primary {
                let ctx = cc.egui_ctx.clone();
                primary.listen(move |message| {
                    let _ = instance_tx.send(message);
                    ctx.request_repaint();
                });
            }
            let mut app = App::new(cc, data_dir.clone(), library, settings, settings_error, instance_rx, background);
            if !initial_urls.is_empty() {
                app.url_input = initial_urls.join("\n");
                app.submit_urls();
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("{e}"));
    instance::release(&library::data_dir());
    result
}

#[derive(Clone, PartialEq)]
enum JobStatus {
    Queued,
    Running(String),
    Done(String),
    Failed(String),
}

struct Job {
    id: JobId,
    label: String,
    status: JobStatus,
    started: Instant,
    /// Entry this job concerns (once known), for "show" links.
    entry_id: Option<String>,
}

enum Preview {
    Loading,
    Ready(Vec<TextureHandle>),
    Failed(String),
}

struct Toast {
    text: String,
    kind: ToastKind,
    shown: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum ToastKind {
    Info,
    Success,
    Error,
}

/// Instapaper queue state held by the UI.
#[derive(Default)]
struct Queue {
    items: Vec<FeedItem>,
    state: QueueState,
    checking: bool,
    last_attempt: Option<Instant>,
    error: Option<String>,
    /// Items the user un-ticked in the queue window.
    unticked: HashSet<String>,
    show_window: bool,
    feed_input: String,
}

/// Edit buffers for the settings window.
struct SettingsDraft {
    settings: Settings,
    sites: String,
    feed: String,
}

#[derive(Clone, Copy, PartialEq)]
enum EditorTab {
    Edit,
    Format,
}

/// The document open in the editor.
struct Editor {
    id: String,
    /// The article's draft; `None` for PDFs (they print as they are) and
    /// editions.
    draft: Option<Draft>,
    /// Set when the open document is an edition.
    edition: Option<Edition>,
    /// An edition whose stories were edited after it was laid out.
    stale: bool,
    tab: EditorTab,
    /// Block being rewritten: (index, Markdown buffer).
    editing: Option<(usize, String)>,
    /// Set by every change; the preview rebuilds once edits pause.
    dirty_since: Option<Instant>,
    rebuilding: bool,
    /// The next rebuild also downloads images that are missing.
    refill_next: bool,
    error: Option<String>,
}

impl Editor {
    fn busy(&self) -> bool {
        self.rebuilding || self.dirty_since.is_some()
    }

    /// What the worker should rebuild this document from.
    fn source(&self, refill_images: bool) -> Option<Source> {
        if let Some(edition) = &self.edition {
            return Some(Source::Edition(edition.clone()));
        }
        self.draft.clone().map(|draft| Source::Draft { draft, refill_images })
    }
}

/// Something done to a story in the edition editor.
enum StoryAction {
    Toggle(usize),
    Up(usize),
    Down(usize),
    Remove(usize),
    Open(String),
}

/// Something done to a block in the editor list.
enum BlockAction {
    Toggle(usize),
    /// A heading's checkbox: print (true) or leave out its whole section.
    ToggleSection(usize, bool),
    /// Fold or unfold the blocks under a heading.
    Fold(usize),
    Up(usize),
    Down(usize),
    StartEdit(usize),
    SaveEdit,
    CancelEdit,
    RevertEdit(usize),
}

struct App {
    data_dir: PathBuf,
    library: Library,
    settings: Settings,
    settings_draft: Option<SettingsDraft>,
    entries: Vec<Entry>,
    selected: Option<String>,
    /// Articles ticked in the library for combining into an edition, in the
    /// order they were picked.
    picked: Vec<String>,
    /// The library shows checkboxes and a click ticks instead of opening.
    select_mode: bool,
    editor: Option<Editor>,
    /// Folded headings (block ids) per document, kept while the app runs.
    folds: HashMap<String, HashSet<u32>>,
    search: String,
    /// Put the cursor in the search field next frame (Ctrl+F, /).
    focus_search: bool,
    /// Scroll the open document's card into view next frame (after a
    /// keyboard move).
    reveal_selected: bool,
    /// A document opened recently whose preview hasn't been started yet.
    preview_due: Option<(String, Instant)>,
    /// A text field had keyboard focus last frame (Esc leaving a field
    /// shouldn't also untick the library).
    was_typing: bool,
    show_shortcuts: bool,
    /// A confirmation dialog has been on screen for a frame, so Enter now
    /// means "confirm" (not the key press that opened it).
    dialog_ready: bool,
    tag_filter: Option<String>,
    index: SearchIndex,
    new_tag: String,
    url_input: String,
    focus_url: bool,
    printers: Vec<PrinterInfo>,
    jobs: Vec<Job>,
    next_job: JobId,
    worker: Worker,
    previews: HashMap<String, Preview>,
    toasts: Vec<Toast>,
    confirm_delete: Option<String>,
    confirm_restore: bool,
    queue: Queue,
    tray: Option<tray::Tray>,
    tray_rx: Receiver<TrayAction>,
    instance_rx: Receiver<Message>,
    hidden: bool,
    quitting: bool,
    /// Frames left before hiding the window when started with --background
    /// (eframe always shows the window after its first frame).
    hide_in_frames: u8,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        data_dir: PathBuf,
        library: Library,
        settings: Settings,
        settings_error: Option<String>,
        instance_rx: Receiver<Message>,
        background: bool,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        let worker = Worker::spawn(cc.egui_ctx.clone(), library.clone());
        worker.send(Request::RefreshPrinters);
        worker.start_heartbeat(Duration::from_secs(30));

        let (tray_tx, tray_rx) = channel();
        let tray = match tray::create(cc.egui_ctx.clone(), tray_tx) {
            Ok(tray) => Some(tray),
            Err(err) => {
                log::warn!("no tray icon: {err:#}");
                None
            }
        };
        if background && tray.is_none() {
            // Without a tray icon a hidden window would be unreachable.
            cc.egui_ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        }

        let queue = Queue { state: QueueState::load(&instapaper::state_path(&data_dir)), ..Queue::default() };
        let mut app = App {
            data_dir,
            library,
            settings,
            settings_draft: None,
            entries: Vec::new(),
            selected: None,
            picked: Vec::new(),
            select_mode: false,
            editor: None,
            folds: HashMap::new(),
            search: String::new(),
            focus_search: false,
            reveal_selected: false,
            preview_due: None,
            was_typing: false,
            show_shortcuts: false,
            dialog_ready: false,
            tag_filter: None,
            index: SearchIndex::default(),
            new_tag: String::new(),
            url_input: String::new(),
            focus_url: true,
            printers: Vec::new(),
            jobs: Vec::new(),
            next_job: 1,
            worker,
            previews: HashMap::new(),
            toasts: Vec::new(),
            confirm_delete: None,
            confirm_restore: false,
            queue,
            hidden: background && tray.is_some(),
            tray,
            tray_rx,
            instance_rx,
            quitting: false,
            hide_in_frames: 0,
        };
        if app.hidden {
            app.hide_in_frames = 2;
        }
        app.reload_entries();
        app.worker.build_index(app.entries.clone());
        if let Some(first) = app.entries.first().map(|e| e.id.clone()) {
            app.select(first);
        }
        if let Some(err) = settings_error {
            app.toast(err, ToastKind::Error);
        }
        app
    }

    // ----- state helpers ---------------------------------------------------

    fn reload_entries(&mut self) {
        match self.library.entries() {
            Ok(entries) => self.entries = entries,
            Err(err) => self.toast(format!("Could not read the library: {err:#}"), ToastKind::Error),
        }
    }

    /// Open a document in the editor.
    fn select(&mut self, id: String) {
        if self.editor.as_ref().is_some_and(|e| e.id == id) {
            return;
        }
        self.flush_pending_edits();
        if !self.previews.contains_key(&id) {
            self.preview_due = Some((id.clone(), Instant::now()));
        }
        let kind = self.entries.iter().find(|e| e.id == id).map(|e| e.kind).unwrap_or_default();
        let (mut edition_state, mut stale) = (None, false);
        let (draft, error) = if kind == EntryKind::Edition {
            match edition::load(&self.library, &id) {
                Ok(e) => {
                    stale = edition::is_stale(&self.library, &id, &e);
                    edition_state = Some(e);
                    (None, None)
                }
                Err(err) => (None, Some(format!("This edition can't be opened: {err:#}"))),
            }
        } else {
            match pipeline::load_draft(&self.library, &id, &self.settings.format) {
                Ok(draft) => (draft, None),
                Err(err) => (None, Some(format!("This document can't be edited: {err:#}"))),
            }
        };
        self.editor = Some(Editor {
            id: id.clone(),
            draft,
            edition: edition_state,
            stale,
            tab: EditorTab::Edit,
            editing: None,
            dirty_since: None,
            rebuilding: false,
            refill_next: true,
            error,
        });
        self.selected = Some(id);
        self.new_tag.clear();
        self.confirm_restore = false;
    }

    /// Start the open document's preview once it has been open a moment.
    fn start_due_preview(&mut self, ctx: &egui::Context) {
        let Some((id, since)) = self.preview_due.clone() else { return };
        if self.selected.as_ref() != Some(&id) {
            self.preview_due = None;
            return;
        }
        if since.elapsed() < PREVIEW_DELAY {
            ctx.request_repaint_after(PREVIEW_DELAY - since.elapsed());
            return;
        }
        self.preview_due = None;
        if !self.previews.contains_key(&id) {
            self.previews.insert(id.clone(), Preview::Loading);
            self.worker.preview(id);
        }
    }

    /// The library entries shown under the current search and tag filter, as
    /// (index into `entries`, search snippet).
    fn visible(&self) -> Vec<(usize, Option<String>)> {
        let terms = parse_query(&self.search);
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.tag_filter.as_ref().is_none_or(|t| e.tags.contains(t)))
            .filter_map(|(i, e)| self.index.matches(e, &terms).map(|hit| (i, hit.snippet)))
            .collect()
    }

    fn visible_ids(&self) -> Vec<String> {
        self.visible().into_iter().map(|(i, _)| self.entries[i].id.clone()).collect()
    }

    /// Arrow keys and friends: open another document. With `extend`, also
    /// tick every document passed over, starting with the one you were on.
    fn move_cursor(&mut self, step: keys::Move, count: usize, extend: bool) {
        let visible = self.visible_ids();
        let from = self.selected.as_deref().and_then(|s| visible.iter().position(|v| v == s));
        let Some(to) = keys::target(&visible, self.selected.as_deref(), step, count) else { return };
        if extend {
            for id in keys::passed_over(&visible, from.unwrap_or(to), to) {
                if !self.picked.contains(&id) {
                    self.picked.push(id);
                }
            }
        }
        self.reveal_selected = true;
        self.select(visible[to].clone());
    }

    /// Rebuild the open document's preview now instead of after a pause.
    fn refresh_preview(&mut self) {
        let Some(editor) = self.editor.as_mut() else { return };
        if editor.draft.is_some() || editor.edition.is_some() {
            editor.dirty_since = Some(Instant::now().checked_sub(REBUILD_DELAY).unwrap_or_else(Instant::now));
        } else {
            let id = editor.id.clone();
            self.previews.insert(id.clone(), Preview::Loading);
            self.worker.preview(id);
        }
    }

    fn open_pdf(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        if let Ok(path) = self.library.pdf_path(&id) {
            if let Err(err) = opener::open(&path) {
                self.toast(format!("Could not open the PDF: {err}"), ToastKind::Error);
            }
        }
    }

    /// Send any unsaved edits of the open document before switching away.
    fn flush_pending_edits(&mut self) {
        if let Some(editor) = self.editor.as_mut() {
            if let Some((index, buffer)) = editor.editing.take() {
                if let Some(block) = editor.draft.as_mut().and_then(|d| d.blocks.get_mut(index)) {
                    if buffer != block.markdown() {
                        block.edited = Some(buffer);
                        editor.dirty_since = Some(Instant::now());
                    }
                }
            }
            if editor.dirty_since.is_some() {
                if let Some(source) = editor.source(false) {
                    self.worker.rebuild(Rebuild { id: editor.id.clone(), source });
                }
            }
        }
    }

    fn mark_dirty(&mut self) {
        if let Some(editor) = self.editor.as_mut() {
            editor.dirty_since = Some(Instant::now());
        }
    }

    /// Start a rebuild once edits have paused.
    fn maybe_rebuild(&mut self, ctx: &egui::Context) {
        let Some(editor) = self.editor.as_mut() else { return };
        let Some(since) = editor.dirty_since else { return };
        if editor.rebuilding {
            return;
        }
        if since.elapsed() < REBUILD_DELAY {
            ctx.request_repaint_after(REBUILD_DELAY - since.elapsed());
            return;
        }
        if let Some(source) = editor.source(editor.refill_next) {
            editor.refill_next = false;
            editor.dirty_since = None;
            editor.rebuilding = true;
            editor.stale = false;
            self.worker.rebuild(Rebuild { id: editor.id.clone(), source });
        } else {
            editor.dirty_since = None;
        }
    }

    fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toasts.push(Toast { text: text.into(), kind, shown: Instant::now() });
    }

    fn new_job(&mut self, label: String, entry_id: Option<String>) -> JobId {
        let id = self.next_job;
        self.next_job += 1;
        self.jobs.push(Job { id, label, status: JobStatus::Queued, started: Instant::now(), entry_id });
        id
    }

    fn job_mut(&mut self, id: JobId) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    fn save_settings_quietly(&mut self) {
        if let Err(err) = self.settings.save(&library::settings_path(&self.data_dir)) {
            self.toast(format!("Could not save settings: {err:#}"), ToastKind::Error);
        }
    }

    /// Queue every link found in `self.url_input`.
    fn submit_urls(&mut self) {
        let urls = fetch::urls_in_text(&self.url_input);
        if urls.is_empty() {
            if !self.url_input.trim().is_empty() {
                self.toast("That doesn't look like a web address.", ToastKind::Error);
            }
            return;
        }
        for url in urls {
            self.queue_url(url, None);
        }
        self.url_input.clear();
        self.focus_url = true;
    }

    fn queue_url(&mut self, url: url::Url, label: Option<String>) {
        let job = self.new_job(label.unwrap_or_else(|| url.to_string()), None);
        self.worker.send(Request::Add { job, url, settings: self.settings.clone() });
    }

    fn print_selected(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        if self.editor.as_ref().is_some_and(|e| e.id == id && e.busy()) {
            self.toast("Wait for the preview to finish updating, then print.", ToastKind::Info);
            return;
        }
        let title = self.entries.iter().find(|e| e.id == id).map(|e| e.title.clone()).unwrap_or_else(|| id.clone());
        let job = self.new_job(format!("Print: {title}"), Some(id.clone()));
        self.worker.send(Request::Print { job, id, settings: self.settings.clone() });
    }

    /// Combine the picked articles into a new edition.
    fn create_edition(&mut self) {
        let ids = self.picked_articles();
        if ids.is_empty() {
            self.toast("Pick some web articles first. PDFs can't flow into columns.", ToastKind::Error);
            return;
        }
        let label = format!("Edition of {} articles", ids.len());
        let job = self.new_job(label, None);
        self.worker.send(Request::CreateEdition { job, ids, settings: self.settings.clone() });
        self.picked.clear();
        self.select_mode = false;
    }

    /// Add the picked articles to the edition that's open.
    fn add_to_open_edition(&mut self) {
        let ids = self.picked_articles();
        let Some(edition) = self.editor.as_mut().and_then(|e| e.edition.as_mut()) else { return };
        let mut added = 0;
        for id in ids {
            if !edition.members.iter().any(|m| m.id == id) {
                edition.members.push(edition::Member { id, included: true });
                added += 1;
            }
        }
        self.picked.clear();
        self.select_mode = false;
        if added > 0 {
            self.mark_dirty();
            self.toast(format!("Added {added} stor{} to the edition.", if added == 1 { "y" } else { "ies" }), ToastKind::Success);
        } else {
            self.toast("Those articles are already in this edition.", ToastKind::Info);
        }
    }

    /// Picked entries that can join an edition, in pick order.
    fn picked_articles(&self) -> Vec<String> {
        self.picked
            .iter()
            .filter(|id| self.entries.iter().any(|e| &e.id == *id && edition::can_join(e)))
            .cloned()
            .collect()
    }

    /// Ctrl+click: tick or untick one entry. Starting a selection this way
    /// also ticks the document that's open, like a file manager.
    fn toggle_pick(&mut self, id: String) {
        if self.picked.is_empty() && !self.select_mode {
            if let Some(open) = self.selected.clone().filter(|s| *s != id) {
                self.picked.push(open);
            }
        }
        if let Some(pos) = self.picked.iter().position(|p| *p == id) {
            self.picked.remove(pos);
        } else {
            self.picked.push(id);
        }
    }

    /// Shift+click: tick everything between the last ticked (or open) entry
    /// and this one, in the order the library shows them.
    fn pick_range(&mut self, visible: &[String], id: String) {
        let anchor = self.picked.last().cloned().or_else(|| self.selected.clone());
        let (Some(a), Some(b)) = (
            anchor.as_ref().and_then(|a| visible.iter().position(|v| v == a)),
            visible.iter().position(|v| *v == id),
        ) else {
            self.toggle_pick(id);
            return;
        };
        let range: Vec<String> = if a <= b { visible[a..=b].to_vec() } else { visible[b..=a].iter().rev().cloned().collect() };
        for v in range {
            if !self.picked.contains(&v) {
                self.picked.push(v);
            }
        }
    }

    fn update_entry(&mut self, entry: Entry) {
        if let Err(err) = self.library.save(&entry) {
            self.toast(format!("Could not save: {err:#}"), ToastKind::Error);
            return;
        }
        if let Some(slot) = self.entries.iter_mut().find(|e| e.id == entry.id) {
            *slot = entry;
        }
    }

    fn open_settings(&mut self) {
        let mut settings = self.settings.clone();
        settings.start_with_windows = autostart::is_enabled();
        self.settings_draft = Some(SettingsDraft {
            sites: settings.browser_sites.join("\n"),
            feed: settings.instapaper_feed.clone().unwrap_or_default(),
            settings,
        });
    }

    fn save_settings(&mut self, draft: SettingsDraft) {
        let mut new = draft.settings;
        new.browser_sites = draft.sites.lines().map(str::to_string).collect();
        let feed = draft.feed.trim();
        if feed.is_empty() {
            new.instapaper_feed = None;
        } else {
            match instapaper::parse_feed_url(feed) {
                Ok(url) => new.instapaper_feed = Some(url.to_string()),
                Err(err) => {
                    self.toast(format!("Instapaper feed: {err:#}"), ToastKind::Error);
                    new.instapaper_feed = self.settings.instapaper_feed.clone();
                }
            }
        }
        new.sanitize();

        if new.start_with_windows != autostart::is_enabled() {
            if let Err(err) = autostart::set_enabled(new.start_with_windows) {
                self.toast(format!("Could not change “{}”: {err:#}", autostart::LABEL), ToastKind::Error);
                new.start_with_windows = autostart::is_enabled();
            }
        }

        let defaults_changed = new.format != self.settings.format;
        let feed_changed = new.instapaper_feed != self.settings.instapaper_feed;
        self.settings = new;
        match self.settings.save(&library::settings_path(&self.data_dir)) {
            Ok(()) if defaults_changed => self.toast(
                "Settings saved. New documents use these defaults; each saved document keeps its own formatting.",
                ToastKind::Info,
            ),
            Ok(()) => self.toast("Settings saved.", ToastKind::Success),
            Err(err) => self.toast(format!("Could not save settings: {err:#}"), ToastKind::Error),
        }
        if feed_changed {
            self.queue.items.clear();
            self.check_feed();
        }
    }

    // ----- Instapaper ------------------------------------------------------

    fn check_feed(&mut self) {
        let Some(feed) = self.settings.instapaper_feed.clone() else { return };
        let Ok(url) = url::Url::parse(&feed) else { return };
        if self.queue.checking {
            return;
        }
        self.queue.checking = true;
        self.queue.last_attempt = Some(Instant::now());
        self.worker.send(Request::CheckFeed { url });
    }

    fn feed_due(&self) -> bool {
        if self.settings.instapaper_feed.is_none() || self.queue.checking {
            return false;
        }
        let interval = Duration::from_secs(u64::from(self.settings.instapaper_interval_min) * 60);
        self.queue.last_attempt.is_none_or(|t| t.elapsed() >= interval)
    }

    fn save_queue_state(&mut self) {
        if let Err(err) = self.queue.state.save(&instapaper::state_path(&self.data_dir)) {
            log::warn!("could not save Instapaper state: {err:#}");
        }
    }

    fn unfetched(&self) -> Vec<&FeedItem> {
        self.queue.state.unseen(&self.queue.items)
    }

    fn fetch_queue_items(&mut self, items: Vec<FeedItem>) {
        for item in &items {
            if let Ok(url) = fetch::parse_url(&item.url) {
                self.queue_url(url, Some(item.title.clone()));
            }
        }
        self.queue.state.mark_seen(items.iter().map(|i| i.id.as_str()));
        self.save_queue_state();
    }

    // ----- events and background work (runs even while hidden) -------------

    fn process_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.worker.events.try_recv() {
            match event {
                Event::Progress { job, stage } => {
                    if let Some(j) = self.job_mut(job) {
                        j.status = JobStatus::Running(stage);
                    }
                }
                Event::Added { job, entry } => {
                    let title = entry.title.clone();
                    let id = entry.id.clone();
                    let size = if entry.spread {
                        format!("{} sheet(s)", entry.pages)
                    } else {
                        format!("{} page(s)", entry.pages)
                    };
                    let edition = entry.kind == EntryKind::Edition;
                    if let Some(j) = self.job_mut(job) {
                        j.status = JobStatus::Done(if edition {
                            format!("Edition ready · {size}")
                        } else {
                            format!("Ready to edit · {size}")
                        });
                        j.label = title.clone();
                        j.entry_id = Some(id.clone());
                    }
                    if edition {
                        let stories = entry.site_name.clone().unwrap_or_default();
                        self.toast(format!("Edition ready: {stories} on {size}. Check the preview, then print"), ToastKind::Success);
                    } else {
                        self.toast(format!("“{title}” is ready: edit, format, then print"), ToastKind::Success);
                    }
                    self.index.add(&self.library, &entry);
                    self.reload_entries();
                    self.select(id);
                }
                Event::Printed { job, record } => {
                    if let Some(j) = self.job_mut(job) {
                        j.status = JobStatus::Done(format!("Printed {} page(s) on {}", record.pages, record.printer));
                    }
                    self.toast(format!("Sent to {}", record.printer), ToastKind::Success);
                    self.reload_entries();
                }
                Event::Failed { job, error } => {
                    let first_line = error.lines().next().unwrap_or("").to_string();
                    if let Some(j) = self.job_mut(job) {
                        j.status = JobStatus::Failed(error);
                    }
                    self.toast(first_line, ToastKind::Error);
                }
                Event::Printers(Ok(list)) => self.printers = list,
                Event::Printers(Err(err)) => self.toast(format!("Could not list printers: {err}"), ToastKind::Error),
                Event::Preview { id, result } => {
                    // A rebuild's pages are newer than a plain preview.
                    if matches!(self.previews.get(&id), Some(Preview::Ready(_)))
                        && self.editor.as_ref().is_some_and(|e| e.id == id && e.rebuilding)
                    {
                        continue;
                    }
                    let preview = to_preview(ctx, &id, result);
                    self.previews.insert(id, preview);
                }
                Event::Rebuilt { id, result } => {
                    let is_open = self.editor.as_ref().is_some_and(|e| e.id == id);
                    match result {
                        Ok((entry, pages)) => {
                            self.index.add(&self.library, &entry);
                            if let Some(slot) = self.entries.iter_mut().find(|e| e.id == entry.id) {
                                *slot = *entry;
                            }
                            self.previews.insert(id.clone(), to_preview(ctx, &id, Ok(pages)));
                            if let Some(editor) = self.editor.as_mut().filter(|_| is_open) {
                                editor.rebuilding = false;
                                editor.error = None;
                            }
                        }
                        Err(err) => {
                            if let Some(editor) = self.editor.as_mut().filter(|_| is_open) {
                                editor.rebuilding = false;
                                editor.error = Some(err);
                            } else {
                                self.toast(format!("Could not update a document: {err}"), ToastKind::Error);
                            }
                        }
                    }
                }
                Event::Feed(result) => {
                    self.queue.checking = false;
                    match result {
                        Ok(items) => {
                            self.queue.error = None;
                            let fresh = self.queue.state.observe(&items);
                            self.queue.items = items;
                            self.save_queue_state();
                            if self.settings.instapaper_auto && !fresh.is_empty() {
                                self.toast(format!("Fetching {} new Instapaper save(s)", fresh.len()), ToastKind::Info);
                                self.fetch_queue_items(fresh);
                            }
                        }
                        Err(err) => self.queue.error = Some(err),
                    }
                }
                Event::Index(index) => {
                    // Keep anything added while the index was being built.
                    let mut index = *index;
                    for entry in &self.entries {
                        index.add(&self.library, entry);
                    }
                    self.index = index;
                }
            }
        }
    }

    fn process_tray_and_instance(&mut self, ctx: &egui::Context) {
        while let Ok(action) = self.tray_rx.try_recv() {
            match action {
                TrayAction::Open => self.show_window(ctx),
                TrayAction::PrintClipboard => {
                    // Opens the link for editing; printing is still your call.
                    let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
                    let urls = fetch::urls_in_text(&text);
                    if urls.is_empty() {
                        self.toast("There is no link on the clipboard.", ToastKind::Error);
                    }
                    for url in urls {
                        self.queue_url(url, None);
                    }
                    self.show_window(ctx);
                }
                TrayAction::CheckInstapaper => {
                    if self.settings.instapaper_feed.is_none() {
                        self.queue.show_window = true;
                        self.show_window(ctx);
                    } else {
                        self.check_feed();
                    }
                }
                TrayAction::Quit => {
                    self.flush_pending_edits();
                    self.quitting = true;
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
        while let Ok(message) = self.instance_rx.try_recv() {
            log::info!("message from another launch: {message:?}");
            match message {
                Message::Show => self.show_window(ctx),
                Message::Add(text) => {
                    for url in fetch::urls_in_text(&text) {
                        self.queue_url(url, None);
                    }
                }
            }
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    /// Closing the window hides it to the tray (unless quitting or the
    /// setting is off). Unsaved edits are sent off first either way.
    fn handle_close(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if !close_requested {
            return;
        }
        self.flush_pending_edits();
        if !self.quitting && self.settings.close_to_tray && self.tray.is_some() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.hidden = true;
        }
    }

    /// Ctrl+V anywhere (while no text field is focused) queues the pasted links.
    fn handle_global_paste(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let pasted: Vec<String> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Paste(text) => Some(text.clone()),
                    _ => None,
                })
                .collect()
        });
        for text in pasted {
            self.url_input = text;
            self.submit_urls();
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.text_edit_focused() || self.was_typing;
        let cx = keys::KeyContext {
            typing,
            widget_focused: !typing && ctx.memory(|m| m.focused().is_some()),
            dialog_open: self.confirm_delete.is_some() || self.confirm_restore,
            window_open: self.show_shortcuts || self.settings_draft.is_some() || self.queue.show_window,
        };
        for command in keys::read(ctx, cx) {
            self.run_command(command);
        }
    }

    fn run_command(&mut self, command: Command) {
        match command {
            Command::Move(step, count) => self.move_cursor(step, count, false),
            Command::Extend(step, count) => self.move_cursor(step, count, true),
            Command::TogglePick => {
                if let Some(id) = self.selected.clone() {
                    match self.picked.iter().position(|p| *p == id) {
                        Some(pos) => {
                            self.picked.remove(pos);
                        }
                        None => self.picked.push(id),
                    }
                }
            }
            Command::PickAll => {
                let visible = self.visible_ids();
                if !visible.is_empty() && visible.iter().all(|id| self.picked.contains(id)) {
                    self.picked.clear();
                } else {
                    for id in visible {
                        if !self.picked.contains(&id) {
                            self.picked.push(id);
                        }
                    }
                }
            }
            Command::Escape => {
                if self.show_shortcuts {
                    self.show_shortcuts = false;
                } else if self.settings_draft.is_some() {
                    self.settings_draft = None;
                } else if self.queue.show_window {
                    self.queue.show_window = false;
                } else {
                    self.picked.clear();
                    self.select_mode = false;
                }
            }
            Command::CombineEdition => {
                if self.picked.is_empty() {
                    self.toast(
                        keys::display("Tick some articles first: Space ticks the open one, Shift+↑ ↓ ticks a run, Ctrl+A ticks them all."),
                        ToastKind::Info,
                    );
                } else {
                    self.create_edition();
                }
            }
            Command::Delete => self.confirm_delete = self.selected.clone(),
            Command::EditTab | Command::FormatTab => {
                if let Some(editor) = self.editor.as_mut() {
                    editor.tab = if command == Command::EditTab { EditorTab::Edit } else { EditorTab::Format };
                }
            }
            Command::Refresh => self.refresh_preview(),
            Command::Print => self.print_selected(),
            Command::OpenPdf => self.open_pdf(),
            Command::FocusLink => self.focus_url = true,
            Command::FocusSearch => self.focus_search = true,
            Command::Instapaper => {
                self.queue.show_window = true;
                if self.queue.items.is_empty() {
                    self.check_feed();
                }
            }
            Command::Settings => {
                if self.settings_draft.is_none() {
                    self.open_settings();
                }
            }
            Command::Help => self.show_shortcuts = !self.show_shortcuts,
        }
    }

    // ----- UI: toolbar and library ------------------------------------------

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Topebag").family(theme::bold_family()).size(22.0).color(ACCENT));
            ui.label(RichText::new("links in, paper out").color(MUTED).italics());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⚙ Settings").on_hover_text(keys::display("Settings (Ctrl+,)")).clicked() {
                    self.open_settings();
                }
                if ui.button("⌨").on_hover_text("Keyboard shortcuts (F1)").clicked() {
                    self.show_shortcuts = !self.show_shortcuts;
                }
                let waiting = self.unfetched().len();
                let label = match (self.settings.instapaper_feed.is_some(), waiting) {
                    (false, _) => "Instapaper".to_string(),
                    (true, 0) => "Instapaper ✔".to_string(),
                    (true, n) => format!("Instapaper · {n}"),
                };
                let mut button = egui::Button::new(label);
                if waiting > 0 {
                    button = button.fill(CARD_SELECTED).stroke(Stroke::new(1.0, ACCENT));
                }
                if ui.add(button).on_hover_text(keys::display("Your Instapaper queue (Ctrl+I)")).clicked() {
                    self.queue.show_window = true;
                    if self.queue.items.is_empty() {
                        self.check_feed();
                    }
                }
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let button_width = 110.0;
            let was_empty = self.url_input.trim().is_empty();
            let edit = egui::TextEdit::singleline(&mut self.url_input)
                .hint_text("Paste a link to an article or PDF — it opens here to edit, format and preview before printing")
                .desired_width(ui.available_width() - button_width - ui.spacing().item_spacing.x)
                .margin(Margin::symmetric(10, 7));
            let response = ui.add(edit);
            if self.focus_url {
                response.request_focus();
                self.focus_url = false;
            }
            // Pasting links into the empty bar fetches them right away.
            let pasted = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
            let paste_submit =
                response.changed() && pasted && was_empty && !fetch::urls_in_text(&self.url_input).is_empty();
            let enter = paste_submit || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            let clicked = ui
                .add_sized(
                    [button_width, response.rect.height()],
                    egui::Button::new(RichText::new("Fetch").color(Color32::WHITE).strong()).fill(ACCENT),
                )
                .clicked();
            if enter || clicked {
                self.submit_urls();
            }
        });
    }

    fn printer_picker(&mut self, ui: &mut Ui) {
        let default_name = self.printers.iter().find(|p| p.is_default).map(|p| p.name.clone());
        let current = match (&self.settings.printer, &default_name) {
            (Some(p), _) => p.clone(),
            (None, Some(d)) => format!("{d} (default)"),
            (None, None) => "System default".to_string(),
        };
        let mut choice = self.settings.printer.clone();
        egui::ComboBox::from_id_salt("printer")
            .selected_text(format!("🖨 {}", truncate(&current, 26)))
            .width(220.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut choice, None, "System default");
                for p in &self.printers {
                    let label = if p.is_default { format!("{} (default)", p.name) } else { p.name.clone() };
                    ui.selectable_value(&mut choice, Some(p.name.clone()), label);
                }
                ui.separator();
                if ui.button("↻ Refresh list").clicked() {
                    self.worker.send(Request::RefreshPrinters);
                }
            });
        if choice != self.settings.printer {
            self.settings.printer = choice;
            self.save_settings_quietly();
        }
    }

    fn library_panel(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Library").family(theme::bold_family()).size(17.0));
            ui.label(RichText::new(format!("{}", self.entries.len())).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if self.select_mode { "Done" } else { "☑ Select" };
                if ui
                    .selectable_label(self.select_mode, RichText::new(label).small())
                    .on_hover_text(keys::display(
                        "Tick several articles to print them together as a newspaper-style edition. Or Ctrl+click / \
                         Shift+click them; from the keyboard, Space ticks the open one and Shift+↑ ↓ ticks a run.",
                    ))
                    .clicked()
                {
                    self.select_mode = !self.select_mode;
                    if !self.select_mode {
                        self.picked.clear();
                    }
                }
            });
        });
        let search = ui
            .add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text(keys::display("🔍 Search titles, tags and article text  (Ctrl+F)"))
                    .desired_width(f32::INFINITY),
            )
            .on_hover_text(
                "Words must all match. Use \"a phrase\", tag:name, or -word to exclude. Enter goes to the results \
                 (then ↑ ↓); Esc clears the search.",
            );
        if self.focus_search {
            search.request_focus();
            self.focus_search = false;
        }
        if search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.search.clear();
        }

        let mut tag_counts: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &self.entries {
            for tag in &entry.tags {
                *tag_counts.entry(tag.clone()).or_default() += 1;
            }
        }
        if !tag_counts.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                for (tag, count) in &tag_counts {
                    let active = self.tag_filter.as_deref() == Some(tag.as_str());
                    if ui.selectable_label(active, RichText::new(format!("#{tag} {count}")).small()).clicked() {
                        self.tag_filter = if active { None } else { Some(tag.clone()) };
                    }
                }
            });
        }
        ui.add_space(4.0);

        if self.entries.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Nothing saved yet").color(MUTED));
                ui.label(RichText::new("Paste a link above to start.").color(MUTED).small());
            });
            return;
        }

        let visible = self.visible();

        let picking = self.select_mode || !self.picked.is_empty();
        if picking {
            self.batch_bar(ui);
        }

        let visible_ids: Vec<String> = visible.iter().map(|(i, _)| self.entries[*i].id.clone()).collect();
        let mut clicked: Option<(String, egui::Modifiers)> = None;
        let reveal = std::mem::take(&mut self.reveal_selected);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, snippet) in &visible {
                let entry = &self.entries[*i];
                let picked = self.picked.contains(&entry.id);
                let is_open = self.selected.as_deref() == Some(entry.id.as_str());
                // Ticked cards are filled; the open document (the keyboard
                // cursor) is outlined, more heavily while ticking.
                let filled = picked || (!picking && is_open);
                let stroke = match (is_open, picked) {
                    (true, _) if picking => Stroke::new(2.0, ACCENT_DARK),
                    (true, _) | (_, true) => Stroke::new(1.0, ACCENT),
                    _ => Stroke::new(1.0, HAIRLINE),
                };
                let response = Frame::new()
                    .fill(if filled { CARD_SELECTED } else { CARD })
                    .stroke(stroke)
                    .corner_radius(8)
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if picking {
                            ui.horizontal(|ui| {
                                let mark = if picked { "☑" } else { "☐" };
                                ui.label(RichText::new(mark).color(if picked { ACCENT } else { MUTED }));
                                ui.add(egui::Label::new(RichText::new(truncate(&entry.title, 110)).strong()).wrap());
                            });
                        } else {
                            ui.label(RichText::new(truncate(&entry.title, 110)).strong());
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let badge = match entry.kind {
                                EntryKind::Pdf => Some("PDF"),
                                EntryKind::Edition => Some("📰 EDITION"),
                                EntryKind::Article => None,
                            };
                            if let Some(badge) = badge {
                                ui.label(RichText::new(badge).small().strong().color(ACCENT_DARK));
                                ui.label(RichText::new("·").small().color(MUTED));
                            }
                            let site = entry.site_name.clone().unwrap_or_else(|| entry.domain());
                            ui.label(RichText::new(truncate(&site, 26)).small().color(MUTED));
                            ui.label(RichText::new("·").small().color(MUTED));
                            ui.label(RichText::new(entry.saved_at.format("%b %-d").to_string()).small().color(MUTED));
                            ui.label(RichText::new("·").small().color(MUTED));
                            let size = if entry.spread { format!("{} sh", entry.pages) } else { format!("{} pp", entry.pages) };
                            ui.label(RichText::new(size).small().color(MUTED));
                            if entry.print_count() > 0 {
                                ui.label(RichText::new(format!("· 🖨 ×{}", entry.print_count())).small().color(SUCCESS));
                            }
                        });
                        if !entry.tags.is_empty() {
                            ui.label(RichText::new(format!("#{}", entry.tags.join("  #"))).small().color(ACCENT_DARK));
                        }
                        if let Some(snippet) = snippet {
                            ui.label(RichText::new(truncate(snippet, 160)).small().italics().color(MUTED));
                        }
                    })
                    .response
                    .interact(Sense::click());
                if response.clicked() {
                    clicked = Some((entry.id.clone(), ui.input(|i| i.modifiers)));
                }
                if reveal && is_open {
                    response.scroll_to_me(None);
                }
                response.on_hover_cursor(egui::CursorIcon::PointingHand);
                ui.add_space(2.0);
            }
            if visible.is_empty() {
                ui.label(RichText::new("No matches.").color(MUTED));
            }
        });
        if let Some((id, modifiers)) = clicked {
            if modifiers.shift {
                self.pick_range(&visible_ids, id);
            } else if modifiers.command || self.select_mode {
                self.toggle_pick(id);
            } else {
                self.picked.clear();
                self.select(id);
            }
        }
    }

    /// "3 selected · Combine into an edition · Clear", under the library.
    fn batch_bar(&mut self, ui: &mut Ui) {
        let articles = self.picked_articles().len();
        let others = self.picked.len() - articles;
        let open_edition = self
            .editor
            .as_ref()
            .filter(|e| e.edition.is_some())
            .and_then(|e| self.entries.iter().find(|x| x.id == e.id))
            .map(|e| e.title.clone());
        let (mut create, mut add, mut clear) = (false, false, false);
        egui::Panel::bottom("batch-bar")
            .frame(
                Frame::new()
                    .fill(CARD_SELECTED)
                    .stroke(Stroke::new(1.0, ACCENT))
                    .corner_radius(8)
                    .inner_margin(Margin::symmetric(10, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let text = match self.picked.len() {
                        0 => "Tick the articles to print together".to_string(),
                        n => format!("{n} selected"),
                    };
                    ui.label(RichText::new(text).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Clear").on_hover_text("Untick everything (Esc)").clicked() {
                            clear = true;
                        }
                    });
                });
                if others > 0 {
                    let what = if others == 1 { "1 PDF or edition is" } else { "PDFs and editions are" };
                    ui.label(RichText::new(format!("{what} left out: only web articles flow into columns.")).small().color(MUTED));
                }
                ui.add_space(2.0);
                let button = egui::Button::new(RichText::new("📰 Combine into an edition").color(Color32::WHITE).strong()).fill(ACCENT);
                if ui
                    .add_enabled(articles >= 1, button)
                    .on_hover_text(keys::display(
                        "Print these together like a newspaper: each story runs on straight after the last, through \
                         shared columns, so no page ends half empty (Ctrl+E)",
                    ))
                    .clicked()
                {
                    create = true;
                }
                if let Some(title) = open_edition {
                    if ui
                        .add_enabled(articles >= 1, egui::Button::new(format!("Add to “{}”", truncate(&title, 28))))
                        .on_hover_text("Add them to the end of the edition that's open")
                        .clicked()
                    {
                        add = true;
                    }
                }
            });
        ui.add_space(4.0);
        if create {
            self.create_edition();
        } else if add {
            self.add_to_open_edition();
        } else if clear {
            self.picked.clear();
            self.select_mode = false;
        }
    }

    // ----- UI: the document workspace ----------------------------------------

    fn workspace(&mut self, ui: &mut Ui) {
        let Some(entry) = self.selected.as_ref().and_then(|id| self.entries.iter().find(|e| &e.id == id)).cloned()
        else {
            self.welcome(ui);
            return;
        };

        self.document_header(ui, &entry);
        ui.add_space(4.0);

        let width = ui.available_width();
        egui::Panel::right("preview-pane")
            .resizable(true)
            .default_size((width * 0.48).max(340.0))
            .size_range(300.0..=width * 0.7)
            .frame(Frame::new().inner_margin(Margin { left: 12, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| self.preview_pane(ui, &entry));

        egui::CentralPanel::no_frame().show(ui, |ui| self.editor_pane(ui, &entry));
    }

    fn document_header(&mut self, ui: &mut Ui, entry: &Entry) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(truncate(&entry.title, 90)).family(theme::bold_family()).size(21.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("Delete").color(DANGER))).on_hover_text(keys::display("Delete (asks first; Delete or Ctrl+Backspace)")).clicked() {
                    self.confirm_delete = Some(entry.id.clone());
                }
                if ui.button("Show in folder").clicked() {
                    if let Ok(path) = self.library.pdf_path(&entry.id) {
                        if let Err(err) = opener::reveal(&path) {
                            self.toast(format!("Could not open the folder: {err}"), ToastKind::Error);
                        }
                    }
                }
                if ui.button("Open PDF").on_hover_text(keys::display("Open in your PDF viewer (Ctrl+O)")).clicked() {
                    self.open_pdf();
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            let site = entry.site_name.clone().unwrap_or_else(|| entry.domain());
            if entry.url.is_empty() {
                ui.label(RichText::new(truncate(&site, 40)).small().color(MUTED));
            } else {
                ui.hyperlink_to(RichText::new(truncate(&site, 40)).small(), &entry.url).on_hover_text(&entry.url);
            }
            let printed = match entry.print_count() {
                0 => "not printed yet".to_string(),
                1 => "printed once".to_string(),
                n => format!("printed {n} times"),
            };
            ui.label(
                RichText::new(format!("· saved {} · {printed} ·", entry.saved_at.format("%-d %b %Y, %H:%M")))
                    .small()
                    .color(MUTED),
            );
            let mut remove: Option<String> = None;
            for tag in &entry.tags {
                if ui
                    .add(egui::Button::new(RichText::new(format!("#{tag}  ×")).small().color(ACCENT_DARK)).small())
                    .on_hover_text("Remove tag")
                    .clicked()
                {
                    remove = Some(tag.clone());
                }
            }
            let response =
                ui.add(egui::TextEdit::singleline(&mut self.new_tag).hint_text("+ tag").desired_width(80.0));
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.new_tag.trim().is_empty() {
                let mut updated = entry.clone();
                for tag in self.new_tag.split(',') {
                    updated.add_tag(tag);
                }
                self.new_tag.clear();
                self.update_entry(updated);
                response.request_focus();
            }
            if let Some(tag) = remove {
                let mut updated = entry.clone();
                updated.remove_tag(&tag);
                self.update_entry(updated);
            }
        });
    }

    fn editor_pane(&mut self, ui: &mut Ui, entry: &Entry) {
        let Some(tab) = self.editor.as_ref().map(|e| e.tab) else { return };
        let is_edition = self.editor.as_ref().is_some_and(|e| e.edition.is_some());
        ui.horizontal(|ui| {
            let mut tab = tab;
            let first = if is_edition { "📰 Stories" } else { "✎ Edit" };
            ui.selectable_value(&mut tab, EditorTab::Edit, RichText::new(first).strong()).on_hover_text(keys::display("Ctrl+1"));
            ui.selectable_value(&mut tab, EditorTab::Format, RichText::new("Aa Format").strong()).on_hover_text(keys::display("Ctrl+2"));
            if let Some(editor) = self.editor.as_mut() {
                editor.tab = tab;
            }
        });
        ui.separator();

        if is_edition {
            match tab {
                EditorTab::Edit => self.stories_tab(ui),
                EditorTab::Format => self.edition_format_tab(ui),
            }
            return;
        }
        let is_pdf = self.editor.as_ref().is_some_and(|e| e.draft.is_none());
        if is_pdf {
            let message = if entry.kind == EntryKind::Pdf {
                "This is a PDF. It prints exactly as it is, so there's nothing to edit or format. Check the preview and print."
            } else {
                "This document can't be edited."
            };
            ui.add_space(12.0);
            ui.label(RichText::new(message).color(MUTED));
            if let Some(err) = self.editor.as_ref().and_then(|e| e.error.clone()) {
                ui.label(RichText::new(err).small().color(DANGER));
            }
            return;
        }
        match tab {
            EditorTab::Edit => self.edit_tab(ui, entry),
            EditorTab::Format => self.format_tab(ui),
        }
    }

    fn edit_tab(&mut self, ui: &mut Ui, entry: &Entry) {
        let image_trouble = entry.warnings.iter().any(|w| w.contains("image(s) could not be included"));
        let mut changed = false;
        let mut actions: Vec<BlockAction> = Vec::new();
        let mut refill = false;
        let mut ask_restore = false;
        let mut fold_all: Option<bool> = None;

        {
            let Some(editor) = self.editor.as_mut() else { return };
            let editing = editor.editing.as_ref().map(|(i, _)| *i);
            let folded = self.folds.get(&editor.id).cloned().unwrap_or_default();
            let Some(draft) = editor.draft.as_mut() else { return };
            let has_sections = (0..draft.blocks.len())
                .any(|i| matches!(draft.blocks[i].kind, BlockKind::Heading(_)) && draft.section(i).len() > 1);

            egui::Grid::new("meta").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Title");
                changed |= ui.add(egui::TextEdit::singleline(&mut draft.meta.title).desired_width(f32::INFINITY)).changed();
                ui.end_row();
                ui.label("Byline");
                let mut byline = draft.meta.byline.clone().unwrap_or_default();
                if ui.add(egui::TextEdit::singleline(&mut byline).hint_text("Author").desired_width(f32::INFINITY)).changed() {
                    draft.meta.byline = Some(byline);
                    changed = true;
                }
                ui.end_row();
                ui.label("Date");
                let mut date = draft.meta.published.clone().unwrap_or_default();
                if ui.add(egui::TextEdit::singleline(&mut date).hint_text("Published").desired_width(f32::INFINITY)).changed() {
                    draft.meta.published = Some(date);
                    changed = true;
                }
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} of {} blocks will print", draft.included_count(), draft.blocks.len()))
                        .small()
                        .color(MUTED),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(draft.is_modified(), egui::Button::new("Restore original").small())
                        .on_hover_text("Bring back every block, unedited, in the original order")
                        .clicked()
                    {
                        ask_restore = true;
                    }
                    if has_sections {
                        if ui.small_button("Expand all").on_hover_text("Show every block").clicked() {
                            fold_all = Some(false);
                        }
                        if ui
                            .small_button("Collapse all")
                            .on_hover_text("Show just the headings: an outline you can reorder and trim section by section")
                            .clicked()
                        {
                            fold_all = Some(true);
                        }
                    }
                    if image_trouble
                        && ui
                            .small_button("Fetch missing images")
                            .on_hover_text("Try downloading images that couldn't be fetched before")
                            .clicked()
                    {
                        refill = true;
                    }
                });
            });
            ui.separator();

            let depths = draft.depths();
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("blocks").show(ui, |ui| {
                // Blocks inside a folded section are skipped up to this index.
                let mut hidden_until = 0;
                for (i, block) in draft.blocks.iter().enumerate() {
                    if i < hidden_until {
                        continue;
                    }
                    let excluded = !block.included;
                    // A heading with blocks beneath it heads a section: its
                    // checkbox, fold arrow and ↑ ↓ act on the whole section.
                    let section = draft.section(i);
                    let heads_section = matches!(block.kind, BlockKind::Heading(_)) && section.len() > 1;
                    let is_folded = heads_section && folded.contains(&block.id);
                    if is_folded {
                        hidden_until = section.end;
                    }
                    let inclusion = draft.section_inclusion(i);
                    let indent = (depths[i].min(5) * 14) as i8;
                    Frame::new()
                        .fill(if excluded { Color32::from_rgb(0xF1, 0xEC, 0xE4) } else { CARD })
                        .stroke(Stroke::new(1.0, HAIRLINE))
                        .corner_radius(6)
                        .inner_margin(Margin::symmetric(8, 5))
                        .outer_margin(Margin { left: indent, right: 0, top: 0, bottom: 0 })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                if heads_section {
                                    let (arrow, tip) = if is_folded {
                                        ("▸", "Show the blocks under this heading")
                                    } else {
                                        ("▾", "Fold the blocks under this heading")
                                    };
                                    let fold = egui::Button::new(RichText::new(arrow).size(16.0).strong().color(ACCENT_DARK)).frame(false);
                                    if ui.add(fold).on_hover_text(tip).clicked() {
                                        actions.push(BlockAction::Fold(i));
                                    }
                                }
                                let mut checked = inclusion != Inclusion::Nothing;
                                let checkbox =
                                    egui::Checkbox::new(&mut checked, "").indeterminate(inclusion == Inclusion::Partly);
                                let tip = if heads_section {
                                    "Print this heading and everything beneath it (a dash means only some of it prints)"
                                } else {
                                    "Print this block"
                                };
                                if ui.add(checkbox).on_hover_text(tip).changed() {
                                    actions.push(if heads_section {
                                        // Partly printed → print it all; all → none.
                                        BlockAction::ToggleSection(i, inclusion != Inclusion::All)
                                    } else {
                                        BlockAction::Toggle(i)
                                    });
                                }
                                let label_color = if excluded { MUTED } else { ACCENT_DARK };
                                ui.add_sized(
                                    [42.0, 18.0],
                                    egui::Label::new(RichText::new(block.kind.label()).small().strong().color(label_color)),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                    if block.kind.editable()
                                        && editing != Some(i)
                                        && ui.small_button("✎").on_hover_text("Edit text").clicked()
                                    {
                                        actions.push(BlockAction::StartEdit(i));
                                    }
                                    let (down, up) = if heads_section {
                                        ("Move down, with everything beneath it", "Move up, with everything beneath it")
                                    } else {
                                        ("Move down", "Move up")
                                    };
                                    if ui.small_button("↓").on_hover_text(down).clicked() {
                                        actions.push(BlockAction::Down(i));
                                    }
                                    if ui.small_button("↑").on_hover_text(up).clicked() {
                                        actions.push(BlockAction::Up(i));
                                    }
                                    let mut text = RichText::new(block.preview());
                                    if excluded {
                                        text = text.strikethrough().color(MUTED);
                                    }
                                    if matches!(block.kind, BlockKind::Heading(_)) {
                                        text = text.strong();
                                    }
                                    ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                                        ui.add(egui::Label::new(text).wrap());
                                        if block.edited.is_some() {
                                            ui.label(RichText::new("edited").small().italics().color(ACCENT));
                                        }
                                        if is_folded {
                                            let hidden = section.len() - 1;
                                            let printed = draft.blocks[section.clone()].iter().skip(1).filter(|b| b.included).count();
                                            let note = match (hidden, printed) {
                                                (1, 1) => "+1 block".to_string(),
                                                (1, _) => "+1 block, left out".to_string(),
                                                (n, p) if p == n => format!("+{n} blocks"),
                                                (n, p) => format!("+{n} blocks, {p} print"),
                                            };
                                            ui.label(RichText::new(note).small().color(MUTED));
                                        }
                                    });
                                });
                            });
                        });
                    if editing == Some(i) {
                        if let Some((_, buffer)) = editor.editing.as_mut() {
                            let code = block.kind == BlockKind::Code;
                            let mut edit = egui::TextEdit::multiline(buffer).desired_width(f32::INFINITY).desired_rows(4);
                            if code {
                                edit = edit.code_editor();
                            }
                            ui.add(edit);
                            ui.horizontal(|ui| {
                                if ui.add(egui::Button::new(RichText::new("Done").color(Color32::WHITE)).fill(ACCENT)).clicked() {
                                    actions.push(BlockAction::SaveEdit);
                                }
                                if ui.button("Cancel").clicked() {
                                    actions.push(BlockAction::CancelEdit);
                                }
                                if block.edited.is_some() && ui.button("Revert to original").clicked() {
                                    actions.push(BlockAction::RevertEdit(i));
                                }
                                if !code {
                                    ui.label(RichText::new("**bold**  *italic*  [link](https://…)").small().color(MUTED));
                                }
                            });
                        }
                    }
                    ui.add_space(3.0);
                }
            });
        }

        // Apply what happened, now that nothing is borrowed.
        for action in actions {
            match action {
                BlockAction::Fold(i) => self.toggle_fold(i),
                action => changed |= self.apply_block_action(action),
            }
        }
        if let Some(fold) = fold_all {
            self.fold_all(fold);
        }
        if ask_restore {
            self.confirm_restore = true;
        }
        if refill {
            if let Some(editor) = self.editor.as_mut() {
                editor.refill_next = true;
            }
            changed = true;
        }
        if changed {
            self.mark_dirty();
        }
    }

    /// Fold or unfold the section under heading `index` of the open draft.
    fn toggle_fold(&mut self, index: usize) {
        let Some(editor) = self.editor.as_ref() else { return };
        let Some(block) = editor.draft.as_ref().and_then(|d| d.blocks.get(index)) else { return };
        let folds = self.folds.entry(editor.id.clone()).or_default();
        if !folds.remove(&block.id) {
            folds.insert(block.id);
        }
    }

    /// Fold every section (an outline of headings) or unfold them all.
    fn fold_all(&mut self, fold: bool) {
        let Some(editor) = self.editor.as_ref() else { return };
        let folds = self.folds.entry(editor.id.clone()).or_default();
        folds.clear();
        if let (true, Some(draft)) = (fold, editor.draft.as_ref()) {
            for (i, block) in draft.blocks.iter().enumerate() {
                if matches!(block.kind, BlockKind::Heading(_)) && draft.section(i).len() > 1 {
                    folds.insert(block.id);
                }
            }
        }
    }

    /// Returns true if the draft changed.
    fn apply_block_action(&mut self, action: BlockAction) -> bool {
        let Some(editor) = self.editor.as_mut() else { return false };
        let Some(draft) = editor.draft.as_mut() else { return false };
        match action {
            BlockAction::Toggle(i) => {
                if let Some(b) = draft.blocks.get_mut(i) {
                    b.included = !b.included;
                }
                true
            }
            BlockAction::ToggleSection(i, included) => {
                draft.set_section_included(i, included);
                true
            }
            BlockAction::Fold(_) => false,
            BlockAction::Up(i) | BlockAction::Down(i) => {
                // Headings move with their section; follow the block being
                // rewritten (by id) wherever the move puts it.
                let up = matches!(action, BlockAction::Up(_));
                let before: Vec<u32> = draft.blocks.iter().map(|b| b.id).collect();
                let editing_id = editor.editing.as_ref().and_then(|(e, _)| before.get(*e).copied());
                draft.move_section(i, up);
                if let (Some(id), Some((editing, _))) = (editing_id, editor.editing.as_mut()) {
                    if let Some(pos) = draft.blocks.iter().position(|b| b.id == id) {
                        *editing = pos;
                    }
                }
                draft.blocks.iter().map(|b| b.id).ne(before.iter().copied())
            }
            BlockAction::StartEdit(i) => {
                let text = draft.blocks.get(i).map(|b| b.markdown()).unwrap_or_default();
                editor.editing = Some((i, text));
                false
            }
            BlockAction::SaveEdit => {
                let Some((i, buffer)) = editor.editing.take() else { return false };
                let Some(block) = draft.blocks.get_mut(i) else { return false };
                let original = topebag::document::html_to_markdown(&block.html, block.kind);
                let new = if buffer.trim() == original.trim() { None } else { Some(buffer) };
                let changed = new != block.edited;
                block.edited = new;
                changed
            }
            BlockAction::CancelEdit => {
                editor.editing = None;
                false
            }
            BlockAction::RevertEdit(i) => {
                editor.editing = None;
                if let Some(b) = draft.blocks.get_mut(i) {
                    b.edited = None;
                }
                true
            }
        }
    }

    fn format_tab(&mut self, ui: &mut Ui) {
        let defaults = self.settings.format.clone();
        let mut use_as_default: Option<Format> = None;
        let mut changed = false;
        {
            let Some(draft) = self.editor.as_mut().and_then(|e| e.draft.as_mut()) else { return };
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("format").show(ui, |ui| {
                ui.label(RichText::new("These settings apply to this document only.").small().color(MUTED));
                ui.add_space(4.0);
                changed |= format_controls(ui, "doc", &mut draft.format, false);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(draft.format != defaults, egui::Button::new("Use as default for new documents"))
                        .clicked()
                    {
                        use_as_default = Some(draft.format.clone());
                    }
                    if ui.add_enabled(draft.format != defaults, egui::Button::new("Reset to defaults")).clicked() {
                        draft.format = defaults.clone();
                        changed = true;
                    }
                });
            });
        }
        if let Some(format) = use_as_default {
            self.settings.format = format;
            self.save_settings_quietly();
            self.toast("New documents will use this formatting.", ToastKind::Success);
        }
        if changed {
            self.mark_dirty();
        }
    }

    /// The edition editor: nameplate, front-page contents, and the stories
    /// in print order.
    fn stories_tab(&mut self, ui: &mut Ui) {
        let mut changed = false;
        let mut actions: Vec<StoryAction> = Vec::new();
        let mut update = false;
        {
            let entries = &self.entries;
            let Some(editor) = self.editor.as_mut() else { return };
            let stale = editor.stale;
            let Some(ed) = editor.edition.as_mut() else { return };

            egui::Grid::new("edition-meta").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Nameplate");
                changed |= ui
                    .add(egui::TextEdit::singleline(&mut ed.title).hint_text("The Topebag").desired_width(f32::INFINITY))
                    .changed();
                ui.end_row();
                ui.label("Front page");
                changed |= ui.checkbox(&mut ed.contents, "List the stories and their page numbers under the nameplate").changed();
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let included = ed.members.iter().filter(|m| m.included).count();
                ui.label(
                    RichText::new(format!("{included} of {} stories will print, one after another", ed.members.len()))
                        .small()
                        .color(MUTED),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut button = egui::Button::new("↻ Update from articles").small();
                    if stale {
                        button = button.fill(CARD_SELECTED).stroke(Stroke::new(1.0, ACCENT));
                    }
                    if ui.add(button).on_hover_text("Lay the edition out again with the latest edits to each article").clicked() {
                        update = true;
                    }
                });
            });
            if stale {
                ui.label(
                    RichText::new("Some of these articles were edited after this edition was laid out.").small().color(ACCENT),
                );
            }
            ui.separator();

            let count = ed.members.len();
            egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("stories").show(ui, |ui| {
                for (i, member) in ed.members.iter().enumerate() {
                    let entry = entries.iter().find(|e| e.id == member.id);
                    let usable = entry.is_some_and(edition::can_join);
                    let excluded = !member.included || !usable;
                    Frame::new()
                        .fill(if excluded { Color32::from_rgb(0xF1, 0xEC, 0xE4) } else { CARD })
                        .stroke(Stroke::new(1.0, HAIRLINE))
                        .corner_radius(6)
                        .inner_margin(Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let mut included = member.included;
                                if ui.add_enabled(usable, egui::Checkbox::new(&mut included, "")).on_hover_text("Print this story").changed() {
                                    actions.push(StoryAction::Toggle(i));
                                }
                                ui.add_sized(
                                    [22.0, 18.0],
                                    egui::Label::new(RichText::new(format!("{}", i + 1)).strong().color(ACCENT_DARK)),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                    if ui
                                        .add_enabled(count > 1, egui::Button::new("✖").small())
                                        .on_hover_text("Take this story out of the edition (the article stays in your library)")
                                        .clicked()
                                    {
                                        actions.push(StoryAction::Remove(i));
                                    }
                                    if ui.small_button("↓").on_hover_text("Move down").clicked() {
                                        actions.push(StoryAction::Down(i));
                                    }
                                    if ui.small_button("↑").on_hover_text("Move up").clicked() {
                                        actions.push(StoryAction::Up(i));
                                    }
                                    if usable
                                        && ui
                                            .small_button("Open")
                                            .on_hover_text("Edit this article, then come back and Update the edition")
                                            .clicked()
                                    {
                                        actions.push(StoryAction::Open(member.id.clone()));
                                    }
                                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                                        match entry {
                                            Some(e) => {
                                                let mut title = RichText::new(truncate(&e.title, 120)).strong();
                                                if excluded {
                                                    title = title.strikethrough().color(MUTED);
                                                }
                                                ui.add(egui::Label::new(title).wrap());
                                                let site = e.site_name.clone().unwrap_or_else(|| e.domain());
                                                let note = if usable { site } else { format!("{site} · PDFs can't be part of an edition") };
                                                ui.label(RichText::new(truncate(&note, 70)).small().color(MUTED));
                                            }
                                            None => {
                                                ui.label(RichText::new("Deleted from the library").italics().color(MUTED));
                                            }
                                        }
                                    });
                                });
                            });
                        });
                    ui.add_space(3.0);
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        keys::display("To add stories, tick articles in the library (☑ Select or Ctrl+click), then choose “Add to …”."),
                    )
                    .small()
                    .color(MUTED),
                );
            });
        }

        let mut open: Option<String> = None;
        if let Some(ed) = self.editor.as_mut().and_then(|e| e.edition.as_mut()) {
            for action in actions {
                match action {
                    StoryAction::Toggle(i) => {
                        if let Some(m) = ed.members.get_mut(i) {
                            m.included = !m.included;
                            changed = true;
                        }
                    }
                    StoryAction::Up(i) => changed |= ed.move_member(i, true) != i,
                    StoryAction::Down(i) => changed |= ed.move_member(i, false) != i,
                    StoryAction::Remove(i) => {
                        if ed.members.len() > 1 && i < ed.members.len() {
                            ed.members.remove(i);
                            changed = true;
                        }
                    }
                    StoryAction::Open(id) => open = Some(id),
                }
            }
        }
        if changed || update {
            self.mark_dirty();
        }
        if let Some(id) = open {
            self.select(id);
        }
    }

    fn edition_format_tab(&mut self, ui: &mut Ui) {
        let defaults = self.settings.format.clone();
        let mut changed = false;
        let Some(ed) = self.editor.as_mut().and_then(|e| e.edition.as_mut()) else { return };
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("edition-format").show(ui, |ui| {
            ui.label(RichText::new("These settings apply to this edition only.").small().color(MUTED));
            ui.add_space(4.0);
            changed |= format_controls(ui, "edition", &mut ed.format, true);
            ui.add_space(10.0);
            let newspaper = Edition::new(&[], &defaults).format;
            if ui.add_enabled(ed.format != newspaper, egui::Button::new("Reset to newspaper defaults")).clicked() {
                ed.format = newspaper;
                changed = true;
            }
        });
        if changed {
            self.mark_dirty();
        }
    }

    fn preview_pane(&mut self, ui: &mut Ui, entry: &Entry) {
        let (busy, error) = self.editor.as_ref().map(|e| (e.busy(), e.error.clone())).unwrap_or((false, None));

        // Print bar at the bottom, preview above it.
        egui::Panel::bottom("print-bar")
            .frame(Frame::new().inner_margin(Margin { left: 0, right: 0, top: 8, bottom: 2 }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let print = ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(RichText::new("🖨 Print").color(Color32::WHITE).strong()).fill(ACCENT),
                        )
                        .on_hover_text(keys::display("Print this document (Ctrl+P)"))
                        .on_disabled_hover_text("Updating the preview…");
                    if print.clicked() {
                        self.print_selected();
                    }
                    ui.label("×");
                    let mut copies = self.settings.copies;
                    if ui.add(egui::DragValue::new(&mut copies).range(1..=99)).on_hover_text("Copies").changed() {
                        self.settings.copies = copies;
                        self.save_settings_quietly();
                    }
                    self.printer_picker(ui);
                });
            });

        ui.horizontal(|ui| {
            ui.label(RichText::new("Preview").strong());
            let size = match (entry.spread, entry.pages) {
                (true, 1) => "1 sheet, two pages side by side".to_string(),
                (true, n) => format!("{n} sheets, two pages side by side"),
                (false, 1) => "1 page".to_string(),
                (false, n) => format!("{n} pages"),
            };
            let tightened = if entry.density > 0.005 {
                format!(" · spacing tightened {:.0}%", entry.density * 100.0)
            } else {
                String::new()
            };
            ui.label(RichText::new(format!("{size}{tightened}")).small().color(MUTED));
            if busy {
                ui.add(egui::Spinner::new().size(13.0));
                ui.label(RichText::new("Updating…").small().color(MUTED));
            }
        });
        if let Some(err) = error {
            ui.label(RichText::new(format!("Couldn't update: {}", truncate(&err, 200))).small().color(DANGER));
        }
        for w in entry.warnings.iter().take(2) {
            ui.label(RichText::new(format!("⚠ {}", truncate(w, 140))).small().color(MUTED));
        }
        ui.add_space(4.0);
        self.preview_pages(ui, &entry.id, busy);
    }

    fn preview_pages(&mut self, ui: &mut Ui, id: &str, busy: bool) {
        match self.previews.get(id) {
            None | Some(Preview::Loading) => {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.add(egui::Spinner::new().size(28.0));
                    ui.label(RichText::new("Rendering preview…").color(MUTED));
                });
            }
            Some(Preview::Failed(err)) => {
                ui.label(RichText::new(format!("Preview unavailable: {err}")).color(DANGER));
            }
            Some(Preview::Ready(pages)) => {
                let pages = pages.clone();
                let fade = if busy { 150 } else { 255 };
                egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("preview").show(ui, |ui| {
                    ui.add_space(6.0);
                    let width = (ui.available_width() - 24.0).clamp(200.0, 900.0);
                    for (i, tex) in pages.iter().enumerate() {
                        let size = tex.size_vec2();
                        let scaled = egui::vec2(width, width * size.y / size.x);
                        ui.vertical_centered(|ui| {
                            Frame::new()
                                .fill(Color32::WHITE)
                                .stroke(Stroke::new(1.0, HAIRLINE))
                                .shadow(egui::Shadow { offset: [0, 3], blur: 12, spread: 0, color: Color32::from_black_alpha(28) })
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Image::new(egui::load::SizedTexture::new(tex.id(), scaled))
                                            .tint(Color32::from_white_alpha(fade)),
                                    );
                                });
                            ui.label(RichText::new(format!("{} / {}", i + 1, pages.len())).small().color(MUTED));
                        });
                        ui.add_space(10.0);
                    }
                });
            }
        }
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.add_space(ui.available_height() * 0.18);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Links in, paper out.").family(theme::bold_family()).size(30.0));
            ui.add_space(6.0);
            ui.label(RichText::new("Paste a link to an article or PDF. Topebag cleans it up and typesets it;").color(MUTED));
            ui.label(RichText::new("you trim and format it with a live preview, then print when it looks right.").color(MUTED));
            ui.add_space(18.0);
            for (n, text) in [
                ("1", "Paste a link (Ctrl+V anywhere in this window)"),
                ("2", "Edit: drop ads and asides, fix the title, rewrite a paragraph"),
                ("3", "Format: paper, type size, two pages per sheet… and watch the preview"),
                ("4", "Print. Every document stays in your library"),
                ("5", "Several to print? Tick them (☑ Select) and combine them into a newspaper-style edition"),
                ("⌨", "Prefer the keyboard? ↑ ↓ move through the library; F1 lists every shortcut"),
            ] {
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 520.0).max(0.0) / 2.0);
                    ui.label(RichText::new(n).strong().color(ACCENT));
                    ui.label(keys::display(text));
                });
            }
        });
    }

    fn activity(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Activity").strong());
            let running = self.jobs.iter().filter(|j| matches!(j.status, JobStatus::Running(_) | JobStatus::Queued)).count();
            if running > 0 {
                ui.add(egui::Spinner::new().size(13.0));
                ui.label(RichText::new(format!("{running} in progress")).small().color(MUTED));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.jobs.iter().any(|j| matches!(j.status, JobStatus::Done(_) | JobStatus::Failed(_)))
                    && ui.small_button("Clear finished").clicked()
                {
                    self.jobs.retain(|j| matches!(j.status, JobStatus::Running(_) | JobStatus::Queued));
                }
            });
        });
        if self.jobs.is_empty() {
            ui.label(RichText::new("Nothing yet. Fetches and print jobs show up here.").small().color(MUTED));
            return;
        }
        let mut show: Option<String> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("activity").show(ui, |ui| {
            for job in self.jobs.iter().rev().take(30) {
                ui.horizontal(|ui| {
                    match &job.status {
                        JobStatus::Queued => {
                            ui.label(RichText::new("…").color(MUTED));
                        }
                        JobStatus::Running(_) => {
                            ui.add(egui::Spinner::new().size(13.0));
                        }
                        JobStatus::Done(_) => {
                            ui.label(RichText::new("✔").color(SUCCESS));
                        }
                        JobStatus::Failed(_) => {
                            ui.label(RichText::new("✖").color(DANGER));
                        }
                    }
                    let label = ui.label(RichText::new(truncate(&job.label, 70)).strong());
                    if let Some(id) = &job.entry_id {
                        if label.interact(Sense::click()).on_hover_text("Open").clicked() {
                            show = Some(id.clone());
                        }
                    }
                    let (text, color) = match &job.status {
                        JobStatus::Queued => ("Waiting…".to_string(), MUTED),
                        JobStatus::Running(s) => (s.clone(), MUTED),
                        JobStatus::Done(s) => (s.clone(), SUCCESS),
                        JobStatus::Failed(s) => (s.lines().next().unwrap_or("").to_string(), DANGER),
                    };
                    let resp = ui.label(RichText::new(truncate(&text, 140)).small().color(color));
                    if let JobStatus::Failed(full) = &job.status {
                        resp.on_hover_text(full);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if matches!(job.status, JobStatus::Running(_) | JobStatus::Queued) {
                            ui.label(RichText::new(format!("{}s", job.started.elapsed().as_secs())).small().color(MUTED));
                        }
                    });
                });
            }
        });
        if let Some(id) = show {
            self.select(id);
        }
        if self.jobs.iter().any(|j| matches!(j.status, JobStatus::Running(_) | JobStatus::Queued)) {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }

    // ----- windows -----------------------------------------------------------

    fn instapaper_window(&mut self, ctx: &egui::Context) {
        if !self.queue.show_window {
            return;
        }
        let mut open = true;
        let mut to_fetch: Option<Vec<FeedItem>> = None;
        let mut to_skip: Option<Vec<String>> = None;
        let mut connect: Option<String> = None;
        let mut disconnect = false;

        egui::Window::new("Instapaper queue")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .default_height(480.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if self.settings.instapaper_feed.is_none() {
                    ui.label(RichText::new("Bring in your Instapaper queue").strong());
                    ui.label("Topebag reads the private RSS feed of your Instapaper Unread folder. No password needed.");
                    ui.add_space(4.0);
                    ui.label("1. On instapaper.com, open Unread.");
                    ui.label("2. Open the Downloads menu (top right) and copy the “RSS Feed” link.");
                    ui.label("3. Paste it here:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.queue.feed_input)
                            .hint_text("https://www.instapaper.com/rss/…")
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(6.0);
                    if ui.add(egui::Button::new(RichText::new("Connect").color(Color32::WHITE).strong()).fill(ACCENT)).clicked() {
                        connect = Some(self.queue.feed_input.clone());
                    }
                    ui.label(RichText::new("Keep the feed link private: anyone with it can read your queue.").small().color(MUTED));
                    return;
                }

                let waiting: Vec<FeedItem> = self.unfetched().into_iter().cloned().collect();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(match waiting.len() {
                            0 => "Everything in your queue is in your library.".to_string(),
                            1 => "1 article waiting".to_string(),
                            n => format!("{n} articles waiting"),
                        })
                        .strong(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.queue.checking {
                            ui.add(egui::Spinner::new().size(14.0));
                        } else if ui.small_button("↻ Check now").clicked() {
                            self.check_feed();
                        }
                    });
                });
                if let Some(t) = self.queue.state.last_checked {
                    ui.label(RichText::new(format!("Last checked {}", t.format("%-d %b, %H:%M"))).small().color(MUTED));
                }
                if let Some(err) = &self.queue.error {
                    ui.label(RichText::new(format!("Could not read the feed: {err}")).small().color(DANGER));
                }
                ui.separator();

                egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
                    for item in &waiting {
                        let mut ticked = !self.queue.unticked.contains(&item.id);
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut ticked, "").changed() {
                                if ticked {
                                    self.queue.unticked.remove(&item.id);
                                } else {
                                    self.queue.unticked.insert(item.id.clone());
                                }
                            }
                            ui.vertical(|ui| {
                                ui.label(RichText::new(truncate(&item.title, 90)).strong());
                                let host = url::Url::parse(&item.url)
                                    .ok()
                                    .and_then(|u| u.host_str().map(str::to_string))
                                    .unwrap_or_default();
                                let saved = item.saved.as_deref().map(|s| format!(" · saved {s}")).unwrap_or_default();
                                ui.label(RichText::new(format!("{}{saved}", host.trim_start_matches("www."))).small().color(MUTED));
                            });
                        });
                    }
                });

                let selected: Vec<FeedItem> =
                    waiting.iter().filter(|i| !self.queue.unticked.contains(&i.id)).cloned().collect();
                ui.separator();
                ui.horizontal(|ui| {
                    let enabled = !selected.is_empty();
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(RichText::new(format!("Fetch {}", selected.len())).color(Color32::WHITE).strong())
                                .fill(ACCENT),
                        )
                        .on_hover_text("Add them to your library to edit and print")
                        .clicked()
                    {
                        to_fetch = Some(selected.clone());
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new(format!("Skip {}", selected.len())))
                        .on_hover_text("Mark as handled without fetching")
                        .clicked()
                    {
                        to_skip = Some(selected.iter().map(|i| i.id.clone()).collect());
                    }
                });
                ui.add_space(4.0);
                let mut auto = self.settings.instapaper_auto;
                if ui
                    .checkbox(
                        &mut auto,
                        format!(
                            "Fetch new saves automatically (every {} min while Topebag runs; nothing prints by itself)",
                            self.settings.instapaper_interval_min
                        ),
                    )
                    .on_hover_text("Only articles saved after you turn this on are fetched automatically.")
                    .changed()
                {
                    self.settings.instapaper_auto = auto;
                    self.save_settings_quietly();
                }
                if ui.small_button("Disconnect Instapaper").clicked() {
                    disconnect = true;
                }
            });

        if let Some(feed) = connect {
            match instapaper::parse_feed_url(&feed) {
                Ok(url) => {
                    self.settings.instapaper_feed = Some(url.to_string());
                    self.save_settings_quietly();
                    self.check_feed();
                }
                Err(err) => self.toast(format!("{err:#}"), ToastKind::Error),
            }
        }
        if let Some(items) = to_fetch {
            self.fetch_queue_items(items);
        }
        if let Some(ids) = to_skip {
            self.queue.state.mark_seen(ids.iter().map(String::as_str));
            self.save_queue_state();
        }
        if disconnect {
            self.settings.instapaper_feed = None;
            self.settings.instapaper_auto = false;
            self.save_settings_quietly();
            self.queue.items.clear();
        }
        self.queue.show_window = open;
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.settings_draft.take() else { return };
        let mut open = true;
        let mut action: Option<bool> = None; // Some(true) = save, Some(false) = cancel
        let has_browser = browser::find_browser().is_some();
        let has_tray = self.tray.is_some();

        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(520.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(580.0).show(ui, |ui| {
                    let s = &mut draft.settings;
                    section(ui, "Printing");
                    egui::Grid::new("settings-print").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
                        ui.label("Copies");
                        ui.add(egui::DragValue::new(&mut s.copies).range(1..=99));
                        ui.end_row();
                        ui.label("Quality");
                        combo(ui, "quality", &mut s.print_quality, &PrintQuality::ALL, |q| q.label());
                        ui.end_row();
                    });

                    section(ui, "Defaults for new documents");
                    ui.label(
                        RichText::new("Each document can be formatted on its own in its Format tab.").small().color(MUTED),
                    );
                    format_controls(ui, "defaults", &mut s.format, false);

                    section(ui, "JavaScript pages and sign-ins");
                    if !has_browser {
                        ui.label(
                            RichText::new("Microsoft Edge or Google Chrome was not found, so this is unavailable.")
                                .small()
                                .color(DANGER),
                        );
                    }
                    egui::Grid::new("settings-browser").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
                        ui.label("Use the browser");
                        combo(ui, "browser", &mut s.browser_mode, &BrowserMode::ALL, |m| m.label());
                        ui.end_row();
                    });
                    ui.label(
                        RichText::new(
                            "Pages that come back (nearly) empty are reloaded in Edge without a window. Sites you list \
                             below are always loaded that way, using Topebag's own browser profile.",
                        )
                        .small()
                        .color(MUTED),
                    );
                    ui.label("Sites you've signed in to (one per line):");
                    ui.add(
                        egui::TextEdit::multiline(&mut draft.sites)
                            .hint_text("nytimes.com\nft.com")
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    ui.horizontal(|ui| {
                        if ui.add_enabled(has_browser, egui::Button::new("Open sign-in browser…")).clicked() {
                            let first = draft.sites.lines().map(browser::normalize_site).find(|s| !s.is_empty());
                            let start = first.map(|s| format!("https://{s}/"));
                            if let Err(err) =
                                browser::open_sign_in_window(&browser::profile_dir(&self.data_dir), start.as_deref())
                            {
                                self.toasts.push(Toast { text: format!("{err:#}"), kind: ToastKind::Error, shown: Instant::now() });
                            }
                        }
                        ui.label(RichText::new("Sign in there, then close that window.").small().color(MUTED));
                    });

                    section(ui, "Instapaper");
                    ui.label("Unread folder RSS feed (instapaper.com → Unread → Downloads → RSS Feed):");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.feed)
                            .hint_text("https://www.instapaper.com/rss/…")
                            .desired_width(f32::INFINITY),
                    );
                    let s = &mut draft.settings;
                    egui::Grid::new("settings-instapaper").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
                        ui.label("New saves");
                        ui.checkbox(&mut s.instapaper_auto, "Fetch automatically (never printed by themselves)");
                        ui.end_row();
                        ui.label("Check every");
                        ui.add(egui::DragValue::new(&mut s.instapaper_interval_min).range(5..=1440).suffix(" min"));
                        ui.end_row();
                    });

                    section(ui, "Background");
                    ui.add_enabled(
                        has_tray,
                        egui::Checkbox::new(&mut s.close_to_tray, "Keep running in the tray when the window is closed"),
                    )
                    .on_disabled_hover_text("The tray icon is only available on Windows for now");
                    ui.add_enabled(autostart::SUPPORTED, egui::Checkbox::new(&mut s.start_with_windows, autostart::LABEL));

                    section(ui, "Library");
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(self.library.root().display().to_string()).small().color(MUTED));
                        if ui.small_button("Open").clicked() {
                            let _ = opener::open(self.library.root());
                        }
                    });
                });

                ui.add_space(10.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new("Save").color(Color32::WHITE).strong()).fill(ACCENT)).clicked() {
                        action = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(false);
                    }
                });
            });

        match action {
            Some(true) => self.save_settings(draft),
            Some(false) => {}
            None if open => self.settings_draft = Some(draft),
            None => {}
        }
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_delete.clone() else { return };
        let title = self.entries.iter().find(|e| e.id == id).map(|e| e.title.clone()).unwrap_or_default();
        let mut close = false;
        let enter = self.dialog_enter(ctx);
        let modal = egui::Modal::new(egui::Id::unique("confirm-delete")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.label(RichText::new("Delete this document?").family(theme::bold_family()).size(18.0));
            ui.label(RichText::new(truncate(&title, 120)).color(MUTED));
            if self.entries.iter().any(|e| e.id == id && e.kind == EntryKind::Edition) {
                ui.label("The edition's PDF will be removed. The articles in it stay in your library.");
            } else {
                ui.label("The PDF, its images and your edits will be removed from the library.");
            }
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let delete = ui
                    .add(egui::Button::new(RichText::new("Delete").color(Color32::WHITE).strong()).fill(DANGER))
                    .on_hover_text("Enter");
                if delete.clicked() || enter {
                    match pipeline::delete_entry(&self.library, &id) {
                        Ok(()) => {
                            self.previews.remove(&id);
                            self.index.remove(&id);
                            if self.selected.as_deref() == Some(id.as_str()) {
                                self.selected = None;
                                self.editor = None;
                            }
                            self.toast("Deleted.", ToastKind::Info);
                        }
                        Err(err) => self.toast(format!("Could not delete: {err:#}"), ToastKind::Error),
                    }
                    close = true;
                }
                if ui.button("Cancel").on_hover_text("Esc").clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.confirm_delete = None;
            self.dialog_ready = false;
            self.reload_entries();
        }
    }

    fn restore_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_restore {
            return;
        }
        let mut close = false;
        let mut restore = false;
        let enter = self.dialog_enter(ctx);
        let modal = egui::Modal::new(egui::Id::unique("confirm-restore")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.label(RichText::new("Restore the original?").family(theme::bold_family()).size(18.0));
            ui.label("All blocks come back, unedited and in their original order, and the title, byline and date are reset. Formatting is kept.");
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let confirm = ui
                    .add(egui::Button::new(RichText::new("Restore").color(Color32::WHITE).strong()).fill(ACCENT))
                    .on_hover_text("Enter");
                if confirm.clicked() || enter {
                    restore = true;
                    close = true;
                }
                if ui.button("Cancel").on_hover_text("Esc").clicked() {
                    close = true;
                }
            });
        });
        close |= modal.should_close();
        if restore {
            let id = self.editor.as_ref().map(|e| e.id.clone());
            if let Some(id) = id {
                match pipeline::original_html(&self.library, &id) {
                    Ok(html) => {
                        if let Some(editor) = self.editor.as_mut() {
                            editor.editing = None;
                            if let Some(draft) = editor.draft.as_mut() {
                                draft.restore_original(&html);
                            }
                        }
                        self.mark_dirty();
                    }
                    Err(err) => self.toast(format!("{err:#}"), ToastKind::Error),
                }
            }
        }
        if close {
            self.confirm_restore = false;
            self.dialog_ready = false;
        }
    }

    /// Enter in a confirmation dialog, once it has been on screen for a frame
    /// (so the key press that opened it can't also confirm it).
    fn dialog_enter(&mut self, ctx: &egui::Context) -> bool {
        let ready = std::mem::replace(&mut self.dialog_ready, true);
        ready && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.show_shortcuts {
            return;
        }
        let mut open = true;
        egui::Window::new("Keyboard shortcuts")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(600.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(560.0).show(ui, |ui| {
                    for (group, rows) in keys::GROUPS {
                        section(ui, group);
                        egui::Grid::new(format!("keys-{group}")).num_columns(2).spacing([24.0, 5.0]).show(ui, |ui| {
                            for (keys, what) in *rows {
                                ui.label(RichText::new(keys::display(keys)).strong().color(ACCENT_DARK));
                                ui.label(keys::display(what));
                                ui.end_row();
                            }
                        });
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Plain-key shortcuts pause while you type in a field or after Tab focuses a button. \
                             Press Esc to get them back.",
                        )
                        .small()
                        .color(MUTED),
                    );
                });
            });
        if !open {
            self.show_shortcuts = false;
        }
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        const LIFETIME: Duration = Duration::from_secs(5);
        self.toasts.retain(|t| t.shown.elapsed() < LIFETIME);
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -16.0])
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                for toast in self.toasts.iter().rev().take(4).rev() {
                    let (fill, icon) = match toast.kind {
                        ToastKind::Info => (Color32::from_rgb(0x3A, 0x34, 0x2E), "ℹ"),
                        ToastKind::Success => (SUCCESS, "✔"),
                        ToastKind::Error => (DANGER, "✖"),
                    };
                    Frame::new().fill(fill).corner_radius(8).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                        ui.set_max_width(380.0);
                        ui.label(RichText::new(format!("{icon}  {}", toast.text)).color(Color32::WHITE));
                    });
                    ui.add_space(6.0);
                }
            });
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

impl eframe::App for App {
    /// Runs before every frame *and* while the window is hidden in the tray.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.hide_in_frames > 0 {
            self.hide_in_frames -= 1;
            if self.hide_in_frames == 0 && self.hidden {
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            } else {
                ctx.request_repaint();
            }
        }
        self.process_events(ctx);
        self.process_tray_and_instance(ctx);
        self.handle_close(ctx);
        self.maybe_rebuild(ctx);
        self.start_due_preview(ctx);
        if self.feed_due() {
            self.check_feed();
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_global_paste(&ctx);
        self.handle_shortcuts(&ctx);

        egui::Panel::top("toolbar")
            .frame(Frame::new().fill(CARD).inner_margin(Margin::symmetric(16, 10)).stroke(Stroke::new(1.0, HAIRLINE)))
            .show(ui, |ui| self.toolbar(ui));

        egui::Panel::bottom("activity")
            .resizable(true)
            .default_size(110.0)
            .size_range(60.0..=320.0)
            .frame(Frame::new().fill(CARD).inner_margin(Margin::symmetric(16, 8)).stroke(Stroke::new(1.0, HAIRLINE)))
            .show(ui, |ui| self.activity(ui));

        egui::Panel::left("library")
            .resizable(true)
            .default_size(290.0)
            .size_range(220.0..=480.0)
            .frame(Frame::new().fill(theme::PAPER).inner_margin(Margin::symmetric(12, 8)).stroke(Stroke::new(1.0, HAIRLINE)))
            .show(ui, |ui| self.library_panel(ui));

        egui::CentralPanel::default_margins()
            .frame(Frame::new().fill(theme::PAPER).inner_margin(Margin::symmetric(16, 10)))
            .show(ui, |ui| self.workspace(ui));

        self.instapaper_window(&ctx);
        self.settings_window(&ctx);
        self.delete_dialog(&ctx);
        self.restore_dialog(&ctx);
        self.shortcuts_window(&ctx);
        self.toasts(&ctx);
        self.was_typing = ctx.text_edit_focused();
    }
}

/// Controls for a [`Format`]; used by a document's Format tab, an edition's
/// Format tab (`edition`: no side-by-side, fitting is always available) and
/// the defaults in Settings. Returns true if anything changed.
fn format_controls(ui: &mut Ui, id: &str, f: &mut Format, edition: bool) -> bool {
    let before = f.clone();
    egui::Grid::new(format!("format-{id}")).num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
        if edition {
            ui.label("Layout");
            ui.checkbox(&mut f.fit_to_sheets, "Tighten spacing to use the fewest pages").on_hover_text(
                "Shrinks text, spacing and margins only as much as needed to save a page (text never below 8.5 pt)",
            );
        } else {
            ui.label("Spread");
            ui.vertical(|ui| {
                ui.checkbox(&mut f.side_by_side, "Two pages side by side on each sheet")
                    .on_hover_text("Each landscape sheet holds two half-size pages, like an open book");
                ui.add_enabled(f.side_by_side, egui::Checkbox::new(&mut f.fit_to_sheets, "Tighten spacing to use the fewest sheets"))
                    .on_hover_text("Shrinks text, spacing and margins only as much as needed to save a sheet (text never below 8.5 pt)");
            });
        }
        ui.end_row();
        ui.label("Paper");
        combo(ui, &format!("{id}-paper"), &mut f.paper, &Paper::ALL, |p| p.label());
        ui.end_row();
        ui.label("Typeface");
        combo(ui, &format!("{id}-typeface"), &mut f.typeface, &Typeface::ALL, |t| t.label());
        ui.end_row();
        ui.label("Text size");
        ui.add(egui::Slider::new(&mut f.font_size, 8.0..=16.0).step_by(0.5).suffix(" pt"));
        ui.end_row();
        ui.label("Columns");
        ui.add_enabled_ui(edition || !f.side_by_side, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut f.columns, 1, "One");
                ui.selectable_value(&mut f.columns, 2, "Two");
                ui.selectable_value(&mut f.columns, 3, "Three");
            });
        })
        .response
        .on_disabled_hover_text("Each half of a side-by-side sheet is one column");
        ui.end_row();
        ui.label("Alignment");
        ui.checkbox(&mut f.justify, "Justify with hyphenation");
        ui.end_row();
        ui.label("Images");
        ui.vertical(|ui| {
            ui.checkbox(&mut f.include_images, "Include images");
            ui.add_enabled(f.include_images, egui::Checkbox::new(&mut f.grayscale_images, "Print in grayscale"));
        });
        ui.end_row();
        ui.label("Links");
        combo(ui, &format!("{id}-links"), &mut f.link_style, &LinkStyle::ALL, |l| l.label());
        ui.end_row();
        ui.label("QR code");
        let qr = if edition { "A small code by each headline, linking to the original" } else { "Link back to the original" };
        ui.checkbox(&mut f.qr_code, qr);
        ui.end_row();
    });
    f.sanitize();
    *f != before
}

fn to_preview(ctx: &egui::Context, id: &str, result: Result<Vec<topebag::typeset::RasterPage>, String>) -> Preview {
    match result {
        Ok(pages) => Preview::Ready(
            pages
                .into_iter()
                .enumerate()
                .map(|(i, page)| {
                    let image =
                        egui::ColorImage::from_rgba_unmultiplied([page.width as usize, page.height as usize], &page.rgba);
                    ctx.load_texture(format!("{id}#{i}"), image, egui::TextureOptions::LINEAR)
                })
                .collect(),
        ),
        Err(err) => Preview::Failed(err),
    }
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong().color(ACCENT_DARK));
    ui.add_space(2.0);
}

fn combo<T: Copy + PartialEq>(ui: &mut Ui, id: &str, value: &mut T, options: &[T], label: impl Fn(T) -> &'static str) {
    egui::ComboBox::from_id_salt(id).selected_text(label(*value)).width(230.0).show_ui(ui, |ui| {
        for &option in options {
            ui.selectable_value(value, option, label(option));
        }
    });
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>().trim_end())
    }
}
