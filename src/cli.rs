//! Command-line interface.
//!
//! Every CLI command is a thin wrapper over [`topebag::pipeline`], so the
//! CLI and the desktop app always behave the same. Useful for scripting,
//! for queueing links from other tools, and for testing the print path
//! without paper (`--print-to-file`).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use topebag::instapaper::{self, QueueState};
use topebag::library::{self, Library};
use topebag::pipeline::{self, PrintTarget, Stage};
use topebag::search::{SearchIndex, parse_query};
use topebag::settings::Settings;
use topebag::{browser, fetch, print};

#[derive(Parser)]
#[command(
    name = "topebag",
    version,
    about = "Paste a URL, get a cleanly typeset, print-ready PDF — sent straight to your printer.",
    long_about = "Topebag turns web articles into print-friendly PDFs, keeps them in a local \
                  library and sends them to your printer.\n\nRun without a command to open the \
                  desktop app.",
    after_help = "Data lives in %LOCALAPPDATA%\\Topebag (override with TOPEBAG_HOME)."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// What the executable should do.
pub enum Mode {
    /// Open the desktop app, queueing `urls`; `background` starts it hidden
    /// in the system tray.
    Gui { urls: Vec<String>, background: bool },
    Command(Command),
}

#[derive(Subcommand)]
pub enum Command {
    /// Fetch URLs into the library as ready-to-print documents.
    Add {
        /// One or more web addresses.
        #[arg(required = true)]
        urls: Vec<String>,
        /// Also print each document once it's ready.
        #[arg(long)]
        print: bool,
        /// Printer to use (implies --print; defaults to settings, then the system default).
        #[arg(long)]
        printer: Option<String>,
        /// Print to a file instead of paper (uses "Microsoft Print to PDF" on Windows).
        #[arg(long, value_name = "FILE")]
        print_to_file: Option<PathBuf>,
        /// Lay out two pages side by side on each sheet (overrides settings).
        #[arg(long)]
        side_by_side: bool,
    },
    /// Print a saved document again.
    Print {
        /// Entry id (see `topebag list`).
        id: String,
        #[arg(long)]
        printer: Option<String>,
        #[arg(long, value_name = "FILE")]
        print_to_file: Option<PathBuf>,
    },
    /// Rebuild a saved document from its draft (edits and formatting),
    /// downloading any images that were missing.
    Retypeset {
        /// Entry id (see `topebag list`).
        id: String,
        /// Switch this document to two pages side by side on each sheet.
        #[arg(long, conflicts_with = "one_page")]
        side_by_side: bool,
        /// Switch this document back to one page per sheet.
        #[arg(long)]
        one_page: bool,
    },
    /// List saved documents, newest first.
    List,
    /// List installed printers.
    Printers,
    /// Show the data folder and current settings.
    Info,
    /// Show your Instapaper queue (unread saves not fetched yet), or fetch it.
    Instapaper {
        /// Save your private feed URL (instapaper.com → Unread → Downloads menu → RSS Feed).
        #[arg(long, value_name = "URL")]
        feed: Option<String>,
        /// Fetch every new item into the library.
        #[arg(long)]
        fetch_all: bool,
        /// With --fetch-all: also print each document.
        #[arg(long)]
        print: bool,
        /// Mark every item as handled without fetching it.
        #[arg(long, conflicts_with = "fetch_all")]
        skip_all: bool,
    },
    /// Add or remove tags, e.g. `topebag tag <id> +reading -todo`.
    Tag {
        /// Entry id (see `topebag list`).
        id: String,
        /// `+tag` adds, `-tag` removes (a bare word adds).
        #[arg(allow_hyphen_values = true)]
        changes: Vec<String>,
    },
    /// Search titles, sites, tags and article text (supports "phrases", tag:x, -word).
    Search {
        #[arg(required = true, allow_hyphen_values = true)]
        query: Vec<String>,
    },
    /// Open Topebag's own browser window to sign in to a site. With a URL,
    /// that site is fetched through the browser from now on.
    SignIn {
        url: Option<String>,
    },
    /// Open the desktop app (the default when no command is given),
    /// optionally queueing links to fetch as soon as it starts. If Topebag is
    /// already running, the links are handed to it.
    Gui {
        /// Links to fetch on startup.
        urls: Vec<String>,
        /// Start hidden in the system tray.
        #[arg(long)]
        background: bool,
    },
}

/// Parse `std::env::args`. Exits the process for `--help` / `--version`.
pub fn parse() -> Mode {
    match Cli::parse().command {
        None => Mode::Gui { urls: Vec::new(), background: false },
        Some(Command::Gui { urls, background }) => Mode::Gui { urls, background },
        Some(command) => Mode::Command(command),
    }
}

