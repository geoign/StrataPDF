//! Hyphens at line ends: a word broken across lines ("volca-" / "nic") loses
//! its hyphen, a compound ("large-" / "scale") keeps it. The document's own
//! words decide: the joined or the hyphenated form written elsewhere on one
//! line, else whether both halves are words of their own.

use std::collections::HashMap;

use crate::rich::{RichBlock, RichPage};

pub(super) fn is_hyphen(c: char) -> bool {
    matches!(c, '-' | '\u{2010}' | '\u{2011}' | '\u{00AD}')
}

#[derive(Default)]
pub(super) struct Lexicon {
    words: HashMap<String, u32>,
}

impl Lexicon {
    /// Words of all pages, except the halves of words broken at a line end.
    pub(super) fn build<'a>(pages: impl Iterator<Item = &'a RichPage>) -> Lexicon {
        let mut words: HashMap<String, u32> = HashMap::new();
        for p in pages {
            for b in &p.blocks {
                let RichBlock::Text { lines, .. } = b else { continue };
                let mut broken_start = false;
                for l in lines {
                    let t = l.text();
                    let toks: Vec<&str> = t.split_whitespace().collect();
                    let broken_end = t.trim_end().ends_with(is_hyphen);
                    for (i, tok) in toks.iter().enumerate() {
                        if (i == 0 && broken_start) || (i + 1 == toks.len() && broken_end) {
                            continue;
                        }
                        let w: String = tok.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
                        if w.chars().filter(|c| c.is_alphabetic()).count() < 2 {
                            continue;
                        }
                        *words.entry(w.clone()).or_default() += 1;
                        if w.contains(is_hyphen) {
                            for part in w.split(is_hyphen) {
                                if part.chars().count() >= 2 {
                                    *words.entry(part.to_string()).or_default() += 1;
                                }
                            }
                        }
                    }
                    broken_start = broken_end;
                }
            }
        }
        Lexicon { words }
    }

    fn has(&self, w: &str) -> bool {
        self.words.contains_key(w)
    }

    /// The word, or a form of it (plural, past, gerund), is in the document.
    fn has_form(&self, w: &str) -> bool {
        self.has(w)
            || ["s", "es", "ed", "d", "ing"].iter().any(|e| self.has(&format!("{w}{e}")))
            || ["s", "es"].iter().any(|e| w.strip_suffix(e).is_some_and(|b| b.chars().count() >= 3 && self.has(b)))
    }

    /// Case-insensitive `has`, for callers outside this module.
    pub(super) fn has_word(&self, w: &str) -> bool {
        self.has(&w.to_lowercase())
    }

    /// Whether "a-" at a line end and "b" at the start of the next line form a
    /// hyphenated compound (keep the hyphen) rather than one word broken in two.
    pub(super) fn keep_hyphen(&self, a: &str, b: &str) -> bool {
        let a = a.to_lowercase();
        let b = b.to_lowercase();
        if a.is_empty() || b.is_empty() {
            return false;
        }
        if self.has(&format!("{a}-{b}")) {
            return true;
        }
        if self.has_form(&format!("{a}{b}")) {
            return false;
        }
        // Both halves are words of three letters or more ("time-scale"), not
        // syllables ("infor-mation", "be-cause").
        a.chars().count() >= 3 && b.chars().count() >= 3 && self.has_form(&a) && self.has_form(&b)
    }
}

/// The letters before a trailing hyphen ("volca" of "…the volca-").
pub(super) fn word_before_hyphen(s: &str) -> Option<&str> {
    let t = s.trim_end();
    let body = t.strip_suffix(is_hyphen)?;
    let start = body.rfind(|c: char| !c.is_alphanumeric()).map_or(0, |i| i + body[i..].chars().next().unwrap().len_utf8());
    let w = &body[start..];
    (!w.is_empty() && w.chars().last().is_some_and(char::is_alphabetic)).then_some(w)
}

/// The leading letters and digits of a line or paragraph, in any case.
pub(super) fn word_after_any(s: &str) -> Option<&str> {
    let t = s.trim_start();
    let end = t.find(|c: char| !c.is_alphanumeric()).unwrap_or(t.len());
    (end > 0).then(|| &t[..end])
}

/// The leading letters of a line or paragraph that starts in lowercase.
pub(super) fn word_after(s: &str) -> Option<&str> {
    let t = s.trim_start();
    if !t.chars().next().is_some_and(char::is_lowercase) {
        return None;
    }
    let end = t.find(|c: char| !c.is_alphanumeric()).unwrap_or(t.len());
    Some(&t[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(words: &[&str]) -> Lexicon {
        Lexicon { words: words.iter().map(|w| (w.to_string(), 1)).collect() }
    }

    #[test]
    fn hyphens() {
        let l = lex(&["volcanic", "large-scale", "large", "scale", "time", "information", "for", "be", "cause"]);
        assert!(!l.keep_hyphen("volca", "nic"));
        assert!(l.keep_hyphen("large", "scale"));
        assert!(!l.keep_hyphen("infor", "mation"));
        assert!(!l.keep_hyphen("be", "cause"));
        let l = lex(&["time", "scale"]);
        assert!(l.keep_hyphen("time", "scale"));
        assert_eq!(word_before_hyphen("the volca- "), Some("volca"));
        assert_eq!(word_before_hyphen("pre-"), Some("pre"));
        assert_eq!(word_before_hyphen("1980-"), None);
        assert_eq!(word_after("nic rocks"), Some("nic"));
        assert_eq!(word_after("Nic"), None);
    }
}
