//! Background work for the desktop app.
//!
//! The UI thread must never block, so all slow work happens elsewhere:
//!
//! * One **job thread** processes [`Request`]s in order (fetching links,
//!   printing, feed checks). Running jobs one at a time keeps print order
//!   predictable and avoids hammering sites with parallel downloads.
//! * One **rebuild thread** turns the editor's drafts into fresh PDFs and
//!   preview pages. While you type, it only ever works on the *latest* draft
//!   of each document, so a burst of edits costs one rebuild, not dozens.
//! * **Previews** of unchanged documents and the **search index** are built
//!   on short-lived threads of their own.
//!
//! Results come back as [`Event`]s over a channel; every send is followed by
//! `request_repaint()` so the UI wakes up and picks them up.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui;

use topebag::document::Draft;
use topebag::instapaper::{self, FeedItem};
use topebag::library::{Entry, Library, PrintRecord};
use topebag::pipeline::{self, PrintTarget};
use topebag::print::{self, PrinterInfo};
use topebag::search::SearchIndex;
use topebag::settings::Settings;
use topebag::typeset::{self, RasterPage};
use url::Url;

/// Identifies a job in the activity list.
pub type JobId = u64;

/// Work for the job thread.
pub enum Request {
    Add { job: JobId, url: Url, settings: Settings },
    Print { job: JobId, id: String, settings: Settings },
    RefreshPrinters,
    CheckFeed { url: Url },
}

/// A draft to rebuild (see [`Worker::rebuild`]).
pub struct Rebuild {
    pub id: String,
    pub draft: Draft,
    /// Download images that aren't stored yet.
    pub refill_images: bool,
}

/// Results reported back to the UI.
pub enum Event {
    Progress { job: JobId, stage: String },
    Added { job: JobId, entry: Box<Entry> },
    Printed { job: JobId, record: PrintRecord },
    Failed { job: JobId, error: String },
    Printers(Result<Vec<PrinterInfo>, String>),
    Preview { id: String, result: Result<Vec<RasterPage>, String> },
    Rebuilt { id: String, result: Result<(Box<Entry>, Vec<RasterPage>), String> },
    Feed(Result<Vec<FeedItem>, String>),
    Index(Box<SearchIndex>),
}

/// Handle to the background threads.
pub struct Worker {
    requests: Sender<Request>,
    rebuilds: Sender<Rebuild>,
    events_tx: Sender<Event>,
    pub events: Receiver<Event>,
    library: Library,
    ctx: egui::Context,
}

/// Resolution of on-screen previews.
const PREVIEW_DPI: f32 = 120.0;
/// Pages rendered for a preview (long documents still print in full).
const PREVIEW_PAGES: usize = 40;

impl Worker {
    pub fn spawn(ctx: egui::Context, library: Library) -> Worker {
        let (requests, request_rx) = channel::<Request>();
        let (rebuilds, rebuild_rx) = channel::<Rebuild>();
        let (events_tx, events) = channel::<Event>();

        let (job_events, job_ctx, job_library) = (events_tx.clone(), ctx.clone(), library.clone());
        std::thread::Builder::new()
            .name("topebag-jobs".into())
            .spawn(move || {
                for request in request_rx {
                    process(request, &job_library, &job_events, &job_ctx);
                }
            })
            .expect("failed to start worker thread");

        let (rb_events, rb_ctx, rb_library) = (events_tx.clone(), ctx.clone(), library.clone());
        std::thread::Builder::new()
            .name("topebag-rebuild".into())
            .spawn(move || {
                while let Ok(first) = rebuild_rx.recv() {
                    // Coalesce: keep only the newest draft per document, but
                    // never drop a document's pending edits.
                    let mut pending: Vec<String> = vec![first.id.clone()];
                    let mut latest: HashMap<String, Rebuild> = HashMap::from([(first.id.clone(), first)]);
                    while let Ok(next) = rebuild_rx.try_recv() {
                        if !latest.contains_key(&next.id) {
                            pending.push(next.id.clone());
                        }
                        let refill = latest.get(&next.id).is_some_and(|r| r.refill_images);
                        let mut next = next;
                        next.refill_images |= refill;
                        latest.insert(next.id.clone(), next);
                    }
                    for id in pending {
                        let Some(job) = latest.remove(&id) else { continue };
                        let result = pipeline::rebuild(&rb_library, &job.id, &job.draft, job.refill_images)
                            .map(|rebuilt| {
                                let pages = (0..rebuilt.compiled.page_count().min(PREVIEW_PAGES))
                                    .filter_map(|i| typeset::render_page(&rebuilt.compiled.document, i, PREVIEW_DPI))
                                    .collect();
                                (Box::new(rebuilt.entry), pages)
                            })
                            .map_err(|e| format!("{e:#}"));
                        let _ = rb_events.send(Event::Rebuilt { id: job.id, result });
                        rb_ctx.request_repaint();
                    }
                }
            })
            .expect("failed to start rebuild thread");

        Worker { requests, rebuilds, events_tx, events, library, ctx }
    }

