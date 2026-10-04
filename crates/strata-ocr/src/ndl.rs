//! Rust port of the NDLOCR-Lite inference pipeline (ndl-lab/ndlocr-lite, CC BY 4.0).
//!
//! 1. DEIM detects text blocks, lines (with a coarse length class) and other
//!    regions on the page padded to a square.
//! 2. PARSeq reads each line; the length class picks the 30-, 50- or
//!    100-character model, escalating when the result is long. Vertical lines
//!    are rotated 90° counter-clockwise before recognition.
//! 3. Lines are grouped into their text blocks and ordered by XY-cut.

use std::path::Path;

use image::{RgbImage, imageops};
use ndarray::Array4;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;
use parking_lot::Mutex;

use crate::models::ModelSet;
use crate::order;
use crate::{Device, LineKind, OcrEngine, OcrError, OcrLine, OcrPage, OcrRegion, RegionKind};

const DET_CONF: f32 = 0.25;
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];

/// Class names of `ndl.yaml`, index = label - 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Line(LineKind),
    Region(RegionKind),
}

fn class_of(i: i64) -> Option<Class> {
    use Class::*;
    Some(match i {
        0 => Region(RegionKind::TextBlock),
        1 => Line(LineKind::Main),
        2 => Line(LineKind::Caption),
        3 => Line(LineKind::Advert),
        4 => Line(LineKind::Note),
        5 => Line(LineKind::InlineNote),
        6 => Region(RegionKind::Figure),
        7 => Region(RegionKind::Advert),
        8 => Region(RegionKind::Header),
        9 => Region(RegionKind::Folio),
        10 => Region(RegionKind::Ruby),
        11 => Region(RegionKind::Chart),
        12 => Region(RegionKind::Equation),
        13 => Region(RegionKind::ChemicalFormula),
        14 => Region(RegionKind::Latin),
        15 => Region(RegionKind::Table),
        16 => Line(LineKind::Title),
        _ => return None,
    })
}

struct Recognizer {
    session: Mutex<Session>,
    width: u32,
    height: u32,
}

pub struct NdlOcr {
    det: Mutex<Session>,
    det_size: u32,
    rec30: Recognizer,
    rec50: Recognizer,
    rec100: Recognizer,
    charset: Vec<char>,
    pub device_used: Device,
}

pub(crate) fn session(path: &Path, device: Device) -> Result<(Session, Device), OcrError> {
    let mut b = Session::builder()?.with_optimization_level(GraphOptimizationLevel::Level3).map_err(|e| OcrError::Inference(e.to_string()))?;
    let mut used = Device::Cpu;
    if device == Device::Gpu {
        // DirectML requires sequential execution and no memory pattern.
        b = b
            .with_parallel_execution(false)
            .map_err(|e| OcrError::Inference(e.to_string()))?
            .with_memory_pattern(false)
            .map_err(|e| OcrError::Inference(e.to_string()))?;
        match b.with_execution_providers([ort::ep::DirectML::default().build().error_on_failure()]) {
            Ok(nb) => {
                b = nb;
                used = Device::Gpu;
            }
            Err(e) => {
                log::warn!("DirectML unavailable, using CPU: {e}");
                b = e.recover();
            }
        }
    }
    let s = b.commit_from_file(path)?;
    Ok((s, used))
}

