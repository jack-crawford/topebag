//! Editable documents.
//!
//! When a link is added, its cleaned article HTML is split into **blocks**:
//! headings, paragraphs, lists, quotes, code, images, tables, rules and
//! embeds. The editor works on these blocks. You can untick a block to drop
//! it, move it, or rewrite its text in light Markdown. The document is then
//! rebuilt from the included blocks, in order, through the same safe
//! HTML → Typst converter as before.
//!
//! A [`Draft`] holds the blocks, the editable masthead fields ([`Meta`]), and
//! the document's own [`Format`] (paper, type size, side-by-side…). It's
//! stored as `document.json` next to the entry, so edits survive restarts and
//! "Restore original" can always go back to `article.html`.
//!
//! Editing round-trips through Markdown: a block's HTML becomes Markdown
//! for the text box ([`html_to_markdown`]), and the edited Markdown becomes
//! HTML again ([`markdown_to_html`], via `pulldown-cmark`). Bold, italics,
//! code and links survive an edit.

use std::fs;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use ego_tree::NodeRef;
use scraper::{ElementRef, Html, Node};
use serde::{Deserialize, Serialize};

use crate::convert::SKIPPED_TAGS;
use crate::settings::Format;

/// What a block is, which decides how it's shown and edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    List,
    Quote,
    Code,
    Image,
    Table,
    Rule,
    Embed,
    Other,
}

impl BlockKind {
    /// Short label for the editor.
    pub fn label(self) -> String {
        match self {
            BlockKind::Heading(n) => format!("H{n}"),
            BlockKind::Paragraph => "Text".into(),
            BlockKind::List => "List".into(),
            BlockKind::Quote => "Quote".into(),
            BlockKind::Code => "Code".into(),
            BlockKind::Image => "Image".into(),
            BlockKind::Table => "Table".into(),
            BlockKind::Rule => "Rule".into(),
            BlockKind::Embed => "Embed".into(),
            BlockKind::Other => "Block".into(),
        }
    }

    /// Can the block's text be rewritten in the editor?
    pub fn editable(self) -> bool {
        matches!(
            self,
            BlockKind::Heading(_) | BlockKind::Paragraph | BlockKind::List | BlockKind::Quote | BlockKind::Code
        )
    }
}

fn yes() -> bool {
    true
}

/// How much of a heading's section is printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inclusion {
    All,
    Partly,
    Nothing,
}

/// One block of an article.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Position in the original article (stable across moves).
    pub id: u32,
    pub kind: BlockKind,
    /// The block's original HTML.
    pub html: String,
    /// Whether the block is printed.
    #[serde(default = "yes")]
    pub included: bool,
    /// Replacement text (light Markdown) if the user rewrote the block.
    #[serde(default)]
    pub edited: Option<String>,
}

impl Block {
    /// A one-line plain-text summary for the editor list.
    pub fn preview(&self) -> String {
        let text = match &self.edited {
            Some(md) => md.replace(['*', '`', '#', '[', ']'], ""),
            None => plain_text(&self.html),
        };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = match self.kind {
            BlockKind::Image if text.is_empty() => image_label(&self.html),
            BlockKind::Rule => "———".to_string(),
            BlockKind::Embed if text.is_empty() => "Embedded media".to_string(),
            _ => text,
        };
        if text.chars().count() > 160 {
            format!("{}…", text.chars().take(159).collect::<String>())
        } else {
            text
        }
    }

    /// The text to put in the editor's text box.
    pub fn markdown(&self) -> String {
        self.edited.clone().unwrap_or_else(|| html_to_markdown(&self.html, self.kind))
    }

    /// The block's HTML, with the user's edit applied.
    pub fn to_html(&self) -> String {
        match &self.edited {
            Some(md) => markdown_to_html(md, self.kind),
            None => self.html.clone(),
        }
    }
}

/// The editable masthead fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub title: String,
    #[serde(default)]
    pub byline: Option<String>,
    #[serde(default)]
    pub published: Option<String>,
}

/// An article being edited: masthead, blocks and formatting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub meta: Meta,
    /// What the masthead said before any edits, for "Restore original".
    pub original_meta: Meta,
    pub blocks: Vec<Block>,
    #[serde(default)]
    pub format: Format,
}

