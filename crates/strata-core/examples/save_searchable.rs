//! save_searchable <in.pdf> <out.pdf>: add an invisible OCR text layer (runs OCR if needed).
use std::sync::{Arc, atomic::AtomicBool};
use strata_core::ocr::{models, Device, OcrEvent, OcrScope};
use strata_core::Document;

fn main() {
    strata_core::fonts::install();
    let a: Vec<String> = std::env::args().collect();
    let doc = Document::open(a[1].as_ref(), None, Arc::new(|| {})).unwrap();
    let engine = Arc::new(strata_ocr::ndl::NdlOcr::load(&models::set("ndlocr-lite").unwrap(), Device::Gpu).unwrap());
    for ev in doc.run_ocr(OcrScope::Needed, engine, Arc::new(AtomicBool::new(false))) {
        if let OcrEvent::Done { pages } = ev { eprintln!("ocr: {pages} new pages"); break; }
    }
    let t = std::time::Instant::now();
    let n = doc.save_searchable_pdf(a[2].as_ref()).unwrap();
    eprintln!("text layer on {n} pages in {:?}", t.elapsed());
}