/// Parse `charset_train` from NDLmoji.yaml (a double-quoted YAML scalar).
fn load_charset(path: &Path) -> Result<Vec<char>, OcrError> {
    let s = std::fs::read_to_string(path)?;
    let start = s.find("charset_train:").ok_or_else(|| OcrError::Inference("charset_train missing".into()))?;
    let rest = &s[start..];
    let q = rest.find('"').ok_or_else(|| OcrError::Inference("bad charset".into()))?;
    let mut out = Vec::new();
    let mut it = rest[q + 1..].chars();
    while let Some(c) = it.next() {
        match c {
            '"' => break,
            '\\' => match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let h: String = it.by_ref().take(4).collect();
                    if let Some(ch) = u32::from_str_radix(&h, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            _ => out.push(c),
        }
    }
    Ok(out)
}

fn input_hw(s: &Session) -> (u32, u32) {
    let shape = s.inputs()[0].dtype().tensor_shape().map(|s| s.to_vec()).unwrap_or_default();
    let h = shape.get(2).copied().unwrap_or(0).max(1) as u32;
    let w = shape.get(3).copied().unwrap_or(0).max(1) as u32;
    (h, w)
}

impl NdlOcr {
    pub fn load(set: &ModelSet, device: Device) -> Result<NdlOcr, OcrError> {
        if !set.is_installed() {
            return Err(OcrError::NotInstalled(set.title.clone()));
        }
        // DEIM's post-processing (TopK/GatherElements) returns no detections
        // under DirectML, so the detector always runs on the CPU.
        let (det, _) = session(&set.path("deim.onnx"), Device::Cpu)?;
        let used = device;
        let (det_size, _) = input_hw(&det);
        let mut used = used;
        let mut rec = |name: &str| -> Result<Recognizer, OcrError> {
            let (s, u) = session(&set.path(name), device)?;
            used = u;
            let (h, w) = input_hw(&s);
            Ok(Recognizer { session: Mutex::new(s), width: w, height: h })
        };
        Ok(NdlOcr {
            det: Mutex::new(det),
            det_size,
            rec30: rec("parseq30.onnx")?,
            rec50: rec("parseq50.onnx")?,
            rec100: rec("parseq100.onnx")?,
            charset: load_charset(&set.path("NDLmoji.yaml"))?,
            device_used: used,
        })
    }

    fn detect(&self, img: &RgbImage) -> Result<Vec<(Class, [f32; 4], f32, f32)>, OcrError> {
        let (w, h) = img.dimensions();
        let side = w.max(h);
        let mut padded = RgbImage::new(side, side);
        imageops::replace(&mut padded, img, 0, 0);
        let n = self.det_size;
        let resized = imageops::resize(&padded, n, n, imageops::FilterType::CatmullRom);
        let mut arr = Array4::<f32>::zeros((1, 3, n as usize, n as usize));
        for (x, y, p) in resized.enumerate_pixels() {
            for c in 0..3 {
                arr[[0, c, y as usize, x as usize]] = (p[c] as f32 / 255.0 - MEAN[c]) / STD[c];
            }
        }
        let sizes = ndarray::Array2::<i64>::from_shape_vec((1, 2), vec![n as i64, n as i64]).unwrap();
        let mut s = self.det.lock();
        let out = s.run(ort::inputs!["images" => Tensor::from_array(arr)?, "orig_target_sizes" => Tensor::from_array(sizes)?])?;
        let labels: Vec<i64> = match out["labels"].try_extract_array::<i64>() {
            Ok(a) => a.iter().copied().collect(),
            Err(_) => out["labels"].try_extract_array::<f32>()?.iter().map(|v| *v as i64).collect(),
        };
        let boxes: Vec<f32> = out["boxes"].try_extract_array::<f32>()?.iter().copied().collect();
        let scores: Vec<f32> = out["scores"].try_extract_array::<f32>()?.iter().copied().collect();
        let counts: Vec<f32> = match out.get("char_count") {
            Some(v) => match v.try_extract_array::<f32>() {
                Ok(a) => a.iter().copied().collect(),
                Err(_) => v.try_extract_array::<i64>()?.iter().map(|x| *x as f32).collect(),
            },
            None => vec![100.0; scores.len()],
        };
        let scale = side as f32 / n as f32;
        let mut dets = Vec::new();
        for i in 0..scores.len() {
            if scores[i] <= DET_CONF {
                continue;
            }
            let Some(cls) = class_of(labels[i] - 1) else { continue };
            let b = &boxes[i * 4..i * 4 + 4];
            // Integer boxes as in the reference implementation (truncation).
            let bb = [
                ((b[0] * scale).trunc()).clamp(0.0, side as f32).min(w as f32),
                ((b[1] * scale).trunc()).clamp(0.0, side as f32).min(h as f32),
                ((b[2] * scale).trunc()).clamp(0.0, side as f32).min(w as f32),
                ((b[3] * scale).trunc()).clamp(0.0, side as f32).min(h as f32),
            ];
            if bb[2] - bb[0] < 1.0 || bb[3] - bb[1] < 1.0 {
                continue;
            }
            dets.push((cls, bb, scores[i], counts.get(i).copied().unwrap_or(100.0)));
        }
        Ok(dets)
    }

    fn read(&self, r: &Recognizer, line: &RgbImage) -> Result<String, OcrError> {
        let (w, h) = line.dimensions();
        let img = if h as f32 > w as f32 * 0.8 { imageops::rotate270(line) } else { line.clone() };
        let resized = resize_bilinear_cv(&img, r.width, r.height);
        let mut arr = Array4::<f32>::zeros((1, 3, r.height as usize, r.width as usize));
        for y in 0..r.height as usize {
            for x in 0..r.width as usize {
                // BGR order, scaled to [-1, 1].
                for c in 0..3 {
                    arr[[0, c, y, x]] = resized[(y * r.width as usize + x) * 3 + (2 - c)] / 127.5 - 1.0;
                }
            }
        }
        let mut s = r.session.lock();
        let out = s.run(ort::inputs![Tensor::from_array(arr)?])?;
        let logits = out[0].try_extract_array::<f32>()?;
        let shape = logits.shape().to_vec();
        let (steps, classes) = (shape[1], shape[2]);
        let flat: Vec<f32> = logits.iter().copied().collect();
        let mut text = String::new();
        for t in 0..steps {
            let row = &flat[t * classes..(t + 1) * classes];
            let (best, _) = row.iter().enumerate().fold((0usize, f32::NEG_INFINITY), |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc });
            if best == 0 {
                break;
            }
            if let Some(&c) = self.charset.get(best - 1) {
                text.push(c);
            }
        }
        Ok(text)
    }

    /// The line read by each recognizer (30, 50 and 100 characters), for diagnosis.
    #[doc(hidden)]
    pub fn read_with_each(&self, line: &RgbImage) -> Result<[String; 3], OcrError> {
        Ok([self.read(&self.rec30, line)?, self.read(&self.rec50, line)?, self.read(&self.rec100, line)?])
    }

    /// The cascade's reading for a length class, for diagnosis.
    #[doc(hidden)]
    pub fn read_line_class(&self, img: &RgbImage, class: f32) -> Result<String, OcrError> {
        self.read_line(img, class)
    }

    /// Cascade: short model first, escalate when the result nearly fills it;
    /// very long horizontal lines are read in two halves.
    fn read_line(&self, img: &RgbImage, class: f32) -> Result<String, OcrError> {
        // The line's length in characters (Japanese characters are about square and
        // take some 80% of the line's thickness, Latin ones about half of it). A model
        // for shorter lines given a longer one may drop a stretch from its middle
        // instead of stopping at its limit: a result far shorter than that goes on to
        // the next model. (Characters without the spaces the models put after punctuation.)
        let (w, h) = img.dimensions();
        let squares = w.max(h) as f32 / (w.min(h).max(1) as f32 * 0.8);
        let dropped = |t: &str| (non_space(t) as f32) < squares * if latin_text(t) { 1.6 } else { 1.0 } * 0.7;
        // The detector's length class can call a short line long (a name set with wide
        // spacing, which the 100-character model read backwards): a line too short for
        // that class by its shape starts with the model for its length.
        let mut class = class.round() as i32;
        if squares < 22.0 {
            class = 3;
        } else if squares < 42.0 && class < 2 {
            class = 2;
        }
        let n = non_space;
        // A model stops at its length in characters, spaces included: a Latin line
        // (many spaces) cut off at 50 has fewer than 45 others ("…on the caldera flo").
        let full = |t: &str, limit: usize| n(t) >= limit - 5 || t.chars().count() >= limit - 2;
        if class == 3 {
            let t = self.read(&self.rec30, img)?;
            if !full(&t, 30) && !dropped(&t) {
                return Ok(t);
            }
        }
        let mut shorter_model: Option<String> = None;
        if class == 3 || class == 2 {
            let t = self.read(&self.rec50, img)?;
            if !full(&t, 50) && !dropped(&t) {
                return Ok(t);
            }
            shorter_model = Some(t);
        }
        let t = self.read(&self.rec100, img)?;
        if (n(&t) >= 98 || t.chars().count() >= 99) && h < w {
            let left = imageops::crop_imm(img, 0, 0, w / 2, h).to_image();
            let right = imageops::crop_imm(img, w / 2, 0, w - w / 2, h).to_image();
            return Ok(self.read(&self.rec100, &left)? + &self.read(&self.rec100, &right)?);
        }
        // The larger model can drop a stretch too: when it reads clearly less than the
        // 50-character model (asked now if it was not), that reading stands.
        let other = match shorter_model {
            Some(s) => Some(s),
            None if dropped(&t) => Some(self.read(&self.rec50, img)?),
            None => None,
        };
        if let Some(s) = other
            && n(&s) * 10 > n(&t) * 12
        {
            return Ok(s);
        }
        Ok(t)
    }
}

