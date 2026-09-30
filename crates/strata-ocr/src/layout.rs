//! Page layout analysis: a Rust port of the inference of PyMuPDF Layout
//! (pymupdf-layout 1.28.2, Artifex, AGPL-3.0; model `layout_rf2.4.1+imf1`).
//!
//! The nodes are the text lines of a page. Each carries the region features of
//! `features.c` (computed by the caller, which owns the MuPDF page), its box and
//! a coarse pattern of its text. A small CNN over a 300×300 grey rendering of the
//! page adds image features pooled over each box. A graph network over the
//! nearest neighbours in four directions labels the lines (DocLayNet classes)
//! and predicts which neighbours belong together; connected lines form the
//! regions.
//!
//! The arithmetic follows the Python reference closely (float32 where it used
//! float32), since the network was trained on exactly these inputs.

use ndarray::{Array0, Array1, Array2, Array4};
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;
use parking_lot::Mutex;

use crate::OcrError;

/// DocLayNet classes, in the order of the network's output.
pub const CLASSES: [&str; 11] = ["text", "title", "picture", "table", "list-item", "page-header", "page-footer", "section-header", "footnote", "caption", "formula"];

pub const TEXT: usize = 0;
pub const TITLE: usize = 1;
pub const PICTURE: usize = 2;
pub const TABLE: usize = 3;
pub const LIST_ITEM: usize = 4;
pub const PAGE_HEADER: usize = 5;
pub const PAGE_FOOTER: usize = 6;
pub const SECTION_HEADER: usize = 7;
pub const FOOTNOTE: usize = 8;
pub const CAPTION: usize = 9;
pub const FORMULA: usize = 10;

/// Tie-break between classes with equal votes in a region: earlier wins.
const CLASS_PRIORITY: [usize; 12] = [1, 5, 6, 4, 0, 3, 2, 7, 8, 9, 10, 11];

/// Feature names of the network, in input order. The first six are computed
/// here, the rest by `features.c` (see `vendor/pymupdf_layout/strata_features.c`).
pub const RF_NAMES: [&str; 118] = [
    "num_ratio", "is_text", "is_vector", "is_image", "is_hline_vector", "is_vline_vector", "alignment_down_with_centre", "alignment_down_with_left",
    "alignment_down_with_right", "alignment_left_with_baseline", "alignment_left_with_bottom", "alignment_left_with_middle", "alignment_left_with_top",
    "alignment_right_with_baseline", "alignment_right_with_bottom", "alignment_right_with_middle", "alignment_right_with_top", "alignment_up_with_centre",
    "alignment_up_with_left", "alignment_up_with_right", "bottom_right_x", "bottommost_baseline", "centre", "char_area", "char_space",
    "consecutive_baseline_alignment_count_left", "consecutive_baseline_alignment_count_right", "consecutive_bottom_alignment_count_left",
    "consecutive_bottom_alignment_count_right", "consecutive_centre_alignment_count_down", "consecutive_centre_alignment_count_up",
    "consecutive_left_alignment_count_down", "consecutive_left_alignment_count_up", "consecutive_middle_alignment_count_left",
    "consecutive_middle_alignment_count_right", "consecutive_right_alignment_count_down", "consecutive_right_alignment_count_up",
    "consecutive_top_alignment_count_left", "consecutive_top_alignment_count_right", "contains_image", "contains_vector", "context_above_font_size",
    "context_above_indent", "context_above_is_header", "context_above_outdent", "context_below_bullet", "context_below_font_size", "context_below_indent",
    "context_below_is_header", "context_below_outdent", "context_header_differs", "dodgy_paragraph_breaks", "font_size", "font_size_median",
    "font_size_mode", "fonts_offset", "imargin_b", "imargin_l", "imargin_r", "imargin_t", "inner_b", "inner_l", "inner_r", "inner_t", "invisible",
    "is_header", "line_bullets", "line_space", "line_within_block", "linespaces_offset", "margin_b", "margin_l", "margin_r", "margin_t",
    "max_non_first_left_indent", "max_non_last_right_indent", "middle", "nearest_nonaligned_down_centre", "nearest_nonaligned_down_left",
    "nearest_nonaligned_down_right", "nearest_nonaligned_left_baseline", "nearest_nonaligned_left_bottom", "nearest_nonaligned_left_middle",
    "nearest_nonaligned_left_top", "nearest_nonaligned_right_baseline", "nearest_nonaligned_right_bottom", "nearest_nonaligned_right_middle",
    "nearest_nonaligned_right_top", "nearest_nonaligned_up_centre", "nearest_nonaligned_up_left", "nearest_nonaligned_up_right", "non_line_bullets",
    "num_fonts_in_region", "num_lines", "num_lines_in_block", "num_non_numerals", "num_numerals", "num_underlines", "numeral_ratio", "raft_edge_down",
    "raft_edge_left", "raft_edge_right", "raft_edge_up", "raft_num", "ratio", "ray_line_distance_down", "ray_line_distance_left",
    "ray_line_distance_right", "ray_line_distance_up", "segment", "smargin_b", "smargin_l", "smargin_r", "smargin_t", "table_element", "table_num",
    "top_left_x", "topmost_baseline",
];

