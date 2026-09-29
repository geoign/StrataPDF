//! render_threads <file> <threads> <pages> [scale]: render every tile of the first pages with
//! N render threads. Regression check for files that crash under parallel rendering (JBIG2
//! scans); NO_ICC=1 turns colour management off.
use std::sync::Arc;
use std::time::Duration;
use strata_core::render::{level_for_scale, tile_grid};
use strata_core::{Document, RenderPool, TileKey};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let threads: usize = a[2].parse().unwrap();
    let pages: u32 = a[3].parse().unwrap();
    let scale: f32 = a.get(4).map(|s| s.parse().unwrap()).unwrap_or(1.5);
    if std::env::var("NO_ICC").is_ok() { mupdf::context::Context::get().disable_icc(); eprintln!("icc disabled"); }
    let waker: strata_core::Waker = Arc::new(|| {});
    let doc = Document::open(a[1].as_ref(), None, waker.clone()).unwrap();
    let pool = RenderPool::new(threads, waker);
    pool.register(doc.client());
    let sizes = doc.page_sizes();
    let level = level_for_scale(scale);
    for page in 0..pages {
        let mut keys = Vec::new();
        let (c, r) = tile_grid(sizes[page as usize], level);
        for ty in 0..r { for tx in 0..c { keys.push(TileKey { doc: doc.id(), page, rev: 0, level, tx, ty }); } }
        let n = keys.len();
        pool.set_wanted(1, keys);
        let mut got = 0;
        while got < n { got += pool.drain().len(); std::thread::sleep(Duration::from_millis(2)); }
        eprintln!("page {page}: {n} tiles ok");
    }
}
