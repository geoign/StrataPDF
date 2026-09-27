//! Structured text with the extra information reflow needs: font styles,
//! segmentation regions (columns), table grids and vector/image blocks.
//!
//! MuPDF performs segmentation, paragraph breaking and table hunting when the
//! corresponding `TextPageFlags` are set; the results are nested STRUCT and GRID
//! blocks that the safe iterators of the `mupdf` crate do not expose, so this
//! module walks the C structures directly (read-only).

use std::collections::HashMap;
use std::ffi::CStr;

use mupdf::{TextPage, TextPageFlags};
use mupdf_sys::*;

use crate::geom::RectF;
use crate::text::FontInfo;

/// Flags used for reflow extraction.
pub fn reflow_flags() -> TextPageFlags {
    TextPageFlags::SEGMENT | TextPageFlags::PARAGRAPH_BREAK | TextPageFlags::TABLE_HUNT | TextPageFlags::PRESERVE_IMAGES | TextPageFlags::DEHYPHENATE
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

impl RichPage {
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
