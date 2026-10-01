//! Markdown and plain-text files in the text view.
//!
//! The file is decoded (UTF-8, UTF-16 with BOM, or Shift_JIS), shown as plain
//! text in the page view (MuPDF lays it out) and turned into the reflow model
//! directly for the text view: headings and paragraphs become ordinary nodes, so
//! fonts, translation and export work on them; lists, code, tables, quotes and
//! paragraphs with images become HTML blocks.

use std::ops::Range;
use std::path::Path;

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd};

use super::output::{latex_to_mathml, latex_to_mathml_inline};
use super::{Node, ReflowDoc, ReflowImage, Span, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkupKind {
    Markdown,
    Text,
}

/// Markdown or plain text, by file extension.
pub fn kind_of(path: &Path) -> Option<MarkupKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "mdown" | "mkd" | "mkdn" => Some(MarkupKind::Markdown),
        "txt" | "text" => Some(MarkupKind::Text),
        _ => None,
    }
}

/// Text of a file: UTF-8 or UTF-16 by BOM, UTF-8, else Shift_JIS (Windows-31J).
pub fn decode(bytes: &[u8]) -> String {
    let s = if let Some((enc, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        enc.decode_without_bom_handling(&bytes[bom..]).0.into_owned()
    } else if let Ok(s) = std::str::from_utf8(bytes) {
        s.to_owned()
    } else {
        encoding_rs::SHIFT_JIS.decode(bytes).0.into_owned()
    };
    s.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn read(path: &Path) -> std::io::Result<String> {
    Ok(decode(&std::fs::read(path)?))
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3000..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

/// The reflow model of a Markdown or text file. `pages`: page count of the page
/// view, to place each block approximately for switching views.
pub fn build(path: &Path, text: &str, kind: MarkupKind, pages: usize) -> ReflowDoc {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut b = Builder { doc: ReflowDoc::default(), len: text.len().max(1), pages: pages.max(1) as u32 };
    match kind {
        MarkupKind::Markdown => b.markdown(text, path.parent().unwrap_or(Path::new("."))),
        MarkupKind::Text => b.text(text),
    }
    let mut doc = b.doc;
    doc.title = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            Node::Heading { spans, .. } => Some(spans.iter().map(|s| s.text.as_str()).collect::<String>()),
            _ => None,
        })
        .unwrap_or(stem);
    doc
}

struct Builder {
    doc: ReflowDoc,
    len: usize,
    pages: u32,
}

impl Builder {
    fn push(&mut self, n: Node, at: usize) {
        // The page view shows the same text: place the block by its offset.
        let page = ((at as f64 / self.len as f64) * self.pages as f64) as u32;
        self.doc.nodes.push(n);
        self.doc.anchors.push((page.min(self.pages - 1), 0.0));
    }

    /// Blank lines separate paragraphs. Japanese text keeps one paragraph per line
    /// (novels and notes are written that way); other text joins wrapped lines.
    fn text(&mut self, text: &str) {
        let cjk = text.chars().filter(|&c| is_cjk(c)).count();
        let per_line = cjk * 5 > text.chars().filter(|c| !c.is_whitespace()).count();
        let mut at = 0;
        for block in text.split("\n\n") {
            let start = at;
            at += block.len() + 2;
            let mut off = start;
            if per_line {
                for line in block.split('\n') {
                    let line_at = off;
                    off += line.len() + 1;
                    if !line.trim().is_empty() {
                        // The style sheet indents Japanese paragraphs itself.
                        self.push(Node::Paragraph { spans: vec![plain_span(line.trim_end().trim_start_matches('\u{3000}'))] }, line_at);
                    }
                }
            } else {
                let joined = block.split('\n').map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
                if !joined.is_empty() {
                    self.push(Node::Paragraph { spans: vec![plain_span(&joined)] }, start);
                }
            }
        }
    }

    fn markdown(&mut self, text: &str, dir: &Path) {
        let opts = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_MATH
            | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
        let events: Vec<(Event, Range<usize>)> = Parser::new_ext(text, opts).into_offset_iter().collect();
        let mut i = 0;
        while i < events.len() {
            let (ev, range) = &events[i];
            match ev {
                Event::Start(tag) => {
                    let end = matching_end(&events, i);
                    let inner = &events[i + 1..end];
                    let source = text[range.clone()].to_string();
                    match tag {
                        Tag::MetadataBlock(_) => {}
                        Tag::Heading { level, .. } => self.push(Node::Heading { level: *level as u8, spans: inline(inner) }, range.start),
                        Tag::Paragraph if !inner.iter().any(|(e, _)| matches!(e, Event::Start(Tag::Image { .. }) | Event::DisplayMath(_))) => {
                            self.push(Node::Paragraph { spans: inline(inner) }, range.start)
                        }
                        _ => {
                            let html = self.html(&events[i..=end], dir);
                            self.push(Node::Html { html, md: source }, range.start);
                        }
                    }
                    i = end + 1;
                }
                Event::Rule => {
                    self.push(Node::Html { html: "<hr>".into(), md: "---".into() }, range.start);
                    i += 1;
                }
                Event::Html(h) | Event::InlineHtml(h) => {
                    self.push(Node::Html { html: h.to_string(), md: h.to_string() }, range.start);
                    i += 1;
                }
                _ => i += 1,
            }
        }
    }

    /// HTML of a block. Local images become document images (`strata-img:N`,
    /// resolved when the page is written); formulas become MathML.
    fn html(&mut self, events: &[(Event, Range<usize>)], dir: &Path) -> String {
        let mut out = Vec::with_capacity(events.len());
        for (e, _) in events {
            out.push(match e {
                Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                    let dest = match self.image(dir, dest_url) {
                        Some(n) => CowStr::from(format!("strata-img:{n}")),
                        None => dest_url.clone(),
                    };
                    Event::Start(Tag::Image { link_type: *link_type, dest_url: dest, title: title.clone(), id: id.clone() })
                }
                Event::InlineMath(t) => Event::InlineHtml(math_html(t, false).into()),
                Event::DisplayMath(t) => Event::InlineHtml(math_html(t, true).into()),
                e => e.clone(),
            });
        }
        let mut s = String::new();
        pulldown_cmark::html::push_html(&mut s, out.into_iter());
        s
    }

    /// Load a local image next to the file; `None` for URLs and unreadable files.
    fn image(&mut self, dir: &Path, url: &str) -> Option<usize> {
        if url.contains("://") || url.starts_with("data:") || url.starts_with('#') {
            return None;
        }
        let rel = url.split(['#', '?']).next().unwrap_or(url).replace("%20", " ");
        let path = dir.join(rel.trim_start_matches("./"));
        let data = std::fs::read(&path).ok()?;
        let img = image::load_from_memory(&data).ok()?;
        let png = if data.starts_with(b"\x89PNG") {
            data
        } else {
            let mut v = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut v), image::ImageFormat::Png).ok()?;
            v
        };
        let n = self.doc.images.len();
        self.doc.images.push(ReflowImage { id: format!("md{n}.png"), page: 0, bbox: Default::default(), width: img.width(), height: img.height(), png });
        Some(n)
    }
}

