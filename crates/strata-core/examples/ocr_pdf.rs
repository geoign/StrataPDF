//! ocr_pdf <file> <out_dir> [all|scans]: OCR pages needing it, then write reflow Markdown.
use std::sync::{Arc, atomic::AtomicBool};
use strata_core::ocr::{models, Device, OcrEvent, OcrScope};
use strata_core::reflow::{output, ReflowEvent, ReflowOptions};
use strata_core::Document;

fn main() {
    strata_core::fonts::install();
    let path = std::env::args().nth(1).unwrap();
    let out = std::path::PathBuf::from(std::env::args().nth(2).unwrap());
    let scope = match std::env::args().nth(3).as_deref() {
        Some("all") => OcrScope::All,
        Some("scans") => OcrScope::Scans,
        _ => OcrScope::Needed,
    };
    std::fs::create_dir_all(out.join("images")).unwrap();
    let set = models::set("ndlocr-lite").unwrap();
    let engine = Arc::new(strata_ocr::ndl::NdlOcr::load(&set, Device::Gpu).unwrap());
    let doc = Document::open(path.as_ref(), None, Arc::new(|| {})).unwrap();
    eprintln!("foreign OCR layer before: {:?}", doc.foreign_ocr_quality().map(|q| (q.pages, q.en_rate(), q.ja_rate(), q.poor())));
    let t = std::time::Instant::now();
    for ev in doc.run_ocr(scope, engine, Arc::new(AtomicBool::new(false))) {
        match ev {
            OcrEvent::Done { pages } => { eprintln!("ocr done: {pages} pages in {:?}", t.elapsed()); break; }
            OcrEvent::Error(e) => eprintln!("ocr error: {e}"),
            _ => {}
        }
    }
    for ev in doc.reflow(ReflowOptions::default(), Arc::new(AtomicBool::new(false))) {
        if let ReflowEvent::Done(d) = ev {
            for im in &d.images { std::fs::write(out.join("images").join(&im.id), &im.png).unwrap(); }
            std::fs::write(out.join("out.md"), output::to_markdown(&d, &|im| format!("images/{}", im.id))).unwrap();
            eprintln!("reflow: {} nodes; foreign OCR layer after: {:?}", d.nodes.len(), d.ocr_layer.map(|q| (q.pages, q.poor())));
            break;
        }
    }
}
