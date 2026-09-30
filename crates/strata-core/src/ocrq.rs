//! Quality of the text layer that another program's OCR left over scanned pages.
//!
//! Scans of old papers often carry the text of an old OCR engine, and some of it is poor
//! enough that reading it, searching it or translating it goes wrong ("x•ith", "1ower",
//! "火Lli体" for 火山体). The measure is taken over the whole document:
//!
//! - English: the share of lower-case words that are neither in a dictionary (the word
//!   list of Webster's Second International, 1934, public domain, with inflections,
//!   British spellings, prefixes and compounds) nor terms of the paper's subject (a word
//!   of five letters or more that the paper uses three times or more), plus words garbled
//!   by symbols, digits or capitals inside.
//! - Japanese: the share of characters with the usual signs of a misrecognition: kanji
//!   outside JIS level 1, a kanji that looks like katakana inside a katakana word
//!   (ユニッ卜), a lone katakana between kanji (同層ヒ部), a few Latin letters between
//!   Japanese characters (火Lli体), a bracket glued to kanji (【E断層).
//!
//! The thresholds come from 50 scanned papers whose recognition errors were counted
//! against the page image. None of the 25 nearly perfect layers (under 1% of words
//! wrong) is poor by them; of the 18 with 1–4% wrong, three Japanese ones are; of the
//! 7 with more, four are. (One that is missed is bad on some pages and fair on most.)

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Lower-case words of Webster's Second International (the Unix `web2` list).
static WORDS_EN: &str = include_str!("../data/words-en.txt");
/// The 2,965 kanji of JIS X 0208 level 1, the common ones.
static JIS_LEVEL1: &str = include_str!("../data/jis-level1.txt");

/// General words the 1934 list lacks: irregular forms, units and abbreviations of
/// scientific writing, modern words. (No subject vocabulary.)
const EXTRA: &str = "began became held paid felt kept lent meant sent spent built dealt led fed fled bred sped shed \
slid spun stuck struck strung swung wrung clung flung sprung sunk shrunk drunk hung \
et al etc vs cf eg ie ca pp ed eds edn vol vols fig figs eq eqs tab ref refs no nos sp spp \
km cm mm nm ml mg kg ha kyr ky myr ma ka ga yr yrs hr hrs min max ppm ppb wt mol kbar mbar \
ln exp log sin cos tan std approx doi https http www org com info \
ii iii iv vi vii viii ix xi xii \
online offline software hardware website homepage email dataset datasets database databases \
metadata workflow download downloaded pixel pixels benchmark timeline broadband baseline \
near box boxes feet classify catalog catalogs sulfur sulfide sulfides sulfate licence moduli versa priori situ fax";

fn lexicon() -> &'static HashSet<&'static str> {
    static L: OnceLock<HashSet<&'static str>> = OnceLock::new();
    L.get_or_init(|| WORDS_EN.lines().chain(EXTRA.split_whitespace()).collect())
}

fn jis_level1() -> &'static HashSet<char> {
    static L: OnceLock<HashSet<char>> = OnceLock::new();
    L.get_or_init(|| JIS_LEVEL1.trim().chars().collect())
}

/// Dictionary forms an inflected word may come from.
fn base_forms(w: &str) -> Vec<String> {
    const RULES: [(&str, &[&str]); 21] = [
        ("ies", &["y"]),
        ("ied", &["y"]),
        ("ier", &["y"]),
        ("iest", &["y"]),
        ("ily", &["y"]),
        ("es", &["", "e"]),
        ("s", &[""]),
        ("ed", &["", "e"]),
        ("ing", &["", "e"]),
        ("er", &["", "e"]),
        ("est", &["", "e"]),
        ("ly", &["", "le"]),
        ("al", &[""]),
        ("ally", &["", "al"]),
        ("ation", &["", "e", "ate"]),
        ("ations", &["", "e", "ate"]),
        ("ness", &[""]),
        ("ment", &[""]),
        ("ments", &[""]),
        ("ity", &["", "e"]),
        ("ities", &["", "e"]),
    ];
    let mut out = Vec::new();
    for (suf, reps) in RULES {
        let Some(stem) = w.strip_suffix(suf) else { continue };
        if stem.len() < 2 {
            continue;
        }
        out.extend(reps.iter().map(|r| format!("{stem}{r}")));
        // A doubled consonant: "stopped", "bigger".
        let b = stem.as_bytes();
        if b.len() >= 3 && b[b.len() - 1] == b[b.len() - 2] && !b"aeiouls".contains(&b[b.len() - 1]) {
            out.push(stem[..stem.len() - 1].to_string());
        }
    }
    out
}