fn non_space(t: &str) -> usize {
    t.chars().filter(|c| !c.is_whitespace()).count()
}

/// Mostly Latin letters, digits and ASCII punctuation (an English line).
fn latin_text(t: &str) -> bool {
    let (mut ascii, mut other, mut letters) = (0usize, 0usize, 0usize);
    for c in t.chars().filter(|c| !c.is_whitespace()) {
        if c.is_ascii() {
            ascii += 1;
            letters += c.is_ascii_alphabetic() as usize;
        } else {
            other += 1;
        }
    }
    letters >= 4 && ascii >= (ascii + other) * 7 / 10
}

/// A reading in which the recognizer got stuck on one character or word: a run
/// that print does not have ("16,00000m", "GaK-55555", "路路路路", "The The The").
/// The model tends to stop early after one, losing the rest of the line.
pub(crate) fn degenerate(t: &str) -> bool {
    degeneracy(t) > 0
}

/// How far a reading is stuck: characters in runs beyond what print has, and
/// repeated words (0 for a clean reading).
fn degeneracy(t: &str) -> usize {
    let mut excess = 0;
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let mut j = i + 1;
        while j < chars.len() && chars[j] == c {
            j += 1;
        }
        let run = j - i;
        let limit = match c {
            // Leaders, rules, dashes, boxes for unreadable characters.
            '.' | '…' | '‥' | '・' | '-' | '—' | '―' | '─' | '－' | '=' | '_' | '*' | '□' | '■' | 'ー' | '〃' | ' ' | '　' => usize::MAX,
            // (1:1000000)
            '0' => 7,
            c if ('\u{4E00}'..='\u{9FFF}').contains(&c) => 3,
            c if ('\u{3040}'..='\u{30FF}').contains(&c) => 4,
            _ => 5,
        };
        if run >= limit {
            excess += run + 1 - limit;
        }
        // Thousands are grouped by three digits: "16,0000m" (not "1960,1961").
        if c == ',' && i > 0 && chars[i - 1].is_ascii_digit() && run == 1 {
            let digits = chars[j..].iter().take_while(|d| d.is_ascii_digit()).count();
            if digits >= 4 && chars[j..j + digits].iter().all(|&d| d == '0') {
                excess += digits - 3;
            }
        }
        i = j;
    }
    // One word three times running.
    let words: Vec<&str> = t.split_whitespace().collect();
    excess + words.windows(3).filter(|w| w[0] == w[1] && w[1] == w[2] && w[0].chars().filter(|c| c.is_alphabetic()).count() >= 2).count()
}

