//! Structured text with the extra information reflow needs: font styles,
//! segmentation regions (columns), table grids and vector/image blocks.
//!
//! MuPDF performs segmentation, paragraph breaking and table hunting when the
//! corresponding `TextPageFlags` are set; the results are nested STRUCT and GRID
//! blocks that the safe iterators of the `mupdf` crate do not expose, so this
//! module walks the C structures directly (read-only).

use std::collections::HashMap;
use std::ffi::CStr;

use mupdf::{Page, TextPage, TextPageFlags};
use mupdf_sys::*;

use crate::geom::RectF;
use crate::text::FontInfo;

/// Flags used for reflow extraction.
pub fn reflow_flags() -> TextPageFlags {
    TextPageFlags::SEGMENT | TextPageFlags::PARAGRAPH_BREAK | TextPageFlags::TABLE_HUNT | TextPageFlags::PRESERVE_IMAGES | TextPageFlags::DEHYPHENATE
}

/// Flags of the second extraction that identifies the glyphs MuPDF found no character for
/// (see [`RichPage::from_page`]): unknown characters carry their glyph index, and ligatures and
/// white space are left alone so that the index stays what it was.
fn glyph_flags() -> TextPageFlags {
    TextPageFlags::USE_GID_FOR_UNKNOWN_UNICODE | TextPageFlags::PRESERVE_LIGATURES | TextPageFlags::PRESERVE_WHITESPACE
}

#[derive(Clone, Copy, Debug)]
pub struct RichChar {
    pub c: char,
    pub bbox: RectF,
    pub size: f32,
    pub font: u16,
    pub bold: bool,
    pub argb: u32,
}

#[derive(Clone, Debug)]
pub struct RichLine {
    pub bbox: RectF,
    pub vertical: bool,
    /// Baseline direction (1,0) for normal horizontal text.
    pub dir: [f32; 2],
    /// Line was joined to the previous one by dehyphenation.
    pub joined: bool,
    pub chars: Vec<RichChar>,
}

impl RichLine {
    pub fn text(&self) -> String {
        self.chars.iter().map(|c| c.c).collect()
    }
}

#[derive(Clone, Debug)]
pub enum RichBlock {
    Text { bbox: RectF, lines: Vec<RichLine> },
    Image { bbox: RectF },
    Vector { bbox: RectF },
    /// Table grid found by table hunting: column and row boundaries.
    Grid { bbox: RectF, xs: Vec<f32>, ys: Vec<f32> },
    /// Start/end of a structure or segmentation region.
    RegionStart { kind: String },
    RegionEnd,
}

#[derive(Clone, Debug, Default)]
pub struct RichPage {
    pub width: f32,
    pub height: f32,
    pub blocks: Vec<RichBlock>,
    pub fonts: Vec<FontInfo>,
}

struct FontTable {
    fonts: Vec<FontInfo>,
    ids: HashMap<usize, u16>,
}

impl FontTable {
    unsafe fn id(&mut self, ctx: *mut fz_context, f: *mut fz_font) -> u16 {
        if f.is_null() {
            return u16::MAX;
        }
        if let Some(&i) = self.ids.get(&(f as usize)) {
            return i;
        }
        let info = unsafe {
            let n = fz_font_name(ctx, f);
            FontInfo {
                name: if n.is_null() { String::new() } else { CStr::from_ptr(n).to_string_lossy().into_owned() },
                bold: fz_font_is_bold(ctx, f) != 0,
                italic: fz_font_is_italic(ctx, f) != 0,
                monospaced: fz_font_is_monospaced(ctx, f) != 0,
                serif: fz_font_is_serif(ctx, f) != 0,
            }
        };
        self.fonts.push(info);
        let i = (self.fonts.len() - 1) as u16;
        self.ids.insert(f as usize, i);
        i
    }
}

