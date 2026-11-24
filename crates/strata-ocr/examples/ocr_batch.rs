//! ocr_batch <model dir> <cpu|gpu> <list> <out.jsonl>: OCR many page images with the
//! models of a directory (files named as in the model set) and write one JSON line per
//! page, to compare model versions without touching the app's models and OCR cache.
//! The list has one "image[<TAB>detail image]" per line.
use std::io::Write;
use strata_ocr::{Device, OcrEngine, models, ndl::NdlOcr};

fn main() {
    env_logger::init();
    let a: Vec<String> = std::env::args().collect();
    let dev = if a[2] == "cpu" { Device::Cpu } else { Device::Gpu };
    let mut set = models::set("ndlocr-lite").unwrap();
    // (An absolute path as the id replaces the model root. Sizes are those of the
    // files there: they are compared, not downloaded.)
    set.id = std::fs::canonicalize(&a[1]).unwrap().to_string_lossy().into_owned();
    for f in &mut set.files {
        f.size = std::fs::metadata(std::path::Path::new(&a[1]).join(&f.name)).unwrap().len();
    }
    let ocr = NdlOcr::load(&set, dev).unwrap();
    eprintln!("models {} device={:?}", set.id, ocr.device_used);
    let mut out = std::io::BufWriter::new(std::fs::File::create(&a[4]).unwrap());
    let mut total = std::time::Duration::ZERO;
    let mut pages = 0;
    for l in std::fs::read_to_string(&a[3]).unwrap().lines().filter(|l| !l.trim().is_empty()) {
        let mut f = l.split('\t');
        let path = f.next().unwrap();
        let img = image::open(path).unwrap().to_rgb8();
        let detail = f.next().map(|p| image::open(p).unwrap().to_rgb8());
        let t = std::time::Instant::now();
        let page = ocr.recognize_detailed(&img, detail.as_ref()).unwrap();
        let dt = t.elapsed();
        total += dt;
        pages += 1;
        writeln!(out, "{}", serde_json::json!({ "image": path, "ms": dt.as_millis() as u64, "page": page })).unwrap();
        if pages % 20 == 0 {
            eprintln!("{pages} pages, {:.2} s/page", total.as_secs_f64() / pages as f64);
        }
    }
    out.flush().unwrap();
    eprintln!("DONE {pages} pages, {:.2} s/page", total.as_secs_f64() / pages.max(1) as f64);
}
