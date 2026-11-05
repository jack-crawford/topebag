//! HTML → Typst markup.
//!
//! [`html_to_typst`] walks the cleaned article HTML and emits Typst markup
//! that the template in `assets/topebag.typ` turns into pages.
//!
//! ## Design
//!
//! * **Safety first.** Arbitrary web text must never be interpreted as Typst
//!   syntax. Every character with meaning in Typst markup is escaped with a
//!   backslash (see [`escape_text`]), and every value passed to a Typst
//!   function goes through [`string_literal`]. Structural markup is always
//!   emitted in function-call form (`#heading(level: 2)[..]`,
//!   `#list([..], [..])`) rather than line-based shorthands, so content can
//!   never accidentally start a heading or list.
//! * **HTML is messy.** Text and blocks mix freely (`<div>text<p>more</p></div>`),
//!   so each container is rendered as a *flow*: runs of inline content are
//!   gathered into paragraphs, and block elements are emitted between them.
//!   Block elements found inside inline elements (an `<img>` inside an `<a>`)
//!   are deferred until the surrounding paragraph ends.
//! * **Whitespace follows HTML rules.** Runs of whitespace collapse to a
//!   single space, except inside `<pre>`.

use ego_tree::NodeRef;
use scraper::node::Element;
use scraper::{Html, Node};
use url::Url;

use crate::images::ImageResolver;
use crate::settings::LinkStyle;

/// Options that influence the generated markup.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub link_style: LinkStyle,
    pub include_images: bool,
    /// The document title. A leading heading with the same text is dropped,
    /// since the template already prints the title.
    pub title: Option<String>,
    /// Also drop page chrome (`header`, `footer`, `aside`). Used when
    /// Readability failed and the whole page body is being converted.
    pub strip_chrome: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self { link_style: LinkStyle::Plain, include_images: true, title: None, strip_chrome: false }
    }
}

/// Elements whose content never makes it to paper.
pub const SKIPPED_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "head", "meta", "link", "title", "nav", "form",
    "button", "input", "select", "option", "textarea", "svg", "canvas", "dialog", "map", "area",
    "track", "source", "param", "object", "portal", "slot", "menu",
];

/// Additional elements skipped when [`ConvertOptions::strip_chrome`] is set.
const CHROME_TAGS: &[&str] = &["header", "footer", "aside"];

/// Convert article HTML into Typst markup.
///
/// `base` is used to resolve relative links and image URLs. Images are
/// fetched (or looked up) through `images`.
pub fn html_to_typst(
    html: &str,
    base: &Url,
    options: &ConvertOptions,
    images: &mut dyn ImageResolver,
) -> String {
    let fragment = Html::parse_fragment(html);
    let mut cx = Converter {
        base,
        options,
        images,
        title_key: options.title.as_deref().map(normalize_key),
        seen_heading: false,
        depth: 0,
    };
    let root = fragment.root_element();
    let out = cx.flow(*root);
    tidy(&out)
}

/// Convert HTML into plain paragraphs only — no images, links, tables or
/// styling. Used as a last-resort fallback if the rich markup fails to
/// compile for some unforeseen reason.
pub fn html_to_plain_typst(html: &str) -> String {
    let fragment = Html::parse_fragment(html);
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    collect_plain(*fragment.root_element(), &mut current, &mut paragraphs);
    paragraphs.push(current);
    paragraphs
        .iter()
        .map(|p| escape_text(&collapse_ws(p)))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Plain, unescaped text of an HTML fragment (paragraphs separated by blank
/// lines). Used for full-text search.
pub fn html_to_plain_text(html: &str) -> String {
    let fragment = Html::parse_fragment(html);
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    collect_plain(*fragment.root_element(), &mut current, &mut paragraphs);
    paragraphs.push(current);
    paragraphs.iter().map(|p| collapse_ws(p)).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("

")
}

fn collect_plain(node: NodeRef<Node>, current: &mut String, out: &mut Vec<String>) {
    for child in node.children() {
        match child.value() {
            Node::Text(t) => current.push_str(t),
            Node::Element(el) => {
                let name = el.name();
                if SKIPPED_TAGS.contains(&name) {
                    continue;
                }
                let block = is_block(name);
                if block {
                    out.push(std::mem::take(current));
                }
                collect_plain(child, current, out);
                if block {
                    out.push(std::mem::take(current));
                }
            }
            _ => {}
        }
    }
}

struct Converter<'a> {
    base: &'a Url,
    options: &'a ConvertOptions,
    images: &'a mut dyn ImageResolver,
    title_key: Option<String>,
    seen_heading: bool,
    /// Nesting depth inside containers (lists, tables, quotes). Only
    /// top-level figures may float.
    depth: usize,
}

/// Inline content being accumulated into a paragraph.
#[derive(Default)]
struct Inline {
    out: String,
    /// Whitespace was seen and a single space should precede the next content.
    pending_space: bool,
    /// The content started with whitespace (needed when wrapping, e.g. in
    /// `#strong[..]`, so the space can be hoisted outside the wrapper).
    leading_space: bool,
    /// Block-level markup (images) found inside inline content.
    deferred: Vec<String>,
}

impl Inline {
    fn text(&mut self, text: &str) {
        for ch in text.chars() {
            // U+00A0 (no-break space) is deliberately *not* collapsed.
            if ch.is_whitespace() && ch != '\u{a0}' {
                if self.out.is_empty() {
                    self.leading_space = true;
                }
                self.pending_space = true;
            } else {
                self.flush_space();
                escape_char(ch, &mut self.out);
            }
        }
    }

    fn flush_space(&mut self) {
        if self.pending_space && !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push(' ');
        }
        self.pending_space = false;
    }

    fn markup(&mut self, markup: &str) {
        self.flush_space();
        self.out.push_str(markup);
    }

    fn linebreak(&mut self) {
        if let Some(rest) = self.out.strip_suffix("#linebreak()") {
            // `<br><br>` is how many older pages separate paragraphs.
            let len = rest.trim_end().len();
            self.out.truncate(len);
            self.out.push_str("\n\n");
        } else if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push_str("#linebreak()");
        }
        self.pending_space = false;
    }

    /// Merge a child's content, wrapping it with `wrap` if non-empty.
    fn absorb(&mut self, child: Inline, wrap: impl FnOnce(&str) -> String) {
        if child.leading_space {
            if self.out.is_empty() {
                self.leading_space = true;
            }
            self.pending_space = true;
        }
        if !child.out.is_empty() {
            let wrapped = wrap(&child.out);
            self.markup(&wrapped);
        }
        if child.pending_space {
            self.pending_space = true;
        }
        self.deferred.extend(child.deferred);
    }

    fn is_empty(&self) -> bool {
        self.out.is_empty()
    }
}