/// Number of features computed by `features.c`.
pub const RF_C: usize = 112;
const RF_LOCAL: usize = RF_NAMES.len() - RF_C;
const IMG: usize = 300;
const FEAT_CH: usize = 160;
const LOGIT_CH: usize = 12;
const IMAGE_DIM: usize = 2 * FEAT_CH + 3 * LOGIT_CH + 2;
const EDGE_DIM: usize = 24;
const TEXT_PATTERN_LEN: usize = 40;
const EDGE_THRESHOLD: f32 = 0.55;

fn c_feature(name: &str) -> usize {
    RF_NAMES.iter().position(|n| *n == name).expect("feature name") - RF_LOCAL
}

/// A text line of the page.
#[derive(Clone, Debug)]
pub struct LayoutNode {
    /// x0, y0, x1, y1 in page points.
    pub bbox: [f32; 4],
    pub text: String,
    /// The features from `features.c`, in [`RF_NAMES`] order (without the first six).
    pub rf: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct LayoutGroup {
    pub bbox: [f32; 4],
    /// Index into [`CLASSES`].
    pub class: usize,
    /// Indices into the nodes passed to [`LayoutModel::analyze`].
    pub nodes: Vec<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct PageLayout {
    /// Per node passed in: class and probability, `None` for invisible text that was dropped.
    pub node_class: Vec<Option<(usize, f32)>>,
    pub groups: Vec<LayoutGroup>,
}

/// Network inputs of one page (exposed for testing against the reference).
#[derive(Clone, Debug, Default)]
pub struct GraphInput {
    pub x: Vec<[f32; 8]>,
    pub edges: Vec<(usize, usize)>,
    pub edge_attr: Vec<[f32; EDGE_DIM]>,
    pub rf: Vec<Vec<f32>>,
    pub text_patterns: Vec<[f32; TEXT_PATTERN_LEN]>,
    pub image_features: Vec<Vec<f32>>,
}

/// Node logits and edge logits, one row per node or edge.
pub type Logits = (Vec<Vec<f32>>, Vec<Vec<f32>>);

pub struct LayoutModel {
    cnn: Vec<Mutex<Session>>,
    gnn: Vec<Mutex<Session>>,
    /// Which session the next caller tries first.
    next: std::sync::atomic::AtomicUsize,
}

static CNN_ONNX: &[u8] = include_bytes!("../models/feature_imf1.onnx");
static GNN_ONNX: &[u8] = include_bytes!("../models/layout_rf2.4.1_imf1.onnx");

impl LayoutModel {
    /// The models are embedded in the executable; they run on the CPU.
    ///
    /// Several sessions with a few threads each, so that pages are analysed side
    /// by side: the networks are small, and one session on every core ran them
    /// little faster than on four.
    pub fn load() -> Result<LayoutModel, OcrError> {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let sessions = (cores / 4).clamp(1, 6);
        let threads = (cores / sessions).max(1);
        let build = |bytes: &[u8]| -> Result<Session, OcrError> {
            Ok(Session::builder()?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|e| OcrError::Inference(e.to_string()))?
                .with_intra_threads(threads)
                .map_err(|e| OcrError::Inference(e.to_string()))?
                .commit_from_memory(bytes)?)
        };
        let pool = |bytes: &[u8]| -> Result<Vec<Mutex<Session>>, OcrError> { (0..sessions).map(|_| build(bytes).map(Mutex::new)).collect() };
        Ok(LayoutModel { cnn: pool(CNN_ONNX)?, gnn: pool(GNN_ONNX)?, next: Default::default() })
    }

    /// A free session of the pool, or (all busy) the next one in turn.
    fn session<'a>(&self, pool: &'a [Mutex<Session>]) -> parking_lot::MutexGuard<'a, Session> {
        let start = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (0..pool.len()).find_map(|k| pool[(start + k) % pool.len()].try_lock()).unwrap_or_else(|| pool[start % pool.len()].lock())
    }

    /// Classify the lines of a page. `rgb` is the page rendered at 72 dpi
    /// (one pixel per point, 3 bytes per pixel, no padding).
    pub fn analyze(&self, rgb: &[u8], width: usize, height: usize, nodes: &[LayoutNode]) -> Result<PageLayout, OcrError> {
        let mut out = PageLayout { node_class: vec![None; nodes.len()], groups: Vec::new() };
        let kept = visible_nodes(nodes);
        if kept.is_empty() {
            return Ok(out);
        }
        let (feat, logits) = self.page_maps(&page_gray(rgb, width, height))?;
        let sel: Vec<&LayoutNode> = kept.iter().map(|&i| &nodes[i]).collect();
        let input = graph_input(&sel, &feat, &logits, width, height);
        let (node_logits, edge_logits) = self.run_graph(&input)?;
        let n = sel.len();
        let mut labels = Vec::with_capacity(n);
        for (k, l) in node_logits.iter().enumerate() {
            let p = softmax(l);
            let (c, s) = p.iter().enumerate().fold((0, f32::MIN), |a, (i, &v)| if v > a.1 { (i, v) } else { a });
            labels.push(c);
            out.node_class[kept[k]] = Some((c, s));
        }
        let linked: Vec<(usize, usize)> = input.edges.iter().zip(&edge_logits).filter(|(_, l)| softmax(&l[..])[1] > EDGE_THRESHOLD).map(|(e, _)| *e).collect();
        let boxes: Vec<[f32; 4]> = sel.iter().map(|n| n.bbox).collect();
        let mut groups = group_nodes(&labels, &linked, &boxes);
        for g in &mut groups {
            for i in &mut g.nodes {
                *i = kept[*i];
            }
        }
        out.groups = merge_overlapping_pictures(groups);
        Ok(out)
    }

    /// The CNN's feature map (160 channels) and class logits (12) over the 300×300 grey page.
    pub fn page_maps(&self, gray: &[f32]) -> Result<(Vec<f32>, Vec<f32>), OcrError> {
        let arr = Array4::from_shape_vec((1, 1, IMG, IMG), gray.to_vec()).map_err(|e| OcrError::Inference(e.to_string()))?;
        let mut s = self.session(&self.cnn);
        let out = s.run(ort::inputs!["input" => Tensor::from_array(arr)?])?;
        let feat: Vec<f32> = out["combined"].try_extract_array::<f32>()?.iter().copied().collect();
        let logits: Vec<f32> = out["logits"].try_extract_array::<f32>()?.iter().copied().collect();
        if feat.len() != FEAT_CH * IMG * IMG || logits.len() != LOGIT_CH * IMG * IMG {
            return Err(OcrError::Inference("unexpected image feature shape".into()));
        }
        Ok((feat, logits))
    }

    /// Node logits (11 classes) and edge logits (2) of the graph network.
    pub fn run_graph(&self, g: &GraphInput) -> Result<Logits, OcrError> {
        let n = g.x.len();
        let e = g.edges.len();
        let flat = |rows: &[Vec<f32>], w: usize| -> Array2<f32> { Array2::from_shape_fn((rows.len(), w), |(i, j)| rows[i][j]) };
        let x = Array2::from_shape_fn((n, 8), |(i, j)| g.x[i][j]);
        let ei = Array2::from_shape_fn((2, e), |(r, k)| if r == 0 { g.edges[k].0 as i64 } else { g.edges[k].1 as i64 });
        let ea = Array2::from_shape_fn((e, EDGE_DIM), |(i, j)| g.edge_attr[i][j]);
        let tp = Array2::from_shape_fn((n, TEXT_PATTERN_LEN), |(i, j)| g.text_patterns[i][j]);
        let k = Array0::from_elem((), n.min(20) as i64);
        let batch = Array1::<i64>::zeros(n);
        let mut s = self.session(&self.gnn);
        let out = s.run(ort::inputs![
            "x" => Tensor::from_array(x)?,
            "edge_index" => Tensor::from_array(ei)?,
            "edge_attr" => Tensor::from_array(ea)?,
            "k" => Tensor::from_array(k)?,
            "batch" => Tensor::from_array(batch)?,
            "rf_features" => Tensor::from_array(flat(&g.rf, RF_NAMES.len()))?,
            "text_patterns" => Tensor::from_array(tp)?,
            "image_features" => Tensor::from_array(flat(&g.image_features, IMAGE_DIM))?,
        ])?;
        let rows = |name: &str, w: usize| -> Result<Vec<Vec<f32>>, OcrError> {
            let a = out[name].try_extract_array::<f32>()?;
            let v: Vec<f32> = a.iter().copied().collect();
            Ok(v.chunks(w).map(|c| c.to_vec()).collect())
        };
        Ok((rows("node_logits", CLASSES.len())?, rows("edge_logits", 2)?))
    }
}

