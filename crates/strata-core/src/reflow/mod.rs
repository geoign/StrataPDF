//! Reflow: turn fixed-layout pages into a linear document (headings, paragraphs,
//! lists, figures, tables, formulas) for Markdown/HTML display and export.
//!
//! The pipeline is heuristic and tuned for academic papers and Japanese books:
//! 1. extract rich text per page, gather document statistics (body font size,
//!    repeated header/footer lines, writing direction);
//! 2. per page: drop headers/footers, turn images and vector clusters into
//!    figures, order units by XY-cut, classify each unit;
//! 3. merge paragraphs split by column or page breaks.
//!
//! For horizontal text, the layout model of PyMuPDF Layout ([`crate::layout`])
//! labels each line (heading, caption, figure text, running head, footnote...);
//! its labels decide headings, running heads and figure labels, and split
//! blocks that mix them. The heuristics remain for what it does not cover and
//! for pages without its labels.

mod order;
pub mod output;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crossbeam_channel::{Receiver, unbounded};

use crate::doc::{Document, Engine, open_engine};
use crate::geom::RectF;
use crate::render::render_region_png;
use crate::rich::{RichBlock, RichLine, RichPage, reflow_flags};
use strata_ocr::layout::{CAPTION, FOOTNOTE, PAGE_FOOTER, PAGE_HEADER, PICTURE, SECTION_HEADER, TITLE};
use crate::text::FontInfo;

#[derive(Clone)]
pub struct ReflowOptions {
    pub strip_headers: bool,
    /// Pixels per point for figure/table crops.
    pub image_scale: f32,
    /// Pixels per point for formula crops (higher: they feed formula recognition).
    pub formula_scale: f32,
    /// Drop furigana (ruby) lines in Japanese text.
    pub drop_ruby: bool,
    /// Converts display formulas to LaTeX when set.
    pub formula: Option<std::sync::Arc<dyn strata_ocr::formula::FormulaEngine>>,
    /// Label lines with the layout model (horizontal text only).
    pub layout: bool,
}

impl Default for ReflowOptions {
    fn default() -> Self {
        ReflowOptions { strip_headers: true, image_scale: 2.0, formula_scale: 3.0, drop_ruby: true, formula: None, layout: true }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub sup: bool,
    pub sub: bool,
    pub mono: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    /// External URI, or `#page-N` for links inside the document.
    pub link: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Node {
    Heading { level: u8, spans: Vec<Span> },
    Paragraph { spans: Vec<Span> },
    ListItem { spans: Vec<Span> },
    Figure { image: usize, caption: Vec<Span> },
    /// Tables are kept as an image plus their raw text lines.
    Table { image: usize, caption: Vec<Span>, rows: Vec<String> },
    Formula { image: usize, text: String, latex: Option<String>, number: Option<String> },
    /// Footnote text, kept where it appears but skipped when joining paragraphs.
    Footnote { spans: Vec<Span> },
    /// Whole page as an image: no usable text layer (see [`needs_ocr`]).
    PageImage { image: usize, reason: String },
    /// Start of a page's content (0-based page index).
    PageStart { page: u32 },
}

#[derive(Clone, Debug)]
pub struct ReflowImage {
    pub id: String,
    pub page: u32,
    pub bbox: RectF,
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct ReflowDoc {
    pub title: String,
    pub vertical: bool,
    pub nodes: Vec<Node>,
    pub images: Vec<ReflowImage>,
    /// Source position of each node: (0-based page, top y in page space).
    /// Used to keep the reading position when switching views.
    pub anchors: Vec<(u32, f32)>,
}

impl ReflowDoc {
    fn fill_anchors(&mut self, a: (u32, f32)) {
        while self.anchors.len() < self.nodes.len() {
            self.anchors.push(a);
        }
    }
}

/// Region label from a layout-analysis model (not used by the heuristics yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionKind {
    Text,
    Title,
    Figure,
    Table,
    Formula,
    Caption,
    Header,
    Footer,
    List,
}

#[derive(Clone, Debug)]
pub struct RegionHint {
    pub page: u32,
    pub bbox: RectF,
    pub kind: RegionKind,
    pub latex: Option<String>,
}

#[derive(Clone, Debug)]
pub enum ReflowEvent {
    Progress { done: usize, total: usize },
    Done(Arc<ReflowDoc>),
    Error(String),
}

impl Document {
    /// Build the reflowed document on a background thread with its own instance.
    pub fn reflow(&self, opts: ReflowOptions, cancel: Arc<AtomicBool>) -> Receiver<ReflowEvent> {
        let (tx, rx) = unbounded();
        let path = self.info().path.clone();
        let password = self.password();
        let waker = self.waker();
        let ocr = self.ocr_store().clone();
        let serial = crate::doc::needs_serial_rendering(&path);
        std::thread::Builder::new()
            .name("strata-reflow".into())
            .spawn(move || {
                let ev = match open_engine(&path, password.as_deref()) {
                    Ok((eng, _)) => {
                        let progress = |done, total| {
                            let _ = tx.send(ReflowEvent::Progress { done, total });
                            waker();
                        };
                        match build(&eng, &opts, &progress, &cancel, &ocr, serial) {
                            Ok(Some(doc)) => ReflowEvent::Done(Arc::new(doc)),
                            Ok(None) => return,
                            Err(e) => ReflowEvent::Error(e),
                        }
                    }
                    Err(e) => ReflowEvent::Error(e.to_string()),
                };
                let _ = tx.send(ev);
                waker();
            })
            .ok();
        rx
    }
}

// ------------------------------------------------------------------ analysis

struct PageData {
    page: u32,
    rich: RichPage,
    links: Vec<(RectF, String)>,
    dl: Option<mupdf::DisplayList>,
    /// The text came from OCR.
    ocr: bool,
    /// Lines found by the layout model and their classes (empty without it).
    layout: Vec<(RectF, usize)>,
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x3000..=0x303F)
}

fn is_math_font(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const KEYS: [&str; 18] = ["math", "cmmi", "cmsy", "cmex", "msbm", "msam", "symbol", "stix", "mt extra", "mtextra", "euclid", "mathematicalpi", "txsy", "txmi", "rtxmi", "mtsy", "mtmi", "cambria math"];
    KEYS.iter().any(|k| n.contains(k))
}

/// Bold or italic from the font, including naming conventions MuPDF does not
/// recognise: "AdvOTc022ae45.B" (Adobe-subset suffixes .B, .I, .BI),
/// "Minion-Semibold", "Helvetica-BoldOblique".
fn font_bold(f: &FontInfo) -> bool {
    let n = f.name.to_ascii_lowercase();
    f.bold || n.contains("bold") || n.contains("semibold") || n.contains("heavy") || n.contains("black") || n.ends_with(".b") || n.ends_with(".bi") || n.ends_with("-bd") || n.ends_with(",bd")
}

fn font_italic(f: &FontInfo) -> bool {
    let n = f.name.to_ascii_lowercase();
    f.italic || n.contains("italic") || n.contains("oblique") || n.ends_with(".i") || n.ends_with(".bi") || n.ends_with("-it")
}

fn is_math_char(c: char) -> bool {
    matches!(c as u32, 0x2200..=0x22FF | 0x2A00..=0x2AFF | 0x1D400..=0x1D7FF | 0x2190..=0x21FF | 0x27C0..=0x27EF | 0x0391..=0x03C9)
        || "=+±×÷≤≥≈∝∞∑∏∫√∂∇".contains(c)
}

/// Some producers (Chromium, Word) write vertical text glyph by glyph, which
/// MuPDF reports as a stack of one-character horizontal lines. Rebuild such
/// blocks into a single vertical line (a column) read top to bottom.
fn normalize_vertical(p: &mut RichPage) {
    for b in &mut p.blocks {
        let RichBlock::Text { bbox, lines } = b else { continue };
        if lines.len() < 2 || lines.iter().any(|l| l.vertical) {
            continue;
        }
        let chars: usize = lines.iter().map(|l| l.chars.iter().filter(|c| !c.c.is_whitespace()).count()).sum();
        let single = lines.iter().filter(|l| l.chars.iter().filter(|c| !c.c.is_whitespace()).count() <= 1).count();
        let size = lines.iter().map(line_size).fold(0.0f32, f32::max).max(1.0);
        let tall = bbox.height() > bbox.width() * 2.0 && bbox.width() < size * 1.8;
        if !(tall && single * 10 >= lines.len() * 8 && chars >= 2) {
            continue;
        }
        let mut all: Vec<_> = lines.drain(..).flat_map(|l| l.chars).filter(|c| !c.c.is_whitespace()).collect();
        all.sort_by(|a, b| a.bbox.y0.total_cmp(&b.bbox.y0));
        let lb = all.iter().skip(1).fold(all[0].bbox, |r, c| r.union(&c.bbox));
        lines.push(RichLine { bbox: lb, vertical: true, dir: [0.0, 1.0], joined: false, chars: all });
    }
}

/// Precompose spacing accents that PDFs place before the base letter
/// ("Universit´e" -> "Université").
fn fix_accents(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let combining = |c: char| match c {
        '\u{00B4}' => Some('\u{0301}'),
        '\u{0060}' => Some('\u{0300}'),
        '\u{00A8}' => Some('\u{0308}'),
        '\u{02C6}' => Some('\u{0302}'),
        '\u{02DC}' => Some('\u{0303}'),
        '\u{02C7}' => Some('\u{030C}'),
        '\u{00B8}' => Some('\u{0327}'),
        '\u{02DA}' => Some('\u{030A}'),
        _ => None,
    };
    if !s.chars().any(|c| combining(c).is_some()) {
        return s.to_string();
    }
    let v: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < v.len() {
        match (combining(v[i]), v.get(i + 1)) {
            (Some(m), Some(&n)) if n.is_alphabetic() => {
                out.push(n);
                out.push(m);
                i += 2;
            }
            _ => {
                out.push(v[i]);
                i += 1;
            }
        }
    }
    out.nfc().collect()
}

/// Why a page's text layer is unusable, if it is: no text on an image-covered
/// page (scan), or mostly undecodable characters (fonts without ToUnicode).
pub fn needs_ocr(p: &RichPage) -> Option<String> {
    let (mut total, mut bad) = (0usize, 0usize);
    let mut image_area = 0.0f32;
    for b in &p.blocks {
        match b {
            RichBlock::Text { lines, .. } => {
                for l in lines {
                    for c in &l.chars {
                        if c.c.is_whitespace() {
                            continue;
                        }
                        total += 1;
                        let u = c.c as u32;
                        if c.c == '\u{FFFD}' || u < 0x20 || (0xE000..=0xF8FF).contains(&u) || (0x80..0xA0).contains(&u) {
                            bad += 1;
                        }
                    }
                }
            }
            RichBlock::Image { bbox } => image_area += bbox.width() * bbox.height(),
            _ => {}
        }
    }
    let page_area = (p.width * p.height).max(1.0);
    if total < 20 && image_area > page_area * 0.5 {
        Some("テキスト層のないスキャンページ".into())
    } else if total >= 20 && bad as f32 > total as f32 * 0.3 {
        Some("文字コードを復元できないフォント（ToUnicode なし）".into())
    } else {
        None
    }
}

fn digits_key(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).map(|c| if c.is_ascii_digit() { '#' } else { c }).collect()
}

