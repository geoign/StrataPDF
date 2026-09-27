//! reflow_export <file> <out_dir>: write out.md, out.html and images/.
use std::sync::{Arc, atomic::AtomicBool};
use strata_core::reflow::{output, ReflowEvent, ReflowOptions};
use strata_core::Document;

fn main() {
    strata_core::fonts::install();
    let path = std::env::args().nth(1).unwrap();
    let out = std::path::PathBuf::from(std::env::args().nth(2).unwrap());
    std::fs::create_dir_all(out.join("images")).unwrap();
    let doc = Document::open(path.as_ref(), std::env::var("STRATA_PW").ok(), Arc::new(|| {})).unwrap();
    let t = std::time::Instant::now();
    let rx = doc.reflow(ReflowOptions::default(), Arc::new(AtomicBool::new(false)));
    for ev in rx {
        match ev {
            ReflowEvent::Progress { .. } => {}
            ReflowEvent::Error(e) => { eprintln!("error {e}"); return; }
            ReflowEvent::Done(d) => {
                eprintln!("reflow {:?}: {} nodes, {} images, vertical={}, title={:?}", t.elapsed(), d.nodes.len(), d.images.len(), d.vertical, d.title);
                for im in &d.images { std::fs::write(out.join("images").join(&im.id), &im.png).unwrap(); }
                let md = output::to_markdown(&d, &|im| format!("images/{}", im.id));
                std::fs::write(out.join("out.md"), md).unwrap();
                let src = |im: &strata_core::reflow::ReflowImage| format!("images/{}", im.id);
                let html = output::to_html(&d, &output::HtmlOptions { theme: output::Theme::Auto, page_markers: true, image_src: &src, extra_css: "" });
                std::fs::write(out.join("out.html"), html).unwrap();
                return;
            }
        }
    }
}
