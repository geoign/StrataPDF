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

mod chars;
mod hyphen;
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
    /// The text is OCR output (ours, or a text layer over a page scan): type
    /// sizes are estimates.
    scan: bool,
    /// Lines found by the layout model: box, class and region (empty without it).
    layout: Vec<(RectF, usize, usize)>,
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x3000..=0x303F)
}

fn is_math_font(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    // STIX is a text family too (STIX-Regular, STIXTwoText set whole papers);
    // its math fonts are "STIXMath", "STIXTwoMath", "STIXSize…", "STIXIntegrals…"
    // and "STIXGeneral" (symbols), matched by "math" or listed here.
    const KEYS: [&str; 21] = [
        "math", "cmmi", "cmsy", "cmex", "msbm", "msam", "symbol", "stixgeneral", "stixsize", "stixintegrals", "stixvariants", "stixnonunicode", "mt extra", "mtextra", "euclid", "mathematicalpi", "txsy", "txmi", "rtxmi", "mtsy", "mtmi",
    ];
    KEYS.iter().any(|k| n.contains(k)) || n.contains("cambria math")
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

/// The combining mark for a spacing accent.
fn combining(c: char) -> Option<char> {
    match c {
        '\u{00B4}' => Some('\u{0301}'),
        '\u{0060}' => Some('\u{0300}'),
        '\u{00A8}' => Some('\u{0308}'),
        '\u{02C6}' => Some('\u{0302}'),
        '\u{02DC}' => Some('\u{0303}'),
        '\u{02C7}' => Some('\u{030C}'),
        '\u{00B8}' => Some('\u{0327}'),
        '\u{02DA}' => Some('\u{030A}'),
        '\u{00AF}' => Some('\u{0304}'),
        '\u{02D8}' => Some('\u{0306}'),
        '\u{02D9}' => Some('\u{0307}'),
        '\u{02DB}' => Some('\u{0328}'),
        '\u{02DD}' => Some('\u{030B}'),
        _ => None,
    }
}

/// Accents drawn as glyphs of their own (a spacing "¨", or a combining mark
/// in the wrong place) moved onto the letter they are drawn over: the
/// neighbouring letter, before or after, that they overlap most. PDFs put
/// them before the letter ("Universit´e") or after it ("Hu¨bscher" draws the
/// "¨" back over the "u").
fn attach_accents(chars: &[crate::rich::RichChar]) -> Vec<crate::rich::RichChar> {
    let accent = |c: char| combining(c).is_some() || ('\u{0300}'..='\u{036F}').contains(&c);
    if !chars.iter().any(|c| accent(c.c)) {
        return chars.to_vec();
    }
    let letter = |k: usize| chars.get(k).is_some_and(|c| c.c.is_alphabetic() && !accent(c.c));
    // Base letter of each accent.
    let mut base: Vec<Option<usize>> = vec![None; chars.len()];
    for (i, c) in chars.iter().enumerate() {
        if !accent(c.c) {
            continue;
        }
        let overlap = |k: usize| {
            let b = &chars[k].bbox;
            b.x1.min(c.bbox.x1) - b.x0.max(c.bbox.x0)
        };
        let cands: Vec<usize> = [i.checked_sub(1), Some(i + 1)].into_iter().flatten().filter(|&k| letter(k)).collect();
        base[i] = cands.into_iter().max_by(|&a, &b| overlap(a).total_cmp(&overlap(b)));
    }
    let mut out = Vec::with_capacity(chars.len());
    for (k, c) in chars.iter().enumerate() {
        if base[k].is_some() {
            continue;
        }
        let marked = base.contains(&Some(k));
        // A dotless i carries the accent of "í" ("Reykjavı́k").
        out.push(if marked && c.c == 'ı' { crate::rich::RichChar { c: 'i', ..*c } } else { *c });
        for (i, b) in base.iter().enumerate() {
            if *b == Some(k) {
                let m = combining(chars[i].c).unwrap_or(chars[i].c);
                out.push(crate::rich::RichChar { c: m, ..*c });
            }
        }
    }
    out
}

/// Precompose spacing accents that PDFs place before the base letter
/// ("Universit´e" -> "Université").
fn fix_accents(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
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
    /// Layout region of most of its text.
    group: Option<usize>,
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

/// Layout class and region of a line: those of the layout line holding most of its characters.
fn line_class(layout: &[(RectF, usize, usize)], l: &RichLine) -> Option<(usize, usize)> {
    let cand: Vec<&(RectF, usize, usize)> = layout.iter().filter(|(r, ..)| overlap_frac(&l.bbox, r) > 0.0).collect();
    match cand.len() {
        0 => None,
        1 => Some((cand[0].1, cand[0].2)),
        _ => {
            let mut votes: HashMap<(usize, usize), usize> = HashMap::new();
            for c in &l.chars {
                let (x, y) = ((c.bbox.x0 + c.bbox.x1) / 2.0, (c.bbox.y0 + c.bbox.y1) / 2.0);
                if let Some((_, k, g)) = cand.iter().find(|(r, ..)| r.x0 <= x && x <= r.x1 && r.y0 <= y && y <= r.y1) {
                    *votes.entry((*k, *g)).or_default() += 1;
                }
            }
            votes.into_iter().max_by_key(|&(k, n)| (n, std::cmp::Reverse(k))).map(|(k, _)| k)
        }
    }
}

/// The class and region of most characters of the lines.
fn majority_class(lines: &[(RichLine, Option<(usize, usize)>)]) -> Option<(usize, usize)> {
    let mut votes: HashMap<(usize, usize), usize> = HashMap::new();
    for (l, c) in lines {
        if let Some(c) = c {
            *votes.entry(*c).or_default() += l.chars.len();
        }
    }
    votes.into_iter().max_by_key(|&(k, n)| (n, std::cmp::Reverse(k))).map(|(k, _)| k)
}

/// Blocks of one layout region (the extractor splits some paragraphs, captions
/// and reference entries into a block per line) become one unit, if they share
/// a column. A reference list taken for one region is split into its entries
/// again by [`regroup_references`].
fn merge_regions(units: Vec<Unit>) -> Vec<Unit> {
    let mut out: Vec<Unit> = Vec::with_capacity(units.len());
    for u in units {
        // Type of a clearly different size (a section label over a title) stays apart.
        let size = u.size();
        if let Some(g) = u.group
            && let Some(o) = out.iter_mut().find(|o| {
                let os = o.size();
                o.group == Some(g)
                    && o.kind == UnitKind::Text
                    && o.bbox.x0 < u.bbox.x1
                    && u.bbox.x0 < o.bbox.x1
                    && (os - size).abs() <= os.max(size) * 0.15
                    // Within one column: joining must not widen the wider block by more
                    // than a few ems (ragged line ends do; a region spanning two columns
                    // would otherwise interleave them).
                    && o.bbox.union(&u.bbox).width() <= o.bbox.width().max(u.bbox.width()) + size.max(os) * 4.0
            })
        {
            o.bbox = o.bbox.union(&u.bbox);
            o.lines.extend(u.lines);
            o.lines.sort_by(|a, b| a.bbox.y0.total_cmp(&b.bbox.y0).then(a.bbox.x0.total_cmp(&b.bbox.x0)));
            continue;
        }
        out.push(u);
    }
    out
}

/// The pieces of one visual line joined into one line (horizontal text):
/// consecutive lines of a row that sit side by side, closer than a word space
/// or two (a gap wider than half an em is a column gutter or a tab stop,
/// unless one side is a short fragment such as a superscript or a number). A
/// space goes between them where the gap is a word space.
fn join_rows(lines: Vec<RichLine>) -> Vec<RichLine> {
    let mut out: Vec<RichLine> = Vec::with_capacity(lines.len());
    for l in lines {
        if let Some(prev) = out.last_mut()
            && !prev.vertical
            && !l.vertical
            && prev.dir[1].abs() < 0.1
            && l.dir[1].abs() < 0.1
            && l.bbox.y1.min(prev.bbox.y1) - l.bbox.y0.max(prev.bbox.y0) >= 0.5 * l.bbox.height().min(prev.bbox.height())
            && l.bbox.x0 >= prev.bbox.x1 - 1.0
            && {
                let em = line_size(&l).max(line_size(prev));
                let gap = l.bbox.x0 - prev.bbox.x1;
                let short = |x: &RichLine| x.chars.iter().filter(|c| !c.c.is_whitespace()).count() <= 3;
                gap < em * 0.6 || ((short(&l) || short(prev)) && gap < em * 1.5)
            }
        {
            let em = line_size(&l).max(line_size(prev));
            let gap = l.bbox.x0 - prev.bbox.x1;
            let spaced = prev.chars.last().is_some_and(|c| c.c == ' ') || l.chars.first().is_some_and(|c| c.c == ' ');
            if gap > em * 0.15 && !spaced
                && let Some(last) = prev.chars.last().copied()
            {
                let mut sp = last;
                sp.c = ' ';
                sp.bbox.x0 = prev.bbox.x1;
                sp.bbox.x1 = l.bbox.x0;
                prev.chars.push(sp);
            }
            prev.bbox = prev.bbox.union(&l.bbox);
            prev.chars.extend(l.chars);
            prev.joined = l.joined;
            continue;
        }
        out.push(l);
    }
    out
}

/// "Almendros, J., Wilcock, W., ..." or "Av´e Lallemant, H.G., ...": the start of a
/// reference entry (a surname, a comma, then initials).
fn author_start(t: &str) -> bool {
    let mut words = t.split_whitespace();
    // The surname may have several words ("Av´e Lallemant,", "de Ronde,").
    for _ in 0..3 {
        let Some(w) = words.next() else { return false };
        if w.ends_with(',') && w.chars().filter(|c| c.is_alphabetic()).count() >= 2 {
            return words.next().is_some_and(|i| {
                let letters: Vec<char> = i.chars().filter(|c| c.is_alphabetic()).collect();
                !letters.is_empty() && letters.iter().all(|c| c.is_uppercase()) && letters.len() <= 3 && i.contains('.')
            });
        }
    }
    false
}

/// Reference lists set with a hanging indent, rebuilt from their lines: the
/// extractor cuts them into blocks that do not follow the entries (a block per
/// continuation line, or every other line of a column in one block). A column
/// where at least three lines start with a name and initials at the same left
/// edge is such a list; its blocks of the same type size whose lines either
/// start an entry at that edge or are indented are pooled, their lines sorted
/// top to bottom, and each line at the edge starts an entry. Body text (plain
/// lines at the edge), headings and entries without personal names keep their blocks.
fn regroup_references(units: Vec<Unit>) -> Vec<Unit> {
    let text: Vec<usize> = (0..units.len()).filter(|&i| units[i].kind == UnitKind::Text && !units[i].lines.is_empty()).collect();
    let starts: Vec<(RectF, f32)> = text
        .iter()
        .flat_map(|&i| units[i].lines.iter().map(move |l| (l, i)))
        .filter(|(l, _)| author_start(&l.text()))
        .map(|(l, i)| (l.bbox, units[i].size()))
        .collect();
    // Left edges shared by at least three entry starts.
    let mut edges: Vec<(f32, f32, f32, usize)> = Vec::new(); // left, right, size, count
    for (b, size) in &starts {
        match edges.iter_mut().find(|e| (e.0 - b.x0).abs() < 1.5) {
            Some(e) => {
                e.1 = e.1.max(b.x1);
                e.3 += 1;
            }
            None => edges.push((b.x0, b.x1, *size, 1)),
        }
    }
    let edges: Vec<(f32, f32, f32)> = edges.into_iter().filter(|e| e.3 >= 3).map(|e| (e.0, e.1, e.2)).collect();
    if edges.is_empty() {
        return units;
    }
    let mut slots: Vec<Option<Unit>> = units.into_iter().map(Some).collect();
    let mut rebuilt: Vec<Unit> = Vec::new();
    for (left, right, size) in edges {
        let tol = size * 0.5;
        let at_edge = |x: f32| (x - left).abs() < tol;
        let indented = |x: f32| x - left > tol && x - left < size * 4.0;
        // Blocks of the list: every line at the edge or the indent, same size, within the column.
        let members: Vec<usize> = text
            .iter()
            .copied()
            .filter(|&i| {
                let Some(u) = slots[i].as_ref() else { return false };
                !matches!(u.class, Some(TITLE | SECTION_HEADER | PICTURE | CAPTION))
                    && (u.size() - size).abs() <= size * 0.1
                    && u.lines.iter().all(|l| (at_edge(l.bbox.x0) || indented(l.bbox.x0)) && l.bbox.x1 <= right + size * 2.0)
                    // Lines at the edge mostly start entries (body text has plain lines
                    // there; some entries start with an organisation's name).
                    && {
                        let edge: Vec<&RichLine> = u.lines.iter().filter(|l| at_edge(l.bbox.x0)).collect();
                        edge.iter().filter(|l| author_start(&l.text())).count() * 5 >= edge.len() * 3
                    }
            })
            .collect();
        if members.is_empty() {
            continue;
        }
        let first = members[0];
        let (class, group) = slots[first].as_ref().map(|u| (u.class, u.group)).unwrap_or((None, None));
        let mut lines: Vec<RichLine> = members.iter().filter_map(|&i| slots[i].take()).flat_map(|u| u.lines).collect();
        lines.sort_by(|a, b| a.bbox.y0.total_cmp(&b.bbox.y0).then(a.bbox.x0.total_cmp(&b.bbox.x0)));
        let mut entries: Vec<Vec<RichLine>> = Vec::new();
        for l in lines {
            match entries.last_mut() {
                Some(e) if !at_edge(l.bbox.x0) => e.push(l),
                _ => entries.push(vec![l]),
            }
        }
        for e in entries {
            let bbox = e.iter().skip(1).fold(e[0].bbox, |a, l| a.union(&l.bbox));
            rebuilt.push(Unit { kind: UnitKind::Text, bbox, lines: e, class, group });
        }
    }
    let mut out: Vec<Unit> = slots.into_iter().flatten().collect();
    out.extend(rebuilt);
    out
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
    // Line numbers count up in steps of 1, 5 or 10 (a column of table values does not).
    let counts_up = |g: &[usize]| {
        let mut v: Vec<(f32, u32)> = g.iter().filter_map(|&i| units[i].text().trim().parse::<u32>().ok().map(|n| (units[i].bbox.y0, n))).collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        let steps: Vec<i64> = v.windows(2).map(|w| w[1].1 as i64 - w[0].1 as i64).collect();
        let regular = steps.iter().filter(|&&d| matches!(d, 1 | 2 | 5 | 10)).count();
        !steps.is_empty() && regular * 10 >= steps.len() * 7
    };
    groups
        .into_iter()
        .filter(|g| g.1.len() >= 8 && counts_up(&g.1) && one_side(g.1.iter().map(|&i| units[i].bbox.x0).fold(f32::INFINITY, f32::min), g.0))
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
    // Lines of the margin bands with their text length and whether they repeat.
    let margin_lines: Vec<(RectF, usize, bool)> = p
        .rich
        .blocks
        .iter()
        .flat_map(|b| match b {
            RichBlock::Text { lines, .. } => lines.iter().collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .filter(|l| l.bbox.y1 < h * 0.15 || l.bbox.y0 > h * 0.85)
        .map(|l| {
            let t = l.text();
            (l.bbox, t.trim().chars().count(), repeated.get(&digits_key(&t)).copied().unwrap_or(0) >= threshold)
        })
        .collect();
    // A running head stands alone in its band (a page number or another running
    // head beside it at most); a repeated fragment inside running text does not.
    let alone_in_band = |b: &RectF| {
        !margin_lines.iter().any(|(o, len, rep)| {
            let v = o.y1.min(b.y1) - o.y0.max(b.y0);
            (o.x0 - b.x0).abs() + (o.y0 - b.y0).abs() > 0.5 && v > 0.5 * o.height().min(b.height()) && *len > 8 && !rep
        })
    };
    for b in &p.rich.blocks {
        match b {
            RichBlock::Text { lines, .. } => {
                let kept: Vec<(RichLine, Option<(usize, usize)>)> = lines
                    .iter()
                    .map(|l| (l, line_class(&p.layout, l)))
                    .filter(|(l, class)| {
                        // Running heads and page numbers found by the layout model.
                        if opts.strip_headers && matches!(class, Some((PAGE_HEADER | PAGE_FOOTER, _))) && (l.bbox.y1 < h * 0.15 || l.bbox.y0 > h * 0.85) {
                            return false;
                        }
                        if opts.strip_headers && (l.bbox.y1 < h * 0.15 || l.bbox.y0 > h * 0.85) {
                            let t = l.text();
                            let k = digits_key(&t);
                            if repeated.get(&k).copied().unwrap_or(0) >= threshold && n_pages > 1 && k.chars().filter(|&c| c != '#').count() >= 3 && alone_in_band(&l.bbox) {
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
                // Split where the layout role or region changes (the extractor's blocks
                // can run from one paragraph or reference entry into the next); lines
                // without a class go with the previous ones. Blocks of one region are
                // joined again by `merge_regions`.
                let split_groups = std::env::var("STRATA_NO_GROUP_SPLIT").is_err();
                let mut runs: Vec<Vec<(RichLine, Option<(usize, usize)>)>> = Vec::new();
                let mut cur_role: Option<u8> = None;
                let mut cur_group: Option<usize> = None;
                let mut prev_size: Option<f32> = None;
                for (l, c) in kept {
                    let r = c.map(|(c, _)| role(c));
                    let g = c.map(|(_, g)| g);
                    let same_group = !split_groups || g.is_none() || cur_group.is_none_or(|cg| Some(cg) == g);
                    if g.is_some() {
                        cur_group = g;
                    }
                    // A change of type size (a title over its authors, a heading over
                    // its paragraph) ends a run; OCR sizes are estimates.
                    let ls = line_size(&l);
                    let same_size = p.scan || prev_size.is_none_or(|ps| (ls - ps).abs() <= ps.max(ls) * 0.25);
                    prev_size = Some(ls);
                    match (runs.last_mut(), r) {
                        (Some(run), Some(r)) if cur_role.is_none_or(|cr| cr == r) && same_group && same_size => {
                            cur_role = Some(r);
                            run.push((l, c));
                        }
                        (Some(run), None) if same_size => run.push((l, c)),
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
                        let cg = majority_class(&run);
                        units.push(Unit { kind: UnitKind::Text, bbox, lines: run.into_iter().map(|(l, _)| l).collect(), class: cg.map(|c| c.0), group: cg.map(|c| c.1) });
                    }
                }
            }
            RichBlock::Image { bbox } => {
                // A page-sized image under a text layer is the scan of an OCRed page.
                let background = bbox.width() * bbox.height() > page_area * 0.7 && text_chars > 100;
                if !background && bbox.width() * bbox.height() > page_area * 0.005 {
                    units.push(Unit { kind: UnitKind::Figure, bbox: *bbox, lines: Vec::new(), class: None, group: None });
                }
            }
            _ => {}
        }
    }
    // Line numbers go first: a region can hold them (the layout lines of some
    // manuscripts start with the number), and merged into text they would stay.
    let numbers = line_number_column(&units, body);
    let manuscript = !numbers.is_empty();
    let mut k = 0;
    units.retain(|_| {
        k += 1;
        !numbers.contains(&(k - 1))
    });
    let mut units = merge_regions(units);
    if std::env::var("STRATA_NO_REGROUP").is_err() {
        units = regroup_references(units);
    }
    if !vertical {
        for u in units.iter_mut().filter(|u| u.kind == UnitKind::Text && u.class != Some(strata_ocr::layout::TABLE)) {
            u.lines = join_rows(std::mem::take(&mut u.lines));
        }
    }
    for r in vector_figures(&p.rich.blocks, page_area) {
        units.push(Unit { kind: UnitKind::Figure, bbox: r, lines: Vec::new(), class: None, group: None });
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
    // A "figure" holding paragraphs of running text is a page decoration (a
    // frame, a coloured box, a background drawing), not a figure.
    figs.retain(|f| {
        let prose: usize = texts
            .iter()
            .flatten()
            .filter(|u| overlap_frac(&u.bbox, f) > 0.8 && u.lines.len() >= 3 && (u.size() - body).abs() < body * 0.1)
            .map(|u| u.chars())
            .sum();
        prose < 400
    });
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
    // A line of running text the model put into a picture stays text.
    let prose_line = |u: &Unit| {
        let t = u.text();
        t.split_whitespace().count() >= 8 && prose_like(&t)
    };
    for (i, slot) in texts.iter().enumerate() {
        if let Some(u) = slot
            && u.class == Some(PICTURE)
            && !prose_line(u)
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
        // Drawings around the text, several labels together, or a scan (the
        // drawing is in the page image): a figure.
        if graphics >= 1 || members.len() >= 3 || p.scan {
            for i in members {
                texts[i] = None;
            }
            figs.push(region);
        }
    }
    // A figure caption with no figure next to it: the figure is the space above
    // it, up to the running text (or the top of the page), across the caption's
    // width, if drawings (or, on a scan, the page image) fill it. Its labels go
    // with it.
    let prose_block = |u: &Unit| u.lines.len() >= 2 && (prose_like(&u.text()) || ((u.size() - body).abs() < body * 0.1 && !p.scan)) && caption_kind(&u.text()).is_none();
    let heading_block = |u: &Unit| {
        let t = u.text();
        u.lines.len() <= 3 && (matches!(u.class, Some(TITLE | SECTION_HEADER)) || u.size() >= body * 1.1 || heading_number(&t).is_some() || caps_heading(&t))
    };
    let caption_boxes: Vec<RectF> = texts.iter().flatten().filter(|u| caption_kind(&u.text()) == Some(false)).map(|u| u.bbox).collect();
    for cb in caption_boxes {
        if std::env::var("STRATA_NO_CAPFIG").is_ok() {
            break;
        }
        let near_figure = figs.iter().any(|f| {
            let h = f.x1.min(cb.x1) - f.x0.max(cb.x0);
            let v = f.y1.min(cb.y1) - f.y0.max(cb.y0);
            (h > 0.0 && (cb.y0 - f.y1).abs().min((f.y0 - cb.y1).abs()) < body * 4.0) || (v > 0.0 && (cb.x0 - f.x1).abs().min((f.x0 - cb.x1).abs()) < body * 4.0)
        });
        if near_figure {
            continue;
        }
        let top = texts
            .iter()
            .flatten()
            .filter(|u| u.bbox.y1 <= cb.y0 + 1.0 && u.bbox.x0 < cb.x1 && u.bbox.x1 > cb.x0 && (prose_block(u) || heading_block(u) || caption_kind(&u.text()).is_some()))
            .map(|u| u.bbox.y1)
            .fold(h * 0.06, f32::max);
        let region = RectF { x0: cb.x0, y0: top + 2.0, x1: cb.x1, y1: cb.y0 - 2.0 };
        if region.height() < (body * 4.0).max(40.0) {
            continue;
        }
        let drawn = p.scan
            || p.rich.blocks.iter().any(|b| matches!(b, RichBlock::Vector { bbox } | RichBlock::Image { bbox } if overlap_frac(bbox, &region) > 0.5 && bbox.width() * bbox.height() < page_area * 0.7));
        if drawn {
            // Grow sideways to the drawings that stick out of the caption's width.
            let mut r = region;
            for b in &p.rich.blocks {
                if let RichBlock::Vector { bbox } | RichBlock::Image { bbox } = b
                    && overlap_frac(bbox, &region) > 0.3
                    && bbox.width() * bbox.height() < page_area * 0.7
                {
                    r = r.union(&RectF { x0: bbox.x0, y0: r.y0, x1: bbox.x1, y1: r.y1 });
                }
            }
            figs.push(r);
        }
    }
    let mut out: Vec<Unit> = Vec::new();
    for u in texts.into_iter().flatten() {
        let inside = figs.iter().any(|f| overlap_frac(&u.bbox, f) > 0.8);
        // Running text survives inside a figure's box (on a scan, sizes vary).
        let bodyish = (u.lines.len() >= 3 && (u.size() - body).abs() < body * 0.1) || (prose_like(&u.text()) && u.text().split_whitespace().count() >= 8);
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
    // Figures found by different means overlap (the panels of a figure and the
    // drawing around them): one figure each.
    // Panels stacked one above the other with nothing but a narrow gap between
    // them are one figure too.
    let stacked = |a: &RectF, b: &RectF| {
        let h_overlap = a.x1.min(b.x1) - a.x0.max(b.x0);
        let (top, bottom) = if a.y0 <= b.y0 { (a, b) } else { (b, a) };
        let gap = bottom.y0 - top.y1;
        let band = RectF { x0: a.x0.max(b.x0), y0: top.y1, x1: a.x1.min(b.x1), y1: bottom.y0 };
        h_overlap > 0.6 * a.width().min(b.width())
            && gap < body * 1.5
            && !out.iter().any(|u| u.bbox.x0 < band.x1 && u.bbox.x1 > band.x0 && u.bbox.y0 < band.y1 - 1.0 && u.bbox.y1 > band.y0 + 1.0)
    };
    let mut merged: Vec<RectF> = Vec::with_capacity(figs.len());
    for f in figs {
        merged.push(f);
        loop {
            let last = *merged.last().unwrap();
            let Some(j) = (0..merged.len() - 1).find(|&j| overlap_frac(&last, &merged[j]) > 0.5 || overlap_frac(&merged[j], &last) > 0.5 || stacked(&last, &merged[j])) else { break };
            let o = merged.remove(j);
            *merged.last_mut().unwrap() = last.union(&o);
        }
    }
    out.extend(merged.into_iter().map(|bbox| Unit { kind: UnitKind::Figure, bbox, lines: Vec::new(), class: None, group: None }));
    (out, manuscript)
}

/// Running text: at least one in eight words is a function word (English and a
/// few European languages), as figure labels and table cells are not; CJK
/// text with sentence punctuation.
fn prose_like(t: &str) -> bool {
    const FUNCTION: [&str; 40] = [
        "the", "of", "and", "in", "to", "a", "is", "are", "was", "were", "for", "on", "with", "by", "as", "at", "from", "that", "this", "be", "or", "an", "which", "we", "it", "its", "not", "has", "have", "der", "die", "und", "das", "le", "la", "les", "et", "des", "el", "los",
    ];
    let words: Vec<String> = t.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase()).filter(|w| !w.is_empty()).collect();
    if t.chars().filter(|&c| is_cjk(c)).count() >= 10 {
        return t.contains(['。', '、', '，', '．']);
    }
    words.len() >= 4 && words.iter().filter(|w| FUNCTION.contains(&w.as_str())).count() * 8 >= words.len()
}

fn caption_kind(t: &str) -> Option<bool> {
    // Some(true) for tables, Some(false) for figures.
    let t = t.trim_start();
    // "Figure 4 shows…", "Table 2 summarizes…": running text, not a caption (the
    // word after the number is a lowercase verb, not a title or a panel letter).
    let verb_after_number = || {
        let mut it = t.split_whitespace();
        it.next();
        let mut w = it.next().unwrap_or("");
        if w.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            // "Figure 2." / "Fig. 3:" end the label: a caption, whatever follows.
            if w.ends_with(['.', ':', '|']) {
                return false;
            }
            w = it.next().unwrap_or("");
        }
        w.chars().next().is_some_and(char::is_lowercase) && w.chars().filter(|c| c.is_alphabetic()).count() >= 3
    };
    // "Fig.8(a)に求められた…": a Japanese sentence (a particle follows the number).
    let particle_after_number = || {
        let rest = t.trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == '.' || c == ' ' || c == '図' || c == '表');
        let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit() || ('０'..='９').contains(&c) || matches!(c, '(' | ')' | '（' | '）' | '.' | '-' | '–' | ',' | ' ') || c.is_ascii_lowercase());
        rest.chars().next().is_some_and(|c| ('\u{3040}'..='\u{309F}').contains(&c))
    };
    if (!t.starts_with(['表', '図']) && verb_after_number()) || particle_after_number() {
        return None;
    }
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

/// Section numbering at the start of a heading, with its depth: "2.1 Methods",
/// "IV. Discussion", "1) Setting", "５．結論", "I. はじめに".
fn heading_number(t: &str) -> Option<u8> {
    if let Some(d) = numbered_heading_depth(t) {
        return Some(d);
    }
    let t = t.trim_start();
    let head: String = t.chars().take_while(|c| !c.is_whitespace()).collect();
    let rest = t[head.len()..].trim_start();
    let title_follows = |s: &str| s.chars().next().is_some_and(|c| c.is_alphabetic());
    // Roman numerals: "IV." (not "I" as a word).
    if let Some(r) = head.strip_suffix('.')
        && !r.is_empty()
        && r.chars().all(|c| matches!(c, 'I' | 'V' | 'X'))
        && title_follows(rest)
    {
        return Some(1);
    }
    // "1)" or full-width "１．" / "1．" followed by a title, with or without a space.
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit() || ('０'..='９').contains(c)).collect();
    if !digits.is_empty() && digits.chars().count() <= 2 {
        let after = &t[digits.len()..];
        if let Some(r) = after.strip_prefix('．')
            && title_follows(r.trim_start())
        {
            return Some(1);
        }
        if let Some(r) = after.strip_prefix(')').or_else(|| after.strip_prefix('）'))
            && title_follows(r.trim_start())
        {
            return Some(2);
        }
    }
    None
}

/// Front-matter headings that are headings even before the body starts.
fn is_front_heading(t: &str) -> bool {
    let l = t.trim().trim_end_matches([':', '.']).to_lowercase();
    ["abstract", "summary", "key points", "keywords", "key words", "highlights", "plain language summary", "introduction", "要旨", "概要", "要約", "はじめに", "序論", "緒言"].contains(&l.as_str())
}

/// An author line: personal names ("A. B. Surname", "Firstname Surname",
/// "SURNAME") separated by commas, "and", "&", "·" or affiliation marks. Each
/// name is a short group of name-like words; a Title Case title has long groups
/// and function words. Only meaningful in the front matter.
fn is_byline(t: &str) -> bool {
    if t.chars().count() > 600 || is_front_heading(t) {
        return false;
    }
    // A short CJK name ("勝間田明男").
    let compact: String = t.chars().filter(|c| !c.is_whitespace() && !matches!(c, '*' | '＊')).collect();
    if (2..=6).contains(&compact.chars().count()) && compact.chars().all(is_cjk) {
        return true;
    }
    let cleaned: String = t
        .chars()
        .map(|c| if matches!(c, ',' | ';' | '·' | '•' | '&' | '*' | '＊' | '∗' | '⁎' | '†' | '‡' | '§' | '¶' | '#' | '(' | ')') || c.is_ascii_digit() { '|' } else { c })
        .collect();
    let cleaned = cleaned.replace(" and ", "|").replace(" und ", "|").replace(" et ", "|");
    let groups: Vec<Vec<&str>> = cleaned
        .split('|')
        .map(|g| g.split_whitespace().filter(|w| !matches!(*w, "and" | "und" | "et") && w.chars().any(char::is_alphabetic)).collect::<Vec<_>>())
        .filter(|g| !g.is_empty())
        .collect();
    if groups.is_empty() {
        return false;
    }
    const PARTICLES: [&str; 18] = ["de", "da", "di", "del", "della", "van", "von", "der", "den", "la", "le", "du", "dos", "das", "y", "e", "bin", "ter"];
    let name_like = |w: &str| {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '.');
        let letters: Vec<char> = w.chars().filter(|c| c.is_alphabetic()).collect();
        if letters.is_empty() || letters.len() > 16 {
            return false;
        }
        // Initials: "A.", "A.B.", "J.-C."
        let initials = w.contains('.') && letters.len() <= 3 && letters.iter().all(|c| c.is_uppercase());
        // "Smith", "SMITH", "McCloskey", "LaFemina", "D’Antonio".
        let capitalised = letters[0].is_uppercase() && (letters[1..].iter().any(|c| c.is_lowercase()) || letters.iter().all(|c| c.is_uppercase()));
        // A lone lowercase letter is an affiliation mark ("Smith a").
        initials || capitalised || PARTICLES.contains(&w) || (letters.len() == 1 && letters[0].is_lowercase())
    };
    // Each group a name of one to six words ("Jr." alone is a group too).
    let good = groups.iter().filter(|g| g.len() <= 6 && g.iter().all(|w| name_like(w))).count();
    let words: usize = groups.iter().map(|g| g.len()).sum();
    // A single name has two to four words.
    if groups.len() == 1 {
        return good == 1 && (2..=4).contains(&words);
    }
    good * 10 >= groups.len() * 9
}

/// A heading set in capitals ("VOLCANIC FLOW TYPES AND DISTRIBUTION"), as older
/// papers do: short, nearly all letters uppercase, no closing punctuation.
fn caps_heading(t: &str) -> bool {
    let t = t.trim();
    let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
    letters.len() >= 6
        && t.chars().count() <= 100
        && letters.iter().filter(|c| c.is_uppercase()).count() * 10 >= letters.len() * 9
        && !t.ends_with([',', ';', '.'])
        && t.chars().filter(char::is_ascii_digit).count() <= 4
}

/// An author line with the marks of one: commas or "and" between names,
/// initials, affiliation numbers or asterisks.
fn is_strong_byline(t: &str) -> bool {
    let initials = t.split_whitespace().any(|w| {
        let l: Vec<char> = w.chars().filter(|c| c.is_alphabetic()).collect();
        w.contains('.') && !l.is_empty() && l.len() <= 3 && l.iter().all(|c| c.is_uppercase())
    });
    let separators = t.chars().filter(|c| matches!(c, ',' | '·' | ';')).count();
    is_byline(t) && (initials || separators >= 2 || t.contains(['*', '＊', '∗', '†', '‡']) || t.chars().any(|c| c.is_ascii_digit()))
}

/// The opening of a note (affiliation, correspondence, dates, editor, copyright,
/// supplementary material), as opposed to running text.
fn note_opening(t: &str) -> bool {
    let t = t.trim_start();
    let l = t.to_lowercase();
    const STARTS: [&str; 22] = [
        "corresponding author",
        "*corresponding",
        "* corresponding",
        "e-mail",
        "email:",
        "email address",
        "received",
        "accepted",
        "editorial handling",
        "handling editor",
        "responsible editor",
        "communicated by",
        "citation:",
        "present address",
        "current address",
        "tel.",
        "tel:",
        "fax:",
        "electronic supplementary material",
        "supplementary information",
        "additional supporting information",
        "orcid",
    ];
    STARTS.iter().any(|s| l.starts_with(s)) || t.starts_with(['*', '†', '‡', '§', '¶', '©', '∗'])
}

/// "References", "Literature Cited", "参考文献"...
fn is_references_heading(t: &str) -> bool {
    let l = t.trim().trim_end_matches([':', '.']).to_lowercase();
    let l = l.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' ');
    ["references", "reference", "bibliography", "literature cited", "references cited", "cited literature", "works cited", "参考文献", "引用文献", "文献", "references and notes"].contains(&l)
}

/// Publisher boilerplate that is not part of the text: copyright and licence
/// lines, banners, download stamps. Shown as notes and kept out of paragraphs.
fn is_boilerplate(t: &str) -> bool {
    let l = t.trim().to_lowercase();
    if l.chars().count() > 500 {
        return false;
    }
    const STARTS: [&str; 14] = [
        "copyright",
        "©",
        "paper number",
        "printed in",
        "contents lists available at",
        "journal homepage",
        "this article is protected by copyright",
        "this article has been accepted for publication",
        "downloaded from",
        "this is an open access article",
        "open access this article",
        "published by elsevier",
        "all rights reserved",
        "crown copyright",
    ];
    STARTS.iter().any(|s| l.starts_with(s)) || l.contains("all rights reserved") || (l.contains("creative commons") && l.contains("licen") && l.chars().count() < 400)
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

/// The last token of `prev` is a URL or DOI cut at a line end, and `next`
/// carries on with it: no space between them.
fn url_continues(prev: &str, next: &str) -> bool {
    let Some(tok) = prev.split_whitespace().last() else { return false };
    let url = tok.contains("://") || tok.starts_with("www.") || tok.starts_with("doi:") || tok.starts_with("doi.org") || (tok.starts_with("10.") && tok.contains('/'));
    let open_end = tok.ends_with(['/', '.', '_', '-', '=', '?', '&', '#']);
    let first = next.trim_start().chars().next();
    url && open_end && first.is_some_and(|c| !c.is_uppercase() && !c.is_whitespace())
}

/// Join text that ends in a hyphen to the text that follows it (the next line,
/// or the next part of a paragraph): without a space, dropping the hyphen of a
/// word broken in two and keeping that of a compound. Returns false, and does
/// nothing, when `prev` does not end in a hyphen after a letter or digit.
fn join_hyphenated(prev: &mut Vec<Span>, next: &str, lex: &hyphen::Lexicon) -> bool {
    while prev.len() > 1 && prev.last().is_some_and(|s| s.text.trim().is_empty()) {
        prev.pop();
    }
    let Some(s) = prev.last_mut() else { return false };
    let t = s.text.trim_end();
    let Some(h) = t.chars().last().filter(|&c| hyphen::is_hyphen(c) || c == '–') else { return false };
    let before = t[..t.len() - h.len_utf8()].chars().last();
    if !before.is_some_and(char::is_alphanumeric) || next.trim_start().is_empty() {
        return false;
    }
    let keep = match (hyphen::word_before_hyphen(t), hyphen::word_after(next)) {
        // An en dash between words or numbers ("lithosphere–asthenosphere") stays.
        _ if h == '–' => true,
        _ if h == '\u{00AD}' => false,
        // "GE-" / "OFON": the joined word is in the document.
        (Some(a), None) if hyphen::word_after_any(next).is_some_and(|b| lex.has_word(&format!("{a}{b}"))) => false,
        (Some(a), Some(b)) => lex.keep_hyphen(a, b),
        // Before a capital or a digit ("Plio-" / "Pleistocene", "1980-" / "1990"),
        // or after a digit ("SO2-" / "rich"): a compound.
        _ => true,
    };
    let len = t.len();
    s.text.truncate(len);
    if !keep {
        s.text.truncate(len - h.len_utf8());
    }
    true
}

/// Split a unit's characters into styled spans.
fn spans_of(u: &Unit, fonts: &[FontInfo], links: &[(RectF, String)], vertical: bool, scan: bool, lex: &hyphen::Lexicon) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut prev_last: Option<char> = None;
    for (li, l) in u.lines.iter().enumerate() {
        let med = line_size(l);
        let lcy = (l.bbox.y0 + l.bbox.y1) * 0.5;
        let first = l.chars.first().map(|c| c.c);
        if li > 0 && !vertical && join_hyphenated(&mut spans, &l.text(), lex) {
            // Joined at a hyphen, without a space.
        } else if li > 0 && !vertical && url_continues(&spans_text(&spans), &l.text()) {
            // A URL or DOI broken at a line end ("https://doi." / "org/10…").
            while let Some(s) = spans.last_mut()
                && s.text.ends_with(' ')
            {
                s.text.pop();
            }
        } else if li > 0 {
            let join_tight = vertical || matches!((prev_last, first), (Some(a), Some(b)) if is_cjk(a) || is_cjk(b));
            if !join_tight
                && let Some(s) = spans.last_mut()
                && !s.text.ends_with(' ')
            {
                // Not inside a superscript ("Vaswani∗"): those are trimmed below.
                if s.style.sup || s.style.sub {
                    spans.push(Span { text: " ".into(), style: Style::default(), link: None });
                } else {
                    s.text.push(' ');
                }
            }
        }
        let cleaned = if vertical { l.chars.clone() } else { chars::clean_line(&l.chars, fonts, scan, is_math_font) };
        for c in &attach_accents(&cleaned) {
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
                // A space after a superscript or subscript is a plain space (trimmed
                // off the superscript below it would be lost: "km3of").
                Some(s) if c.c == ' ' && s.link == link && !(s.style.sup || s.style.sub) => s.text.push(' '),
                _ => spans.push(Span { text: c.c.to_string(), style, link }),
            }
        }
        prev_last = l.chars.last().map(|c| c.c);
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
        use unicode_normalization::UnicodeNormalization;
        s.text = fix_accents(&s.text).nfc().collect();
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

/// Letters set apart with spacing ("G E O P H YS I C S") joined into the word.
/// A run of at least four one- or two-letter capital tokens, most of them single.
fn collapse_letterspacing(s: &str) -> String {
    let tokens: Vec<&str> = s.split(' ').collect();
    let caps = |t: &str| (1..=2).contains(&t.chars().count()) && t.chars().all(|c| c.is_uppercase());
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let mut j = i;
        while j < tokens.len() && caps(tokens[j]) {
            j += 1;
        }
        let singles = tokens[i..j].iter().filter(|t| t.chars().count() == 1).count();
        if j - i >= 4 && singles * 3 >= (j - i) * 2 {
            out.push(tokens[i..j].concat());
            i = j;
        } else {
            out.push(tokens[i].to_string());
            i += 1;
        }
    }
    out.join(" ")
}

fn spans_text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn ends_sentence(t: &str) -> bool {
    let t = t.trim_end();
    t.ends_with(['.', '!', '?', ':', '。', '．', '！', '？', '」', '』', '）', ')']) || t.is_empty()
}

#[cfg(test)]
fn continues(prev: &str, next: &str) -> bool {
    continues_strongly(prev, next) || continues_weakly(prev, next)
}

/// `next` carries on the sentence `prev` left open: it starts in lowercase or
/// with a comma or semicolon.
fn continues_strongly(prev: &str, next: &str) -> bool {
    let Some(first) = next.trim_start().chars().next() else { return false };
    !ends_sentence(prev) && !next.starts_with(['\u{3000}', ' ']) && (first.is_lowercase() || matches!(first, ',' | ';'))
}

/// `next` may carry on `prev`: it starts with a CJK character, a digit or an
/// opening bracket, as the next sentence of a note or a list can too; the
/// layout has to confirm it.
fn continues_weakly(prev: &str, next: &str) -> bool {
    let Some(first) = next.trim_start().chars().next() else { return false };
    !ends_sentence(prev) && !next.starts_with(['\u{3000}', ' ']) && (is_cjk(first) || first.is_ascii_digit() || matches!(first, '(' | '[' | '、'))
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
    // Only the running text of the column counts: captions, figure labels,
    // tables and notes above or below it are floats.
    let size = u.size();
    let flow = |o: &Unit| {
        !matches!(o.class, Some(CAPTION | PICTURE | FOOTNOTE | PAGE_HEADER | PAGE_FOOTER | strata_ocr::layout::TABLE))
            && caption_kind(&o.text()).is_none()
            && (o.size() - size).abs() <= size.max(o.size()) * 0.08
    };
    let (mut above, mut below) = (f32::NEG_INFINITY, f32::INFINITY);
    for (j, o) in units.iter().enumerate() {
        if j == i || o.kind != UnitKind::Text || !same_column(o) || !flow(o) {
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

/// A note (footnote, affiliation, correspondence) rather than running text:
/// it opens like one (an affiliation number set as a superscript, a note
/// mark, "Corresponding author", "Received"...) in the front matter or the
/// lower half of the page; or it is set smaller than the running text at the
/// foot of a column, with no running text of that column below it.
fn is_note(u: &Unit, units: &[Unit], i: usize, text: &str) -> bool {
    let Some(first) = u.lines.first() else { return false };
    let size = u.size();
    let page_h = units.iter().map(|o| o.bbox.y1).fold(0.0f32, f32::max).max(1.0);
    let body = {
        // The running text of the page: the most common size of multi-line units.
        let mut hist: HashMap<i32, usize> = HashMap::new();
        for o in units.iter().filter(|o| o.kind == UnitKind::Text && o.lines.len() >= 3) {
            *hist.entry((o.size() * 2.0).round() as i32).or_default() += o.chars();
        }
        hist.into_iter().max_by_key(|e| e.1).map_or(size, |e| e.0 as f32 / 2.0)
    };
    if caption_kind(text).is_some() || u.lines.len() > 12 || size > body * 1.02 {
        return false;
    }
    let mark = first.chars.first().is_some_and(|c| c.c.is_ascii_digit() && c.size < line_size(first) * 0.8);
    if (mark || note_opening(text)) && u.lines.len() <= 8 {
        return true;
    }
    let low = u.bbox.y0 > page_h * 0.6;
    let smaller = size <= body * 0.94;
    let body_below = units.iter().enumerate().any(|(j, o)| {
        let overlap = u.bbox.x1.min(o.bbox.x1) - u.bbox.x0.max(o.bbox.x0);
        j != i && o.kind == UnitKind::Text && overlap > 0.3 * u.bbox.width().min(o.bbox.width()) && o.bbox.y0 >= u.bbox.y1 - 2.0 && (o.size() - body).abs() <= body * 0.06 && o.lines.len() >= 2
    });
    low && smaller && !body_below
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
    /// Type size of its last unit.
    size: f32,
}

/// Figures and captions of one page left apart by the reading order: each
/// figure caption shown as a paragraph goes to the nearest figure without a
/// caption, above, below or beside it. Small figures without a caption in the
/// top or bottom margin (logos, icons) are dropped. Emptied nodes become empty
/// paragraphs, removed at the end.
fn pair_figures(doc: &mut ReflowDoc, first: usize, node_bbox: &HashMap<usize, RectF>, width: f32, height: f32) {
    // Figures (`table` false) or tables (true) without a caption.
    let empty_float = |doc: &ReflowDoc, k: usize, table: bool| match &doc.nodes[k] {
        Node::Figure { image, caption } if caption.is_empty() && !table => Some(doc.images[*image].bbox),
        Node::Table { image, caption, .. } if caption.is_empty() && table => Some(doc.images[*image].bbox),
        _ => None,
    };
    let empty_fig = |doc: &ReflowDoc, k: usize| empty_float(doc, k, false);
    let captions: Vec<(usize, bool)> = (first..doc.nodes.len())
        .filter_map(|k| match &doc.nodes[k] {
            Node::Paragraph { spans } => caption_kind(&spans_text(spans)).map(|t| (k, t)),
            _ => None,
        })
        .collect();
    for (c, table) in captions {
        let Some(cb) = node_bbox.get(&c).copied() else { continue };
        let gap = |f: &RectF| {
            let h_overlap = f.x1.min(cb.x1) - f.x0.max(cb.x0);
            let v_overlap = f.y1.min(cb.y1) - f.y0.max(cb.y0);
            if h_overlap > 0.3 * f.width().min(cb.width()) {
                // Above (preferred) or below the caption.
                if f.y1 <= cb.y0 + 2.0 { Some(cb.y0 - f.y1) } else if f.y0 >= cb.y1 - 2.0 { Some((f.y0 - cb.y1) * 1.2) } else { None }
            } else if v_overlap > 0.3 * f.height().min(cb.height()) {
                // Beside it.
                Some(if f.x1 <= cb.x0 { cb.x0 - f.x1 } else { f.x0 - cb.x1 }.max(0.0) * 1.5)
            } else {
                None
            }
        };
        let best = (first..doc.nodes.len())
            .filter_map(|k| empty_float(doc, k, table).and_then(|f| gap(&f).map(|g| (k, g))))
            .filter(|&(_, g)| g < height * 0.2)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((f, _)) = best {
            let spans = match &mut doc.nodes[c] {
                Node::Paragraph { spans } => std::mem::take(spans),
                _ => continue,
            };
            if let Node::Figure { caption, .. } | Node::Table { caption, .. } = &mut doc.nodes[f] {
                *caption = spans;
            }
        }
    }
    let page_area = width * height;
    for k in first..doc.nodes.len() {
        if let Some(f) = empty_fig(doc, k)
            && f.width() * f.height() < page_area * 0.02
            && (f.y1 < height * 0.12 || f.y0 > height * 0.92)
        {
            doc.nodes[k] = Node::Paragraph { spans: Vec::new() };
        }
    }
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
    let results: Vec<parking_lot::Mutex<Vec<(RectF, usize, usize)>>> = pages.iter().map(|_| parking_lot::Mutex::new(Vec::new())).collect();
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
                    // Feature extraction scans every drawing: a detailed map (hundreds of
                    // thousands of paths) takes many seconds for a handful of text lines,
                    // which the heuristics handle alone.
                    let drawings = p.rich.blocks.iter().filter(|b| matches!(b, RichBlock::Vector { .. })).count();
                    if !p.ocr
                        && drawings <= 20_000
                        && let Some(dl) = &p.dl
                    {
                        match crate::layout::analyze_display_list(model, dl, p.rich.width, p.rich.height) {
                            Ok(a) => {
                                // Each line takes the class of its region (the majority of its lines).
                                *results[i].lock() = a
                                    .layout
                                    .groups
                                    .iter()
                                    .enumerate()
                                    .flat_map(|(gi, g)| g.nodes.iter().map(move |&k| (k, g.class, gi)))
                                    .map(|(k, c, gi)| {
                                        let b = a.nodes[k].bbox;
                                        (RectF { x0: b[0], y0: b[1], x1: b[2], y1: b[3] }, c, gi)
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
        let mut rich = RichPage::from_page(&page, &tp, b.width(), b.height());
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
        let area = b.width() * b.height();
        let over_scan = rich.blocks.iter().any(|b| matches!(b, RichBlock::Image { bbox } if bbox.width() * bbox.height() > area * 0.7))
            && rich.blocks.iter().map(|b| if let RichBlock::Text { lines, .. } = b { lines.iter().map(|l| l.chars.len()).sum() } else { 0 }).sum::<usize>() > 100;
        let scan = ocr || over_scan;
        pages.push(PageData { page: p as u32, rich, links, dl: page.to_display_list(true).ok(), ocr, scan, layout: Vec::new() });
        if p % 4 == 0 {
            progress(p + 1, total);
        }
    }
    let body = body_size(&pages);
    let repeated = repeated_margin_lines(&pages);
    let lex = hyphen::Lexicon::build(pages.iter().map(|p| &p.rich));
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
    // Type size (of the last unit) and page of each paragraph node.
    let mut para_size: HashMap<usize, (f32, u32)> = HashMap::new();
    // The last float caption: its node, type size, box and page (for a caption
    // continued in the next column).
    let mut last_caption: Option<(usize, f32, RectF, u32)> = None;
    // The last heading from the layout model: node, page, box and size, for
    // headings that the extractor split into one block per line.
    let mut last_heading: Option<(usize, u32, RectF, f32)> = None;
    // The body starts at the abstract, the introduction or the first numbered
    // section (or a long paragraph); before it, on the first page, lie the title,
    // authors and affiliations.
    let mut body_started = false;
    // Inside a reference list (its small type at the foot of columns is no note).
    let mut in_refs = false;
    for (pi, p) in pages.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let (mut units, manuscript) = page_units(p, body, &repeated, pages.len(), opts, vertical);
        // Double-spaced manuscripts leave a blank line's height between lines.
        let line_gap = if manuscript { body * 1.8 } else { body * 0.6 };
        let rects: Vec<RectF> = units.iter().map(|u| u.bbox).collect();
        let order = order::reading_order(&rects, vertical, p.scan);
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
        // A numbered heading that opens a block ("I. はじめに", then the paragraph):
        // a short first line with section numbering becomes a heading of its own.
        let mut forced_heading: Vec<bool> = Vec::with_capacity(ordered.len());
        if !vertical {
            let mut out: Vec<Unit> = Vec::with_capacity(ordered.len());
            for mut u in ordered.drain(..) {
                while u.kind == UnitKind::Text && u.lines.len() >= 2 {
                    let first = &u.lines[0];
                    let ft = first.text();
                    let ft = ft.trim();
                    let short = first.bbox.x1 < u.bbox.x1 - body * 3.0;
                    if !(short && ft.chars().count() <= 60 && heading_number(ft).is_some() && !ft.ends_with(['。', ',', '、', ';', '.'])) {
                        break;
                    }
                    let rest = u.lines.split_off(1);
                    let head = Unit { kind: UnitKind::Text, bbox: u.lines[0].bbox, lines: u.lines, class: u.class, group: u.group };
                    out.push(head);
                    forced_heading.push(true);
                    let bbox = rest.iter().skip(1).fold(rest[0].bbox, |a, l| a.union(&l.bbox));
                    u = Unit { kind: UnitKind::Text, bbox, lines: rest, class: u.class, group: u.group };
                }
                out.push(u);
                forced_heading.push(false);
            }
            ordered = out;
        } else {
            forced_heading.resize(ordered.len(), false);
        }
        // The title of the first page: the most title-like unit in its upper half.
        // What lies above it is the journal's masthead (banners, logos' text).
        let page_title: Option<usize> = (pi == 0 && !vertical)
            .then(|| {
                // The title is followed by its authors; a journal masthead is not.
                let authors_below = |k: usize| ordered[k + 1..].iter().filter(|o| o.kind == UnitKind::Text).take(2).any(|o| is_byline(&o.text()));
                let score = |(k, u): (usize, &Unit)| {
                    let bonus = if u.class == Some(TITLE) { 1.3 } else { 1.0 } * if authors_below(k) { 1.5 } else { 1.0 };
                    u.size() * (u.text().chars().count().min(150) as f32).sqrt() * bonus
                };
                let cands = ordered
                    .iter()
                    .enumerate()
                    .filter(|(_, u)| {
                        let t = u.text();
                        let n = t.chars().count();
                        let cjk = t.chars().filter(|&c| is_cjk(c)).count();
                        u.kind == UnitKind::Text
                            && ((10..=300).contains(&n) || (cjk >= 4 && n <= 300))
                            && u.lines.len() <= 6
                            && u.bbox.y0 < p.rich.height * 0.5
                            && (u.class == Some(TITLE) || u.size() >= body * 1.25)
                            && (cjk >= 4 || t.split_whitespace().filter(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 2).count() >= 2)
                            && caption_kind(&t).is_none()
                            && !is_strong_byline(&t)
                            && !is_boilerplate(&t)
                    })
                    .map(|(k, u)| (k, score((k, u)), u.bbox.y0, authors_below(k)))
                    .collect::<Vec<_>>();
                // Of titles of nearly equal weight (a paper titled in two languages,
                // each followed by its authors), the upper one.
                let best = cands.iter().max_by(|a, b| a.1.total_cmp(&b.1)).copied();
                best.and_then(|(_, max, _, authors)| cands.iter().filter(|c| c.1 >= max * 0.8 && c.3 == authors).min_by(|a, b| a.2.total_cmp(&b.2)).map(|c| c.0))
            })
            .flatten();
        let title_top = page_title.map(|t| ordered[t].bbox.y0);
        let title_size_pt = page_title.map_or(0.0, |t| ordered[t].size());
        if std::env::var("STRATA_DEBUG_UNITS").ok().and_then(|v| v.parse::<u32>().ok()) == Some(p.page + 1) {
            eprintln!("page title: {:?}", page_title.map(|t| ordered[t].text()));
        }
        // `STRATA_DEBUG_UNITS=<page>` (1-based): the ordered units of that page on stderr.
        if std::env::var("STRATA_DEBUG_UNITS").ok().and_then(|v| v.parse::<u32>().ok()) == Some(p.page + 1) {
            for u in &ordered {
                let t = u.text();
                let head: String = t.chars().take(60).collect();
                let tail: String = t.chars().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect();
                eprintln!(
                    "{:?} [{:.0},{:.0},{:.0},{:.0}] lines={} size={:.1} class={:?} group={:?} | {head} … {tail}",
                    u.kind, u.bbox.x0, u.bbox.y0, u.bbox.x1, u.bbox.y1, u.lines.len(), u.size(), u.class.map(|c| strata_ocr::layout::CLASSES[c]), u.group
                );
            }
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
        // shown as images until OCR supplies text; so are pages whose text is
        // mostly turned by 90 degrees (landscape tables and figures), which the
        // reading order cannot follow.
        let (mut turned, mut upright) = (0usize, 0usize);
        for b in &p.rich.blocks {
            if let RichBlock::Text { lines, .. } = b {
                for l in lines {
                    if !l.vertical && l.dir[1].abs() > 0.5 { turned += l.chars.len() } else { upright += l.chars.len() }
                }
            }
        }
        let rotated = (!vertical && turned > 200 && turned > upright * 2).then(|| "横向きに組まれたページ（表・図）".to_string());
        if let Some(reason) = needs_ocr(&p.rich).or(rotated) {
            let full = RectF { x0: 0.0, y0: 0.0, x1: p.rich.width, y1: p.rich.height };
            if let Some(img) = crop(full, opts.image_scale, &mut doc) {
                doc.nodes.push(Node::PageImage { image: img, reason });
            }
            doc.fill_anchors((p.page, 0.0));
            progress(2 * n + pi + 1, total);
            continue;
        }

        let mut last_anchor = (p.page, 0.0f32);
        let first_node = doc.nodes.len();
        // Box of each node made on this page (the unit it came from).
        let mut node_bbox: HashMap<usize, RectF> = HashMap::new();
        let mut pending: Option<(usize, RectF)> = None;
        let mut i = 0;
        while i < ordered.len() {
            if let Some((start, b)) = pending.take() {
                for k in start..doc.nodes.len() {
                    node_bbox.entry(k).or_insert(b);
                }
            }
            pending = Some((doc.nodes.len(), ordered[i].bbox));
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
                    caption = spans_of(next, &p.rich.fonts, &p.links, vertical, p.scan, &lex);
                    if let Some(img) = crop(u.bbox, opts.image_scale, &mut doc) {
                        doc.nodes.push(Node::Figure { image: img, caption });
                        last_caption = Some((doc.nodes.len() - 1, next.size(), next.bbox, p.page));
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
            // Glyphs of a badge or icon font, all undecodable.
            if chars::is_junk(&text) {
                i += 1;
                continue;
            }
            let in_front = pi == 0 && !body_started;
            // The masthead above the title of the first page, and publisher
            // boilerplate: notes, outside the running text.
            // (Text above the title set larger than it is part of the title, unless
            // it is a short banner: a title split in pieces, or a wrong pick.)
            let masthead = title_top.is_some_and(|top| {
                Some(i) != page_title && u.bbox.y1 <= top + 2.0 && (size < title_size_pt * 0.95 || (text.split_whitespace().count() <= 5 && !text.chars().any(is_cjk)))
            });
            if masthead || is_boilerplate(&text) {
                doc.nodes.push(Node::Footnote { spans: spans_of(u, &p.rich.fonts, &p.links, vertical, p.scan, &lex) });
                i += 1;
                continue;
            }
            // A table without a caption here (continued from the page before, or its
            // caption elsewhere): the model's table units that are not running text.
            if !vertical && u.class == Some(strata_ocr::layout::TABLE) && !prose_like(&text) && caption_kind(&text).is_none() {
                let mut j = i;
                let mut region = u.bbox;
                let mut rows = Vec::new();
                while let Some(t) = ordered.get(j) {
                    let table_unit = t.kind == UnitKind::Text && t.class == Some(strata_ocr::layout::TABLE) && !prose_like(&t.text()) && caption_kind(&t.text()).is_none();
                    let figure_inside = t.kind == UnitKind::Figure && overlap_frac(&t.bbox, &region.union(&t.bbox)) > 0.0 && t.bbox.y0 < region.y1 + body * 2.0;
                    if !(table_unit || (j > i && figure_inside)) {
                        break;
                    }
                    region = region.union(&t.bbox);
                    rows.extend(t.lines.iter().map(|l| l.text()));
                    j += 1;
                }
                if let Some(img) = crop(region, opts.image_scale, &mut doc) {
                    doc.nodes.push(Node::Table { image: img, caption: Vec::new(), rows });
                    i = j;
                    continue;
                }
            }
            // Running text that happens to start with "Table 2 summarizes..." is no
            // caption: with the layout model, a table caption must lead into a table.
            let running_text = matches!(u.class, Some(strata_ocr::layout::TEXT | strata_ocr::layout::LIST_ITEM));
            let leads_to_table = ordered[i + 1..].iter().take(3).any(|n| n.kind == UnitKind::Figure || n.class == Some(strata_ocr::layout::TABLE));
            match caption_kind(&text).filter(|&table| !(running_text && table && !leads_to_table)) {
                Some(true) => {
                    // Table: caption, then small-font units until body text resumes.
                    let caption = spans_of(u, &p.rich.fonts, &p.links, vertical, p.scan, &lex);
                    let mut j = i + 1;
                    let mut region: Option<RectF> = None;
                    let mut rows = Vec::new();
                    while let Some(t) = ordered.get(j) {
                        if t.kind == UnitKind::Figure {
                            region = Some(region.map_or(t.bbox, |r| r.union(&t.bbox)));
                            j += 1;
                            continue;
                        }
                        // The table ends where running text resumes (cells can be set at
                        // body size: the text decides, and the layout model's table class).
                        let s = t.size();
                        let tt = t.text();
                        let running = t.class != Some(strata_ocr::layout::TABLE) && s >= body * 0.97 && t.lines.len() >= 2 && prose_like(&tt);
                        if running || caption_kind(&tt).is_some() || (numbered_heading_depth(&tt).is_some() && s >= body * 0.97) {
                            break;
                        }
                        region = Some(region.map_or(t.bbox, |r| r.union(&t.bbox)));
                        rows.extend(t.lines.iter().map(|l| l.text()));
                        j += 1;
                    }
                    match region.and_then(|r| crop(r, opts.image_scale, &mut doc)) {
                        Some(img) => {
                            doc.nodes.push(Node::Table { image: img, caption, rows });
                            last_caption = Some((doc.nodes.len() - 1, size, u.bbox, p.page));
                        }
                        None => doc.nodes.push(Node::Paragraph { spans: caption }),
                    }
                    i = j;
                    continue;
                }
                Some(false) => {
                    // Figure caption whose figure came earlier: attach if possible.
                    let spans = spans_of(u, &p.rich.fonts, &p.links, vertical, p.scan, &lex);
                    if let Some(Node::Figure { caption, .. }) = doc.nodes.last_mut()
                        && caption.is_empty()
                    {
                        *caption = spans;
                        last_caption = Some((doc.nodes.len() - 1, size, u.bbox, p.page));
                    } else {
                        doc.nodes.push(Node::Paragraph { spans });
                    }
                    i += 1;
                    continue;
                }
                None if u.class == Some(CAPTION) => {
                    // The rest of a caption ("(b) PPL image of ...") belongs to the float before it.
                    let spans = spans_of(u, &p.rich.fonts, &p.links, vertical, p.scan, &lex);
                    let last = doc.nodes.len().saturating_sub(1);
                    match doc.nodes.last_mut() {
                        Some(Node::Figure { caption, .. } | Node::Table { caption, .. }) => {
                            if !caption.is_empty() && !join_hyphenated(caption, &text, &lex) {
                                caption.push(Span { text: " ".into(), style: Style::default(), link: None });
                            }
                            caption.extend(spans);
                            last_caption = Some((last, size, u.bbox, p.page));
                        }
                        _ => doc.nodes.push(Node::Paragraph { spans }),
                    }
                    i += 1;
                    continue;
                }
                None => {}
            }
            // A caption set across two columns under a wide float: its second part
            // starts beside the first, at the same height and in the same (non-body)
            // type size, while the caption so far stops mid-sentence.
            if !vertical
                && let Some((ix, csize, cbox, cpage)) = last_caption
                && cpage == p.page
                && (size - csize).abs() <= csize * 0.05
                && (size - body).abs() > body * 0.05
                && (u.bbox.y0 - cbox.y0).abs() < csize * 1.5
                && u.bbox.x0 >= cbox.x1 - 1.0
                && !matches!(u.class, Some(TITLE | SECTION_HEADER))
                && let Some(Node::Figure { caption, .. } | Node::Table { caption, .. }) = doc.nodes.get_mut(ix)
                && !caption.is_empty()
                && !ends_sentence(&spans_text(caption))
            {
                let spans = spans_of(u, &p.rich.fonts, &p.links, vertical, p.scan, &lex);
                if !join_hyphenated(caption, &text, &lex) {
                    caption.push(Span { text: " ".into(), style: Style::default(), link: None });
                }
                caption.extend(spans);
                last_caption = Some((ix, csize, cbox.union(&u.bbox), cpage));
                i += 1;
                continue;
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
            // Headings in capitals (older papers), numbered first lines split off above,
            // and the title found for the page.
            let caps = !vertical
                && u.lines.len() <= 2
                && size >= body * 0.7
                && caps_heading(&text)
                && !matches!(u.class, Some(strata_ocr::layout::TABLE | PICTURE | CAPTION | FOOTNOTE | PAGE_HEADER | PAGE_FOOTER));
            let mut level = level;
            if Some(i) == page_title {
                level = Some(1);
                doc.title = text.trim().to_string();
                title_size = f32::MAX;
            } else if forced_heading[i] {
                level = Some(heading_number(&text).map_or(3, |d| (d + 1).min(6)));
            } else if level.is_none() && caps {
                level = Some(heading_number(&text).map_or(2, |d| (d + 1).min(6)));
            }
            // Author lines in the front matter are no headings.
            if in_front && level.is_some() && Some(i) != page_title && !is_front_heading(&text) && is_byline(&text) {
                doc.nodes.push(Node::Paragraph { spans: spans_of(u, &p.rich.fonts, &p.links, vertical, p.scan, &lex) });
                i += 1;
                continue;
            }
            if level.is_some() && (is_front_heading(&text) || heading_number(&text).is_some()) {
                body_started = true;
            }
            if level.is_some() {
                in_refs = is_references_heading(&text);
            }
            let mut spans = spans_of(u, &p.rich.fonts, &p.links, vertical, p.scan, &lex);
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
                    && (u.class.is_some() || caps || level == 1)
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
                    let spans = spans.into_iter().map(|mut s| {
                        s.text = collapse_letterspacing(&s.text);
                        s
                    });
                    doc.nodes.push(Node::Heading { level: level as u8, spans: spans.collect() });
                    if u.class.is_some() || caps || Some(i) == page_title {
                        last_heading = Some((doc.nodes.len() - 1, p.page, u.bbox, size));
                    }
                }
            } else if u.class == Some(FOOTNOTE) || (!vertical && !in_refs && is_note(u, &ordered, i, &text)) {
                doc.nodes.push(Node::Footnote { spans });
            } else if is_list_marker(&text) {
                doc.nodes.push(Node::ListItem { spans });
            } else {
                // Continuation across a column or page break; figures, tables,
                // footnotes and stray captions that interrupt a paragraph are floats
                // and are skipped.
                // Paragraphs in another type size (affiliations, editorial notes,
                // captions the model took for text) interrupt running text like
                // floats: a few of them, on this page or the one before, are skipped.
                let mut skipped = 0;
                let prev = doc
                    .nodes
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(ix, n)| match n {
                        Node::PageStart { .. } | Node::Figure { .. } | Node::Table { .. } | Node::Footnote { .. } => false,
                        Node::Paragraph { spans } if caption_kind(&spans_text(spans)).is_some() => false,
                        Node::Paragraph { .. }
                            if !vertical
                                && !p.scan
                                && skipped < 3
                                && para_size.get(ix).is_some_and(|&(s, pg)| pg + 1 >= p.page && (s - size).abs() > s.max(size) * 0.07) =>
                        {
                            skipped += 1;
                            false
                        }
                        _ => true,
                    })
                    .map(|(ix, _)| ix);
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
                            // Running text by size, or by the layout model (references are set smaller).
                            let bodyish = ((size - body).abs() <= body * 0.1 && (u.lines.len() >= 2 || text.chars().count() >= 40))
                                || matches!(u.class, Some(strata_ocr::layout::TEXT | strata_ocr::layout::LIST_ITEM));
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
                            // Running text continues in its own type size: a caption or a note
                            // set smaller (or a display line set larger) is no continuation.
                            // OCR sizes are estimates.
                            // A short fragment (the end of a sentence in a math font) is exempt.
                            let fragment = u.lines.len() == 1 && text.chars().count() <= 30;
                            let same_size = p.scan || fragment || last_para.as_ref().is_none_or(|l| Some(l.node) != prev || (l.size - size).abs() <= l.size.max(size) * 0.07);
                            // A CJK character, digit or bracket continues only where the
                            // layout agrees: the next lines of the column, or a column top
                            // after a column cut at its edge.
                            let weak = continues_weakly(&prev_text, &text) && (adjacent || (cut && at_top));
                            same_size && (continues_strongly(&prev_text, &text) || weak || (!ends_sentence(&prev_text) && !first_indented && bodyish && ((cut && at_top) || adjacent)))
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
                        if !cjk && !vertical && !join_hyphenated(prev, &text, &lex) {
                            prev.push(Span { text: " ".into(), style: Style::default(), link: None });
                        }
                        prev.extend(spans);
                    }
                    // A short fragment (math, a last word) keeps the paragraph's size.
                    let size = match &last_para {
                        Some(l) if l.node == idx && u.lines.len() == 1 && text.chars().count() <= 30 => l.size,
                        _ => size,
                    };
                    last_para = Some(LastPara { node: idx, cut_at_edge, last_full, page: p.page, bbox: u.bbox, size });
                    para_size.insert(idx, (size, p.page));
                } else {
                    body_started |= text.chars().count() >= 400;
                    doc.nodes.push(Node::Paragraph { spans });
                    last_para = Some(LastPara { node: doc.nodes.len() - 1, cut_at_edge, last_full, page: p.page, bbox: u.bbox, size });
                    para_size.insert(doc.nodes.len() - 1, (size, p.page));
                }
            }
            i += 1;
        }
        if let Some((start, b)) = pending.take() {
            for k in start..doc.nodes.len() {
                node_bbox.entry(k).or_insert(b);
            }
        }
        doc.fill_anchors(last_anchor);
        pair_figures(&mut doc, first_node, &node_bbox, p.rich.width, p.rich.height);
        progress(2 * n + pi + 1, total);
    }
    let last = doc.anchors.last().copied().unwrap_or((0, 0.0));
    doc.fill_anchors(last);
    // Nodes emptied by `pair_figures`.
    let keep: Vec<bool> = doc.nodes.iter().map(|n| !matches!(n, Node::Paragraph { spans } if spans.is_empty())).collect();
    let mut k = 0;
    doc.nodes.retain(|_| {
        k += 1;
        keep[k - 1]
    });
    let mut k = 0;
    doc.anchors.retain(|_| {
        k += 1;
        keep[k - 1]
    });
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
        assert_eq!(caption_kind("Figure 4 shows spectrograms"), None);
        assert_eq!(caption_kind("Table 2 summarizes the data"), None);
        assert_eq!(caption_kind("Figure 2. volcanoes for five sets"), Some(false));
        assert_eq!(caption_kind("Fig. 1a Map of the flow"), Some(false));
        assert_eq!(caption_kind("Table 1 (continued)"), Some(true));
        assert_eq!(caption_kind("Fig.8(a)に求められたバックスリップ量"), None);
        assert_eq!(caption_kind("Fig. 8. 求められたバックスリップ量"), Some(false));
    }

    #[test]
    fn reference_starts() {
        assert!(author_start("Almendros, J., Wilcock, W., Soule, D."));
        assert!(author_start("Av´e Lallemant, H.G., Oldow, J.S., 2000."));
        assert!(author_start("Arai, K., Matsuda, H."));
        assert!(!author_start("Bru˜na, J.L."[..0].trim()));
        assert!(!author_start("Geophysical investigation of rifting and volcanism"));
        assert!(!author_start("In contrast, the northern part"));
    }

    #[test]
    fn bylines() {
        assert!(is_byline("Andrew F. Bell1*, Stephen Hernandez2, John McCloskey1, Mario Ruiz2, Peter C. LaFemina3"));
        assert!(is_byline("R. J. Brown · L. Civetta · I. Arienzo · M. D’Antonio"));
        assert!(is_byline("PETER CATTERMOLE"));
        assert!(is_byline("Eysteinn Tryggvason"));
        assert!(is_byline("Bowen Zhu 1,2,3and Zhigang Zeng 1,2,4,*"));
        assert!(is_byline("勝間田明男"));
        assert!(!is_byline("Monitoring and Modeling the Rapid Evolution of Earth’s Newest Volcanic Island: Hunga Tonga Hunga Ha’apai (Tonga) Using High Spatial Resolution Satellite Observations"));
        assert!(!is_strong_byline("Hydrothermal Calderas and Their Deposits"));
        assert!(is_strong_byline("J. B. Garvin, D. A. Slayback, V. Ferrini"));
        assert!(!is_byline("Key Points"));
        assert!(!is_byline("Geological Setting of the Izu Arc"));
    }

    #[test]
    fn letterspacing() {
        assert_eq!(collapse_letterspacing("G E O P H YS I C S Delayed submarine"), "GEOPHYSICS Delayed submarine");
        assert_eq!(collapse_letterspacing("Units A B and C"), "Units A B and C");
        assert_eq!(collapse_letterspacing("R E S U L T S"), "RESULTS");
    }

    #[test]
    fn continuation() {
        assert!(continues("the fracture network was", "formed in the magmatic state"));
        assert!(!continues("It ended.", "then"));
        assert!(continues("これは段落の途中で", "続く文章です。"));
        assert!(!continues("終わり。", "次"));
    }
}
