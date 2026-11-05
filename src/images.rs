//! Article images.
//!
//! The HTML converter asks an [`ImageResolver`] to make every `<img>` it
//! meets available on disk. Three resolvers exist:
//!
//! * [`Downloader`] fetches images from the web, normalises them for print
//!   (downscale, optional grayscale, re-encode as JPEG/PNG) and writes them
//!   into the entry's `img/` folder. Used when a URL is first added.
//! * [`StoredImages`] replays images that were downloaded earlier.
//! * [`RefillImages`] replays stored images and downloads any that are
//!   missing (for example ones an older version failed to fetch).
//! * [`GrayImages`] wraps another resolver and hands out grayscale copies,
//!   so "grayscale" can be switched per document without re-downloading.
//! * [`NoImages`] drops every image (the "Include images" option is off).
//!
//! Downloads always keep full-colour originals in `img/`.

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use image::{DynamicImage, GenericImageView, ImageFormat, imageops::FilterType};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::fetch;

/// Images narrower than this are treated as icons / tracking pixels.
const MIN_SIDE_PX: u32 = 40;
/// Images are downscaled to at most this width (≈ 250 dpi across a page).
const MAX_WIDTH_PX: u32 = 1800;
/// Hard cap on images per document, to keep PDFs and print jobs sane.
const MAX_IMAGES: usize = 60;

/// An image that has been saved next to a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredImage {
    /// Where the image originally came from.
    pub source_url: String,
    /// File name inside the entry's `img/` directory, e.g. `003.jpg`.
    pub file: String,
    /// Pixel dimensions (for SVGs: the declared size or a sensible default).
    pub width: u32,
    pub height: u32,
}

impl StoredImage {
    /// Path of the image inside the Typst project (absolute, rooted at the
    /// entry directory).
    pub fn typst_path(&self) -> String {
        format!("/img/{}", self.file)
    }
}

/// Supplies local copies of images referenced by an article.
pub trait ImageResolver {
    /// Return a local copy of the image at `url`, or `None` to omit it.
    fn resolve(&mut self, url: &Url) -> Option<StoredImage>;
}

/// Resolver that omits all images.
pub struct NoImages;

impl ImageResolver for NoImages {
    fn resolve(&mut self, _url: &Url) -> Option<StoredImage> {
        None
    }
}

/// Resolver that replays images saved by an earlier [`Downloader`] run.
pub struct StoredImages {
    by_url: HashMap<String, StoredImage>,
}

impl StoredImages {
    pub fn new(images: &[StoredImage]) -> Self {
        Self {
            by_url: images.iter().map(|i| (i.source_url.clone(), i.clone())).collect(),
        }
    }
}

impl ImageResolver for StoredImages {
    fn resolve(&mut self, url: &Url) -> Option<StoredImage> {
        self.by_url.get(url.as_str()).cloned()
    }
}

/// Resolver that downloads and normalises images into `dir`.
pub struct Downloader<'a> {
    dir: PathBuf,
    referer: Option<Url>,
    cache: HashMap<String, Option<StoredImage>>,
    /// Number used for the next file name (`001.jpg`, `002.png`, …).
    next_index: usize,
    /// Every image successfully stored, in document order.
    pub stored: Vec<StoredImage>,
    /// Images that could not be included (download or decoding failed).
    /// Icons and tracking pixels that are dropped on purpose are not listed.
    pub skipped: Vec<String>,
    /// URLs dropped on purpose (icons, tracking pixels): never worth retrying.
    pub rejected: Vec<String>,
    /// Called after each download attempt with (attempted, stored).
    progress: Option<Box<dyn FnMut(usize, usize) + 'a>>,
}

impl<'a> Downloader<'a> {
    /// Create a downloader writing into `dir` (created if needed).
    pub fn new(dir: impl Into<PathBuf>, referer: Option<Url>) -> Self {
        Self {
            dir: dir.into(),
            referer,
            cache: HashMap::new(),
            next_index: 1,
            stored: Vec::new(),
            skipped: Vec::new(),
            rejected: Vec::new(),
            progress: None,
        }
    }

