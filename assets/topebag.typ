// Topebag document template
// =========================
//
// Every saved article is typeset from a generated `main.typ` that looks like:
//
//     #import "/topebag.typ": *
//     #show: tb-article.with(title: "…", site: "…", …)
//     …article body produced by src/convert.rs…
//
// Everything about the *look* of printed documents lives in this file, so it
// is the place to tweak typography. It is compiled into the binary with
// `include_str!`, so changes require a rebuild.

// Only fonts that src/typeset.rs actually loads are listed here (bundled
// Typst fonts + a curated set of Windows fonts). Missing ones are skipped.
// The first family that's installed wins: Segoe UI on Windows, Helvetica
// Neue on macOS. Libertinus Serif and DejaVu Sans Mono are compiled in, so
// the body text and code look the same everywhere.
#let tb-sans = ("Segoe UI", "Helvetica Neue", "Arial", "Libertinus Serif")
#let tb-serif = ("Libertinus Serif", "Georgia")
#let tb-mono = ("DejaVu Sans Mono", "Consolas", "Menlo")
// Fallbacks for symbols, emoji and non-Latin scripts (Windows, then macOS
// system fonts).
#let tb-fallback = (
  "Segoe UI Symbol", "Segoe UI Emoji", "Microsoft YaHei", "Yu Gothic", "Malgun Gothic", "Nirmala UI", "Ebrima",
  "Apple Symbols", "Hiragino Sans GB", "Hiragino Sans", "Apple SD Gothic Neo", "Kohinoor Devanagari", "Arial Unicode MS",
)

#let tb-muted = luma(90)
#let tb-hairline = luma(165)

// ---------------------------------------------------------------------------
// Building blocks used by the generated article body
// ---------------------------------------------------------------------------

/// Maximum figure height. `tb-article` sets it from the layout (smaller
/// for side-by-side pages and when spacing is tightened).
#let tb-figure-max = state("tb-figure-max", 4in)

/// An image that fits the column, never upscales past its natural size
/// (assuming 96 dpi), and never exceeds `tb-figure-max` in height.
///
/// With `float: true` the figure may move to the top or bottom of the page
/// (as in books) so a tall image never leaves a half-empty page behind.
#let tb-figure(path, w, h, caption: none, float: false) = figure(
  context {
    let max-height = tb-figure-max.get()
    layout(size => {
      let width = calc.min(w * 0.75pt, size.width)
      if width * h / w > max-height {
        width = max-height * w / h
      }
      image(path, width: width)
    })
  },
  caption: caption,
  placement: if float { auto } else { none },
)

/// A small caption paragraph (for figures whose image was dropped, tables…).
#let tb-caption(body) = align(center, text(size: 0.85em, fill: tb-muted, body))

/// Placeholder for videos, audio and iframes that cannot be printed.
#let tb-embed(kind, url, display) = block(
  width: 100%,
  inset: (x: 9pt, y: 7pt),
  radius: 3pt,
  stroke: (paint: tb-hairline, thickness: 0.6pt, dash: "dashed"),
  text(size: 0.82em, fill: tb-muted)[
    #strong(kind) not included in print · #link(url, text(font: tb-mono, size: 0.92em, display))
  ],
)

/// A short centred rule (from `<hr>`).
#let tb-rule() = align(center, block(above: 1.3em, below: 1.3em, line(length: 22%, stroke: 0.6pt + tb-hairline)))

/// A table that keeps to the column width; `small` shrinks wide tables.
#let tb-table(columns: 1, small: false, ..cells) = {
  set text(size: if small { 0.75em } else { 0.88em })
  set par(justify: false)
  block(width: 100%, table(columns: columns, ..cells))
}

// ---------------------------------------------------------------------------
// Page setup and masthead
// ---------------------------------------------------------------------------