impl Converter<'_> {
    /// Render a container's children as a sequence of blocks.
    fn flow(&mut self, node: NodeRef<Node>) -> String {
        let mut blocks: Vec<String> = Vec::new();
        let mut para = Inline::default();
        for child in node.children() {
            match child.value() {
                Node::Text(text) => para.text(text),
                Node::Element(el) => {
                    if self.skip(el) {
                        continue;
                    }
                    if is_block(el.name()) {
                        flush_paragraph(&mut para, &mut blocks);
                        if let Some(block) = self.block(child, el) {
                            if !block.trim().is_empty() {
                                blocks.push(block);
                            }
                        }
                    } else if contains_structural_block(child) {
                        // An inline wrapper around real blocks, e.g.
                        // `<font><p>…</p><p>…</p></font>`: keep the
                        // paragraph structure rather than flattening it.
                        flush_paragraph(&mut para, &mut blocks);
                        let body = self.flow(child);
                        if !body.trim().is_empty() {
                            blocks.push(body);
                        }
                    } else {
                        self.inline_element(child, el, &mut para);
                    }
                }
                _ => {}
            }
        }
        flush_paragraph(&mut para, &mut blocks);
        blocks.join("\n\n")
    }

    fn skip(&self, el: &Element) -> bool {
        let name = el.name();
        if SKIPPED_TAGS.contains(&name) {
            return true;
        }
        if self.options.strip_chrome && CHROME_TAGS.contains(&name) {
            return true;
        }
        if el.attr("hidden").is_some() || el.attr("aria-hidden") == Some("true") {
            return true;
        }
        if let Some(style) = el.attr("style") {
            let style: String = style.chars().filter(|c| !c.is_whitespace()).collect();
            if style.contains("display:none") || style.contains("visibility:hidden") {
                return true;
            }
        }
        false
    }

    /// Render a block-level element.
    fn block(&mut self, node: NodeRef<Node>, el: &Element) -> Option<String> {
        let name = el.name();
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.heading(node, name),
            "ul" | "ol" => self.list(node, el),
            "blockquote" => {
                let body = self.nested(|cx| cx.flow(node));
                (!body.trim().is_empty()).then(|| format!("#quote(block: true)[{body}]"))
            }
            "pre" => self.code_block(node, el),
            "figure" => self.figure(node),
            "img" => self.image(el, None),
            "picture" => self.first_image(node).and_then(|img| self.image(img, None)),
            "table" => self.table(node),
            "hr" => Some("#tb-rule()".to_string()),
            "dl" => self.definition_list(node),
            "dt" | "summary" => {
                let mut inline = self.inline_children(node);
                inline.out = format!("#strong[{}]", inline.out);
                let mut blocks = Vec::new();
                flush_paragraph(&mut inline, &mut blocks);
                Some(blocks.join("\n\n"))
            }
            "dd" => {
                let body = self.nested(|cx| cx.flow(node));
                (!body.trim().is_empty()).then(|| format!("#pad(left: 1.2em)[{body}]"))
            }
            "iframe" | "video" | "audio" | "embed" => self.embed(node, el),
            "br" => None,
            // Everything else (p, div, section, article, li, …) is a plain
            // container whose children form a flow.
            _ => {
                let body = self.flow(node);
                (!body.trim().is_empty()).then_some(body)
            }
        }
    }

    fn heading(&mut self, node: NodeRef<Node>, name: &str) -> Option<String> {
        let mut inline = self.inline_children(node);
        if inline.is_empty() {
            return deferred_only(inline);
        }
        // Drop a leading heading that just repeats the document title.
        if !self.seen_heading {
            self.seen_heading = true;
            let text: String = node_text(node);
            if self.title_key.as_deref() == Some(normalize_key(&text).as_str()) {
                return deferred_only(inline);
            }
        }
        let level = match name {
            "h1" => 1,
            "h2" => 2,
            "h3" => 3,
            _ => 4,
        };
        let mut out = format!("#heading(level: {level})[{}]", strip_trailing_breaks(&inline.out));
        for d in inline.deferred.drain(..) {
            out.push_str("\n\n");
            out.push_str(&d);
        }
        Some(out)
    }

    fn list(&mut self, node: NodeRef<Node>, el: &Element) -> Option<String> {
        let mut items = Vec::new();
        for child in node.children() {
            match child.value() {
                Node::Element(item) if !self.skip(item) => {
                    let body = if is_block(item.name()) || item.name() == "li" {
                        self.nested(|cx| cx.flow(child))
                    } else {
                        let mut inline = Inline::default();
                        self.inline_element(child, item, &mut inline);
                        let mut blocks = Vec::new();
                        flush_paragraph(&mut inline, &mut blocks);
                        blocks.join("\n\n")
                    };
                    if !body.trim().is_empty() {
                        items.push(format!("[{body}]"));
                    }
                }
                Node::Text(t) if !t.trim().is_empty() => {
                    items.push(format!("[{}]", escape_text(&collapse_ws(t))));
                }
                _ => {}
            }
        }
        if items.is_empty() {
            return None;
        }
        let items = items.join(",\n  ");
        if el.name() == "ol" {
            let start = el.attr("start").and_then(|s| s.trim().parse::<i64>().ok()).filter(|&s| s >= 0);
            let numbering = match el.attr("type") {
                Some("a") => Some("a."),
                Some("A") => Some("A."),
                Some("i") => Some("i."),
                Some("I") => Some("I."),
                _ => None,
            };
            let mut args = String::new();
            if let Some(start) = start {
                args.push_str(&format!("start: {start}, "));
            }
            if let Some(numbering) = numbering {
                args.push_str(&format!("numbering: {}, ", string_literal(numbering)));
            }
            Some(format!("#enum({args}\n  {items}\n)"))
        } else {
            Some(format!("#list(\n  {items}\n)"))
        }
    }

    fn definition_list(&mut self, node: NodeRef<Node>) -> Option<String> {
        let mut parts = Vec::new();
        for child in node.children() {
            if let Node::Element(el) = child.value() {
                if !self.skip(el) {
                    if let Some(block) = self.block(child, el) {
                        if !block.trim().is_empty() {
                            parts.push(block);
                        }
                    }
                }
            }
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    fn code_block(&mut self, node: NodeRef<Node>, el: &Element) -> Option<String> {
        let mut code = String::new();
        preformatted_text(node, &mut code);
        let code = code.trim_end_matches(['\n', '\r', ' ', '\t']).trim_start_matches(['\n', '\r']);
        if code.trim().is_empty() {
            return None;
        }
        let lang = code_language(el).or_else(|| {
            node.children().find_map(|c| match c.value() {
                Node::Element(inner) if inner.name() == "code" => code_language(inner),
                _ => None,
            })
        });
        let lang = lang.map(|l| format!("lang: {}, ", string_literal(&l))).unwrap_or_default();
        Some(format!("#raw(block: true, {lang}{})", string_literal(code)))
    }

    fn figure(&mut self, node: NodeRef<Node>) -> Option<String> {
        let caption_node = node.descendants().find(|d| {
            matches!(d.value(), Node::Element(e) if e.name() == "figcaption")
        });
        let caption = caption_node.map(|c| self.inline_children(c)).filter(|c| !c.is_empty());

        let imgs: Vec<_> = node
            .descendants()
            .filter(|d| {
                matches!(d.value(), Node::Element(e) if e.name() == "img")
                    && !d.ancestors().any(|a| matches!(a.value(), Node::Element(e) if e.name() == "figcaption"))
            })
            .collect();

        if !imgs.is_empty() {
            let caption_markup = caption.as_ref().map(|c| strip_trailing_breaks(&c.out).to_string());
            let mut rendered = Vec::new();
            let count = imgs.len();
            for (i, img) in imgs.into_iter().enumerate() {
                let Node::Element(el) = img.value() else { continue };
                let cap = if i + 1 == count { caption_markup.as_deref() } else { None };
                if let Some(markup) = self.image(el, cap) {
                    rendered.push(markup);
                }
            }
            if rendered.is_empty() {
                // Images were dropped (setting off, or download failed); keep
                // the caption as a small paragraph so the text isn't lost.
                return caption.map(|c| format!("#tb-caption[{}]", c.out));
            }
            return Some(rendered.join("\n\n"));
        }

        // A figure without images (code listing, quote, embed…).
        let mut body = String::new();
        for child in node.children() {
            match child.value() {
                Node::Element(el) if el.name() == "figcaption" || self.skip(el) => {}
                Node::Element(el) if is_block(el.name()) => {
                    if let Some(b) = self.block(child, el) {
                        body.push_str(&b);
                        body.push_str("\n\n");
                    }
                }
                Node::Element(el) => {
                    let mut inline = Inline::default();
                    self.inline_element(child, el, &mut inline);
                    let mut blocks = Vec::new();
                    flush_paragraph(&mut inline, &mut blocks);
                    body.push_str(&blocks.join("\n\n"));
                    body.push_str("\n\n");
                }
                Node::Text(t) if !t.trim().is_empty() => {
                    body.push_str(&escape_text(&collapse_ws(t)));
                    body.push_str("\n\n");
                }
                _ => {}
            }
        }
        if let Some(c) = caption {
            body.push_str(&format!("#tb-caption[{}]", c.out));
        }
        (!body.trim().is_empty()).then(|| body.trim_end().to_string())
    }

    fn first_image<'n>(&self, node: NodeRef<'n, Node>) -> Option<&'n Element> {
        node.descendants().find_map(|d| match d.value() {
            Node::Element(e) if e.name() == "img" => Some(e),
            _ => None,
        })
    }

    /// Render an `<img>` as a figure (with an optional caption).
    fn image(&mut self, el: &Element, caption: Option<&str>) -> Option<String> {
        if !self.options.include_images {
            return None;
        }
        let src = image_source(el)?;
        let url = self.base.join(src.trim()).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let stored = self.images.resolve(&url)?;
        let caption = caption
            .filter(|c| !c.trim().is_empty())
            .map(|c| format!(", caption: [{c}]"))
            .unwrap_or_default();
        // Top-level figures float to the top/bottom of a page (like in a
        // book), which avoids half-empty pages before tall images. Figures
        // inside lists, tables or quotes stay where they are.
        let float = if self.depth == 0 { ", float: true" } else { "" };
        let (width, height) = display_size(el, stored.width, stored.height);
        Some(format!(
            "#tb-figure({}, {}, {}{caption}{float})",
            string_literal(&stored.typst_path()),
            width.max(1),
            height.max(1),
        ))
    }

    fn embed(&mut self, node: NodeRef<Node>, el: &Element) -> Option<String> {
        let src = el.attr("src").map(str::to_string).or_else(|| {
            node.descendants().find_map(|d| match d.value() {
                Node::Element(s) if s.name() == "source" => s.attr("src").map(str::to_string),
                _ => None,
            })
        })?;
        let url = self.base.join(src.trim()).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let kind = match el.name() {
            "video" => "Video",
            "audio" => "Audio",
            _ if url.host_str().is_some_and(|h| h.contains("youtube") || h.contains("vimeo")) => "Video",
            _ => "Embedded content",
        };
        Some(format!(
            "#tb-embed({}, {}, {})",
            string_literal(kind),
            string_literal(url.as_str()),
            string_literal(&breakable_url(url.as_str())),
        ))
    }

    fn table(&mut self, node: NodeRef<Node>) -> Option<String> {
        struct Cell {
            body: String,
            colspan: usize,
            rowspan: usize,
            header: bool,
        }

        // Collect rows that belong to *this* table (not nested tables).
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        let mut caption: Option<String> = None;
        let mut row_nodes = Vec::new();
        for child in node.children() {
            if let Node::Element(el) = child.value() {
                match el.name() {
                    "tr" => row_nodes.push((child, false)),
                    "thead" | "tbody" | "tfoot" => {
                        let in_head = el.name() == "thead";
                        for row in child.children() {
                            if matches!(row.value(), Node::Element(r) if r.name() == "tr") {
                                row_nodes.push((row, in_head));
                            }
                        }
                    }
                    "caption" => {
                        let c = self.inline_children(child);
                        if !c.is_empty() {
                            caption = Some(c.out);
                        }
                    }
                    _ => {}
                }
            }
        }
        for (row, in_head) in row_nodes {
            let mut cells = Vec::new();
            for cell in row.children() {
                let Node::Element(el) = cell.value() else { continue };
                if el.name() != "td" && el.name() != "th" {
                    continue;
                }
                let span = |attr: &str| {
                    el.attr(attr).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).clamp(1, 50)
                };
                cells.push(Cell {
                    body: self.nested(|cx| cx.flow(cell)),
                    colspan: span("colspan"),
                    rowspan: span("rowspan"),
                    header: in_head || el.name() == "th",
                });
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }

        let columns = rows.iter().map(|r| r.iter().map(|c| c.colspan).sum::<usize>()).max().unwrap_or(0);
        if columns == 0 {
            return None;
        }
        // Single-column tables are layout tables: just flow their content.
        if columns == 1 {
            let body: Vec<String> = rows
                .into_iter()
                .flatten()
                .map(|c| c.body)
                .filter(|b| !b.trim().is_empty())
                .collect();
            return (!body.is_empty()).then(|| body.join("\n\n"));
        }

        let row_count = rows.len();
        let header_rows = rows.iter().take_while(|r| r.iter().all(|c| c.header)).count();
        let render_cell = |c: &Cell, row_index: usize| -> String {
            let colspan = c.colspan.min(columns);
            let rowspan = c.rowspan.min(row_count - row_index);
            let body = if c.header && row_index >= header_rows {
                format!("#strong[{}]", c.body)
            } else {
                c.body.clone()
            };
            if colspan > 1 || rowspan > 1 {
                format!("table.cell(colspan: {colspan}, rowspan: {rowspan})[{body}]")
            } else {
                format!("[{body}]")
            }
        };

        let mut parts = Vec::new();
        if header_rows > 0 && header_rows < row_count {
            let cells: Vec<String> = rows[..header_rows]
                .iter()
                .enumerate()
                .flat_map(|(i, r)| r.iter().map(move |c| (i, c)))
                .map(|(i, c)| render_cell(c, i))
                .collect();
            parts.push(format!("table.header({})", cells.join(", ")));
        }
        let first_body_row = if header_rows < row_count { header_rows } else { 0 };
        for (i, row) in rows.iter().enumerate().skip(first_body_row) {
            for cell in row {
                parts.push(render_cell(cell, i));
            }
        }

        let size = if columns >= 6 { ", small: true" } else { "" };
        let mut out = format!("#tb-table(columns: {columns}{size},\n  {}\n)", parts.join(",\n  "));
        if let Some(caption) = caption {
            out.push_str(&format!("\n\n#tb-caption[{caption}]"));
        }
        Some(out)
    }

    /// Run `f` one container level deeper.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.depth += 1;
        let out = f(self);
        self.depth -= 1;
        out
    }

    /// Render the inline content of `node`'s children.
    fn inline_children(&mut self, node: NodeRef<Node>) -> Inline {
        let mut inline = Inline::default();
        for child in node.children() {
            match child.value() {
                Node::Text(t) => inline.text(t),
                Node::Element(el) if !self.skip(el) => self.inline_element(child, el, &mut inline),
                _ => {}
            }
        }
        inline
    }

    /// Render one inline element into `para`.
    fn inline_element(&mut self, node: NodeRef<Node>, el: &Element, para: &mut Inline) {
        match el.name() {
            "br" => para.linebreak(),
            "wbr" => {}
            "img" | "picture" | "figure" | "video" | "iframe" | "audio" | "embed" => {
                if let Some(block) = self.block(node, el) {
                    para.deferred.push(block);
                }
            }
            "a" => {
                let child = self.inline_children(node);
                let href = el
                    .attr("href")
                    .and_then(|h| self.base.join(h.trim()).ok())
                    .filter(|u| matches!(u.scheme(), "http" | "https" | "mailto"));
                match href {
                    Some(url) => {
                        let target = url.as_str().to_string();
                        let is_anchor = same_page(&url, self.base);
                        let footnote = self.options.link_style == LinkStyle::Footnotes
                            && !is_anchor
                            && url.scheme() != "mailto"
                            && !node_text(node).contains(url.host_str().unwrap_or("\u{0}"));
                        para.absorb(child, |body| {
                            let mut s = format!("#link({})[{body}]", string_literal(&target));
                            if footnote {
                                s.push_str(&format!(
                                    "#footnote[#link({})[{}]]",
                                    string_literal(&target),
                                    escape_text(&breakable_url(&target))
                                ));
                            }
                            s
                        });
                    }
                    None => para.absorb(child, |body| body.to_string()),
                }
            }
            "em" | "i" | "cite" | "dfn" | "var" => {
                let child = self.inline_children(node);
                para.absorb(child, |b| format!("#emph[{b}]"));
            }
            "strong" | "b" => {
                let child = self.inline_children(node);
                para.absorb(child, |b| format!("#strong[{b}]"));
            }
            "code" | "kbd" | "samp" | "tt" => {
                let mut text = String::new();
                preformatted_text(node, &mut text);
                let text = collapse_ws(&text);
                let text = text.trim();
                if !text.is_empty() {
                    para.markup(&format!("#raw({})", string_literal(text)));
                }
            }
            "sub" => {
                let child = self.inline_children(node);
                para.absorb(child, |b| format!("#sub[{b}]"));
            }
            "sup" => {
                let child = self.inline_children(node);
                para.absorb(child, |b| format!("#super[{b}]"));
            }
            "small" => {
                let child = self.inline_children(node);
                para.absorb(child, |b| format!("#text(size: 0.85em)[{b}]"));
            }
            "mark" => {
                let child = self.inline_children(node);
                para.absorb(child, |b| format!("#highlight[{b}]"));
            }
            "s" | "del" | "strike" => {
                let child = self.inline_children(node);
                para.absorb(child, |b| format!("#strike[{b}]"));
            }
            "u" | "ins" => {
                let child = self.inline_children(node);
                para.absorb(child, |b| format!("#underline[{b}]"));
            }
            "q" => {
                let child = self.inline_children(node);
                // Plain `"` in Typst markup becomes a language-aware smart quote.
                para.absorb(child, |b| format!("\"{b}\""));
            }
            // Transparent inline containers (span, abbr, time, font, label…)
            // and block elements nested in inline context: keep the text.
            _ => {
                let child = self.inline_children(node);
                para.absorb(child, |b| b.to_string());
            }
        }
    }
}