/// Drop invisible text (white on white) when the page also has visible text.
fn visible_nodes(nodes: &[LayoutNode]) -> Vec<usize> {
    let (inv, num, non) = (c_feature("invisible"), c_feature("num_numerals"), c_feature("num_non_numerals"));
    let visible_text = nodes.iter().any(|n| n.rf[num] + n.rf[non] > 0.0 && n.rf[inv] == 0.0);
    (0..nodes.len()).filter(|&i| !visible_text || nodes[i].rf[inv] != 1.0).collect()
}

fn softmax(v: &[f32]) -> Vec<f32> {
    let m = v.iter().copied().fold(f32::MIN, f32::max);
    let e: Vec<f32> = v.iter().map(|x| (x - m).exp()).collect();
    let s: f32 = e.iter().sum();
    e.iter().map(|x| x / s).collect()
}

/// All network inputs of a page.
pub fn graph_input(nodes: &[&LayoutNode], feat: &[f32], logits: &[f32], width: usize, height: usize) -> GraphInput {
    let boxes: Vec<[f32; 4]> = nodes.iter().map(|n| n.bbox).collect();
    let (edges, edge_attr) = if boxes.len() == 1 {
        (vec![(0, 0)], vec![[0.0; EDGE_DIM]])
    } else {
        let e = directional_edges(&boxes);
        let a = edge_features(&boxes, &e);
        (e, a)
    };
    let rf = nodes
        .iter()
        .map(|n| {
            let mut v = Vec::with_capacity(RF_NAMES.len());
            // num_ratio, is_text, is_vector, is_image, is_hline_vector, is_vline_vector
            v.extend([num_ratio(&n.text), 1.0, 0.0, 0.0, 0.0, 0.0]);
            v.extend_from_slice(&n.rf);
            v
        })
        .collect();
    GraphInput {
        x: boxes_transform(&boxes),
        edges,
        edge_attr,
        rf,
        text_patterns: nodes.iter().map(|n| text_pattern(&n.text)).collect(),
        image_features: roi_features(feat, logits, &boxes, width, height),
    }
}

