//! Page layout analysis with PyMuPDF Layout's graph network (see
//! [`strata_ocr::layout`]). This side owns the MuPDF page: it extracts the text
//! lines the way PyMuPDF's "dict" extraction does, computes their region
//! features with `features.c` (vendor/pymupdf_layout) and renders the page.

use std::ffi::CStr;

use mupdf::{Colorspace, DisplayList, Matrix, Page, Pixmap, TextPage, TextPageFlags};
use mupdf_sys::*;
use strata_ocr::layout::{LayoutModel, LayoutNode, PageLayout, RF_C};

unsafe extern "C" {
    fn strata_rf_count() -> i32;
    fn strata_region_features(ctx: *mut fz_context, page: *mut fz_stext_page, x0: f32, y0: f32, x1: f32, y1: f32, out: *mut f32) -> i32;
}

// MuPDF's FZ_MIN_INF_RECT / FZ_MAX_INF_RECT (macros, not in the bindings).
const MIN_INF: f32 = i32::MIN as f32;
const MAX_INF: f32 = 0x7fff_ff80 as f32;
const LAZY_VECTORS: u32 = 1 << 20;
const FUZZY_VECTORS: u32 = 1 << 21;

/// The structured-text options PyMuPDF Layout extracts with.
pub fn layout_flags() -> TextPageFlags {
    TextPageFlags::PRESERVE_IMAGES
        | TextPageFlags::PRESERVE_WHITESPACE
        | TextPageFlags::PRESERVE_LIGATURES
        | TextPageFlags::ACCURATE_BBOXES
        | TextPageFlags::COLLECT_VECTORS
        | TextPageFlags::COLLECT_STYLES
        | TextPageFlags::SEGMENT
        | TextPageFlags::PARAGRAPH_BREAK
        | TextPageFlags::COLLECT_STRUCTURE
        | TextPageFlags::TABLE_HUNT
        | TextPageFlags::CLIP
        | TextPageFlags::from_bits_retain(LAZY_VECTORS | FUZZY_VECTORS)
}

/// The layout model, loaded on first use and shared (`None` if it failed to load).
pub fn shared_model() -> Option<std::sync::Arc<LayoutModel>> {
    static MODEL: std::sync::OnceLock<Option<std::sync::Arc<LayoutModel>>> = std::sync::OnceLock::new();
    MODEL
        .get_or_init(|| match LayoutModel::load() {
            Ok(m) => Some(std::sync::Arc::new(m)),
            Err(e) => {
                log::warn!("layout model: {e}");
                None
            }
        })
        .clone()
}

/// Lines and layout of one page.
pub struct PageAnalysis {
    pub nodes: Vec<LayoutNode>,
    pub layout: PageLayout,
}

/// Analyse a page. Must run on the thread that owns `page` (MuPDF contexts are per thread).
pub fn analyze_page(model: &LayoutModel, page: &Page) -> Result<PageAnalysis, String> {
    let b = page.bounds().map_err(|e| e.to_string())?;
    let tp = page.to_text_page(layout_flags()).map_err(|e| e.to_string())?;
    let pix = page.to_pixmap(&Matrix::IDENTITY, &Colorspace::device_rgb(), false, true).map_err(|e| e.to_string())?;
    analyze(model, &tp, &pix, b.x1, b.y1)
}

/// Analyse a page from its display list (usable on any thread).
pub fn analyze_display_list(model: &LayoutModel, dl: &DisplayList, width: f32, height: f32) -> Result<PageAnalysis, String> {
    let tp = dl.to_text_page(layout_flags()).map_err(|e| e.to_string())?;
    let pix = dl.to_pixmap(&Matrix::IDENTITY, &Colorspace::device_rgb(), false).map_err(|e| e.to_string())?;
    analyze(model, &tp, &pix, width, height)
}

fn analyze(model: &LayoutModel, tp: &TextPage, pix: &Pixmap, width: f32, height: f32) -> Result<PageAnalysis, String> {
    let nodes = text_page_nodes(tp, width, height)?;
    if nodes.is_empty() {
        return Ok(PageAnalysis { nodes, layout: PageLayout::default() });
    }
    let (w, h, n) = (pix.width() as usize, pix.height() as usize, pix.n() as usize);
    let stride = pix.stride() as usize;
    let samples = pix.samples();
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let p = y * stride + x * n;
            rgb.extend_from_slice(&samples[p..p + 3]);
        }
    }
    let layout = model.analyze(&rgb, w, h, &nodes).map_err(|e| e.to_string())?;
    Ok(PageAnalysis { nodes, layout })
}