/// End the current paragraph: emit its text (if any) and deferred blocks.
fn flush_paragraph(para: &mut Inline, blocks: &mut Vec<String>) {
    let text = strip_trailing_breaks(para.out.trim());
    if !text.is_empty() {
        blocks.push(text.to_string());
    }
    blocks.append(&mut para.deferred);
    *para = Inline::default();
}

fn deferred_only(inline: Inline) -> Option<String> {
    (!inline.deferred.is_empty()).then(|| inline.deferred.join("\n\n"))
}

fn strip_trailing_breaks(s: &str) -> &str {
    let mut s = s.trim_end();
    while let Some(rest) = s.strip_suffix("#linebreak()") {
        s = rest.trim_end();
    }
    while let Some(rest) = s.strip_prefix("#linebreak()") {
        s = rest.trim_start();
    }
    s
}

/// Does `node` contain paragraph-level structure (as opposed to images,
/// which are handled by deferring them)?
fn contains_structural_block(node: NodeRef<Node>) -> bool {
    node.descendants().skip(1).any(|d| match d.value() {
        Node::Element(e) => {
            is_block(e.name())
                && !matches!(
                    e.name(),
                    "img" | "picture" | "figure" | "figcaption" | "video" | "iframe" | "audio" | "embed"
                )
        }
        _ => false,
    })
}

