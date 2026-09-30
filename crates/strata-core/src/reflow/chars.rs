//! Character clean-up of one line before it becomes text: invisible and
//! exotic spaces, spaces MuPDF invents or misses, doubled glyphs from broken
//! ToUnicode maps, and Symbol-font private-use code points.

use crate::rich::RichChar;
use crate::text::FontInfo;

/// Adobe Symbol encoding (codes 0x20-0xFF) for the code points MuPDF gives as
/// U+F0xx when a Symbol font has no usable ToUnicode. 0 = keep the code point.
fn symbol_char(code: u8) -> Option<char> {
    const LOW: &str = " !∀#∃%&∋()∗+,−./0123456789:;<=>?≅ΑΒΧΔΕΦΓΗΙϑΚΛΜΝΟΠΘΡΣΤΥςΩΞΨΖ[∴]⊥_‾αβχδεφγηιϕκλμνοπθρστυϖωξψζ{|}∼";
    const HIGH: &str = "€ϒ′≤⁄∞ƒ♣♦♥♠↔←↑→↓°±″≥×∝∂•÷≠≡≈…⏐⎯↵ℵℑℜ℘⊗⊕∅∩∪⊃⊇⊄⊂⊆∈∉∠∇®©™∏√⋅¬∧∨⇔⇐⇑⇒⇓◊〈®©™∑";
    match code {
        0x20..=0x7E => LOW.chars().nth((code - 0x20) as usize),
        0xA0..=0xE5 => HIGH.chars().nth((code - 0xA0) as usize),
        0xF1 => Some('〉'),
        0xF2 => Some('∫'),
        _ => None,
    }
}

fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}')
}

