//! annot_test <copy.pdf>: add annotations, undo/redo, save incrementally and as a new file.
use std::sync::Arc;
use strata_core::annot::{AnnotKind, AnnotSpec};
use strata_core::{AnnotOp, Document, Pending, QuadF, RectF, SaveOptions};

fn wait<T: Clone>(mut p: Pending<T>) -> T {
    loop {
        if let Some(r) = p.poll() { return r.clone().expect("request failed"); }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn main() {
    strata_core::fonts::install();
    let path = std::env::args().nth(1).unwrap();
    let doc = Document::open(path.as_ref(), None, Arc::new(|| {})).unwrap();
    let mut hl = AnnotSpec::new(AnnotKind::Highlight, [1.0, 0.9, 0.0]);
    hl.quads = vec![QuadF { ul: [80.0, 100.0], ur: [300.0, 100.0], ll: [80.0, 112.0], lr: [300.0, 112.0] }];
    let mut sq = AnnotSpec::new(AnnotKind::Square, [0.9, 0.1, 0.1]);
    sq.rect = RectF { x0: 100.0, y0: 200.0, x1: 250.0, y1: 280.0 };
    let mut ft = AnnotSpec::new(AnnotKind::FreeText, [0.1, 0.2, 0.8]);
    ft.rect = RectF { x0: 100.0, y0: 300.0, x1: 400.0, y1: 340.0 };
    ft.contents = "日本語のメモ（テスト）".into();
    let mut ln = AnnotSpec::new(AnnotKind::Line, [0.0, 0.5, 0.0]);
    ln.line = Some(([100.0, 400.0], [300.0, 450.0])); ln.arrow = true;
    let mut ink = AnnotSpec::new(AnnotKind::Ink, [0.5, 0.0, 0.5]);
    ink.ink = vec![(0..20).map(|i| [100.0 + i as f32 * 5.0, 500.0 + ((i as f32) * 0.5).sin() * 20.0]).collect()];
    let mut note = AnnotSpec::new(AnnotKind::Text, [1.0, 0.8, 0.0]);
    note.rect = RectF { x0: 500.0, y0: 100.0, x1: 520.0, y1: 120.0 }; note.contents = "付箋の内容".into();
    let mut ids = Vec::new();
    for s in [hl, sq, ft, ln, ink, note] {
        let r = wait(doc.edit(AnnotOp::Create { page: 0, spec: s }));
        ids.push(r.id.unwrap());
    }
    println!("created {:?}, rev={}, dirty={}", ids, doc.page_rev(0), doc.is_dirty());
    let list = wait(doc.annotations(0));
    for a in list.iter() { println!("  {} {:?} bounds={:?} contents={:?}", a.id, a.spec.kind, a.bounds, a.spec.contents); }
    // modify square color, delete ink, then undo both, redo one
    let mut sq2 = list.iter().find(|a| a.spec.kind == AnnotKind::Square).unwrap().spec.clone();
    sq2.color = [0.0, 0.0, 1.0]; sq2.translate(20.0, 20.0);
    wait(doc.edit(AnnotOp::Modify { page: 0, id: ids[1], spec: sq2 }));
    wait(doc.edit(AnnotOp::Delete { page: 0, id: ids[4] }));
    println!("after modify+delete: {} annots", wait(doc.annotations(0)).len());
    wait(doc.edit(AnnotOp::Undo)); wait(doc.edit(AnnotOp::Undo));
    let l = wait(doc.annotations(0));
    println!("after 2 undo: {} annots, square color {:?}", l.len(), l.iter().find(|a| a.spec.kind == AnnotKind::Square).map(|a| a.spec.color));
    wait(doc.edit(AnnotOp::Redo));
    println!("after redo: {} annots, can_undo={} can_redo={}", wait(doc.annotations(0)).len(), doc.can_undo(), doc.can_redo());
    let t = std::time::Instant::now();
    wait(doc.save(SaveOptions { path: path.clone().into(), incremental: true, decrypt: false }));
    println!("incremental save ok in {:?}, dirty={}", t.elapsed(), doc.is_dirty());
    let out = path.replace(".pdf", "_full.pdf");
    wait(doc.save(SaveOptions { path: out.into(), incremental: false, decrypt: true }));
    println!("full save ok");
}