/// Run a CLI command and turn its result into an exit code.
pub fn run(command: Command) -> ExitCode {
    match execute(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(command: Command) -> Result<()> {
    let data_dir = library::data_dir();
    let library = Library::open(&data_dir)?;
    let settings = Settings::load(&library::settings_path(&data_dir))?;

    match command {
        Command::Add { urls, print, printer, print_to_file, side_by_side } => {
            let mut settings = settings;
            settings.format.side_by_side |= side_by_side;
            let target = print_target(printer, print_to_file)?;
            let print_requested = print || target.output_file.is_some() || target.printer.is_some();
            let mut failures = 0;
            for raw in &urls {
                let url = match fetch::parse_url(raw) {
                    Ok(url) => url,
                    Err(err) => {
                        eprintln!("✗ {raw}: {err:#}");
                        failures += 1;
                        continue;
                    }
                };
                println!("→ {url}");
                let mut progress = |stage: Stage| println!("  {stage}");
                match pipeline::add_url(&library, &settings, &url, print_requested.then_some(&target), &mut progress) {
                    Ok(outcome) => {
                        let e = &outcome.entry;
                        let size = if e.spread {
                            format!("{} sheet(s) side by side, spacing tightened {:.0}%", e.pages, e.density * 100.0)
                        } else {
                            format!("{} page(s)", e.pages)
                        };
                        println!("✓ \"{}\" — {size}, saved as {}", e.title, e.id);
                        println!("  {}", library.pdf_path(&e.id)?.display());
                        for w in &e.warnings {
                            println!("  note: {w}");
                        }
                        match outcome.print {
                            Some(Ok(rec)) => println!("  printed {} sheet(s) on {}", rec.pages, rec.printer),
                            Some(Err(err)) => {
                                eprintln!("  ✗ saved, but printing failed: {err:#}");
                                failures += 1;
                            }
                            None => {}
                        }
                    }
                    Err(err) => {
                        eprintln!("✗ {url}: {err:#}");
                        failures += 1;
                    }
                }
            }
            if failures > 0 {
                bail!("{failures} of {} URL(s) had problems", urls.len());
            }
        }
        Command::Print { id, printer, print_to_file } => {
            let target = print_target(printer, print_to_file)?;
            let rec = pipeline::print_entry(&library, &id, &settings, &target)?;
            println!("printed {} sheet(s) on {}", rec.pages, rec.printer);
        }
        Command::Retypeset { id, side_by_side, one_page } => {
            let entry = match pipeline::load_draft(&library, &id, &settings.format)? {
                Some(mut draft) => {
                    if side_by_side || one_page {
                        draft.format.side_by_side = side_by_side;
                    }
                    pipeline::rebuild(&library, &id, &draft, true)?.entry
                }
                None => library.entry(&id)?, // PDFs print as they are
            };
            if entry.spread {
                println!(
                    "re-typeset \"{}\" — {} sheet(s) side by side, spacing tightened {:.0}%",
                    entry.title,
                    entry.pages,
                    entry.density * 100.0
                );
            } else {
                println!("re-typeset \"{}\" — {} page(s)", entry.title, entry.pages);
            }
        }
        Command::List => {
            let entries = library.entries()?;
            if entries.is_empty() {
                println!("The library is empty. Add something with: topebag add <url>");
            }
            for e in entries {
                let printed = match e.print_count() {
                    0 => "not printed".to_string(),
                    n => format!("printed ×{n}"),
                };
                let tags = if e.tags.is_empty() { String::new() } else { format!(" · #{}", e.tags.join(" #")) };
                let kind = if e.kind == library::EntryKind::Pdf { " · PDF" } else { "" };
                println!("{}\n    {} · {} · {} page(s){kind} · {printed}{tags}", e.id, e.title, e.domain(), e.pages);
            }
        }
        Command::Printers => {
            let printers = print::printers()?;
            if printers.is_empty() {
                println!("No printers found.");
            }
            for p in printers {
                println!("{}{}", p.name, if p.is_default { "  (default)" } else { "" });
            }
        }
        Command::Info => {
            println!("Data folder: {}", data_dir.display());
            println!("Library:     {}", library.root().display());
            println!("Settings:    {}", library::settings_path(&data_dir).display());
            println!("{}", serde_json::to_string_pretty(&settings)?);
        }
        Command::Instapaper { feed, fetch_all, print, skip_all } => {
            let mut settings = settings;
            if let Some(feed) = feed {
                let url = instapaper::parse_feed_url(&feed)?;
                settings.instapaper_feed = Some(url.to_string());
                settings.save(&library::settings_path(&data_dir))?;
                println!("Saved feed URL.");
            }
            let Some(feed) = settings.instapaper_feed.clone() else {
                bail!(
                    "no Instapaper feed set. On instapaper.com open Unread → Downloads menu → RSS Feed, \
                     copy the link, then run: topebag instapaper --feed <URL>"
                );
            };
            let items = instapaper::fetch_feed(&url::Url::parse(&feed)?)?;
            let state_path = instapaper::state_path(&data_dir);
            let mut state = QueueState::load(&state_path);
            state.observe(&items);
            let queue: Vec<_> = state.unseen(&items).into_iter().cloned().collect();
            if queue.is_empty() {
                println!("Your Instapaper queue is all printed. ({} item(s) in the feed)", items.len());
            } else if skip_all {
                state.mark_seen(queue.iter().map(|i| i.id.as_str()));
                println!("Marked {} item(s) as handled.", queue.len());
            } else if fetch_all {
                let target = PrintTarget::default();
                let print = print;
                for item in &queue {
                    println!("→ {}", item.title);
                    let url = fetch::parse_url(&item.url)?;
                    match pipeline::add_url(&library, &settings, &url, print.then_some(&target), &mut |_| {}) {
                        Ok(outcome) => {
                            let printed = match outcome.print {
                                Some(Ok(rec)) => format!(", printed on {}", rec.printer),
                                Some(Err(err)) => format!(", printing failed: {err:#}"),
                                None => String::new(),
                            };
                            println!("  ✓ {} page(s){printed}", outcome.entry.pages);
                            state.mark_seen([item.id.as_str()]);
                        }
                        Err(err) => eprintln!("  ✗ {err:#}"),
                    }
                    state.save(&state_path)?;
                }
            } else {
                println!("{} unprinted item(s) in your Instapaper queue:", queue.len());
                for item in &queue {
                    println!("  • {}\n    {}", item.title, item.url);
                }
                println!("\nFetch them with: topebag instapaper --fetch-all   (add --print to print too)");
            }
            state.save(&state_path)?;
        }
        Command::Tag { id, changes } => {
            let mut entry = library.entry(&id)?;
            for change in &changes {
                if let Some(tag) = change.strip_prefix('-') {
                    entry.remove_tag(tag);
                } else {
                    entry.add_tag(change.strip_prefix('+').unwrap_or(change));
                }
            }
            library.save(&entry)?;
            println!("{}: {}", entry.title, if entry.tags.is_empty() { "(no tags)".to_string() } else { entry.tags.join(", ") });
        }
        Command::Search { query } => {
            let entries = library.entries()?;
            let index = SearchIndex::build(&library, &entries);
            let terms = parse_query(&query.join(" "));
            let mut found = 0;
            for entry in &entries {
                if let Some(hit) = index.matches(entry, &terms) {
                    found += 1;
                    println!("{}\n    {} · {}", entry.id, entry.title, entry.domain());
                    if let Some(snippet) = hit.snippet {
                        println!("    “{snippet}”");
                    }
                }
            }
            println!("{found} result(s)");
        }
        Command::SignIn { url } => {
            let mut settings = settings;
            if let Some(raw) = &url {
                let parsed = fetch::parse_url(raw)?;
                let site = browser::normalize_site(parsed.host_str().unwrap_or(""));
                if !settings.browser_sites.contains(&site) {
                    settings.browser_sites.push(site.clone());
                    settings.sanitize();
                    settings.save(&library::settings_path(&data_dir))?;
                }
                println!("{site} will be fetched through Topebag's browser from now on.");
            }
            browser::open_sign_in_window(&browser::profile_dir(&data_dir), url.as_deref())?;
            println!("Sign in in the browser window that opened, then close it before fetching.");
        }
        Command::Gui { .. } => unreachable!("handled by parse()"),
    }
    Ok(())
}

fn print_target(printer: Option<String>, print_to_file: Option<PathBuf>) -> Result<PrintTarget> {
    let output_file = match print_to_file {
        Some(path) => {
            let path = if path.is_absolute() { path } else { std::env::current_dir()?.join(path) };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
            Some(path)
        }
        None => None,
    };
    let printer = match (&printer, &output_file) {
        (None, Some(_)) if cfg!(windows) => Some(print::PRINT_TO_PDF_PRINTER.to_string()),
        _ => printer,
    };
    Ok(PrintTarget { printer, output_file })
}
