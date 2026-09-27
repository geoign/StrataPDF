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
//! A layout model can later supply [`RegionHint`]s that override the heuristics.

mod order;
pub mod output;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, unbounded};

use crate::doc::{Document, Engine, open_engine};
use crate::geom::RectF;
use crate::render::render_region_png;
use crate::rich::{RichBlock, RichLine, RichPage, reflow_flags};
use crate::text::FontInfo;

#[derive(Clone, Debug)]
pub struct ReflowOptions {
    pub strip_headers: bool,
    /// Pixels per point for figure/table crops.
    pub image_scale: f32,
    /// Pixels per point for formula crops (higher: they feed formula recognition).
    pub formula_scale: f32,
    /// Drop furigana (ruby) lines in Japanese text.
    pub drop_ruby: bool,
}

impl Default for ReflowOptions {
    fn default() -> Self {
        ReflowOptions { strip_headers: true, image_scale: 2.0, formula_scale: 3.0, drop_ruby: true }
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
    Formula { image: usize, text: String, latex: Option<String> },
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
        std::thread::Builder::new()
            .name("strata-reflow".into())
            .spawn(move || {
                let ev = match open_engine(&path, password.as_deref()) {
                    Ok((eng, _)) => {
                        let progress = |done, total| {
                            let _ = tx.send(ReflowEvent::Progress { done, total });
                            waker();
                        };
                        match build(&eng, &opts, &progress, &cancel, &ocr) {
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
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x3000..=0x303F)
}

fn is_math_font(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const KEYS: [&str; 18] = ["math", "cmmi", "cmsy", "cmex", "msbm", "msam", "symbol", "stix", "mt extra", "mtextra", "euclid", "mathematicalpi", "txsy", "txmi", "rtxmi", "mtsy", "mtmi", "cambria math"];
    KEYS.iter().any(|k| n.contains(k))
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
                    if l.bbox.y1 < h * 0.09 || l.bbox.y0 > h * 0.91 {
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

fn page_units(p: &PageData, body: f32, repeated: &HashMap<String, usize>, n_pages: usize, opts: &ReflowOptions, vertical: bool) -> Vec<Unit> {
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
                let kept: Vec<RichLine> = lines
                    .iter()
                    .filter(|l| {
                        if opts.strip_headers && (l.bbox.y1 < h * 0.09 || l.bbox.y0 > h * 0.91) {
                            let t = l.text();
                            let k = digits_key(&t);
                            if repeated.get(&k).copied().unwrap_or(0) >= threshold && n_pages > 1 {
                                return false;
                            }
                            // Lone page numbers in the margins.
                            if t.trim().chars().all(|c| c.is_ascii_digit() || c == '-' || c == '—' || c.is_whitespace()) {
                                return false;
                            }
                        }
                        if opts.drop_ruby && line_size(l) < body * 0.6 && l.chars.iter().all(|c| matches!(c.c as u32, 0x3040..=0x30FF) || c.c.is_whitespace()) {
                            return false;
                        }
                        let _ = vertical;
                        true
                    })
                    .cloned()
                    .collect();
                if !kept.is_empty() {
                    let bbox = kept.iter().skip(1).fold(kept[0].bbox, |a, l| a.union(&l.bbox));
                    units.push(Unit { kind: UnitKind::Text, bbox, lines: kept });
                }
            }
            RichBlock::Image { bbox } => {
                // A page-sized image under a text layer is the scan of an OCRed page.
                let background = bbox.width() * bbox.height() > page_area * 0.7 && text_chars > 100;
                if !background && bbox.width() * bbox.height() > page_area * 0.005 {
                    units.push(Unit { kind: UnitKind::Figure, bbox: *bbox, lines: Vec::new() });
                }
            }
            _ => {}
        }
    }
    for r in vector_figures(&p.rich.blocks, page_area) {
        units.push(Unit { kind: UnitKind::Figure, bbox: r, lines: Vec::new() });
    }
    // Merge overlapping figures; absorb labels inside figures.
    let mut figs: Vec<RectF> = Vec::new();
    for u in units.iter().filter(|u| u.kind == UnitKind::Figure) {
        match figs.iter_mut().find(|f| overlap_frac(&u.bbox, f) > 0.3 || overlap_frac(f, &u.bbox) > 0.3) {
            Some(f) => *f = f.union(&u.bbox),
            None => figs.push(u.bbox),
        }
    }
    let mut out: Vec<Unit> = Vec::new();
    for u in units.into_iter().filter(|u| u.kind == UnitKind::Text) {
        let inside = figs.iter().any(|f| overlap_frac(&u.bbox, f) > 0.8);
        let bodyish = u.lines.len() >= 3 && (u.size() - body).abs() < body * 0.1;
        if inside && !bodyish {
            continue;
        }
        out.push(u);
    }
    out.extend(figs.into_iter().map(|bbox| Unit { kind: UnitKind::Figure, bbox, lines: Vec::new() }));
    out
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
    (saw_digit && rest.starts_with(char::is_whitespace) && rest.trim().chars().next().is_some_and(|c| c.is_alphabetic())).then_some(depth)
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
            let name = f.map(|f| f.name.to_ascii_lowercase()).unwrap_or_default();
            let small = c.size < med * 0.8 && !vertical;
            let cy = (c.bbox.y0 + c.bbox.y1) * 0.5;
            let style = Style {
                bold: c.bold || f.is_some_and(|f| f.bold) || name.contains("bold"),
                italic: f.is_some_and(|f| f.italic) || name.contains("italic") || name.contains("oblique"),
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
    first.is_lowercase() || is_cjk(first) || first.is_ascii_digit() || matches!(first, ',' | ';' | '(' | '、')
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

fn build(eng: &Engine, opts: &ReflowOptions, progress: &dyn Fn(usize, usize), cancel: &AtomicBool, ocr: &crate::ocr::OcrStore) -> Result<Option<ReflowDoc>, String> {
    let n = eng.page_count().map_err(|e| e.to_string())?.max(0) as usize;
    let total = n * 2;
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
        if let Some(o) = ocr.get(p as u32)
            && needs_ocr(&rich).is_some()
        {
            rich = o.to_rich(b.width(), b.height());
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
        pages.push(PageData { page: p as u32, rich, links });
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

    // Pass 2: per page layout and classification.
    let mut doc = ReflowDoc { vertical, ..Default::default() };
    let mut title_size = 0.0f32;
    // Vertical text: did the last paragraph column run to the bottom of the text area?
    let mut last_col_full = false;
    for (pi, p) in pages.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let mut units = page_units(p, body, &repeated, pages.len(), opts, vertical);
        let rects: Vec<RectF> = units.iter().map(|u| u.bbox).collect();
        let order = order::reading_order(&rects, vertical);
        let mut ordered: Vec<Unit> = Vec::with_capacity(units.len());
        let mut slots: Vec<Option<Unit>> = units.drain(..).map(Some).collect();
        for i in order {
            if let Some(u) = slots[i].take() {
                ordered.push(u);
            }
        }
        doc.nodes.push(Node::PageStart { page: p.page });
        let dl = eng.load_page(p.page as i32).and_then(|pg| pg.to_display_list(true)).ok();
        let crop = |bbox: RectF, scale: f32, doc: &mut ReflowDoc| -> Option<usize> {
            let dl = dl.as_ref()?;
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
            progress(n + pi + 1, total);
            continue;
        }

        let mut i = 0;
        while i < ordered.len() {
            let u = &ordered[i];
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
            match caption_kind(&text) {
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
            let math = u.frac(|c| (is_math_char(c.c) || p.rich.fonts.get(c.font as usize).is_some_and(|f| is_math_font(&f.name))) && c.c != '•');
            let short = text.chars().count() < 160 && u.lines.len() <= 3;
            let bold = u.frac(|c| c.bold || p.rich.fonts.get(c.font as usize).is_some_and(|f| f.bold || f.name.to_ascii_lowercase().contains("bold")));
            let italic = u.frac(|c| p.rich.fonts.get(c.font as usize).is_some_and(|f| f.italic || f.name.to_ascii_lowercase().contains("italic")));
            let numbered = numbered_heading_depth(&text);
            if !vertical && math >= 0.35 && u.lines.len() <= 6 && u.chars() >= 3 {
                if let Some(img) = crop(u.bbox, opts.formula_scale, &mut doc) {
                    doc.nodes.push(Node::Formula { image: img, text: text.clone(), latex: None });
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
            let level = if short && size >= body * 1.4 {
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
                let spans = spans.into_iter().map(|mut s| {
                    s.style.bold = false;
                    s.style.italic = false;
                    s
                });
                doc.nodes.push(Node::Heading { level: level as u8, spans: spans.collect() });
            } else if is_list_marker(&text) {
                doc.nodes.push(Node::ListItem { spans });
            } else if !vertical && size <= body * 0.92 && u.bbox.y0 > p.rich.height * 0.6 && u.lines.len() <= 8 {
                doc.nodes.push(Node::Footnote { spans });
            } else {
                // Continuation across a column or page break; figures, tables and
                // footnotes that interrupt a paragraph are floats and are skipped.
                let prev = doc.nodes.iter().rposition(|n| !matches!(n, Node::PageStart { .. } | Node::Figure { .. } | Node::Table { .. } | Node::Footnote { .. }));
                let merge = match prev.map(|ix| &doc.nodes[ix]) {
                    Some(Node::Paragraph { spans: prev_spans }) => {
                        if vertical {
                            let (top, _) = vertical_text_area(&ordered, body);
                            let indented = u.bbox.y0 > top + body * 0.5;
                            last_col_full && !indented
                        } else {
                            continues(&spans_text(prev_spans), &text)
                        }
                    }
                    _ => false,
                };
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
                } else {
                    doc.nodes.push(Node::Paragraph { spans });
                }
            }
            i += 1;
        }
        progress(n + pi + 1, total);
    }
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
