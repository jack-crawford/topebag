//! User preferences.
//!
//! Settings are stored as pretty-printed JSON in `settings.json` inside the
//! Topebag data directory (see [`crate::library::data_dir`]). Unknown or
//! missing fields fall back to their defaults, so older settings files keep
//! working when new options are added.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Paper sizes Topebag can typeset for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Paper {
    Letter,
    Legal,
    A4,
    A5,
}

impl Paper {
    /// All variants, in the order they appear in the UI.
    pub const ALL: [Paper; 4] = [Paper::Letter, Paper::A4, Paper::Legal, Paper::A5];

    /// The identifier Typst's `page(paper: ..)` expects.
    pub fn typst_name(self) -> &'static str {
        match self {
            Paper::Letter => "us-letter",
            Paper::Legal => "us-legal",
            Paper::A4 => "a4",
            Paper::A5 => "a5",
        }
    }

    /// Human readable label.
    pub fn label(self) -> &'static str {
        match self {
            Paper::Letter => "US Letter (8.5 × 11 in)",
            Paper::Legal => "US Legal (8.5 × 14 in)",
            Paper::A4 => "A4 (210 × 297 mm)",
            Paper::A5 => "A5 (148 × 210 mm)",
        }
    }

    /// Best guess for the user's paper size, based on the OS region settings.
    pub fn system_default() -> Paper {
        #[cfg(windows)]
        {
            use windows::Win32::Globalization::{
                GetLocaleInfoEx, LOCALE_IPAPERSIZE, LOCALE_RETURN_NUMBER,
            };
            use windows::core::PCWSTR;

            // LOCALE_IPAPERSIZE: 1 = Letter, 5 = Legal, 8 = A3, 9 = A4.
            let mut buf = [0u16; 2];
            // SAFETY: `buf` is large enough for the DWORD that
            // LOCALE_RETURN_NUMBER writes, and a null locale name means
            // "the current user's locale".
            let written = unsafe {
                GetLocaleInfoEx(
                    PCWSTR::null(),
                    LOCALE_IPAPERSIZE | LOCALE_RETURN_NUMBER,
                    Some(&mut buf),
                )
            };
            if written > 0 {
                let value = u32::from(buf[0]) | (u32::from(buf[1]) << 16);
                return match value {
                    1 => Paper::Letter,
                    5 => Paper::Legal,
                    _ => Paper::A4,
                };
            }
            Paper::Letter
        }
        #[cfg(not(windows))]
        {
            // Rough heuristic: the US and Canada use Letter, everyone else A4.
            let lang = std::env::var("LC_PAPER")
                .or_else(|_| std::env::var("LANG"))
                .unwrap_or_default();
            if lang.contains("_US") || lang.contains("_CA") {
                Paper::Letter
            } else {
                Paper::A4
            }
        }
    }
}

/// Typeface used for body text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Typeface {
    /// Libertinus Serif (bundled) — a classic book face.
    Serif,
    /// Segoe UI / system sans-serif — cleaner on low-resolution printers.
    Sans,
}

impl Typeface {
    pub const ALL: [Typeface; 2] = [Typeface::Serif, Typeface::Sans];

    pub fn label(self) -> &'static str {
        match self {
            Typeface::Serif => "Serif (Libertinus)",
            Typeface::Sans => "Sans-serif (Segoe UI)",
        }
    }
}

/// How links in the article are represented on paper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkStyle {
    /// Links stay clickable in the PDF but are invisible on paper.
    Plain,
    /// Every external link also gets a footnote with its URL.
    Footnotes,
}

impl LinkStyle {
    pub const ALL: [LinkStyle; 2] = [LinkStyle::Plain, LinkStyle::Footnotes];

    pub fn label(self) -> &'static str {
        match self {
            LinkStyle::Plain => "Plain text",
            LinkStyle::Footnotes => "Footnote with URL",
        }
    }
}

/// Raster resolution used when sending pages to the printer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrintQuality {
    /// 300 dpi — fast, low memory, perfectly readable.
    Standard,
    /// 600 dpi — crisper text on laser printers, ~4× the memory per page.
    High,
}

impl PrintQuality {
    pub const ALL: [PrintQuality; 2] = [PrintQuality::Standard, PrintQuality::High];

    pub fn dpi(self) -> u32 {
        match self {
            PrintQuality::Standard => 300,
            PrintQuality::High => 600,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PrintQuality::Standard => "Standard (300 dpi)",
            PrintQuality::High => "High (600 dpi)",
        }
    }
}

/// When to load pages in a headless browser (see [`crate::browser`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrowserMode {
    /// Plain download first; use the browser for sign-in sites and when the
    /// plain download finds little or no article text.
    Auto,
    /// Always use the browser (slower, most faithful).
    Always,
    /// Never use the browser.
    Never,
}

