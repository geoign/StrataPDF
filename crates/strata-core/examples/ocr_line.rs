//! ocr_line <pdf> <page> <x0> <y0> <x1> <y1>: read one line (box in points) with each
//! recognizer of NDLOCR-Lite, rendered as the OCR renders pages.
use strata_core::ocr::{models, Device, OCR_DPI};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let doc = mupdf::Document::open(&a[1]).unwrap();
    let page = doc.load_page(a[2].parse().unwrap()).unwrap();
    let r: Vec<f32> = a[3..7].iter().map(|v| v.parse().unwrap()).collect();
    let s = OCR_DPI / 72.0;
    let pix = page.to_pixmap(&mupdf::Matrix::new_scale(s, s), &mupdf::Colorspace::device_rgb(), false, true).unwrap();
    let (w, h) = (pix.width(), pix.height());
    let n = pix.n() as usize;
    let rgb: Vec<u8> = pix.samples().chunks(n).flat_map(|p| [p[0], p[1], p[2]]).collect();
    let img = image::RgbImage::from_raw(w, h, rgb).unwrap();
    let (x0, y0, x1, y1) = ((r[0] * s) as u32, (r[1] * s) as u32, (r[2] * s) as u32, (r[3] * s) as u32);
    let crop = image::imageops::crop_imm(&img, x0, y0, x1 - x0, y1 - y0).to_image();
    crop.save("ocr_line.png").unwrap();
    let set = models::set("ndlocr-lite").unwrap();
    let ocr = strata_ocr::ndl::NdlOcr::load(&set, Device::Gpu).unwrap();
    for (name, t) in ["rec30", "rec50", "rec100"].iter().zip(ocr.read_with_each(&crop).unwrap()) {
        println!("{name} ({} chars): {t}", t.chars().count());
    }
}
