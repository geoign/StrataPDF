//! Formula recognition (image to LaTeX) with Pix2Text MFR 1.5 (MIT): a DeiT
//! encoder and TrOCR decoder exported to ONNX, decoded greedily.

use std::collections::HashMap;

use image::{RgbImage, imageops};
use ndarray::{Array2, Array4};
use ort::session::Session;
use ort::value::Tensor;
use parking_lot::Mutex;

use crate::models::ModelSet;
use crate::{Device, OcrError};

const SIZE: u32 = 384;
const BOS: i64 = 1;
const EOS: i64 = 2;
const MAX_TOKENS: usize = 400;

pub trait FormulaEngine: Send + Sync {
    fn to_latex(&self, img: &RgbImage) -> Result<String, OcrError>;
}

pub struct Pix2TextMfr {
    encoder: Mutex<Session>,
    decoder: Mutex<Session>,
    /// Token id -> byte-level BPE token.
    vocab: Vec<String>,
    byte_of: HashMap<char, u8>,
}

/// Inverse of GPT-2's `bytes_to_unicode`.
fn byte_decoder() -> HashMap<char, u8> {
    let mut printable: Vec<u32> = (33..=126).chain(161..=172).chain(174..=255).collect();
    let mut chars: Vec<u32> = printable.clone();
    let mut n = 0;
    for b in 0..=255u32 {
        if !printable.contains(&b) {
            printable.push(b);
            chars.push(256 + n);
            n += 1;
        }
    }
    printable.into_iter().zip(chars).map(|(b, c)| (char::from_u32(c).unwrap(), b as u8)).collect()
}

fn load_vocab(path: &std::path::Path) -> Result<Vec<String>, OcrError> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?).map_err(|e| OcrError::Inference(e.to_string()))?;
    let map = v["model"]["vocab"].as_object().ok_or_else(|| OcrError::Inference("tokenizer vocab missing".into()))?;
    let mut vocab = vec![String::new(); map.len()];
    for (tok, id) in map {
        if let Some(i) = id.as_u64().map(|i| i as usize)
            && i < vocab.len()
        {
            vocab[i] = tok.clone();
        }
    }
    // Added special tokens live outside the BPE vocab.
    if let Some(added) = v["added_tokens"].as_array() {
        for a in added {
            if let (Some(id), Some(c)) = (a["id"].as_u64(), a["content"].as_str())
                && (id as usize) < vocab.len()
            {
                vocab[id as usize] = c.to_string();
            }
        }
    }
    Ok(vocab)
}

impl Pix2TextMfr {
    /// Always runs on the CPU: the decoder's input grows every step, and
    /// DirectML re-plans the graph for each new shape (5-10x slower).
    pub fn load(set: &ModelSet) -> Result<Pix2TextMfr, OcrError> {
        let device = Device::Cpu;
        if !set.is_installed() {
            return Err(OcrError::NotInstalled(set.title.clone()));
        }
        let (encoder, _) = crate::ndl::session(&set.path("encoder_model.onnx"), device)?;
        let (decoder, _) = crate::ndl::session(&set.path("decoder_model.onnx"), device)?;
        Ok(Pix2TextMfr { encoder: Mutex::new(encoder), decoder: Mutex::new(decoder), vocab: load_vocab(&set.path("tokenizer.json"))?, byte_of: byte_decoder() })
    }

    fn decode(&self, ids: &[i64]) -> String {
        let mut bytes = Vec::new();
        for &i in ids {
            let Some(tok) = self.vocab.get(i as usize) else { continue };
            if tok.starts_with('<') && tok.ends_with('>') {
                continue;
            }
            for c in tok.chars() {
                match self.byte_of.get(&c) {
                    Some(b) => bytes.push(*b),
                    None => bytes.extend(c.to_string().as_bytes()),
                }
            }
        }
        String::from_utf8_lossy(&bytes).trim().to_string()
    }
}