/// Elements rendered as blocks (they interrupt paragraphs).
fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "aside" | "address"
            | "blockquote" | "pre" | "figure" | "figcaption" | "ul" | "ol" | "li" | "dl" | "dt"
            | "dd" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th" | "caption"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "hr" | "img" | "picture" | "details"
            | "summary" | "iframe" | "video" | "audio" | "embed" | "center" | "html" | "body"
            | "hgroup" | "fieldset" | "legend"
    )
}

/// The size an image was meant to be shown at, in CSS pixels.
///
/// High-DPI pages serve images at 2× their display size (via `srcset`), so
/// the `width`/`height` attributes are preferred when they are *smaller*
/// than the pixel size. Images are never enlarged beyond their pixels.
fn display_size(el: &Element, px_w: u32, px_h: u32) -> (u32, u32) {
    let attr = |name: &str| {
        el.attr(name)
            .map(|v| v.trim().trim_end_matches("px"))
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 16.0)
    };
    let (pw, ph) = (f64::from(px_w.max(1)), f64::from(px_h.max(1)));
    match (attr("width"), attr("height")) {
        (Some(w), _) if w < pw => (w.round() as u32, (w * ph / pw).round() as u32),
        (None, Some(h)) if h < ph => ((h * pw / ph).round() as u32, h.round() as u32),
        _ => (px_w, px_h),
    }
}