/// American spellings of a British one ("colour", "centre", "characterised").
fn american(w: &str) -> Vec<String> {
    const ENDINGS: [(&str, &str); 10] = [
        ("our", "or"),
        ("ours", "ors"),
        ("oured", "ored"),
        ("ouring", "oring"),
        ("ourable", "orable"),
        ("tre", "ter"),
        ("tres", "ters"),
        ("tred", "tered"),
        ("ogue", "og"),
        ("ogues", "ogs"),
    ];
    let mut out: Vec<String> = ENDINGS.iter().filter_map(|(a, b)| w.strip_suffix(a).map(|s| format!("{s}{b}"))).collect();
    for suf in ["ise", "ised", "ises", "ising", "isation", "isations", "iser", "isers", "yse", "ysed", "yses", "ysing"] {
        if let Some(s) = w.strip_suffix(suf) {
            out.push(format!("{s}{}", suf.replacen('s', "z", 1)));
        }
    }
    out
}

fn known1(w: &str) -> bool {
    let lex = lexicon();
    lex.contains(w) || base_forms(w).iter().any(|b| lex.contains(b.as_str()))
}

/// A lower-case ASCII word that is English.
fn known(w: &str) -> bool {
    if known1(w) || american(w).iter().any(|a| known1(a)) {
        return true;
    }
    const PREFIXES: [&str; 19] = ["pre", "re", "un", "non", "sub", "inter", "intra", "multi", "micro", "macro", "over", "under", "co", "de", "post", "semi", "super", "ultra", "mega"];
    if PREFIXES.iter().any(|p| w.strip_prefix(p).is_some_and(|rest| rest.len() >= 4 && known1(rest))) {
        return true;
    }
    // A compound: "seafloor", "worldwide".
    (3..w.len().saturating_sub(2)).any(|i| w.len() - i >= 3 && lexicon().contains(&w[..i]) && known1(&w[i..]))
}

/// The words of a page's text: line-end hyphenation joined, hyphenated and slashed words
/// split, surrounding punctuation and stray symbols dropped.
fn words(text: &str) -> Vec<&str> {
    const STRIP: &[char] = &[
        '"', '\'', '`', '.', ',', ';', ':', '!', '?', '(', ')', '[', ']', '{', '}', '<', '>', '«', '»', '“', '”', '‘', '’', '*', '†', '‡', '§', '¶', '|', '/', '\\', '=', '•',
        '~', '^', '_', '+', '#', '@', '$', '%', '&',
    ];
    let mut out = Vec::new();
    for tok in text.split_whitespace() {
        for part in tok.split(['-', '–', '/']) {
            let mut w = part.trim_matches(STRIP);
            if let Some(s) = w.strip_suffix("'s").or_else(|| w.strip_suffix("’s")) {
                w = s;
            }
            if w.len() >= 2 && w.is_ascii() {
                out.push(w);
            }
        }
    }
    out
}

/// Joins words hyphenated at line ends ("depo-\nsits").
fn dehyphenate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('-') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let trimmed = after.trim_start_matches([' ', '\t', '\r']);
        if let Some(next) = trimmed.strip_prefix('\n') {
            let next = next.trim_start();
            if next.starts_with(|c: char| c.is_ascii_lowercase()) {
                rest = next;
                continue;
            }
        }
        out.push('-');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// A word garbled by the recognition: a symbol between letters ("x•ith", "unfn,ctionated"),
