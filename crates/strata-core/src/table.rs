//! Table extraction from a user-selected region: text segments (from the text
//! layer or OCR) are grouped into rows, columns come from ruling lines or from
//! empty vertical bands, and the result is checked for structural problems.

use std::sync::Arc;

use crossbeam_channel::{Receiver, bounded};

use crate::doc::{Document, Engine, open_engine};
use crate::geom::RectF;
use crate::rich::{RichBlock, RichPage, reflow_flags};

/// A run of text on one line, separated from its neighbours by a wide gap.
#[derive(Clone, Debug)]
pub struct Seg {
    pub rect: RectF,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableSource {
    TextLayer,
    Ocr,
}

#[derive(Clone, Debug)]
pub struct TableResult {
    pub rows: Vec<Vec<String>>,
    /// Problems found; the table should be reviewed when non-empty.
    pub issues: Vec<String>,
    pub source: TableSource,
}

impl TableResult {
    pub fn cols(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }

    pub fn to_delimited(&self, sep: char) -> String {
        let mut out = String::new();
        let n = self.cols();
        for r in &self.rows {
            for c in 0..n {
                if c > 0 {
                    out.push(sep);
                }
                let v = r.get(c).map(String::as_str).unwrap_or("");
                if v.contains([sep, '"', '\n', '\r']) {
                    out.push('"');
                    out.push_str(&v.replace('"', "\"\""));
                    out.push('"');
                } else {
                    out.push_str(v);
                }
            }
            out.push_str("\r\n");
        }
        out
    }

    pub fn to_csv(&self) -> String {
        self.to_delimited(',')
    }

    pub fn to_tsv(&self) -> String {
        self.to_delimited('\t')
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3000..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xFF00..=0xFFEF)
}

fn join_text(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_string();
    }
    let tight = a.chars().last().is_some_and(is_cjk) || b.chars().next().is_some_and(is_cjk);
    if tight { format!("{a}{b}") } else { format!("{a} {b}") }
}

fn looks_numeric(s: &str) -> bool {
    let t: String = s.chars().filter(|c| !matches!(c, ',' | ' ' | '%' | '±' | '+' | '−' | '-' | '(' | ')' | '*' | '~' | '<' | '>')).collect();
    !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '×')
}

/// Column boundaries (x positions between columns).
fn column_cuts(rows: &[Vec<&Seg>], region: RectF, rulings: &[RectF]) -> Vec<f32> {
    // Vertical ruling lines spanning most of the table.
    let mut vx: Vec<f32> = rulings
        .iter()
        .filter(|r| r.width() < 2.5 && r.height() > region.height() * 0.5)
        .map(|r| (r.x0 + r.x1) * 0.5)
        .filter(|x| *x > region.x0 + 3.0 && *x < region.x1 - 3.0)
        .collect();
    vx.sort_by(f32::total_cmp);
    vx.dedup_by(|a, b| (*a - *b).abs() < 3.0);
    if !vx.is_empty() {
        return vx;
    }
    // Otherwise: x ranges covered by text in few rows are gutters. Spanning
    // cells (headers) cover gutters in one or two rows only, so a small share
    // of rows is tolerated.
    let x0 = region.x0.floor() as i32;
    let x1 = region.x1.ceil() as i32;
    let w = (x1 - x0).max(1) as usize;
    let mut cover = vec![0usize; w];
    for r in rows {
        let mut row_cover = vec![false; w];
        for s in r {
            let a = ((s.rect.x0 - x0 as f32).floor().max(0.0) as usize).min(w - 1);
            let b = ((s.rect.x1 - x0 as f32).ceil().max(0.0) as usize).min(w);
            for c in &mut row_cover[a..b] {
                *c = true;
            }
        }
        for (i, c) in row_cover.iter().enumerate() {
            cover[i] += *c as usize;
        }
    }
    // Header rows often span several columns (grouped headings).
    let allowed = if rows.len() >= 3 { ((rows.len() as f32 * 0.2).floor() as usize).max(1) } else { 0 };
    let mut cuts = Vec::new();
    let mut i = 0;
    // Ignore the margins before the first and after the last text.
    let first = cover.iter().position(|&c| c > allowed).unwrap_or(0);
    let last = cover.iter().rposition(|&c| c > allowed).unwrap_or(w - 1);
    i = i.max(first);
    while i <= last {
        if cover[i] <= allowed {
            let start = i;
            while i <= last && cover[i] <= allowed {
                i += 1;
            }
            // Gutters must be wider than a thin space.
            if i - start >= 3 {
                cuts.push(x0 as f32 + (start + i) as f32 * 0.5);
            }
        } else {
            i += 1;
        }
    }
    cuts
}