/// Python's `str.isdigit()` for the characters that occur in documents.
fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
        || matches!(c, '²' | '³' | '¹' | '⁰' | '⁴'..='⁹' | '₀'..='₉' | '０'..='９')
        || (c.is_numeric() && !c.is_alphabetic() && c.to_digit(10).is_none() && unicode_decimal(c))
}

/// Decimal digits of other scripts (Unicode Nd), by their blocks' digit runs.
fn unicode_decimal(c: char) -> bool {
    const ZEROS: [u32; 22] = [
        0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66, 0xDE6, 0xE50, 0xED0, 0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0xFF10,
        0x1D7CE,
    ];
    let u = c as u32;
    ZEROS.iter().any(|&z| u >= z && u < z + 10)
}

fn num_ratio(text: &str) -> f32 {
    let n = text.chars().count();
    if n == 0 {
        return 0.0;
    }
    (text.chars().filter(|&c| is_digit(c)).count() as f64 / n as f64) as f32
}

const SYMBOLS: [char; 19] = ['•', '-', '*', '+', '<', '>', '(', ')', '→', '✓', '#', '□', '■', '‣', '◦', '▪', '.', ':', '※'];

/// Run-length pattern of character classes, octal-coded into 40 values.
pub fn text_pattern(text: &str) -> [f32; TEXT_PATTERN_LEN] {
    let pattern: Vec<char> = text
        .chars()
        .map(|c| {
            if SYMBOLS.contains(&c) {
                c
            } else if c.is_whitespace() {
                'W'
            } else if is_digit(c) {
                'D'
            } else if !c.is_alphanumeric() {
                'S'
            } else {
                'C'
            }
        })
        .collect();
    let mut compressed = String::new();
    let mut i = 0;
    while i < pattern.len() {
        let mut j = i + 1;
        while j < pattern.len() && pattern[j] == pattern[i] {
            j += 1;
        }
        compressed.push(pattern[i]);
        compressed.push_str(&(j - i).to_string());
        i = j;
    }
    let mut out = [0.0f32; TEXT_PATTERN_LEN];
    let mut k = 0;
    'outer: for c in compressed.chars() {
        let v = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'C' => 10,
            'D' => 11,
            'S' => 12,
            'W' => 13,
            _ => 14 + SYMBOLS.iter().position(|&s| s == c).unwrap_or(0) as u32,
        };
        for d in format!("{v:02o}").chars() {
            out[k] = (d as u32 - '0' as u32) as f32 / 8.0;
            k += 1;
            if k >= TEXT_PATTERN_LEN {
                break 'outer;
            }
        }
    }
    out
}