fn math_html(latex: &str, display: bool) -> String {
    let m = if display { latex_to_mathml(latex) } else { latex_to_mathml_inline(latex) };
    match m {
        Some(m) if display => format!("<div class=\"formula\">{m}</div>"),
        Some(m) => m,
        None => format!("<code>{}</code>", latex.replace('&', "&amp;").replace('<', "&lt;")),
    }
}

/// Index of the `End` that closes the `Start` at `i`.
fn matching_end(events: &[(Event, Range<usize>)], i: usize) -> usize {
    let mut depth = 0;
    for (k, (e, _)) in events.iter().enumerate().skip(i) {
        match e {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return k;
                }
            }
            _ => {}
        }
    }
    events.len() - 1
}

fn plain_span(t: &str) -> Span {
    Span { text: t.to_string(), style: Style::default(), link: None }
}

/// Spans of a heading's or paragraph's inline content.
fn inline(events: &[(Event, Range<usize>)]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let (mut bold, mut italic, mut sup, mut sub) = (0, 0, 0, 0);
    let mut links: Vec<String> = Vec::new();
    // A line break inside a paragraph: a space, except next to Japanese.
    let mut pending_break = false;
    for (e, _) in events {
        let (text, mono, math, is_sup) = match e {
            Event::Text(t) => (t.to_string(), false, false, false),
            Event::Code(t) => (t.to_string(), true, false, false),
            Event::InlineMath(t) | Event::DisplayMath(t) => (t.to_string(), false, true, false),
            Event::FootnoteReference(l) => (format!("[{l}]"), false, false, true),
            Event::SoftBreak | Event::HardBreak => {
                pending_break = true;
                continue;
            }
            Event::Start(t) => {
                match t {
                    Tag::Strong => bold += 1,
                    Tag::Emphasis => italic += 1,
                    Tag::Superscript => sup += 1,
                    Tag::Subscript => sub += 1,
                    Tag::Link { dest_url, .. } => links.push(dest_url.to_string()),
                    _ => {}
                }
                continue;
            }
            Event::End(t) => {
                match t {
                    TagEnd::Strong => bold -= 1,
                    TagEnd::Emphasis => italic -= 1,
                    TagEnd::Superscript => sup -= 1,
                    TagEnd::Subscript => sub -= 1,
                    TagEnd::Link => {
                        links.pop();
                    }
                    _ => {}
                }
                continue;
            }
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        let style = Style { bold: bold > 0, italic: italic > 0, sup: sup > 0 || is_sup, sub: sub > 0, mono, math };
        let link = links.last().cloned();
        if std::mem::take(&mut pending_break) {
            let prev = spans.last().and_then(|s| s.text.chars().last());
            let next = text.chars().next();
            if !prev.is_some_and(is_cjk) && !next.is_some_and(is_cjk) {
                push_text(&mut spans, " ", Style { math: false, ..style }, link.clone());
            }
        }
        push_text(&mut spans, &text, style, link);
    }
    spans
}

fn push_text(spans: &mut Vec<Span>, text: &str, style: Style, link: Option<String>) {
    match spans.last_mut() {
        Some(s) if s.style == style && s.link == link && !style.math => s.text.push_str(text),
        _ => spans.push(Span { text: text.to_string(), style, link }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(s: &str) -> ReflowDoc {
        build(Path::new("x.md"), s, MarkupKind::Markdown, 3)
    }

    #[test]
    fn headings_and_paragraphs_are_nodes() {
        let d = md("# Title\n\nSome **bold** and `code`,\nwrapped line.\n\n日本語の\n段落。\n");
        assert_eq!(d.title, "Title");
        assert!(matches!(&d.nodes[0], Node::Heading { level: 1, .. }));
        let Node::Paragraph { spans } = &d.nodes[1] else { panic!() };
        let t: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(t, "Some bold and code, wrapped line.");
        assert!(spans.iter().any(|s| s.style.bold && s.text == "bold"));
        assert!(spans.iter().any(|s| s.style.mono && s.text == "code"));
        let Node::Paragraph { spans } = &d.nodes[2] else { panic!() };
        assert_eq!(spans[0].text, "日本語の段落。");
    }

    #[test]
    fn blocks_become_html() {
        let d = md("- a\n- b\n\n```rust\nfn x() {}\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n> quote\n");
        let html: Vec<&str> = d.nodes.iter().filter_map(|n| if let Node::Html { html, .. } = n { Some(html.as_str()) } else { None }).collect();
        assert_eq!(html.len(), 4);
        assert!(html[0].contains("<li>a</li>"));
        assert!(html[1].contains("<code class=\"language-rust\">"));
        assert!(html[2].contains("<table>"));
        assert!(html[3].contains("<blockquote>"));
        let Node::Html { md, .. } = &d.nodes[0] else { panic!() };
        assert_eq!(md.trim(), "- a\n- b");
    }

    #[test]
    fn math_and_links() {
        let d = md("Energy $E=mc^2$ in [docs](https://example.com).\n\n$$\nx^2\n$$\n");
        let Node::Paragraph { spans } = &d.nodes[0] else { panic!() };
        assert!(spans.iter().any(|s| s.style.math && s.text == "E=mc^2"));
        assert!(spans.iter().any(|s| s.link.as_deref() == Some("https://example.com")));
        let Node::Html { html, .. } = &d.nodes[1] else { panic!() };
        assert!(html.contains("<math"));
    }

    #[test]
    fn front_matter_is_skipped_and_anchors_spread_over_pages() {
        let d = md("---\ntitle: x\n---\n\n# A\n\ntext\n\n# B\n\nmore text at the end\n");
        assert!(matches!(&d.nodes[0], Node::Heading { .. }));
        assert_eq!(d.anchors.len(), d.nodes.len());
        assert_eq!(d.anchors[0].0, 0);
        // The last paragraph starts 34 bytes into 55: page 1 of 0..3.
        assert_eq!(d.anchors.last().unwrap().0, 1);
    }

    #[test]
    fn plain_text() {
        let d = build(Path::new("note.txt"), "Wrapped English\nlines here.\n\nSecond.\n", MarkupKind::Text, 1);
        assert_eq!(d.title, "note");
        assert_eq!(d.nodes.len(), 2);
        let Node::Paragraph { spans } = &d.nodes[0] else { panic!() };
        assert_eq!(spans[0].text, "Wrapped English lines here.");
        let d = build(Path::new("j.txt"), "　吾輩は猫である。\n　名前はまだ無い。\n", MarkupKind::Text, 1);
        assert_eq!(d.nodes.len(), 2);
        let Node::Paragraph { spans } = &d.nodes[0] else { panic!() };
        assert_eq!(spans[0].text, "吾輩は猫である。");
    }

    #[test]
    fn decoding() {
        assert_eq!(decode("a\r\nb".as_bytes()), "a\nb");
        assert_eq!(decode(b"\xef\xbb\xbfx"), "x");
        // 「日本」 in Shift_JIS.
        assert_eq!(decode(b"\x93\xfa\x96\x7b"), "日本");
    }
}
