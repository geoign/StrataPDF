//! Text for glyphs that have no Unicode value in the PDF.
//!
//! Symbol and pi fonts of the Springer/Wiley "Advent" system, the Elsevier
//! "Universal"/"Mathematical Pi" fonts and TeX fonts are embedded as subsets
//! whose codes are private. MuPDF finds no character for them, but the glyph
//! *names* are stable across papers: `C0` of `AdvP4C4E74` is a minus sign in every
//! paper that has the glyph, while the code differs from paper to paper.
//!
//! The tables are built from evidence, not guessed: every row was checked by
//! rendering the glyph (up to five of the papers that contain it) in a corpus of 4,675 papers.
//! The comments give the number of corpus papers (first 40 pages) that contain each
//! glyph; a row that stands for a single paper says so. Glyphs that are not listed
//! stay undecodable.

use std::ffi::CString;

use mupdf_sys::fz_unicode_from_glyph_name_strict;

/// Adobe Symbol encoding (codes 0x20-0xFF) for the code points MuPDF gives as
/// U+F0xx when a Symbol font has no usable ToUnicode. 0 = keep the code point.
pub(crate) fn symbol_char(code: u8) -> Option<char> {
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

/// The text of the glyph `glyph` of the font `font` (as MuPDF names it, with
/// the subset prefix), or `None` if nothing is known about it. One glyph may
/// stand for several characters (ligatures).
pub fn glyph_text(font: &str, glyph: &str) -> Option<String> {
    let fam = family(font);
    if let Some(c) = symbol_layout(&fam, glyph) {
        return Some(c.to_string());
    }
    layout_text(&fam, glyph).map(str::to_string).or_else(|| named_text(glyph))
}

/// Normalised font name: lower case, without the subset prefix `ABCDEF+` and
/// without the random word some producers put in front of an Advent name
/// (`XxkbtbAdvP4C4E46`).
fn family(font: &str) -> String {
    let mut s = font;
    if s.len() > 7 && s.as_bytes()[6] == b'+' && s[..6].bytes().all(|b| b.is_ascii_uppercase()) {
        s = &s[7..];
    }
    if let Some(i) = s.rfind("Adv")
        && i > 0
        && s[..i].bytes().all(|b| b.is_ascii_alphabetic())
    {
        s = &s[i..];
    }
    s.to_ascii_lowercase()
}

/// `name` followed by digits only (`tex_cm_maths_symbols16`).
fn numbered(family: &str, name: &str) -> bool {
    family.strip_prefix(name).is_some_and(|r| r.bytes().all(|b| b.is_ascii_digit()))
}

/// The number of a glyph named `C<n>` (Advent; `C40._` is a variant of `C40`) or `.<nnnn>`.
fn code_of(glyph: &str) -> Option<u32> {
    let glyph = glyph.strip_suffix("._").unwrap_or(glyph);
    let digits = glyph.strip_prefix('C').or_else(|| glyph.strip_prefix('.'))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A character kept as `&'static str` so that ligatures fit.
type Text = Option<&'static str>;

fn layout_text(family: &str, glyph: &str) -> Text {
    let f = family;
    // Advent/TeX pi fonts: the glyph is `C<n>`, n = the code in a fixed layout.
    if matches!(f, "advp4c4e74" | "advmacmthsyn" | "advmt_sy" | "texcmmathssymbols" | "advmathsymb" | "advp49c2a1") || numbered(f, "tex_cm_maths_symbols") {
        return math_symbols(code_of(glyph)?);
    }
    if matches!(f, "advp4c4e46" | "texcmmathsextension") || numbered(f, "tex_cm_maths_extension") {
        return math_extension(code_of(glyph)?);
    }
    if matches!(f, "advp4c4e51" | "tex_cm_maths_italic" | "advmt_mi") {
        return math_italic(code_of(glyph)?);
    }
    if matches!(f, "advp4c4e59" | "tex_cm_roman") {
        return math_roman(code_of(glyph)?);
    }
    if let Some(t) = advent_pi(f, glyph) {
        return Some(t);
    }
    if let Some(t) = h_names(f, glyph) {
        return Some(t);
    }
    if let Some(t) = mathpack(f, glyph) {
        return Some(t);
    }
    advent_text(f, code_of(glyph)?)
}

/// Layout of the TeX math symbol font (cmsy10) that the Advent and TeX-derived symbol fonts
/// follow, with the brackets, plus and equals sign at 133-138. Papers: `C0` 438, `C24` 209, `C2` 203,
/// `C138` 118, `C1` `C3` `C14` 93-94, `C21` 60, `C20` 53, `C15` 51, `C6` 44, `C25` 41, `C136` 38,
/// `C133` `C134` 30, `C28` 23, `C135` 21, `C29` 14, `C137` 12, `C17` 9, `C48` 5, `C33` 2, `C41` 1.
fn math_symbols(code: u32) -> Text {
    Some(match code {
        0 => "−",
        1 => "·",
        2 => "×",
        3 => "∗",
        6 => "±",
        14 => "°",
        15 => "•",
        17 => "≡",
        20 => "≤",
        21 => "≥",
        24 => "∼",
        25 => "≈",
        28 => "≪",
        29 => "≫",
        33 => "→",
        41 => "⇒",
        48 => "′",
        133 => "(",
        134 => ")",
        135 => "+",
        136 => "=",
        137 => "[",
        138 => "]",
        _ => return None,
    })
}

/// Layout of the TeX math extension font (cmex10): delimiters in the sizes that are single
/// glyphs. Papers: `C18` `C19` 157, `C0` `C1` 135, `C16` `C17` 116, `C20` `C21` 67, `C2` `C3` 36,
/// `C26` 22, `C27` 16, `C12` 16, `C14` 11, `C8` 7, `C9` 5, `C10` `C11` 3, `C28` `C29` 2.
fn math_extension(code: u32) -> Text {
    Some(match code {
        0 | 16 | 18 => "(",
        1 | 17 | 19 => ")",
        2 | 20 => "[",
        3 | 21 => "]",
        8 | 26 => "{",
        9 | 27 => "}",
        10 | 28 => "⟨",
        11 | 29 => "⟩",
        12 => "|",
        14 => "/",
        _ => return None,
    })
}

/// Layout of the TeX math italic font (cmmi10): Greek letters and punctuation. Papers: `C27` 13,
/// `C26` 11, `C22` 9, `C14` 8, `C25` `C61` 7, `C15` `C18` `C28` `C30` `C58` 5, `C11` 3, `C12` `C13` `C17`
/// `C20` `C21` `C59` `C60` `C62` 2 to 4.
fn math_italic(code: u32) -> Text {
    Some(match code {
        11 => "α",
        12 => "β",
        13 => "γ",
        14 => "δ",
        15 => "ϵ",
        17 => "η",
        18 => "θ",
        20 => "κ",
        21 => "λ",
        22 => "μ",
        25 => "π",
        26 => "ρ",
        27 => "σ",
        28 => "τ",
        30 => "φ",
        58 => ".",
        59 => ",",
        60 => "<",
        61 => "/",
        62 => ">",
        _ => return None,
    })
}

/// Layout of the TeX text font (cmr10, OT1): capital Greek letters and the
/// accents, which are glyphs of their own and are attached to the letter they are drawn over by
/// the reflow. Papers: `C19` 72, `C22` 45, `C18` 24, `C1` 9, `C20` 7, `C21` 5, `C10` 3, `C9` `C23` `C25` 2;
/// `C0` `C3` `C5` `C8` one paper each.
fn math_roman(code: u32) -> Text {
    Some(match code {
        0 => "Γ",
        1 => "Δ",
        3 => "Λ",
        5 => "Π",
        8 => "Φ",
        9 => "Ψ",
        10 => "Ω",
        18 => "`",
        19 => "´",
        20 => "ˇ",
        21 => "˘",
        22 => "¯",
        23 => "˚",
        25 => "ß",
        _ => return None,
    })
}

/// Advent fonts whose glyph `C<n>` is code n of Adobe Symbol: the 8 codes seen in `AdvPSSym`
/// (`C211` 79 papers, `C176` 52, `C210` 5, `C162` `C180` 2, `C68` `C179` `C226` 1), `C176` (degree) in
/// `AdvTTab7e17fd` (71) and `Advsymbol` (18), and the Greek letters of `AdvGreekM` (13 letters, 1-5 papers each).
fn symbol_layout(family: &str, glyph: &str) -> Option<char> {
    let code = u8::try_from(code_of(glyph)?).ok()?;
    match family {
        "advpssym" | "advsymbol" | "advttab7e17fd" => symbol_char(code),
        "advgreekm" => symbol_char(code).filter(|c| ('α'..='ω').contains(c)),
        _ => None,
    }
}

/// Other Advent pi fonts with a small private layout.
fn advent_pi(family: &str, glyph: &str) -> Text {
    let code = code_of(glyph)?;
    Some(match (family, code) {
        // `AdvPi2`: Greek letters at their Symbol codes (2-3 papers each) and (c) (2).
        ("advpi2", 101) => "ε",
        ("advpi2", 102) => "φ",
        ("advpi2", 109) => "μ",
        ("advpi2", 112) => "π",
        ("advpi2", 114) => "ρ",
        ("advpi2", 115) => "σ",
        ("advpi2", 42) => "©",
        // `AdvPS7DA6` (`C44` `C46` 9-10 papers, `C49` `C50` `C56` 8, `C35` `C51` 2, `C36` 1) and `AdvPS7DB7` (`C44` 9,
        // `C60` 2, `C59` 1): relations at the codes of ASCII characters.
        ("advps7da6", 35) => "≤",
        ("advps7da6", 36) => "≥",
        ("advps7da6", 44) => "<",
        ("advps7da6", 46) => ">",
        ("advps7da6", 49) => "+",
        ("advps7da6", 50) => "−",
        ("advps7da6", 51) => "×",
        ("advps7da6", 56) => "°",
        ("advps7db7", 44) => "∼",
        ("advps7db7", 59) => "≡",
        ("advps7db7", 60) => "≈",
        ("advps586d", 113) => "©", // 8 papers
        // Raised minus signs are minus signs: the superscript is the position. 2-5 papers each.
        ("advps586b", 56) => "°",
        ("advps586b", 57) => "′",
        ("advps697c", 45) => "−",
        ("advps697c", 180) => "×",
        ("advps7cfd", 49) => "∞",
        ("advps7cfd", 163) => "×",
        ("advps7cfd", 178) => "•",
        ("advpi1", 46) => "•",
        ("advpi1", 55) => "−",
        ("advpi1", 56) => "°",
        ("advpi1", 109) => "μ",
        ("advpi1", 123) => "†",
        // A cedilla is a glyph of its own; the reflow attaches it to the letter.
        ("advspsasort", 223) => "¸",
        // `AdvPSMP1`: Mac Roman codes (2-3 papers each).
        ("advpsmp1", 160) => "†",
        ("advpsmp1", 212) => "‘",
        ("advpsmp1", 213) => "’",
        ("advpsmp1", 224) => "‡",
        ("advpsmp1", 225) => "·",
        ("advps_tir", c) => return times_special(c),
        _ => return None,
    })
}

/// Special glyphs of the Advent Times subsets (`AdvPS_TIR`, `AdvPTTIM*`).
fn times_special(code: u32) -> Text {
    Some(match code {
        23 => "©",
        30 => "°",
        31 => "µ",
        148 => "×",
        159 => "−",
        170 => "“",
        177 => "–",
        180 => "·",
        181 => "á",
        186 => "”",
        237 => "í",
        _ => return None,
    })
}

/// Advent text fonts whose whole character set has no Unicode map (60 papers, one to nine per font):
/// `C<n>` with n = the ASCII code, plus the ligatures, dashes and accents that the family keeps
/// at higher codes. The ASCII part turns 90-99% of the words of each of these fonts into words of the
/// corpus lexicon (65,000 words). Other codes: `C174` `C175` fi, fl (lexicon test in six fonts, 4-9
/// papers each), `C177` en dash (7-9 papers); `Advm1046*` (Elsevier) keeps ff, fi, fl, ffi at 161-164
/// (5 papers, lexicon test) and the en dash at 94 (5); the rest of the rows are 1-7 papers each.
fn advent_text(family: &str, code: u32) -> Text {
    let f = family;
    let elsevier = matches!(f, "advm1046a" | "advm1046c" | "advm1046e");
    let text = elsevier
        || f.starts_with("advtimes")
        || f.starts_with("advpttim")
        || matches!(
            f,
            "advps6f00" | "advps6f0b" | "advps6f01" | "advps94ba" | "advps94b2" | "advps9488" | "advps81db" | "advps81c9" | "advps81cf" | "advpsa18f"
                | "advpshvcl" | "advpshvcb" | "advpshvc" | "advp8e19" | "advp8e16" | "advp8e1c" | "advps40d469" | "advps40d46c" | "advps40d46b"
                | "advfrgcon" | "advpsfgc" | "advcyexp" | "advcyexp-i" | "advpsa177"
        );
    if !text {
        return None;
    }
    if elsevier {
        // Ligatures and accents at Mac Roman-like codes (5-38 papers for `advm1046a`).
        match code {
            94 => return Some("–"),
            161 => return Some("ff"),
            162 => return Some("fi"),
            163 => return Some("fl"),
            164 => return Some("ffi"),
            171 => return Some("¨"),
            179 => return Some("°"),
            214 => return Some("ø"),
            223 => return Some("©"),
            254 => return Some("±"),
            _ => {}
        }
    } else {
        match code {
            // Adobe StandardEncoding codes.
            170 => return Some("“"),
            174 => return Some("fi"),
            175 => return Some("fl"),
            177 => return Some("–"),
            186 => return Some("”"),
            _ => {}
        }
        match (f, code) {
            // 8 papers `C208`, 2 papers `C241` and `C249`.
            ("advps6f00", 208) => return Some("—"),
            ("advps6f00", 241) => return Some("æ"),
            ("advps6f00" | "advtimes", 249) => return Some("ø"),
            // 7 papers `C128`, 4 `C129`, 2 `C139` and `C194`.
            ("advtimes", 128) => return Some("ff"),
            ("advtimes", 129) => return Some("ffi"),
            ("advtimes", 139) => return Some("±"),
            ("advtimes", 194) => return Some("´"),
            _ => {}
        }
        if let Some(t) = times_special(code)
            && f.starts_with("advpttim")
            && code != 177
        {
            return Some(t);
        }
    }
    match code {
        // Quotes as in Adobe StandardEncoding (8 papers `C39`, 4 papers `C96`, in `AdvPS6F00`, `Advm1046a`, `AdvTimes`).
        39 => Some("’"),
        96 => Some("‘"),
        32..=126 => Some(ASCII[(code - 32) as usize]),
        _ => None,
    }
}

const ASCII: [&str; 95] = [
    " ", "!", "\"", "#", "$", "%", "&", "'", "(", ")", "*", "+", ",", "-", ".", "/", "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", ":", ";", "<", "=", ">", "?", "@", "A",
    "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X", "Y", "Z", "[", "\\", "]", "^", "_", "`", "a", "b", "c",
    "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z", "{", "|", "}", "~",
];

/// The `H<n>` glyph names of the Universal and Mathematical Pi fonts: the Greek letters (`H92xx`) are the
/// same in all of them, the symbols (`H11xxx`) are numbered per font.
fn h_names(family: &str, glyph: &str) -> Text {
    let n: u32 = glyph.strip_prefix('H')?.parse().ok()?;
    let f = family;
    // Greek letters, the same in all of these fonts (2-11 papers each; `H9023` and `H9004` in the Universal fonts only).
    let greek = match n {
        9004 => Some("Δ"),
        9023 => Some("Ψ"),
        9251 => Some("α"),
        9252 => Some("β"),
        9254 => Some("δ"),
        9255 => Some("ε"),
        9257 => Some("η"),
        9262 => Some("μ"),
        9263 => Some("ν"),
        9267 => Some("ρ"),
        9268 => Some("σ"),
        9270 => Some("τ"),
        9280 => Some("ϵ"),
        _ => None,
    };
    if greek.is_some() && (f.starts_with("universal-") || f.starts_with("mathematicalpi-") || f.starts_with("mathg-")) {
        return greek;
    }
    if f.starts_with("universal-greekwithmath") || f == "mathematicalpi-one" {
        // `+`, `−`, `=`, `±`, `<`, `>`, degree: papers of both fonts (7-31 each); the others in one font.
        return Some(match n {
            11001 => "+",
            11002 => "−",
            11003 => "×",
            11005 => "=",
            11006 => "±",
            11011 => "∼",
            11015 => "≈",
            11021 => "<",
            11022 => ">",
            11032 => "′",
            11008 => "∝",
            11009 => "∞",
            11033 => "″",
            11034 => "°",
            11349 => "≤",
            11350 => "≥",
            11509 => "∂",
            11612 => "∇",
            20848 => "∫",
            9278 => "φ",
            _ => return None,
        });
    }
    if f == "mathematicalpi-four" {
        // 3-7 papers each.
        return Some(match n {
            11543 => "°",
            11545 => "+",
            11546 => "−",
            11547 => "×",
            11549 => "=",
            11550 => "±",
            _ => return None,
        });
    }
    if f.starts_with("universal-newswithcomm") {
        // 2-31 papers each.
        return Some(match n {
            11501 => "+",
            11502 => "−",
            11503 => "×",
            11505 => "=",
            17015 => "©",
            18528 => "·",
            _ => return None,
        });
    }
    None
}

/// `MathPackTen`: Greek letters named `afii98xx` (2-10 papers each), and `p10` (2 papers).
fn mathpack(family: &str, glyph: &str) -> Text {
    if family != "mathpackten" {
        return None;
    }
    Some(match glyph {
        "afii9826" => "β",
        "afii9830" => "ε",
        "afii9838" => "λ",
        "afii9841" => "ξ",
        "afii9845" => "ρ",
        "afii9846" => "σ",
        "afii9848" => "τ",
        "p10" => "φ",
        _ => return None,
    })
}

/// Meaning that a glyph name carries in any font: Adobe standard names,
/// `uniXXXX` / `uXXXXX`, the TeX names of delimiters and big operators, and the
/// capital Greek letters of MathType (`Delta1`).
fn named_text(glyph: &str) -> Option<String> {
    let base = glyph.split('.').next().unwrap_or(glyph);
    if let Some(c) = unicode_name(base) {
        return Some(c.to_string());
    }
    if let Some(t) = common_name(base) {
        return Some(t.to_string());
    }
    if let Some(g) = base.strip_suffix('1') {
        let c = match g {
            "Gamma" => 'Γ',
            "Delta" => 'Δ',
            "Theta" => 'Θ',
            "Lambda" => 'Λ',
            "Xi" => 'Ξ',
            "Pi" => 'Π',
            "Sigma" => 'Σ',
            "Upsilon" => 'Υ',
            "Phi" => 'Φ',
            "Psi" => 'Ψ',
            "Omega" => 'Ω',
            _ => return None,
        };
        return Some(c.to_string());
    }
    None
}

/// `uni20AC`, `u1D7CE` and the Adobe Glyph List (`minus`, `degree`, `plusminus`, `mu`...).
fn unicode_name(name: &str) -> Option<char> {
    let hex = |s: &str, min: usize, max: usize| {
        (s.len() >= min && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))).then(|| u32::from_str_radix(s, 16).ok()).flatten()
    };
    let cp = if let Some(h) = name.strip_prefix("uni") {
        hex(h, 4, 4)
    } else if let Some(h) = name.strip_prefix('u') {
        hex(h, 4, 6)
    } else {
        None
    };
    let cp = match cp {
        Some(cp) => cp,
        None => {
            // Numbers ("12"), "g12" and the like are glyph indices, not names.
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric()) || name.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
                return None;
            }
            let c = CString::new(name).ok()?;
            // SAFETY: pure table lookup on a NUL-terminated string.
            let cp = unsafe { fz_unicode_from_glyph_name_strict(c.as_ptr()) };
            u32::try_from(cp).ok().filter(|&v| v != 0)?
        }
    };
    let c = char::from_u32(cp)?;
    // Controls and private use are no answers (`u` + hex letters can be a word).
    (!c.is_control() && !matches!(cp, 0xE000..=0xF8FF)).then_some(c)
}