    pub fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }

    /// Rebuild a document from an edited draft (latest wins).
    pub fn rebuild(&self, rebuild: Rebuild) {
        let _ = self.rebuilds.send(rebuild);
    }

    /// Build the full-text search index on a separate thread.
    pub fn build_index(&self, entries: Vec<Entry>) {
        let tx = self.events_tx.clone();
        let ctx = self.ctx.clone();
        let library = self.library.clone();
        std::thread::spawn(move || {
            let index = SearchIndex::build(&library, &entries);
            let _ = tx.send(Event::Index(Box::new(index)));
            ctx.request_repaint();
        });
    }

    /// Wake the UI every `interval` so timers (Instapaper checks) run even
    /// while the window is hidden in the tray.
    pub fn start_heartbeat(&self, interval: std::time::Duration) {
        let ctx = self.ctx.clone();
        std::thread::Builder::new()
            .name("topebag-heartbeat".into())
            .spawn(move || loop {
                std::thread::sleep(interval);
                ctx.request_repaint();
            })
            .expect("failed to start heartbeat thread");
    }

    /// Render previews of a saved document on a separate thread.
    pub fn preview(&self, id: String) {
        let tx = self.events_tx.clone();
        let ctx = self.ctx.clone();
        let library = self.library.clone();
        std::thread::spawn(move || {
            let result = pipeline::render_previews(&library, &id, PREVIEW_DPI)
                .map(|mut pages| {
                    pages.truncate(PREVIEW_PAGES);
                    pages
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Preview { id, result });
            ctx.request_repaint();
        });
    }
}

fn process(request: Request, library: &Library, events: &Sender<Event>, ctx: &egui::Context) {
    let send = |event: Event| {
        let _ = events.send(event);
        ctx.request_repaint();
    };
    match request {
        Request::Add { job, url, settings } => {
            let mut progress = |stage: pipeline::Stage| send(Event::Progress { job, stage: stage.to_string() });
            match pipeline::add_url(library, &settings, &url, None, &mut progress) {
                Ok(outcome) => send(Event::Added { job, entry: Box::new(outcome.entry) }),
                Err(err) => send(Event::Failed { job, error: format!("{err:#}") }),
            }
        }
        Request::Print { job, id, settings } => {
            let printer = settings.printer.clone().or_else(print::default_printer).unwrap_or_else(|| "printer".into());
            send(Event::Progress { job, stage: format!("Sending to {printer}…") });
            match pipeline::print_entry(library, &id, &settings, &PrintTarget::default()) {
                Ok(record) => send(Event::Printed { job, record }),
                Err(err) => send(Event::Failed { job, error: format!("{err:#}") }),
            }
        }
        Request::RefreshPrinters => send(Event::Printers(print::printers().map_err(|e| format!("{e:#}")))),
        Request::CheckFeed { url } => send(Event::Feed(instapaper::fetch_feed(&url).map_err(|e| format!("{e:#}")))),
    }
}