/// Pick the best image URL from an `<img>`'s attributes.
///
/// Lazy-loading attributes win over `src` (which is often a placeholder),
/// and from a `srcset` we take the largest candidate up to ~1600 px wide.
fn image_source(el: &Element) -> Option<&str> {
    let usable = |s: &&str| !s.trim().is_empty() && !s.trim_start().starts_with("data:");
    for attr in ["data-src", "data-original", "data-lazy-src", "data-url", "data-hi-res-src"] {
        if let Some(v) = el.attr(attr).filter(usable) {
            return Some(v);
        }
    }
    if let Some(best) = el.attr("srcset").or_else(|| el.attr("data-srcset")).and_then(best_srcset) {
        return Some(best);
    }
    el.attr("src").filter(usable)
}

fn best_srcset(srcset: &str) -> Option<&str> {
    let mut candidates: Vec<(&str, f64)> = parse_srcset(srcset)
        .into_iter()
        .filter(|(url, _)| !url.starts_with("data:"))
        .map(|(url, descriptor)| {
            let size = descriptor
                .strip_suffix('w')
                .and_then(|w| w.parse::<f64>().ok())
                .or_else(|| descriptor.strip_suffix('x').and_then(|x| x.parse::<f64>().ok()).map(|x| x * 800.0))
                .unwrap_or(800.0);
            (url, size)
        })
        .collect();
    candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
    candidates
        .iter()
        .rev()
        .find(|(_, w)| *w <= 1600.0)
        .or_else(|| candidates.first())
        .map(|(u, _)| *u)
}

