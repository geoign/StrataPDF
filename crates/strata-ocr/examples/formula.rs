//! formula <image>...: LaTeX for formula images (downloads the model if needed).
use std::sync::atomic::AtomicBool;
use strata_ocr::formula::{split_equation_number, FormulaEngine, Pix2TextMfr};
use strata_ocr::models;

fn main() {
    let set = models::set("pix2text-mfr").unwrap();
    if !set.is_installed() {
        eprintln!("downloading {} ...", set.title);
        models::install(&set, &|_, _| {}, &AtomicBool::new(false)).unwrap();
    }
    let m = Pix2TextMfr::load(&set).unwrap();
    for p in std::env::args().skip(1) {
        let img = image::open(&p).unwrap().to_rgb8();
        let t = std::time::Instant::now();
        let l = m.to_latex(&img).unwrap();
        println!("{:?} {:?} {}", t.elapsed(), split_equation_number(&l).1, l);
    }
}
