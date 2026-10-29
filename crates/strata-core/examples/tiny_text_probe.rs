//! tiny_text_probe <pdf>...: pages whose text layer has characters too small to see
//! (below 1 pt), as a JSON line per document: such pages are the ones where the choice
//! of pages to OCR and reflow could judge the text layer differently.
use mupdf::Document;
use serde_json::json;
use strata_core::rich::{reflow_flags, RichBlock, RichPage};

fn main() {
    strata_core::fonts::install();
    for path in std::env::args().skip(1) {
        let Ok(doc) = Document::open(&path) else {
            println!("{}", json!({"path": path, "error": "open"}));
            continue;
        };
        let n = doc.page_count().unwrap_or(0);
        let mut pages = Vec::new();
        for p in 0..n {
            let Ok(page) = doc.load_page(p) else { continue };
            let Ok(b) = page.bounds() else { continue };
            let Ok(tp) = page.to_text_page(reflow_flags()) else { continue };
            let rich = RichPage::from_page(&page, &tp, b.width(), b.height());
            let (mut tiny, mut seen) = (0usize, 0usize);
            for blk in &rich.blocks {
                if let RichBlock::Text { lines, .. } = blk {
                    for c in lines.iter().flat_map(|l| l.chars.iter()).filter(|c| !c.c.is_whitespace()) {
                        if c.size < 1.0 { tiny += 1 } else { seen += 1 }
                    }
                }
            }
            if tiny > 0 {
                pages.push(json!({"page": p + 1, "tiny": tiny, "seen": seen, "needs_ocr": strata_core::reflow::needs_ocr(&rich)}));
            }
        }
        println!("{}", json!({"path": path, "pages": n, "tiny_pages": pages}));
    }
}