impl Draft {
    /// A fresh draft of an article, with every block included.
    pub fn from_article(html: &str, meta: Meta, format: Format) -> Draft {
        Draft { original_meta: meta.clone(), meta, blocks: split_blocks(html), format }
    }

    /// The HTML of the included blocks, in their current order.
    pub fn html(&self) -> String {
        self.blocks.iter().filter(|b| b.included).map(Block::to_html).collect::<Vec<_>>().join("\n")
    }

    /// Undo all edits: every block back, in the original order, unedited,
    /// and the masthead as it was. Formatting is kept.
    pub fn restore_original(&mut self, original_html: &str) {
        self.blocks = split_blocks(original_html);
        self.meta = self.original_meta.clone();
    }

    /// Move block `index` one place up or down. Returns the new index.
    pub fn move_block(&mut self, index: usize, up: bool) -> usize {
        let target = if up { index.checked_sub(1) } else { Some(index + 1).filter(|&i| i < self.blocks.len()) };
        match target {
            Some(t) if index < self.blocks.len() => {
                self.blocks.swap(index, t);
                t
            }
            _ => index,
        }
    }

    fn level(&self, index: usize) -> Option<u8> {
        match self.blocks.get(index)?.kind {
            BlockKind::Heading(level) => Some(level),
            _ => None,
        }
    }

    /// The blocks a heading governs: the heading and everything after it up
    /// to the next heading of the same or a higher level. An H2's section
    /// includes its H3 subsections and ends at the next H2 (or H1). Any other
    /// block's section is just that block.
    pub fn section(&self, index: usize) -> Range<usize> {
        let len = self.blocks.len();
        let Some(level) = self.level(index) else { return index.min(len)..(index + 1).min(len) };
        let end = (index + 1..len).find(|&j| self.level(j).is_some_and(|l| l <= level)).unwrap_or(len);
        index..end
    }

    /// How much of block `index`'s section prints.
    pub fn section_inclusion(&self, index: usize) -> Inclusion {
        let blocks = &self.blocks[self.section(index)];
        match blocks.iter().filter(|b| b.included).count() {
            0 => Inclusion::Nothing,
            n if n == blocks.len() => Inclusion::All,
            _ => Inclusion::Partly,
        }
    }

    /// Print or leave out a heading together with everything beneath it.
    pub fn set_section_included(&mut self, index: usize, included: bool) {
        let range = self.section(index);
        for block in &mut self.blocks[range] {
            block.included = included;
        }
    }

    /// How deeply each block sits under headings: 0 for blocks before the
    /// first heading and for top-level headings; a heading's contents are one
    /// deeper than the heading.
    pub fn depths(&self) -> Vec<usize> {
        let mut open: Vec<u8> = Vec::new();
        self.blocks
            .iter()
            .map(|block| match block.kind {
                BlockKind::Heading(level) => {
                    while open.last().is_some_and(|&l| l >= level) {
                        open.pop();
                    }
                    open.push(level);
                    open.len() - 1
                }
                _ => open.len(),
            })
            .collect()
    }

    /// Move a heading one step up or down **with its section**: past the
    /// neighbouring section of the same level, or past a single block where
    /// there's no such neighbour (which moves it out of, or into, the
    /// surrounding section). Other blocks move like [`Draft::move_block`].
    /// Returns the heading's new index.
    pub fn move_section(&mut self, index: usize, up: bool) -> usize {
        let Some(level) = self.level(index) else { return self.move_block(index, up) };
        let section = self.section(index);
        if up {
            let Some(above) = index.checked_sub(1) else { return index };
            // The nearest heading above at this level or higher: if it's at
            // this level, its section ends right here and we swap with it.
            let start = (0..index)
                .rev()
                .find(|&j| self.level(j).is_some_and(|l| l <= level))
                .filter(|&j| self.level(j) == Some(level))
                .unwrap_or(above);
            self.blocks[start..section.end].rotate_left(index - start);
            start
        } else {
            let next = section.end;
            if next >= self.blocks.len() {
                return index;
            }
            let next_end = if self.level(next) == Some(level) { self.section(next).end } else { next + 1 };
            self.blocks[index..next_end].rotate_left(next - index);
            index + (next_end - next)
        }
    }

    /// Number of blocks that will be printed.
    pub fn included_count(&self) -> usize {
        self.blocks.iter().filter(|b| b.included).count()
    }

