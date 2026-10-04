//! OCR glue: run an engine over pages, keep the results per document (memory
//! plus an append-only disk cache), and expose them as text for selection,
//! search and reflow wherever the PDF's own text layer is unusable.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::{Receiver, unbounded};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
pub use strata_ocr::{Device, OcrEngine, models};

use crate::doc::{Document, Engine, open_engine};
use crate::geom::{QuadF, RectF};
use crate::render::render_page_rgb;
use crate::rich::{RichBlock, RichChar, RichLine, RichPage, reflow_flags};
use crate::text::{Block, PageText, TextChar, TextLine};

/// Resolution pages are rendered at for OCR (the NDLOCR-Lite default).
pub const OCR_DPI: f32 = 150.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrTextLine {
    /// Page space, points.
    pub bbox: RectF,
    pub text: String,
    pub vertical: bool,
    pub block: Option<u32>,
    /// The engine's class of the line ("Main", "Title", "Caption", "Note",
    /// "InlineNote", "Advert"); absent in older caches.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrRegionInfo {
    pub bbox: RectF,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageOcr {
    pub page: u32,
    /// OCR was requested for this page although it has a text layer: prefer
    /// the OCR text over the existing one.
    #[serde(default)]
    pub forced: bool,
    pub engine: String,
    pub vertical: bool,
    pub lines: Vec<OcrTextLine>,
    pub regions: Vec<OcrRegionInfo>,
}

/// The characters of an OCR line with the comma and full stop of Japanese text as
/// printed: the engine reads "，" and "．" as "," and "." (not after a Latin
/// letter or a digit: "Fig.1", "3.5", "1.はじめに").
fn japanese_stops(t: &str) -> Vec<char> {
    let c: Vec<char> = t.chars().collect();
    let cjk = |x: char| matches!(x as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF01..=0xFF60);
    (0..c.len())
        .map(|i| {
            let before = i.checked_sub(1).map(|k| c[k]);
            let after = c.get(i + 1).copied();
            let japanese = before.is_some_and(cjk) || (after.is_some_and(cjk) && !before.is_some_and(|b| b.is_ascii_alphanumeric()));
            match c[i] {
                ',' if japanese => '，',
                '.' if japanese => '．',
                x => x,
            }
        })
        .collect()
}

/// Evenly spaced character boxes along an OCR line (the engines report lines only).
fn char_boxes(l: &OcrTextLine) -> Vec<(char, RectF)> {
    let chars: Vec<char> = l.text.chars().collect();
    let n = chars.len().max(1) as f32;
    let b = l.bbox;
    chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let (i0, i1) = (i as f32 / n, (i + 1) as f32 / n);
            let r = if l.vertical {
                RectF { x0: b.x0, x1: b.x1, y0: b.y0 + b.height() * i0, y1: b.y0 + b.height() * i1 }
            } else {
                RectF { x0: b.x0 + b.width() * i0, x1: b.x0 + b.width() * i1, y0: b.y0, y1: b.y1 }
            };
            (c, r)
        })
        .collect()
}

fn quad(r: RectF) -> QuadF {
    QuadF { ul: [r.x0, r.y0], ur: [r.x1, r.y0], ll: [r.x0, r.y1], lr: [r.x1, r.y1] }
}

impl PageOcr {
    fn blocks(&self) -> Vec<Vec<&OcrTextLine>> {
        let mut out: Vec<Vec<&OcrTextLine>> = Vec::new();
        let mut last_block: Option<Option<u32>> = None;
        for l in &self.lines {
            match (last_block, l.block) {
                (Some(Some(a)), Some(b)) if a == b => out.last_mut().unwrap().push(l),
                _ => out.push(vec![l]),
            }
            last_block = Some(l.block);
        }
        out
    }