fn line_size(l: &RichLine) -> f32 {
    let mut v: Vec<f32> = l.chars.iter().map(|c| c.size).collect();
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

/// Typical body font size: the size carrying the most characters.
fn body_size(pages: &[PageData]) -> f32 {
    let mut hist: HashMap<i32, usize> = HashMap::new();
    for p in pages {
        for b in &p.rich.blocks {
            if let RichBlock::Text { lines, .. } = b {
                for l in lines {
                    for c in &l.chars {
                        *hist.entry((c.size * 2.0).round() as i32).or_default() += 1;
                    }
                }
            }
        }
    }
    hist.into_iter().max_by_key(|e| e.1).map(|e| e.0 as f32 / 2.0).unwrap_or(10.0)
}

/// Header/footer line keys that repeat across pages.
fn repeated_margin_lines(pages: &[PageData]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for p in pages {
        let h = p.rich.height;
        let mut seen = std::collections::HashSet::new();
        for b in &p.rich.blocks {
            if let RichBlock::Text { lines, .. } = b {
                for l in lines {
                    if l.bbox.y1 < h * 0.15 || l.bbox.y0 > h * 0.85 {
                        let k = digits_key(&l.text());
                        if !k.is_empty() && seen.insert(k.clone()) {
                            *counts.entry(k).or_default() += 1;
                        }
                    }
                }
            }
        }
    }
    counts
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum UnitKind {
    Text,
    Figure,
}

struct Unit {
    kind: UnitKind,
    bbox: RectF,
    lines: Vec<RichLine>,
    /// Layout class of most of its text.
    class: Option<usize>,
}

impl Unit {
    fn text(&self) -> String {
        let mut s = String::new();
        for (i, l) in self.lines.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(&l.text());
        }
        s
    }
    fn size(&self) -> f32 {
        let mut v: Vec<f32> = self.lines.iter().flat_map(|l| l.chars.iter().map(|c| c.size)).collect();
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    }
    fn frac(&self, f: impl Fn(&crate::rich::RichChar) -> bool) -> f32 {
        let (mut n, mut k) = (0usize, 0usize);
        for l in &self.lines {
            for c in &l.chars {
                if !c.c.is_whitespace() {
                    n += 1;
                    if f(c) {
                        k += 1;
                    }
                }
            }
        }
        if n == 0 { 0.0 } else { k as f32 / n as f32 }
    }
    fn chars(&self) -> usize {
        self.lines.iter().map(|l| l.chars.len()).sum()
    }
}

fn union(a: RectF, b: RectF) -> RectF {
    a.union(&b)
}

fn overlap_frac(inner: &RectF, outer: &RectF) -> f32 {
    let x0 = inner.x0.max(outer.x0);
    let y0 = inner.y0.max(outer.y0);
    let x1 = inner.x1.min(outer.x1);
    let y1 = inner.y1.min(outer.y1);
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let a = (inner.width() * inner.height()).max(1e-3);
    (x1 - x0) * (y1 - y0) / a
}

/// Layout class of a line: that of the layout line holding most of its characters.
fn line_class(layout: &[(RectF, usize)], l: &RichLine) -> Option<usize> {
    let cand: Vec<&(RectF, usize)> = layout.iter().filter(|(r, _)| overlap_frac(&l.bbox, r) > 0.0).collect();
    match cand.len() {
        0 => None,
        1 => Some(cand[0].1),
        _ => {
            let mut votes: HashMap<usize, usize> = HashMap::new();
            for c in &l.chars {
                let (x, y) = ((c.bbox.x0 + c.bbox.x1) / 2.0, (c.bbox.y0 + c.bbox.y1) / 2.0);
                if let Some((_, k)) = cand.iter().find(|(r, _)| r.x0 <= x && x <= r.x1 && r.y0 <= y && y <= r.y1) {
                    *votes.entry(*k).or_default() += 1;
                }
            }
            votes.into_iter().max_by_key(|&(k, n)| (n, std::cmp::Reverse(k))).map(|(k, _)| k)
        }
    }
}

/// The class of most characters of the lines.
fn majority_class(lines: &[(RichLine, Option<usize>)]) -> Option<usize> {
    let mut votes: HashMap<usize, usize> = HashMap::new();
    for (l, c) in lines {
        if let Some(c) = c {
            *votes.entry(*c).or_default() += l.chars.len();
        }
    }
    votes.into_iter().max_by_key(|&(k, n)| (n, std::cmp::Reverse(k))).map(|(k, _)| k)
}

/// Lines of one block that play different parts (heading, caption, figure text,
/// running text) become separate units.
fn role(c: usize) -> u8 {
    match c {
        TITLE | SECTION_HEADER => 1,
        CAPTION => 2,
        PICTURE => 3,
        _ => 0,
    }
}

/// Merge vector blocks into clusters; clusters that look like drawings (not just
/// table rules or underlines) become figure candidates.
fn vector_figures(blocks: &[RichBlock], page_area: f32) -> Vec<RectF> {
    let mut clusters: Vec<(RectF, usize, usize)> = Vec::new(); // bbox, members, non-line members
    for b in blocks {
        let RichBlock::Vector { bbox } = b else { continue };
        let thin = bbox.width() < 1.5 || bbox.height() < 1.5;
        let grown = RectF { x0: bbox.x0 - 4.0, y0: bbox.y0 - 4.0, x1: bbox.x1 + 4.0, y1: bbox.y1 + 4.0 };
        match clusters.iter_mut().find(|c| overlap_frac(&grown, &c.0) > 0.0) {
            Some(c) => {
                c.0 = union(c.0, *bbox);
                c.1 += 1;
                c.2 += (!thin) as usize;
            }
            None => clusters.push((*bbox, 1, (!thin) as usize)),
        }
    }
    // One more pass merging overlapping clusters.
    let mut merged: Vec<(RectF, usize, usize)> = Vec::new();
    for c in clusters {
        match merged.iter_mut().find(|m| overlap_frac(&c.0, &m.0) > 0.0 || overlap_frac(&m.0, &c.0) > 0.0) {
            Some(m) => {
                m.0 = union(m.0, c.0);
                m.1 += c.1;
                m.2 += c.2;
            }
            None => merged.push(c),
        }
    }
    merged
        .into_iter()
        .filter(|(r, n, shapes)| {
            let area = r.width() * r.height();
            area > page_area * 0.02 && *shapes >= 3 && *n >= 5 && area < page_area * 0.9
        })
        .map(|c| c.0)
        .collect()
}

/// Line numbers of a manuscript (review copies, discussion papers): a column of
/// short number-only blocks at the same x. Returns their indices among `units`.
fn line_number_column(units: &[Unit], body: f32) -> Vec<usize> {
    let numbers: Vec<usize> = units
        .iter()
        .enumerate()
        .filter(|(_, u)| {
            let t = u.text();
            let t = t.trim();
            u.kind == UnitKind::Text && u.lines.len() == 1 && (1..=4).contains(&t.len()) && t.chars().all(|c| c.is_ascii_digit()) && u.bbox.width() < body * 2.5
        })
        .map(|(i, _)| i)
        .collect();
    // Group by right edge (numbers are right-aligned).
    let mut groups: Vec<(f32, Vec<usize>)> = Vec::new();
    for &i in &numbers {
        let x = units[i].bbox.x1;
        match groups.iter_mut().find(|g| (g.0 - x).abs() < 3.0) {
            Some(g) => g.1.push(i),
            None => groups.push((x, vec![i])),
        }
    }
    // In the margin, left or right of all other text (a column of a table is not).
    let others: Vec<RectF> = units.iter().enumerate().filter(|(i, u)| u.kind == UnitKind::Text && !numbers.contains(i)).map(|(_, u)| u.bbox).collect();
    // Nearly all other text (running heads aside) lies on one side of the column.
    let one_side = |x0: f32, x1: f32| {
        let left = others.iter().filter(|b| b.x0 < x1 - 1.0).count();
        let right = others.iter().filter(|b| b.x1 > x0 + 1.0).count();
        left * 10 <= others.len() || right * 10 <= others.len()
    };
    groups
        .into_iter()
        .filter(|g| g.1.len() >= 8 && one_side(g.1.iter().map(|&i| units[i].bbox.x0).fold(f32::INFINITY, f32::min), g.0))
        .flat_map(|g| g.1)
        .collect()
}

/// Text and figure units of a page, and whether the page is a line-numbered
/// manuscript (then usually double-spaced).
fn page_units(p: &PageData, body: f32, repeated: &HashMap<String, usize>, n_pages: usize, opts: &ReflowOptions, vertical: bool) -> (Vec<Unit>, bool) {
    let h = p.rich.height;
    let w = p.rich.width;
    let threshold = (n_pages as f32 * 0.25).ceil().max(2.0) as usize;
    let mut units: Vec<Unit> = Vec::new();
    let page_area = w * h;
    let text_chars: usize = p
        .rich
        .blocks
        .iter()
        .map(|b| match b {
            RichBlock::Text { lines, .. } => lines.iter().map(|l| l.chars.len()).sum(),
            _ => 0,
        })
        .sum();
    for b in &p.rich.blocks {
        match b {
            RichBlock::Text { lines, .. } => {
                let kept: Vec<(RichLine, Option<usize>)> = lines
                    .iter()
                    .map(|l| (l, line_class(&p.layout, l)))
                    .filter(|(l, class)| {
                        // Running heads and page numbers found by the layout model.
                        if opts.strip_headers && matches!(class, Some(PAGE_HEADER | PAGE_FOOTER)) && (l.bbox.y1 < h * 0.15 || l.bbox.y0 > h * 0.85) {
                            return false;
                        }
                        if opts.strip_headers && (l.bbox.y1 < h * 0.15 || l.bbox.y0 > h * 0.85) {
                            let t = l.text();
                            let k = digits_key(&t);
                            if repeated.get(&k).copied().unwrap_or(0) >= threshold && n_pages > 1 {
                                return false;
                            }
                            // Lone page numbers in the margins.
                            let margin = l.bbox.y1 < h * 0.09 || l.bbox.y0 > h * 0.91;
                            if margin && t.trim().chars().all(|c| c.is_ascii_digit() || c == '-' || c == '—' || c.is_whitespace()) {
                                return false;
                            }
                        }
                        // Rotated text (margin stamps such as arXiv identifiers).
                        if !l.vertical && l.dir[1].abs() > 0.5 {
                            return false;
                        }
                        if opts.drop_ruby && line_size(l) < body * 0.6 && l.chars.iter().all(|c| matches!(c.c as u32, 0x3040..=0x30FF) || c.c.is_whitespace()) {
                            return false;
                        }
                        let _ = vertical;
                        true
                    })
                    .map(|(l, c)| (l.clone(), c))
                    .collect();
                // Split where the layout role changes; lines without a class go with the previous ones.
                let mut runs: Vec<Vec<(RichLine, Option<usize>)>> = Vec::new();
                let mut cur_role: Option<u8> = None;
                for (l, c) in kept {
                    let r = c.map(role);
                    match (runs.last_mut(), r) {
                        (Some(run), Some(r)) if cur_role.is_none_or(|cr| cr == r) => {
                            cur_role = Some(r);
                            run.push((l, c));
                        }
                        (Some(run), None) => run.push((l, c)),
                        _ => {
                            cur_role = r;
                            runs.push(vec![(l, c)]);
                        }
                    }
                }
                for run in runs {
                    // Whitespace-only blocks (spacing in manuscripts) would become empty
                    // paragraphs that stop paragraph joining.
                    if run.iter().any(|(l, _)| l.chars.iter().any(|c| !c.c.is_whitespace())) {
                        let bbox = run.iter().skip(1).fold(run[0].0.bbox, |a, (l, _)| a.union(&l.bbox));
                        let class = majority_class(&run);
                        units.push(Unit { kind: UnitKind::Text, bbox, lines: run.into_iter().map(|(l, _)| l).collect(), class });
                    }
                }
            }
            RichBlock::Image { bbox } => {
                // A page-sized image under a text layer is the scan of an OCRed page.
                let background = bbox.width() * bbox.height() > page_area * 0.7 && text_chars > 100;
                if !background && bbox.width() * bbox.height() > page_area * 0.005 {
                    units.push(Unit { kind: UnitKind::Figure, bbox: *bbox, lines: Vec::new(), class: None });
                }
            }
            _ => {}
        }
    }
    for r in vector_figures(&p.rich.blocks, page_area) {
        units.push(Unit { kind: UnitKind::Figure, bbox: r, lines: Vec::new(), class: None });
    }
    // Merge overlapping figures; absorb labels inside figures.
    let mut figs: Vec<RectF> = Vec::new();
    for u in units.iter().filter(|u| u.kind == UnitKind::Figure) {
        match figs.iter_mut().find(|f| overlap_frac(&u.bbox, f) > 0.3 || overlap_frac(f, &u.bbox) > 0.3) {
            Some(f) => *f = f.union(&u.bbox),
            None => figs.push(u.bbox),
        }
    }
    let mut texts: Vec<Option<Unit>> = units.into_iter().filter(|u| u.kind == UnitKind::Text).map(Some).collect();
    // Labels around a figure (axis ticks, legends, panel letters, a chart title)
    // lie just outside the drawing: take short blocks within a few lines of it
    // into the figure, growing it so that chained labels follow, and so that
    // the figure image shows them. Captions and headings stay text.
    let label_like = |u: &Unit| {
        // The layout model knows figure text from running text.
        if let Some(c) = u.class {
            return c == PICTURE;
        }
        let t = u.text();
        let n = t.chars().count();
        // A heading next to a figure ("Attention Visualizations" above a chart) is
        // larger than body text, or bold words at body size; panel letters and
        // legend titles are smaller or single letters.
        let size = u.size();
        let bold = u.frac(|c| c.bold || p.rich.fonts.get(c.font as usize).is_some_and(font_bold));
        let words = t.split_whitespace().filter(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 3).count();
        let heading_like = words >= 2 && (size >= body * 1.1 || (bold >= 0.8 && size >= body * 0.95));
        !heading_like
            && caption_kind(&t).is_none()
            && numbered_heading_depth(&t).is_none()
            && ((u.lines.len() <= 2 && n <= 60) || (size < body * 0.9 && u.lines.len() <= 6 && n <= 200))
    };
    let margin = body * 2.5;
    for _ in 0..4 {
        let mut grew = false;
        for slot in texts.iter_mut() {
            let Some(u) = slot else { continue };
            if !label_like(u) {
                continue;
            }
            if let Some(f) = figs.iter_mut().find(|f| {
                let near = RectF { x0: f.x0 - margin, y0: f.y0 - margin, x1: f.x1 + margin, y1: f.y1 + margin };
                overlap_frac(&u.bbox, &near) > 0.8
            }) {
                *f = f.union(&u.bbox);
                *slot = None;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    // Figure text (per the layout model) away from every figure found above
    // belongs to a drawing the clustering missed: the text and the vector
    // graphics around it make a figure.
    let grow = |r: &RectF| RectF { x0: r.x0 - margin, y0: r.y0 - margin, x1: r.x1 + margin, y1: r.y1 + margin };
    let mut clusters: Vec<(RectF, Vec<usize>)> = Vec::new();
    for (i, slot) in texts.iter().enumerate() {
        if let Some(u) = slot
            && u.class == Some(PICTURE)
        {
            match clusters.iter_mut().find(|c| overlap_frac(&u.bbox, &grow(&c.0)) > 0.0) {
                Some(c) => {
                    c.0 = c.0.union(&u.bbox);
                    c.1.push(i);
                }
                None => clusters.push((u.bbox, vec![i])),
            }
        }
    }
    for (mut region, members) in clusters {
        let mut graphics = 0;
        for _ in 0..3 {
            let near = grow(&region);
            for b in &p.rich.blocks {
                if let RichBlock::Vector { bbox } | RichBlock::Image { bbox } = b
                    && overlap_frac(bbox, &near) > 0.5
                    && overlap_frac(bbox, &region) < 1.0
                    && bbox.width() * bbox.height() < page_area * 0.5
                {
                    region = region.union(bbox);
                    graphics += 1;
                }
            }
        }
        if graphics >= 3 {
            for i in members {
                texts[i] = None;
            }
            figs.push(region);
        }
    }
    let mut out: Vec<Unit> = Vec::new();
    for u in texts.into_iter().flatten() {
        let inside = figs.iter().any(|f| overlap_frac(&u.bbox, f) > 0.8);
        let bodyish = u.lines.len() >= 3 && (u.size() - body).abs() < body * 0.1;
        if inside && !bodyish {
            continue;
        }
        out.push(u);
    }
    // Section numbers set apart from their titles ("1.4" | "Characteristics of…")
    // would be read as a column of their own: join each to the title on its line.
    let mut k = 0;
    while k < out.len() {
        let t = out[k].text();
        let t = t.trim();
        let number = out[k].lines.len() == 1 && !t.is_empty() && t.len() <= 12 && t.chars().all(|c| c.is_ascii_digit() || c == '.') && t.contains(|c: char| c.is_ascii_digit());
        let nb = out[k].bbox;
        let title = number
            .then(|| {
                out.iter().position(|o| {
                    // Only a heading's number: table cells are numbers next to text too.
                    let heading = match (o.class, out[k].class) {
                        (Some(c), _) => matches!(c, TITLE | SECTION_HEADER),
                        (None, None) => o.lines.len() <= 2 && t.contains('.'),
                        (None, Some(_)) => false,
                    };
                    let first = o.lines.first().map(|l| l.bbox);
                    heading && first.is_some_and(|f| {
                        let overlap = f.y1.min(nb.y1) - f.y0.max(nb.y0);
                        overlap > nb.height().min(f.height()) * 0.5 && f.x0 >= nb.x1 - 1.0 && f.x0 - nb.x1 < body * 4.0
                    })
                })
            })
            .flatten();
        match title {
            Some(j) if j != k => {
                let num = out.remove(k);
                let j = if j > k { j - 1 } else { j };
                let o = &mut out[j];
                o.bbox = o.bbox.union(&num.bbox);
                let mut lines = num.lines;
                lines.append(&mut o.lines);
                o.lines = lines;
                if o.class.is_none() {
                    o.class = num.class;
                }
            }
            _ => k += 1,
        }
    }
    out.extend(figs.into_iter().map(|bbox| Unit { kind: UnitKind::Figure, bbox, lines: Vec::new(), class: None }));
    let numbers = line_number_column(&out, body);
    let manuscript = !numbers.is_empty();
    let mut i = 0;
    out.retain(|_| {
        i += 1;
        !numbers.contains(&(i - 1))
    });
    (out, manuscript)
}

fn caption_kind(t: &str) -> Option<bool> {
    // Some(true) for tables, Some(false) for figures.
    let t = t.trim_start();
    let starts = |p: &str| t.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p)) && t.len() > p.len();
    let num_after = |p: &str| t.get(p.len()..).unwrap_or("").trim_start().chars().next().is_some_and(|c| c.is_ascii_digit() || matches!(c, 'I' | 'V' | 'X' | 'S'));
    if (starts("table") && num_after("table")) || t.starts_with('表') {
        Some(true)
    } else if (starts("fig.") && num_after("fig.")) || (starts("figure") && num_after("figure")) || t.starts_with('図') || (starts("plate") && num_after("plate")) {
        Some(false)
    } else {
        None
    }
}

fn numbered_heading_depth(t: &str) -> Option<u8> {
    let t = t.trim_start();
    let mut depth = 0u8;
    let mut chars = t.chars().peekable();
    let mut saw_digit = false;
    loop {
        let mut any = false;
        while let Some(c) = chars.peek() {
            if c.is_ascii_digit() {
                chars.next();
                any = true;
            } else {
                break;
            }
        }
        if !any {
            break;
        }
        saw_digit = true;
        depth += 1;
        match chars.peek() {
            Some('.') => {
                chars.next();
            }
            _ => break,
        }
    }
    let rest: String = chars.collect();
    // "2.1 | Methods" (Wiley) as well as "2.1 Methods".
    let title = rest.trim_start();
    let title = title.strip_prefix('|').map(str::trim_start).unwrap_or(title);
    (saw_digit && rest.starts_with(char::is_whitespace) && title.chars().next().is_some_and(|c| c.is_alphabetic())).then_some(depth)
}

fn is_list_marker(t: &str) -> bool {
    let t = t.trim_start();
    t.starts_with(['•', '·', '▪', '●', '◦', '‣', '–', '・']) || {
        let mut it = t.chars();
        let a = it.next();
        let b = it.next();
        matches!((a, b), (Some('('), Some(c)) if c.is_ascii_alphanumeric()) && t.find(')').is_some_and(|i| i <= 4)
    }
}

/// Split a unit's characters into styled spans.
fn spans_of(u: &Unit, fonts: &[FontInfo], links: &[(RectF, String)], vertical: bool) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut prev_last: Option<char> = None;
    let mut prev_joined = false;
    for (li, l) in u.lines.iter().enumerate() {
        let med = line_size(l);
        let lcy = (l.bbox.y0 + l.bbox.y1) * 0.5;
        let first = l.chars.first().map(|c| c.c);
        if li > 0 && prev_joined {
            // MuPDF flags the line ending in a hyphen; drop the hyphen and join.
            if let Some(s) = spans.last_mut()
                && s.text.ends_with(['-', '\u{2010}', '\u{2011}', '\u{00AD}'])
            {
                s.text.pop();
            }
        } else if li > 0 {
            // A real hyphen at the line end ("Species-" / "Poor") joins without a space.
            let join_tight = vertical || prev_last == Some('-') || matches!((prev_last, first), (Some(a), Some(b)) if is_cjk(a) || is_cjk(b));
            if !join_tight
                && let Some(s) = spans.last_mut()
                && !s.text.ends_with(' ')
            {
                s.text.push(' ');
            }
        }
        for c in &l.chars {
            let f = fonts.get(c.font as usize);
            let small = c.size < med * 0.8 && !vertical;
            let cy = (c.bbox.y0 + c.bbox.y1) * 0.5;
            let style = Style {
                bold: c.bold || f.is_some_and(font_bold),
                italic: f.is_some_and(font_italic),
                sup: small && cy < lcy - med * 0.1,
                sub: small && cy > lcy + med * 0.1,
                mono: f.is_some_and(|f| f.monospaced),
            };
            let (cx, ccy) = c.bbox.center();
            let link = links.iter().find(|(r, _)| r.contains(cx, ccy)).map(|(_, u)| u.clone());
            match spans.last_mut() {
                Some(s) if s.style == style && s.link == link => s.text.push(c.c),
                Some(s) if c.c == ' ' && s.link == link => s.text.push(' '),
                _ => spans.push(Span { text: c.c.to_string(), style, link }),
            }
        }
        prev_last = l.chars.last().map(|c| c.c);
        prev_joined = l.joined;
    }
    for s in &mut spans {
        if s.style.sup || s.style.sub {
            s.text = s.text.trim().to_string();
        }
    }
    // A spacing accent set in another font forms its own span; move it onto the
    // letter it belongs to before composing.
    let mut i = 0;
    while i + 1 < spans.len() {
        let t = spans[i].text.as_str();
        if t.chars().count() == 1 && matches!(t, "\u{00B4}" | "`" | "\u{00A8}" | "\u{02C6}" | "\u{02DC}" | "\u{02C7}" | "\u{00B8}" | "\u{02DA}") {
            let acc = spans.remove(i).text;
            spans[i].text.insert_str(0, &acc);
        } else {
            i += 1;
        }
    }
    for s in &mut spans {
        s.text = fix_accents(&s.text);
    }
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for s in spans {
        match merged.last_mut() {
            Some(m) if m.style == s.style && m.link == s.link => m.text.push_str(&s.text),
            _ => merged.push(s),
        }
    }
    let mut spans = merged;
    spans.retain(|s| !s.text.is_empty());
    spans
}

fn spans_text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn ends_sentence(t: &str) -> bool {
    let t = t.trim_end();
    t.ends_with(['.', '!', '?', ':', '。', '．', '！', '？', '」', '』', '）', ')']) || t.is_empty()
}

fn continues(prev: &str, next: &str) -> bool {
    let Some(first) = next.trim_start().chars().next() else { return false };
    if ends_sentence(prev) {
        return false;
    }
    if next.starts_with(['\u{3000}', ' ']) {
        return false;
    }
    first.is_lowercase() || is_cjk(first) || first.is_ascii_digit() || matches!(first, ',' | ';' | '(' | '[' | '、')
}

/// Whether a text unit starts or ends its column: no other text of the same
/// column lies above (below) it, or only a figure does. Full-width blocks such
/// as an abstract above two columns do not count as the same column.
fn column_edges(units: &[Unit], i: usize) -> (bool, bool) {
    let u = &units[i];
    let same_column = |o: &Unit| {
        let overlap = u.bbox.x1.min(o.bbox.x1) - u.bbox.x0.max(o.bbox.x0);
        overlap > 0.3 * u.bbox.width().min(o.bbox.width()) && o.bbox.width() < u.bbox.width() * 1.5
    };
    let (mut above, mut below) = (f32::NEG_INFINITY, f32::INFINITY);
    for (j, o) in units.iter().enumerate() {
        if j == i || o.kind != UnitKind::Text || !same_column(o) {
            continue;
        }
        if o.bbox.y1 <= u.bbox.y0 + 2.0 {
            above = above.max(o.bbox.y1);
        }
        if o.bbox.y0 >= u.bbox.y1 - 2.0 {
            below = below.min(o.bbox.y0);
        }
    }
    let figure_between = |a: f32, b: f32| {
        units.iter().any(|o| {
            let overlap = u.bbox.x1.min(o.bbox.x1) - u.bbox.x0.max(o.bbox.x0);
            o.kind == UnitKind::Figure && overlap > 0.3 * u.bbox.width() && o.bbox.y0 >= a - 2.0 && o.bbox.y1 <= b + 2.0
        })
    };
    let top = above == f32::NEG_INFINITY || figure_between(above, u.bbox.y0);
    let bottom = below == f32::INFINITY || figure_between(u.bbox.y1, below);
    (top, bottom)
}

/// Where the last paragraph unit sat, for joining a sentence cut by a column,
/// page or figure break that resumes with a capital letter.
struct LastPara {
    node: usize,
    /// It was the last text of its column and its last line ran to the column edge.
    cut_at_edge: bool,
    /// Its last line ran to the right edge of the unit.
    last_full: bool,
    page: u32,
    bbox: RectF,
}

fn math_fraction(u: &Unit, fonts: &[FontInfo]) -> f32 {
    u.frac(|c| (is_math_char(c.c) || fonts.get(c.font as usize).is_some_and(|f| is_math_font(&f.name))) && c.c != '•')
}

/// Ends with an equation number such as "(12)" or "(3a)".
fn has_equation_number(t: &str) -> bool {
    let t = t.trim_end();
    t.ends_with(')')
        && t.rfind('(').is_some_and(|i| {
            let n = &t[i + 1..t.len() - 1];
            !n.is_empty() && n.len() <= 5 && n.chars().any(|c| c.is_ascii_digit()) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
        })
}

/// A display formula is often extracted as several fragments (numerator,
/// radical sign, denominator, equation number). Merge consecutive math-like
/// fragments that sit on neighbouring lines into one unit.
fn merge_display_math(units: Vec<Unit>, fonts: &[FontInfo], body: f32) -> Vec<Unit> {
    let mathish = |u: &Unit| {
        if u.kind != UnitKind::Text || u.lines.len() > 4 {
            return false;
        }
        let t = u.text();
        let tiny = t.trim().chars().count() <= 4 && t.chars().any(|c| is_math_char(c) || "√∑∫∏()".contains(c));
        math_fraction(u, fonts) >= 0.2 || tiny || (has_equation_number(&t) && t.chars().count() < 80)
    };
    let mut out: Vec<Unit> = Vec::with_capacity(units.len());
    let mut prev_math = false;
    for u in units {
        let m = mathish(&u);
        if m
            && prev_math
            && let Some(last) = out.last_mut()
        {
            let gap = u.bbox.y0 - last.bbox.y1;
            let overlap_x = u.bbox.x0 < last.bbox.x1 + body * 4.0 && u.bbox.x1 > last.bbox.x0 - body * 4.0;
            if gap < body * 1.6 && overlap_x {
                last.bbox = last.bbox.union(&u.bbox);
                last.lines.extend(u.lines);
                continue;
            }
        }
        prev_math = m;
        out.push(u);
    }
    out
}

/// Label the lines of every page with the layout model, on several threads
/// (one for documents whose display lists must not run concurrently).
fn run_layout(model: &strata_ocr::layout::LayoutModel, pages: &mut [PageData], serial: bool, cancel: &AtomicBool, progress: &(dyn Fn(usize) + Sync)) {
    let workers = if serial { 1 } else { std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8) };
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Vec<parking_lot::Mutex<Vec<(RectF, usize)>>> = pages.iter().map(|_| parking_lot::Mutex::new(Vec::new())).collect();
    let pages_ref = &*pages;
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= pages_ref.len() || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let p = &pages_ref[i];
                    if !p.ocr
                        && let Some(dl) = &p.dl
                    {
                        match crate::layout::analyze_display_list(model, dl, p.rich.width, p.rich.height) {
                            Ok(a) => {
                                // Each line takes the class of its region (the majority of its lines).
                                *results[i].lock() = a
                                    .layout
                                    .groups
                                    .iter()
                                    .flat_map(|g| g.nodes.iter().map(move |&k| (k, g.class)))
                                    .map(|(k, c)| {
                                        let b = a.nodes[k].bbox;
                                        (RectF { x0: b[0], y0: b[1], x1: b[2], y1: b[3] }, c)
                                    })
                                    .collect()
                            }
                            Err(e) => log::warn!("layout analysis of page {}: {e}", p.page + 1),
                        }
                    }
                    progress(done.fetch_add(1, Ordering::Relaxed) + 1);
                }
            });
        }
    });
    for (p, r) in pages.iter_mut().zip(results) {
        p.layout = r.into_inner();
    }
}