    /// Continue numbering after images that already exist in the folder.
    pub fn after_existing(mut self, existing: &[StoredImage]) -> Self {
        let highest = existing
            .iter()
            .filter_map(|i| i.file.split('.').next().and_then(|stem| stem.parse::<usize>().ok()))
            .max()
            .unwrap_or(0);
        self.next_index = highest + 1;
        self
    }

    /// Register a callback invoked after each image download attempt.
    pub fn on_progress(mut self, f: impl FnMut(usize, usize) + 'a) -> Self {
        self.progress = Some(Box::new(f));
        self
    }

    fn fetch_and_store(&mut self, url: &Url) -> Result<StoredImage> {
        let resource = fetch::fetch_resource(url, self.referer.as_ref())?;
        let stem = format!("{:03}", self.next_index);
        fs::create_dir_all(&self.dir)?;
        let normalized = normalize(&resource.bytes, resource.content_type.as_deref(), false)?;
        self.next_index += 1;
        let file = format!("{stem}.{}", normalized.extension);
        fs::write(self.dir.join(&file), &normalized.bytes)
            .with_context(|| format!("saving image {file}"))?;
        Ok(StoredImage {
            source_url: url.to_string(),
            file,
            width: normalized.width,
            height: normalized.height,
        })
    }
}

impl ImageResolver for Downloader<'_> {
    fn resolve(&mut self, url: &Url) -> Option<StoredImage> {
        if let Some(hit) = self.cache.get(url.as_str()) {
            return hit.clone();
        }
        let result = if self.stored.len() >= MAX_IMAGES {
            Err(anyhow::anyhow!("image limit of {MAX_IMAGES} reached"))
        } else {
            self.fetch_and_store(url)
        };
        let outcome = match result {
            Ok(image) => {
                self.stored.push(image.clone());
                Some(image)
            }
            Err(err) => {
                log::info!("skipping image {url}: {err:#}");
                if format!("{err}").contains("icon-sized") {
                    self.rejected.push(url.to_string());
                } else {
                    self.skipped.push(format!("{url}: {err:#}"));
                }
                None
            }
        };
        self.cache.insert(url.to_string(), outcome.clone());
        if let Some(progress) = self.progress.as_mut() {
            progress(self.cache.len(), self.stored.len());
        }
        outcome
    }
}

/// Resolver that uses stored images and downloads the ones that are
/// missing (skipping URLs known to be icons).
pub struct RefillImages<'a> {
    by_url: HashMap<String, StoredImage>,
    existing: Vec<StoredImage>,
    rejected: Vec<String>,
    downloader: Downloader<'a>,
}

impl<'a> RefillImages<'a> {
    pub fn new(existing: &[StoredImage], rejected: &[String], dir: impl Into<PathBuf>, referer: Option<Url>) -> Self {
        Self {
            by_url: existing.iter().map(|i| (i.source_url.clone(), i.clone())).collect(),
            existing: existing.to_vec(),
            rejected: rejected.to_vec(),
            downloader: Downloader::new(dir, referer).after_existing(existing),
        }
    }

    /// A stored image, without downloading anything.
    pub fn stored(&self, url: &Url) -> Option<StoredImage> {
        self.by_url.get(url.as_str()).cloned()
    }

    /// All images now available (old and newly downloaded), plus the updated
    /// list of rejected URLs and how many images still failed.
    pub fn finish(self) -> (Vec<StoredImage>, Vec<String>, usize) {
        let mut images = self.existing;
        images.extend(self.downloader.stored);
        let mut rejected = self.rejected;
        rejected.extend(self.downloader.rejected);
        (images, rejected, self.downloader.skipped.len())
    }
}

