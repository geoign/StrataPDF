use mupdf::{Colorspace, Document, Matrix};

fn main() {
    let path = std::env::args().nth(1).expect("usage: render_one <file> [page]");
    let page_no: i32 = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(0);
    let t0 = std::time::Instant::now();
    let doc = Document::open(&path).expect("open");
    println!("pages={} needs_password={} open={:?}", doc.page_count().unwrap(), doc.needs_password().unwrap(), t0.elapsed());
    let page = doc.load_page(page_no).expect("load_page");
    let pix = page
        .to_pixmap(&Matrix::new_scale(2.0, 2.0), &Colorspace::device_rgb(), false, true)
        .expect("render");
    pix.save_as("C:/tmp/strata_render_one.png", mupdf::ImageFormat::PNG).expect("save");
    println!("{}x{} total={:?}", pix.width(), pix.height(), t0.elapsed());
}
