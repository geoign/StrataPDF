//! layout_golden <dir>: compare the layout port with reference dumps of PyMuPDF
//! Layout (`<pdf stem>_p<page>.json`, made by a Python script from the
//! intermediate values of `pymupdf.layout`), stage by stage. The PDF of each
//! dump is looked up in the directories given by `LAYOUT_PDF_DIRS` (`;`-separated).
use std::path::{Path, PathBuf};

use serde_json::Value;
use strata_core::layout::page_nodes;
use strata_ocr::layout::{self, LayoutModel, LayoutNode, CLASSES};

fn floats(v: &Value) -> Vec<f32> {
    v.as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(f64::NAN) as f32).collect()).unwrap_or_default()
}

fn rows(v: &Value) -> Vec<Vec<f32>> {
    v.as_array().map(|a| a.iter().map(floats).collect()).unwrap_or_default()
}

/// Largest absolute difference, and the share of values off by more than `tol`.
fn diff(a: &[Vec<f32>], b: &[Vec<f32>], tol: f32) -> (f32, f32) {
    let mut max = 0f32;
    let (mut bad, mut n) = (0usize, 0usize);
    for (x, y) in a.iter().zip(b) {
        for (p, q) in x.iter().zip(y) {
            let d = (p - q).abs();
            max = max.max(d);
            bad += (d > tol) as usize;
            n += 1;
        }
    }
    (max, if n > 0 { bad as f32 / n as f32 } else { 0.0 })
}