    /// Text for selection and copy in the viewer.
    pub fn to_page_text(&self) -> PageText {
        let blocks = self
            .paragraphs(false)
            .into_iter()
            .map(|ls| {
                let lines: Vec<TextLine> = ls
                    .iter()
                    .map(|l| {
                        let size = if l.vertical { l.bbox.width() } else { l.bbox.height() } * 0.85;
                        TextLine {
                            bbox: l.bbox,
                            vertical: l.vertical,
                            chars: char_boxes(l)
                                .into_iter()
                                .map(|(c, r)| TextChar { c, quad: quad(r), origin: [r.x0, r.y1], size, font: u16::MAX, argb: 0xff000000 })
                                .collect(),
                        }
                    })
                    .collect();
                let bbox = lines.iter().skip(1).fold(lines[0].bbox, |a, l| a.union(&l.bbox));
                Block::Text { bbox, lines }
            })
            .collect();
        PageText { blocks, fonts: Vec::new() }
    }

    /// Split OCR text blocks the way the reflow heuristics expect: vertical
    /// text one column per block (paragraphs are found from indents later),
    /// horizontal text at indented lines and after short lines.
    fn paragraph_blocks(&self) -> Vec<Vec<&OcrTextLine>> {
        self.paragraphs(true)
    }

    /// Paragraphs from indents and short lines. With `split_columns`, vertical
    /// text is returned one column per group (the reflow heuristics rejoin
    /// columns themselves); otherwise columns are grouped into paragraphs.
    fn paragraphs(&self, split_columns: bool) -> Vec<Vec<&OcrTextLine>> {
        let mut out = Vec::new();
        for ls in self.blocks() {
            let vertical = ls.iter().all(|l| l.vertical);
            if vertical && split_columns {
                out.extend(ls.into_iter().map(|l| vec![l]));
                continue;
            }
            let bb = ls.iter().skip(1).fold(ls[0].bbox, |a, l| a.union(&l.bbox));
            let mut cur: Vec<&OcrTextLine> = Vec::new();
            for l in ls {
                // (A wide gap parts paragraphs too: the engine's block can hold a
                // journal's masthead and an abstract far below it.)
                let (indented, prev_short, gap) = if vertical {
                    let size = l.bbox.width().max(1.0);
                    (l.bbox.y0 - bb.y0 > size * 0.6, cur.last().is_some_and(|p| p.bbox.y1 < bb.y1 - size * 1.5), cur.last().is_some_and(|p| p.bbox.x0 - l.bbox.x1 > size * 1.5))
                } else {
                    let size = l.bbox.height().max(1.0);
                    (l.bbox.x0 - bb.x0 > size * 0.7, cur.last().is_some_and(|p| p.bbox.x1 < bb.x1 - size * 1.5), cur.last().is_some_and(|p| l.bbox.y0 - p.bbox.y1 > size * 1.5))
                };
                if !cur.is_empty() && (indented || prev_short || gap) {
                    out.push(std::mem::take(&mut cur));
                }
                cur.push(l);
            }
            if !cur.is_empty() {
                out.push(cur);
            }
        }
        out
    }