/// Boxes normalised to the extent of all boxes, with centre and size.
pub fn boxes_transform(boxes: &[[f32; 4]]) -> Vec<[f32; 8]> {
    if boxes.is_empty() {
        return Vec::new();
    }
    let min_x = boxes.iter().flat_map(|b| [b[0], b[2]]).fold(f32::INFINITY, f32::min);
    let min_y = boxes.iter().flat_map(|b| [b[1], b[3]]).fold(f32::INFINITY, f32::min);
    let w = (boxes.iter().flat_map(|b| [b[0], b[2]]).fold(f32::NEG_INFINITY, f32::max) - min_x).max(1e-6);
    let h = (boxes.iter().flat_map(|b| [b[1], b[3]]).fold(f32::NEG_INFINITY, f32::max) - min_y).max(1e-6);
    boxes
        .iter()
        .map(|b| {
            let (x0, y0, x1, y1) = ((b[0] - min_x) / w, (b[1] - min_y) / h, (b[2] - min_x) / w, (b[3] - min_y) / h);
            [x0, y0, x1, y1, (x1 + x0) / 2.0, (y1 + y0) / 2.0, x1 - x0, y1 - y0]
        })
        .collect()
}

/// Edges to the nearest neighbour in each direction (and the second nearest
/// vertically), as unordered pairs (i < j), sorted.
pub fn directional_edges(b: &[[f32; 4]]) -> Vec<(usize, usize)> {
    let n = b.len();
    let vg = 0.3f32;
    let cy: Vec<f32> = b.iter().map(|r| (r[1] + r[3]) / 2.0).collect();
    let cx: Vec<f32> = b.iter().map(|r| (r[0] + r[2]) / 2.0).collect();
    let h: Vec<f32> = b.iter().map(|r| r[3] - r[1]).collect();
    // Row i, column j: the reference compares box j's values against box i's.
    let y_flag = |i: usize, j: usize| cy[j] < b[i][1] - h[i] * vg || cy[j] > b[i][3] + h[i] * vg;
    let x_flag = |i: usize, j: usize| cx[j] < b[i][0] || cx[j] > b[i][2];
    let sq = |v: f32| v * v;
    let hor = |i: usize, j: usize| sq(b[j][0] - b[i][2]) + sq(b[j][1] - b[i][1]);
    let ver = |i: usize, j: usize| sq(b[j][0] - b[i][0]) + sq(b[j][1] - b[i][3]);
    let ver_y = |i: usize, j: usize| sq(b[j][1] - b[i][3]);
    let nearest = |i: usize, dist: &dyn Fn(usize, usize) -> f32, skip: &dyn Fn(usize, usize) -> bool, exclude: Option<usize>| -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for j in 0..n {
            if j == i || skip(i, j) || Some(j) == exclude {
                continue;
            }
            let d = dist(i, j);
            if d.is_infinite() {
                continue;
            }
            if best.is_none_or(|(_, bd)| d < bd) {
                best = Some((j, d));
            }
        }
        best.map(|(j, _)| j)
    };
    let right_skip = |i: usize, j: usize| y_flag(i, j) || b[j][2] <= b[i][2];
    let left_skip = |i: usize, j: usize| y_flag(i, j) || b[j][0] >= b[i][0];
    let down_skip = |i: usize, j: usize| b[j][3] <= b[i][3];
    let up_skip = |i: usize, j: usize| b[j][1] >= b[i][1];
    let down_y_skip = |i: usize, j: usize| b[j][3] <= b[i][3] || x_flag(i, j);
    let mut set = std::collections::BTreeSet::new();
    let mut add = |i: usize, j: Option<usize>| {
        if let Some(j) = j {
            set.insert((i.min(j), i.max(j)));
        }
    };
    for i in 0..n {
        add(i, nearest(i, &hor, &left_skip, None));
        add(i, nearest(i, &hor, &right_skip, None));
        let up = nearest(i, &ver, &up_skip, None);
        add(i, up);
        let down = nearest(i, &ver, &down_skip, None);
        add(i, down);
        add(i, nearest(i, &ver_y, &down_y_skip, None));
        if let Some(d) = down {
            add(i, nearest(i, &ver, &down_skip, Some(d)));
        }
        if let Some(u) = up {
            add(i, nearest(i, &ver, &up_skip, Some(u)));
        }
    }
    set.into_iter().filter(|(i, j)| j - i < 50000).collect()
}