/// Text lines with their region features.
pub fn page_nodes(page: &Page) -> Result<Vec<LayoutNode>, String> {
    let b = page.bounds().map_err(|e| e.to_string())?;
    let tp = page.to_text_page(layout_flags()).map_err(|e| e.to_string())?;
    text_page_nodes(&tp, b.x1, b.y1)
}

/// Lines of a text page extracted with [`layout_flags`], as PyMuPDF's "dict"
/// extraction gives them, inside the page of the given size, with their region features.
fn text_page_nodes(tp: &TextPage, width: f32, height: f32) -> Result<Vec<LayoutNode>, String> {
    // SAFETY: plain query of the compiled C code.
    assert_eq!(unsafe { strata_rf_count() } as usize, RF_C, "features.c shim out of step with the model");
    let ctx = mupdf::context::raw_context();
    let mut lines = Vec::new();
    // SAFETY: the stext page is alive and only read here.
    unsafe {
        let p = tp.as_raw();
        walk_blocks(ctx, (*p).first_block, (*p).mediabox, &mut lines);
    }
    let mut nodes: Vec<LayoutNode> = Vec::new();
    for (bbox, text) in lines {
        let [x1, y1, x2, y2] = bbox;
        let inside = !text.is_empty() && 0.0 <= x2 && x1 <= x2 && x1 <= width && 0.0 <= y2 && y1 <= y2 && y1 <= height;
        if inside && !nodes.iter().any(|n| n.bbox == bbox) {
            nodes.push(LayoutNode { bbox, text, rf: Vec::new() });
        }
    }
    drop_line_numbers(&mut nodes);
    region_features(tp, &mut nodes)?;
    Ok(nodes)
}

/// Line numbers of a manuscript (a column of number-only lines at the same
/// right edge, in the margin) confuse the model, which has not seen them: it
/// groups them with the text and takes the text for list items. They are left out.
fn drop_line_numbers(nodes: &mut Vec<LayoutNode>) {
    let is_number = |n: &LayoutNode| (1..=4).contains(&n.text.len()) && n.text.chars().all(|c| c.is_ascii_digit());
    let text: Vec<[f32; 4]> = nodes.iter().filter(|n| !is_number(n)).map(|n| n.bbox).collect();
    // Nearly all other text (running heads aside) lies on one side of the column.
    let one_side = |x0: f32, x1: f32| {
        let left = text.iter().filter(|b| b[0] < x1 - 1.0).count();
        let right = text.iter().filter(|b| b[2] > x0 + 1.0).count();
        left * 10 <= text.len() || right * 10 <= text.len()
    };
    let mut columns: Vec<(f32, f32, usize)> = Vec::new();
    for n in nodes.iter().filter(|n| is_number(n)) {
        match columns.iter_mut().find(|c| (c.0 - n.bbox[2]).abs() < 3.0) {
            Some(c) => {
                c.1 = c.1.min(n.bbox[0]);
                c.2 += 1;
            }
            None => columns.push((n.bbox[2], n.bbox[0], 1)),
        }
    }
    // In the margin: left of all text, or right of it (not a column of a table).
    let edges: Vec<f32> = columns.into_iter().filter(|c| c.2 >= 8 && one_side(c.1, c.0)).map(|c| c.0).collect();
    if !edges.is_empty() {
        nodes.retain(|n| !(is_number(n) && edges.iter().any(|e| (e - n.bbox[2]).abs() < 3.0)));
    }
}

/// Region features of each node. Like the reference, a fresh feature context per
/// region (the page statistics are cheap next to the region scans). The context
/// removes the page fill from the stext page, so this runs after the lines were taken.
fn region_features(tp: &TextPage, nodes: &mut [LayoutNode]) -> Result<(), String> {
    let ctx = mupdf::context::raw_context();
    for n in nodes {
        let mut out = vec![0f32; RF_C];
        let [x0, y0, x1, y1] = n.bbox;
        // SAFETY: `out` holds RF_C floats; the page outlives the call.
        let ok = unsafe { strata_region_features(ctx, tp.as_raw(), x0, y0, x1, y1, out.as_mut_ptr()) };
        if ok == 0 {
            return Err("layout features failed".into());
        }
        n.rf = out;
    }
    Ok(())
}

