//! ocr_image <image> [cpu|gpu] [detail image]: download models if needed, OCR an image, print lines.
use std::sync::atomic::AtomicBool;
use strata_ocr::{models, ndl::NdlOcr, Device, OcrEngine};

fn main() {
    env_logger::init();
    let path = std::env::args().nth(1).expect("image path");
    let dev = if std::env::args().nth(2).as_deref() == Some("cpu") { Device::Cpu } else { Device::Gpu };
    let set = models::set("ndlocr-lite").unwrap();
    if !set.is_installed() {
        eprintln!("downloading {} ({:.0} MB) to {}", set.title, set.total_size() as f64 / 1e6, set.dir().display());
        let last = std::cell::Cell::new(0u64);
        models::install(&set, &|d, t| { if d - last.get() > 20_000_000 || d == t { eprintln!("  {:.0}/{:.0} MB", d as f64 / 1e6, t as f64 / 1e6); last.set(d); } }, &AtomicBool::new(false)).unwrap();
    }
    let t = std::time::Instant::now();
    let ocr = NdlOcr::load(&set, dev).unwrap();
    eprintln!("load {:?} device={:?} charset={}", t.elapsed(), ocr.device_used, 0);
    let img = image::open(&path).unwrap().to_rgb8();
    let detail = std::env::args().nth(3).map(|p| image::open(p).unwrap().to_rgb8());
    for round in 0..2 {
        let t = std::time::Instant::now();
        let page = ocr.recognize_detailed(&img, detail.as_ref()).unwrap();
        eprintln!("round {round}: {:?}, {} lines, vertical={}", t.elapsed(), page.lines.len(), page.vertical);
        if round == 1 { println!("{}", page.text()); }
    }
}