/// Build a table from text segments inside `region`.
pub fn build_table(mut segs: Vec<Seg>, region: RectF, rulings: &[RectF], source: TableSource) -> TableResult {
    let mut issues = Vec::new();
    segs.retain(|s| !s.text.trim().is_empty());
    if segs.is_empty() {
        return TableResult { rows: Vec::new(), issues: vec!["選択範囲に文字が見つかりませんでした".into()], source };
    }
    // Rows: cluster by vertical overlap.
    segs.sort_by(|a, b| ((a.rect.y0 + a.rect.y1) * 0.5).total_cmp(&((b.rect.y0 + b.rect.y1) * 0.5)));
    let mut rows: Vec<(f32, f32, Vec<&Seg>)> = Vec::new();
    for s in &segs {
        let h = s.rect.height().max(1.0);
        match rows.last_mut() {
            Some((y0, y1, v)) if (y1.min(s.rect.y1) - y0.max(s.rect.y0)) > h.min(*y1 - *y0) * 0.4 => {
                *y0 = y0.min(s.rect.y0);
                *y1 = y1.max(s.rect.y1);
                v.push(s);
            }
            _ => rows.push((s.rect.y0, s.rect.y1, vec![s])),
        }
    }
    let row_segs: Vec<Vec<&Seg>> = rows.iter().map(|r| r.2.clone()).collect();
    let cuts = column_cuts(&row_segs, region, rulings);
    let ncol = cuts.len() + 1;
    let col_of = |x: f32| cuts.iter().filter(|c| x > **c).count();

    let mut spanning = 0;
    let mut table: Vec<(f32, f32, Vec<String>)> = Vec::new();
    for (y0, y1, v) in &rows {
        let mut cells = vec![String::new(); ncol];
        let mut v = v.clone();
        v.sort_by(|a, b| a.rect.x0.total_cmp(&b.rect.x0));
        for s in v {
            let (a, b) = (col_of(s.rect.x0 + 0.5), col_of(s.rect.x1 - 0.5));
            if a != b {
                spanning += 1;
            }
            let c = &mut cells[a.min(ncol - 1)];
            *c = join_text(c, s.text.trim());
        }
        table.push((*y0, *y1, cells));
    }

    // Continuation lines of multi-line cells: first column empty, few cells,
    // and close to the row above.
    let filled = |r: &Vec<String>| r.iter().filter(|c| !c.is_empty()).count();
    let typical = {
        let mut counts: Vec<usize> = table.iter().map(|r| filled(&r.2)).collect();
        counts.sort_unstable();
        counts.get(counts.len() / 2).copied().unwrap_or(0)
    };
    let mut merged: Vec<(f32, f32, Vec<String>)> = Vec::new();
    let mut continuations = 0;
    for r in table {
        if let Some(prev) = merged.last_mut() {
            let gap = r.0 - prev.1;
            let h = (r.1 - r.0).max(1.0);
            if ncol > 1 && r.2[0].is_empty() && filled(&r.2) < typical && gap < h * 0.6 {
                for (c, t) in prev.2.iter_mut().zip(&r.2) {
                    if !t.is_empty() {
                        *c = join_text(c, t);
                    }
                }
                prev.1 = r.1;
                continuations += 1;
                continue;
            }
        }
        merged.push(r);
    }
    let rows: Vec<Vec<String>> = merged.into_iter().map(|r| r.2).collect();

    // Structural checks.
    if rows.len() < 2 || ncol < 2 {
        issues.push(format!("{} 行 × {} 列しか検出できませんでした。範囲や列の区切りを確認してください", rows.len(), ncol));
    }
    let typical = {
        let mut counts: Vec<usize> = rows.iter().map(filled).collect();
        counts.sort_unstable();
        counts.get(counts.len() / 2).copied().unwrap_or(0)
    };
    let odd: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| filled(r) + 1 < typical).map(|(i, _)| i + 1).collect();
    if !odd.is_empty() && rows.len() > 2 {
        let list: Vec<String> = odd.iter().take(8).map(|i| i.to_string()).collect();
        issues.push(format!("セルの数が他の行より少ない行があります（{} 行目）。結合セルや読み取り漏れの可能性があります", list.join(", ")));
    }
    if spanning > 0 {
        issues.push(format!("{spanning} か所の文字列が複数の列にまたがっています。列の区切りが合っていない可能性があります"));
    }
    for c in 0..ncol {
        let vals: Vec<&str> = rows.iter().skip(1).filter_map(|r| r.get(c)).map(String::as_str).filter(|v| !v.is_empty()).collect();
        if vals.len() >= 4 {
            let num = vals.iter().filter(|v| looks_numeric(v)).count();
            if num * 10 >= vals.len() * 7 && num < vals.len() {
                issues.push(format!("{} 列目は数値の列ですが、数値でないセルが {} 個あります", c + 1, vals.len() - num));
            }
        }
    }
    if continuations > 0 && issues.is_empty() && source == TableSource::Ocr {
        issues.push(format!("複数行のセルを {continuations} か所で結合しました。OCR 由来なので結合結果を確認してください"));
    }
    TableResult { rows, issues, source }
}