/// a capital inside a lower-case word ("frQm", "mLxed"), digits inside a word ("wa1er"),
/// or a digit for its first letter ("1ower").
fn garbled(w: &str) -> bool {
    let c: Vec<char> = w.chars().collect();
    if c[0].is_ascii_digit() && c.len() >= 4 && c[1..].iter().all(|c| c.is_ascii_lowercase()) {
        return true;
    }
    if c.iter().filter(|c| c.is_ascii_alphabetic()).count() < 4 {
        return false;
    }
    let symbol_inside = c.windows(3).any(|t| t[0].is_ascii_lowercase() && t[2].is_ascii_lowercase() && !(t[1].is_ascii_alphanumeric() || t[1].is_whitespace() || matches!(t[1], '\'' | '’' | '.')));
    let capital_inside = c[0].is_ascii_lowercase() && c.windows(2).any(|t| t[0].is_ascii_lowercase() && t[1].is_ascii_uppercase());
    let digits_inside = c[0].is_ascii_alphabetic() && {
        let mut i = 1;
        let mut found = false;
        while i < c.len() {
            if c[i].is_ascii_digit() && c[i - 1].is_ascii_alphabetic() {
                let mut j = i;
                while j < c.len() && c[j].is_ascii_digit() {
                    j += 1;
                }
                if j < c.len() && c[j].is_ascii_lowercase() {
                    found = true;
                    break;
                }
                i = j;
            } else {
                i += 1;
            }
        }
        found
    };
    symbol_inside || capital_inside || digits_inside
}

fn is_kanji(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF)
}

fn is_hiragana(c: char) -> bool {
    matches!(c as u32, 0x3041..=0x3096)
}

fn is_katakana(c: char) -> bool {
    matches!(c as u32, 0x30A1..=0x30FA | 0x30FC)
}