/// Type styles shared by articles and editions: text, paragraphs, headings,
/// code, quotes, figures, tables, lists and footnotes, all scaled by
/// `density` (see below).
#let tb-base(
  body-font: tb-serif,
  font-size: 11pt,
  lang: "en",
  region: none,
  justify: true,
  density: 0.0,
  body,
) = {
  let d = calc.clamp(density, 0.0, 1.0)
  let lerp(a, b) = a + (b - a) * d
  let sans = tb-sans + tb-fallback
  let head-scale = lerp(1.0, 0.55)

  set text(font: body-font + tb-fallback, size: lerp(font-size, calc.min(font-size, 8.5pt)), lang: lang, region: region)
  // As spacing between paragraphs shrinks, switch to a book-style first-line
  // indent so paragraphs stay easy to tell apart.
  set par(
    justify: justify,
    leading: lerp(0.64em, 0.5em),
    spacing: lerp(1.05em, 0.55em),
    first-line-indent: if d > 0.3 { 1.1em } else { 0em },
  )
  set block(spacing: lerp(1.2em, 0.65em))

  // Headings
  set heading(numbering: none)
  show heading: set text(font: sans, weight: "bold", hyphenate: false)
  show heading: set par(justify: false)
  show heading: set block(above: 1.45em * head-scale, below: 0.7em * head-scale)
  show heading.where(level: 1): set text(size: 1.4em)
  show heading.where(level: 2): set text(size: 1.22em)
  show heading.where(level: 3): set text(size: 1.08em)
  show heading.where(level: 4): set text(size: 1em, style: "italic")

  // Code
  show raw: set text(font: tb-mono, size: 0.86em)
  show raw.where(block: true): it => block(
    width: 100%,
    fill: luma(245),
    stroke: 0.5pt + luma(212),
    inset: lerp(8pt, 4.5pt),
    radius: 2pt,
    {
      set par(justify: false)
      it
    },
  )

  // Quotes, figures, tables, lists, footnotes
  show quote.where(block: true): it => block(
    stroke: (left: 2pt + tb-hairline),
    inset: (left: lerp(12pt, 8pt), y: 2pt),
    it.body,
  )
  set figure(numbering: none, gap: lerp(0.65em, 0.4em))
  show figure.caption: set text(size: 0.85em, fill: tb-muted)
  set table(stroke: 0.5pt + tb-hairline, inset: (x: lerp(5pt, 3.5pt), y: lerp(4pt, 2.5pt)))
  show table.cell.where(y: 0): set text(weight: "bold")
  set list(indent: 0.4em, body-indent: 0.6em)
  set enum(indent: 0.4em, body-indent: 0.6em)
  set highlight(fill: luma(222))
  show footnote.entry: set text(size: 0.8em)
  show footnote.entry: set par(justify: false)

  body
}

// Layout knobs
// -------------
//
// `spread: true` prints two pages side by side on each landscape sheet, like
// an open book: the sheet is split into two columns that each act as a page,
// with their own page numbers and a fold mark in the middle.
//
// `density` (0–1) tightens everything at once: text size (never below
// 8.5 pt), leading, paragraph and heading spacing, margins, figure height
// and the masthead. Topebag's optimizer (src/typeset.rs) picks the smallest
// density that still yields the fewest sheets, so short articles fit on one
// sheet without looking cramped.

