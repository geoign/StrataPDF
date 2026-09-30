//! extract_time <file> <page>: time MuPDF text extraction of one page with reflow's flags,
//! with and without segmentation and vectors.
use mupdf::{Document, TextPageFlags};
use strata_core::rich::reflow_flags;

fn main() {
    strata_core::fonts::install();
    let path = std::env::args().nth(1).unwrap();
    let pno: i32 = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(0);
    let doc = Document::open(&path).unwrap();
    let page = doc.load_page(pno).unwrap();
    let base = reflow_flags() & !TextPageFlags::TABLE_HUNT;
    for (name, flags) in [
        ("reflow (no vectors)", base),
        ("reflow - segment + vectors", (base & !TextPageFlags::SEGMENT) | TextPageFlags::COLLECT_VECTORS),
        ("reflow + vectors", base | TextPageFlags::COLLECT_VECTORS),
    ] {
        let t = std::time::Instant::now();
        let _tp = page.to_text_page(flags).unwrap();
        eprintln!("{name}: {:?}", t.elapsed());
    }
}
