//! Half-width letters, digits and symbols in vertical (tategaki) text.
//!
//! In `writing-mode: vertical-rl` the browser lays every half-width character
//! on its side. Japanese books set them by kind instead, which this module
//! marks up for the style sheet (the text itself is unchanged, so copying and
//! searching see what the PDF has):
//!
//! - `tcy` (tate-chu-yoko, horizontal in one em box): one or two digits ("12"),
//!   one or two letters ("m", "km", "Mw"), "!?" / "!!", and lone symbols such
//!   as "%" or "*" that would otherwise lie down.
//! - `up` (each character upright, stacked): numbers of three or four digits
//!   ("1994") and short capitals or formulas ("GPS", "NHK", "H2O").
//! - left on its side: words, phrases with spaces, longer numbers and
//!   expressions joined by punctuation ("3.4km/", "Hi-net", "1,000"), brackets,
//!   dashes and the like, whose rotated forms are the vertical ones.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Side,
    Tcy,
    Up,
}

fn is_run_char(c: char) -> bool {
    c.is_ascii_graphic()
}

/// Joins letters and digits into one expression when it stands between them.
fn is_connector(c: char) -> bool {
    matches!(c, '.' | ',' | ':' | '/' | '-' | '\'' | '+' | '_' | '=' | '&')
}

/// Symbols that stand upright on their own.
fn is_upright_symbol(c: char) -> bool {
    matches!(c, '%' | '*' | '#' | '&' | '@' | '+' | '/' | '\\' | '$' | '^' | '|')
}

fn alnum_kind(s: &str) -> Kind {
    let n = s.chars().count();
    let lower = s.chars().any(|c| c.is_ascii_lowercase());
    match n {
        1 | 2 => Kind::Tcy,
        // Years and the like; acronyms and formulas (GPS, NHK, H2O, CO2).
        3 | 4 if !lower => Kind::Up,
        _ => Kind::Side,
    }
}

/// Pieces of one run of half-width characters without spaces, as byte ranges.
fn classify(run: &str) -> Vec<(Kind, usize, usize)> {
    let b = run.as_bytes();
    let mut out: Vec<(Kind, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        let mut start = i;
        let after_side = out.last().is_some_and(|p| p.0 == Kind::Side && p.2 == start);
        let kind = if c.is_ascii_alphanumeric() {
            // An expression: letters and digits, with connectors between them.
            let mut joined = false;
            i += 1;
            while i < b.len() {
                let d = b[i] as char;
                if d.is_ascii_alphanumeric() {
                    i += 1;
                } else if is_connector(d) && i + 1 < b.len() && (b[i + 1] as char).is_ascii_alphanumeric() {
                    joined = true;
                    i += 1;
                } else {
                    break;
                }
            }
            let word = &run[start..i];
            // A number and its unit ("8cm", "20km"): each part on its own, when
            // neither has to lie down.
            let split = word.find(|c: char| c.is_ascii_alphabetic()).filter(|&d| d > 0 && word[..d].bytes().all(|c| c.is_ascii_digit()) && word[d..].bytes().all(|c| c.is_ascii_alphabetic()));
            match split {
                Some(d) if !joined && alnum_kind(&word[..d]) != Kind::Side && alnum_kind(&word[d..]) != Kind::Side => {
                    out.push((alnum_kind(&word[..d]), start, start + d));
                    start += d;
                    alnum_kind(&word[d..])
                }
                _ if joined => Kind::Side,
                _ => alnum_kind(word),
            }
        } else if c == '!' || c == '?' {
            while i < b.len() && matches!(b[i], b'!' | b'?') {
                i += 1;
            }
            if i - start <= 2 { Kind::Tcy } else { Kind::Side }
        } else {
            i += 1;
            // "km/" reads on its side as a whole.
            if is_upright_symbol(c) && !(after_side && start > 0 && (b[start - 1] as char).is_ascii_alphanumeric()) { Kind::Tcy } else { Kind::Side }
        };
        match out.last_mut() {
            // Neighbouring pieces on their side share one text node.
            Some(p) if p.0 == Kind::Side && kind == Kind::Side => p.2 = i,
            _ => out.push((kind, start, i)),
        }
    }
    out
}

fn esc(s: &str, o: &mut String) {
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
}

