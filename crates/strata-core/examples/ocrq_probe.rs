//! ocrq_probe <pdf>...: quality of the foreign OCR text layer of each document, a JSON line each.
use serde_json::json;
use std::sync::Arc;
use strata_core::Document;

fn main() {
    for path in std::env::args().skip(1) {
        let t = std::time::Instant::now();
        let q = Document::open(path.as_ref(), None, Arc::new(|| {})).ok().and_then(|d| d.foreign_ocr_quality());
        let mut j = match q {
            Some(q) => json!({"pages": q.pages, "en_words": q.en_words, "en_bad": q.en_bad, "ja_chars": q.ja_chars, "ja_bad": q.ja_bad, "poor": q.poor()}),
            None => json!({"pages": 0}),
        };
        j["ms"] = json!(t.elapsed().as_millis() as u64);
        j["path"] = json!(path);
        println!("{j}");
    }
}
