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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OcrRegionInfo {
    pub bbox: RectF,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageOcr {
    pub page: u32,
    pub engine: String,
    pub vertical: bool,
    pub lines: Vec<OcrTextLine>,
    pub regions: Vec<OcrRegionInfo>,
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
            .blocks()
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
        let mut out = Vec::new();
        for ls in self.blocks() {
            if ls.iter().all(|l| l.vertical) {
                out.extend(ls.into_iter().map(|l| vec![l]));
                continue;
            }
            let bb = ls.iter().skip(1).fold(ls[0].bbox, |a, l| a.union(&l.bbox));
            let mut cur: Vec<&OcrTextLine> = Vec::new();
            for l in ls {
                let size = l.bbox.height().max(1.0);
                let indented = l.bbox.x0 - bb.x0 > size * 0.7;
                let prev_short = cur.last().is_some_and(|p| p.bbox.x1 < bb.x1 - size * 1.5);
                if !cur.is_empty() && (indented || prev_short) {
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
    pub fn to_rich(&self, width: f32, height: f32) -> RichPage {
        let mut blocks: Vec<RichBlock> = self
            .paragraph_blocks()
            .into_iter()
            .map(|ls| {
                let n = ls.len();
                let lines: Vec<RichLine> = ls
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        let size = if l.vertical { l.bbox.width() } else { l.bbox.height() } * 0.85;
                        // Hyphenated line break in Latin text: join like MuPDF's dehyphenation.
                        let next_lower = ls.get(i + 1).and_then(|n| n.text.chars().next()).is_some_and(|c| c.is_lowercase());
                        let joined = i + 1 < n && l.text.ends_with('-') && next_lower;
                        RichLine {
                            bbox: l.bbox,
                            vertical: l.vertical,
                            dir: if l.vertical { [0.0, 1.0] } else { [1.0, 0.0] },
                            joined,
                            chars: char_boxes(l).into_iter().map(|(c, bbox)| RichChar { c, bbox, size, font: u16::MAX, bold: false, argb: 0xff000000 }).collect(),
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

fn cache_key(path: &Path) -> Option<String> {
    use std::hash::{Hash, Hasher};
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
    let res = ocr.recognize(&img).map_err(|e| e.to_string())?;
    let to_pt = |r: [f32; 4]| RectF { x0: r[0] / scale + b.x0, y0: r[1] / scale + b.y0, x1: r[2] / scale + b.x0, y1: r[3] / scale + b.y0 };
    Ok(PageOcr {
        page,
        engine: ocr.name().to_string(),
        vertical: res.vertical,
        lines: res.lines.into_iter().map(|l| OcrTextLine { bbox: to_pt(l.bbox), text: l.text, vertical: l.vertical, block: l.block.map(|b| b as u32) }).collect(),
        regions: res.regions.into_iter().map(|r| OcrRegionInfo { bbox: to_pt(r.bbox), kind: format!("{:?}", r.kind) }).collect(),
    })
}

/// A page whose own text layer cannot be used (so OCR text should be shown).
pub(crate) fn text_layer_unusable(eng: &Engine, page: u32) -> bool {
    let Ok(pg) = eng.load_page(page as i32) else { return false };
    let Ok(b) = pg.bounds() else { return false };
    let Ok(tp) = pg.to_text_page(reflow_flags()) else { return true };
    crate::reflow::needs_ocr(&RichPage::from_text_page(&tp, b.width(), b.height())).is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OcrScope {
    /// Pages without a usable text layer.
    Needed,
    /// Every page, replacing nothing but adding OCR text alongside.
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
                let pages: Vec<u32> = (0..n as u32)
                    .filter(|&p| store.get(p).is_none())
                    .filter(|&p| scope == OcrScope::All || text_layer_unusable(&eng, p))
                    .collect();
                let total = pages.len();
                let _ = tx.send(OcrEvent::Progress { done: 0, total });
                waker();
                for (i, p) in pages.iter().enumerate() {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    match ocr_page(&eng, *p, engine.as_ref(), OCR_DPI) {
                        Ok(r) => {
                            store.insert(r);
                            let _ = tx.send(OcrEvent::Page(*p));
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