    /// Structured text for reflow, with figure/table regions as image blocks.
    /// The engine's layout of the page in the classes of the layout model: (line
    /// box, class, region) per line, as reflow takes them from that model for pages
    /// with a text layer. Titles become section headers, captions and notes stay
    /// what they are, and lines inside a running head or page number region are
    /// page headers. Empty for OCR results cached without line classes.
    pub fn layout_classes(&self) -> Vec<(RectF, usize, usize)> {
        use strata_ocr::layout::{CAPTION, FOOTNOTE, PAGE_FOOTER, PAGE_HEADER, SECTION_HEADER, TEXT};
        if self.lines.iter().all(|l| l.kind.is_none()) {
            return Vec::new();
        }
        let region_of = |b: &RectF| {
            let (x, y) = ((b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0);
            self.regions.iter().find(|r| matches!(r.kind.as_str(), "Header" | "Folio") && r.bbox.x0 <= x && x <= r.bbox.x1 && r.bbox.y0 <= y && y <= r.bbox.y1).map(|r| r.kind.as_str())
        };
        self.lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let class = match (region_of(&l.bbox), l.kind.as_deref()) {
                    (Some("Header"), _) => PAGE_HEADER,
                    (Some(_), _) => PAGE_FOOTER,
                    (None, Some("Title")) => SECTION_HEADER,
                    (None, Some("Caption")) => CAPTION,
                    (None, Some("Note")) => FOOTNOTE,
                    _ => TEXT,
                };
                // (Lines of one text block are one region; others each their own.)
                (l.bbox, class, l.block.map_or(10_000 + i, |b| b as usize))
            })
            .collect()
    }

    /// Type size of each line, from the thickness of its box. A box is as thick as
    /// the ink of its line: a line without ascenders or descenders, or of kana, or a
    /// short one comes out smaller than its neighbours of the same size. Lines of one
    /// text block share a size (the median), unless one stands well apart; headings
    /// keep their own. Latin boxes run taller than Japanese ones of the same size.
    fn line_sizes(&self) -> HashMap<*const OcrTextLine, f32> {
        let raw = |l: &OcrTextLine| {
            let latin = {
                let (mut ascii, mut all) = (0usize, 0usize);
                for c in l.text.chars().filter(|c| !c.is_whitespace()) {
                    all += 1;
                    ascii += c.is_ascii() as usize;
                }
                !l.vertical && all >= 4 && ascii * 10 >= all * 7
            };
            (if l.vertical { l.bbox.width() } else { l.bbox.height() }) * if latin { 0.78 } else { 0.85 }
        };
        let mut out: HashMap<*const OcrTextLine, f32> = self.lines.iter().map(|l| (l as *const _, raw(l))).collect();
        // (Headings apart from the running text of their block: two lines of one
        // heading share a size.)
        let mut by_block: HashMap<(u32, bool), Vec<&OcrTextLine>> = HashMap::new();
        for l in &self.lines {
            if let Some(b) = l.block {
                by_block.entry((b, l.kind.as_deref() == Some("Title"))).or_default().push(l);
            }
        }
        for ls in by_block.values().filter(|ls| ls.len() >= 2) {
            let mut v: Vec<f32> = ls.iter().map(|l| raw(l)).collect();
            v.sort_by(f32::total_cmp);
            let med = v[v.len() / 2];
            for l in ls {
                let r = raw(l);
                if r >= med * 0.7 && r <= med * 1.35 {
                    out.insert(*l as *const _, med);
                }
            }
        }
        out
    }

    pub fn to_rich(&self, width: f32, height: f32) -> RichPage {
        let sizes = self.line_sizes();
        let mut blocks: Vec<RichBlock> = self
            .paragraph_blocks()
            .into_iter()
            .map(|ls| {
                let n = ls.len();
                let lines: Vec<RichLine> = ls
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        let size = sizes.get(&(*l as *const OcrTextLine)).copied().unwrap_or(l.bbox.height() * 0.85);
                        // Hyphenated line break in Latin text: join like MuPDF's dehyphenation.
                        let next_lower = ls.get(i + 1).and_then(|n| n.text.chars().next()).is_some_and(|c| c.is_lowercase());
                        let joined = i + 1 < n && l.text.ends_with('-') && next_lower;
                        RichLine {
                            bbox: l.bbox,
                            vertical: l.vertical,
                            dir: if l.vertical { [0.0, 1.0] } else { [1.0, 0.0] },
                            joined,
                            chars: char_boxes(l).into_iter().zip(japanese_stops(&l.text)).map(|((_, bbox), c)| RichChar { c, bbox, size, font: u16::MAX, bold: false, argb: 0xff000000 }).collect(),
                        }
                    })
                    .collect();
                let bbox = lines.iter().skip(1).fold(lines[0].bbox, |a, l| a.union(&l.bbox));
                RichBlock::Text { bbox, lines }
            })
            .collect();
        for r in &self.regions {
            if matches!(r.kind.as_str(), "Figure" | "Chart" | "Table") {
                blocks.push(RichBlock::Image { bbox: r.bbox });
            }
        }
        RichPage { width, height, blocks, fonts: Vec::new() }
    }

    /// Case-insensitive search in OCR text; one quad per matched line segment.
    pub fn search(&self, needle: &str) -> Vec<QuadF> {
        let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).filter(|c| !c.is_whitespace()).collect();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for l in &self.lines {
            let boxes = char_boxes(l);
            let hay: Vec<(char, RectF)> = boxes.iter().filter(|(c, _)| !c.is_whitespace()).map(|(c, r)| (c.to_lowercase().next().unwrap_or(*c), *r)).collect();
            if hay.len() < needle.len() {
                continue;
            }
            for i in 0..=hay.len() - needle.len() {
                if hay[i..i + needle.len()].iter().zip(&needle).all(|(a, b)| a.0 == *b) {
                    let r = hay[i..i + needle.len()].iter().skip(1).fold(hay[i].1, |a, (_, r)| a.union(r));
                    out.push(quad(r));
                }
            }
        }
        out
    }
}

