//! images <pdf> <first page(0-based)> <last page> <out dir>: list and save every image.
use std::sync::Arc;
use strata_core::Document;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let doc = Document::open(a[1].as_ref(), None, Arc::new(|| {})).unwrap();
    let out = std::path::Path::new(&a[4]);
    std::fs::create_dir_all(out).unwrap();
    for p in a[2].parse::<u32>().unwrap()..=a[3].parse::<u32>().unwrap() {
        let mut pending = doc.images(p);
        let list = loop {
            if let Some(v) = pending.poll() { break v.clone().unwrap(); }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        for (i, im) in list.iter().enumerate() {
            println!("p{p} #{i} {}x{} jpeg={} bbox={:?}", im.width, im.height, im.jpeg, im.bbox);
            for jpeg in [true, false] {
                if jpeg && !im.jpeg { continue; }
                let path = out.join(format!("p{p}_{i}.{}", if jpeg { "jpg" } else { "png" }));
                let t = std::time::Instant::now();
                let mut s = doc.save_image(p, i, jpeg, path.clone());
                let r = loop {
                    if let Some(v) = s.poll() { break v.clone(); }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                };
                println!("  -> {} {:?} {:?}", path.display(), r, t.elapsed());
            }
        }
    }
}