/// Relative geometry of the two boxes of each edge and their alignment ("SEO").
pub fn edge_features(b: &[[f32; 4]], edges: &[(usize, usize)]) -> Vec<[f32; EDGE_DIM]> {
    let delta = 1e-10f32;
    let clean = |v: f32| {
        if v.is_nan() {
            0.0
        } else if v == f32::INFINITY {
            1e5
        } else if v == f32::NEG_INFINITY {
            -1e5
        } else {
            v
        }
    };
    edges
        .iter()
        .map(|&(i, j)| {
            let (s, o) = (b[i], b[j]);
            let (sw, sh, ow, oh) = (s[2] - s[0], s[3] - s[1], o[2] - o[0], o[3] - o[1]);
            let r = [s[0].min(o[0]), s[1].min(o[1]), s[2].max(o[2]), s[3].max(o[3])];
            let (rw, rh) = (r[2] - r[0], r[3] - r[1]);
            let al = |v: f32| if v.abs() <= 1e-5 { 1.0 } else { 0.0 };
            let f = [
                (s[0] - o[0]) / (sw + delta),
                (s[1] - o[1]) / (sh + delta),
                (o[0] - s[0]) / (ow + delta),
                (o[1] - s[1]) / (oh + delta),
                (sw / (ow + delta)).ln(),
                (sh / (oh + delta)).ln(),
                (s[0] - r[0]) / (sw + delta),
                (s[1] - r[1]) / (sh + delta),
                (r[0] - s[0]) / (rw + delta),
                (r[1] - s[1]) / (rh + delta),
                (sw / (rw + delta)).ln(),
                (sh / (rh + delta)).ln(),
                (o[0] - r[0]) / (ow + delta),
                (o[1] - r[1]) / (oh + delta),
                (r[0] - o[0]) / (rw + delta),
                (r[1] - o[1]) / (rh + delta),
                (ow / (rw + delta)).ln(),
                (oh / (rh + delta)).ln(),
                al(s[0] - o[0]),
                al((s[0] + s[2]) / 2.0 - (o[0] + o[2]) / 2.0),
                al(s[2] - o[2]),
                al(s[1] - o[1]),
                al((s[1] + s[3]) / 2.0 - (o[1] + o[3]) / 2.0),
                al(s[3] - o[3]),
            ];
            f.map(clean)
        })
        .collect()
}