/// Per-document OCR results with a JSON-lines disk cache keyed by file identity.
pub struct OcrStore {
    pages: RwLock<HashMap<u32, Arc<PageOcr>>>,
    cache_file: Option<PathBuf>,
    revision: AtomicU64,
}

fn cache_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "StrataPDF").map(|d| d.data_local_dir().join("ocr"))
}

/// Per-file cache key: path, size and modification time.
pub fn cache_key(path: &Path) -> Option<String> {
    use std::hash::{Hash, Hasher};
    // The same file opened by a relative path must hit the same entry.
    // (`canonicalize` returns a verbatim `\\?\` path on Windows; drop the prefix.)
    let canon = std::fs::canonicalize(path).map(|p| {
        let s = p.to_string_lossy().into_owned();
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
    });
    let path = &canon.unwrap_or_else(|_| path.to_path_buf());
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.to_string_lossy().to_lowercase().hash(&mut h);
    meta.len().hash(&mut h);
    mtime.hash(&mut h);
    Some(format!("{:016x}", h.finish()))
}

impl OcrStore {
    pub fn open(path: &Path) -> OcrStore {
        let cache_file = cache_dir().zip(cache_key(path)).map(|(d, k)| d.join(format!("{k}.jsonl")));
        let mut pages = HashMap::new();
        if let Some(f) = &cache_file
            && let Ok(s) = std::fs::read_to_string(f)
        {
            for line in s.lines() {
                if let Ok(p) = serde_json::from_str::<PageOcr>(line) {
                    pages.insert(p.page, Arc::new(p));
                }
            }
        }
        OcrStore { pages: RwLock::new(pages), cache_file, revision: AtomicU64::new(0) }
    }

    pub fn get(&self, page: u32) -> Option<Arc<PageOcr>> {
        self.pages.read().get(&page).cloned()
    }