/// Of several readings of one line, the least stuck, and among those the one
/// closest to all the others (a reading that dropped or repeated a stretch
/// differs from the rest there); ties go to the longer one.
fn consensus(cands: Vec<String>) -> String {
    let least = cands.iter().map(|t| degeneracy(t)).min().unwrap_or(0);
    let cands: Vec<String> = cands.into_iter().filter(|t| degeneracy(t) == least).collect();
    let chars: Vec<Vec<char>> = cands.iter().map(|t| t.chars().filter(|c| !c.is_whitespace()).collect()).collect();
    let cost = |i: usize| -> usize { (0..chars.len()).filter(|&j| j != i).map(|j| edit_distance(&chars[i], &chars[j])).sum() };
    let best = (0..cands.len()).min_by_key(|&i| (cost(i), usize::MAX - chars[i].len())).unwrap_or(0);
    cands.into_iter().nth(best).unwrap_or_default()
}

fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + (ca != cb) as usize).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Bilinear resize matching OpenCV's `INTER_LINEAR` (half-pixel centres, no
/// anti-aliasing when shrinking), which the recognizers were trained with.
/// Returns interleaved RGB as f32.
fn resize_bilinear_cv(img: &RgbImage, w: u32, h: u32) -> Vec<f32> {
    let (sw, sh) = img.dimensions();
    let (fx, fy) = (sw as f32 / w as f32, sh as f32 / h as f32);
    let src = img.as_raw();
    let px = |x: u32, y: u32, c: usize| src[((y * sw + x) * 3) as usize + c] as f32;
    let mut out = vec![0f32; (w * h * 3) as usize];
    for y in 0..h {
        let sy = ((y as f32 + 0.5) * fy - 0.5).max(0.0);
        let y0 = (sy.floor() as u32).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = sy - y0 as f32;
        for x in 0..w {
            let sx = ((x as f32 + 0.5) * fx - 0.5).max(0.0);
            let x0 = (sx.floor() as u32).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let wx = sx - x0 as f32;
            for c in 0..3 {
                let top = px(x0, y0, c) * (1.0 - wx) + px(x1, y0, c) * wx;
                let bot = px(x0, y1, c) * (1.0 - wx) + px(x1, y1, c) * wx;
                // OpenCV rounds to u8 before the model sees the image.
                out[((y * w + x) * 3) as usize + c] = (top * (1.0 - wy) + bot * wy).round();
            }
        }
    }
    out
}