/// The page resized to 300×300 (bilinear, as the reference's numpy code) and
/// converted to grey with the reference's channel weights, scaled to 0..1.
pub fn page_gray(rgb: &[u8], w: usize, h: usize) -> Vec<f32> {
    let axis = |n: usize, len: usize| -> Vec<(usize, usize, f64)> {
        (0..n)
            .map(|o| {
                let x = (o as f64 + 0.5) * (len as f64 / n as f64) - 0.5;
                let x0 = x.floor();
                let d = x - x0;
                let x0 = x0 as i64;
                ((x0.clamp(0, len as i64 - 1)) as usize, ((x0 + 1).clamp(0, len as i64 - 1)) as usize, d)
            })
            .collect()
    };
    let xs = axis(IMG, w);
    let ys = axis(IMG, h);
    let mut gray = vec![0u8; IMG * IMG];
    for (oy, &(y0, y1, dy)) in ys.iter().enumerate() {
        for (ox, &(x0, x1, dx)) in xs.iter().enumerate() {
            let px = |x: usize, y: usize, c: usize| rgb[(y * w + x) * 3 + c] as f64;
            let mut ch = [0u8; 3];
            for (c, v) in ch.iter_mut().enumerate() {
                let s = (1.0 - dx) * (1.0 - dy) * px(x0, y0, c) + dx * (1.0 - dy) * px(x1, y0, c) + (1.0 - dx) * dy * px(x0, y1, c) + dx * dy * px(x1, y1, c);
                *v = s.clamp(0.0, 255.0) as u8;
            }
            // The reference treats the pixmap as BGR: channel 0 gets the blue weight.
            let g = 0.114f32 * ch[0] as f32 + 0.587f32 * ch[1] as f32 + 0.299f32 * ch[2] as f32;
            gray[oy * IMG + ox] = g.clamp(0.0, 255.0) as u8;
        }
    }
    let lo = *gray.iter().min().unwrap() as f32;
    let hi = *gray.iter().max().unwrap() as f32;
    if hi > lo { gray.iter().map(|&g| (g as f32 - lo) / (hi - lo)).collect() } else { vec![0.0; IMG * IMG] }
}

/// Per box: mean and max of the feature map, then mean, min and max of the class
/// probabilities, their mean entropy and mean top-2 margin.
pub fn roi_features(feat: &[f32], logits: &[f32], boxes: &[[f32; 4]], width: usize, height: usize) -> Vec<Vec<f32>> {
    let hw = IMG * IMG;
    // Class probabilities per pixel, and their entropy and margin.
    let mut probs = vec![0f32; LOGIT_CH * hw];
    let mut entropy = vec![0f32; hw];
    let mut margin = vec![0f32; hw];
    for p in 0..hw {
        let m = (0..LOGIT_CH).map(|c| logits[c * hw + p]).fold(f32::MIN, f32::max);
        let mut sum = 0f32;
        for c in 0..LOGIT_CH {
            let e = (logits[c * hw + p] - m).exp();
            probs[c * hw + p] = e;
            sum += e;
        }
        let (mut t1, mut t2, mut ent) = (f32::MIN, f32::MIN, 0f32);
        for c in 0..LOGIT_CH {
            let v = probs[c * hw + p] / (sum + 1e-12);
            probs[c * hw + p] = v;
            ent -= v * (v + 1e-12).ln();
            if v > t1 {
                t2 = t1;
                t1 = v;
            } else if v > t2 {
                t2 = v;
            }
        }
        entropy[p] = ent;
        margin[p] = t1 - t2;
    }
    let sx = IMG as f64 / width as f64;
    let sy = IMG as f64 / height as f64;
    let last = IMG as i64 - 1;
    boxes
        .iter()
        .map(|b| {
            let gx1 = ((b[0] as f64 * sx).floor() as i64).clamp(0, last) as usize;
            let gy1 = ((b[1] as f64 * sy).floor() as i64).clamp(0, last) as usize;
            let gx2 = (((b[2] as f64 * sx).ceil() as i64 - 1).clamp(0, last) as usize).max(gx1);
            let gy2 = (((b[3] as f64 * sy).ceil() as i64 - 1).clamp(0, last) as usize).max(gy1);
            let area = ((gy2 - gy1 + 1) * (gx2 - gx1 + 1)) as f64;
            let mut out = vec![0f32; IMAGE_DIM];
            let pool = |map: &[f32], c: usize| -> (f64, f32, f32) {
                let (mut s, mut lo, mut hi) = (0f64, f32::MAX, f32::MIN);
                for y in gy1..=gy2 {
                    let row = &map[c * hw + y * IMG + gx1..=c * hw + y * IMG + gx2];
                    for &v in row {
                        s += v as f64;
                        lo = lo.min(v);
                        hi = hi.max(v);
                    }
                }
                (s / area, lo, hi)
            };
            for c in 0..FEAT_CH {
                let (mean, _, max) = pool(feat, c);
                out[c] = mean as f32;
                out[FEAT_CH + c] = max;
            }
            let base = 2 * FEAT_CH;
            for c in 0..LOGIT_CH {
                let (mean, min, max) = pool(&probs, c);
                out[base + c] = mean as f32;
                out[base + LOGIT_CH + c] = min;
                out[base + 2 * LOGIT_CH + c] = max;
            }
            out[base + 3 * LOGIT_CH] = pool(&entropy, 0).0 as f32;
            out[base + 3 * LOGIT_CH + 1] = pool(&margin, 0).0 as f32;
            out
        })
        .collect()
}