impl ImageResolver for RefillImages<'_> {
    fn resolve(&mut self, url: &Url) -> Option<StoredImage> {
        if let Some(hit) = self.by_url.get(url.as_str()) {
            return Some(hit.clone());
        }
        if self.rejected.iter().any(|r| r == url.as_str()) {
            return None;
        }
        self.downloader.resolve(url)
    }
}

/// Wraps a resolver and hands out grayscale copies (`gray-001.jpg`), made on
/// first use and kept next to the originals.
pub struct GrayImages<'r> {
    inner: &'r mut dyn ImageResolver,
    dir: PathBuf,
}

impl<'r> GrayImages<'r> {
    pub fn new(inner: &'r mut dyn ImageResolver, dir: impl Into<PathBuf>) -> Self {
        Self { inner, dir: dir.into() }
    }
}

impl ImageResolver for GrayImages<'_> {
    fn resolve(&mut self, url: &Url) -> Option<StoredImage> {
        let original = self.inner.resolve(url)?;
        if original.file.ends_with(".svg") {
            return Some(original); // vectors stay as they are
        }
        let gray_file = format!("gray-{}", original.file);
        let gray_path = self.dir.join(&gray_file);
        if !gray_path.exists() {
            let made = (|| -> Result<()> {
                let img = image::open(self.dir.join(&original.file))?;
                let gray = DynamicImage::ImageLumaA8(img.to_luma_alpha8());
                if original.file.ends_with(".png") {
                    gray.save(&gray_path)?;
                } else {
                    let mut out = std::fs::File::create(&gray_path)?;
                    gray.to_luma8().write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85))?;
                }
                Ok(())
            })();
            if let Err(err) = made {
                log::warn!("could not make a grayscale copy of {}: {err:#}", original.file);
                return Some(original);
            }
        }
        Some(StoredImage { file: gray_file, ..original })
    }
}

/// An image re-encoded for print.
struct Normalized {
    bytes: Vec<u8>,
    extension: &'static str,
    width: u32,
    height: u32,
}

/// Decode, filter, downscale and re-encode an image.
///
/// * SVGs are kept as-is (Typst renders them as vectors).
/// * Tiny images (icons, spacer GIFs, tracking pixels) are rejected.
/// * Opaque images become JPEG (q85); images with transparency stay PNG.
fn normalize(bytes: &[u8], content_type: Option<&str>, grayscale: bool) -> Result<Normalized> {
    if is_svg(bytes, content_type) {
        let (width, height) = svg_size(bytes).unwrap_or((800, 500));
        if width < MIN_SIDE_PX && height < MIN_SIDE_PX {
            bail!("SVG is icon-sized ({width}×{height})");
        }
        return Ok(Normalized { bytes: bytes.to_vec(), extension: "svg", width, height });
    }

    let format = image::guess_format(bytes).context("unrecognised image format")?;
    let mut img = image::load_from_memory_with_format(bytes, format).context("could not decode image")?;
    let (w, h) = img.dimensions();
    if w < MIN_SIDE_PX || h < MIN_SIDE_PX {
        bail!("image is icon-sized ({w}×{h})");
    }
    if w > MAX_WIDTH_PX {
        let new_h = ((h as f64) * (MAX_WIDTH_PX as f64) / (w as f64)).round().max(1.0) as u32;
        img = img.resize_exact(MAX_WIDTH_PX, new_h, FilterType::CatmullRom);
    }
    if grayscale {
        img = DynamicImage::ImageLumaA8(img.to_luma_alpha8());
    }

    let (width, height) = img.dimensions();
    let mut out = Cursor::new(Vec::new());
    let extension = if has_transparency(&img) {
        img.write_to(&mut out, ImageFormat::Png)?;
        "png"
    } else {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        if grayscale {
            img.to_luma8().write_with_encoder(encoder)?;
        } else {
            img.to_rgb8().write_with_encoder(encoder)?;
        }
        "jpg"
    };
    Ok(Normalized { bytes: out.into_inner(), extension, width, height })
}