#let tb-article(
  title: "Untitled",
  short-title: none,
  site: none,
  meta: (),
  url: none,
  url-display: none,
  domain: none,
  saved: none,
  lang: "en",
  region: none,
  paper: "us-letter",
  font-size: 11pt,
  typeface: "serif",
  cols: 1,
  justify: true,
  qr: none,
  spread: false,
  density: 0.0,
  body,
) = {
  let body-font = if typeface == "sans" { tb-sans } else { tb-serif }
  let sans = tb-sans + tb-fallback
  let d = calc.clamp(density, 0.0, 1.0)
  // Interpolate between the comfortable (d = 0) and compact (d = 1) value.
  let lerp(a, b) = a + (b - a) * d

  let margins = if spread {
    (outer: lerp(0.55in, 0.32in), inner: lerp(0.5in, 0.28in), top: lerp(0.6in, 0.38in), bottom: lerp(0.68in, 0.46in))
  } else {
    (outer: lerp(0.85in, 0.5in), inner: lerp(0.85in, 0.5in), top: lerp(0.85in, 0.5in), bottom: lerp(0.9in, 0.55in))
  }
  // Space between the two pages of a spread (both inner margins).
  let gutter = 2 * margins.inner
  let fig-max = if spread { lerp(3.1in, 2in) } else { lerp(4in, 2.4in) }
  let chrome = (font: sans, size: lerp(7.5pt, 6.5pt), fill: tb-muted)

  set document(title: title)
  show: tb-base.with(body-font: body-font, font-size: font-size, lang: lang, region: region, justify: justify, density: density)

  set page(
    paper: paper,
    flipped: spread,
    fill: white,
    margin: (left: margins.outer, right: margins.outer, top: margins.top, bottom: margins.bottom),
    header: context if here().page() > 1 {
      set text(..chrome)
      let left = if short-title != none { short-title } else { title }
      if spread {
        // Title over the left page, site over the right one.
        grid(columns: (1fr, 1fr), column-gutter: gutter, left, align(right, if domain != none { domain }))
      } else {
        grid(columns: (1fr, auto), column-gutter: 14pt, left, if domain != none { domain })
      }
    },
    footer: context {
      set text(..chrome)
      if spread {
        // Each half of the sheet is numbered like a book page.
        let n = here().page()
        grid(
          columns: (1fr, 1fr),
          column-gutter: gutter,
          [#(2 * n - 1)#h(0.8em)#if domain != none { domain }],
          align(right)[printed with Topebag#h(0.8em)#(2 * n)],
        )
      } else {
        grid(
          columns: (1fr, auto),
          if domain != none [#domain · printed with Topebag] else [Printed with Topebag],
          counter(page).display("1 / 1", both: true),
        )
      }
    },
    // Small fold/cut marks in the middle of each spread.
    background: if spread {
      let mark = line(angle: 90deg, length: 0.18in, stroke: 0.4pt + luma(185))
      place(top + center, mark)
      place(bottom + center, dy: -0.18in, mark)
    },
  )

  // Masthead
  let info = {
    if site != none {
      block(below: 0.55em, text(font: sans, size: lerp(7.8pt, 6.8pt), weight: "bold", tracking: 0.09em, fill: tb-muted, upper(site)))
    }
    block(below: lerp(0.65em, 0.4em), {
      set par(justify: false, leading: 0.34em)
      text(size: lerp(2.05em, 1.55em), weight: "bold", hyphenate: false, title)
    })
    if meta.len() > 0 {
      block(below: 0.45em, text(size: 0.95em, style: "italic", meta.join("  ·  ")))
    }
    if url != none {
      block(below: 0.3em, text(font: tb-mono, size: 0.7em, fill: tb-muted, link(url, url-display)))
    }
    if saved != none {
      text(font: sans, size: 0.7em, fill: tb-muted)[Saved #saved]
    }
  }
  let masthead = block(width: 100%, {
    if qr != none {
      grid(
        columns: (1fr, auto),
        column-gutter: lerp(18pt, 10pt),
        align: (left + bottom, right + bottom),
        info,
        stack(
          spacing: 3pt,
          image(qr, width: lerp(0.75in, 0.55in)),
          align(center, text(font: sans, size: 5.5pt, fill: tb-muted)[ORIGINAL]),
        ),
      )
    } else {
      info
    }
    v(lerp(0.7em, 0.35em))
    line(length: 100%, stroke: 0.9pt)
  })

  // The masthead is placed as the first top float, so figures that float
  // (see tb-figure) always queue up *below* it instead of jumping above the
  // title on the first page.
  // Floats ignore a block's spacing, so the gap below the masthead rule is
  // the float's clearance.
  let masthead = place(top, float: true, clearance: lerp(1.4em, 0.7em), masthead)

  tb-figure-max.update(fig-max)
  if spread {
    // The masthead sits at the top of the left page; text then flows down
    // the left page and continues on the right one.
    columns(2, gutter: gutter, masthead + body)
  } else if cols > 1 {
    masthead
    columns(cols, gutter: 0.3in, body)
  } else {
    masthead
    body
  }
}

// ---------------------------------------------------------------------------
// Editions: several articles printed together like a newspaper
// ---------------------------------------------------------------------------
//
// Stories flow one after another through the columns, with no page breaks
// between them, so the only blank space is at the very end. Page 1 opens
// with a nameplate and an "In this edition" list with page numbers.

/// One story in an edition: kicker, headline, byline, then the article.
/// The body is inserted straight into the column flow (no container), so a
/// story can continue across columns and pages.
#let tb-story(
  title: "",
  site: none,
  byline: none,
  date: none,
  minutes: none,
  qr: none,
  first: false,
  lang: "en",
  region: none,
  body,
) = {
  let sans = tb-sans + tb-fallback
  set text(lang: lang, region: region)
  [#metadata((tb-story: title))]
  let details = (byline, date, if minutes != none and minutes > 0 [#minutes min]).filter(x => x != none)
  let head = {
    if site != none {
      block(below: 0.35em, text(font: sans, size: 0.72em, weight: "bold", tracking: 0.08em, fill: tb-muted, upper(site)))
    }
    block(below: 0.4em, {
      set par(justify: false, leading: 0.3em, first-line-indent: 0em)
      text(size: 1.62em, weight: "bold", hyphenate: false, title)
    })
    if details.len() > 0 {
      text(size: 0.85em, style: "italic", fill: tb-muted, details.join("  ·  "))
    }
  }
  // The rule above a story and its headline are one sticky, unbreakable
  // block: it moves to the next column together with the story's first
  // lines instead of being stranded at the foot of a column.
  block(sticky: true, breakable: false, above: if first { 0em } else { 1.3em }, below: 0.8em, {
    if not first {
      line(length: 100%, stroke: 1.1pt)
      v(0.9em)
    }
    if qr != none {
      grid(columns: (1fr, auto), column-gutter: 6pt, align: (left + bottom, right + bottom), head, image(qr, width: 0.42in))
    } else {
      head
    }
    v(0.25em)
    line(length: 100%, stroke: 0.4pt + tb-hairline)
  })
  {
    // Headings inside a story stay below the story's headline.
    show heading: set text(size: 1.05em)
    body
  }
}

/// A newspaper-style edition of several stories.
#let tb-edition(
  title: "The Topebag",
  dateline: "",
  stories: 0,
  minutes: 0,
  contents: true,
  lang: "en",
  paper: "us-letter",
  font-size: 9.5pt,
  typeface: "serif",
  cols: 3,
  justify: true,
  density: 0.0,
  body,
) = {
  let body-font = if typeface == "sans" { tb-sans } else { tb-serif }
  let sans = tb-sans + tb-fallback
  let d = calc.clamp(density, 0.0, 1.0)
  let lerp(a, b) = a + (b - a) * d
  let margin = (x: lerp(0.6in, 0.4in), top: lerp(0.62in, 0.42in), bottom: lerp(0.65in, 0.45in))
  let gutter = lerp(0.24in, 0.17in)
  let chrome = (font: sans, size: lerp(7.2pt, 6.5pt), fill: tb-muted)

  set document(title: title + " — " + dateline)
  show: tb-base.with(body-font: body-font, font-size: font-size, lang: lang, justify: justify, density: density)

  set page(
    paper: paper,
    fill: white,
    margin: margin,
    header: context if here().page() > 1 {
      set text(..chrome)
      grid(columns: (1fr, auto), [#smallcaps(title) · #dateline], [page #here().page()])
    },
    footer: context {
      set text(..chrome)
      grid(columns: (1fr, auto), [printed with Topebag], counter(page).display("1 / 1", both: true))
    },
    // Hairlines between the columns, like a newspaper. On page 1 the
    // nameplate's white background covers them.
    background: if cols > 1 {
      context {
        let width = page.width - 2 * margin.x
        let column = (width - (cols - 1) * gutter) / cols
        let height = page.height - margin.top - margin.bottom
        for i in range(1, cols) {
          let x = margin.x + i * column + (i - 1) * gutter + gutter / 2
          place(top + left, dx: x, dy: margin.top, line(angle: 90deg, length: height, stroke: 0.5pt + luma(165)))
        }
      }
    },
  )

  let contents-line = context {
    let marks = query(metadata).filter(m => type(m.value) == dictionary and "tb-story" in m.value)
    set text(font: sans, size: 7.5pt)
    set par(justify: false, first-line-indent: 0em)
    [*In this edition* #h(0.6em)]
    marks
      .map(m => [#m.value.at("tb-story")#h(0.3em)#text(fill: tb-muted)[p.#h(0.1em)#m.location().page()]])
      .join([#h(0.5em)·#h(0.5em)])
  }
  let nameplate = block(width: 100%, fill: white, inset: (bottom: 4pt), stack(
    spacing: lerp(6pt, 4pt),
    line(length: 100%, stroke: 2.2pt),
    align(center, text(size: lerp(3.4em, 2.6em), weight: "bold", tracking: -0.01em, title)),
    line(length: 100%, stroke: 0.5pt),
    grid(
      columns: (1fr, 1fr),
      align: (left, right),
      text(font: sans, size: 7.5pt, tracking: 0.06em, upper(dateline)),
      text(font: sans, size: 7.5pt, tracking: 0.06em, upper[#stories #if stories == 1 [story] else [stories] · #minutes min read]),
    ),
    line(length: 100%, stroke: 0.5pt),
    ..if contents { (contents-line, line(length: 100%, stroke: 1.2pt)) } else { (line(length: 100%, stroke: 1.2pt),) },
  ))

  tb-figure-max.update(lerp(3in, 2in))
  columns(cols, gutter: gutter, {
    // The nameplate spans all columns at the top of page 1.
    place(top, float: true, scope: "parent", clearance: lerp(1.1em, 0.7em), nameplate)
    body
  })
}