/// Names outside the Adobe list that say what the glyph is: `equals` (`AdvminionSymbols`, 12 papers), and the
/// TeX big delimiters and operators (`parenleftbigg`, `summationdisplay`), which are drawn at
/// full size (CMEX10, MTEX, BLEX and variants, 250 papers).
fn common_name(name: &str) -> Text {
    match name {
        "equals" => return Some("="),
        // `NewtonA` (5 papers): the hyphen at the end of a line.
        "hyphenminus" => return Some("-"),
        // `wasy9` (5 papers).
        "permil" => return Some("‰"),
        _ => {}
    }
    let base = ["bigg", "Bigg", "big", "Big", "text", "display"].iter().find_map(|s| name.strip_suffix(s)).unwrap_or(name);
    Some(match base {
        "parenleft" => "(",
        "parenright" => ")",
        "bracketleft" => "[",
        "bracketright" => "]",
        "braceleft" => "{",
        "braceright" => "}",
        "summation" => "∑",
        "integral" => "∫",
        "product" => "∏",
        "radical" => "√",
        "slash" => "/",
        _ if name == "vextendsingle" => "|",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_names() {
        assert_eq!(family("ABCDEF+AdvP4C4E74"), "advp4c4e74");
        assert_eq!(family("XxkbtbAdvP4C4E46"), "advp4c4e46");
        assert_eq!(family("DDGEGD+Whitney-Light"), "whitney-light");
        assert_eq!(family("PxshxvVdvbhgAdvP4C4E74"), "advp4c4e74");
    }

    #[test]
    fn standard_names() {
        assert_eq!(glyph_text("Foo", "minus").as_deref(), Some("−"));
        assert_eq!(glyph_text("Foo", "degree").as_deref(), Some("°"));
        assert_eq!(glyph_text("Foo", "plusminus").as_deref(), Some("±"));
        assert_eq!(glyph_text("Foo", "mu").as_deref(), Some("µ"));
        assert_eq!(glyph_text("Foo", "parenleft.s2").as_deref(), Some("("));
        assert_eq!(glyph_text("Foo", "uni2211.s1").as_deref(), Some("∑"));
        assert_eq!(glyph_text("Foo", "u1D7CE").as_deref(), Some("\u{1D7CE}"));
        assert_eq!(glyph_text("Foo", "equal").as_deref(), Some("="));
        assert_eq!(glyph_text("AdvminionSymbols", "equals").as_deref(), Some("="));
        // Indices and unknown names are no characters.
        for n in ["12", "g113", "gid00030", ".notdef", "C0", "H11001", "a12x", ""] {
            assert_eq!(glyph_text("Foo", n), None, "{n}");
        }
        // "u" + hex letters that is not a code point.
        assert_eq!(glyph_text("Foo", "uface"), None);
    }

    #[test]
    fn advent_pi_fonts() {
        // The same glyph name, three fonts, three layouts.
        assert_eq!(glyph_text("ABCDEF+AdvP4C4E74", "C0").as_deref(), Some("−"));
        assert_eq!(glyph_text("ABCDEF+AdvP4C4E74", "C1").as_deref(), Some("·"));
        assert_eq!(glyph_text("ABCDEF+AdvP4C4E74", "C24").as_deref(), Some("∼"));
        assert_eq!(glyph_text("XYZABC+AdvP4C4E59", "C0").as_deref(), Some("Γ"));
        assert_eq!(glyph_text("ABCDEF+AdvP4C4E59", "C19").as_deref(), Some("´"));
        assert_eq!(glyph_text("ABCDEF+AdvP4C4E46", "C0").as_deref(), Some("("));
        assert_eq!(glyph_text("ABCDEF+AdvP4C4E51", "C27").as_deref(), Some("σ"));
        assert_eq!(glyph_text("TeX_CM_Maths_Symbols16", "C138").as_deref(), Some("]"));
        // Adobe Symbol codes.
        assert_eq!(glyph_text("AdvPSSym", "C211").as_deref(), Some("©"));
        assert_eq!(glyph_text("AdvPSSym", "C176").as_deref(), Some("°"));
        assert_eq!(glyph_text("AdvTTab7e17fd", "C176").as_deref(), Some("°"));
        assert_eq!(glyph_text("AdvGreekM", "C109").as_deref(), Some("μ"));
        assert_eq!(glyph_text("AdvGreekM", "C176"), None);
        // Unlisted glyphs and unknown fonts stay undecoded.
        assert_eq!(glyph_text("ABCDEF+AdvP4C4E74", "C99"), None);
        assert_eq!(glyph_text("SomeFont", "C0"), None);
        assert_eq!(glyph_text("AdvP49C2A1", ".0136").as_deref(), Some("="));
    }

    #[test]
    fn h_families() {
        assert_eq!(glyph_text("ABCDEF+Universal-GreekwithMathP", "H11001").as_deref(), Some("+"));
        assert_eq!(glyph_text("ABCDEF+MathematicalPi-One", "H11001").as_deref(), Some("+"));
        assert_eq!(glyph_text("ABCDEF+MathematicalPi-Four", "H11543").as_deref(), Some("°"));
        assert_eq!(glyph_text("ABCDEF+Universal-GreekwithMathP", "H11543"), None);
        assert_eq!(glyph_text("ABCDEF+MathematicalPi-Four", "H9268").as_deref(), Some("σ"));
        assert_eq!(glyph_text("ABCDEF+Universal-NewswithCommPi", "H17015").as_deref(), Some("©"));
        assert_eq!(glyph_text("SomeFont", "H9268"), None);
    }

    #[test]
    fn tex_names() {
        assert_eq!(glyph_text("CMEX10", "parenleftbigg").as_deref(), Some("("));
        assert_eq!(glyph_text("MTEX", "bracketrightBig").as_deref(), Some("]"));
        assert_eq!(glyph_text("MTEX", "summationdisplay").as_deref(), Some("∑"));
        assert_eq!(glyph_text("MTEX", "integraltext").as_deref(), Some("∫"));
        assert_eq!(glyph_text("MTEX", "radicalbig").as_deref(), Some("√"));
        assert_eq!(glyph_text("MTEX", "vextendsingle").as_deref(), Some("|"));
        // Pieces of assembled delimiters are not characters.
        assert_eq!(glyph_text("MTEX", "bracehtipupleft"), None);
        assert_eq!(glyph_text("MTEX", "radicaltp"), None);
        assert_eq!(glyph_text("MTMI", "Delta1").as_deref(), Some("Δ"));
        assert_eq!(glyph_text("MTMI", "Omega1").as_deref(), Some("Ω"));
    }

    #[test]
    fn advent_text_fonts() {
        assert_eq!(glyph_text("ABCDEF+AdvPS6F00", "C101").as_deref(), Some("e"));
        assert_eq!(glyph_text("ABCDEF+AdvPS6F00", "C174").as_deref(), Some("fi"));
        assert_eq!(glyph_text("ABCDEF+AdvPS6F00", "C177").as_deref(), Some("–"));
        assert_eq!(glyph_text("ABCDEF+Advm1046a", "C94").as_deref(), Some("–"));
        assert_eq!(glyph_text("ABCDEF+Advm1046a", "C164").as_deref(), Some("ffi"));
        assert_eq!(glyph_text("ABCDEF+Advm1046a", "C46").as_deref(), Some("."));
        assert_eq!(glyph_text("ABCDEF+AdvTimes", "C128").as_deref(), Some("ff"));
        assert_eq!(glyph_text("ABCDEF+AdvTimes", "C39").as_deref(), Some("’"));
        assert_eq!(glyph_text("ABCDEF+AdvPS6F00", "C40._").as_deref(), Some("("));
        // A pi font of the same family of names is not text.
        assert_eq!(glyph_text("ABCDEF+AdvP4C4E74", "C101"), None);
        // The names of an unknown font say nothing about the code.
        assert_eq!(glyph_text("ABCDEF+AdvUnknown", "C101"), None);
    }

    #[test]
    fn symbol_table() {
        assert_eq!(symbol_char(0x62), Some('β'));
        assert_eq!(symbol_char(0xB0), Some('°'));
        assert_eq!(symbol_char(0xE2), Some('®'));
    }
}
