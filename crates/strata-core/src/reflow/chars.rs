//! Character clean-up of one line before it becomes text: invisible and
//! exotic spaces, spaces MuPDF invents or misses, doubled glyphs from broken
//! ToUnicode maps, and Symbol-font private-use code points.

use crate::glyphs::symbol_char;
use crate::rich::RichChar;
use crate::text::FontInfo;

fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}')
}

fn is_exotic_space(c: char) -> bool {
    matches!(c, '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

/// A line set letter-spaced ("a r t i c l e  i n f o", "S U M M A R Y"), whose
/// letters come with a space between each: the spaces stay only where the gap
/// between the letters is clearly wider than between the others (the word
/// spaces).
fn unspace_letters(chars: &[RichChar]) -> Option<Vec<RichChar>> {
    let letters: Vec<&RichChar> = chars.iter().filter(|c| !c.c.is_whitespace()).collect();
    // Runs of non-space characters between spaces: nearly all single letters.
    let mut runs: Vec<usize> = Vec::new();
    let mut n = 0;
    for c in chars {
        if c.c.is_whitespace() {
            if n > 0 {
                runs.push(n);
            }
            n = 0;
        } else {
            n += 1;
        }
    }
    if n > 0 {
        runs.push(n);
    }
    let singles = runs.iter().filter(|&&r| r == 1).count();
    if letters.len() < 4 || runs.len() < 4 || singles * 10 < runs.len() * 8 || !letters.iter().all(|c| c.c.is_alphabetic() || c.c == '-') {
        return None;
    }
    let gaps: Vec<f32> = letters.windows(2).map(|w| w[1].bbox.x0 - w[0].bbox.x1).collect();
    let mut sorted = gaps.clone();
    sorted.sort_by(f32::total_cmp);
    let median = sorted[sorted.len() / 2];
    let em = letters.iter().map(|c| c.size).fold(0.0f32, f32::max);
    let word = |g: f32| g > median * 1.6 && g > median + em * 0.12;
    let mut out: Vec<RichChar> = Vec::with_capacity(letters.len() + 4);
    for (k, c) in letters.iter().enumerate() {
        if k > 0 && word(gaps[k - 1]) {
            let mut sp = **c;
            sp.c = ' ';
            out.push(sp);
        }
        out.push(**c);
    }
    Some(out)
}

/// Clean the characters of one line (horizontal text).
pub(super) fn clean_line(chars: &[RichChar], fonts: &[FontInfo], scan: bool, math_font: impl Fn(&str) -> bool) -> Vec<RichChar> {
    let unspaced = unspace_letters(chars);
    // (A letter-spaced line keeps its wide letter gaps: no spaces are added back.)
    let letter_spaced = unspaced.is_some();
    let chars: &[RichChar] = unspaced.as_deref().unwrap_or(chars);
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
    let letter_gap = (!scan && !letter_spaced && proportional && glyphs.len() >= 20 && spaces * 15 < glyphs.len())
        .then(|| {
            let mut gaps: Vec<f32> = glyphs.windows(2).filter(|w| (w[0].size - w[1].size).abs() < 0.1).map(|w| w[1].bbox.x0 - w[0].bbox.x1).collect();
            gaps.sort_by(f32::total_cmp);
            gaps.get(gaps.len() / 2).copied()
        })
        .flatten();
    // The gap between letters set next to each other: a space whose gap is no
    // wider is one MuPDF invented inside tracked capitals ("SU MMARY").
    let tight_gap = {
        let mut g: Vec<f32> = chars.windows(2).filter(|w| w[0].c.is_alphabetic() && w[1].c.is_alphabetic()).map(|w| w[1].bbox.x0 - w[0].bbox.x1).collect();
        g.sort_by(f32::total_cmp);
        (g.len() >= 3).then(|| g[g.len() / 2])
    };
    let mut out: Vec<RichChar> = Vec::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        let mut c = *c;
        if ruby(&c) {
            continue;
        }
        if c.c == ' '
            && let (Some(tg), Some(a), Some(b)) = (tight_gap, i.checked_sub(1).and_then(|k| chars.get(k)), chars.get(i + 1))
            && a.c.is_alphabetic()
            && b.c.is_alphabetic()
            && b.bbox.x0 - a.bbox.x1 < tg + a.size.max(b.size) * 0.05
            && tg > a.size * 0.05
        {
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
            // The overhang of an italic letter before a hyphen ("T -axes", "P -wave"),
            // read as a space although it is far too narrow for one.
            if let (Some(a), Some(b)) = (out.last(), chars.get(i + 1))
                && a.c.is_alphabetic()
                && matches!(b.c, '-' | '\u{2010}' | '\u{2011}')
                && b.bbox.x0 - a.bbox.x1 < a.size.max(b.size) * 0.2
            {
                continue;
            }
            // A space inside a word after a ligature that MuPDF split into letters
            // ("Th e", "fi ssure"): the letters after the first have no width, and the
            // next glyph starts where the ligature glyph ends. (A word ending in a
            // ligature has a real gap before the next word.)
            if let (Some(a), Some(b)) = (out.last(), chars[i + 1..].iter().find(|x| !x.c.is_whitespace()))
                && a.bbox.width() < 0.01
                && !a.c.is_whitespace()
                && a.c.is_alphabetic()
                && b.c.is_alphabetic()
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
            && !letter_spaced
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
/// Drop spaces between Japanese characters, which OCR engines put after
/// punctuation and around brackets ("よく、 都下", "」 『").
pub(super) fn drop_cjk_spaces(chars: Vec<RichChar>) -> Vec<RichChar> {
    if !chars.iter().any(|c| c.c == ' ') {
        return chars;
    }
    let glyph = |k: usize| chars.get(k).map(|c| c.c).filter(|c| !c.is_whitespace());
    let mut out = Vec::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        // (An ideographic space is typeset: "第一章　題名".)
        if c.c == ' ' {
            let before = (0..i).rev().find_map(glyph);
            let after = (i + 1..chars.len()).find_map(glyph);
            if before.is_some_and(super::is_cjk) && after.is_some_and(super::is_cjk) {
                continue;
            }
        }
        out.push(*c);
    }
    out
}

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

    fn glyph(c: char, x0: f32, x1: f32) -> RichChar {
        RichChar { c, bbox: crate::geom::RectF { x0, y0: 0.0, x1, y1: 10.0 }, size: 10.0, font: 0, bold: false, argb: 0 }
    }

    fn text(chars: &[RichChar]) -> String {
        clean_line(chars, &[], false, |_| false).iter().map(|c| c.c).collect()
    }

    #[test]
    fn no_space_inside_a_word_after_a_split_ligature() {
        // "Th" is one glyph that MuPDF split into "T" and an "h" without width, and
        // it added a space inside the glyph.
        let inside = [glyph('T', 0.0, 10.7), glyph('h', 10.7, 10.7), glyph(' ', 5.3, 7.6), glyph('e', 10.6, 15.0)];
        assert_eq!(text(&inside), "The");
        // A word that ends in a ligature has a real gap before the next word.
        let word_end = [glyph('f', 0.0, 3.0), glyph('f', 3.0, 3.0), glyph(' ', 3.0, 5.3), glyph('a', 5.5, 10.0)];
        assert_eq!(text(&word_end), "ff a");
    }
}