/// The name of a font as MuPDF gives it (with the subset prefix).
unsafe fn font_name(ctx: *mut fz_context, f: *mut fz_font) -> String {
    let n = unsafe { fz_font_name(ctx, f) };
    if n.is_null() { String::new() } else { unsafe { CStr::from_ptr(n) }.to_string_lossy().into_owned() }
}

fn rect(r: fz_rect) -> RectF {
    RectF { x0: r.x0, y0: r.y0, x1: r.x1, y1: r.y1 }
}

fn quad_bbox(q: &fz_quad) -> RectF {
    let xs = [q.ul.x, q.ur.x, q.ll.x, q.lr.x];
    let ys = [q.ul.y, q.ur.y, q.ll.y, q.lr.y];
    RectF {
        x0: xs.iter().copied().fold(f32::INFINITY, f32::min),
        y0: ys.iter().copied().fold(f32::INFINITY, f32::min),
        x1: xs.iter().copied().fold(f32::NEG_INFINITY, f32::max),
        y1: ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
    }
}

unsafe fn positions(p: *mut fz_stext_grid_positions) -> Vec<f32> {
    if p.is_null() {
        return Vec::new();
    }
    unsafe {
        let len = (*p).len.max(0) as usize;
        (*p).list.as_slice(len).iter().map(|e| e.pos).collect()
    }
}

unsafe fn walk(ctx: *mut fz_context, mut b: *mut fz_stext_block, out: &mut Vec<RichBlock>, fonts: &mut FontTable, depth: u32) {
    while !b.is_null() {
        unsafe {
            let blk = &*b;
            match blk.type_ {
                FZ_STEXT_BLOCK_TEXT => {
                    let mut lines = Vec::new();
                    let mut l = blk.u.t.first_line;
                    while !l.is_null() {
                        let line = &*l;
                        let mut chars = Vec::new();
                        let mut c = line.first_char;
                        while !c.is_null() {
                            let ch = &*c;
                            if let Some(cc) = char::from_u32(ch.c as u32) {
                                chars.push(RichChar {
                                    c: cc,
                                    bbox: quad_bbox(&ch.quad),
                                    size: ch.size,
                                    font: fonts.id(ctx, ch.font),
                                    bold: (ch.flags as i32 & FZ_STEXT_BOLD) != 0,
                                    argb: ch.argb,
                                });
                            }
                            c = ch.next;
                        }
                        if !chars.is_empty() {
                            lines.push(RichLine {
                                bbox: rect(line.bbox),
                                vertical: line.wmode == 1,
                                dir: [line.dir.x, line.dir.y],
                                joined: (line.flags as i32 & FZ_STEXT_LINE_FLAGS_JOINED) != 0,
                                chars,
                            });
                        }
                        l = line.next;
                    }
                    if !lines.is_empty() {
                        out.push(RichBlock::Text { bbox: rect(blk.bbox), lines });
                    }
                }
                FZ_STEXT_BLOCK_IMAGE => out.push(RichBlock::Image { bbox: rect(blk.bbox) }),
                FZ_STEXT_BLOCK_VECTOR => out.push(RichBlock::Vector { bbox: rect(blk.bbox) }),
                FZ_STEXT_BLOCK_GRID => out.push(RichBlock::Grid { bbox: rect(blk.bbox), xs: positions(blk.u.b.xs), ys: positions(blk.u.b.ys) }),
                FZ_STEXT_BLOCK_STRUCT => {
                    let s = blk.u.s.down;
                    if !s.is_null() && depth < 64 {
                        let raw = CStr::from_ptr((*s).raw.as_ptr()).to_string_lossy().into_owned();
                        out.push(RichBlock::RegionStart { kind: raw });
                        walk(ctx, (*s).first_block, out, fonts, depth + 1);
                        out.push(RichBlock::RegionEnd);
                    }
                }
                _ => {}
            }
            b = blk.next;
        }
    }
}

