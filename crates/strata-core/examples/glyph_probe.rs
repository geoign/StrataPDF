//! glyph_probe <file> [--max-pages N]: the characters MuPDF finds no Unicode value for, one JSON line
//! each (font, glyph index, glyph name, box, and the text `glyphs::glyph_text` gives for it), and one
//! line per page with the match statistics. Used to collect the (font, glyph name) evidence that
//! `strata_core::glyphs` is built from: a page is extracted with the reflow flags and
//! again with `USE_GID_FOR_UNKNOWN_UNICODE`, and the unknown characters of the first are looked up
//! by position in the second, as `RichPage::from_page` does.
use std::ffi::CStr;

use mupdf_sys::*;
use serde_json::json;
use strata_core::rich::reflow_flags;

struct Ch {
    c: i32,
    flags: u16,
    font: *mut fz_font,
    bbox: [f32; 4],
    size: f32,
}

unsafe fn flat(mut b: *mut fz_stext_block, out: &mut Vec<Ch>, depth: u32) {
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
                            let q = &ch.quad;
                            let xs = [q.ul.x, q.ur.x, q.ll.x, q.lr.x];
                            let ys = [q.ul.y, q.ur.y, q.ll.y, q.lr.y];
                            out.push(Ch {
                                c: ch.c,
                                flags: ch.flags as u16,
                                font: ch.font,
                                bbox: [
                                    xs.iter().copied().fold(f32::INFINITY, f32::min),
                                    ys.iter().copied().fold(f32::INFINITY, f32::min),
                                    xs.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                                    ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                                ],
                                size: ch.size,
                            });
                            c = ch.next;
                        }
                        l = (*l).next;
                    }
                }
                FZ_STEXT_BLOCK_STRUCT => {
                    let s = blk.u.s.down;
                    if !s.is_null() && depth < 64 {
                        flat((*s).first_block, out, depth + 1);
                    }
                }
                _ => {}
            }
            b = blk.next;
        }
    }
}

fn main() {
    strata_core::fonts::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = &args[0];
    let max_pages: i32 = args.iter().position(|a| a == "--max-pages").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(i32::MAX);
    let Ok(doc) = mupdf::Document::open(path) else { return };
    let n = doc.page_count().unwrap_or(0).min(max_pages);
    let ctx = mupdf::context::raw_context();
    let gid_flags = mupdf::TextPageFlags::USE_GID_FOR_UNKNOWN_UNICODE | mupdf::TextPageFlags::PRESERVE_LIGATURES | mupdf::TextPageFlags::PRESERVE_WHITESPACE;
    for p in 0..n {
        let Ok(page) = doc.load_page(p) else { continue };
        let Ok(a) = page.to_text_page(reflow_flags()) else { continue };
        let mut va = Vec::new();
        unsafe { flat((*a.as_raw()).first_block, &mut va, 0) };
        if !va.iter().any(|c| c.c == 0xFFFD) {
            continue;
        }
        let Ok(b) = page.to_text_page(gid_flags) else { continue };
        let mut vb = Vec::new();
        unsafe { flat((*b.as_raw()).first_block, &mut vb, 0) };
        let mut by_pos: std::collections::HashMap<(u32, u32), Vec<usize>> = std::collections::HashMap::new();
        for (i, y) in vb.iter().enumerate() {
            by_pos.entry((y.bbox[0].to_bits(), y.bbox[1].to_bits())).or_default().push(i);
        }
        let (mut hit, mut miss, mut unflagged) = (0, 0, 0);
        for x in va.iter().filter(|x| x.c == 0xFFFD) {
            let cand = by_pos.get(&(x.bbox[0].to_bits(), x.bbox[1].to_bits())).and_then(|v| v.iter().map(|&i| &vb[i]).find(|y| y.flags & 256 != 0));
            let Some(y) = cand else {
                if by_pos.contains_key(&(x.bbox[0].to_bits(), x.bbox[1].to_bits())) { unflagged += 1 } else { miss += 1 }
                continue;
            };
            hit += 1;
            let (name, count, fname) = unsafe {
                let mut buf = [0i8; 128];
                fz_get_glyph_name(ctx, y.font, y.c, buf.as_mut_ptr(), 128);
                let nm = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
                let fname = CStr::from_ptr(fz_font_name(ctx, y.font)).to_string_lossy().into_owned();
                (nm, (*y.font).glyph_count, fname)
            };
            println!(
                "{}",
                json!({"kind": "glyph", "page": p, "font": fname, "gid": y.c, "name": name, "count": count,
                       "bbox": y.bbox, "size": y.size, "text": strata_core::glyphs::glyph_text(&fname, &name)})
            );
        }
        println!("{}", json!({"kind": "page", "page": p, "hit": hit, "miss": miss, "unflagged": unflagged, "na": va.len(), "nb": vb.len()}));
    }
}
