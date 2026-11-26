//! Appearance streams StrataPDF writes itself instead of MuPDF's: free text in
//! Gothic or Mincho (MuPDF always sets Japanese in Mincho), and free text and
//! image stamps kept upright on rotated pages.
//!
//! Each stream is written after MuPDF's own synthesis (which `update` runs after
//! any property is set), so MuPDF leaves it alone: it only resynthesises an
//! annotation whose properties are set again, and `annot::apply_spec` always
//! writes the stream again afterwards.
//!
//! Text uses fonts every PDF reader has without embedding: Helvetica or
//! Times-Roman for Latin runs, and the non-embedded Adobe-Japan1 CID fonts
//! "Gothic" or "Mincho" (UniJIS-UTF16-H) for the rest, as MuPDF does. The CID
//! fonts have no width table, so every character there advances one em (half
//! an em for half-width katakana, which get a width entry), and the layout
//! below measures the same way.

use std::sync::OnceLock;
use std::fmt::Write as _;

use mupdf::pdf::{PdfAnnotation, PdfDocument, PdfObject, PdfPage};
use mupdf::{Buffer, CjkFontOrdering, Font, SimpleFontEncoding, WriteMode};

use crate::annot::{AnnotSpec, TextFont};

/// Inner margin of a text box, in points.
pub const PAD: f32 = 2.0;
/// Line pitch as a multiple of the font size.
pub const LEADING: f32 = 1.25;
/// Baseline below the top of a line, as a multiple of the font size.
const ASCENT: f32 = 0.92;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    /// WinAnsi characters set in Helvetica or Times.
    Latin,
    /// Everything else, set in the Japanese CID font.
    Cjk,
}

/// One laid-out line: characters with their script and advance (points).
#[derive(Clone, Debug, Default)]
pub struct Line {
    pub chars: Vec<(char, Script, f32)>,
    pub width: f32,
}

impl Line {
    fn refresh(&mut self) {
        while self.chars.last().is_some_and(|c| c.0 == ' ') {
            self.chars.pop();
        }
        self.width = self.chars.iter().map(|c| c.2).sum();
    }
}

/// Advances (em) of the WinAnsi characters in Helvetica and Times-Roman, by
/// code. Measured once; MuPDF fonts must not outlive their thread's context.
fn latin_widths() -> &'static [[f32; 256]; 2] {
    static W: OnceLock<[[f32; 256]; 2]> = OnceLock::new();
    W.get_or_init(|| {
        let mut w = [[0.55f32; 256]; 2];
        for (i, name) in ["Helvetica", "Times-Roman"].into_iter().enumerate() {
            let Ok(f) = Font::new(name) else { continue };
            let chars = (0x20u32..=0x7E).chain(0xA0..=0xFF).filter_map(char::from_u32).chain("€‚ƒ„…†‡ˆ‰Š‹ŒŽ‘’“”•–—˜™š›œžŸ".chars());
            for c in chars {
                if let (Some(b), Ok(a)) = (win_ansi(c), f.encode_character(c as i32).and_then(|g| f.advance_glyph(g))) {
                    w[i][b as usize] = a;
                }
            }
        }
        w
    })
}

fn latin_advance(font: TextFont, c: char) -> f32 {
    let t = &latin_widths()[(font == TextFont::Serif) as usize];
    win_ansi(c).map(|b| t[b as usize]).unwrap_or(0.55)
}