/// MuPDF adds a space where the pen jumps by more than `SPACE_DIST` and less than `SPACE_MAX_DIST` em.
const SPACE_DIST: f32 = 0.15;
const SPACE_MAX_DIST: f32 = 0.8;

/// Glyphs of a `USE_GID_FOR_UNKNOWN_UNICODE` extraction by position (left, top of the
/// character box): the font and the glyph index.
type GlyphIds = HashMap<(u32, u32), Vec<(*mut fz_font, i32)>>;

/// The characters of the page that carry a glyph index in place of a character.
///
/// MuPDF sets the flag `UNICODE_IS_GID` on every character that follows an unknown one in the same
/// text run, so the flag alone does not tell an unknown glyph from a known one: the caller looks up
/// only the positions where the reflow extraction has U+FFFD.
unsafe fn glyph_ids(mut b: *mut fz_stext_block, out: &mut GlyphIds, depth: u32) {
    while !b.is_null() {
        unsafe {
            let blk = &*b;
            match blk.type_ {
                FZ_STEXT_BLOCK_TEXT => {
                    let mut l = blk.u.t.first_line;
                    while !l.is_null() {
                        let mut c = (*l).first_char;
                        while !c.is_null() {
                            let ch = &*c;
                            if ch.flags as i32 & FZ_STEXT_UNICODE_IS_GID != 0 {
                                let r = quad_bbox(&ch.quad);
                                out.entry((r.x0.to_bits(), r.y0.to_bits())).or_default().push((ch.font, ch.c));
                            }
                            c = ch.next;
                        }
                        l = (*l).next;
                    }
                }
                FZ_STEXT_BLOCK_STRUCT => {
                    let s = blk.u.s.down;
                    if !s.is_null() && depth < 64 {
                        glyph_ids((*s).first_block, out, depth + 1);
                    }
                }
                _ => {}
            }
            b = blk.next;
        }
    }
}