/// Split structured-text lines inside the region into segments at wide gaps.
fn segments_from_rich(p: &RichPage, region: RectF) -> Vec<Seg> {
    let mut out = Vec::new();
    for b in &p.blocks {
        let RichBlock::Text { lines, .. } = b else { continue };
        for l in lines {
            let mut sizes: Vec<f32> = l.chars.iter().map(|c| c.size).collect();
            sizes.sort_by(f32::total_cmp);
            let med = sizes.get(sizes.len() / 2).copied().unwrap_or(8.0);
            let line_cy = (l.bbox.y0 + l.bbox.y1) * 0.5;
            let mut cur: Option<(RectF, String, f32)> = None;
            let mut space = false;
            let mut in_sup = false;
            for c in &l.chars {
                let (cx, cy) = c.bbox.center();
                if !region.contains(cx, cy) {
                    continue;
                }
                // Spaces fill the gaps between columns: they only mark a word break.
                if c.c.is_whitespace() {
                    space = true;
                    continue;
                }
                let sup = c.size < med * 0.8 && cy < line_cy - med * 0.1;
                let mut piece = String::new();
                if sup && !in_sup {
                    piece.push('^');
                }
                in_sup = sup;
                piece.push(c.c);
                match &mut cur {
                    Some((r, t, last_x1)) if c.bbox.x0 - *last_x1 < med.max(4.0) * 0.6 => {
                        *r = r.union(&c.bbox);
                        if space {
                            t.push(' ');
                        }
                        t.push_str(&piece);
                        *last_x1 = c.bbox.x1;
                    }
                    _ => {
                        if let Some((r, t, _)) = cur.take() {
                            out.push(Seg { rect: r, text: t });
                        }
                        cur = Some((c.bbox, piece, c.bbox.x1));
                    }
                }
                space = false;
            }
            if let Some((r, t, _)) = cur {
                out.push(Seg { rect: r, text: t });
            }
        }
    }
    out
}

fn rulings(p: &RichPage, region: RectF) -> Vec<RectF> {
    p.blocks
        .iter()
        .filter_map(|b| match b {
            RichBlock::Vector { bbox } if bbox.width() < 2.5 || bbox.height() < 2.5 => Some(*bbox),
            _ => None,
        })
        .filter(|r| r.x1 > region.x0 && r.x0 < region.x1 && r.y1 > region.y0 && r.y0 < region.y1)
        .collect()
}

fn extract(eng: &Engine, page: u32, region: RectF, ocr: Option<&dyn crate::ocr::OcrEngine>, store: &crate::ocr::OcrStore) -> Result<TableResult, String> {
    let pg = eng.load_page(page as i32).map_err(|e| e.to_string())?;
    let b = pg.bounds().map_err(|e| e.to_string())?;
    let tp = pg.to_text_page(reflow_flags() | mupdf::TextPageFlags::COLLECT_VECTORS).map_err(|e| e.to_string())?;
    let rich = RichPage::from_text_page(&tp, b.width(), b.height());
    let lines = rulings(&rich, region);
    let forced = store.get(page).is_some_and(|o| o.forced);
    let segs = segments_from_rich(&rich, region);
    let usable = segs.iter().map(|s| s.text.chars().count()).sum::<usize>() >= 3 && crate::reflow::needs_ocr(&rich).is_none();
    if usable && !forced {
        return Ok(build_table(segs, region, &lines, TableSource::TextLayer));
    }
    // OCR the region at 300 dpi (table text is small).
    let Some(ocr) = ocr else { return Err("OCR_REQUIRED".into()) };
    let dl = pg.to_display_list(false).map_err(|e| e.to_string())?;
    let scale = 300.0 / 72.0;
    let pad = 4.0;
    let crop = RectF { x0: region.x0 - pad, y0: region.y0 - pad, x1: region.x1 + pad, y1: region.y1 + pad };
    let (_, _, png) = crate::render::render_region_png(&dl, crop, scale)?;
    let img = image::load_from_memory(&png).map_err(|e| e.to_string())?.to_rgb8();
    let res = ocr.recognize(&img).map_err(|e| e.to_string())?;
    let segs = res
        .lines
        .into_iter()
        .map(|l| Seg {
            rect: RectF { x0: crop.x0 + l.bbox[0] / scale, y0: crop.y0 + l.bbox[1] / scale, x1: crop.x0 + l.bbox[2] / scale, y1: crop.y0 + l.bbox[3] / scale },
            text: l.text,
        })
        .collect();
    Ok(build_table(segs, region, &lines, TableSource::Ocr))
}

