//! Smoke test: open a file, render every tile of a few pages through the pool, run a search.
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use strata_core::render::{level_for_scale, tile_grid};
use strata_core::{Document, RenderPool, SearchEvent, TileKey};

fn main() {
    let path = std::env::args().nth(1).expect("usage: smoke <file> [needle]");
    let needle = std::env::args().nth(2);
    let waker: strata_core::Waker = Arc::new(|| {});
    let t0 = Instant::now();
    let pw = std::env::var("STRATA_PW").ok();
    let doc = match Document::open(path.as_ref(), pw, waker.clone()) { Ok(d) => d, Err(e) => { println!("open error: {e}"); return; } };
    let info = doc.info().clone();
    println!("open {:?}: pages={} format={} enc='{}' r2l={} layout={:?}", t0.elapsed(), info.page_count, info.format, info.encryption, info.right_to_left, info.page_layout);

    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_sub(2).max(2);
    let pool = RenderPool::new(threads, waker.clone());
    pool.register(doc.client());
    let sizes = doc.page_sizes();
    let level = level_for_scale(1.5);
    let mut keys = Vec::new();
    for page in 0..info.page_count.min(4) as u32 {
        let (c, r) = tile_grid(sizes[page as usize], level);
        for ty in 0..r { for tx in 0..c { keys.push(TileKey { doc: doc.id(), page, rev: 0, level, tx, ty }); } }
    }
    let n = keys.len();
    let t1 = Instant::now();
    pool.set_wanted(1, keys);
    let mut got = 0; let mut errs = 0;
    while got < n {
        for t in pool.drain() { got += 1; if t.error.is_some() { errs += 1; eprintln!("{:?}", t.error); } }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("rendered {n} tiles ({threads} threads, level {level}) in {:?}, errors={errs}", t1.elapsed());

    let t2 = Instant::now();
    while !doc.sizes_complete() { std::thread::sleep(Duration::from_millis(10)); }
    println!("page sizes complete in {:?}", t2.elapsed());

    let mut outline = doc.outline();
    loop { if let Some(r) = outline.poll() { println!("outline top-level items: {}", r.as_ref().map(|o| o.len()).unwrap_or(0)); break; } std::thread::sleep(Duration::from_millis(5)); }

    if let Some(needle) = needle {
        let t3 = Instant::now();
        let rx = doc.search(needle, 0, Arc::new(AtomicBool::new(false)));
        let mut hits = 0; let mut pages = 0;
        for ev in rx { match ev { SearchEvent::Hits { quads, .. } => { hits += quads.len(); pages += 1; } SearchEvent::Done => break, SearchEvent::Error(e) => { println!("search error {e}"); break } _ => {} } }
        println!("search: {hits} hits on {pages} pages in {:?}", t3.elapsed());
    }
    let mut text = doc.text(0);
    loop { if let Some(r) = text.poll() { let t = r.as_ref().unwrap(); let s = t.plain_text(); println!("page0 text chars={} head={:?}", s.chars().count(), s.chars().take(60).collect::<String>()); break; } std::thread::sleep(Duration::from_millis(5)); }
}