/// `text` as HTML for vertical writing: escaped, with the half-width runs that
/// should not lie on their side wrapped in `<span class="tcy">` / `"up"`.
pub fn vertical_html(text: &str) -> String {
    let mut o = String::with_capacity(text.len() + 16);
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut k = 0;
    while k < chars.len() {
        let (pos, c) = chars[k];
        if !is_run_char(c) {
            esc(&text[pos..pos + c.len_utf8()], &mut o);
            k += 1;
            continue;
        }
        // A run, spaces included when half-width characters follow them.
        let mut e = k + 1;
        let mut spaced = false;
        while e < chars.len() {
            let d = chars[e].1;
            if is_run_char(d) {
                e += 1;
            } else if d == ' ' && e + 1 < chars.len() && is_run_char(chars[e + 1].1) {
                spaced = true;
                e += 1;
            } else {
                break;
            }
        }
        let end = chars.get(e).map_or(text.len(), |&(p, _)| p);
        let run = &text[pos..end];
        if spaced {
            // Words of a phrase read along the line.
            esc(run, &mut o);
        } else {
            for (kind, a, b) in classify(run) {
                let piece = &run[a..b];
                match kind {
                    Kind::Side => esc(piece, &mut o),
                    Kind::Tcy | Kind::Up => {
                        o.push_str(if kind == Kind::Tcy { "<span class=\"tcy\">" } else { "<span class=\"up\">" });
                        esc(piece, &mut o);
                        o.push_str("</span>");
                    }
                }
            }
        }
        k = e;
    }
    o
}

#[cfg(test)]
mod tests {
    use super::vertical_html as v;

    #[test]
    fn short_numbers_and_units_sit_in_one_box() {
        assert_eq!(v("数百m、"), "数百<span class=\"tcy\">m</span>、");
        assert_eq!(v("60%"), "<span class=\"tcy\">60</span><span class=\"tcy\">%</span>");
        assert_eq!(v("Mw七・九"), "<span class=\"tcy\">Mw</span>七・九");
        assert_eq!(v("なぜか!?"), "なぜか<span class=\"tcy\">!?</span>");
    }

    #[test]
    fn years_and_acronyms_stand_upright() {
        assert_eq!(v("1994年"), "<span class=\"up\">1994</span>年");
        assert_eq!(v("GPSの"), "<span class=\"up\">GPS</span>の");
        assert_eq!(v("H2O"), "<span class=\"up\">H2O</span>");
    }

    #[test]
    fn words_and_expressions_lie_on_their_side() {
        assert_eq!(v("毎秒3.4km/秒"), "毎秒3.4km/秒");
        // An upright unit keeps its slash upright too.
        assert_eq!(v("八m/秒"), "八<span class=\"tcy\">m</span><span class=\"tcy\">/</span>秒");
        assert_eq!(v("Hi-net"), "Hi-net");
        assert_eq!(v("slip"), "slip");
        assert_eq!(v("GEONET"), "GEONET");
        assert_eq!(v("(Global Positioning System)"), "(Global Positioning System)");
        assert_eq!(v("Mw7.7"), "Mw7.7");
        assert_eq!(v("kHz"), "kHz");
        assert_eq!(v("51994"), "51994");
    }

    #[test]
    fn numbers_with_units() {
        assert_eq!(v("8cm"), "<span class=\"tcy\">8</span><span class=\"tcy\">cm</span>");
        assert_eq!(v("200km"), "<span class=\"up\">200</span><span class=\"tcy\">km</span>");
        // A unit of three letters lies down with its number.
        assert_eq!(v("50kHz"), "50kHz");
    }

    #[test]
    fn figure_numbers() {
        assert_eq!(v("図-1(A)"), "図-<span class=\"tcy\">1</span>(<span class=\"tcy\">A</span>)");
        assert_eq!(v("(1992)"), "(<span class=\"up\">1992</span>)");
    }

    #[test]
    fn escapes() {
        assert_eq!(v("A&B <x>"), "A&amp;B &lt;x&gt;");
        assert_eq!(v("R&D"), "R&amp;D");
        assert_eq!(v("*1"), "<span class=\"tcy\">*</span><span class=\"tcy\">1</span>");
    }
}
