//! Win32 GDI printing.

use std::ffi::c_void;
use std::mem::size_of;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDCW, DIB_RGB_COLORS, DeleteDC, GetDeviceCaps, HALFTONE,
    HDC, LOGPIXELSX, LOGPIXELSY, PHYSICALHEIGHT, PHYSICALOFFSETX, PHYSICALOFFSETY, PHYSICALWIDTH, SRCCOPY,
    SetBrushOrgEx, SetStretchBltMode, StretchDIBits,
};
use windows::Win32::Graphics::Printing::{
    EnumPrintersW, GetDefaultPrinterW, PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL, PRINTER_INFO_4W,
};
use windows::Win32::Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};
use windows::core::{PCWSTR, PWSTR};

use super::{PrintOutcome, PrintRequest, PrinterInfo};
use crate::typeset;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn default_printer() -> Option<String> {
    let mut len = 0u32;
    // SAFETY: a null buffer asks only for the required length.
    unsafe {
        let _ = GetDefaultPrinterW(None, &mut len);
    }
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds `len` UTF-16 units, as GetDefaultPrinterW requested.
    let ok = unsafe { GetDefaultPrinterW(Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    if !ok.as_bool() {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

pub fn printers() -> Result<Vec<PrinterInfo>> {
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let mut needed = 0u32;
    let mut returned = 0u32;
    // SAFETY: first call with no buffer only reports the size needed.
    unsafe {
        let _ = EnumPrintersW(flags, PCWSTR::null(), 4, None, &mut needed, &mut returned);
    }
    if needed == 0 {
        return Ok(Vec::new());
    }
    // Back the byte buffer with u64s so the PRINTER_INFO_4W structs written
    // into it are correctly aligned.
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `storage` is at least `needed` bytes long.
    let bytes = unsafe { std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), needed as usize) };
    // SAFETY: buffer and size match; the API fills `returned` structs.
    unsafe { EnumPrintersW(flags, PCWSTR::null(), 4, Some(bytes), &mut needed, &mut returned) }
        .context("could not list printers")?;
    // SAFETY: EnumPrintersW wrote `returned` PRINTER_INFO_4W at the start of
    // the (aligned) buffer; their strings point into the same buffer.
    let infos = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<PRINTER_INFO_4W>(), returned as usize) };

    let default = default_printer();
    let mut list: Vec<PrinterInfo> = infos
        .iter()
        .filter_map(|info| {
            // SAFETY: pPrinterName is a valid NUL-terminated string in `storage`.
            let name = unsafe { info.pPrinterName.to_string() }.ok()?;
            Some(PrinterInfo { is_default: Some(&name) == default.as_ref(), name })
        })
        .collect();
    list.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
    Ok(list)
}

/// Deletes the device context when dropped.
struct DcGuard(HDC);

