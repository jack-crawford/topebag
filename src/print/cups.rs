//! CUPS printing for macOS and Linux (via `lp` / `lpstat`).

use std::process::Command;

use anyhow::{Context, Result, bail};

use super::{PrintOutcome, PrintRequest, PrinterInfo};

pub fn default_printer() -> Option<String> {
    let out = Command::new("lpstat").arg("-d").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "system default destination: Brother_HL"
    text.split(':').nth(1).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn printers() -> Result<Vec<PrinterInfo>> {
    let out = Command::new("lpstat").arg("-e").output().context("running lpstat (is CUPS installed?)")?;
    let default = default_printer();
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|name| PrinterInfo { name: name.to_string(), is_default: Some(name) == default.as_deref() })
        .collect())
}

pub fn print(req: &PrintRequest) -> Result<PrintOutcome> {
    if let Some(out) = req.output_file {
        std::fs::copy(req.pdf_path, out).with_context(|| format!("copying PDF to {}", out.display()))?;
        return Ok(PrintOutcome {
            printer: format!("file: {}", out.display()),
            sheets: req.document.pages().len(),
        });
    }
    let mut cmd = Command::new("lp");
    cmd.arg("-n").arg(req.copies.max(1).to_string()).arg("-t").arg(req.job_name);
    if let Some(printer) = req.printer {
        cmd.arg("-d").arg(printer);
    }
    cmd.arg(req.pdf_path);
    let out = cmd.output().context("running lp (is CUPS installed?)")?;
    if !out.status.success() {
        bail!("lp failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(PrintOutcome {
        printer: req.printer.map(str::to_string).or_else(default_printer).unwrap_or_else(|| "default".into()),
        sheets: req.document.pages().len() * usize::from(req.copies.max(1)),
    })
}
