//! Print the reflow block tree of a page: dump_rich <file> <page>
use mupdf::Document;
use strata_core::rich::{reflow_flags, RichBlock, RichPage};

fn main() {
    strata_core::fonts::install();
    let path = std::env::args().nth(1).unwrap();
    let pno: i32 = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(0);
    let doc = Document::open(&path).unwrap();
    let page = doc.load_page(pno).unwrap();
    let b = page.bounds().unwrap();
    let t = std::time::Instant::now();
    let tp = page.to_text_page(reflow_flags()).unwrap();
    let rp = RichPage::from_page(&page, &tp, b.width(), b.height());
    eprintln!("extract {:?}", t.elapsed());
    let mut depth = 0usize;
    for blk in &rp.blocks {
        let ind = "  ".repeat(depth);
        match blk {
            RichBlock::RegionStart { kind } => { println!("{ind}<{kind}>"); depth += 1; }
            RichBlock::RegionEnd => { depth = depth.saturating_sub(1); println!("{}</>", "  ".repeat(depth)); }
            RichBlock::Text { bbox, lines } => {
                let first = lines[0].text();
                let sz = lines[0].chars[0].size;
                let f = &rp.fonts.get(lines[0].chars[0].font as usize).map(|f| f.name.clone()).unwrap_or_default();
                println!("{ind}TEXT [{:.0},{:.0},{:.0},{:.0}] {} lines sz={sz:.1} font={f} v={} | {}", bbox.x0, bbox.y0, bbox.x1, bbox.y1, lines.len(), lines[0].vertical, first.chars().take(70).collect::<String>());
            }
            RichBlock::Image { bbox } => println!("{ind}IMAGE [{:.0},{:.0},{:.0},{:.0}]", bbox.x0, bbox.y0, bbox.x1, bbox.y1),
            RichBlock::Vector { .. } => {}
            RichBlock::Grid { bbox, xs, ys } => println!("{ind}GRID [{:.0},{:.0},{:.0},{:.0}] cols={} rows={}", bbox.x0, bbox.y0, bbox.x1, bbox.y1, xs.len(), ys.len()),
        }
    }
}