/// The name of glyph `gid` of `font` (MuPDF answers with the number for a font without names).
unsafe fn glyph_name(ctx: *mut fz_context, font: *mut fz_font, gid: i32) -> Option<String> {
    if font.is_null() || gid < 0 || gid >= unsafe { (*font).glyph_count } {
        return None;
    }
    let mut buf = [0 as std::ffi::c_char; 128];
    unsafe { fz_get_glyph_name(ctx, font, gid, buf.as_mut_ptr(), buf.len() as i32) };
    Some(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

/// A glyph that stands for several characters ("fi"): the characters share its box.
fn split_char(c: RichChar, text: &str) -> impl Iterator<Item = RichChar> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(1) as f32;
    let horizontal = c.bbox.width() >= c.bbox.height();
    chars.into_iter().enumerate().map(move |(i, ch)| {
        let mut b = c.bbox;
        if horizontal {
            b.x0 = c.bbox.x0 + c.bbox.width() / n * i as f32;
            b.x1 = b.x0 + c.bbox.width() / n;
        } else {
            b.y0 = c.bbox.y0 + c.bbox.height() / n * i as f32;
            b.y1 = b.y0 + c.bbox.height() / n;
        }
        RichChar { c: ch, bbox: b, ..c }
    })
}

impl RichPage {
    /// Like [`from_text_page`](Self::from_text_page), with the text of glyphs that the PDF gives
    /// no Unicode value for (symbol and pi fonts): MuPDF has U+FFFD there, but the name of the
    /// glyph often says what it is ([`crate::glyphs`]). `tp` is `page` extracted with [`reflow_flags`].
    ///
    /// A page that has such glyphs costs a second, light extraction in which MuPDF returns the glyph
    /// index of unknown characters. The two are matched by position, so layout and reading order stay
    /// those of `tp` (extracting the page once with the index in place of the character would change
    /// where MuPDF adds spaces and joins lines, and marks the characters after an unknown one as
    /// unknown too).
    pub fn from_page(page: &Page, tp: &TextPage, width: f32, height: f32) -> RichPage {
        let mut rich = Self::from_text_page(tp, width, height);
        rich.name_glyphs(page);
        rich
    }

    /// Replace U+FFFD by the text of the glyph where its name says what it is.
    fn name_glyphs(&mut self, page: &Page) {
        let RichPage { blocks, fonts, .. } = self;
        let unknown = |c: &RichChar| c.c == '\u{FFFD}';
        let has_unknown = blocks.iter().any(|b| matches!(b, RichBlock::Text { lines, .. } if lines.iter().any(|l| l.chars.iter().any(unknown))));
        if !has_unknown {
            return;
        }
        let Ok(tp) = page.to_text_page(glyph_flags()) else { return };
        let ctx = mupdf::context::raw_context();
        let mut ids = GlyphIds::new();
        // SAFETY: the stext page is alive for the duration of the walk and only read.
        unsafe { glyph_ids((*tp.as_raw()).first_block, &mut ids, 0) };
        let mut names: HashMap<usize, String> = HashMap::new();
        let mut texts: HashMap<(usize, i32), Option<String>> = HashMap::new();
        let mut recover = |c: &RichChar| -> Option<String> {
            let mine = &fonts.get(c.font as usize)?.name;
            let &(f, gid) = ids.get(&(c.bbox.x0.to_bits(), c.bbox.y0.to_bits()))?.iter().find(|&&(f, _)| {
                // SAFETY: `f` is a font of `tp`, which is alive.
                names.entry(f as usize).or_insert_with(|| unsafe { font_name(ctx, f) }).as_str() == mine.as_str()
            })?;
            texts
                .entry((f as usize, gid))
                .or_insert_with(|| {
                    // SAFETY: as above.
                    let name = unsafe { glyph_name(ctx, f, gid) }?;
                    crate::glyphs::glyph_text(mine, &name)
                })
                .clone()
        };
        for b in blocks {
            let RichBlock::Text { lines, .. } = b else { continue };
            for line in lines.iter_mut().filter(|l| l.chars.iter().any(unknown)) {
                let horizontal = !line.vertical && line.dir[1].abs() < 0.1;
                let mut out: Vec<RichChar> = Vec::with_capacity(line.chars.len());
                let mut after_named = false;
                for c in &line.chars {
                    if after_named
                        && horizontal
                        && let Some(p) = out.last()
                    {
                        // MuPDF adds no space after a character it does not know; after one we
                        // named, add the space it would have added.
                        let gap = (c.bbox.x0 - p.bbox.x1) / c.size.max(0.1);
                        let may_add = p.c < '\u{700}' || ('\u{2000}'..='\u{20CF}').contains(&p.c);
                        if c.c != ' ' && p.c != ' ' && may_add && gap > SPACE_DIST && gap < SPACE_MAX_DIST {
                            out.push(RichChar { c: ' ', bbox: RectF { x0: p.bbox.x1, x1: c.bbox.x0, ..c.bbox }, ..*c });
                        }
                    }
                    match unknown(c).then(|| recover(c)).flatten() {
                        Some(t) => {
                            out.extend(split_char(*c, &t));
                            after_named = true;
                        }
                        None => {
                            out.push(*c);
                            after_named = false;
                        }
                    }
                }
                line.chars = out;
            }
        }
    }

    /// Must be called on the thread that created `tp` (MuPDF contexts are per thread).
    pub fn from_text_page(tp: &TextPage, width: f32, height: f32) -> RichPage {
        let ctx = mupdf::context::raw_context();
        let mut fonts = FontTable { fonts: Vec::new(), ids: HashMap::new() };
        let mut blocks = Vec::new();
        // SAFETY: the stext page is alive for the duration of the walk and only read.
        unsafe {
            let p = tp.as_raw();
            walk(ctx, (*p).first_block, &mut blocks, &mut fonts, 0);
        }
        RichPage { width, height, blocks, fonts: fonts.fonts }
    }
}