    /// Whether anything differs from the original article.
    pub fn is_modified(&self) -> bool {
        self.meta != self.original_meta
            || self.blocks.iter().enumerate().any(|(i, b)| !b.included || b.edited.is_some() || b.id as usize != i)
    }

    pub fn load(path: &Path) -> Result<Draft> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_str(text.trim_start_matches('\u{feff}'))?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Splitting HTML into blocks
// ---------------------------------------------------------------------------

/// Containers that are looked *through* when they hold block content.
const CONTAINERS: &[&str] = &[
    "div", "section", "article", "main", "header", "footer", "aside", "span", "font", "center", "body", "html",
    "hgroup", "address",
];

const BLOCKS: &[&str] = &[
    "p", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "dl", "blockquote", "pre", "figure", "picture", "img",
    "table", "hr", "iframe", "video", "audio", "embed", "details", "fieldset",
];

/// Split article HTML into top-level blocks.
pub fn split_blocks(html: &str) -> Vec<Block> {
    let fragment = Html::parse_fragment(html);
    let mut out: Vec<(BlockKind, String)> = Vec::new();
    let mut inline = String::new();
    walk(*fragment.root_element(), &mut out, &mut inline);
    flush_inline(&mut inline, &mut out);
    out.into_iter()
        .enumerate()
        .map(|(i, (kind, html))| Block { id: i as u32, kind, html, included: true, edited: None })
        .collect()
}

fn walk(node: NodeRef<Node>, out: &mut Vec<(BlockKind, String)>, inline: &mut String) {
    for child in node.children() {
        match child.value() {
            Node::Text(t) => inline.push_str(&escape_html(t)),
            Node::Element(el) => {
                let name = el.name();
                if SKIPPED_TAGS.contains(&name) {
                    continue;
                }
                let Some(element) = ElementRef::wrap(child) else { continue };
                let is_container = CONTAINERS.contains(&name);
                if is_container && has_block_descendant(child) {
                    flush_inline(inline, out);
                    walk(child, out, inline);
                } else if BLOCKS.contains(&name) || is_container {
                    flush_inline(inline, out);
                    let html = element.html();
                    if let Some(kind) = classify(child, name) {
                        out.push((kind, html));
                    }
                } else {
                    inline.push_str(&element.html());
                }
            }
            _ => {}
        }
    }
}

fn flush_inline(inline: &mut String, out: &mut Vec<(BlockKind, String)>) {
    let html = std::mem::take(inline);
    if plain_text(&html).trim().is_empty() && !html.contains("<img") {
        return;
    }
    let kind = if plain_text(&html).trim().is_empty() { BlockKind::Image } else { BlockKind::Paragraph };
    out.push((kind, format!("<p>{}</p>", html.trim())));
}

fn has_block_descendant(node: NodeRef<Node>) -> bool {
    node.descendants().skip(1).any(|d| matches!(d.value(), Node::Element(e) if BLOCKS.contains(&e.name())))
}

/// Decide a block's kind; `None` drops empty blocks.
fn classify(node: NodeRef<Node>, name: &str) -> Option<BlockKind> {
    let text = node.descendants().filter_map(|d| d.value().as_text().map(|t| t.to_string())).collect::<String>();
    let has_text = !text.trim().is_empty();
    let has = |tag: &str| node.descendants().any(|d| matches!(d.value(), Node::Element(e) if e.name() == tag));
    let kind = match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => BlockKind::Heading(name[1..].parse().unwrap_or(2)),
        "ul" | "ol" | "dl" => BlockKind::List,
        "blockquote" => BlockKind::Quote,
        "pre" => BlockKind::Code,
        "table" => BlockKind::Table,
        "hr" => return Some(BlockKind::Rule),
        "iframe" | "video" | "audio" | "embed" => return Some(BlockKind::Embed),
        "img" => return Some(BlockKind::Image),
        "figure" | "picture" if has("img") => return Some(BlockKind::Image),
        "p" if !has_text && has("img") => return Some(BlockKind::Image),
        _ if CONTAINERS.contains(&name) && !has_text && has("img") => return Some(BlockKind::Image),
        "p" => BlockKind::Paragraph,
        _ if CONTAINERS.contains(&name) => BlockKind::Paragraph,
        _ => BlockKind::Other,
    };
    has_text.then_some(kind)
}

fn plain_text(html: &str) -> String {
    // Join text pieces directly ("<em>mixed</em>," → "mixed,"); block
    // boundaries inside one block are rare and whitespace is collapsed later.
    let fragment = Html::parse_fragment(html);
    fragment.root_element().text().collect::<String>()
}

