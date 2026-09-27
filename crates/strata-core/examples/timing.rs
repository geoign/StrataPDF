use mupdf::{Colorspace, Document, Matrix};
use std::time::Instant;
fn main() {
    strata_core::fonts::install();
    let path = std::env::args().nth(1).unwrap();
    let doc = Document::open(&path).unwrap();
    for round in 0..2 {
        for p in 0..3 {
            let t = Instant::now();
            let page = doc.load_page(p).unwrap();
            let t1 = t.elapsed();
            let dl = page.to_display_list(true).unwrap();
            let t2 = t.elapsed();
            let _pix = dl.to_pixmap(&Matrix::new_scale(2.0, 2.0), &Colorspace::device_rgb(), false).unwrap();
            println!("round {round} page {p}: load {:?} dl {:?} render {:?}", t1, t2 - t1, t.elapsed() - t2);
        }
    }
}
