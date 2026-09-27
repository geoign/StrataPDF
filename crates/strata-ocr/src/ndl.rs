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

fn session(path: &Path, device: Device) -> Result<(Session, Device), OcrError> {
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

    /// Cascade: short model first, escalate when the result nearly fills it;
    /// very long horizontal lines are read in two halves.
    fn read_line(&self, img: &RgbImage, class: f32) -> Result<String, OcrError> {
        let class = class.round() as i32;
        if class == 3 {
            let t = self.read(&self.rec30, img)?;
            if t.chars().count() < 25 {
                return Ok(t);
            }
        }
        if class == 3 || class == 2 {
            let t = self.read(&self.rec50, img)?;
            if t.chars().count() < 45 {
                return Ok(t);
            }
        }
        let t = self.read(&self.rec100, img)?;
        let (w, h) = img.dimensions();
        if t.chars().count() >= 98 && h < w {
            let left = imageops::crop_imm(img, 0, 0, w / 2, h).to_image();
            let right = imageops::crop_imm(img, w / 2, 0, w - w / 2, h).to_image();
            return Ok(self.read(&self.rec100, &left)? + &self.read(&self.rec100, &right)?);
        }
        Ok(t)
    }
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
        let dets = self.detect(img)?;
        let mut regions: Vec<OcrRegion> = Vec::new();
        let mut raw_lines: Vec<([f32; 4], LineKind, f32, f32)> = Vec::new();
        for (cls, bb, conf, count) in dets {
            match cls {
                Class::Region(kind) => regions.push(OcrRegion { bbox: bb, kind, conf }),
                Class::Line(kind) => raw_lines.push((bb, kind, conf, count)),
            }
        }
        // The detector returns overlapping candidates; keep the most confident line.
        raw_lines.sort_by(|a, b| b.2.total_cmp(&a.2));
        let mut kept: Vec<([f32; 4], LineKind, f32, f32)> = Vec::with_capacity(raw_lines.len());
        for l in raw_lines {
            if !kept.iter().any(|k| overlap(&k.0, &l.0) > 0.4) {
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
            let text = self.read_line(&crop, count)?;
            if text.is_empty() {
                continue;
            }
            lines.push(OcrLine { bbox: bb, text, vertical: bb[3] - bb[1] > bb[2] - bb[0], kind, conf, block: line_block[li] });
        }
        Ok(OcrPage { width: img.width(), height: img.height(), lines, regions, vertical })
    }
}
