//! ocr_variants <image> <x0> <y0> <x1> <y1>: read one line box (pixels) with each
//! recognizer, from crops grown or shrunk by a few pixels.
use strata_ocr::{models, ndl::NdlOcr, Device};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let img = image::open(&a[1]).unwrap().to_rgb8();
    let b: Vec<i64> = a[2..6].iter().map(|v| v.parse().unwrap()).collect();
    let set = models::set("ndlocr-lite").unwrap();
    let ocr = NdlOcr::load(&set, Device::Gpu).unwrap();
    for (dx, dy) in [(0, 0), (2, 0), (-2, 0), (0, 2), (0, -1), (3, 3), (4, 1)] {
        let (x0, y0) = ((b[0] - dx).max(0) as u32, (b[1] - dy).max(0) as u32);
        let (x1, y1) = ((b[2] + dx).min(img.width() as i64) as u32, (b[3] + dy).min(img.height() as i64) as u32);
        let crop = image::imageops::crop_imm(&img, x0, y0, x1 - x0, y1 - y0).to_image();
        let each = ocr.read_with_each(&crop).unwrap();
        println!("({dx},{dy}) 50: {}\n       100: {}", each[1], each[2]);
    }
}