/// The WinAnsiEncoding byte of a character.
pub fn win_ansi(c: char) -> Option<u8> {
    let u = c as u32;
    if (0x20..=0x7E).contains(&u) || (0xA0..=0xFF).contains(&u) {
        return Some(u as u8);
    }
    Some(match c {
        '€' => 0x80,
        '‚' => 0x82,
        'ƒ' => 0x83,
        '„' => 0x84,
        '…' => 0x85,
        '†' => 0x86,
        '‡' => 0x87,
        'ˆ' => 0x88,
        '‰' => 0x89,
        'Š' => 0x8A,
        '‹' => 0x8B,
        'Œ' => 0x8C,
        'Ž' => 0x8E,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '˜' => 0x98,
        '™' => 0x99,
        'š' => 0x9A,
        '›' => 0x9B,
        'œ' => 0x9C,
        'ž' => 0x9E,
        'Ÿ' => 0x9F,
        _ => return None,
    })
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2FFF | 0x3000..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

/// Which font sets a character. ASCII is always Latin (the CID fonts would give
/// it a full em); WinAnsi punctuation such as “ ” … — follows the text before it.
fn script(c: char, prev: Option<Script>) -> Script {
    if is_cjk(c) {
        return Script::Cjk;
    }
    if c.is_ascii() {
        return Script::Latin;
    }
    match win_ansi(c) {
        None => Script::Cjk,
        Some(_) if c.is_alphanumeric() => Script::Latin,
        Some(_) => prev.unwrap_or(Script::Latin),
    }
}

/// Advance in em.
fn advance(font: TextFont, c: char, s: Script) -> f32 {
    match s {
        Script::Latin => latin_advance(font, c),
        Script::Cjk if ('\u{FF61}'..='\u{FF9F}').contains(&c) => 0.5,
        Script::Cjk => 1.0,
    }
}

/// Characters that must not begin a line (kinsoku).
const NO_START: &str = "、。，．・：；？！ー－～〜…‥）」』】〕〉》］｝〙〗”’ぁぃぅぇぉっゃゅょゎゕゖァィゥェォッャュョヮヵヶㇰㇱㇲㇳㇴㇵㇶㇷㇸㇹㇺㇻㇼㇽㇾㇿ々ゝゞヽヾ,.;:!?)]}%";
/// Characters that must not end a line.
const NO_END: &str = "（「『【〔〈《［｛〘〖“‘([{";

fn can_break(prev: char, next: char) -> bool {
    if next == ' ' {
        return false;
    }
    if prev == ' ' {
        return true;
    }
    if NO_START.contains(next) || NO_END.contains(prev) {
        return false;
    }
    is_cjk(prev) || is_cjk(next)
}

/// Lay out text in lines no wider than `wrap` points: Latin breaks at spaces,
/// Japanese between any two characters except where kinsoku forbids it, and a
/// word longer than a line is cut. Every paragraph gives at least one line.
pub fn layout(text: &str, font: TextFont, size: f32, wrap: f32) -> Vec<Line> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = Line::default();
        let mut prev = None;
        for c in para.trim_end_matches('\r').chars() {
            let c = if c == '\t' { ' ' } else { c };
            if c.is_control() {
                continue;
            }
            let s = script(c, prev);
            prev = Some(s);
            let w = advance(font, c, s) * size;
            if line.width + w > wrap && !line.chars.is_empty() && c != ' ' {
                let n = line.chars.len();
                let at = if can_break(line.chars[n - 1].0, c) {
                    n
                } else {
                    (1..n).rev().find(|&k| can_break(line.chars[k - 1].0, line.chars[k].0)).unwrap_or(n)
                };
                let rest = line.chars.split_off(at);
                line.refresh();
                out.push(std::mem::take(&mut line));
                line.chars = rest.into_iter().skip_while(|c| c.0 == ' ').collect();
                line.width = line.chars.iter().map(|c| c.2).sum();
            }
            line.chars.push((c, s, w));
            line.width += w;
        }
        line.refresh();
        out.push(line);
    }
    out
}

/// Size of laid-out text with its padding, in points.
pub fn text_size(lines: &[Line], size: f32) -> (f32, f32) {
    let w = lines.iter().map(|l| l.width).fold(0.0, f32::max);
    (w + 2.0 * PAD, lines.len().max(1) as f32 * size * LEADING + 2.0 * PAD)
}

/// A page's size and the direction of its upright space in PDF user space.
#[derive(Clone, Copy, Debug)]
pub struct PageGeom {
    pub width: f32,
    pub height: f32,
    /// Linear part of the map from upright space (y up, as the page is shown)
    /// to user space: PDF matrix order a b c d.
    pub rot: [f32; 4],
}

impl PageGeom {
    pub fn of(page: &PdfPage) -> Result<PageGeom, mupdf::Error> {
        let b = page.bounds()?;
        let m = page.object().page_ctm()?;
        // Row vectors, as in PDF: user · L = shown (y down). Upright → shown is
        // F = diag(1, −1), so upright → user is F · L⁻¹.
        let det = m.a * m.d - m.b * m.c;
        let inv = if det.abs() > 1e-6 { [m.d / det, -m.b / det, -m.c / det, m.a / det] } else { [1.0, 0.0, 0.0, -1.0] };
        let r = [inv[0], inv[1], -inv[2], -inv[3]];
        // Only the direction matters: the form's box is mapped onto the annotation rectangle.
        let s = (r[0] * r[3] - r[1] * r[2]).abs().sqrt().max(1e-6);
        let rot = r.map(|v| ((v / s) * 1e4).round() / 1e4 + 0.0);
        Ok(PageGeom { width: b.width(), height: b.height(), rot })
    }
}

