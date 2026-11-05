//! The desktop app (egui / eframe).
//!
//! Layout:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ Topebag   [ paste a link…            ] [Print]  Instapaper ⚙ │  toolbar
//! ├───────────────┬──────────────────────────────────────────────┤
//! │ Library       │  Title, source, tags, actions                │
//! │ [search]      │  ┌────────────┐                              │
//! │ #tag #tag     │  │  page 1    │  live preview of the PDF     │
//! │ ▸ entry       │  └────────────┘                              │
//! ├───────────────┴──────────────────────────────────────────────┤
//! │ Activity: fetching… typesetting… printed ✓                   │  activity
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! All slow work goes through [`worker::Worker`]; this module keeps UI state
//! and reacts to [`worker::Event`]s. Event handling, timers, tray actions and
//! messages from later launches are processed in `App::logic`, which eframe
//! also runs while the window is hidden in the tray.

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
use topebag::instapaper::{self, FeedItem, QueueState};
use topebag::instance::{self, Message, Role};
use topebag::library::{self, Entry, EntryKind, Library};
use topebag::print::PrinterInfo;
use topebag::search::{SearchIndex, parse_query};
use topebag::settings::{BrowserMode, LinkStyle, Paper, PrintQuality, Settings, Typeface};
use topebag::{autostart, browser, fetch};

use self::theme::{ACCENT, ACCENT_DARK, CARD, CARD_SELECTED, DANGER, HAIRLINE, MUTED, SUCCESS};
use self::tray::TrayAction;
use self::worker::{Event, JobId, Request, Worker};

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
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([860.0, 540.0])
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

struct App {
    data_dir: PathBuf,
    library: Library,
    settings: Settings,
    draft: Option<SettingsDraft>,
    entries: Vec<Entry>,
    selected: Option<String>,
    search: String,
    tag_filter: Option<String>,
    index: SearchIndex,
    new_tag: String,
    /// Title being edited in the detail panel.
    renaming: Option<String>,
    url_input: String,
    focus_url: bool,
    printers: Vec<PrinterInfo>,
    jobs: Vec<Job>,
    next_job: JobId,
    worker: Worker,
    previews: HashMap<String, Preview>,
    toasts: Vec<Toast>,
    confirm_delete: Option<String>,
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
            draft: None,
            entries: Vec::new(),
            selected: None,
            search: String::new(),
            tag_filter: None,
            index: SearchIndex::default(),
            new_tag: String::new(),
            renaming: None,
            url_input: String::new(),
            focus_url: true,
            printers: Vec::new(),
            jobs: Vec::new(),
            next_job: 1,
            worker,
            previews: HashMap::new(),
            toasts: Vec::new(),
            confirm_delete: None,
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

    fn select(&mut self, id: String) {
        if !self.previews.contains_key(&id) {
            self.previews.insert(id.clone(), Preview::Loading);
            self.worker.preview(id.clone());
        }
        self.selected = Some(id);
        self.new_tag.clear();
        self.renaming = None;
    }

    fn refresh_preview(&mut self, id: &str) {
        self.previews.insert(id.to_string(), Preview::Loading);
        self.worker.preview(id.to_string());
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
        self.worker.send(Request::Add { job, url, settings: self.settings.clone(), print: self.settings.auto_print });
    }

    fn print_selected(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        let title = self.entries.iter().find(|e| e.id == id).map(|e| e.title.clone()).unwrap_or_else(|| id.clone());
        let job = self.new_job(format!("Print: {title}"), Some(id.clone()));
        self.worker.send(Request::Print { job, id, settings: self.settings.clone() });
    }