impl Document {
    /// Extract a table from a page region on a background thread. Returns
    /// `Err("OCR_REQUIRED")` when the region has no usable text and no OCR
    /// engine was given.
    pub fn extract_table(&self, page: u32, region: RectF, ocr: Option<Arc<dyn crate::ocr::OcrEngine>>) -> Receiver<Result<TableResult, String>> {
        let (tx, rx) = bounded(1);
        let path = self.info().path.clone();
        let password = self.password();
        let waker = self.waker();
        let store = self.ocr_store().clone();
        std::thread::Builder::new()
            .name("strata-table".into())
            .spawn(move || {
                let r = open_engine(&path, password.as_deref()).map_err(|e| e.to_string()).and_then(|(eng, _)| extract(&eng, page, region, ocr.as_deref(), &store));
                let _ = tx.send(r);
                waker();
            })
            .ok();
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(x0: f32, y0: f32, x1: f32, text: &str) -> Seg {
        Seg { rect: RectF { x0, y0, x1, y1: y0 + 10.0 }, text: text.into() }
    }

    fn region() -> RectF {
        RectF { x0: 0.0, y0: 0.0, x1: 300.0, y1: 200.0 }
    }

    #[test]
    fn simple_grid_without_rulings() {
        let segs = vec![
            seg(10.0, 10.0, 60.0, "Name"),
            seg(110.0, 10.0, 150.0, "Age"),
            seg(210.0, 10.0, 260.0, "City"),
            seg(10.0, 30.0, 50.0, "Alice"),
            seg(110.0, 30.0, 125.0, "30"),
            seg(210.0, 30.0, 250.0, "Tokyo"),
            seg(10.0, 50.0, 45.0, "Bob"),
            seg(110.0, 50.0, 125.0, "41"),
            seg(210.0, 50.0, 255.0, "Osaka"),
        ];
        let t = build_table(segs, region(), &[], TableSource::TextLayer);
        assert_eq!(t.rows, vec![vec!["Name", "Age", "City"], vec!["Alice", "30", "Tokyo"], vec!["Bob", "41", "Osaka"]]);
        assert!(t.issues.is_empty(), "{:?}", t.issues);
        assert_eq!(t.to_csv(), "Name,Age,City\r\nAlice,30,Tokyo\r\nBob,41,Osaka\r\n");
    }

    #[test]
    fn spanning_header_and_continuation_line() {
        let segs = vec![
            seg(10.0, 10.0, 250.0, "Measurements of the samples"),
            seg(10.0, 30.0, 60.0, "Sample"),
            seg(110.0, 30.0, 160.0, "Porosity"),
            seg(210.0, 30.0, 260.0, "Note"),
            seg(10.0, 50.0, 40.0, "S1"),
            seg(110.0, 50.0, 125.0, "12.5"),
            seg(210.0, 50.0, 280.0, "flow bands"),
            seg(210.0, 62.0, 270.0, "near top"),
            seg(10.0, 80.0, 40.0, "S2"),
            seg(110.0, 80.0, 125.0, "8.1"),
            seg(210.0, 80.0, 250.0, "fracture"),
        ];
        let t = build_table(segs, region(), &[], TableSource::TextLayer);
        assert_eq!(t.cols(), 3);
        assert_eq!(t.rows[2], vec!["S1", "12.5", "flow bands near top"]);
        assert!(!t.issues.is_empty(), "the spanning title row should be reported");
    }

    #[test]
    fn rulings_define_columns_and_csv_quotes() {
        let segs = vec![seg(5.0, 10.0, 95.0, "a, b"), seg(105.0, 10.0, 150.0, "c"), seg(5.0, 30.0, 40.0, "d\"e"), seg(105.0, 30.0, 140.0, "f")];
        let lines = vec![RectF { x0: 99.5, y0: 0.0, x1: 100.5, y1: 190.0 }];
        let t = build_table(segs, region(), &lines, TableSource::TextLayer);
        assert_eq!(t.to_csv(), "\"a, b\",c\r\n\"d\"\"e\",f\r\n");
    }
}