impl FormulaEngine for Pix2TextMfr {
    fn to_latex(&self, img: &RgbImage) -> Result<String, OcrError> {
        let resized = imageops::resize(img, SIZE, SIZE, imageops::FilterType::CatmullRom);
        let mut arr = Array4::<f32>::zeros((1, 3, SIZE as usize, SIZE as usize));
        for (x, y, p) in resized.enumerate_pixels() {
            for c in 0..3 {
                arr[[0, c, y as usize, x as usize]] = (p[c] as f32 / 255.0 - 0.5) / 0.5;
            }
        }
        let hidden = {
            let mut enc = self.encoder.lock();
            let out = enc.run(ort::inputs!["pixel_values" => Tensor::from_array(arr)?])?;
            out["last_hidden_state"].try_extract_array::<f32>()?.to_owned()
        };
        let mut ids: Vec<i64> = vec![BOS];
        let mut dec = self.decoder.lock();
        for _ in 0..MAX_TOKENS {
            let input = Array2::<i64>::from_shape_vec((1, ids.len()), ids.clone()).unwrap();
            let out = dec.run(ort::inputs!["input_ids" => Tensor::from_array(input)?, "encoder_hidden_states" => Tensor::from_array(hidden.clone())?])?;
            let logits = out["logits"].try_extract_array::<f32>()?;
            let shape = logits.shape();
            let (steps, vocab) = (shape[1], shape[2]);
            let last: Vec<f32> = logits.iter().skip((steps - 1) * vocab).take(vocab).copied().collect();
            let next = last.iter().enumerate().fold((0usize, f32::NEG_INFINITY), |a, (i, &v)| if v > a.1 { (i, v) } else { a }).0 as i64;
            if next == EOS {
                break;
            }
            ids.push(next);
        }
        Ok(tidy_latex(&self.decode(&ids[1..])))
    }
}

/// Remove the token spacing of the model output ("\mathrm { F F N }" ->
/// "\mathrm{FFN}") while keeping spaces that separate a command from a letter.
pub fn tidy_latex(s: &str) -> String {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let mut out = String::with_capacity(s.len());
    for (i, t) in toks.iter().enumerate() {
        if i > 0 {
            let prev = toks[i - 1];
            let prev_is_cmd = prev.starts_with('\\') && prev.len() > 1 && prev[1..].chars().all(|c| c.is_ascii_alphabetic());
            if prev_is_cmd && t.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                out.push(' ');
            }
        }
        out.push_str(t);
    }
    out
}

/// Split a trailing equation number: "... \qquad (2)" -> ("...", Some("2")).
pub fn split_equation_number(latex: &str) -> (String, Option<String>) {
    // "\mathrm{(1)}" and "\text{(1)}" wrap the number in a text command.
    let unwrapped;
    let mut s = latex.trim_end();
    for cmd in ["\\mathrm{", "\\text{", "\\textrm{"] {
        if let Some(i) = s.rfind(cmd)
            && s.ends_with(")}")
            && s[i + cmd.len()..].starts_with('(')
        {
            unwrapped = format!("{}{}", &s[..i], &s[i + cmd.len()..s.len() - 1]);
            s = unwrapped.as_str();
            break;
        }
    }
    if let Some(open) = s.rfind('(')
        && s.ends_with(')')
    {
        let num = &s[open + 1..s.len() - 1];
        if !num.is_empty() && num.len() <= 6 && num.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') && num.chars().any(|c| c.is_ascii_digit()) {
            let mut body = s[..open].trim_end().to_string();
            // Drop the spacing that pushed the number to the margin.
            loop {
                let before = body.len();
                for sp in ["\\qquad", "\\quad", "\\space", "\\,", "\\;", "\\:", "~", "\\ "] {
                    if let Some(b) = body.strip_suffix(sp) {
                        body = b.trim_end().to_string();
                    }
                }
                if let Some(i) = body.rfind("\\hspace")
                    && body.ends_with('}')
                {
                    body = body[..i].trim_end().to_string();
                }
                if body.len() == before {
                    break;
                }
            }
            // Only a number preceded by spacing is an equation number.
            if body.len() < s[..open].trim_end().len() {
                return (body, Some(num.to_string()));
            }
        }
    }
    (latex.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidies_spacing() {
        assert_eq!(tidy_latex("\\mathrm { F F N } ( x ) = \\operatorname* { m a x } ( 0 , x W _ { 1 } )"), "\\mathrm{FFN}(x)=\\operatorname*{max}(0,xW_{1})");
        assert_eq!(tidy_latex("\\cdot x"), "\\cdot x");
    }

    #[test]
    fn splits_numbers() {
        let (b, n) = split_equation_number("W_{2}+b_{2}\\qquad\\qquad(2)");
        assert_eq!((b.as_str(), n.as_deref()), ("W_{2}+b_{2}", Some("2")));
        let (b, n) = split_equation_number("\\frac{a}{b})V\\hspace{2cm}(1)");
        assert_eq!((b.as_str(), n.as_deref()), ("\\frac{a}{b})V", Some("1")));
        assert_eq!(split_equation_number("f(x)").1, None);
        let (b, n) = split_equation_number("V\\space\\space\\quad\\quad\\mathrm{(1)}");
        assert_eq!((b.as_str(), n.as_deref()), ("V", Some("1")));
    }
}