/// A number for a content stream.
fn num(v: f32) -> String {
    let s = format!("{:.3}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" || s.is_empty() { "0".into() } else { s.into() }
}

fn gstate(opacity: f32) -> (String, &'static str) {
    if opacity < 1.0 {
        let a = num(opacity.clamp(0.0, 1.0));
        (format!("/ExtGState << /GS0 << /Type /ExtGState /CA {a} /ca {a} >> >>"), "/GS0 gs\n")
    } else {
        (String::new(), "")
    }
}

/// The Japanese CID font of a style, with widths for half-width katakana.
fn cjk_font(pdf: &mut PdfDocument, font: TextFont) -> Result<PdfObject, mupdf::Error> {
    // MuPDF caches CJK font resources by script and key font but not by style,
    // so each style is asked for with a key font of its own. MuPDF's own free
    // text synthesis keys Mincho with the Latin font, Times-Roman for /TiRo.
    let (key, serif) = match font {
        TextFont::Sans => (Font::new_cjk(CjkFontOrdering::AdobeJapan)?, false),
        TextFont::Serif => (Font::new("Times-Roman")?, true),
    };
    let f = pdf.add_cjk_font(&key, CjkFontOrdering::AdobeJapan, WriteMode::Horizontal, serif)?;
    if let Some(desc) = f.get_dict("DescendantFonts")?
        && let Some(mut d) = desc.get_array(0)?
        && d.get_dict("W")?.is_none()
    {
        d.dict_put("W", pdf.new_object_from_str("[327 389 500]")?)?;
    }
    Ok(f)
}

/// Escape bytes for a PDF literal string.
fn literal(bytes: &[u8], out: &mut String) {
    out.push('(');
    for &b in bytes {
        match b {
            b'(' | b')' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7E => out.push(b as char),
            _ => {
                let _ = write!(out, "\\{:03o}", b);
            }
        }
    }
    out.push(')');
}

fn hex_utf16(chars: &[char], out: &mut String) {
    out.push('<');
    for c in chars {
        let mut buf = [0u16; 2];
        for u in c.encode_utf16(&mut buf) {
            let _ = write!(out, "{:04X}", u);
        }
    }
    out.push('>');
}

/// Point the annotation's normal appearance at a new form XObject.
fn set_appearance(pdf: &mut PdfDocument, a: &PdfAnnotation, bbox: [f32; 2], rot: [f32; 4], resources: &str, content: &str) -> Result<(), mupdf::Error> {
    let [a0, b0, c0, d0] = rot.map(num);
    let dict = pdf.new_object_from_str(&format!(
        "<< /Type /XObject /Subtype /Form /BBox [0 0 {} {}] /Matrix [{a0} {b0} {c0} {d0} 0 0] /Resources << {resources} >> >>",
        num(bbox[0]),
        num(bbox[1])
    ))?;
    let stream = pdf.add_stream(&Buffer::from_bytes(content.as_bytes())?, Some(&dict), false)?;
    let mut ap = pdf.new_dict()?;
    ap.dict_put("N", stream)?;
    a.object().dict_put("AP", ap)?;
    Ok(())
}

/// Write the appearance of a free text box of the given size (points, as shown).
pub(crate) fn write_free_text(pdf: &mut PdfDocument, a: &PdfAnnotation, s: &AnnotSpec, lines: &[Line], w: f32, h: f32, g: &PageGeom) -> Result<(), mupdf::Error> {
    let size = s.font_size;
    let (gs_res, gs_op) = gstate(s.opacity);
    let mut cs = String::new();
    cs.push_str("q\n");
    cs.push_str(gs_op);
    if let Some(f) = s.fill {
        let _ = writeln!(cs, "{} {} {} rg 0 0 {} {} re f", num(f[0]), num(f[1]), num(f[2]), num(w), num(h));
    }
    let _ = writeln!(cs, "BT\n{} {} {} rg", num(s.color[0]), num(s.color[1]), num(s.color[2]));
    let (mut latin_used, mut cjk_used) = (false, false);
    for (i, line) in lines.iter().enumerate() {
        if line.chars.is_empty() {
            continue;
        }
        let y = h - PAD - i as f32 * size * LEADING - size * ASCENT;
        let _ = writeln!(cs, "1 0 0 1 {} {} Tm", num(PAD), num(y));
        let mut k = 0;
        while k < line.chars.len() {
            let sc = line.chars[k].1;
            let end = (k..line.chars.len()).find(|&j| line.chars[j].1 != sc).unwrap_or(line.chars.len());
            let run: Vec<char> = line.chars[k..end].iter().map(|c| c.0).collect();
            match sc {
                Script::Latin => {
                    latin_used = true;
                    let _ = write!(cs, "/FL {} Tf ", num(size));
                    let bytes: Vec<u8> = run.iter().map(|&c| win_ansi(c).unwrap_or(b'?')).collect();
                    literal(&bytes, &mut cs);
                }
                Script::Cjk => {
                    cjk_used = true;
                    let _ = write!(cs, "/FC {} Tf ", num(size));
                    hex_utf16(&run, &mut cs);
                }
            }
            cs.push_str(" Tj\n");
            k = end;
        }
    }
    cs.push_str("ET\nQ\n");
    let mut fonts = String::new();
    if latin_used {
        let name = if s.font == TextFont::Serif { "Times-Roman" } else { "Helvetica" };
        let f = pdf.add_simple_font(&Font::new(name)?, SimpleFontEncoding::Latin)?;
        let _ = write!(fonts, "/FL {} 0 R ", f.as_indirect()?);
    }
    if cjk_used {
        let f = cjk_font(pdf, s.font)?;
        let _ = write!(fonts, "/FC {} 0 R ", f.as_indirect()?);
    }
    let res = format!("/Font << {fonts}>> {gs_res}");
    set_appearance(pdf, a, [w, h], g.rot, &res, &cs)
}

/// Write the appearance of an image stamp: the image filling the box, upright.
pub(crate) fn write_stamp(pdf: &mut PdfDocument, a: &PdfAnnotation, s: &AnnotSpec, g: &PageGeom) -> Result<(), mupdf::Error> {
    let (gs_res, gs_op) = gstate(s.opacity);
    let content = format!("q\n{gs_op}/I Do\nQ\n");
    // A stamp whose appearance has a single image XObject is an image stamp to MuPDF too.
    let res = format!("/XObject << /I {} 0 R >> {gs_res}", s.image);
    set_appearance(pdf, a, [1.0, 1.0], g.rot, &res, &content)
}

/// Object number of the image a stamp shows, if its appearance is just an image.
pub(crate) fn stamp_image(a: &PdfAnnotation) -> Option<i32> {
    let xo = a.object().get_dict("AP").ok()??.get_dict("N").ok()??.get_dict("Resources").ok()??.get_dict("XObject").ok()??;
    if xo.dict_len().ok()? != 1 {
        return None;
    }
    let img = xo.get_dict_val(0).ok()??;
    let num = img.as_indirect().ok().filter(|&n| n > 0)?;
    let is_image = img.get_dict("Subtype").ok()?.and_then(|s| s.as_name().ok()).is_some_and(|n| n == b"Image");
    is_image.then_some(num)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.chars.iter().map(|c| c.0).collect()).collect()
    }

    #[test]
    fn latin_wraps_at_spaces() {
        let l = layout("The quick brown fox jumps", TextFont::Sans, 10.0, 60.0);
        assert!(l.len() > 1);
        for line in texts(&l) {
            assert!(!line.starts_with(' ') && !line.ends_with(' '), "{line:?}");
        }
        assert_eq!(texts(&l).join(" "), "The quick brown fox jumps");
    }

    #[test]
    fn japanese_breaks_anywhere_but_kinsoku() {
        // 10 pt per character, 5 characters per line: "。" may not start a line.
        let l = layout("あいうえお。かきくけこ", TextFont::Sans, 10.0, 50.0);
        let t = texts(&l);
        assert!(t.iter().all(|s| !s.starts_with('。')), "{t:?}");
        assert_eq!(t.concat(), "あいうえお。かきくけこ");
        assert!((l[0].width - 40.0).abs() < 0.01, "{}", l[0].width);
    }

    #[test]
    fn paragraphs_and_widths() {
        let l = layout("ab\n\nｱｲ", TextFont::Sans, 10.0, 1000.0);
        assert_eq!(l.len(), 3);
        assert!(l[1].chars.is_empty());
        assert!((l[2].width - 10.0).abs() < 0.01);
        assert_eq!(l[0].chars[0].1, Script::Latin);
        assert_eq!(l[2].chars[0].1, Script::Cjk);
        // Quotes follow the text around them.
        let q = layout("「“あ”」 “a”", TextFont::Sans, 10.0, 1000.0);
        let s: Vec<Script> = q[0].chars.iter().map(|c| c.1).collect();
        assert_eq!(s[1], Script::Cjk);
        assert_eq!(*s.last().unwrap(), Script::Latin);
    }

    #[test]
    fn long_word_is_cut() {
        let l = layout("abcdefghijklmnopqrstuvwxyz", TextFont::Sans, 10.0, 40.0);
        assert!(l.len() > 2);
        assert!(l.iter().all(|x| x.width <= 40.0 + 0.01));
    }
}