/// Split a `srcset` into (URL, descriptor) pairs.
///
/// Follows the HTML spec rather than splitting on commas: a URL runs until
/// whitespace, so image-CDN URLs that contain commas
/// (`…/fetch/w_424,c_limit,f_auto/…`) survive intact. A candidate ends at a
/// comma after its descriptor, or at a trailing comma on the URL itself.
fn parse_srcset(srcset: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = srcset;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if rest.is_empty() {
            break;
        }
        let url_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let raw_url = &rest[..url_end];
        rest = &rest[url_end..];
        let url = raw_url.trim_end_matches(',');
        let descriptor = if raw_url.ends_with(',') {
            ""
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let d = rest[..end].trim();
            rest = &rest[end..];
            d
        };
        if !url.is_empty() {
            out.push((url, descriptor));
        }
    }
    out
}

/// Language of a code block from `class="language-rust"` / `lang-rust`.
fn code_language(el: &Element) -> Option<String> {
    let classes = el.attr("class")?;
    classes.split_whitespace().find_map(|c| {
        let lang = c
            .strip_prefix("language-")
            .or_else(|| c.strip_prefix("lang-"))
            .or_else(|| c.strip_prefix("highlight-source-"))?;
        let lang = lang.to_ascii_lowercase();
        (!lang.is_empty() && lang.chars().all(|ch| ch.is_ascii_alphanumeric() || "+#-_".contains(ch)))
            .then_some(lang)
    })
}