fn find_pdf(stem: &str) -> Option<PathBuf> {
    let dirs = std::env::var("LAYOUT_PDF_DIRS").unwrap_or_default();
    dirs.split(';').filter(|d| !d.is_empty()).map(|d| Path::new(d).join(format!("{stem}.pdf"))).find(|p| p.exists())
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: layout_golden <dir>");
    let model = LayoutModel::load().expect("model");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    files.sort();
    for f in files {
        let g: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let name = f.file_stem().unwrap().to_string_lossy().into_owned();
        let (stem, page) = name.rsplit_once("_p").unwrap();
        let page: i32 = page.parse::<i32>().unwrap() - 1;
        let Some(pdf) = find_pdf(stem) else {
            println!("{name}: PDF not found");
            continue;
        };
        let doc = mupdf::Document::open(pdf.to_str().unwrap()).unwrap();
        let pg = doc.load_page(page).unwrap();

        // 1. Lines.
        let gb = rows(&g["bboxes"]);
        let gt: Vec<String> = g["text"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
        let grf = rows(&g["rf"]);
        let ours = page_nodes(&pg).unwrap();
        let mut matched = 0;
        let mut text_diff = 0;
        let mut rf_ours = Vec::new();
        let mut rf_ref = Vec::new();
        for (i, b) in gb.iter().enumerate() {
            if let Some(n) = ours.iter().find(|n| n.bbox.iter().zip(b).all(|(p, q)| (p - q).abs() < 0.01)) {
                matched += 1;
                if n.text != gt[i] {
                    text_diff += 1;
                    if text_diff <= 3 {
                        println!("    text: ours {:?} ref {:?}", n.text, gt[i]);
                    }
                }
                rf_ours.push(n.rf.clone());
                rf_ref.push(grf[i][6..].to_vec());
            }
        }
        if std::env::var("SHOW_UNMATCHED").is_ok() {
            for (i, b) in gb.iter().enumerate() {
                if !ours.iter().any(|n| n.bbox.iter().zip(b).all(|(p, q)| (p - q).abs() < 0.01)) {
                    println!("    ref only  {:?} {:?}", b, gt[i].chars().take(40).collect::<String>());
                }
            }
            for n in &ours {
                if !gb.iter().any(|b| n.bbox.iter().zip(b).all(|(p, q)| (p - q).abs() < 0.01)) {
                    println!("    ours only {:?} {:?}", n.bbox, n.text.chars().take(40).collect::<String>());
                }
            }
        }
        let (rf_max, rf_bad) = diff(&rf_ours, &rf_ref, 1e-3);
        // Which features differ most.
        let mut worst: Vec<(f32, &str)> = (0..layout::RF_C)
            .map(|k| (rf_ours.iter().zip(&rf_ref).map(|(a, b)| (a[k] - b[k]).abs()).fold(0f32, f32::max), layout::RF_NAMES[k + 6]))
            .filter(|(d, _)| *d > 1e-3)
            .collect();
        worst.sort_by(|a, b| b.0.total_cmp(&a.0));
        println!(
            "{name}: lines ours {} ref {} matched {matched} text-diff {text_diff}; rf max {rf_max:.4} bad {:.1}% {:?}",
            ours.len(),
            gb.len(),
            rf_bad * 100.0,
            worst.iter().take(4).map(|(d, n)| format!("{n}={d:.2}")).collect::<Vec<_>>()
        );

        // 2. Graph inputs from the reference nodes.
        let ref_nodes: Vec<LayoutNode> = gb.iter().zip(&gt).zip(&grf).map(|((b, t), r)| LayoutNode { bbox: [b[0], b[1], b[2], b[3]], text: t.clone(), rf: r[6..].to_vec() }).collect();
        let refs: Vec<&LayoutNode> = ref_nodes.iter().collect();
        let boxes: Vec<[f32; 4]> = ref_nodes.iter().map(|n| n.bbox).collect();
        let x: Vec<Vec<f32>> = layout::boxes_transform(&boxes).iter().map(|r| r.to_vec()).collect();
        let edges = if boxes.len() > 1 { layout::directional_edges(&boxes) } else { vec![(0, 0)] };
        let mut gedges: Vec<(usize, usize)> = g["edge_index"].as_array().unwrap().iter().map(|e| (e[0].as_u64().unwrap() as usize, e[1].as_u64().unwrap() as usize)).collect();
        gedges.sort();
        let ea: Vec<Vec<f32>> = layout::edge_features(&boxes, &gedges).iter().map(|r| r.to_vec()).collect();
        // Reference edge attributes, reordered to the sorted edges.
        let gea_raw = rows(&g["edge_attr"]);
        let gidx: Vec<(usize, usize)> = g["edge_index"].as_array().unwrap().iter().map(|e| (e[0].as_u64().unwrap() as usize, e[1].as_u64().unwrap() as usize)).collect();
        let gea: Vec<Vec<f32>> = gedges.iter().map(|e| gea_raw[gidx.iter().position(|x| x == e).unwrap()].clone()).collect();
        let tp: Vec<Vec<f32>> = ref_nodes.iter().map(|n| layout::text_pattern(&n.text).to_vec()).collect();
        println!(
            "    x {:?}  edges ours {} ref {} same {}  edge_attr {:?}  text_pattern {:?}",
            diff(&x, &rows(&g["x"]), 1e-5).0,
            edges.len(),
            gedges.len(),
            edges == gedges,
            diff(&ea, &gea, 1e-3),
            diff(&tp, &rows(&g["text_patterns"]), 1e-6)
        );

        // 3. Image features on our rendering.
        let pix = pg.to_pixmap(&mupdf::Matrix::IDENTITY, &mupdf::Colorspace::device_rgb(), false, true).unwrap();
        let (w, h, n, stride) = (pix.width() as usize, pix.height() as usize, pix.n() as usize, pix.stride() as usize);
        let mut rgb = Vec::new();
        for y in 0..h {
            for xx in 0..w {
                rgb.extend_from_slice(&pix.samples()[y * stride + xx * n..y * stride + xx * n + 3]);
            }
        }
        let gray = layout::page_gray(&rgb, w, h);
        let gsum: f64 = gray.iter().map(|&v| v as f64).sum();
        let (feat, logits) = model.page_maps(&gray).unwrap();
        let imf = layout::roi_features(&feat, &logits, &boxes, w, h);
        println!(
            "    image {w}x{h} (ref {:?})  gray sum {gsum:.1} (ref {:.1})  image_features {:?}",
            g["image_shape"],
            g["gray_sum"].as_f64().unwrap(),
            diff(&imf, &rows(&g["image_features"]), 1e-3)
        );

        // 4. Network output on our inputs from the reference nodes.
        let input = layout::graph_input(&refs, &feat, &logits, w, h);
        let (nl, _) = model.run_graph(&input).unwrap();
        let glab: Vec<String> = g["labels"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect();
        let agree = nl.iter().zip(&glab).filter(|(l, r)| CLASSES[l.iter().enumerate().fold((0, f32::MIN), |a, (i, &v)| if v > a.1 { (i, v) } else { a }).0] == r.as_str()).count();
        println!("    labels agree {agree}/{} (node_logits {:?})", glab.len(), diff(&nl, &rows(&g["node_logits"]), 0.05));

        // 5. The whole pipeline on our own lines and features.
        let res = model.analyze(&rgb, w, h, &ours).unwrap();
        let (mut same, mut total) = (0, 0);
        for (i, n) in ours.iter().enumerate() {
            let Some((c, _)) = res.node_class[i] else { continue };
            if let Some(k) = gb.iter().position(|b| n.bbox.iter().zip(b).all(|(p, q)| (p - q).abs() < 0.01)) {
                total += 1;
                if CLASSES[c] == glab[k] {
                    same += 1;
                } else {
                    println!("      {} vs ref {}: {}", CLASSES[c], glab[k], n.text.chars().take(50).collect::<String>());
                }
            }
        }
        println!("    end to end: labels agree {same}/{total}");
    }
}
