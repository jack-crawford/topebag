//! Sending documents to a printer.
//!
//! ## Windows
//!
//! Topebag prints through the classic Win32 GDI print path, which every
//! Windows printer driver supports — no PDF reader, no Edge, no extra
//! software. Each page is rasterised by Typst at the printer's resolution
//! (capped by [`crate::settings::PrintQuality`]) and blitted onto the page
//! with `StretchDIBits`, aligned to the physical paper so the document's own
//! margins are preserved. The job appears in the Windows print queue like
//! any other.
//!
//! Printing to a file is supported via `DOCINFO.lpszOutput`; combined with
//! the built-in *Microsoft Print to PDF* printer this gives a silent,
//! paper-free way to test the whole print path (`topebag add <url>
//! --print-to-file out.pdf`).
//!
//! ## macOS and Linux
//!
//! CUPS accepts PDF directly, so the saved PDF is handed to `lp`.

use std::path::Path;

use anyhow::Result;
use typst_layout::PagedDocument;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as platform;

#[cfg(not(windows))]
mod cups;
#[cfg(not(windows))]
use self::cups as platform;

/// The printer used for paper-free test prints on Windows.
pub const PRINT_TO_PDF_PRINTER: &str = "Microsoft Print to PDF";

/// An installed printer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterInfo {
    pub name: String,
    pub is_default: bool,
}

/// Everything needed to print one document.
pub struct PrintRequest<'a> {
    /// The laid-out document (rasterised on Windows).
    pub document: &'a PagedDocument,
    /// The saved PDF (sent as-is through CUPS on macOS/Linux).
    pub pdf_path: &'a Path,
    /// Name shown in the print queue.
    pub job_name: &'a str,
    /// Printer name; `None` = system default.
    pub printer: Option<&'a str>,
    pub copies: u16,
    /// Maximum raster resolution in dots per inch.
    pub dpi: u32,
    /// Spool the job to this file instead of the device (Windows), e.g. to
    /// produce a PDF through "Microsoft Print to PDF" without a dialog.
    pub output_file: Option<&'a Path>,
}

/// Result of a successful print job.
#[derive(Debug, Clone)]
pub struct PrintOutcome {
    pub printer: String,
    /// Physical pages sent (pages × copies).
    pub sheets: usize,
}

/// List installed printers.
pub fn printers() -> Result<Vec<PrinterInfo>> {
    platform::printers()
}

/// Name of the system default printer, if one is configured.
pub fn default_printer() -> Option<String> {
    platform::default_printer()
}

/// Print a document. Blocks until the job has been handed to the spooler.
pub fn print(request: &PrintRequest) -> Result<PrintOutcome> {
    platform::print(request)
}