impl Drop for DcGuard {
    fn drop(&mut self) {
        // SAFETY: the DC was created by CreateDCW and is deleted exactly once.
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

/// Printer geometry in device pixels.
struct Geometry {
    dpi_x: f64,
    dpi_y: f64,
    paper_w: f64,
    paper_h: f64,
    offset_x: f64,
    offset_y: f64,
}

pub fn print(req: &PrintRequest) -> Result<PrintOutcome> {
    let printer = req
        .printer
        .map(str::to_string)
        .or_else(default_printer)
        .ok_or_else(|| anyhow!("no printer selected and Windows has no default printer"))?;

    let printer_w = wide(&printer);
    // SAFETY: printer_w is NUL-terminated and outlives the call.
    let hdc = unsafe { CreateDCW(PCWSTR::null(), PCWSTR(printer_w.as_ptr()), PCWSTR::null(), None) };
    if hdc.is_invalid() {
        bail!("could not open printer \"{printer}\" (is it installed and online?)");
    }
    let _guard = DcGuard(hdc);

    // SAFETY: valid DC.
    let cap = |index| f64::from(unsafe { GetDeviceCaps(Some(hdc), index) });
    let geo = Geometry {
        dpi_x: cap(LOGPIXELSX),
        dpi_y: cap(LOGPIXELSY),
        paper_w: cap(PHYSICALWIDTH),
        paper_h: cap(PHYSICALHEIGHT),
        offset_x: cap(PHYSICALOFFSETX),
        offset_y: cap(PHYSICALOFFSETY),
    };
    if geo.dpi_x <= 0.0 || geo.dpi_y <= 0.0 || geo.paper_w <= 0.0 || geo.paper_h <= 0.0 {
        bail!("printer \"{printer}\" reported an invalid page size");
    }

    let doc_name = wide(req.job_name);
    let output = req.output_file.map(|p| wide(&p.to_string_lossy()));
    let info = DOCINFOW {
        cbSize: size_of::<DOCINFOW>() as i32,
        lpszDocName: PCWSTR(doc_name.as_ptr()),
        lpszOutput: output.as_ref().map_or(PCWSTR::null(), |o| PCWSTR(o.as_ptr())),
        lpszDatatype: PCWSTR::null(),
        fwType: 0,
    };
    // SAFETY: `info` and the strings it points to outlive the call.
    if unsafe { StartDocW(hdc, &info) } <= 0 {
        bail!(
            "printer \"{printer}\" refused the job: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: valid DC. HALFTONE gives smooth scaling when the printer's
    // resolution is higher than the raster's.
    unsafe {
        SetStretchBltMode(hdc, HALFTONE);
        let _ = SetBrushOrgEx(hdc, 0, 0, None);
    }

    let result = (|| -> Result<usize> {
        let mut sheets = 0;
        for _ in 0..req.copies.max(1) {
            for index in 0..req.document.pages().len() {
                print_page(hdc, req, index, &geo)?;
                sheets += 1;
            }
        }
        Ok(sheets)
    })();

    match result {
        Ok(sheets) => {
            // SAFETY: valid DC with an open document.
            if unsafe { EndDoc(hdc) } <= 0 {
                bail!("printer \"{printer}\" failed to finish the job: {}", std::io::Error::last_os_error());
            }
            Ok(PrintOutcome { printer, sheets })
        }
        Err(err) => {
            // SAFETY: valid DC with an open document.
            unsafe {
                AbortDoc(hdc);
            }
            Err(err)
        }
    }
}

fn print_page(hdc: HDC, req: &PrintRequest, index: usize, geo: &Geometry) -> Result<()> {
    let (mut w_pt, mut h_pt) = typeset::page_size_pt(req.document, index).context("missing page")?;
    // Never rasterise above the device resolution.
    let dpi = (req.dpi as f64).min(geo.dpi_x.max(geo.dpi_y)).max(72.0) as f32;
    let mut raster = typeset::render_page(req.document, index, dpi).context("missing page")?;

    // Side-by-side spreads (and landscape PDFs) are wider than tall, while
    // printers default to portrait paper: turn the page a quarter turn so it
    // fills the sheet instead of being shrunk to fit.
    if (w_pt > h_pt) != (geo.paper_w > geo.paper_h) {
        raster = rotate_ccw(&raster);
        std::mem::swap(&mut w_pt, &mut h_pt);
    }

    // Destination rectangle in device pixels: actual size, unless the paper
    // is smaller than the document page, in which case scale to fit.
    let mut dest_w = w_pt / 72.0 * geo.dpi_x;
    let mut dest_h = h_pt / 72.0 * geo.dpi_y;
    let scale = (geo.paper_w / dest_w).min(geo.paper_h / dest_h).min(1.0);
    dest_w *= scale;
    dest_h *= scale;
    // Centre on the physical sheet; device coordinates start at the
    // printable area, so shift by the physical offset.
    let x = ((geo.paper_w - dest_w) / 2.0 - geo.offset_x).round() as i32;
    let y = ((geo.paper_h - dest_h) / 2.0 - geo.offset_y).round() as i32;

    let (dib, bmi) = bgr_dib(&raster);

    // SAFETY: valid DC with an open document.
    if unsafe { StartPage(hdc) } <= 0 {
        bail!("StartPage failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: `dib` matches the dimensions/format described by `bmi`.
    let lines = unsafe {
        StretchDIBits(
            hdc,
            x,
            y,
            dest_w.round() as i32,
            dest_h.round() as i32,
            0,
            0,
            raster.width as i32,
            raster.height as i32,
            Some(dib.as_ptr().cast::<c_void>()),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        )
    };
    if lines == 0 {
        bail!("sending page {} to the printer failed: {}", index + 1, std::io::Error::last_os_error());
    }
    // SAFETY: valid DC with an open page.
    if unsafe { EndPage(hdc) } <= 0 {
        bail!("EndPage failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Rotate an RGBA raster 90° counter-clockwise (the top of the page ends up
/// on the left edge, the usual orientation for landscape printing).
fn rotate_ccw(raster: &typeset::RasterPage) -> typeset::RasterPage {
    let (w, h) = (raster.width as usize, raster.height as usize);
    let mut out = vec![0u8; raster.rgba.len()];
    for y in 0..h {
        for x in 0..w {
            // Source (x, y) lands at (y, w - 1 - x) in the h×w result.
            let src = (y * w + x) * 4;
            let dst = ((w - 1 - x) * h + y) * 4;
            out[dst..dst + 4].copy_from_slice(&raster.rgba[src..src + 4]);
        }
    }
    typeset::RasterPage { width: raster.height, height: raster.width, rgba: out }
}

/// Convert RGBA rows into a bottom-up, 24-bit BGR DIB — the most widely
/// supported bitmap format across printer drivers.
fn bgr_dib(raster: &typeset::RasterPage) -> (Vec<u8>, BITMAPINFO) {
    let w = raster.width as usize;
    let h = raster.height as usize;
    let stride = (w * 3).div_ceil(4) * 4;
    let mut dib = vec![0u8; stride * h];
    for (y, src_row) in raster.rgba.chunks_exact(w * 4).enumerate() {
        let dst_row = &mut dib[(h - 1 - y) * stride..][..w * 3];
        for (dst, src) in dst_row.chunks_exact_mut(3).zip(src_row.chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
        }
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: raster.width as i32,
            biHeight: raster.height as i32, // positive = bottom-up
            biPlanes: 1,
            biBitCount: 24,
            biCompression: BI_RGB.0,
            biSizeImage: dib.len() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    (dib, bmi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_turns_the_top_edge_to_the_left() {
        // 2×1 image: red | green  →  1×2 image: green on top, red below.
        let raster = typeset::RasterPage { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 0, 255, 0, 255] };
        let r = rotate_ccw(&raster);
        assert_eq!((r.width, r.height), (1, 2));
        assert_eq!(&r.rgba[0..4], &[0, 255, 0, 255], "top pixel is the old right edge");
        assert_eq!(&r.rgba[4..8], &[255, 0, 0, 255], "bottom pixel is the old left edge");
    }

    #[test]
    fn dib_rows_are_flipped_padded_and_bgr() {
        // 2×2 image: top row red, green; bottom row blue, white.
        let raster = typeset::RasterPage {
            width: 2,
            height: 2,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255],
        };
        let (dib, bmi) = bgr_dib(&raster);
        assert_eq!(bmi.bmiHeader.biHeight, 2);
        // stride = 6 bytes rounded up to 8
        assert_eq!(dib.len(), 16);
        // First DIB row is the *bottom* image row: blue, white (as BGR).
        assert_eq!(&dib[0..6], &[255, 0, 0, 255, 255, 255]);
        // Second DIB row is the top image row: red, green (as BGR).
        assert_eq!(&dib[8..14], &[0, 0, 255, 0, 255, 0]);
    }
}