/// Top and bottom of the body text columns on a vertical page.
fn vertical_text_area(units: &[Unit], body: f32) -> (f32, f32) {
    let cols = units.iter().filter(|u| u.kind == UnitKind::Text && u.lines.iter().all(|l| l.vertical) && (u.size() - body).abs() <= body * 0.15);
    let (mut top, mut bottom) = (f32::INFINITY, f32::NEG_INFINITY);
    for u in cols {
        top = top.min(u.bbox.y0);
        bottom = bottom.max(u.bbox.y1);
    }
    if top > bottom { (0.0, 0.0) } else { (top, bottom) }
}

fn build(eng: &Engine, opts: &ReflowOptions, progress: &(dyn Fn(usize, usize) + Sync), cancel: &AtomicBool, ocr: &crate::ocr::OcrStore, serial: bool) -> Result<Option<ReflowDoc>, String> {
    let n = eng.page_count().map_err(|e| e.to_string())?.max(0) as usize;
    let total = n * 3;
    // Pass 1: extraction.
    let mut pages = Vec::with_capacity(n);
    for p in 0..n {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let Ok(page) = eng.load_page(p as i32) else { continue };
        let b = page.bounds().map_err(|e| e.to_string())?;
        let tp = page.to_text_page(reflow_flags() | mupdf::TextPageFlags::COLLECT_VECTORS);
        let Ok(tp) = tp else { continue };
        let mut rich = RichPage::from_text_page(&tp, b.width(), b.height());
        // OCR text replaces an unusable text layer.
        let mut from_ocr = false;
        if let Some(o) = ocr.get(p as u32)
            && (o.forced || needs_ocr(&rich).is_some())
        {
            rich = o.to_rich(b.width(), b.height());
            from_ocr = true;
        }
        normalize_vertical(&mut rich);
        let links = page
            .links()
            .map(|it| {
                it.filter_map(|l| {
                    let target = match l.dest {
                        Some(d) => format!("#page-{}", d.loc.page_number + 1),
                        None if !l.uri.is_empty() => l.uri,
                        None => return None,
                    };
                    Some((RectF::from(l.bounds), target))
                })
                .collect()
            })
            .unwrap_or_default();
        let ocr = from_ocr || needs_ocr(&rich).is_some();
        pages.push(PageData { page: p as u32, rich, links, dl: page.to_display_list(true).ok(), ocr, layout: Vec::new() });
        if p % 4 == 0 {
            progress(p + 1, total);
        }
    }
    let body = body_size(&pages);
    let repeated = repeated_margin_lines(&pages);
    let (mut vchars, mut hchars) = (0usize, 0usize);
    for p in &pages {
        for b in &p.rich.blocks {
            if let RichBlock::Text { lines, .. } = b {
                for l in lines {
                    if l.vertical { vchars += l.chars.len() } else { hchars += l.chars.len() }
                }
            }
        }
    }
    let vertical = vchars > hchars;
    if opts.layout
        && !vertical
        && let Some(model) = crate::layout::shared_model()
    {
        run_layout(&model, &mut pages, serial, cancel, &|done| progress(n + done, total));
    }

    // Pass 2: per page layout and classification.
    let mut doc = ReflowDoc { vertical, ..Default::default() };
    let mut title_size = 0.0f32;
    // Vertical text: did the last paragraph column run to the bottom of the text area?
    let mut last_col_full = false;
    let mut last_para: Option<LastPara> = None;
    // The last heading from the layout model: node, page, box and size, for
    // headings that the extractor split into one block per line.
    let mut last_heading: Option<(usize, u32, RectF, f32)> = None;
    for (pi, p) in pages.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let (mut units, manuscript) = page_units(p, body, &repeated, pages.len(), opts, vertical);
        // Double-spaced manuscripts leave a blank line's height between lines.
        let line_gap = if manuscript { body * 1.8 } else { body * 0.6 };
        let rects: Vec<RectF> = units.iter().map(|u| u.bbox).collect();
        let order = order::reading_order(&rects, vertical);
        let mut ordered: Vec<Unit> = Vec::with_capacity(units.len());
        let mut slots: Vec<Option<Unit>> = units.drain(..).map(Some).collect();
        for i in order {
            if let Some(u) = slots[i].take() {
                ordered.push(u);
            }
        }
        if !vertical {
            ordered = merge_display_math(ordered, &p.rich.fonts, body);
        }
        doc.fill_anchors((p.page, 0.0));
        doc.nodes.push(Node::PageStart { page: p.page });
        doc.fill_anchors((p.page, 0.0));
        let crop = |bbox: RectF, scale: f32, doc: &mut ReflowDoc| -> Option<usize> {
            let dl = p.dl.as_ref()?;
            let pad = RectF { x0: bbox.x0 - 2.0, y0: bbox.y0 - 2.0, x1: bbox.x1 + 2.0, y1: bbox.y1 + 2.0 };
            let (w, h, png) = render_region_png(dl, pad, scale).ok()?;
            let id = format!("p{}_{}.png", p.page + 1, doc.images.len() + 1);
            doc.images.push(ReflowImage { id, page: p.page, bbox, width: w, height: h, png });
            Some(doc.images.len() - 1)
        };

        // Pages without a usable text layer (scans, fonts lacking ToUnicode) are
        // shown as images until OCR supplies text.
        if let Some(reason) = needs_ocr(&p.rich) {
            let full = RectF { x0: 0.0, y0: 0.0, x1: p.rich.width, y1: p.rich.height };
            if let Some(img) = crop(full, opts.image_scale, &mut doc) {
                doc.nodes.push(Node::PageImage { image: img, reason });
            }
            doc.fill_anchors((p.page, 0.0));
            progress(2 * n + pi + 1, total);
            continue;
        }

        let mut last_anchor = (p.page, 0.0f32);
        let mut i = 0;
        while i < ordered.len() {
            let u = &ordered[i];
            // Nodes created for this unit start at its top edge.
            doc.fill_anchors(last_anchor);
            last_anchor = (p.page, if vertical { 0.0 } else { u.bbox.y0 });
            if u.kind == UnitKind::Figure {
                // Caption right after (or before) the figure.
                let mut caption = Vec::new();
                if let Some(next) = ordered.get(i + 1)
                    && next.kind == UnitKind::Text
                    && caption_kind(&next.text()) == Some(false)
                {
                    caption = spans_of(next, &p.rich.fonts, &p.links, vertical);
                    if let Some(img) = crop(u.bbox, opts.image_scale, &mut doc) {
                        doc.nodes.push(Node::Figure { image: img, caption });
                    }
                    i += 2;
                    continue;
                }
                if let Some(img) = crop(u.bbox, opts.image_scale, &mut doc) {
                    doc.nodes.push(Node::Figure { image: img, caption });
                }
                i += 1;
                continue;
            }
            let text = u.text();
            let size = u.size();
            // Running text that happens to start with "Table 2 summarizes..." is no
            // caption: with the layout model, a table caption must lead into a table.
            let running_text = matches!(u.class, Some(strata_ocr::layout::TEXT | strata_ocr::layout::LIST_ITEM));
            let leads_to_table = ordered[i + 1..].iter().take(3).any(|n| n.kind == UnitKind::Figure || n.class == Some(strata_ocr::layout::TABLE));
            match caption_kind(&text).filter(|&table| !(running_text && table && !leads_to_table)) {
                Some(true) => {
                    // Table: caption, then small-font units until body text resumes.
                    let caption = spans_of(u, &p.rich.fonts, &p.links, vertical);
                    let mut j = i + 1;
                    let mut region: Option<RectF> = None;
                    let mut rows = Vec::new();
                    while let Some(t) = ordered.get(j) {
                        if t.kind == UnitKind::Figure {
                            region = Some(region.map_or(t.bbox, |r| r.union(&t.bbox)));
                            j += 1;
                            continue;
                        }
                        let s = t.size();
                        let bodyish = s >= body * 0.97 && t.lines.len() >= 2;
                        if bodyish || caption_kind(&t.text()).is_some() || numbered_heading_depth(&t.text()).is_some() && s >= body * 0.97 {
                            break;
                        }
                        region = Some(region.map_or(t.bbox, |r| r.union(&t.bbox)));
                        rows.extend(t.lines.iter().map(|l| l.text()));
                        j += 1;
                    }
                    match region.and_then(|r| crop(r, opts.image_scale, &mut doc)) {
                        Some(img) => doc.nodes.push(Node::Table { image: img, caption, rows }),
                        None => doc.nodes.push(Node::Paragraph { spans: caption }),
                    }
                    i = j;
                    continue;
                }
                Some(false) => {
                    // Figure caption whose figure came earlier: attach if possible.
                    let spans = spans_of(u, &p.rich.fonts, &p.links, vertical);
                    if let Some(Node::Figure { caption, .. }) = doc.nodes.last_mut()
                        && caption.is_empty()
                    {
                        *caption = spans;
                    } else {
                        doc.nodes.push(Node::Paragraph { spans });
                    }
                    i += 1;
                    continue;
                }
                None => {}
            }
            let math = math_fraction(u, &p.rich.fonts);
            let short = text.chars().count() < 160 && u.lines.len() <= 3;
            let bold = u.frac(|c| c.bold || p.rich.fonts.get(c.font as usize).is_some_and(font_bold));
            let italic = u.frac(|c| p.rich.fonts.get(c.font as usize).is_some_and(font_italic));
            let numbered = numbered_heading_depth(&text);
            // Words set in text fonts mean prose with inline math, not a display formula.
            // Only lowercase words count: operator names such as "MultiHead" or
            // "Concat" are capitalised.
            let prose_words = {
                let mut words = 0;
                for l in &u.lines {
                    let (mut run, mut lower_start) = (0, false);
                    for c in &l.chars {
                        let text_font = !p.rich.fonts.get(c.font as usize).is_some_and(|f| is_math_font(&f.name));
                        if c.c.is_ascii_alphabetic() && text_font {
                            if run == 0 {
                                lower_start = c.c.is_ascii_lowercase();
                            }
                            run += 1;
                        } else {
                            words += (run >= 3 && lower_start) as usize;
                            run = 0;
                        }
                    }
                    words += (run >= 3 && lower_start) as usize;
                }
                words
            };
            let display_math = (prose_words < 4 && (math >= 0.35 || (math >= 0.12 && has_equation_number(&text) && text.chars().count() < 160)))
                || (prose_words <= 1 && math >= 0.2 && u.lines.len() <= 4);
            if !vertical && display_math && u.lines.len() <= 8 && u.chars() >= 3 {
                if let Some(img) = crop(u.bbox, opts.formula_scale, &mut doc) {
                    let (latex, number) = match &opts.formula {
                        Some(f) => {
                            let png = &doc.images[img].png;
                            match image::load_from_memory(png).map(|i| i.to_rgb8()).ok().and_then(|i| f.to_latex(&i).ok()) {
                                Some(l) if !l.is_empty() => {
                                    let (body, num) = strata_ocr::formula::split_equation_number(&l);
                                    (Some(body), num)
                                }
                                _ => (None, None),
                            }
                        }
                        None => (None, None),
                    };
                    doc.nodes.push(Node::Formula { image: img, text: text.clone(), latex, number });
                }
                i += 1;
                continue;
            }
            // Author lists carry many superscript affiliation marks.
            let sups = u
                .lines
                .iter()
                .flat_map(|l| {
                    let med = line_size(l);
                    l.chars.iter().filter(move |c| c.size < med * 0.8 && !c.c.is_whitespace())
                })
                .count();
            let short = short && sups <= 2;
            // A heading has a word of two letters, or is a bare section number ("3.2")
            // whose title follows.
            let wordy = text.split_whitespace().any(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 2);
            let number_only = !text.trim().is_empty() && text.trim().chars().all(|c| c.is_ascii_digit() || c == '.');
            let level = if let Some(c) = u.class {
                // The layout model decides what is a heading; size and numbering give the level.
                let number = number_only
                    && ordered.get(i + 1).is_some_and(|n| {
                        matches!(n.class, Some(TITLE | SECTION_HEADER))
                            && n.text().split_whitespace().any(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 2)
                            && (n.bbox.y0 - u.bbox.y0).abs() < size * 2.0
                    });
                // Large type the model took for running text is still a title.
                let large = c == strata_ocr::layout::TEXT && size >= body * 1.4 && u.lines.len() <= 5 && text.chars().count() < 300;
                if (matches!(c, TITLE | SECTION_HEADER) || large) && (wordy || number) {
                    let level = if c == TITLE || size >= body * 1.4 {
                        let score = size * (text.chars().count().min(150) as f32).sqrt();
                        if pi == 0 && score > title_size {
                            title_size = score;
                            doc.title = text.trim().to_string();
                        }
                        1
                    } else if size >= body * 1.15 {
                        2
                    } else {
                        numbered.map_or(3, |d| (d + 1).min(6))
                    };
                    Some(level)
                } else {
                    None
                }
            } else if !(wordy || number_only) {
                None
            } else if short && size >= body * 1.4 {
                // Prefer the long title over a large journal banner.
                let score = size * (text.chars().count().min(150) as f32).sqrt();
                if pi == 0 && score > title_size {
                    title_size = score;
                    doc.title = text.trim().to_string();
                }
                Some(1)
            } else if short && size >= body * 1.15 {
                Some(2)
            } else if short && !text.trim_end().ends_with(['.', '。', '．', ',', ';', ':']) && (bold > 0.8 || (italic > 0.8 && numbered.is_some())) {
                Some(match numbered {
                    Some(d) => (d + 1).min(6),
                    None => 3,
                })
            } else {
                None
            };
            let mut spans = spans_of(u, &p.rich.fonts, &p.links, vertical);
            // Run-in heading: "4.1.1. Porous flow bands  Body text..." in one block.
            if level.is_none() {
                let lead: usize = spans.iter().take_while(|s| s.style.bold || s.style.italic || s.text.trim().is_empty()).count();
                let head: String = spans[..lead].iter().map(|s| s.text.as_str()).collect();
                if lead > 0 && lead < spans.len() && head.chars().count() < 120 && let Some(d) = numbered_heading_depth(&head) {
                    let rest = spans.split_off(lead);
                    let hs = spans.into_iter().map(|mut s| {
                        s.style.bold = false;
                        s.style.italic = false;
                        s
                    });
                    doc.nodes.push(Node::Heading { level: (d + 1).min(6), spans: hs.collect() });
                    spans = rest;
                    if let Some(f) = spans.first_mut() {
                        f.text = f.text.trim_start().to_string();
                    }
                }
            }
            if let Some(level) = level {
                let spans: Vec<Span> = spans
                    .into_iter()
                    .map(|mut s| {
                        s.style.bold = false;
                        s.style.italic = false;
                        s
                    })
                    .collect();
                // "3.2.1" and its title extracted as separate blocks.
                if let Some(Node::Heading { level: pl, spans: ps }) = doc.nodes.last_mut()
                    && spans_text(ps).trim().chars().all(|c| c.is_ascii_digit() || c == '.')
                {
                    let num = spans_text(ps).trim().trim_end_matches('.').to_string();
                    let depth = num.split('.').filter(|s| !s.is_empty()).count() as u8;
                    *pl = (depth + 1).clamp(2, 6);
                    ps.clear();
                    ps.push(Span { text: format!("{num} "), style: Style::default(), link: None });
                    ps.extend(spans);
                } else if let Some((ix, pg, bbox, hsize)) = last_heading
                    && u.class.is_some()
                    && ix + 1 == doc.nodes.len()
                    && pg == p.page
                    && (size - hsize).abs() <= hsize * 0.05
                    && numbered.is_none()
                    && ((u.bbox.y0 >= bbox.y1 - 2.0 && u.bbox.y0 - bbox.y1 < size * 0.8 && u.bbox.x0 < bbox.x1 && u.bbox.x1 > bbox.x0)
                        || ((u.bbox.y0 - bbox.y0).abs() < 2.0 && u.bbox.x0 >= bbox.x1 - 1.0 && u.bbox.x0 - bbox.x1 < size * 1.5))
                {
                    // The next line, or the rest of the line, of the same heading.
                    if let Node::Heading { spans: ps, .. } = &mut doc.nodes[ix] {
                        ps.push(Span { text: " ".into(), style: Style::default(), link: None });
                        ps.extend(spans);
                    }
                    last_heading = Some((ix, p.page, bbox.union(&u.bbox), hsize));
                } else {
                    doc.nodes.push(Node::Heading { level: level as u8, spans });
                    if u.class.is_some() {
                        last_heading = Some((doc.nodes.len() - 1, p.page, u.bbox, size));
                    }
                }
            } else if is_list_marker(&text) {
                doc.nodes.push(Node::ListItem { spans });
            } else if u.class == Some(FOOTNOTE) || (!vertical && size <= body * 0.92 && u.bbox.y0 > p.rich.height * 0.6 && u.lines.len() <= 8) {
                doc.nodes.push(Node::Footnote { spans });
            } else {
                // Continuation across a column or page break; figures, tables,
                // footnotes and stray captions that interrupt a paragraph are floats
                // and are skipped.
                let prev = doc.nodes.iter().rposition(|n| match n {
                    Node::PageStart { .. } | Node::Figure { .. } | Node::Table { .. } | Node::Footnote { .. } => false,
                    Node::Paragraph { spans } => caption_kind(&spans_text(spans)).is_none(),
                    _ => true,
                });
                let (at_top, at_bottom) = column_edges(&ordered, i);
                let merge = match prev.map(|ix| &doc.nodes[ix]) {
                    Some(Node::Paragraph { spans: prev_spans }) => {
                        if vertical {
                            let (top, _) = vertical_text_area(&ordered, body);
                            let indented = u.bbox.y0 > top + body * 0.5;
                            last_col_full && !indented
                        } else {
                            let prev_text = spans_text(prev_spans);
                            // A sentence cut at the edge of a column and resumed at the top
                            // of the next one, even with a capital. Only body text continues
                            // a paragraph: figure labels near a column top are short.
                            let cut = last_para.as_ref().is_some_and(|l| Some(l.node) == prev && l.cut_at_edge);
                            let first_indented = u.lines.first().is_some_and(|l| l.bbox.x0 - u.bbox.x0 > body * 0.6);
                            let bodyish = (size - body).abs() <= body * 0.1 && (u.lines.len() >= 2 || text.chars().count() >= 40);
                            // The next lines of the same column, split off by the extractor
                            // (it breaks blocks at a change of font, such as an italic
                            // species name at the start of a line).
                            let adjacent = last_para.as_ref().is_some_and(|l| {
                                Some(l.node) == prev
                                    && l.page == p.page
                                    && l.last_full
                                    && u.bbox.x0 < l.bbox.x1
                                    && u.bbox.x1 > l.bbox.x0
                                    && (u.bbox.x0 - l.bbox.x0).abs() < body
                                    && u.bbox.y0 >= l.bbox.y1 - 2.0
                                    && u.bbox.y0 - l.bbox.y1 < line_gap
                            });
                            continues(&prev_text, &text)
                                || (!ends_sentence(&prev_text) && !first_indented && bodyish && ((cut && at_top) || adjacent))
                        }
                    }
                    _ => false,
                };
                // For the next unit: did this one stop at the edge of its column?
                let last_full = match u.lines.last() {
                    Some(l) if u.lines.len() >= 2 => l.bbox.x1 >= u.bbox.x1 - body * 1.5,
                    Some(l) => {
                        let widest = ordered.iter().filter(|o| o.kind == UnitKind::Text).map(|o| o.bbox.width()).fold(0.0f32, f32::max);
                        l.bbox.width() >= widest * 0.8
                    }
                    None => false,
                };
                let cut_at_edge = at_bottom && last_full;
                if vertical {
                    let (_, bottom) = vertical_text_area(&ordered, body);
                    last_col_full = u.bbox.y1 >= bottom - body * 1.5;
                }
                if merge {
                    let idx = prev.unwrap();
                    if let Node::Paragraph { spans: prev } = &mut doc.nodes[idx] {
                        let cjk = spans_text(prev).chars().last().is_some_and(is_cjk);
                        if !cjk && !vertical {
                            prev.push(Span { text: " ".into(), style: Style::default(), link: None });
                        }
                        prev.extend(spans);
                    }
                    last_para = Some(LastPara { node: idx, cut_at_edge, last_full, page: p.page, bbox: u.bbox });
                } else {
                    doc.nodes.push(Node::Paragraph { spans });
                    last_para = Some(LastPara { node: doc.nodes.len() - 1, cut_at_edge, last_full, page: p.page, bbox: u.bbox });
                }
            }
            i += 1;
        }
        doc.fill_anchors(last_anchor);
        progress(2 * n + pi + 1, total);
    }
    let last = doc.anchors.last().copied().unwrap_or((0, 0.0));
    doc.fill_anchors(last);
    if doc.title.is_empty()
        && let Some(Node::Heading { spans, .. }) = doc.nodes.iter().find(|n| matches!(n, Node::Heading { .. }))
    {
        doc.title = spans_text(spans);
    }
    Ok(Some(doc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_numbers() {
        assert_eq!(numbered_heading_depth("3. Methods"), Some(1));
        assert_eq!(numbered_heading_depth("3.1. Thin section microscopy"), Some(2));
        assert_eq!(numbered_heading_depth("3.1 Thin"), Some(2));
        assert_eq!(numbered_heading_depth("2024 was"), Some(1));
        assert_eq!(numbered_heading_depth("12,3 mm"), None);
    }

    #[test]
    fn captions() {
        assert_eq!(caption_kind("Fig. 1. Location"), Some(false));
        assert_eq!(caption_kind("Table 2"), Some(true));
        assert_eq!(caption_kind("Tables are"), None);
        assert_eq!(caption_kind("図3 地質図"), Some(false));
    }

    #[test]
    fn continuation() {
        assert!(continues("the fracture network was", "formed in the magmatic state"));
        assert!(!continues("It ended.", "then"));
        assert!(continues("これは段落の途中で", "続く文章です。"));
        assert!(!continues("終わり。", "次"));
    }
}
