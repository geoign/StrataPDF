//! ocrq_probe <pdf>...: quality of the foreign OCR text layer of each document, a JSON line each.
use std::sync::Arc;
use strata_core::Document;

fn main() {
    for path in std::env::args().skip(1) {
        let t = std::time::Instant::now();
        let q = Document::open(path.as_ref(), None, Arc::new(|| {})).ok().and_then(|d| d.foreign_ocr_quality());
        let json = match q {
            Some(q) => format!(
                "{{\"pages\": {}, \"en_words\": {}, \"en_bad\": {}, \"ja_chars\": {}, \"ja_bad\": {}, \"poor\": {}",
                q.pages, q.en_words, q.en_bad, q.ja_chars, q.ja_bad, q.poor()
            ),
            None => "{\"pages\": 0".to_string(),
        };
        println!("{json}, \"ms\": {}, \"path\": {:?}}}", t.elapsed().as_millis(), path);
    }
}
