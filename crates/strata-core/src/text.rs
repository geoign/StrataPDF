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
                    stack_vertical(&mut lines, bbox);
                    blocks.push(Block::Text { bbox, lines });
                }
                TextBlockType::Image => blocks.push(Block::Image { bbox }),
                _ => {}
            }
        }
        PageText { blocks, fonts }
    }

    /// Separator between two consecutive lines of a selection: nothing between
    /// wrapped CJK lines, a space between wrapped Latin lines, a newline between blocks.
    fn line_separator(prev: &TextLine, next: &TextLine, same_block: bool) -> &'static str {
        if !same_block {
            return "\n";
        }
        let (Some(a), Some(b)) = (prev.chars.iter().rev().find(|c| !c.c.is_whitespace()), next.chars.iter().find(|c| !c.c.is_whitespace())) else {
            return "\n";
        };
        if is_cjk(a.c) || is_cjk(b.c) || prev.vertical {
            ""
        } else if a.c == '-' {
            ""
        } else {
            " "
        }
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
        let mut prev: Option<(u32, &TextLine)> = None;
        for (bi, li, l) in self.lines() {
            let start = CharPos { block: bi, line: li, ch: 0 };
            let end = CharPos { block: bi, line: li, ch: l.chars.len() as u32 };
            if end <= a || start >= b {
                continue;
            }
            if let Some((pb, pl)) = prev {
                out.push_str(Self::line_separator(pl, l, pb == bi));
            }
            prev = Some((bi, l));
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
            if l.vertical {
                // Columns run top to bottom: span from the first glyph's top edge
                // to the last glyph's bottom edge.
                let (fb, lb) = (first.bbox(), last.bbox());
                let (x0, x1) = (fb.x0.min(lb.x0), fb.x1.max(lb.x1));
                out.push(QuadF { ul: [x0, fb.y0], ur: [x1, fb.y0], ll: [x0, lb.y1], lr: [x1, lb.y1] });
            } else {
                out.push(QuadF { ul: first.ul, ll: first.ll, ur: last.ur, lr: last.lr });
            }
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

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3000..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

/// Vertical text written glyph by glyph (Chromium, Word, some OCR layers)
/// arrives as a stack of one-character lines; rebuild the column as one
/// vertical line so selection and copy work per column instead of per glyph.
fn stack_vertical(lines: &mut Vec<TextLine>, bbox: RectF) {
    if lines.len() < 2 || lines.iter().any(|l| l.vertical) {
        return;
    }
    let count = |l: &TextLine| l.chars.iter().filter(|c| !c.c.is_whitespace()).count();
    let single = lines.iter().filter(|l| count(l) <= 1).count();
    let size = lines.iter().flat_map(|l| l.chars.iter().map(|c| c.size)).fold(0.0f32, f32::max).max(1.0);
    let tall = bbox.height() > bbox.width() * 2.0 && bbox.width() < size * 1.8;
    if !(tall && single * 10 >= lines.len() * 8) {
        return;
    }
    let mut chars: Vec<TextChar> = lines.drain(..).flat_map(|l| l.chars).filter(|c| !c.c.is_whitespace()).collect();
    if chars.is_empty() {
        return;
    }
    chars.sort_by(|a, b| a.quad.bbox().y0.total_cmp(&b.quad.bbox().y0));
    let lb = chars.iter().skip(1).fold(chars[0].quad.bbox(), |r, c| r.union(&c.quad.bbox()));
    lines.push(TextLine { bbox: lb, vertical: true, chars });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::QuadF;

    fn ch(c: char, x: f32, y: f32) -> TextChar {
        TextChar { c, quad: QuadF { ul: [x, y], ur: [x + 10.0, y], ll: [x, y + 10.0], lr: [x + 10.0, y + 10.0] }, origin: [x, y + 10.0], size: 10.0, font: 0, argb: 0 }
    }

    #[test]
    fn stacked_glyphs_become_one_column_and_copy_without_breaks() {
        let col = |x: f32, s: &str| -> Vec<TextLine> {
            s.chars().enumerate().map(|(i, c)| TextLine { bbox: ch(c, x, i as f32 * 10.0).quad.bbox(), vertical: false, chars: vec![ch(c, x, i as f32 * 10.0)] }).collect()
        };
        let mut l1 = col(100.0, "吾輩は猫");
        stack_vertical(&mut l1, RectF { x0: 100.0, y0: 0.0, x1: 110.0, y1: 40.0 });
        assert_eq!(l1.len(), 1);
        assert!(l1[0].vertical);
        let mut l2 = col(80.0, "である");
        stack_vertical(&mut l2, RectF { x0: 80.0, y0: 0.0, x1: 90.0, y1: 30.0 });
        let t = PageText { blocks: vec![Block::Text { bbox: RectF::default(), lines: vec![l1.remove(0), l2.remove(0)] }], fonts: vec![] };
        assert_eq!(t.plain_text(), "吾輩は猫である");
    }
}
