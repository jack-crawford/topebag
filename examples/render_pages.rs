//! Developer tool: render a saved entry's pages to PNG files.
//!
//! Handy when tweaking `assets/topebag.typ` — re-typeset an entry, then look
//! at the pages without opening a PDF viewer.
//!
//! ```text
//! cargo run --example render_pages -- <entry-folder> <output-folder> [dpi]
//! ```

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: render_pages <entry-folder> <output-folder> [dpi]";
    let entry = PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!(usage))?);
    let out = PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!(usage))?);
    let dpi: f32 = args.next().map(|d| d.parse()).transpose()?.unwrap_or(100.0);

    std::fs::create_dir_all(&out)?;
    let compiled = topebag::typeset::compile_dir(&entry)?;
    for w in &compiled.warnings {
        eprintln!("warning: {w}");
    }
    for i in 0..compiled.page_count() {
        let page = topebag::typeset::render_page(&compiled.document, i, dpi).expect("page exists");
        let path = out.join(format!("page-{:02}.png", i + 1));
        image::save_buffer(&path, &page.rgba, page.width, page.height, image::ColorType::Rgba8)?;
        println!("{}", path.display());
    }
    Ok(())
}