/// Text inside `<pre>` with whitespace preserved and `<br>` as newlines.
fn preformatted_text(node: NodeRef<Node>, out: &mut String) {
    for child in node.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(el) if el.name() == "br" => out.push('\n'),
            Node::Element(el) if SKIPPED_TAGS.contains(&el.name()) => {}
            Node::Element(_) => preformatted_text(child, out),
            _ => {}
        }
    }
}

fn node_text(node: NodeRef<Node>) -> String {
    let mut out = String::new();
    for d in node.descendants() {
        if let Node::Text(t) = d.value() {
            out.push_str(t);
        }
    }
    out
}

fn same_page(url: &Url, base: &Url) -> bool {
    url.fragment().is_some()
        && url.host_str() == base.host_str()
        && url.path() == base.path()
        && url.query() == base.query()
}

/// Lower-case alphanumerics only, for fuzzy title comparison.
fn normalize_key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove runs of more than one blank line.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim().to_string()
}

/// Insert zero-width spaces after URL punctuation so long URLs can wrap.
pub fn breakable_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len() + url.len() / 8);
    for ch in url.chars() {
        out.push(ch);
        if matches!(ch, '/' | '.' | '-' | '_' | '?' | '&' | '=' | '#') {
            out.push('\u{200B}');
        }
    }
    out
}

/// Escape a single character for Typst markup.
fn escape_char(ch: char, out: &mut String) {
    match ch {
        // Characters with syntactic meaning somewhere in Typst markup.
        '\\' | '#' | '*' | '_' | '`' | '$' | '<' | '>' | '@' | '[' | ']' | '=' | '-' | '+' | '/'
        | '~' | '{' | '}' | '(' | ')' | '.' | ';' | ':' => {
            out.push('\\');
            out.push(ch);
        }
        // Control characters would only cause trouble.
        c if c.is_control() => {}
        // `'` and `"` stay unescaped on purpose: Typst turns them into
        // typographically correct, language-aware smart quotes.
        c => out.push(c),
    }
}

/// Escape arbitrary text for safe inclusion in Typst markup.
pub fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for ch in text.chars() {
        escape_char(ch, &mut out);
    }
    out
}

