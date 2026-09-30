//! crop_test <file> <page 1-based>: time region rendering from a display list,
//! with and without text-page extraction first (reflow keeps both).
use std::time::Instant;

use mupdf::{Document, TextPageFlags};
use strata_core::geom::RectF;
use strata_core::render::render_region_png;
use strata_core::rich::reflow_flags;

fn main() {
    strata_core::fonts::install();
    let args: Vec<String> = std::env::args().collect();
    let doc = Document::open(&args[1]).unwrap();
    // Display lists (and text pages) of every page kept alive, as reflow does.
    let t = Instant::now();
    let mut keep = Vec::new();
    let mode = args.get(3).map(String::as_str).unwrap_or("both");
    for i in 0..doc.page_count().unwrap() {
        if mode == "skip18" && i == 17 {
            continue;
        }
        if mode == "text" && i >= 17 || mode == "images" && i < 18 {
            continue;
        }
        let pg = doc.load_page(i).unwrap();
        let tp = if mode != "dl" && mode != "text" && mode != "images" { Some(pg.to_text_page(reflow_flags() | TextPageFlags::COLLECT_VECTORS).unwrap()) } else { None };
        let dl = if mode != "tp" { Some(pg.to_display_list(true).unwrap()) } else { None };
        keep.push((dl, tp));
    }
    println!("all pages {:?}", t.elapsed());
    let page = doc.load_page(args[2].parse::<i32>().unwrap() - 1).unwrap();
    let b = page.bounds().unwrap();
    let region = RectF { x0: b.x0, y0: b.y0, x1: b.x1, y1: b.y1 };
    let t = Instant::now();
    let dl = page.to_display_list(true).unwrap();
    println!("display list {:?}", t.elapsed());
    let t = Instant::now();
    let (w, h, png) = render_region_png(&dl, region, 2.0).unwrap();
    println!("crop from fresh display list {:?} ({w}x{h}, {} KB)", t.elapsed(), png.len() / 1024);
    let t = Instant::now();
    let _tp = page.to_text_page(reflow_flags() | TextPageFlags::COLLECT_VECTORS).unwrap();
    println!("text page {:?}", t.elapsed());
    let t = Instant::now();
    let (w, h, png) = render_region_png(&dl, region, 2.0).unwrap();
    println!("crop again {:?} ({w}x{h}, {} KB)", t.elapsed(), png.len() / 1024);
}