fn is_exotic_space(c: char) -> bool {
    matches!(c, '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

/// Clean the characters of one line (horizontal text).
pub(super) fn clean_line(chars: &[RichChar], fonts: &[FontInfo], scan: bool, math_font: impl Fn(&str) -> bool) -> Vec<RichChar> {
    let font_name = |c: &RichChar| fonts.get(c.font as usize).map(|f| f.name.as_str()).unwrap_or("");
    // Ruby (furigana): kana in small type above the base text of the line.
    let mut sizes: Vec<f32> = chars.iter().filter(|c| !c.c.is_whitespace()).map(|c| c.size).collect();
    sizes.sort_by(f32::total_cmp);
    let med = sizes.get(sizes.len() / 2).copied().unwrap_or(0.0);
    let base_mid = {
        let mut mids: Vec<f32> = chars.iter().filter(|c| c.size >= med * 0.9 && !c.c.is_whitespace()).map(|c| (c.bbox.y0 + c.bbox.y1) / 2.0).collect();
        mids.sort_by(f32::total_cmp);
        mids.get(mids.len() / 2).copied().unwrap_or(0.0)
    };
    let ruby = |c: &RichChar| matches!(c.c as u32, 0x3040..=0x30FF) && c.size < med * 0.7 && (c.bbox.y0 + c.bbox.y1) / 2.0 < base_mid - med * 0.3;
    // Text set without space glyphs, words apart by a small gap only: the letter
    // spacing is the median gap between glyphs, and a gap clearly wider is a
    // word space ("significantlyto", "arc,Japan,and").
    let glyphs: Vec<&RichChar> = chars.iter().filter(|c| !c.c.is_whitespace()).collect();
    let spaces = chars.len() - glyphs.len();
    // (Only with glyph boxes of the glyphs' own widths: Latin set in a CJK font
    // has em-square boxes that overlap, and their gaps say nothing.)
    let proportional = {
        let mut widths: Vec<f32> = glyphs.iter().filter(|c| c.c.is_ascii_alphabetic()).map(|c| c.bbox.width() / c.size.max(0.1)).collect();
        widths.sort_by(f32::total_cmp);
        widths.get(widths.len() / 2).is_some_and(|&w| w < 0.75)
    };
    let letter_gap = (!scan && proportional && glyphs.len() >= 20 && spaces * 15 < glyphs.len())
        .then(|| {
            let mut gaps: Vec<f32> = glyphs.windows(2).filter(|w| (w[0].size - w[1].size).abs() < 0.1).map(|w| w[1].bbox.x0 - w[0].bbox.x1).collect();
            gaps.sort_by(f32::total_cmp);
            gaps.get(gaps.len() / 2).copied()
        })
        .flatten();
    let mut out: Vec<RichChar> = Vec::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        let mut c = *c;
        if ruby(&c) {
            continue;
        }
        if is_zero_width(c.c) {
            continue;
        }
        // U+3000 (ideographic space) stays: it indents Japanese paragraphs.
        if is_exotic_space(c.c) && c.c != '\u{3000}' {
            c.c = ' ';
        }
        // Symbol fonts without a usable ToUnicode map: U+F020..U+F0FF.
        if ('\u{F020}'..='\u{F0FF}').contains(&c.c)
            && font_name(&c).to_ascii_lowercase().contains("symbol")
            && let Some(m) = symbol_char((c.c as u32 - 0xF000) as u8)
        {
            c.c = m;
        }
        // A math font whose ToUnicode map gives two code points for one glyph: the
        // second copy has no width. (Text fonts expand ligatures the same way,
        // "ff" into "f" and a zero-width "f": those letters are real.)
        if let Some(prev) = out.last()
            && prev.c == c.c
            && prev.font == c.font
            && c.bbox.width() < 0.01
            && !c.c.is_whitespace()
            && math_font(font_name(&c))
        {
            continue;
        }
        if c.c == ' ' {
            // A space MuPDF adds at a jump in position although the glyphs touch
            // ("( Bryan", "2008 ;"): real word spaces leave a gap.
            let invented = font_name(&c).contains("dummy");
            if invented
                && let (Some(a), Some(b)) = (out.last(), chars.get(i + 1))
                && b.bbox.x0 - a.bbox.x1 < a.size.max(b.size) * 0.08
            {
                continue;
            }
        } else if let (Some(lg), Some(a)) = (letter_gap, out.last())
            && a.c != ' '
            && (a.c.is_alphanumeric() || matches!(a.c, ',' | '.' | ';' | ':' | ')'))
            && (c.c.is_alphanumeric() || c.c == '(')
            && !(a.c.is_ascii_digit() && matches!(c.c, '0'..='9'))
            && c.bbox.x0 - a.bbox.x1 > lg + c.size.max(a.size) * 0.08
            && !math_font(font_name(&c))
        {
            let mut sp = c;
            sp.c = ' ';
            sp.bbox.x0 = a.bbox.x1;
            sp.bbox.x1 = c.bbox.x0;
            out.push(sp);
        } else if !scan
            && let Some(a) = out.last()
            && a.c.is_lowercase()
            && c.c.is_lowercase()
            && a.font == c.font
            && (a.size - c.size).abs() < 0.1
            && c.bbox.x0 - a.bbox.x1 >= c.size * 0.13
            && !math_font(font_name(&c))
        {
            // A word space without a space glyph in tightly justified text.
            let mut sp = c;
            sp.c = ' ';
            sp.bbox.x0 = a.bbox.x1;
            sp.bbox.x1 = c.bbox.x0;
            out.push(sp);
        }
        out.push(c);
    }
    out
}

/// Only undecodable or private-use characters: glyphs of a badge or icon font.
pub(super) fn is_junk(t: &str) -> bool {
    let mut any = false;
    for c in t.chars().filter(|c| !c.is_whitespace()) {
        any = true;
        let u = c as u32;
        if !(c == '\u{FFFD}' || (0xE000..=0xF8FF).contains(&u) || u < 0x20) {
            return false;
        }
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol() {
        assert_eq!(symbol_char(0x62), Some('β'));
        assert_eq!(symbol_char(0xB0), Some('°'));
        assert_eq!(symbol_char(0xB1), Some('±'));
        assert_eq!(symbol_char(0x7E), Some('∼'));
        assert_eq!(symbol_char(0xE5), Some('∑'));
        assert!(is_junk("\u{FFFD} \u{FFFD}"));
        assert!(!is_junk("a\u{FFFD}"));
    }
}