    pub fn len(&self) -> usize {
        self.pages.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bumped whenever a page is added.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    fn insert(&self, p: PageOcr) {
        if let Some(f) = &self.cache_file
            && let Some(dir) = f.parent()
            && std::fs::create_dir_all(dir).is_ok()
            && let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(f)
            && let Ok(line) = serde_json::to_string(&p)
        {
            let _ = writeln!(file, "{line}");
        }
        self.pages.write().insert(p.page, Arc::new(p));
        self.revision.fetch_add(1, Ordering::AcqRel);
    }
}

/// OCR one page of an open engine.
pub(crate) fn ocr_page(eng: &Engine, page: u32, ocr: &dyn OcrEngine, dpi: f32) -> Result<PageOcr, String> {
    let dl = eng.load_page(page as i32).and_then(|p| p.to_display_list(false)).map_err(|e| e.to_string())?;
    let b = dl.bounds();
    let scale = dpi / 72.0;
    let (w, h, rgb) = render_page_rgb(&dl, scale, false)?;
    let img = image::RgbImage::from_raw(w, h, rgb).ok_or("bad image buffer")?;
    // The page at twice the resolution, for lines of Latin text.
    let detail = render_page_rgb(&dl, scale * 2.0, false).ok().and_then(|(w, h, rgb)| image::RgbImage::from_raw(w, h, rgb));
    let res = ocr.recognize_detailed(&img, detail.as_ref()).map_err(|e| e.to_string())?;
    let to_pt = |r: [f32; 4]| RectF { x0: r[0] / scale + b.x0, y0: r[1] / scale + b.y0, x1: r[2] / scale + b.x0, y1: r[3] / scale + b.y0 };
    Ok(PageOcr {
        page,
        forced: false,
        engine: ocr.name().to_string(),
        vertical: res.vertical,
        lines: res.lines.into_iter().map(|l| OcrTextLine { bbox: to_pt(l.bbox), text: l.text, vertical: l.vertical, block: l.block.map(|b| b as u32), kind: Some(format!("{:?}", l.kind)) }).collect(),
        regions: res.regions.into_iter().map(|r| OcrRegionInfo { bbox: to_pt(r.bbox), kind: format!("{:?}", r.kind) }).collect(),
    })
}

/// A page whose own text layer cannot be used (so OCR text should be shown).
pub(crate) fn text_layer_unusable(eng: &Engine, page: u32) -> bool {
    let Ok(pg) = eng.load_page(page as i32) else { return false };
    let Ok(b) = pg.bounds() else { return false };
    let Ok(tp) = pg.to_text_page(reflow_flags()) else { return true };
    crate::reflow::needs_ocr(&RichPage::from_page(&pg, &tp, b.width(), b.height())).is_some()
}

/// Whether OCR of `scope` covers a page, and if so whether its text is to replace the
/// page's own text layer.
fn ocr_wanted(eng: &Engine, page: u32, scope: OcrScope) -> Option<bool> {
    let forced = scope == OcrScope::All;
    let Ok(pg) = eng.load_page(page as i32) else { return None };
    let Ok(b) = pg.bounds() else { return None };
    let Ok(tp) = pg.to_text_page(reflow_flags()) else { return Some(forced) };
    let rich = RichPage::from_page(&pg, &tp, b.width(), b.height());
    if crate::reflow::needs_ocr(&rich).is_some() {
        return Some(forced);
    }
    let scan = crate::reflow::scanned_with_text(&rich);
    match scope {
        OcrScope::All => Some(true),
        // A scan whose text layer is mostly garbage (text view shows the page image).
        OcrScope::Needed => (scan && crate::reflow::ocr_layer_quality(&rich) < 0.4).then_some(true),
        OcrScope::Scans => scan.then_some(true),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OcrScope {
    /// Pages without a usable text layer.
    Needed,
    /// Scanned pages, also those with another program's OCR text, which ours replaces.
    Scans,
    /// Every page; the OCR text replaces the page's own.
    All,
}

#[derive(Clone, Debug)]
pub enum OcrEvent {
    Progress { done: usize, total: usize },
    Page(u32),
    Done { pages: usize },
    Error(String),
}

impl Document {
    /// OCR pages on a background thread. Pages already in the store are skipped.
    pub fn run_ocr(&self, scope: OcrScope, engine: Arc<dyn OcrEngine>, cancel: Arc<AtomicBool>) -> Receiver<OcrEvent> {
        let (tx, rx) = unbounded();
        let path = self.info().path.clone();
        let password = self.password();
        let waker = self.waker();
        let store = self.ocr_store().clone();
        let n = self.page_count();
        std::thread::Builder::new()
            .name("strata-ocr".into())
            .spawn(move || {
                let eng = match open_engine(&path, password.as_deref()) {
                    Ok((e, _)) => e,
                    Err(e) => {
                        let _ = tx.send(OcrEvent::Error(e.to_string()));
                        waker();
                        return;
                    }
                };
                let pages: Vec<(u32, bool)> = (0..n as u32)
                    .filter(|&p| store.get(p).is_none())
                    .filter_map(|p| ocr_wanted(&eng, p, scope).map(|forced| (p, forced)))
                    .collect();
                let total = pages.len();
                let _ = tx.send(OcrEvent::Progress { done: 0, total });
                waker();
                for (i, &(p, forced)) in pages.iter().enumerate() {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    match ocr_page(&eng, p, engine.as_ref(), OCR_DPI) {
                        Ok(mut r) => {
                            r.forced = forced;
                            store.insert(r);
                            let _ = tx.send(OcrEvent::Page(p));
                        }
                        Err(e) => {
                            let _ = tx.send(OcrEvent::Error(format!("{} ページ: {e}", p + 1)));
                        }
                    }
                    let _ = tx.send(OcrEvent::Progress { done: i + 1, total });
                    waker();
                }
                let _ = tx.send(OcrEvent::Done { pages: total });
                waker();
            })
            .ok();
        rx
    }
}

// ------------------------------------------------------------ searchable PDF

fn apply_vec(m: &mupdf::Matrix, x: f32, y: f32) -> (f32, f32) {
    (x * m.a + y * m.c, x * m.b + y * m.d)
}

fn apply_pt(m: &mupdf::Matrix, x: f32, y: f32) -> (f32, f32) {
    let (vx, vy) = apply_vec(m, x, y);
    (vx + m.e, vy + m.f)
}

/// Advance of a character in ems for the (proportional) Adobe-Japan1 font.
fn em_width(c: char) -> f32 {
    if (c as u32) >= 0x2E80 { 1.0 } else { 0.5 }
}

fn utf16_hex(s: &str) -> String {
    s.encode_utf16().map(|u| format!("{u:04X}")).collect()
}

/// Content stream drawing the OCR lines as invisible text (render mode 3).
/// `inv` maps MuPDF page space (y down) to PDF user space.
fn text_layer(o: &PageOcr, inv: &mupdf::Matrix) -> String {
    let mut s = String::from("q\nBT\n3 Tr\n");
    for l in &o.lines {
        let text: String = l.text.chars().filter(|c| !c.is_control()).collect();
        if text.trim().is_empty() {
            continue;
        }
        let b = l.bbox;
        let (e1x, e1y) = apply_vec(inv, 1.0, 0.0);
        let (e2x, e2y) = apply_vec(inv, 0.0, -1.0);
        let (font, a, bb, c, d, (ox, oy)) = if l.vertical {
            // Horizontal font rotated 90° clockwise: the baseline runs down the
            // column and glyph tops face right. Readers then extract one line
            // per column (a vertical-mode font yields one line per glyph in MuPDF).
            let size = b.width().max(0.1);
            let ems = text.chars().count().max(1) as f32;
            let s_adv = b.height() / (ems * size);
            let (dx, dy) = apply_vec(inv, 0.0, 1.0);
            let (ux, uy) = apply_vec(inv, 1.0, 0.0);
            ("StrataOcrH", dx * size * s_adv, dy * size * s_adv, ux * size, uy * size, apply_pt(inv, b.x0 + size * 0.12, b.y0))
        } else {
            let size = b.height().max(0.1);
            let ems: f32 = text.chars().map(em_width).sum::<f32>().max(0.5);
            let xs = b.width() / (ems * size);
            ("StrataOcrH", e1x * size * xs, e1y * size * xs, e2x * size, e2y * size, apply_pt(inv, b.x0, b.y1 - size * 0.12))
        };
        s.push_str(&format!("/{font} 1 Tf {a:.4} {bb:.4} {c:.4} {d:.4} {ox:.3} {oy:.3} Tm <{}> Tj\n", utf16_hex(&text)));
    }
    s.push_str("ET\nQ\n");
    s
}

fn add_text_layer(pdf: &mut mupdf::pdf::PdfDocument, page: u32, o: &PageOcr, font: &mupdf::pdf::PdfObject) -> Result<(), mupdf::Error> {
    use mupdf::pdf::PdfObject;
    let mut pobj = pdf.find_page(page as i32)?;
    let ctm = pobj.page_ctm()?;
    let Some(inv) = ctm.invert() else { return Ok(()) };
    // Resources: the page gets its own dictionary if it only inherits one.
    // (`try_clone` deep-copies, so dictionaries are completed before insertion.)
    let own = pobj.get_dict("Resources")?;
    let has_own = own.is_some();
    let mut res = match own {
        Some(r) => r.resolve()?.unwrap_or(r),
        None => match pobj.get_dict_inheritable("Resources")? {
            Some(i) => i.resolve()?.unwrap_or(i).copy_dict()?,
            None => pdf.new_dict()?,
        },
    };
    match res.get_dict("Font")? {
        Some(f) => {
            let mut f = f.resolve()?.unwrap_or(f);
            f.dict_put("StrataOcrH", font.try_clone()?)?;
        }
        None => {
            let mut f = pdf.new_dict()?;
            f.dict_put("StrataOcrH", font.try_clone()?)?;
            res.dict_put("Font", f)?;
        }
    }
    if !has_own {
        pobj.dict_put("Resources", res)?;
    }

    // Wrap existing content in q/Q so its graphics state cannot leak into ours.
    let mut stream = |data: &str| -> Result<PdfObject, mupdf::Error> {
        let buf = mupdf::Buffer::from_bytes(data.as_bytes())?;
        pdf.add_stream(&buf, None, true)
    };
    let q = stream("q\n")?;
    let big_q = stream("Q\n")?;
    let layer = stream(&text_layer(o, &inv))?;
    let mut contents = pdf.new_array()?;
    contents.array_push(q)?;
    if let Some(old) = pobj.get_dict("Contents")? {
        let resolved = old.resolve()?.unwrap_or(old.try_clone()?);
        if resolved.is_array()? {
            for i in 0..resolved.len()? as i32 {
                if let Some(item) = resolved.get_array(i)? {
                    contents.array_push(item)?;
                }
            }
        } else {
            contents.array_push(old)?;
        }
    }
    contents.array_push(big_q)?;
    contents.array_push(layer)?;
    pobj.dict_put("Contents", contents)?;
    Ok(())
}

impl Document {
    /// Recognition quality of the text that another program's OCR left on the scanned
    /// pages (`None` without such pages); pages read by our OCR are left out. Blocking:
    /// call from a background thread.
    pub fn foreign_ocr_quality(&self) -> Option<crate::ocrq::LayerQuality> {
        let (eng, _) = open_engine(&self.info().path, self.password().as_deref()).ok()?;
        let store = self.ocr_store().clone();
        let mut texts = Vec::new();
        for p in 0..self.page_count() as u32 {
            let Ok(pg) = eng.load_page(p as i32) else { continue };
            let Ok(b) = pg.bounds() else { continue };
            let Ok(tp) = pg.to_text_page(reflow_flags() & !mupdf::TextPageFlags::TABLE_HUNT) else { continue };
            let rich = RichPage::from_page(&pg, &tp, b.width(), b.height());
            let ours = store.get(p).is_some_and(|o| o.forced) || crate::reflow::needs_ocr(&rich).is_some();
            if !ours && crate::reflow::scanned_with_text(&rich) {
                texts.push(crate::reflow::page_text(&rich));
            }
        }
        crate::ocrq::LayerQuality::assess_document(&texts, self.page_count())
    }

    /// Save a copy with an invisible text layer on every OCR'd page whose own
    /// text layer is unusable, so the file becomes searchable elsewhere.
    /// Encryption is removed. Blocking: call from a background thread.
    pub fn save_searchable_pdf(&self, dest: &Path) -> Result<usize, String> {
        let (eng, _) = open_engine(&self.info().path, self.password().as_deref()).map_err(|e| e.to_string())?;
        let store = self.ocr_store().clone();
        let n = self.page_count() as u32;
        let pages: Vec<u32> = (0..n).filter(|&p| store.get(p).is_some_and(|o| !o.lines.is_empty()) && text_layer_unusable(&eng, p)).collect();
        let Engine::Pdf(mut pdf) = eng else { return Err("PDF 以外の形式にはテキスト層を追加できません".into()) };
        let e = |e: mupdf::Error| e.to_string();
        let font = mupdf::Font::new_cjk(mupdf::CjkFontOrdering::AdobeJapan).map_err(e)?;
        let fh = pdf.add_cjk_font(&font, mupdf::CjkFontOrdering::AdobeJapan, mupdf::WriteMode::Horizontal, false).map_err(e)?;
        let fonts = fh;
        for &p in &pages {
            if let Some(o) = store.get(p) {
                add_text_layer(&mut pdf, p, &o, &fonts).map_err(|err| format!("{} ページ: {err}", p + 1))?;
            }
        }
        let mut opts = mupdf::pdf::PdfWriteOptions::default();
        opts.set_garbage_level(1).set_compress(true).set_encryption(mupdf::pdf::Encryption::None);
        pdf.save_with_options(&dest.to_string_lossy(), opts).map_err(e)?;
        Ok(pages.len())
    }
}
