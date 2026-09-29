//! translate <file> <engine> <out.html>
//! engine: `gemini:<model>` (key from GEMINI_API_KEY or the file named by GEMINI_KEY_FILE),
//! or `llama:<model id>` (GGUF in STRATA_MODELS; GPU unless LLAMA_CPU is set).
//! Writes a side-by-side HTML with the translations filled in, and prints timing.
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use strata_core::Document;
use strata_core::reflow::{ReflowEvent, ReflowImage, ReflowOptions, output};
use strata_translate::{Event, Store, Translator};

fn engine(spec: &str) -> Arc<dyn Translator> {
    let (kind, model) = spec.split_once(':').unwrap_or((spec, ""));
    match kind {
        "gemini" => {
            let key = std::env::var("GEMINI_API_KEY")
                .ok()
                .or_else(|| std::env::var("GEMINI_KEY_FILE").ok().and_then(|f| std::fs::read_to_string(f).ok()))
                .expect("GEMINI_API_KEY or GEMINI_KEY_FILE");
            Arc::new(strata_translate::gemini::Gemini::new(key.trim().to_string(), model))
        }
        #[cfg(feature = "llama")]
        "llama" => {
            let spec = strata_translate::llama::MODELS.iter().find(|m| m.id == model).copied().expect("model id");
            let dir = std::env::var("STRATA_MODELS").map(std::path::PathBuf::from).ok().or_else(strata_translate::llama::models_dir).unwrap();
            let gpu = if std::env::var("LLAMA_CPU").is_ok() { 0 } else { 999 };
            let t = std::time::Instant::now();
            let l = strata_translate::llama::Llama::load(spec, &dir.join(spec.file), gpu).unwrap();
            eprintln!("loaded {} in {:.1}s (gpu={})", spec.label, t.elapsed().as_secs_f32(), l.uses_gpu());
            Arc::new(l)
        }
        _ => panic!("unknown engine {kind}"),
    }
}

fn main() {
    strata_core::fonts::install();
    let a: Vec<String> = std::env::args().collect();
    let doc = Document::open(a[1].as_ref(), None, Arc::new(|| {})).unwrap();
    let rx = doc.reflow(ReflowOptions::default(), Arc::new(AtomicBool::new(false)));
    let d = loop {
        match rx.recv().unwrap() {
            ReflowEvent::Done(d) => break d,
            ReflowEvent::Error(e) => panic!("{e}"),
            _ => {}
        }
    };
    let plan = Arc::new(strata_translate::plan(&d));
    let chars: usize = plan.segments.iter().map(|s| s.text.len()).sum();
    eprintln!("{} nodes, {} segments, {chars} chars", d.nodes.len(), plan.segments.len());
    let eng = engine(&a[2]);
    let store = Arc::new(Store::open(a[1].as_ref(), &eng.id()));
    let t = std::time::Instant::now();
    let job = strata_translate::start(plan.clone(), store, eng.clone(), 0, Arc::new(|| {}));
    let mut tr: Vec<(usize, String)> = Vec::new();
    for ev in job.rx.iter() {
        match ev {
            Event::Translated(v) => tr.extend(v),
            Event::Progress { done, total } => eprintln!("[{:>6.1}s] {done}/{total}", t.elapsed().as_secs_f32()),
            Event::Waiting { seconds, message } => eprintln!("waiting {seconds}s: {message}"),
            Event::Failed(v) => eprintln!("failed nodes: {v:?}"),
            Event::Stopped(e) => {
                eprintln!("stopped: {e}");
                break;
            }
            Event::Done => break,
        }
    }
    let secs = t.elapsed().as_secs_f32();
    eprintln!("{} translated in {secs:.1}s by {}", tr.len(), eng.label());
    // Side file for comparisons: source and translation per node.
    let map: std::collections::HashMap<usize, &String> = tr.iter().map(|(n, t)| (*n, t)).collect();
    let segs: Vec<serde_json::Value> = plan.segments.iter().map(|s| serde_json::json!({"node": s.node, "src": s.text, "tr": map.get(&s.node)})).collect();
    let json = serde_json::json!({"engine": eng.id(), "label": eng.label(), "seconds": secs, "segments": segs});
    std::fs::write(format!("{}.json", a[3].trim_end_matches(".html")), serde_json::to_string_pretty(&json).unwrap()).unwrap();
    let nodes = plan.nodes();
    let src = |im: &ReflowImage| format!("data:image/png;base64,{}", b64(&im.png));
    let mut html = output::to_html(&d, &output::HtmlOptions { theme: output::Theme::Auto, page_markers: false, image_src: &src, extra_css: "", bilingual: Some(&nodes) });
    for (id, text) in tr {
        let marker = format!("<div class=\"tr pending\" id=\"tr-{id}\" data-tag=\"");
        if let Some(i) = html.find(&marker) {
            let tag_end = html[i + marker.len()..].find('"').unwrap() + i + marker.len();
            let tag = html[i + marker.len()..tag_end].to_string();
            let close = html[tag_end..].find("></div>").unwrap() + tag_end;
            let (name, class) = tag.split_once('.').map(|(n, c)| (n.to_string(), format!(" class=\"{c}\""))).unwrap_or((tag.clone(), String::new()));
            let cell = format!("<div class=\"tr\" id=\"tr-{id}\"><{name}{class}>{}</{name}></div>", esc(&text));
            html.replace_range(i..close + "></div>".len(), &cell);
        }
    }
    std::fs::write(&a[3], html).unwrap();
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(data.len() * 4 / 3 + 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= c.len() { o.push(T[(n >> (18 - 6 * k) & 63) as usize] as char) } else { o.push('=') }
        }
    }
    o
}