/// Intersection over the smaller box: catches both duplicates and a short
/// candidate nested inside a longer one.
fn overlap(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let w = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let h = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let area = |r: &[f32; 4]| ((r[2] - r[0]) * (r[3] - r[1])).max(1e-3);
    w * h / area(a).min(area(b))
}

impl OcrEngine for NdlOcr {
    fn name(&self) -> &str {
        "NDLOCR-Lite"
    }

    fn recognize(&self, img: &RgbImage) -> Result<OcrPage, OcrError> {
        self.recognize_detailed(img, None)
    }

    fn recognize_detailed(&self, img: &RgbImage, detail: Option<&RgbImage>) -> Result<OcrPage, OcrError> {
        let dets = self.detect(img)?;
        let mut regions: Vec<OcrRegion> = Vec::new();
        let mut raw_lines: Vec<([f32; 4], LineKind, f32, f32)> = Vec::new();
        for (cls, bb, conf, count) in dets {
            match cls {
                Class::Region(kind) => regions.push(OcrRegion { bbox: bb, kind, conf }),
                Class::Line(kind) => raw_lines.push((bb, kind, conf, count)),
            }
        }
        // The detector returns overlapping candidates; keep the most confident line,
        // unless a less confident one is the whole line that it is part of: the same
        // thickness, longer, holding it (and any other parts kept). Keeping the
        // confident part lost the rest of the line, a clause or a sentence.
        raw_lines.sort_by(|a, b| b.2.total_cmp(&a.2));
        let area = |r: &[f32; 4]| ((r[2] - r[0]) * (r[3] - r[1])).max(1e-3);
        let mut kept: Vec<([f32; 4], LineKind, f32, f32)> = Vec::with_capacity(raw_lines.len());
        for l in raw_lines {
            let over: Vec<usize> = (0..kept.len()).filter(|&i| overlap(&kept[i].0, &l.0) > 0.4).collect();
            if over.is_empty() {
                kept.push(l);
                continue;
            }
            let b = l.0;
            let vertical = b[3] - b[1] > b[2] - b[0];
            let thickness = |r: &[f32; 4]| if vertical { r[2] - r[0] } else { r[3] - r[1] };
            let whole = over.iter().all(|&i| {
                let k = &kept[i].0;
                let inside = ((k[2].min(b[2]) - k[0].max(b[0])).max(0.0) * (k[3].min(b[3]) - k[1].max(b[1])).max(0.0)) / area(k);
                inside > 0.7 && thickness(&b) <= thickness(k) * 1.3 && area(&b) > area(k) * 1.3
            });
            if whole {
                for &i in over.iter().rev() {
                    kept.remove(i);
                }
                kept.push(l);
            }
        }
        let raw_lines = kept;
        let vertical_count = raw_lines.iter().filter(|l| l.0[3] - l.0[1] > l.0[2] - l.0[0]).count();
        let vertical = vertical_count * 2 > raw_lines.len();

        // Group lines into text blocks (by line center).
        let blocks: Vec<usize> = regions.iter().enumerate().filter(|(_, r)| r.kind == RegionKind::TextBlock).map(|(i, _)| i).collect();
        let in_block = |b: &[f32; 4]| -> Option<usize> {
            let (cx, cy) = ((b[0] + b[2]) * 0.5, (b[1] + b[3]) * 0.5);
            blocks.iter().copied().filter(|&i| {
                let r = regions[i].bbox;
                cx >= r[0] && cx <= r[2] && cy >= r[1] && cy <= r[3]
            }).min_by(|&a, &b| {
                let area = |r: [f32; 4]| (r[2] - r[0]) * (r[3] - r[1]);
                area(regions[a].bbox).total_cmp(&area(regions[b].bbox))
            })
        };
        // Units for ordering: each text block with its lines, and lone lines.
        let mut groups: Vec<([f32; 4], Vec<usize>)> = Vec::new();
        let mut block_group: std::collections::HashMap<usize, usize> = Default::default();
        let mut line_block = Vec::with_capacity(raw_lines.len());
        for (li, l) in raw_lines.iter().enumerate() {
            let b = in_block(&l.0);
            line_block.push(b);
            match b {
                Some(bi) => {
                    let gi = *block_group.entry(bi).or_insert_with(|| {
                        groups.push((regions[bi].bbox, Vec::new()));
                        groups.len() - 1
                    });
                    groups[gi].1.push(li);
                }
                None => groups.push((l.0, vec![li])),
            }
        }
        let rects: Vec<[f32; 4]> = groups.iter().map(|g| g.0).collect();
        let gap = (img.width().max(img.height()) as f32 * 0.004).max(2.0);
        let mut ordered_lines = Vec::with_capacity(raw_lines.len());
        for gi in order::order(&rects, vertical, gap) {
            let mut ls = groups[gi].1.clone();
            if vertical {
                ls.sort_by(|&a, &b| raw_lines[b].0[2].total_cmp(&raw_lines[a].0[2]));
            } else {
                ls.sort_by(|&a, &b| raw_lines[a].0[1].total_cmp(&raw_lines[b].0[1]));
            }
            ordered_lines.extend(ls);
        }

        let mut lines = Vec::with_capacity(ordered_lines.len());
        for li in ordered_lines {
            let (bb, kind, conf, count) = raw_lines[li];
            let x0 = bb[0] as u32;
            let y0 = bb[1] as u32;
            let x1 = (bb[2] as u32).min(img.width());
            let y1 = (bb[3] as u32).min(img.height());
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let crop = imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image();
            let mut text = self.read_line(&crop, count)?;
            let vertical_line = bb[3] - bb[1] > bb[2] - bb[0];
            // The recognizers are unsteady on some lines: a box a pixel or two off reads
            // differently, dropping or repeating words ("found near the the deposit",
            // "16,00000m. Scov-"), each time in another place. Such lines are read again
            // from slightly different crops, and the reading closest to the others stands.
            // Latin letters are a few pixels high at the page's resolution: their other
            // crops come from the page at a higher resolution, which reads them better.
            // (Japanese reads worse from it: its strokes alias when shrunk to the models'
            // height.)
            let latin = !vertical_line && latin_text(&text);
            if latin || degenerate(&text) {
                let mut cands = vec![std::mem::take(&mut text)];
                let fine = detail.filter(|_| !vertical_line).map(|d| (d, d.width() as f32 / img.width() as f32));
                // Crops: from the page or its finer rendering, grown across and along the
                // line by some pixels of the page.
                let crops: &[(bool, f32, f32)] = if fine.is_some() {
                    &[(true, 0.0, 0.0), (true, 2.0, 0.0)]
                } else {
                    &[(false, 2.0, 0.0), (false, 3.0, 3.0), (false, 0.0, 2.0)]
                };
                for &(use_fine, across, along) in crops {
                    let (gx, gy) = if vertical_line { (across, along) } else { (along, across) };
                    let (src, s) = match fine {
                        Some((d, s)) if use_fine => (d, s),
                        _ => (img, 1.0),
                    };
                    let cx0 = ((bb[0] - gx) * s).max(0.0) as u32;
                    let cy0 = ((bb[1] - gy) * s).max(0.0) as u32;
                    let cx1 = (((bb[2] + gx) * s) as u32).min(src.width());
                    let cy1 = (((bb[3] + gy) * s) as u32).min(src.height());
                    if cx1 > cx0 && cy1 > cy0 {
                        cands.push(self.read_line(&imageops::crop_imm(src, cx0, cy0, cx1 - cx0, cy1 - cy0).to_image(), count)?);
                    }
                }
                text = consensus(cands);
            }
            // `STRATA_OCR_DEBUG`: each line with its length class and every model's reading.
            if std::env::var("STRATA_OCR_DEBUG").is_ok() {
                let each = self.read_with_each(&crop)?;
                eprintln!("[{x0},{y0},{x1},{y1}] class={count:.0} -> {text}\n   30: {}\n   50: {}\n  100: {}", each[0], each[1], each[2]);
            }
            if text.is_empty() {
                continue;
            }
            lines.push(OcrLine { bbox: bb, text, vertical: vertical_line, kind, conf, block: line_block[li] });
        }
        Ok(OcrPage { width: img.width(), height: img.height(), lines, regions, vertical })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuck_readings() {
        assert!(degenerate("and TOGASHI, S. (198666666666666 of Izu-Oshima Volcano."));
        assert!(degenerate("rose up to a height of 16,0000m. Scov-"));
        assert!(degenerate("屈斜路路路路路路路釧斜路"));
        assert!(degenerate("and The The The Ball"));
        assert!(!degenerate("NAKAMURA(1960,1961)らによっ"));
        assert!(!degenerate("Turbidites……………………………………11"));
        assert!(!degenerate("1:1000000 の地形図"));
        assert!(!degenerate("LBI and III lavas"));
    }

    #[test]
    fn consensus_prefers_agreement() {
        let c = consensus(vec![
            "found near the the deposit.".into(),
            "found near the top of the deposit.".into(),
            "found near the top of the deposit".into(),
        ]);
        assert_eq!(c, "found near the top of the deposit.");
        assert_eq!(consensus(vec!["16,00000m. Scov-".into(), "16,000m. Scoriaceous".into()]), "16,000m. Scoriaceous");
    }

    #[test]
    fn latin_lines() {
        assert!(latin_text("Geol. Surv. Japan, vol. 38(11), p. 609-630."));
        assert!(!latin_text("1986年伊豆大島噴火は,若干の前兆的現象に続いて,"));
    }
}
