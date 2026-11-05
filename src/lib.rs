//! # Topebag
//!
//! Topebag turns web pages into clean, print-ready PDFs and sends them
//! straight to your printer. Every document is also kept in a local library
//! so it can be previewed, re-printed or re-typeset later.
//!
//! The crate is split into small, single-purpose modules that form a
//! pipeline:
//!
//! ```text
//!   URL ──► fetch ──► extract ──► convert ──► typeset ──► PDF ──► print
//!                                    │            │
//!                                 images      library (on disk)
//! ```
//!
//! | Module        | Responsibility                                               |
//! |---------------|--------------------------------------------------------------|
//! | [`fetch`]     | HTTP download of pages, PDFs, images and feeds               |
//! | [`browser`]   | Headless Edge/Chrome for JavaScript-heavy and signed-in pages |
//! | [`extract`]   | Reader-mode extraction (title, byline, main content)         |
//! | [`images`]    | Downloading, normalising and caching article images          |
//! | [`convert`]   | Cleaned HTML → Typst markup                                  |
//! | [`typeset`]   | Embedded Typst compiler: markup → pages → PDF / raster       |
//! | [`library`]   | The on-disk library of saved documents                       |
//! | [`print`]     | Sending documents to a printer (Win32 GDI or CUPS `lp`)      |
//! | [`pipeline`]  | Glue that runs the steps above for one URL                   |
//! | [`instapaper`]| Reading-queue feeds (Instapaper Unread RSS, any RSS/Atom)    |
//! | [`search`]    | Library search over titles, tags and full text               |
//! | [`instance`]  | Single running instance (hand-off from later launches)       |
//! | [`autostart`] | "Start with Windows" (per-user Run key)                      |
//! | [`settings`]  | User preferences, persisted as JSON                          |
//!
//! The desktop UI lives in the binary (`src/gui`), and a small command-line
//! interface lives in `src/cli.rs`; both are thin layers over [`pipeline`].

pub mod autostart;
pub mod browser;
pub mod convert;
pub mod extract;
pub mod fetch;
pub mod images;
pub mod instance;
pub mod instapaper;
pub mod library;
pub mod pipeline;
pub mod print;
pub mod search;
pub mod settings;
pub mod typeset;
