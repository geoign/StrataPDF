//! Owned copy of MuPDF structured text, so it can cross threads and outlive the page.

use std::collections::HashMap;

use mupdf::text_page::TextBlockType;
use mupdf::{TextPage, WriteMode};

use crate::geom::{QuadF, RectF};

#[derive(Clone, Debug, Default)]
pub struct FontInfo {
    pub name: String,
    pub bold: bool,
    pub italic: bool,
    pub monospaced: bool,
    pub serif: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct TextChar {
    pub c: char,
    pub quad: QuadF,
    pub origin: [f32; 2],
    pub size: f32,
    /// Index into [`PageText::fonts`].
    pub font: u16,
    pub argb: u32,
}

#[derive(Clone, Debug)]
pub struct TextLine {
    pub bbox: RectF,
    pub vertical: bool,
    pub chars: Vec<TextChar>,
}

#[derive(Clone, Debug)]
pub enum Block {
    Text { bbox: RectF, lines: Vec<TextLine> },
    Image { bbox: RectF },
}

impl Block {
    pub fn bbox(&self) -> RectF {
        match self {
            Block::Text { bbox, .. } | Block::Image { bbox } => *bbox,
        }
    }
}

/// Position of a character in reading order (block, line, char).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CharPos {
    pub block: u32,
    pub line: u32,
    pub ch: u32,
}

#[derive(Clone, Debug, Default)]
pub struct PageText {
    pub blocks: Vec<Block>,
    pub fonts: Vec<FontInfo>,
}

impl PageText {
    pub fn from_text_page(tp: &TextPage) -> PageText {
        let mut fonts: Vec<FontInfo> = Vec::new();
        let mut font_ids: HashMap<String, u16> = HashMap::new();
        let mut blocks = Vec::new();
        for b in tp.blocks() {
            let bbox = RectF::from(b.bounds());
            match b.r#type() {
                TextBlockType::Text => {
                    let mut lines = Vec::new();
                    for l in b.lines() {
                        let vertical = matches!(l.wmode(), WriteMode::Vertical);
                        let mut chars = Vec::new();
                        for c in l.chars() {
                            let Some(ch) = c.char() else { continue };
                            let font = match c.font() {
                                Some(f) => {
                                    let name = f.name().to_string();
                                    *font_ids.entry(name.clone()).or_insert_with(|| {
                                        fonts.push(FontInfo {
                                            name,
                                            bold: f.is_bold(),
                                            italic: f.is_italic(),
                                            monospaced: f.is_monospaced(),
                                            serif: f.is_serif(),
                                        });
                                        (fonts.len() - 1) as u16
                                    })
                                }
                                None => u16::MAX,
                            };
                            let o = c.origin();
                            chars.push(TextChar {
                                c: ch,
                                quad: c.quad().into(),
                                origin: [o.x, o.y],
                                size: c.size(),
                                font,
                                argb: c.argb(),
                            });
                        }
                        if !chars.is_empty() {
                            lines.push(TextLine { bbox: l.bounds().into(), vertical, chars });
                        }
                    }
                    blocks.push(Block::Text { bbox, lines });
                }
                TextBlockType::Image => blocks.push(Block::Image { bbox }),
                _ => {}
            }
        }
        PageText { blocks, fonts }
    }

    pub fn font(&self, id: u16) -> Option<&FontInfo> {
        self.fonts.get(id as usize)
    }

    pub fn lines(&self) -> impl Iterator<Item = (u32, u32, &TextLine)> {
        self.blocks.iter().enumerate().flat_map(|(bi, b)| match b {
            Block::Text { lines, .. } => lines.iter().enumerate().map(move |(li, l)| (bi as u32, li as u32, l)).collect::<Vec<_>>(),
            Block::Image { .. } => Vec::new(),
        })
    }

    fn line(&self, block: u32, line: u32) -> Option<&TextLine> {
        match self.blocks.get(block as usize)? {
            Block::Text { lines, .. } => lines.get(line as usize),
            Block::Image { .. } => None,
        }
    }

    /// Nearest character to a page-space point; `None` for pages without text.
    /// Inside a char, the position snaps to whichever half was hit, so a drag
    /// that starts on the right half of a glyph does not select it.
    pub fn hit(&self, x: f32, y: f32) -> Option<CharPos> {
        let mut best: Option<(f32, u32, u32, &TextLine)> = None;
        for (bi, li, l) in self.lines() {
            let d = l.bbox.dist2(x, y);
            if best.map_or(true, |b| d < b.0) {
                best = Some((d, bi, li, l));
            }
        }
        let (_, block, line, l) = best?;
        let mut best_c = (f32::INFINITY, 0u32);
        for (ci, c) in l.chars.iter().enumerate() {
            let d = c.quad.bbox().dist2(x, y);
            if d < best_c.0 {
                best_c = (d, ci as u32);
            }
        }
        let c = &l.chars[best_c.1 as usize];
        let bb = c.quad.bbox();
        let after = if l.vertical { y > (bb.y0 + bb.y1) * 0.5 } else { x > (bb.x0 + bb.x1) * 0.5 };
        Some(CharPos { block, line, ch: best_c.1 + after as u32 })
    }

    /// Characters in `[a, b)` in reading order, with a newline between lines.
    pub fn text_between(&self, a: CharPos, b: CharPos) -> String {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut out = String::new();
        let mut first = true;
        for (bi, li, l) in self.lines() {
            let start = CharPos { block: bi, line: li, ch: 0 };
            let end = CharPos { block: bi, line: li, ch: l.chars.len() as u32 };
            if end <= a || start >= b {
                continue;
            }
            if !first {
                out.push('\n');
            }
            first = false;
            let s = if bi == a.block && li == a.line { a.ch as usize } else { 0 };
            let e = if bi == b.block && li == b.line { b.ch as usize } else { l.chars.len() };
            out.extend(l.chars[s.min(l.chars.len())..e.min(l.chars.len())].iter().map(|c| c.c));
        }
        out
    }

    /// Per-line highlight quads for the selection `[a, b)`.
    pub fn selection_quads(&self, a: CharPos, b: CharPos) -> Vec<QuadF> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let mut out = Vec::new();
        for (bi, li, l) in self.lines() {
            let start = CharPos { block: bi, line: li, ch: 0 };
            let end = CharPos { block: bi, line: li, ch: l.chars.len() as u32 };
            if end <= a || start >= b {
                continue;
            }
            let s = if bi == a.block && li == a.line { a.ch as usize } else { 0 };
            let e = if bi == b.block && li == b.line { b.ch as usize } else { l.chars.len() };
            let e = e.min(l.chars.len());
            if s >= e {
                continue;
            }
            let first = l.chars[s].quad;
            let last = l.chars[e - 1].quad;
            out.push(QuadF { ul: first.ul, ll: first.ll, ur: last.ur, lr: last.lr });
        }
        out
    }

    /// All text of the page in reading order.
    pub fn plain_text(&self) -> String {
        let end = CharPos { block: u32::MAX, line: 0, ch: 0 };
        self.text_between(CharPos { block: 0, line: 0, ch: 0 }, end)
    }

    pub fn char_at(&self, p: CharPos) -> Option<&TextChar> {
        self.line(p.block, p.line)?.chars.get(p.ch as usize)
    }
}
