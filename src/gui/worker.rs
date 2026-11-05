//! Background work for the desktop app.
//!
//! The UI thread must never block, so all slow work happens elsewhere:
//!
//! * One **job thread** processes [`Request`]s in order (fetching, typesetting,
//!   printing). Running jobs one at a time keeps print order predictable and
//!   avoids hammering sites with parallel downloads.
//! * **Previews** are rendered on short-lived threads of their own, so
//!   browsing the library stays snappy even while a long job is running.
//!
//! Results come back as [`Event`]s over a channel; every send is followed by
//! `request_repaint()` so the UI wakes up and picks them up.

use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui;

use topebag::instapaper::{self, FeedItem};
use topebag::library::{Entry, Library, PrintRecord};
use topebag::pipeline::{self, PrintTarget};
use topebag::print::{self, PrinterInfo};
use topebag::search::SearchIndex;
use topebag::settings::Settings;
use topebag::typeset::RasterPage;
use url::Url;

/// Identifies a job in the activity list.
pub type JobId = u64;

/// Work for the job thread.
pub enum Request {
    Add { job: JobId, url: Url, settings: Settings, print: bool },
    Print { job: JobId, id: String, settings: Settings },
    Retypeset { job: JobId, id: String, settings: Settings },
    RefreshPrinters,
    CheckFeed { url: Url },
}

/// Results reported back to the UI.
pub enum Event {
    Progress { job: JobId, stage: String },
    Added { job: JobId, entry: Box<Entry>, print: Option<Result<PrintRecord, String>> },
    Printed { job: JobId, record: PrintRecord },
    Retypeset { job: JobId, entry: Box<Entry> },
    Failed { job: JobId, error: String },
    Printers(Result<Vec<PrinterInfo>, String>),
    Preview { id: String, result: Result<Vec<RasterPage>, String> },
    Feed(Result<Vec<FeedItem>, String>),
    Index(Box<SearchIndex>),
}

/// Handle to the background threads.
pub struct Worker {
    requests: Sender<Request>,
    events_tx: Sender<Event>,
    pub events: Receiver<Event>,
    library: Library,
    ctx: egui::Context,
}

/// Resolution of on-screen previews.
const PREVIEW_DPI: f32 = 120.0;

impl Worker {
    pub fn spawn(ctx: egui::Context, library: Library) -> Worker {
        let (requests, request_rx) = channel::<Request>();
        let (events_tx, events) = channel::<Event>();

        let thread_events = events_tx.clone();
        let thread_ctx = ctx.clone();
        let thread_library = library.clone();
        std::thread::Builder::new()
            .name("topebag-jobs".into())
            .spawn(move || {
                for request in request_rx {
                    process(request, &thread_library, &thread_events, &thread_ctx);
                }
            })
            .expect("failed to start worker thread");

        Worker { requests, events_tx, events, library, ctx }
    }

    pub fn send(&self, request: Request) {
        let _ = self.requests.send(request);
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

    /// Render previews for entry `id` on a separate thread.
    pub fn preview(&self, id: String) {
        let tx = self.events_tx.clone();
        let ctx = self.ctx.clone();
        let library = self.library.clone();
        std::thread::spawn(move || {
            let result = pipeline::render_previews(&library, &id, PREVIEW_DPI).map_err(|e| format!("{e:#}"));
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
        Request::Add { job, url, settings, print } => {
            let target = PrintTarget::default();
            let mut progress = |stage: pipeline::Stage| send(Event::Progress { job, stage: stage.to_string() });
            match pipeline::add_url(library, &settings, &url, print.then_some(&target), &mut progress) {
                Ok(outcome) => send(Event::Added {
                    job,
                    entry: Box::new(outcome.entry),
                    print: outcome.print.map(|r| r.map_err(|e| format!("{e:#}"))),
                }),
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
        Request::Retypeset { job, id, settings } => {
            send(Event::Progress { job, stage: "Typesetting…".into() });
            match pipeline::retypeset(library, &id, &settings) {
                Ok(entry) => send(Event::Retypeset { job, entry: Box::new(entry) }),
                Err(err) => send(Event::Failed { job, error: format!("{err:#}") }),
            }
        }
        Request::RefreshPrinters => send(Event::Printers(print::printers().map_err(|e| format!("{e:#}")))),
        Request::CheckFeed { url } => send(Event::Feed(instapaper::fetch_feed(&url).map_err(|e| format!("{e:#}")))),
    }
}