    fn retypeset_selected(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        let title = self.entries.iter().find(|e| e.id == id).map(|e| e.title.clone()).unwrap_or_else(|| id.clone());
        let job = self.new_job(format!("Re-typeset: {title}"), Some(id.clone()));
        self.worker.send(Request::Retypeset { job, id, settings: self.settings.clone() });
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
        self.draft = Some(SettingsDraft {
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
                self.toast(format!("Could not change “Start with Windows”: {err:#}"), ToastKind::Error);
                new.start_with_windows = autostart::is_enabled();
            }
        }

        let layout_changed = new.paper != self.settings.paper
            || new.font_size != self.settings.font_size
            || new.typeface != self.settings.typeface
            || new.columns != self.settings.columns
            || new.justify != self.settings.justify
            || new.link_style != self.settings.link_style
            || new.include_images != self.settings.include_images
            || new.qr_code != self.settings.qr_code
            || new.side_by_side != self.settings.side_by_side
            || new.fit_to_sheets != self.settings.fit_to_sheets;
        let feed_changed = new.instapaper_feed != self.settings.instapaper_feed;
        self.settings = new;
        match self.settings.save(&library::settings_path(&self.data_dir)) {
            Ok(()) if layout_changed && !self.entries.is_empty() => self.toast(
                "Settings saved. New documents use them; click “Re-typeset” to update a saved one.",
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

    fn unprinted(&self) -> Vec<&FeedItem> {
        self.queue.state.unseen(&self.queue.items)
    }

    fn print_queue_items(&mut self, items: Vec<FeedItem>) {
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
                Event::Added { job, entry, print } => {
                    let title = entry.title.clone();
                    let id = entry.id.clone();
                    let (status, toast) = match print {
                        Some(Ok(rec)) => (
                            JobStatus::Done(format!("Printed {} page(s) on {}", rec.pages, rec.printer)),
                            (format!("Printed “{title}”"), ToastKind::Success),
                        ),
                        Some(Err(err)) => (
                            JobStatus::Failed(format!("Saved, but printing failed: {err}")),
                            (format!("Saved “{title}”, but printing failed"), ToastKind::Error),
                        ),
                        None => (
                            JobStatus::Done(format!("Saved · {} page(s)", entry.pages)),
                            (format!("Saved “{title}”"), ToastKind::Success),
                        ),
                    };
                    if let Some(j) = self.job_mut(job) {
                        j.status = status;
                        j.label = title;
                        j.entry_id = Some(id.clone());
                    }
                    self.toast(toast.0, toast.1);
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
                Event::Retypeset { job, entry } => {
                    if let Some(j) = self.job_mut(job) {
                        j.status = JobStatus::Done(format!("Re-typeset · {} page(s)", entry.pages));
                    }
                    self.reload_entries();
                    self.refresh_preview(&entry.id);
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
                    let preview = match result {
                        Ok(pages) => Preview::Ready(
                            pages
                                .into_iter()
                                .enumerate()
                                .take(40)
                                .map(|(i, page)| {
                                    let image = egui::ColorImage::from_rgba_unmultiplied(
                                        [page.width as usize, page.height as usize],
                                        &page.rgba,
                                    );
                                    ctx.load_texture(format!("{id}#{i}"), image, egui::TextureOptions::LINEAR)
                                })
                                .collect(),
                        ),
                        Err(err) => Preview::Failed(err),
                    };
                    self.previews.insert(id, preview);
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
                                self.print_queue_items(fresh);
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
                    let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
                    let urls = fetch::urls_in_text(&text);
                    if urls.is_empty() {
                        self.toast("There is no link on the clipboard.", ToastKind::Error);
                        self.show_window(ctx);
                    }
                    for url in urls {
                        self.queue_url(url, None);
                    }
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
        log::info!("showing the window");
        self.hidden = false;
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    /// Closing the window hides it to the tray (unless quitting or the
    /// setting is off).
    fn handle_close(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested && !self.quitting && self.settings.close_to_tray && self.tray.is_some() {
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
        let (print, settings) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::P),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma),
            )
        });
        if print {
            self.print_selected();
        }
        if settings && self.draft.is_none() {
            self.open_settings();
        }
    }

    // ----- UI --------------------------------------------------------------

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Topebag").family(theme::bold_family()).size(22.0).color(ACCENT));
            ui.label(RichText::new("links in, paper out").color(MUTED).italics());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⚙ Settings").on_hover_text("Settings (Ctrl+,)").clicked() {
                    self.open_settings();
                }
                let unprinted = self.unprinted().len();
                let label = match (self.settings.instapaper_feed.is_some(), unprinted) {
                    (false, _) => "Instapaper".to_string(),
                    (true, 0) => "Instapaper ✔".to_string(),
                    (true, n) => format!("Instapaper · {n}"),
                };
                let mut button = egui::Button::new(label);
                if unprinted > 0 {
                    button = button.fill(CARD_SELECTED).stroke(Stroke::new(1.0, ACCENT));
                }
                if ui.add(button).on_hover_text("Your Instapaper queue").clicked() {
                    self.queue.show_window = true;
                    if self.queue.items.is_empty() {
                        self.check_feed();
                    }
                }
                self.printer_picker(ui);
                let spread_label = if self.settings.side_by_side { "▮▮ Side by side" } else { "▮ One page" };
                if ui
                    .selectable_label(self.settings.side_by_side, spread_label)
                    .on_hover_text(
                        "Side by side: two half-size pages on each landscape sheet, with spacing tightened to save sheets.\n\
                         Applies to new documents; use Re-typeset to convert a saved one.",
                    )
                    .clicked()
                {
                    self.settings.side_by_side = !self.settings.side_by_side;
                    self.save_settings_quietly();
                    let msg = if self.settings.side_by_side {
                        "New documents print two pages per sheet. Re-typeset to convert a saved one."
                    } else {
                        "New documents print one page per sheet."
                    };
                    self.toast(msg, ToastKind::Info);
                }
                let mut auto = self.settings.auto_print;
                if ui
                    .checkbox(&mut auto, "Print automatically")
                    .on_hover_text("Send new documents straight to the printer after they are typeset")
                    .changed()
                {
                    self.settings.auto_print = auto;
                    self.save_settings_quietly();
                }
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let button_label = if self.settings.auto_print { "Fetch & print" } else { "Fetch & save" };
            let button_width = 128.0;
            let was_empty = self.url_input.trim().is_empty();
            let edit = egui::TextEdit::singleline(&mut self.url_input)
                .hint_text("Paste a link to an article or PDF (or several) and press Enter…")
                .desired_width(ui.available_width() - button_width - ui.spacing().item_spacing.x)
                .margin(Margin::symmetric(10, 7));
            let response = ui.add(edit);
            if self.focus_url {
                response.request_focus();
                self.focus_url = false;
            }
            // Pasting links into the empty bar submits them right away, so
            // "copy link → Ctrl+V" is all it takes.
            let pasted = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));
            let paste_submit =
                response.changed() && pasted && was_empty && !fetch::urls_in_text(&self.url_input).is_empty();
            let enter = paste_submit || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            let clicked = ui
                .add_sized(
                    [button_width, response.rect.height()],
                    egui::Button::new(RichText::new(button_label).color(Color32::WHITE).strong()).fill(ACCENT),
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
            .selected_text(format!("🖨 {}", truncate(&current, 28)))
            .width(240.0)
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
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text("🔍 Search titles, tags and article text")
                .desired_width(f32::INFINITY),
        )
        .on_hover_text("Words must all match. Use \"a phrase\", tag:name, or -word to exclude.");

        // Tag filter chips.
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
                    let text = RichText::new(format!("#{tag} {count}")).small();
                    if ui.selectable_label(active, text).clicked() {
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

        let terms = parse_query(&self.search);
        let visible: Vec<(usize, Option<String>)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.tag_filter.as_ref().is_none_or(|t| e.tags.contains(t)))
            .filter_map(|(i, e)| self.index.matches(e, &terms).map(|hit| (i, hit.snippet)))
            .collect();

        let mut clicked: Option<String> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, snippet) in &visible {
                let entry = &self.entries[*i];
                let selected = self.selected.as_deref() == Some(entry.id.as_str());
                let response = Frame::new()
                    .fill(if selected { CARD_SELECTED } else { CARD })
                    .stroke(Stroke::new(1.0, if selected { ACCENT } else { HAIRLINE }))
                    .corner_radius(8)
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(truncate(&entry.title, 110)).strong());
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if entry.kind == EntryKind::Pdf {
                                ui.label(RichText::new("PDF").small().strong().color(ACCENT_DARK));
                                ui.label(RichText::new("·").small().color(MUTED));
                            }
                            let site = entry.site_name.clone().unwrap_or_else(|| entry.domain());
                            ui.label(RichText::new(truncate(&site, 28)).small().color(MUTED));
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
                    clicked = Some(entry.id.clone());
                }
                response.on_hover_cursor(egui::CursorIcon::PointingHand);
                ui.add_space(2.0);
            }
            if visible.is_empty() {
                ui.label(RichText::new("No matches.").color(MUTED));
            }
        });
        if let Some(id) = clicked {
            self.select(id);
        }
    }

    fn detail(&mut self, ui: &mut Ui) {
        let Some(entry) = self.selected.as_ref().and_then(|id| self.entries.iter().find(|e| &e.id == id)).cloned()
        else {
            self.welcome(ui);
            return;
        };

        ui.add_space(4.0);
        if let Some(buffer) = self.renaming.as_mut() {
            let mut done: Option<bool> = None;
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(buffer)
                        .font(egui::FontId::new(20.0, theme::bold_family()))
                        .desired_width(ui.available_width() - 150.0),
                );
                response.request_focus();
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = Some(true);
                }
                if ui.button("Save").clicked() {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    done = Some(false);
                }
            });
            match done {
                Some(true) => {
                    let title = self.renaming.take().unwrap_or_default().trim().to_string();
                    if !title.is_empty() && title != entry.title {
                        let mut updated = entry.clone();
                        updated.title = title;
                        self.update_entry(updated);
                        if entry.kind == EntryKind::Article {
                            self.toast("Renamed. Re-typeset to update the printed title too.", ToastKind::Info);
                        }
                    }
                }
                Some(false) => self.renaming = None,
                None => {}
            }
        } else {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&entry.title).family(theme::bold_family()).size(24.0));
                if ui.small_button("✎").on_hover_text("Rename").clicked() {
                    self.renaming = Some(entry.title.clone());
                }
            });
        }
        let mut meta = Vec::new();
        if entry.kind == EntryKind::Pdf {
            meta.push("PDF document".to_string());
        }
        if let Some(b) = &entry.byline {
            meta.push(b.clone());
        }
        meta.push(entry.site_name.clone().unwrap_or_else(|| entry.domain()));
        if let Some(p) = &entry.published {
            meta.push(p.clone());
        }
        ui.label(RichText::new(meta.join("  ·  ")).color(MUTED));
        ui.horizontal_wrapped(|ui| {
            ui.hyperlink_to(truncate(&entry.url, 90), &entry.url);
        });
        let words = if entry.kind == EntryKind::Pdf { String::new() } else { format!(" · {} words", entry.word_count) };
        let browser = if entry.via_browser { " · loaded with the browser" } else { "" };
        let size = match (entry.spread, entry.pages) {
            (true, 1) => "1 sheet, side by side".to_string(),
            (true, n) => format!("{n} sheets, side by side"),
            (false, n) => format!("{n} page(s)"),
        };
        let tightened = if entry.spread && entry.density > 0.005 {
            format!(" (spacing tightened {:.0}%)", entry.density * 100.0)
        } else {
            String::new()
        };
        ui.label(
            RichText::new(format!(
                "Saved {} · {size}{tightened}{words}{browser} · {}",
                entry.saved_at.format("%-d %b %Y, %H:%M"),
                match entry.print_count() {
                    0 => "not printed yet".to_string(),
                    1 => "printed once".to_string(),
                    n => format!("printed {n} times"),
                }
            ))
            .small()
            .color(MUTED),
        );

        // Tags
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
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
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.new_tag).hint_text("+ add tag").desired_width(110.0),
            );
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
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(RichText::new("🖨 Print").color(Color32::WHITE).strong()).fill(ACCENT))
                .on_hover_text("Print again (Ctrl+P)")
                .clicked()
            {
                self.print_selected();
            }
            if ui.button("Open PDF").clicked() {
                if let Ok(path) = self.library.pdf_path(&entry.id) {
                    if let Err(err) = opener::open(&path) {
                        self.toast(format!("Could not open the PDF: {err}"), ToastKind::Error);
                    }
                }
            }
            if ui.button("Show in folder").clicked() {
                if let Ok(path) = self.library.pdf_path(&entry.id) {
                    if let Err(err) = opener::reveal(&path) {
                        self.toast(format!("Could not open the folder: {err}"), ToastKind::Error);
                    }
                }
            }
            if entry.kind == EntryKind::Article
                && ui
                    .button("Re-typeset")
                    .on_hover_text("Rebuild this PDF with your current settings (paper, font, columns…)")
                    .clicked()
            {
                self.retypeset_selected();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("Delete").color(DANGER))).clicked() {
                    self.confirm_delete = Some(entry.id.clone());
                }
            });
        });

        if !entry.warnings.is_empty() {
            ui.add_space(4.0);
            Frame::new()
                .fill(Color32::from_rgb(0xFF, 0xF6, 0xE0))
                .stroke(Stroke::new(1.0, Color32::from_rgb(0xF0, 0xD9, 0xA0)))
                .corner_radius(6)
                .inner_margin(Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    for w in entry.warnings.iter().take(4) {
                        ui.label(RichText::new(format!("⚠ {}", truncate(w, 200))).small());
                    }
                });
        }

        ui.add_space(8.0);
        ui.separator();
        self.preview(ui, &entry.id);
    }

    fn preview(&mut self, ui: &mut Ui, id: &str) {
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
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_space(10.0);
                    let width = (ui.available_width() - 40.0).clamp(240.0, 760.0);
                    for (i, tex) in pages.iter().enumerate() {
                        let size = tex.size_vec2();
                        let scaled = egui::vec2(width, width * size.y / size.x);
                        ui.vertical_centered(|ui| {
                            Frame::new()
                                .fill(Color32::WHITE)
                                .stroke(Stroke::new(1.0, HAIRLINE))
                                .shadow(egui::Shadow { offset: [0, 3], blur: 12, spread: 0, color: Color32::from_black_alpha(28) })
                                .show(ui, |ui| {
                                    ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), scaled)));
                                });
                            ui.label(RichText::new(format!("{} / {}", i + 1, pages.len())).small().color(MUTED));
                        });
                        ui.add_space(14.0);
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
            ui.label(RichText::new("Paste a link to an article or PDF and Topebag will clean it up,").color(MUTED));
            ui.label(RichText::new("typeset it as a print-friendly PDF and send it to your printer.").color(MUTED));
            ui.add_space(18.0);
            for (n, text) in [
                ("1", "Copy a link from your browser"),
                ("2", "Press Ctrl+V anywhere in this window, or paste into the bar above"),
                ("3", "Collect your pages from the printer — every PDF stays in your library"),
            ] {
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 470.0).max(0.0) / 2.0);
                    ui.label(RichText::new(n).strong().color(ACCENT));
                    ui.label(text);
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
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
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
                        if label.interact(Sense::click()).on_hover_text("Show in library").clicked() {
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

    fn instapaper_window(&mut self, ctx: &egui::Context) {
        if !self.queue.show_window {
            return;
        }
        let mut open = true;
        let mut to_print: Option<Vec<FeedItem>> = None;
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
                    ui.label(RichText::new("Print your Instapaper queue").strong());
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

                let unprinted: Vec<FeedItem> = self.unprinted().into_iter().cloned().collect();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(match unprinted.len() {
                        0 => "Everything in your queue has been printed.".to_string(),
                        1 => "1 article waiting to be printed".to_string(),
                        n => format!("{n} articles waiting to be printed"),
                    }).strong());
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
                    for item in &unprinted {
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
                                let host = url::Url::parse(&item.url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
                                let saved = item.saved.as_deref().map(|s| format!(" · saved {s}")).unwrap_or_default();
                                ui.label(RichText::new(format!("{}{saved}", host.trim_start_matches("www."))).small().color(MUTED));
                            });
                        });
                    }
                });

                let selected: Vec<FeedItem> =
                    unprinted.iter().filter(|i| !self.queue.unticked.contains(&i.id)).cloned().collect();
                ui.separator();
                ui.horizontal(|ui| {
                    let verb = if self.settings.auto_print { "Fetch & print" } else { "Fetch & save" };
                    let enabled = !selected.is_empty();
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(RichText::new(format!("{verb} {}", selected.len())).color(Color32::WHITE).strong())
                                .fill(ACCENT),
                        )
                        .clicked()
                    {
                        to_print = Some(selected.clone());
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new(format!("Skip {}", selected.len())))
                        .on_hover_text("Mark as handled without printing")
                        .clicked()
                    {
                        to_skip = Some(selected.iter().map(|i| i.id.clone()).collect());
                    }
                });
                ui.add_space(4.0);
                let mut auto = self.settings.instapaper_auto;
                if ui
                    .checkbox(&mut auto, format!(
                        "Print new saves automatically (checks every {} min while Topebag runs)",
                        self.settings.instapaper_interval_min
                    ))
                    .on_hover_text("Only articles saved after you turn this on are printed automatically.")
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
        if let Some(items) = to_print {
            self.print_queue_items(items);
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
        let Some(mut draft) = self.draft.take() else { return };
        let mut open = true;
        let mut action: Option<bool> = None; // Some(true) = save, Some(false) = cancel
        let has_browser = browser::find_browser().is_some();
        let has_tray = self.tray.is_some();

        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(500.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(560.0).show(ui, |ui| {
                    let s = &mut draft.settings;
                    section(ui, "Printing");
                    egui::Grid::new("settings-print").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
                        ui.label("Print automatically");
                        ui.checkbox(&mut s.auto_print, "after a link is typeset");
                        ui.end_row();
                        ui.label("Copies");
                        ui.add(egui::DragValue::new(&mut s.copies).range(1..=99));
                        ui.end_row();
                        ui.label("Quality");
                        combo(ui, "quality", &mut s.print_quality, &PrintQuality::ALL, |q| q.label());
                        ui.end_row();
                    });

                    section(ui, "Page layout");
                    egui::Grid::new("settings-layout").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
                        ui.label("Paper");
                        combo(ui, "paper", &mut s.paper, &Paper::ALL, |p| p.label());
                        ui.end_row();
                        ui.label("Typeface");
                        combo(ui, "typeface", &mut s.typeface, &Typeface::ALL, |t| t.label());
                        ui.end_row();
                        ui.label("Text size");
                        ui.add(egui::Slider::new(&mut s.font_size, 8.0..=16.0).step_by(0.5).suffix(" pt"));
                        ui.end_row();
                        ui.label("Spread");
                        ui.vertical(|ui| {
                            ui.checkbox(&mut s.side_by_side, "Two pages side by side on each sheet")
                                .on_hover_text("Each landscape sheet holds two half-size pages, like an open book");
                            ui.add_enabled(
                                s.side_by_side,
                                egui::Checkbox::new(&mut s.fit_to_sheets, "Tighten spacing to use the fewest sheets"),
                            )
                            .on_hover_text("Shrinks text, spacing and margins only as much as needed to save a sheet (text never below 8.5 pt)");
                        });
                        ui.end_row();
                        ui.label("Columns");
                        ui.add_enabled_ui(!s.side_by_side, |ui| {
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut s.columns, 1, "One");
                                ui.selectable_value(&mut s.columns, 2, "Two");
                            });
                        })
                        .response
                        .on_disabled_hover_text("Each half of a side-by-side sheet is one column");
                        ui.end_row();
                        ui.label("Alignment");
                        ui.checkbox(&mut s.justify, "Justify with hyphenation");
                        ui.end_row();
                    });

                    section(ui, "Content");
                    egui::Grid::new("settings-content").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
                        ui.label("Images");
                        ui.vertical(|ui| {
                            ui.checkbox(&mut s.include_images, "Include images");
                            ui.add_enabled(s.include_images, egui::Checkbox::new(&mut s.grayscale_images, "Convert to grayscale"));
                        });
                        ui.end_row();
                        ui.label("Links");
                        combo(ui, "links", &mut s.link_style, &LinkStyle::ALL, |l| l.label());
                        ui.end_row();
                        ui.label("QR code");
                        ui.checkbox(&mut s.qr_code, "Link back to the original");
                        ui.end_row();
                    });

                    section(ui, "JavaScript pages and sign-ins");
                    if !has_browser {
                        ui.label(RichText::new("Microsoft Edge or Google Chrome was not found, so this is unavailable.").small().color(DANGER));
                    }
                    egui::Grid::new("settings-browser").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
                        ui.label("Use the browser");
                        combo(ui, "browser", &mut s.browser_mode, &BrowserMode::ALL, |m| m.label());
                        ui.end_row();
                    });
                    ui.label(RichText::new(
                        "Pages that come back (nearly) empty are reloaded in Edge without a window. Sites you list \
                         below are always loaded that way, using Topebag's own browser profile.",
                    ).small().color(MUTED));
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
                            if let Err(err) = browser::open_sign_in_window(&browser::profile_dir(&self.data_dir), start.as_deref()) {
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
                        ui.checkbox(&mut s.instapaper_auto, "Print automatically");
                        ui.end_row();
                        ui.label("Check every");
                        ui.add(egui::DragValue::new(&mut s.instapaper_interval_min).range(5..=1440).suffix(" min"));
                        ui.end_row();
                    });

                    section(ui, "Background");
                    ui.add_enabled(has_tray, egui::Checkbox::new(&mut s.close_to_tray, "Keep running in the tray when the window is closed"));
                    ui.add_enabled(cfg!(windows), egui::Checkbox::new(&mut s.start_with_windows, "Start with Windows (in the tray)"));

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
            None if open => self.draft = Some(draft),
            None => {}
        }
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_delete.clone() else { return };
        let title = self.entries.iter().find(|e| e.id == id).map(|e| e.title.clone()).unwrap_or_default();
        let mut close = false;
        egui::Modal::new(egui::Id::unique("confirm-delete")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.label(RichText::new("Delete this document?").family(theme::bold_family()).size(18.0));
            ui.label(RichText::new(truncate(&title, 120)).color(MUTED));
            ui.label("The PDF and its images will be removed from the library.");
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("Delete").color(Color32::WHITE).strong()).fill(DANGER)).clicked() {
                    match topebag::pipeline::delete_entry(&self.library, &id) {
                        Ok(()) => {
                            self.previews.remove(&id);
                            self.index.remove(&id);
                            if self.selected.as_deref() == Some(id.as_str()) {
                                self.selected = None;
                            }
                            self.toast("Deleted.", ToastKind::Info);
                        }
                        Err(err) => self.toast(format!("Could not delete: {err:#}"), ToastKind::Error),
                    }
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close {
            self.confirm_delete = None;
            self.reload_entries();
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
        log::trace!("logic tick (hidden: {})", self.hidden);
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
            .default_size(130.0)
            .size_range(70.0..=320.0)
            .frame(Frame::new().fill(CARD).inner_margin(Margin::symmetric(16, 8)).stroke(Stroke::new(1.0, HAIRLINE)))
            .show(ui, |ui| self.activity(ui));

        egui::Panel::left("library")
            .resizable(true)
            .default_size(330.0)
            .size_range(250.0..=520.0)
            .frame(Frame::new().fill(theme::PAPER).inner_margin(Margin::symmetric(12, 8)).stroke(Stroke::new(1.0, HAIRLINE)))
            .show(ui, |ui| self.library_panel(ui));

        egui::CentralPanel::default_margins()
            .frame(Frame::new().fill(theme::PAPER).inner_margin(Margin::symmetric(22, 12)))
            .show(ui, |ui| self.detail(ui));

        self.instapaper_window(&ctx);
        self.settings_window(&ctx);
        self.delete_dialog(&ctx);
        self.toasts(&ctx);
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
