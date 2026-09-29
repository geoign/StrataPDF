//! layout_page <file> [page]: layout analysis of each page (or one page), with
//! timings and the regions found. `-q`: timings only.
use std::time::Instant;

use strata_core::layout::analyze_page;
use strata_ocr::layout::{LayoutModel, CLASSES};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "-q").collect();
    let quiet = std::env::args().any(|a| a == "-q");
    let t0 = Instant::now();
    let model = LayoutModel::load().expect("model");
    println!("model loaded in {:?}", t0.elapsed());
    let doc = mupdf::Document::open(&args[0]).unwrap();
    let pages: Vec<i32> = match args.get(1) {
        Some(p) => vec![p.parse::<i32>().unwrap() - 1],
        None => (0..doc.page_count().unwrap()).collect(),
    };
    let t_all = Instant::now();
    for p in pages {
        let page = doc.load_page(p).unwrap();
        let tn = Instant::now();
        let nodes = strata_core::layout::page_nodes(&page).unwrap();
        let t_nodes = tn.elapsed();
        drop(nodes);
        let t = Instant::now();
        let a = analyze_page(&model, &page).unwrap();
        print!("[nodes+features {t_nodes:?}] ");
        println!("page {}: {} lines, {} regions, {:?}", p + 1, a.nodes.len(), a.layout.groups.len(), t.elapsed());
        if quiet {
            continue;
        }
        for g in &a.layout.groups {
            let text: String = g.nodes.iter().map(|&i| a.nodes[i].text.as_str()).collect::<Vec<_>>().join(" ");
            println!("  {:<15} {:>6.1} {:>6.1} {:>6.1} {:>6.1}  {}", CLASSES[g.class], g.bbox[0], g.bbox[1], g.bbox[2], g.bbox[3], text.chars().take(70).collect::<String>());
        }
    }
    println!("total {:?}", t_all.elapsed());
    let gray = vec![0.5f32; 300 * 300];
    let t = Instant::now();
    for _ in 0..5 {
        model.page_maps(&gray).unwrap();
    }
    println!("CNN {:?} per page", t.elapsed() / 5);
}
