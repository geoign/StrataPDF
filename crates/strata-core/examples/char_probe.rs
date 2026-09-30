//! char_probe <file> [--max-pages N] [--max-glyphs N]: the glyphs of the fonts of a document that
//! have few distinct glyphs (symbol, pi and "special character" fonts), as JSON lines: font, the
//! Unicode value MuPDF takes from the PDF (`ucs`), glyph index and name, a hash of the glyph outline
//! (the same glyph of the same font has the same hash in every paper), advance and box of the
//! outline in em, use count and the page position of the first use. Evidence for
//! `strata_core::glyphs`: a (font, ucs) whose outline is not the character `ucs` names.
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::rc::Rc;

use mupdf::path::PathWalker;
use mupdf::{ColorParams, Colorspace, Device, Font, Matrix, NativeDevice, StrokeState, Text};
use mupdf_sys::fz_get_glyph_name;
use serde_json::json;

struct GlyphUse {
    n: usize,
    page: i32,
    x: f32,
    y: f32,
    size: f32,
}

struct FontUse {
    font: Font,
    glyphs: HashMap<(i32, i32), GlyphUse>,
}

#[derive(Clone)]
struct Recorder {
    fonts: Rc<RefCell<HashMap<String, FontUse>>>,
    page: Rc<RefCell<i32>>,
}

impl Recorder {
    fn record(&self, text: &Text, cmt: &Matrix) {
        let page = *self.page.borrow();
        for span in text.spans() {
            let font = span.font();
            let name = font.name().to_string();
            let mut fonts = self.fonts.borrow_mut();
            let fu = fonts.entry(name).or_insert_with(|| FontUse { font, glyphs: HashMap::new() });
            let t = span.trm();
            for it in span.items() {
                let (a, b, c, d) = (t.a * cmt.a + t.b * cmt.c, t.a * cmt.b + t.b * cmt.d, t.c * cmt.a + t.d * cmt.c, t.c * cmt.b + t.d * cmt.d);
                let x = it.x() * cmt.a + it.y() * cmt.c + cmt.e;
                let y = it.x() * cmt.b + it.y() * cmt.d + cmt.f;
                let size = (a * d - b * c).abs().sqrt();
                let g = fu.glyphs.entry((it.ucs(), it.gid())).or_insert(GlyphUse { n: 0, page, x, y, size });
                g.n += 1;
            }
        }
    }
}

impl NativeDevice for Recorder {
    fn fill_text(&mut self, text: &Text, cmt: Matrix, _cs: &Colorspace, _color: &[f32], _alpha: f32, _cp: ColorParams) {
        self.record(text, &cmt);
    }
    fn stroke_text(&mut self, text: &Text, _ss: &StrokeState, cmt: Matrix, _cs: &Colorspace, _color: &[f32], _alpha: f32, _cp: ColorParams) {
        self.record(text, &cmt);
    }
}

/// Hash and box of an outline.
struct Outline {
    h: u64,
    bb: [f32; 4],
    n: usize,
    /// Operations as text: `M x y`, `L x y`, `C x1 y1 x2 y2 x y`, `Z`, coordinates in 1/1000 em.
    ops: String,
}

impl Outline {
    fn mix(&mut self, tag: u8, pts: &[f32]) {
        self.ops.push(match tag {
            1 => 'M',
            2 => 'L',
            3 => 'C',
            _ => 'Z',
        });
        for &p in pts {
            self.ops.push_str(&format!("{} ", (p * 1000.0).round() as i32));
        }
        let mut h = self.h ^ tag as u64;
        h = h.wrapping_mul(0x100000001b3);
        for &p in pts {
            h ^= (p * 1000.0).round() as i64 as u64;
            h = h.wrapping_mul(0x100000001b3);
            self.n += 1;
        }
        self.h = h;
        for k in (0..pts.len()).step_by(2) {
            self.bb = [self.bb[0].min(pts[k]), self.bb[1].min(pts[k + 1]), self.bb[2].max(pts[k]), self.bb[3].max(pts[k + 1])];
        }
    }
}

impl PathWalker for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.mix(1, &[x, y]);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.mix(2, &[x, y]);
    }
    fn curve_to(&mut self, a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) {
        self.mix(3, &[a, b, c, d, e, f]);
    }
    fn close(&mut self) {
        self.mix(4, &[]);
    }
}

fn main() {
    strata_core::fonts::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = &args[0];
    let num = |k: &str, d: usize| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(d);
    let max_pages = num("--max-pages", usize::MAX) as i32;
    let max_glyphs = num("--max-glyphs", 24);
    let Ok(doc) = mupdf::Document::open(path) else { return };
    let n = doc.page_count().unwrap_or(0).min(max_pages);
    let rec = Recorder { fonts: Rc::new(RefCell::new(HashMap::new())), page: Rc::new(RefCell::new(0)) };
    let Ok(dev) = Device::from_native(rec.clone()) else { return };
    for p in 0..n {
        let Ok(page) = doc.load_page(p) else { continue };
        *rec.page.borrow_mut() = p;
        let _ = page.run(&dev, &Matrix::IDENTITY);
    }
    let ctx = mupdf::context::raw_context();
    for (name, fu) in rec.fonts.borrow().iter() {
        if fu.glyphs.len() > max_glyphs {
            continue;
        }
        for (&(ucs, gid), g) in &fu.glyphs {
            let mut o = Outline { h: 0xcbf29ce484222325, bb: [f32::MAX, f32::MAX, f32::MIN, f32::MIN], n: 0, ops: String::new() };
            // SAFETY: the font is alive; `glyph_count` is a plain field.
            let valid = gid >= 0 && gid < unsafe { (*fu.font.as_raw()).glyph_count };
            let mut adv = 0.0;
            let mut gname = String::new();
            if valid {
                if let Ok(Some(p)) = fu.font.outline_glyph(gid) {
                    let _ = p.walk(&mut o);
                }
                adv = fu.font.advance_glyph(gid).unwrap_or(0.0);
                gname = unsafe {
                    let mut buf = [0 as std::ffi::c_char; 128];
                    fz_get_glyph_name(ctx, fu.font.as_raw(), gid, buf.as_mut_ptr(), buf.len() as i32);
                    CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
                };
            }
            println!(
                "{}",
                json!({"font": name, "ucs": ucs, "gid": gid, "name": gname, "n": g.n, "ohash": format!("{:016x}", o.h), "adv": adv,
                       "bb": if o.n > 0 { json!(o.bb) } else { json!(null) }, "path": o.ops, "page": g.page, "x": g.x, "y": g.y, "size": g.size,
                       "nglyphs": fu.glyphs.len()})
            );
        }
    }
}