fn has_transparency(img: &DynamicImage) -> bool {
    if !img.color().has_alpha() {
        return false;
    }
    img.to_rgba8().pixels().any(|p| p.0[3] < 250)
}

fn is_svg(bytes: &[u8], content_type: Option<&str>) -> bool {
    if content_type.is_some_and(|ct| ct.contains("svg")) {
        return true;
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_ascii_lowercase();
    head.trim_start().starts_with("<svg") || (head.contains("<?xml") && head.contains("<svg"))
}

/// Best-effort size of an SVG from its root `width`/`height` or `viewBox`.
fn svg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let start = text.find("<svg")?;
    let tag = &text[start..start + text[start..].find('>')?];
    let attr = |name: &str| -> Option<&str> {
        let idx = tag.find(&format!(" {name}="))? + name.len() + 2;
        let rest = &tag[idx..];
        let quote = rest.chars().next()?;
        let rest = &rest[1..];
        Some(&rest[..rest.find(quote)?])
    };
    let num = |v: &str| v.trim_end_matches("px").trim().parse::<f64>().ok();
    if let (Some(w), Some(h)) = (attr("width").and_then(num), attr("height").and_then(num)) {
        return Some((w.round() as u32, h.round() as u32));
    }
    let vb: Vec<f64> = attr("viewBox")?
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter_map(|p| p.parse().ok())
        .collect();
    (vb.len() == 4).then(|| (vb[2].round() as u32, vb[3].round() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, alpha: u8) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 30, alpha]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn opaque_png_becomes_jpeg_and_is_downscaled() {
        let n = normalize(&png(2400, 1200, 255), Some("image/png"), false).unwrap();
        assert_eq!(n.extension, "jpg");
        assert_eq!((n.width, n.height), (1800, 900));
    }

    #[test]
    fn transparent_png_stays_png() {
        let n = normalize(&png(100, 100, 0), None, true).unwrap();
        assert_eq!(n.extension, "png");
    }

    struct Fixed(StoredImage);
    impl ImageResolver for Fixed {
        fn resolve(&mut self, _url: &Url) -> Option<StoredImage> {
            Some(self.0.clone())
        }
    }

    #[test]
    fn grayscale_copies_are_made_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("001.png"), png(80, 60, 255)).unwrap();
        let stored = StoredImage { source_url: "https://x/a.png".into(), file: "001.png".into(), width: 80, height: 60 };
        let mut inner = Fixed(stored);
        let mut gray = GrayImages::new(&mut inner, dir.path());
        let url = Url::parse("https://x/a.png").unwrap();
        let got = gray.resolve(&url).unwrap();
        assert_eq!(got.file, "gray-001.png");
        let pixel = image::open(dir.path().join("gray-001.png")).unwrap().to_rgba8().get_pixel(1, 1).0;
        assert_eq!(pixel[0], pixel[1], "grayscale: channels equal");
        assert_eq!(gray.resolve(&url).unwrap().file, "gray-001.png");
    }

    #[test]
    fn numbering_continues_after_existing_images() {
        let existing = vec![
            StoredImage { source_url: "a".into(), file: "001.jpg".into(), width: 1, height: 1 },
            StoredImage { source_url: "b".into(), file: "007.png".into(), width: 1, height: 1 },
        ];
        let d = Downloader::new("x", None).after_existing(&existing);
        assert_eq!(d.next_index, 8);
    }

    #[test]
    fn icons_are_rejected() {
        assert!(normalize(&png(16, 16, 255), None, false).is_err());
    }

    #[test]
    fn reads_svg_size() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 640 480"><rect/></svg>"#;
        assert_eq!(svg_size(svg), Some((640, 480)));
        let svg = br#"<svg width="300px" height="150" xmlns="http://www.w3.org/2000/svg"></svg>"#;
        assert_eq!(svg_size(svg), Some((300, 150)));
    }
}
