//! table_extract <pdf> <page(0-based)> <x0> <y0> <x1> <y1> [ocr]
use std::sync::Arc;
use strata_core::{Document, RectF};

fn main() {
    strata_core::fonts::install();
    let a: Vec<String> = std::env::args().collect();
    let doc = Document::open(a[1].as_ref(), None, Arc::new(|| {})).unwrap();
    let f = |i: usize| a[i].parse::<f32>().unwrap();
    let region = RectF { x0: f(3), y0: f(4), x1: f(5), y1: f(6) };
    let ocr: Option<Arc<dyn strata_core::ocr::OcrEngine>> = if a.get(7).map(String::as_str) == Some("ocr") {
        Some(Arc::new(strata_ocr::ndl::NdlOcr::load(&strata_ocr::models::set("ndlocr-lite").unwrap(), strata_ocr::Device::Gpu).unwrap()))
    } else { None };
    let t = std::time::Instant::now();
    match doc.extract_table(a[2].parse().unwrap(), region, ocr).recv().unwrap() {
        Ok(r) => {
            println!("{:?} {} rows x {} cols in {:?}", r.source, r.rows.len(), r.cols(), t.elapsed());
            for i in &r.issues { println!("ISSUE: {i}"); }
            print!("{}", r.to_csv());
        }
        Err(e) => println!("error: {e}"),
    }
}