/// (Japanese characters, suspicious recognitions) of a text.
fn japanese(text: &str) -> (usize, usize) {
    // Kanji that look like katakana (or the long-vowel mark).
    const LOOKALIKE: &[char] = &['卜', '一', '口', '工', '力', '夕', '二', '八'];
    let t: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    let jis = jis_level1();
    let (mut ja, mut bad) = (0, 0);
    let at = |i: usize| t.get(i).copied();
    let mut i = 0;
    while i < t.len() {
        let c = t[i];
        let prev = i.checked_sub(1).map(|p| t[p]);
        let next = at(i + 1);
        if is_kanji(c) || is_hiragana(c) || is_katakana(c) {
            ja += 1;
        }
        if is_kanji(c) && !jis.contains(&c) {
            bad += 1;
        }
        // ユニッ卜の, but not データ一覧.
        if LOOKALIKE.contains(&c) && prev.is_some_and(is_katakana) && !next.is_some_and(is_kanji) {
            bad += 1;
        }
        // 同層ヒ部.
        if c != 'ー' && is_katakana(c) && prev.is_some_and(is_kanji) && next.is_some_and(is_kanji) {
            bad += 1;
        }
        // 【E断層, 火［ll.
        if matches!(c, '【' | '［' | '「' | '〔') {
            let n = (1..=2).take_while(|&k| at(i + k).is_some_and(|c| c.is_ascii_alphanumeric())).count();
            let glued_before = n > 0 && at(i + n + 1).is_some_and(is_kanji);
            let glued_after = c != '【' && prev.is_some_and(is_kanji) && next.is_some_and(|c| c.is_ascii_alphabetic());
            if glued_before || glued_after {
                bad += 1;
            }
        }
        // 火Lli体: a run of one to three Latin letters between Japanese characters.
        if c.is_ascii_alphabetic() {
            let mut j = i;
            while j < t.len() && t[j].is_ascii_alphabetic() {
                j += 1;
            }
            let japanese = |c: Option<char>| c.is_some_and(|c| is_kanji(c) || is_hiragana(c));
            if j - i <= 3 && japanese(prev) && japanese(at(j)) {
                bad += 1;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    (ja, bad)
}

/// Recognition quality of a document's foreign OCR text layer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayerQuality {
    /// Scanned pages whose text layer was assessed.
    pub pages: usize,
    /// English words judged, and those that are not words.
    pub en_words: usize,
    pub en_bad: usize,
    /// Japanese characters, and suspicious recognitions among them.
    pub ja_chars: usize,
    pub ja_bad: usize,
}

impl LayerQuality {
    /// Assess a document from the text of its scanned pages with a foreign text layer,
    /// if they are at least half of its `total` pages: in a born-digital paper they are
    /// figures filling a page, whose labels read like garbage.
    pub fn assess_document<S: AsRef<str>>(scanned: &[S], total: usize) -> Option<Self> {
        (!scanned.is_empty() && scanned.len() * 2 >= total).then(|| Self::assess(scanned))
    }

    /// Assess the text of each scanned page (lines separated by newlines).
    pub fn assess<S: AsRef<str>>(pages: &[S]) -> Self {
        let texts: Vec<String> = pages.iter().map(|p| dehyphenate(p.as_ref())).collect();
        let ws: Vec<&str> = texts.iter().flat_map(|t| words(t)).collect();
        let mut freq: HashMap<String, usize> = HashMap::new();
        for w in &ws {
            *freq.entry(w.to_ascii_lowercase()).or_default() += 1;
        }
        let mut q = LayerQuality { pages: pages.len(), ..Default::default() };
        for w in ws {
            if garbled(w) {
                q.en_words += 1;
                q.en_bad += 1;
            } else if w.bytes().all(|b| b.is_ascii_lowercase()) {
                q.en_words += 1;
                // A word the dictionary lacks that the paper keeps using is a term of its subject;
                // a short one is more often a misreading that recurs ("ith" for "with").
                if !known(w) && (w.len() < 5 || freq[w] < 3) {
                    q.en_bad += 1;
                }
            }
        }
        for t in &texts {
            let (a, b) = japanese(t);
            q.ja_chars += a;
            q.ja_bad += b;
        }
        q
    }

    /// Share of English words that are misrecognitions (`None` with too few words).
    pub fn en_rate(&self) -> Option<f32> {
        (self.en_words >= 300).then(|| self.en_bad as f32 / self.en_words as f32)
    }

    /// Share of Japanese characters that look misrecognised (`None` with too few).
    pub fn ja_rate(&self) -> Option<f32> {
        (self.ja_chars >= 1000).then(|| self.ja_bad as f32 / self.ja_chars as f32)
    }

    /// Poor enough that OCR of the page images would read better. Judged in the paper's
    /// main language: the English of a Japanese paper (abstract, captions, romanised
    /// names) is too little and too mixed to tell.
    pub fn poor(&self) -> bool {
        if self.ja_chars >= 1000 && self.ja_chars > self.en_words {
            self.ja_rate().is_some_and(|r| r > 0.01)
        } else {
            self.en_rate().is_some_and(|r| r > 0.04)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_with_inflections_and_british_spellings() {
        for w in ["the", "larger", "identified", "began", "colour", "centre", "characterised", "seafloor", "preexisting", "km", "online"] {
            assert!(known(w), "{w}");
        }
        for w in ["thc", "tbe", "vcnts", "distributioll", "wliich", "tlie"] {
            assert!(!known(w), "{w}");
        }
    }

    #[test]
    fn garbled_words() {
        for w in ["x•ith", "unfn,ctionated", "frQm", "mLxed", "1ower", "wa1er"] {
            assert!(garbled(w), "{w}");
        }
        for w in ["MgO", "DeCelles", "km2", "e.g", "don't", "1st", "10km", "r,t"] {
            assert!(!garbled(w), "{w}");
        }
    }

    #[test]
    fn hyphenation_at_line_ends_is_joined() {
        assert_eq!(dehyphenate("depo-\nsits and two-\nStage"), "deposits and two-\nStage");
        assert_eq!(dehyphenate("a - b"), "a - b");
    }

    #[test]
    fn japanese_misrecognitions() {
        assert_eq!(japanese("ユニッ卜の").1, 1);
        assert_eq!(japanese("同層ヒ部").1, 1);
        assert_eq!(japanese("火Lli体").1, 1);
        assert_eq!(japanese("【E断層").1, 1);
        for ok in ["データ一覧", "SiO2量", "K-Ar年代", "Fig.1に", "コンピューター", "平成10年", "ルート"] {
            assert_eq!(japanese(ok).1, 0, "{ok}");
        }
    }

    #[test]
    fn a_subject_term_is_no_error_but_a_recurring_short_misreading_is() {
        let text = "the caldera wall and caldera floor of the caldera ith ith ith";
        let q = LayerQuality::assess(&[text]);
        assert_eq!(q.en_bad, 3);
    }
}