fn is_infinite(r: &fz_rect) -> bool {
    r.x0 == MIN_INF && r.x1 == MAX_INF && r.y0 == MIN_INF && r.y1 == MAX_INF
}

fn is_empty(r: &fz_rect) -> bool {
    r.x0 >= r.x1 || r.y0 >= r.y1
}

fn empty_rect() -> fz_rect {
    fz_rect { x0: MAX_INF, y0: MAX_INF, x1: MIN_INF, y1: MIN_INF }
}

fn overlaps(a: &fz_rect, b: &fz_rect) -> bool {
    !(a.x0 >= b.x1 || a.y0 >= b.y1 || a.x1 <= b.x0 || a.y1 <= b.y0)
}

/// Blocks as in PyMuPDF's `JM_make_textpage_dict`, recursing into structure blocks.
unsafe fn walk_blocks(ctx: *mut fz_context, mut b: *mut fz_stext_block, tp_rect: fz_rect, out: &mut Vec<([f32; 4], String)>) {
    let inf = is_infinite(&tp_rect);
    while !b.is_null() {
        unsafe {
            let blk = &*b;
            // Structure blocks carry no box of their own; only their contents are checked.
            let skip = blk.type_ != FZ_STEXT_BLOCK_STRUCT && !inf && is_empty(&fz_intersect_rect(tp_rect, blk.bbox));
            if !skip {
                match blk.type_ {
                    FZ_STEXT_BLOCK_TEXT => {
                        let mut l = blk.u.t.first_line;
                        while !l.is_null() {
                            let line = &*l;
                            if inf || !is_empty(&fz_intersect_rect(tp_rect, line.bbox)) {
                                let (r, text) = line_spans(ctx, line, &tp_rect);
                                out.push(([r.x0, r.y0, r.x1, r.y1], text));
                            }
                            l = line.next;
                        }
                    }
                    FZ_STEXT_BLOCK_STRUCT => {
                        let s = blk.u.s.down;
                        if !s.is_null() {
                            walk_blocks(ctx, (*s).first_block, tp_rect, out);
                        }
                    }
                    _ => {}
                }
            }
            b = blk.next;
        }
    }
}

#[derive(PartialEq)]
struct SpanStyle {
    size: f32,
    flags: i32,
    char_flags: i32,
    font: String,
    argb: u32,
    bidi: u16,
}

/// Font name without a subset prefix ("ABCDEF+Times" -> "Times").
unsafe fn font_name(ctx: *mut fz_context, f: *mut fz_font) -> String {
    let n = unsafe { fz_font_name(ctx, f) };
    let name = if n.is_null() { String::new() } else { unsafe { CStr::from_ptr(n) }.to_string_lossy().into_owned() };
    match name.find('+') {
        Some(6) => name[7..].to_string(),
        _ => name,
    }
}

/// The line's box and text as PyMuPDF builds them: spans split on a change of
/// style, their texts joined with a space, the box the union of the span boxes.
unsafe fn line_spans(ctx: *mut fz_context, line: &fz_stext_line, tp_rect: &fz_rect) -> (fz_rect, String) {
    let infinite = is_infinite(tp_rect);
    let mut spans: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut style: Option<SpanStyle> = None;
    let mut span_rect = empty_rect();
    let mut line_rect = empty_rect();
    let horizontal = line.wmode == 0 && line.dir.x == 1.0 && line.dir.y == 0.0;
    let first_y = if line.first_char.is_null() { 0.0 } else { unsafe { (*line.first_char).origin.y } };
    let mut c = line.first_char;
    while !c.is_null() {
        let ch = unsafe { &*c };
        c = ch.next;
        let r = unsafe { char_bbox(ctx, line, ch) };
        if !overlaps(tp_rect, &r) && !infinite {
            continue;
        }
        let f = ch.font;
        let sup = horizontal && ch.origin.y < first_y - ch.size * 0.1;
        // SAFETY: font queries on a live font.
        let flags = unsafe {
            [sup, fz_font_is_italic(ctx, f) != 0, fz_font_is_serif(ctx, f) != 0, fz_font_is_monospaced(ctx, f) != 0, fz_font_is_bold(ctx, f) != 0]
                .iter()
                .enumerate()
                .fold(0, |a, (i, &b)| a | ((b as i32) << i))
        };
        let s = SpanStyle { size: ch.size, flags, char_flags: ch.flags as i32 & !FZ_STEXT_SYNTHETIC, font: unsafe { font_name(ctx, f) }, argb: ch.argb, bidi: ch.bidi };
        if style.as_ref() != Some(&s) {
            if style.is_some() {
                spans.push(std::mem::take(&mut cur));
                line_rect = unsafe { fz_union_rect(line_rect, span_rect) };
            }
            style = Some(s);
            span_rect = r;
        }
        span_rect = unsafe { fz_union_rect(span_rect, r) };
        cur.push(char::from_u32(ch.c as u32).unwrap_or('\u{fffd}'));
    }
    if style.is_some() && !is_empty(&span_rect) {
        spans.push(cur);
        line_rect = unsafe { fz_union_rect(line_rect, span_rect) };
    }
    (line_rect, spans.join(" ").trim().to_string())
}