fn image_label(html: &str) -> String {
    let fragment = Html::parse_fragment(html);
    let caption = fragment.root_element().descendants().find_map(|d| match d.value() {
        Node::Element(e) if e.name() == "figcaption" => {
            ElementRef::wrap(d).map(|c| c.text().collect::<Vec<_>>().join(" "))
        }
        _ => None,
    });
    let alt = || {
        fragment.root_element().descendants().find_map(|d| match d.value() {
            Node::Element(e) if e.name() == "img" => e.attr("alt").map(str::to_string),
            _ => None,
        })
    };
    caption.or_else(alt).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "Image".into())
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Markdown round trip
// ---------------------------------------------------------------------------

/// The text of a block as light Markdown, for editing.
pub fn html_to_markdown(html: &str, kind: BlockKind) -> String {
    let fragment = Html::parse_fragment(html);
    let root = *fragment.root_element();
    let element = root.children().find(|c| c.value().is_element()).unwrap_or(root);
    match kind {
        BlockKind::Code => {
            let mut text = String::new();
            for d in element.descendants() {
                match d.value() {
                    Node::Text(t) => text.push_str(t),
                    Node::Element(e) if e.name() == "br" => text.push('\n'),
                    _ => {}
                }
            }
            text.trim_end().to_string()
        }
        BlockKind::List => list_markdown(element, 0),
        BlockKind::Quote => {
            let parts: Vec<String> = element
                .children()
                .filter_map(|c| match c.value() {
                    Node::Element(e) if BLOCKS.contains(&e.name()) || CONTAINERS.contains(&e.name()) => {
                        Some(inline_markdown(c))
                    }
                    Node::Text(t) if !t.trim().is_empty() => Some(escape_markdown(t)),
                    Node::Element(_) => Some(inline_markdown(c)),
                    _ => None,
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            parts.join("\n\n")
        }
        _ => inline_markdown(element).trim().to_string(),
    }
}

fn list_markdown(list: NodeRef<Node>, depth: usize) -> String {
    let ordered = matches!(list.value(), Node::Element(e) if e.name() == "ol");
    let mut lines = Vec::new();
    let mut n = 0;
    for item in list.children() {
        let Node::Element(e) = item.value() else { continue };
        if e.name() != "li" && e.name() != "dt" && e.name() != "dd" {
            continue;
        }
        n += 1;
        let marker = if ordered { format!("{n}.") } else { "-".to_string() };
        // The item's own text, excluding nested lists.
        let mut text = String::new();
        for child in item.children() {
            match child.value() {
                Node::Element(c) if c.name() == "ul" || c.name() == "ol" => {}
                Node::Text(t) => text.push_str(&escape_markdown(t)),
                Node::Element(_) => text.push_str(&inline_markdown(child)),
                _ => {}
            }
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(format!("{}{marker} {text}", "  ".repeat(depth)));
        for child in item.children() {
            if matches!(child.value(), Node::Element(c) if c.name() == "ul" || c.name() == "ol") {
                lines.push(list_markdown(child, depth + 1));
            }
        }
    }
    lines.join("\n")
}

fn inline_markdown(node: NodeRef<Node>) -> String {
    let mut out = String::new();
    for child in node.children() {
        match child.value() {
            Node::Text(t) => out.push_str(&escape_markdown(t)),
            Node::Element(e) => {
                let inner = || inline_markdown(child).trim().to_string();
                match e.name() {
                    "strong" | "b" => out.push_str(&wrap("**", &inner())),
                    "em" | "i" | "cite" => out.push_str(&wrap("*", &inner())),
                    "s" | "del" | "strike" => out.push_str(&wrap("~~", &inner())),
                    "code" | "kbd" | "samp" => {
                        let text: String = ElementRef::wrap(child).map(|c| c.text().collect()).unwrap_or_default();
                        if !text.trim().is_empty() {
                            out.push_str(&format!("`{}`", text.replace('`', "'")));
                        }
                    }
                    "a" => {
                        let text = inner();
                        match e.attr("href").filter(|h| !h.trim().is_empty()) {
                            Some(href) if !text.is_empty() => {
                                out.push_str(&format!("[{text}]({})", href.trim().replace(' ', "%20").replace(')', "%29")))
                            }
                            _ => out.push_str(&text),
                        }
                    }
                    "br" => out.push_str("\\\n"),
                    "img" => {
                        if let Some(src) = e.attr("src") {
                            let alt = e.attr("alt").unwrap_or("").replace([']', '['], "");
                            out.push_str(&format!("![{alt}]({})", src.replace(' ', "%20")));
                        }
                    }
                    name if SKIPPED_TAGS.contains(&name) => {}
                    _ => out.push_str(&inline_markdown(child)),
                }
            }
            _ => {}
        }
    }
    // Collapse whitespace but keep explicit line breaks.
    out.split("\\\n")
        .map(|part| part.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\\\n")
}

fn wrap(marker: &str, inner: &str) -> String {
    if inner.is_empty() { String::new() } else { format!("{marker}{inner}{marker}") }
}

fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '>' | '#' | '~' | '|') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Turn edited Markdown back into HTML for a block of the given kind.
pub fn markdown_to_html(md: &str, kind: BlockKind) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let render = |text: &str| {
        let mut out = String::new();
        html::push_html(&mut out, Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES));
        out
    };
    match kind {
        BlockKind::Code => format!("<pre><code>{}</code></pre>", escape_html(md)),
        BlockKind::Heading(level) => {
            // Render inline only: strip any leading '#' the user typed and the
            // <p> wrapper pulldown adds.
            let text = md.trim().trim_start_matches('#').trim();
            let inner = render(text);
            let inner = inner.trim().trim_start_matches("<p>").trim_end_matches("</p>");
            format!("<h{level}>{inner}</h{level}>")
        }
        BlockKind::Quote => format!("<blockquote>{}</blockquote>", render(md)),
        _ => render(md),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline() -> Draft {
        let html = "<p>intro</p><h2>A</h2><p>a1</p><h3>A.1</h3><p>a11</p><h2>B</h2><p>b1</p><h2>C</h2><p>c1</p>";
        Draft::from_article(html, Meta::default(), Format::default())
    }

    fn order(draft: &Draft) -> Vec<String> {
        draft.blocks.iter().map(Block::preview).collect()
    }

    #[test]
    fn headings_govern_everything_beneath_them() {
        let mut d = outline();
        assert_eq!(order(&d), ["intro", "A", "a1", "A.1", "a11", "B", "b1", "C", "c1"]);
        assert_eq!(d.section(1), 1..5, "an H2 takes its H3 subsection with it");
        assert_eq!(d.section(3), 3..5);
        assert_eq!(d.section(7), 7..9, "the last section runs to the end");
        assert_eq!(d.section(0), 0..1, "a paragraph is its own section");
        assert_eq!(d.depths(), [0, 0, 1, 1, 2, 0, 1, 0, 1]);

        d.set_section_included(1, false);
        let printed: Vec<bool> = d.blocks.iter().map(|b| b.included).collect();
        assert_eq!(printed, [true, false, false, false, false, true, true, true, true]);
        assert_eq!(d.section_inclusion(1), Inclusion::Nothing);
        d.blocks[4].included = true;
        assert_eq!(d.section_inclusion(1), Inclusion::Partly);
        assert_eq!(d.section_inclusion(3), Inclusion::Partly);
        d.set_section_included(1, true);
        assert_eq!(d.section_inclusion(1), Inclusion::All);
        assert_eq!(d.section_inclusion(0), Inclusion::All);
    }

    #[test]
    fn headings_move_with_their_sections() {
        let mut d = outline();
        assert_eq!(d.move_section(5, true), 1, "B jumps over the whole of A");
        assert_eq!(order(&d), ["intro", "B", "b1", "A", "a1", "A.1", "a11", "C", "c1"]);
        assert_eq!(d.move_section(3, false), 5, "A jumps over C");
        assert_eq!(order(&d), ["intro", "B", "b1", "C", "c1", "A", "a1", "A.1", "a11"]);
        // No sibling above: A.1 steps over a single block.
        assert_eq!(d.move_section(7, true), 6);
        assert_eq!(order(&d), ["intro", "B", "b1", "C", "c1", "A", "A.1", "a11", "a1"]);
        // At the edges nothing moves; paragraphs still move one step.
        assert_eq!(d.move_section(1, true), 0, "B steps above the intro paragraph");
        let last = d.blocks.iter().position(|b| b.preview() == "A").unwrap();
        assert_eq!(d.move_section(last, false), last, "the last section can't go lower");
        assert_eq!(d.move_section(0, true), 0);
        assert_eq!(d.move_section(2, false), 3);
    }

    const HTML: &str = r#"<div><div class="intro"><h2>Why paper</h2><p>Paper is <strong>calm</strong>, see <a href="https://example.com/x">this</a>.</p></div>
        Loose <em>text</em> here.
        <figure><img src="a.jpg" alt="A press"><figcaption>The press</figcaption></figure>
        <p><img src="b.jpg" alt="B"></p>
        <ul><li>One</li><li>Two<ul><li>Nested</li></ul></li></ul>
        <blockquote><p>First</p><p>Second</p></blockquote>
        <pre><code>fn main() {}</code></pre>
        <p>   </p>
        <hr>
        <table><tr><td>a</td><td>b</td></tr></table></div>"#;

    #[test]
    fn splits_into_blocks_through_containers() {
        let blocks = split_blocks(HTML);
        let kinds: Vec<BlockKind> = blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![
                BlockKind::Heading(2),
                BlockKind::Paragraph,
                BlockKind::Paragraph, // loose inline text
                BlockKind::Image,
                BlockKind::Image,
                BlockKind::List,
                BlockKind::Quote,
                BlockKind::Code,
                BlockKind::Rule,
                BlockKind::Table,
            ]
        );
        assert_eq!(blocks[2].preview(), "Loose text here.");
        assert_eq!(blocks[1].preview(), "Paper is calm, see this.", "no space before punctuation");
        assert_eq!(blocks[3].preview(), "The press");
        assert_eq!(blocks[4].preview(), "B");
        assert!(blocks.iter().enumerate().all(|(i, b)| b.id as usize == i));
    }

    #[test]
    fn markdown_round_trip_keeps_inline_formatting() {
        let blocks = split_blocks(HTML);
        let md = blocks[1].markdown();
        assert_eq!(md, "Paper is **calm**, see [this](https://example.com/x).");
        let html = markdown_to_html(&md, BlockKind::Paragraph);
        assert!(html.contains("<strong>calm</strong>") && html.contains(r#"<a href="https://example.com/x">this</a>"#), "{html}");

        assert_eq!(blocks[5].markdown(), "- One\n- Two\n  - Nested");
        assert_eq!(blocks[6].markdown(), "First\n\nSecond");
        assert_eq!(blocks[7].markdown(), "fn main() {}");
        assert_eq!(markdown_to_html("## Better *title*", BlockKind::Heading(2)), "<h2>Better <em>title</em></h2>");
        assert_eq!(markdown_to_html("a < b", BlockKind::Code), "<pre><code>a &lt; b</code></pre>");
        assert!(markdown_to_html("Quoted", BlockKind::Quote).starts_with("<blockquote><p>Quoted</p>"));
        // Special characters in plain text survive as text.
        let tricky = split_blocks("<p>Use *stars* and [brackets] #1</p>");
        assert_eq!(markdown_to_html(&tricky[0].markdown(), BlockKind::Paragraph).trim(), "<p>Use *stars* and [brackets] #1</p>");
    }

    #[test]
    fn drafts_drop_move_edit_and_restore() {
        let meta = Meta { title: "T".into(), byline: Some("Ada".into()), published: None };
        let mut draft = Draft::from_article(HTML, meta, Format::default());
        assert!(!draft.is_modified());

        draft.blocks[0].included = false;
        draft.blocks[1].edited = Some("Rewritten **text**.".into());
        let moved = draft.move_block(2, true);
        assert_eq!(moved, 1);
        draft.meta.title = "New title".into();
        assert!(draft.is_modified());

        let html = draft.html();
        assert!(!html.contains("Why paper"), "dropped block is gone");
        assert!(html.contains("<strong>text</strong>"), "edit applied");
        assert!(html.find("Loose").unwrap() < html.find("Rewritten").unwrap(), "moved block comes first");
        assert_eq!(draft.included_count(), draft.blocks.len() - 1);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("document.json");
        draft.save(&path).unwrap();
        assert_eq!(Draft::load(&path).unwrap(), draft);

        draft.restore_original(HTML);
        assert!(!draft.is_modified());
        assert_eq!(draft.meta.title, "T");
        assert_eq!(draft.move_block(0, true), 0, "can't move above the top");
    }
}