/// Quote `s` as a Typst string literal.
pub fn string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::{NoImages, StoredImage};

    fn convert(html: &str) -> String {
        convert_with(html, &ConvertOptions::default())
    }

    fn convert_with(html: &str, options: &ConvertOptions) -> String {
        let base = Url::parse("https://example.com/blog/post").unwrap();
        html_to_typst(html, &base, options, &mut NoImages)
    }

    struct FakeImages;
    impl ImageResolver for FakeImages {
        fn resolve(&mut self, url: &Url) -> Option<StoredImage> {
            Some(StoredImage { source_url: url.to_string(), file: "001.jpg".into(), width: 800, height: 600 })
        }
    }

    #[test]
    fn escapes_markup_characters() {
        let out = convert("<p>Use #hashtags, *stars*, $money$ and [brackets] = fun_times</p>");
        assert_eq!(
            out,
            r"Use \#hashtags, \*stars\*, \$money\$ and \[brackets\] \= fun\_times"
        );
    }

    #[test]
    fn numbered_looking_paragraph_is_not_a_list() {
        assert_eq!(convert("<p>1. Not a list</p>"), r"1\. Not a list");
        assert_eq!(convert("<p>- dash</p>"), r"\- dash");
    }

    #[test]
    fn collapses_whitespace_and_keeps_spaces_around_inline_elements() {
        let out = convert("<p>  Hello\n   <strong> bold </strong>world  </p>");
        assert_eq!(out, "Hello #strong[bold] world");
        let out = convert("<p>a<em>b</em>c</p>");
        assert_eq!(out, "a#emph[b]c");
    }

    #[test]
    fn paragraphs_and_mixed_content() {
        let out = convert("<div>Loose text<p>Para one</p>tail</div>");
        assert_eq!(out, "Loose text\n\nPara one\n\ntail");
    }

    #[test]
    fn headings_skip_duplicate_title() {
        let options = ConvertOptions { title: Some("My Great Post!".into()), ..Default::default() };
        let out = convert_with("<h1>My great post</h1><h2>Section</h2><p>Body</p>", &options);
        assert_eq!(out, "#heading(level: 2)[Section]\n\nBody");
    }

    #[test]
    fn nested_lists() {
        let out = convert("<ul><li>One</li><li>Two<ol start=\"3\"><li>Three</li></ol></li></ul>");
        assert!(out.starts_with("#list("), "{out}");
        assert!(out.contains("[One]"));
        assert!(out.contains("#enum(start: 3,"), "{out}");
        assert!(out.contains("[Three]"));
    }

    #[test]
    fn links_resolve_relative_urls_and_optionally_footnote() {
        let out = convert(r#"<p>See <a href="/docs">the docs</a>.</p>"#);
        assert_eq!(out, r#"See #link("https://example.com/docs")[the docs]\."#);

        let options = ConvertOptions { link_style: LinkStyle::Footnotes, ..Default::default() };
        let out = convert_with(r#"<p><a href="https://rust-lang.org/learn">Learn</a></p>"#, &options);
        assert!(out.contains("#footnote[#link(\"https://rust-lang.org/learn\")"), "{out}");
    }

    #[test]
    fn code_blocks_keep_whitespace_and_language() {
        let out = convert("<pre><code class=\"language-rust\">fn main() {\n    println!(\"hi\");\n}</code></pre>");
        assert_eq!(
            out,
            r#"#raw(block: true, lang: "rust", "fn main() {\n    println!(\"hi\");\n}")"#
        );
        assert_eq!(convert("<p>Run <code>cargo  run</code></p>"), r#"Run #raw("cargo run")"#);
    }

    #[test]
    fn skips_scripts_navigation_and_hidden_content() {
        let out = convert(
            "<nav>Menu</nav><script>alert(1)</script><p>Keep</p><div hidden>Gone</div><p style=\"display: none\">Gone</p>",
        );
        assert_eq!(out, "Keep");
    }

    #[test]
    fn images_become_figures_with_captions() {
        let base = Url::parse("https://example.com/a/").unwrap();
        let html = r#"<figure><img src="pic.jpg"><figcaption>A <em>nice</em> view</figcaption></figure>"#;
        let out = html_to_typst(html, &base, &ConvertOptions::default(), &mut FakeImages);
        assert_eq!(out, r#"#tb-figure("/img/001.jpg", 800, 600, caption: [A #emph[nice] view], float: true)"#);

        // An image inside a link inside a paragraph is deferred after the text.
        let html = r#"<p>Before <a href="x"><img src="pic.jpg"></a> after</p>"#;
        let out = html_to_typst(html, &base, &ConvertOptions::default(), &mut FakeImages);
        assert_eq!(out, "Before after\n\n#tb-figure(\"/img/001.jpg\", 800, 600, float: true)");

        // A 2× image with width/height attributes is shown at its CSS size.
        let html = r#"<img src="pic.jpg" width="200" height="150">"#;
        let out = html_to_typst(html, &base, &ConvertOptions::default(), &mut FakeImages);
        assert_eq!(out, "#tb-figure(\"/img/001.jpg\", 200, 150, float: true)");

        // Inside a list, figures must not float.
        let html = r#"<ul><li><img src="pic.jpg"></li></ul>"#;
        let out = html_to_typst(html, &base, &ConvertOptions::default(), &mut FakeImages);
        assert!(out.contains(r#"#tb-figure("/img/001.jpg", 800, 600)"#), "{out}");
    }

    #[test]
    fn double_line_breaks_are_paragraphs() {
        let out = convert("<div>One<br>still one<br><br>\nTwo<br> <br>Three</div>");
        assert_eq!(out, "One#linebreak()still one\n\nTwo\n\nThree");
    }

    #[test]
    fn inline_wrappers_around_blocks_keep_paragraphs() {
        let out = convert("<font face=verdana><p>First para.</p><p>Second para.</p></font>");
        assert_eq!(out, "First para\\.\n\nSecond para\\.");
    }

    #[test]
    fn srcset_urls_may_contain_commas() {
        let srcset = "https://cdn.example/fetch/w_424,c_limit,f_auto/a.png 424w, \
                      https://cdn.example/fetch/w_1456,c_limit,f_auto/a.png 1456w";
        assert_eq!(
            parse_srcset(srcset),
            vec![
                ("https://cdn.example/fetch/w_424,c_limit,f_auto/a.png", "424w"),
                ("https://cdn.example/fetch/w_1456,c_limit,f_auto/a.png", "1456w"),
            ]
        );
        assert_eq!(best_srcset(srcset), Some("https://cdn.example/fetch/w_1456,c_limit,f_auto/a.png"));
        // Compact form without spaces after commas, and bare URLs.
        assert_eq!(parse_srcset("a.jpg 1x,b.jpg 2x"), vec![("a.jpg", "1x"), ("b.jpg", "2x")]);
        assert_eq!(parse_srcset("only.jpg"), vec![("only.jpg", "")]);
        assert_eq!(parse_srcset("one.jpg, two.jpg 2x"), vec![("one.jpg", ""), ("two.jpg", "2x")]);
    }

    #[test]
    fn tables_with_headers_and_spans() {
        let out = convert(
            "<table><thead><tr><th>Name</th><th>Qty</th></tr></thead>\
             <tbody><tr><td>Apples</td><td>3</td></tr><tr><td colspan=2>Total</td></tr></tbody></table>",
        );
        assert!(out.starts_with("#tb-table(columns: 2,"), "{out}");
        assert!(out.contains("table.header([Name], [Qty])"));
        assert!(out.contains("table.cell(colspan: 2, rowspan: 1)[Total]"));
    }

    #[test]
    fn layout_tables_are_flattened() {
        assert_eq!(convert("<table><tr><td>Only</td></tr><tr><td>Column</td></tr></table>"), "Only\n\nColumn");
    }

    #[test]
    fn string_literals_are_escaped() {
        assert_eq!(string_literal("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
    }

    #[test]
    fn plain_fallback_is_text_only() {
        let out = html_to_plain_typst("<h1>Title</h1><p>One <b>two</b></p><script>x</script><p>3 * 4</p>");
        assert_eq!(out, "Title\n\nOne two\n\n3 \\* 4");
    }
}