/// PyMuPDF's `JM_char_bbox`: the character quad, re-computed for fonts whose
/// ascender and descender make no sense.
unsafe fn char_bbox(ctx: *mut fz_context, line: &fz_stext_line, ch: &fz_stext_char) -> fz_rect {
    let q = unsafe { char_quad(ctx, line, ch) };
    let mut r = unsafe { fz_rect_from_quad(q) };
    if line.wmode != 0 && r.y1 < r.y0 + ch.size {
        r.y0 = r.y1 - ch.size;
    }
    r
}

unsafe fn char_quad(ctx: *mut fz_context, line: &fz_stext_line, ch: &fz_stext_char) -> fz_quad {
    if line.wmode != 0 {
        return ch.quad;
    }
    let font = ch.font;
    let (mut asc, mut dsc) = unsafe { (fz_font_ascender(ctx, font), fz_font_descender(ctx, font)) };
    let fsize = ch.size;
    let mut asc_dsc = asc - dsc + f32::EPSILON;
    if asc_dsc >= 1.0 {
        return ch.quad;
    }
    if asc < 1e-3 {
        dsc = -0.1;
        asc = 0.9;
        asc_dsc = 1.0;
    }
    if asc_dsc < 1.0 {
        dsc /= asc_dsc;
        asc /= asc_dsc;
    }
    asc_dsc = asc - dsc;
    asc = asc * fsize / asc_dsc;
    dsc = dsc * fsize / asc_dsc;
    let (c, s) = (line.dir.x, line.dir.y);
    let mut trm1 = fz_matrix { a: c, b: -s, c: s, d: c, e: 0.0, f: 0.0 };
    let mut trm2 = fz_matrix { a: c, b: s, c: -s, d: c, e: 0.0, f: 0.0 };
    if c == -1.0 {
        trm1.d = 1.0;
        trm2.d = 1.0;
    }
    let o = ch.origin;
    let xlate1 = fz_matrix { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: -o.x, f: -o.y };
    let xlate2 = fz_matrix { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: o.x, f: o.y };
    let mut q = unsafe { fz_transform_quad(fz_transform_quad(ch.quad, xlate1), trm1) };
    if c == 1.0 && q.ul.y > 0.0 {
        q.ul.y = asc;
        q.ur.y = asc;
        q.ll.y = dsc;
        q.lr.y = dsc;
    } else {
        q.ul.y = -asc;
        q.ur.y = -asc;
        q.ll.y = -dsc;
        q.lr.y = -dsc;
    }
    if q.ll.x < 0.0 {
        q.ll.x = 0.0;
        q.ul.x = 0.0;
    }
    #[allow(clippy::float_equality_without_abs)] // as the reference: zero or negative width
    let zero_width = q.lr.x - q.ll.x < f32::EPSILON;
    if zero_width {
        let glyph = unsafe { fz_encode_character(ctx, font, ch.c) };
        if glyph != 0 {
            let fwidth = unsafe { fz_advance_glyph(ctx, font, glyph, line.wmode as i32) };
            q.lr.x = q.ll.x + fwidth * fsize;
            q.ur.x = q.lr.x;
        }
    }
    unsafe { fz_transform_quad(fz_transform_quad(q, trm2), xlate2) }
}