/// Connected components of the linked nodes; each takes the majority class
/// (ties broken by [`CLASS_PRIORITY`]) and the union of its boxes.
pub fn group_nodes(labels: &[usize], links: &[(usize, usize)], boxes: &[[f32; 4]]) -> Vec<LayoutGroup> {
    let n = labels.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for &(i, j) in links {
        if i == j {
            continue;
        }
        let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
        if ri != rj {
            parent[ri] = rj;
        }
    }
    let mut comp_of_root = std::collections::HashMap::new();
    let mut comps: Vec<Vec<usize>> = Vec::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        let c = *comp_of_root.entry(r).or_insert_with(|| {
            comps.push(Vec::new());
            comps.len() - 1
        });
        comps[c].push(i);
    }
    let priority = |c: usize| CLASS_PRIORITY.iter().position(|&p| p == c).unwrap_or(usize::MAX);
    comps
        .into_iter()
        .map(|nodes| {
            let mut votes = [0usize; CLASSES.len()];
            for &i in &nodes {
                votes[labels[i]] += 1;
            }
            let max = *votes.iter().max().unwrap();
            let class = (0..CLASSES.len()).filter(|&c| votes[c] == max).min_by_key(|&c| priority(c)).unwrap();
            let bbox = nodes.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |a, &i| {
                let b = boxes[i];
                [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
            });
            LayoutGroup { bbox, class, nodes }
        })
        .collect()
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let iw = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let ih = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = iw * ih;
    let union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// Pictures that overlap (IoU > 0.2) become one picture.
fn merge_overlapping_pictures(groups: Vec<LayoutGroup>) -> Vec<LayoutGroup> {
    let pics: Vec<usize> = (0..groups.len()).filter(|&i| groups[i].class == PICTURE).collect();
    if pics.len() < 2 {
        return groups;
    }
    let mut parent: Vec<usize> = (0..pics.len()).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for a in 0..pics.len() {
        for b in a + 1..pics.len() {
            if iou(&groups[pics[a]].bbox, &groups[pics[b]].bbox) > 0.2 {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                if ra != rb {
                    parent[rb] = ra;
                }
            }
        }
    }
    let mut members: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for (k, &g) in pics.iter().enumerate() {
        let r = find(&mut parent, k);
        members.entry(r).or_default().push(g);
    }
    let merged: Vec<Vec<usize>> = members.into_values().filter(|m| m.len() > 1).collect();
    if merged.is_empty() {
        return groups;
    }
    let gone: std::collections::HashSet<usize> = merged.iter().flatten().copied().collect();
    let mut out: Vec<LayoutGroup> = groups.iter().enumerate().filter(|(i, _)| !gone.contains(i)).map(|(_, g)| g.clone()).collect();
    for m in merged {
        let mut g = LayoutGroup { bbox: groups[m[0]].bbox, class: PICTURE, nodes: Vec::new() };
        for &i in &m {
            let b = groups[i].bbox;
            g.bbox = [g.bbox[0].min(b[0]), g.bbox[1].min(b[1]), g.bbox[2].max(b[2]), g.bbox[3].max(b[3])];
            g.nodes.extend(&groups[i].nodes);
        }
        g.nodes.sort_unstable();
        out.push(g);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_pattern_matches_reference() {
        // get_text_pattern("3.1 Encoder") = D1.1D1W1C7 -> 11,1,30,1,11,1,13,1,10,7
        let v = text_pattern("3.1 Encoder");
        let codes = [0o13, 0o01, 0o36, 0o01, 0o13, 0o01, 0o15, 0o01, 0o12, 0o07];
        let expect: Vec<f32> = codes.iter().flat_map(|c: &u32| [(c / 8) as f32 / 8.0, (c % 8) as f32 / 8.0]).collect();
        assert_eq!(&v[..20], &expect[..]);
        assert!(v[20..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn rf_names_split() {
        assert_eq!(RF_LOCAL, 6);
        assert_eq!(c_feature("alignment_down_with_centre"), 0);
        assert_eq!(c_feature("topmost_baseline"), RF_C - 1);
    }
}