impl BrowserMode {
    pub const ALL: [BrowserMode; 3] = [BrowserMode::Auto, BrowserMode::Always, BrowserMode::Never];

    pub fn label(self) -> &'static str {
        match self {
            BrowserMode::Auto => "Automatic (when needed)",
            BrowserMode::Always => "Always",
            BrowserMode::Never => "Never",
        }
    }
}

/// All user-tweakable options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Printer to use. `None` means the system default printer.
    pub printer: Option<String>,
    /// Print immediately after a URL has been typeset.
    pub auto_print: bool,
    /// Number of copies per print job.
    pub copies: u16,
    /// Raster resolution for printing.
    pub print_quality: PrintQuality,

    /// Paper size the PDF is laid out for.
    pub paper: Paper,
    /// Body text size in points.
    pub font_size: f32,
    /// Body typeface.
    pub typeface: Typeface,
    /// Number of text columns (1 or 2).
    pub columns: u8,
    /// Justify paragraphs (with hyphenation) instead of ragged-right.
    pub justify: bool,
    /// Print two pages side by side on each landscape sheet (a spread).
    pub side_by_side: bool,
    /// With side-by-side spreads: tighten spacing just enough to use the
    /// fewest sheets (never below 8.5 pt text).
    pub fit_to_sheets: bool,

    /// Include images from the article.
    pub include_images: bool,
    /// Convert images to grayscale (saves colour ink).
    pub grayscale_images: bool,
    /// How links are shown on paper.
    pub link_style: LinkStyle,
    /// Print a QR code linking back to the original page in the header.
    pub qr_code: bool,

    /// When to use the headless browser.
    pub browser_mode: BrowserMode,
    /// Sites (domains) that need your sign-in; always fetched with the browser.
    pub browser_sites: Vec<String>,

    /// Private RSS feed of your Instapaper "Unread" folder (or any feed).
    pub instapaper_feed: Option<String>,
    /// Print new Instapaper saves automatically while Topebag is running.
    pub instapaper_auto: bool,
    /// Minutes between Instapaper checks.
    pub instapaper_interval_min: u32,

    /// Closing the window keeps Topebag running in the system tray.
    pub close_to_tray: bool,
    /// Start Topebag (in the tray) when you sign in to Windows.
    pub start_with_windows: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            printer: None,
            auto_print: true,
            copies: 1,
            print_quality: PrintQuality::Standard,
            paper: Paper::system_default(),
            font_size: 11.0,
            typeface: Typeface::Serif,
            columns: 1,
            justify: true,
            side_by_side: false,
            fit_to_sheets: true,
            include_images: true,
            grayscale_images: false,
            link_style: LinkStyle::Plain,
            qr_code: true,
            browser_mode: BrowserMode::Auto,
            browser_sites: Vec::new(),
            instapaper_feed: None,
            instapaper_auto: false,
            instapaper_interval_min: 15,
            close_to_tray: true,
            start_with_windows: false,
        }
    }
}

impl Settings {
    /// Load settings from `path`, falling back to defaults if the file does
    /// not exist yet. A corrupt file is reported as an error rather than
    /// silently discarded.
    pub fn load(path: &Path) -> Result<Settings> {
        if !path.exists() {
            return Ok(Settings::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        // Tolerate a UTF-8 byte-order mark (Notepad and PowerShell add one).
        let mut settings: Settings = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .with_context(|| format!("parsing {}", path.display()))?;
        settings.sanitize();
        Ok(settings)
    }

    /// Persist settings to `path` (atomically, via a temp file).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Clamp values into supported ranges.
    pub fn sanitize(&mut self) {
        self.copies = self.copies.clamp(1, 99);
        self.columns = self.columns.clamp(1, 2);
        if !self.font_size.is_finite() {
            self.font_size = 11.0;
        }
        self.font_size = self.font_size.clamp(7.0, 18.0);
        if self.printer.as_deref().is_some_and(|p| p.trim().is_empty()) {
            self.printer = None;
        }
        self.instapaper_interval_min = self.instapaper_interval_min.clamp(5, 24 * 60);
        if self.instapaper_feed.as_deref().is_some_and(|f| f.trim().is_empty()) {
            self.instapaper_feed = None;
        }
        let mut sites: Vec<String> = self
            .browser_sites
            .iter()
            .map(|s| crate::browser::normalize_site(s))
            .filter(|s| !s.is_empty())
            .collect();
        sites.sort();
        sites.dedup();
        self.browser_sites = sites;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults_for_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{ "copies": 3, "columns": 9 }"#).unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.copies, 3);
        assert_eq!(s.columns, 2, "columns are clamped");
        assert!(s.auto_print, "missing fields use defaults");

        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);

        // A file saved with a UTF-8 byte-order mark still loads.
        fs::write(&path, "\u{feff}{ \"side_by_side\": true }").unwrap();
        assert!(Settings::load(&path).unwrap().side_by_side);
    }
}
