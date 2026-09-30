//! Reference lists. The extractor cuts a list into blocks that do not follow its
//! entries (a block per continuation line, every other line in one block, a
//! number or the tail of a line as a block of its own, two columns in one line),
//! and a page break or a column break cuts an entry in two. A reference section is
//! therefore rebuilt from its lines. It runs from its heading (or, without one, from
//! a column whose lines read as entries) to the next heading, over all pages. Per page
//! the lines are put in columns (by the gutter, or by where they start), fragments of
//! one baseline are joined, and each column is cut into entries by
//! 1. a list marker at the start of a line ("[12]", "12.", "12)"), if the list is numbered;
//! 2. the indent: the layout of the document's entries (hanging: the first line
//!    at the column's edge and the rest indented; or first-line indent) is learned
//!    from the columns that show it and applied to the ones that do not (the tail
//!    of a list with an entry or two);
//! 3. otherwise the line that opens with an author (or an organisation, or a
//!    Japanese name and a year) after a line that ended an entry, or after a gap.
//!
//! The first line of a column that does not start an entry continues the last
//! entry of the column or page before.
//!
//! `STRATA_NO_REFS` switches the pass off; `STRATA_DEBUG_REFS=<page>` prints the
//! columns and cuts of a page (1-based), `STRATA_DEBUG_REFS_EVENTS` the sections found.

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;

use super::hyphen::Lexicon;
use super::{Unit, UnitKind, caps_heading, caption_kind, heading_number, is_boilerplate, is_cjk, line_size};
use crate::geom::RectF;
use crate::rich::{RichChar, RichLine};
use crate::text::FontInfo;
use strata_ocr::layout::{CAPTION, FORMULA, PAGE_FOOTER, PAGE_HEADER, PICTURE, SECTION_HEADER, TABLE, TITLE};

/// What a unit is in a reference list.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum Ref {
    #[default]
    No,
    /// A whole entry.
    Entry,
    /// The rest of the entry before it, across a column or page break.
    Cont,
    /// The first lines of an entry of another kind of list (a glossary, a hanging-indent
    /// list; see [`super::entries`]): a paragraph or list item of its own, which never
    /// carries on the paragraph before.
    Start,
    /// An entry of a table of contents or an index (see [`super::toc`]): a list item of its own.
    Item,
}

/// What the pass needs to know of the page.
pub(super) struct Cx<'a> {
    /// 0-based page index.
    pub page: u32,
    pub body: f32,
    pub height: f32,
    /// OCR text: positions and sizes jitter.
    pub scan: bool,
    pub fonts: &'a [FontInfo],
    /// The document's words: they tell a word cut in two from two words (`None`: two).
    pub lex: Option<&'a Lexicon>,
}

// ------------------------------------------------------------------ patterns

/// Spacing accents ("Av´e") and combining marks dropped, then NFKC (full-width
/// digits and punctuation of Japanese lists become plain ones).
fn plain(t: &str) -> String {
    t.chars().filter(|c| !matches!(c, '\u{00B4}' | '`' | '\u{00A8}' | '\u{02C6}' | '\u{02DC}' | '\u{02C7}' | '\u{00B8}' | '\u{02DA}' | '\u{00AF}' | '\u{02D8}' | '\u{02D9}' | '\u{02DB}' | '\u{02DD}') && !('\u{0300}'..='\u{036F}').contains(c)).nfkc().collect()
}

/// The number of a list marker at the start of a line: "[12]", "12.", "12)",
/// "(12)". "12.5" is a number and "2010." a year, no markers.
pub(super) fn marker(t: &str) -> Option<u32> {
    let t = t.trim_start();
    let (close, body) = match t.chars().next()? {
        '[' => (Some(']'), &t[1..]),
        '(' => (Some(')'), &t[1..]),
        _ => (None, t),
    };
    let digits: String = body.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let rest = &body[digits.len()..];
    // A number and an author, no stop between ("1 Smith, J., 2010").
    if close.is_none() && rest.starts_with(char::is_whitespace) && author_start(rest.trim_start()) {
        return digits.parse().ok();
    }
    let rest = match close {
        Some(c) => rest.strip_prefix(c)?,
        None => rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?,
    };
    let next = rest.chars().next();
    if next.is_some_and(|c| c.is_ascii_digit()) || (close.is_none() && next.is_some_and(|c| !c.is_whitespace() && !c.is_uppercase() && !is_cjk(c))) {
        return None;
    }
    digits.parse().ok()
}

/// A bare marker ("12.", "[3]") that the extractor left as a line of its own.
fn marker_only(t: &str) -> bool {
    marker(t).is_some() && t.trim().chars().filter(|c| c.is_alphabetic()).count() == 0
}

/// A surname word: capitalised, with hyphens or apostrophes ("Smith", "McCloskey", "O'Neil").
fn surname(w: &str) -> bool {
    let w = w.trim_end_matches([',', ';']);
    w.chars().next().is_some_and(char::is_uppercase) && w.chars().filter(|c| c.is_alphabetic()).count() >= 2 && w.chars().all(|c| c.is_alphabetic() || matches!(c, '-' | '\'' | '’'))
}

const PARTICLES: [&str; 20] = ["de", "da", "di", "del", "della", "van", "von", "der", "den", "la", "le", "du", "dos", "das", "ter", "ten", "al", "el", "bin", "st"];

/// Initials with full stops ("J.", "D.W.", "J.-C.,").
fn initials(w: &str) -> bool {
    let w = w.trim_end_matches(',');
    let letters: Vec<char> = w.chars().filter(|c| c.is_alphabetic()).collect();
    !letters.is_empty() && letters.len() <= 3 && letters.iter().all(|c| c.is_uppercase()) && w.contains('.') && w.chars().all(|c| c.is_alphabetic() || matches!(c, '.' | '-'))
}

/// Initials after a surname without a comma: "T", "SG", "LV," (Springer, Vancouver),
/// "K.," and "B.W." (Surname K., Surname P., 2002:).
fn bare_initials(w: &str) -> bool {
    let w = w.trim_end_matches(',');
    (1..=3).contains(&w.chars().filter(|c| c.is_alphabetic()).count()) && w.chars().all(|c| c.is_ascii_uppercase() || matches!(c, '.' | '-'))
}

/// Words that open a sentence or a label where a surname could stand.
const NOT_NAMES: [&str; 22] = [
    "Figure", "Fig", "Table", "Section", "Appendix", "Plate", "Panel", "Part", "Method", "Type", "Group", "Unit", "Region", "Site", "Sample", "Model", "Case", "Step", "Phase", "Zone", "Line", "Box",
];

/// How a line starts an entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Lead {
    /// "Surname, I." or "Surname I, Surname I": personal names as a reference list sets them.
    Strong,
    /// Something that can be an entry's start: an organisation and a year, a
    /// name in full, initials first, a Japanese name and a year.
    Weak,
    No,
}

/// A four-digit year (1800-2030) not part of a longer number, and where it starts (in chars).
fn find_year(t: &str) -> Option<usize> {
    let v: Vec<char> = t.chars().collect();
    (0..v.len().saturating_sub(3)).find(|&i| {
        let four = v[i..i + 4].iter().all(char::is_ascii_digit);
        if !four || (i > 0 && v[i - 1].is_ascii_digit()) || v.get(i + 4).is_some_and(char::is_ascii_digit) {
            return false;
        }
        let y: u32 = v[i..i + 4].iter().collect::<String>().parse().unwrap_or(0);
        (1800..=2030).contains(&y) && !v.get(i + 4).is_some_and(|c| c.is_alphabetic() && v.get(i + 5).is_some_and(|d| d.is_alphabetic()))
    })
}

/// The text has a year or the words that stand for one.
pub(super) fn has_year(t: &str) -> bool {
    let l = t.to_lowercase();
    find_year(t).is_some() || l.contains("in press") || l.contains("submitted") || l.contains("forthcoming") || l.contains("n.d.")
}

/// "Almendros, J., Wilcock, W." or "Av´e Lallemant, H.G., ..." (a surname of up
/// to three words, a comma, then initials), and "Angerer T, Hagemann SG (2010)"
/// (a surname, initials without stops, then a comma, a year or "and").
pub(super) fn author_start(t: &str) -> bool {
    let t = plain(t);
    let toks: Vec<&str> = t.split_whitespace().take(9).collect();
    for k in 0..toks.len().min(3) {
        let w = toks[k];
        if w.ends_with(',') && w.chars().filter(|c| c.is_alphabetic()).count() >= 2 && toks.get(k + 1).is_some_and(|i| initials(i)) {
            // The words before the comma are the surname: names or particles, no initials.
            return toks[..k].iter().all(|p| surname(p) || PARTICLES.contains(p));
        }
    }
    // Springer style. Particles may lead ("de Ronde CEJ").
    let mut k = 0;
    while toks.get(k).is_some_and(|w| PARTICLES.contains(w)) {
        k += 1;
    }
    let Some(name) = toks.get(k) else { return false };
    if !surname(name) || name.ends_with(',') || NOT_NAMES.contains(name) {
        return false;
    }
    // A second surname word ("Lo Bello SG").
    let j = if toks.get(k + 1).is_some_and(|w| surname(w) && !w.ends_with(',') && !bare_initials(w)) && toks.get(k + 2).is_some_and(|w| bare_initials(w)) { k + 1 } else { k };
    let Some(ini) = toks.get(j + 1) else { return false };
    if !bare_initials(ini) {
        return false;
    }
    match toks.get(j + 2) {
        None => ini.ends_with(','),
        Some(n) => ini.ends_with(',') || n.starts_with('(') || matches!(*n, "and" | "&" | "et"),
    }
}

/// The first characters of the line make a Japanese name followed by a year.
fn cjk_year(t: &str) -> bool {
    let head: String = t.chars().take(40).collect();
    head.chars().next().is_some_and(|c| is_cjk(c) && !matches!(c, '\u{3000}'..='\u{303F}' | '\u{FF00}'..='\u{FF0F}')) && find_year(&head).is_some()
}

/// Names, a comma and a year as a Japanese reference list sets them
/// ("荒巻重雄，1977：", "気象庁，1978-1991："), not as a citation in a sentence ("山田(2010)は").
fn cjk_bib(t: &str) -> bool {
    let Some(y) = find_year(t) else { return false };
    let name: String = t.chars().take(y).collect();
    let name = name.trim_end();
    y <= 40 && cjk_year(t) && name.chars().all(|c| is_cjk(c) || c.is_whitespace() || matches!(c, ',' | '.' | '&' | 'ー' | '・')) && !name.contains(['(', ')', '（', '）'])
}

/// Initials before the surname: "R. J. Brown, L. Civetta" (Nature, Science).
fn initials_first(t: &str) -> bool {
    let toks: Vec<&str> = t.split_whitespace().take(6).collect();
    let n = toks.iter().take_while(|w| initials(w) && !w.ends_with(',')).count();
    (1..=3).contains(&n) && toks.get(n).is_some_and(|w| surname(w) && w.chars().filter(|c| c.is_lowercase()).count() >= 2) && toks.get(n + 1).is_none_or(|w| matches!(*w, "and" | "&" | "et") || w.starts_with(|c: char| c.is_uppercase()) || w.ends_with(','))
}

/// An organisation (or a name in full) and a year within the first words:
/// "USGS (2010)", "National Research Council, 2010.", "Smith, John A. 2010."
fn named_year(t: &str) -> bool {
    let toks: Vec<&str> = t.split_whitespace().take(9).collect();
    let Some(first) = toks.first() else { return false };
    if !first.chars().next().is_some_and(char::is_uppercase) || NOT_NAMES.contains(first) {
        return false;
    }
    let head = toks.join(" ");
    let Some(y) = find_year(&head) else { return false };
    let before_text: String = head.chars().take(y).collect();
    let before: Vec<&str> = before_text.split_whitespace().collect();
    // Words before the year: capitalised names, abbreviations, particles ("and", "of").
    let words = before.iter().filter(|w| w.chars().any(char::is_alphabetic)).count();
    let lower = before.iter().filter(|w| w.chars().next().is_some_and(char::is_lowercase) && !matches!(**w, "and" | "of" | "for" | "the" | "de" | "van" | "von" | "der" | "et" | "al." | "al")).count();
    words <= 7 && lower <= 1
}

/// "Surname, Firstname" with a year or more names in the line.
fn full_name(t: &str) -> bool {
    let toks: Vec<&str> = t.split_whitespace().take(8).collect();
    let Some(s) = toks.first() else { return false };
    s.ends_with(',')
        && surname(s)
        && !NOT_NAMES.contains(&s.trim_end_matches(','))
        && toks.get(1).is_some_and(|w| surname(w) && w.chars().filter(|c| c.is_alphabetic()).count() >= 3 && !matches!(*w, "The" | "In" | "However" | "For" | "And" | "But" | "This" | "That" | "These" | "See" | "Thus"))
        && (has_year(&toks.join(" ")) || toks.iter().skip(2).any(|w| initials(w) || w.ends_with(',')))
}

/// How the text starts an entry of a reference list.
pub(super) fn lead(t: &str) -> Lead {
    let t = plain(t);
    let t = t.trim_start();
    // A marker in front is not part of the start.
    let t = match marker(t) {
        Some(_) => t.trim_start_matches(|c: char| c == '[' || c == '(' || c.is_ascii_digit()).trim_start_matches([']', ')', '.']).trim_start(),
        None => t,
    };
    if author_start(t) || cjk_bib(t) {
        Lead::Strong
    } else if initials_first(t) || named_year(t) || full_name(t) || cjk_year(t) {
        Lead::Weak
    } else {
        Lead::No
    }
}

/// How firmly a line ends an entry: 2 with a DOI, a URL, a page range and a
/// stop, or a Japanese full stop; 1 with a full stop, a digit or a bracket;
/// 0 otherwise. A stop after an abbreviation ends nothing, hence the two levels.
pub(super) fn entry_end(t: &str) -> u8 {
    let t = plain(t);
    let t = t.trim_end();
    let Some(last) = t.split_whitespace().last() else { return 2 };
    let url = last.contains("://") || last.contains("doi.org") || last.to_lowercase().starts_with("doi:") || (last.starts_with("10.") && last.contains('/')) || last.starts_with("www.");
    let stop = t.ends_with('.');
    let core = last.trim_end_matches(['.', ')']);
    let range = core.rsplit(['-', '–', '—', '−']).next().is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())) && core.contains(['-', '–', '—', '−']);
    let year_end = core.len() >= 4 && find_year(core).is_some() && core.ends_with(|c: char| c.is_ascii_digit() || c.is_ascii_lowercase());
    if url || (stop && (range || year_end)) || t.ends_with("in press.") || t.ends_with(['。', '．']) {
        2
    } else if stop || t.ends_with(|c: char| c.is_ascii_digit() || c == ')') {
        1
    } else {
        0
    }
}

/// A heading that opens a reference section ("References", "Literature Cited", "参考文献").
pub(super) fn is_refs_heading(t: &str) -> bool {
    let t = plain(t);
    let l = t.trim().trim_end_matches([':', '.', '。']).trim();
    // A section number in front: "7.", "VII.", "6 ".
    let l = match l.split_once(char::is_whitespace) {
        Some((n, rest)) if n.trim_end_matches('.').chars().all(|c| c.is_ascii_digit()) && !n.is_empty() || n.trim_end_matches('.').chars().all(|c| matches!(c, 'I' | 'V' | 'X')) && !n.is_empty() => rest.trim(),
        _ => l,
    };
    let compact: String = l.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.chars().count() <= 14 && compact.chars().any(is_cjk) {
        // "参考文献", "引用文献", "文献一覧", "引用・参考文献": no particles in it ("には文献番号").
        let plain_word = !compact.chars().any(|c| ('\u{3040}'..='\u{309F}').contains(&c));
        return plain_word && compact.chars().count() <= 10 && ["文献", "文献一覧", "文献リスト", "文献目録"].iter().any(|e| compact.ends_with(e));
    }
    const NAMES: [&str; 26] = [
        "references",
        "reference",
        "references cited",
        "cited references",
        "literature cited",
        "literature",
        "bibliography",
        "reference list",
        "list of references",
        "works cited",
        "references and notes",
        "notes and references",
        "selected references",
        "selected bibliography",
        "supplementary references",
        "supporting references",
        "literatur",
        "literaturverzeichnis",
        "bibliographie",
        "références",
        "referencias",
        "bibliografía",
        "bibliografia",
        "riferimenti",
        "references cited in the text",
        "literature references",
    ];
    NAMES.contains(&l.to_lowercase().as_str())
}

/// Headings that end a reference section by their words.
fn section_word(t: &str) -> bool {
    let l = t.trim().trim_end_matches([':', '.']).to_lowercase();
    let l = l.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c.is_whitespace());
    ["acknowledg", "appendix", "appendices", "supplementary", "supporting information", "supplement", "data availability", "author contributions", "funding", "conflict", "competing", "declaration", "figure captions", "figure legends", "tables", "glossary", "abbreviations", "nomenclature", "notes", "謝辞", "付録", "附録", "補遺", "注", "脚注", "索引"]
        .iter()
        .any(|w| l.starts_with(w))
}

/// "Manuscript received 14 November 2005", "Revised 20 March 2006", "Accepted ...": the
/// dates a journal prints under its last column.
fn dated_note(t: &str) -> bool {
    let l = t.trim().trim_start_matches(['(', '[']).to_lowercase();
    ["manuscript received", "manuscript accepted", "revised manuscript", "received", "revised", "accepted", "submitted", "published online", "available online"].iter().any(|w| l.starts_with(w)) && find_year(&l).is_some() && l.chars().count() < 120
}

/// Text that reads as a reference (so that it is no heading, whatever the model said).
fn entry_like(t: &str) -> bool {
    let p = plain(t);
    marker(&p).is_some() || lead(&p) != Lead::No || (has_year(&p) && p.chars().count() > 60)
}

// -------------------------------------------------------------------- lines

/// A line of the list with what the segmentation reads from it.
#[derive(Clone)]
struct Ln {
    line: RichLine,
    /// For the pattern tests (see [`plain`]).
    text: String,
    /// Vertical centre of its characters: where it lies down the column.
    key: f32,
    size: f32,
    /// Left and right end of the ink: the box of an OCR line starts at a leading space.
    ink: (f32, f32),
    class: Option<usize>,
    group: Option<usize>,
}

impl Ln {
    fn new(line: RichLine, class: Option<usize>, group: Option<usize>) -> Ln {
        let text = plain(&line.text());
        let size = line_size(&line);
        let mut ys: Vec<f32> = line.chars.iter().filter(|c| !c.c.is_whitespace() && c.size >= size * 0.8).map(|c| (c.bbox.y0 + c.bbox.y1) * 0.5).collect();
        ys.sort_by(f32::total_cmp);
        let key = ys.get(ys.len() / 2).copied().unwrap_or_else(|| (line.bbox.y0 + line.bbox.y1) * 0.5);
        let ink = match (line.chars.iter().find(|c| !c.c.is_whitespace()), line.chars.iter().rev().find(|c| !c.c.is_whitespace())) {
            (Some(a), Some(b)) => (a.bbox.x0, b.bbox.x1),
            _ => (line.bbox.x0, line.bbox.x1),
        };
        Ln { line, text, key, size, ink, class, group }
    }
    fn x0(&self) -> f32 {
        self.ink.0
    }
    fn x1(&self) -> f32 {
        self.ink.1
    }
    fn width(&self) -> f32 {
        self.ink.1 - self.ink.0
    }
}

fn median(mut v: Vec<f32>) -> f32 {
    v.sort_by(f32::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// A line the extractor ran across a gutter (two columns on one baseline), cut where two
/// words stand further apart than two ems. Running text has no such gap; the parts of a
/// line that stays in one column are joined again by [`join_fragments`]. Spaces at the
/// ends are dropped (OCR lines start with one).
fn split_wide_gaps(l: &RichLine) -> Vec<RichLine> {
    if l.vertical || l.chars.is_empty() {
        return vec![l.clone()];
    }
    let size = line_size(l);
    let mut parts: Vec<Vec<RichChar>> = vec![Vec::new()];
    let mut last: Option<RectF> = None;
    for c in &l.chars {
        if !c.c.is_whitespace() {
            if last.is_some_and(|b| c.bbox.x0 - b.x1 > 2.0 * size) {
                parts.push(Vec::new());
            }
            last = Some(c.bbox);
        }
        parts.last_mut().unwrap().push(*c);
    }
    parts
        .into_iter()
        .filter_map(|mut p| {
            while p.first().is_some_and(|c| c.c.is_whitespace()) {
                p.remove(0);
            }
            while p.last().is_some_and(|c| c.c.is_whitespace()) {
                p.pop();
            }
            let first = p.first()?.bbox;
            let bbox = p.iter().skip(1).fold(first, |a, c| a.union(&c.bbox));
            // The part keeps the line's height (superscripts, descenders).
            let bbox = RectF { y0: l.bbox.y0, y1: l.bbox.y1, ..bbox };
            Some(RichLine { bbox, vertical: false, dir: l.dir, joined: l.joined, chars: p })
        })
        .collect()
}

/// The lines in columns, and lines the extractor ran across a gutter (two columns
/// on one baseline) cut at it. A gutter is a band, at least 0.35 em wide, that (nearly)
/// no glyph of any line touches, with a column of at least six ems on each side: a
/// list's number markers, set apart from their text, are no column.
pub(super) fn ink_columns(lines: Vec<(RichLine, Option<usize>, Option<usize>)>) -> Vec<Vec<(RichLine, Option<usize>, Option<usize>)>> {
    let n = lines.len();
    let em = median(lines.iter().map(|l| line_size(&l.0)).collect()).max(1.0);
    let ink = |l: &RichLine| l.chars.iter().filter(|c| !c.c.is_whitespace()).map(|c| c.bbox).collect::<Vec<_>>();
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for (l, ..) in &lines {
        for b in ink(l) {
            lo = lo.min(b.x0);
            hi = hi.max(b.x1);
        }
    }
    if n < 8 || !(hi > lo) {
        return vec![lines];
    }
    // Lines touching each half-point cell.
    let lo = lo.floor();
    let cells = ((hi - lo) * 2.0) as usize + 2;
    let mut cover = vec![0usize; cells];
    for (l, ..) in &lines {
        let mut mine = vec![false; cells];
        for b in ink(l) {
            for m in mine.iter_mut().take(((b.x1 - lo) * 2.0) as usize + 1).skip(((b.x0 - lo) * 2.0) as usize) {
                *m = true;
            }
        }
        for (c, m) in cover.iter_mut().zip(mine) {
            *c += m as usize;
        }
    }
    let thr = n * 2 / 100;
    let mut valleys: Vec<(f32, f32)> = Vec::new();
    let mut c = 0;
    while c < cells {
        if cover[c] > thr {
            c += 1;
            continue;
        }
        let s = c;
        while c < cells && cover[c] <= thr {
            c += 1;
        }
        let (a, b) = (lo + s as f32 / 2.0, lo + c as f32 / 2.0);
        if s > 0 && c < cells - 1 && b - a >= (0.35 * em).max(2.0) {
            valleys.push((a, b));
        }
    }
    // A column needs six ems: a marker column, or a sliver, is no column.
    loop {
        let mut edges = vec![lo];
        edges.extend(valleys.iter().flat_map(|v| [v.0, v.1]));
        edges.push(hi);
        let narrow = (0..=valleys.len()).find(|&k| edges[2 * k + 1] - edges[2 * k] < 6.0 * em);
        match narrow {
            Some(k) if !valleys.is_empty() => {
                // Drop the valley next to the narrow region, the narrower side of the two.
                let left = if k > 0 { valleys[k - 1].1 - valleys[k - 1].0 } else { f32::INFINITY };
                let right = if k < valleys.len() { valleys[k].1 - valleys[k].0 } else { f32::INFINITY };
                valleys.remove(if left <= right { k - 1 } else { k });
            }
            _ => break,
        }
    }
    if valleys.is_empty() {
        valleys = start_cuts(&lines, em);
    }
    if valleys.is_empty() {
        return vec![lines];
    }
    let mut cols: Vec<Vec<(RichLine, Option<usize>, Option<usize>)>> = (0..=valleys.len()).map(|_| Vec::new()).collect();
    for (l, class, group) in lines {
        let mut parts: Vec<Vec<RichChar>> = (0..=valleys.len()).map(|_| Vec::new()).collect();
        let mut k = 0;
        for c in &l.chars {
            if !c.c.is_whitespace() {
                k = valleys.iter().filter(|v| (c.bbox.x0 + c.bbox.x1) * 0.5 >= (v.0 + v.1) * 0.5).count();
            }
            parts[k].push(*c);
        }
        for (k, mut p) in parts.into_iter().enumerate() {
            while p.first().is_some_and(|c| c.c.is_whitespace()) {
                p.remove(0);
            }
            while p.last().is_some_and(|c| c.c.is_whitespace()) {
                p.pop();
            }
            let Some(first) = p.first().map(|c| c.bbox) else { continue };
            // The part keeps the line's height (superscripts, descenders) and its ink from side to side.
            let bbox = p.iter().skip(1).fold(first, |a, c| a.union(&c.bbox));
            let bbox = RectF { y0: l.bbox.y0, y1: l.bbox.y1, ..bbox };
            cols[k].push((RichLine { bbox, vertical: l.vertical, dir: l.dir, joined: l.joined, chars: p }, class, group));
        }
    }
    cols.retain(|c| !c.is_empty());
    cols
}

/// Cuts between columns set so close that the gutter is not free of ink (a few lines
/// run across it): lines that start in two places more than ten ems apart, a fifth of
/// the lines each at least, are two columns, cut just left of where the right one
/// starts, if hardly any line has a glyph there (the tail of each line, in a block of
/// its own, starts inside the line's run).
fn start_cuts(lines: &[(RichLine, Option<usize>, Option<usize>)], em: f32) -> Vec<(f32, f32)> {
    let n = lines.len();
    let x0 = |l: &RichLine| l.chars.iter().find(|c| !c.c.is_whitespace()).map_or(l.bbox.x0, |c| c.bbox.x0);
    let mut starts: Vec<f32> = lines.iter().map(|l| x0(&l.0)).collect();
    starts.sort_by(f32::total_cmp);
    let need = (n / 5).max(3);
    (need..=n.saturating_sub(need))
        .filter(|&k| k > 0 && k < n && starts[k] - starts[k - 1] >= 10.0 * em)
        .map(|k| starts[k] - 0.5)
        .filter(|&c| lines.iter().filter(|l| l.0.chars.iter().any(|g| !g.c.is_whitespace() && g.bbox.x0 < c && c < g.bbox.x1)).count() <= (n / 20).max(2))
        .map(|c| (c, c))
        .collect()
}

/// Fragments of one visual line (a number set apart, a run in another font, a
/// superscript) become one line: same baseline band, side by side, close.
fn join_fragments(mut col: Vec<Ln>, lex: Option<&Lexicon>) -> Vec<Ln> {
    col.sort_by(|a, b| a.key.total_cmp(&b.key).then(a.x0().total_cmp(&b.x0())));
    let n = col.len();
    let mut used = vec![false; n];
    let mut out: Vec<Ln> = Vec::with_capacity(n);
    for i in 0..n {
        if used[i] {
            continue;
        }
        used[i] = true;
        let mut group = vec![col[i].clone()];
        let mut band = col[i].line.bbox;
        for j in i + 1..n {
            if used[j] {
                continue;
            }
            let b = col[j].line.bbox;
            let v = band.y1.min(b.y1) - band.y0.max(b.y0);
            if v >= 0.5 * band.height().min(b.height()) && v > 0.0 {
                used[j] = true;
                band = band.union(&b);
                group.push(col[j].clone());
            }
        }
        group.sort_by(|a, b| a.x0().total_cmp(&b.x0()));
        // Merge neighbours that touch or lie within three ems (a number: eight).
        let mut merged: Vec<Ln> = Vec::new();
        for f in group {
            match merged.last_mut() {
                // Side by side: two lines of one column overlap.
                Some(m) if f.x0() - m.x1() < if marker_only(&m.text) { 8.0 } else { 3.0 } * m.size.max(f.size) && m.x1().min(f.x1()) - m.x0().max(f.x0()) <= 0.3 * m.width().min(f.width()) => *m = fuse(m, f, lex),
                _ => merged.push(f),
            }
        }
        out.extend(merged);
    }
    out.sort_by(|a, b| a.key.total_cmp(&b.key).then(a.x0().total_cmp(&b.x0())));
    out
}

/// The last word of `a` and the first of `b` make a word of the document.
fn joined_word(a: &str, b: &str, lex: Option<&Lexicon>) -> bool {
    let last: String = a.trim_end().chars().rev().take_while(|c| c.is_alphabetic()).collect::<Vec<_>>().into_iter().rev().collect();
    let first: String = b.trim_start().chars().take_while(|c| c.is_alphabetic()).collect();
    lex.is_some_and(|l| !last.is_empty() && !first.is_empty() && l.has_word(&format!("{last}{first}")))
}

/// Two fragments side by side as one line, a space between them where they do not touch.
fn fuse(a: &Ln, b: Ln, lex: Option<&Lexicon>) -> Ln {
    let mut chars = a.line.chars.clone();
    let gap = b.x0() - a.x1();
    if let (Some(l), Some(f)) = (chars.last().copied(), b.line.chars.first()) {
        // Words apart; or, where the fragments touch or overlap between letters of one size,
        // two words unless the document has the word they make together ("Ra" + "nge").
        let same = l.c.is_alphabetic() && f.c.is_alphabetic() && (l.size - f.size).abs() <= 0.15 * l.size.max(f.size);
        let word_break = gap > 0.12 * l.size || (same && !joined_word(&a.text, &b.text, lex));
        if word_break && !l.c.is_whitespace() && !f.c.is_whitespace() {
            chars.push(RichChar { c: ' ', bbox: RectF { x0: a.x1(), y0: l.bbox.y0, x1: b.x0(), y1: l.bbox.y1 }, ..l });
        }
    }
    chars.extend(b.line.chars.iter().copied());
    let line = RichLine { bbox: a.line.bbox.union(&b.line.bbox), vertical: false, dir: a.line.dir, joined: a.line.joined, chars };
    // The wider fragment carries the line's class and position.
    let main = if a.width() >= b.width() { a } else { &b };
    let (class, group, key) = (main.class, main.group, main.key);
    let mut l = Ln::new(line, class, group);
    l.key = key;
    l
}

// ------------------------------------------------------------- segmentation

/// How the entries of a list begin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Style {
    /// A number in front of each entry.
    Numbered,
    /// First line at the column's edge, the rest indented.
    Hanging,
    /// First line indented, the rest at the edge.
    FirstLine,
    /// No indent: authors and stops tell.
    Flush,
}

/// Where a line starts, against the column's edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pos {
    Edge,
    Indent,
    /// Far from the edge (over eight ems).
    Far,
}

/// The entry starts of one column.
struct Cut {
    starts: Vec<bool>,
    style: Style,
    /// The layout shows in the column itself (two positions, many lines).
    sure: bool,
    edge: f32,
    indent: f32,
}

/// Reference-section state carried from page to page.
#[derive(Clone, Default)]
pub(super) struct Refs {
    /// Inside a reference section: after its heading, or a list found by its entries.
    active: bool,
    /// The section was found by its entries, with no heading: it ends with the first page
    /// that has no list.
    by_content: bool,
    style: Option<Style>,
    /// Indent of the hanging or first-line layout, in points.
    indent: f32,
    /// Left edges of the columns of the list.
    edges: Vec<f32>,
    /// Width of a column of the list: text much wider than that is no entry.
    width: Option<f32>,
    /// The entries carry numbers (last: `last_no`).
    numbered: bool,
    last_no: Option<u32>,
    /// The last entry stopped short of its end (its rest may follow in the next column).
    open: bool,
    have_entry: bool,
    /// Type size of the entries.
    size: Option<f32>,
}

impl Refs {
    /// Rebuild the reference lists among the units of a page, given in reading order.
    pub(super) fn page(&mut self, ordered: Vec<Unit>, cx: &Cx) -> Vec<Unit> {
        let mut out: Vec<Unit> = Vec::with_capacity(ordered.len());
        let mut run: Vec<Unit> = Vec::new();
        let mut aside: Vec<Unit> = Vec::new();
        let mut active = self.active;
        let mut touched = active;
        let (mut accepted, mut rejected) = (false, false);
        // Whether entries follow the unit: a heading needs them, or a look of its own.
        let follows: Vec<bool> = (0..ordered.len())
            .map(|i| {
                ordered[i + 1..].iter().filter(|u| u.kind == UnitKind::Text).take(3).any(|u| u.lines.iter().take(3).any(|l| marker(&l.text()).is_some() || lead(&l.text()) == Lead::Strong))
            })
            .collect();
        // Where the section lies on this page once its heading is met here: under the
        // heading or to the right of it, not above it. (The reading order can put text
        // of another column between a heading and its list.)
        let mut flow: Option<RectF> = None;
        let note = |r: Option<bool>, accepted: &mut bool, rejected: &mut bool| match r {
            Some(true) => *accepted = true,
            Some(false) => *rejected = true,
            None => {}
        };
        for (i, u) in ordered.into_iter().enumerate() {
            let u = match split_heading(u, follows[i], cx) {
                Ok((head, rest)) => {
                    note(self.flush(&mut out, &mut run, &mut aside, cx), &mut accepted, &mut rejected);
                    if !active || self.by_content {
                        self.numbered = false;
                        self.last_no = None;
                        self.open = false;
                        self.have_entry = false;
                    }
                    active = true;
                    touched = true;
                    self.by_content = false;
                    if std::env::var("STRATA_DEBUG_REFS_EVENTS").is_ok() {
                        eprintln!("refs: page {} heading {:?}", cx.page + 1, head.text().chars().take(50).collect::<String>());
                    }
                    flow = Some(head.bbox);
                    out.push(head);
                    if let Some(r) = rest {
                        if self.eligible(&r, cx) { run.push(r) } else { aside.push(r) }
                    }
                    continue;
                }
                Err(u) => u,
            };
            let in_flow = active
                && flow.is_none_or(|h| {
                    let b = u.bbox;
                    let beside = b.x1.min(h.x1) - b.x0.max(h.x0) >= 0.3 * b.width().min(h.width());
                    (beside || b.x0 >= h.x1 - 1.0) && !(b.y1 <= h.y0 + 2.0 && b.x0 < h.x1 - 1.0)
                });
            if in_flow && self.ends_section(&u, cx) {
                note(self.flush(&mut out, &mut run, &mut aside, cx), &mut accepted, &mut rejected);
                active = false;
                out.push(u);
            } else if in_flow {
                if self.eligible(&u, cx) { run.push(u) } else { aside.push(u) }
            } else {
                out.push(u);
            }
        }
        note(self.flush(&mut out, &mut run, &mut aside, cx), &mut accepted, &mut rejected);
        self.active = active;
        if !touched {
            let (units, found, at_end) = self.detect(out, cx);
            out = units;
            if std::env::var("STRATA_DEBUG_REFS_EVENTS").is_ok() && found {
                eprintln!("refs: page {} list found by its entries (at end of page: {at_end})", cx.page + 1);
            }
            if found && at_end {
                self.active = true;
                self.by_content = true;
            }
        } else if (self.by_content && !accepted) || (rejected && !accepted) {
            // A section found by its entries ends where a page has none; so does one
            // whose text turns out not to be a list (a table's "References" column).
            self.active = false;
            self.by_content = false;
        }
        out
    }

    /// The end of a run: rebuild it, and let the units set aside follow. Says
    /// whether the run was taken for a list (None: there was none).
    fn flush(&mut self, out: &mut Vec<Unit>, run: &mut Vec<Unit>, aside: &mut Vec<Unit>, cx: &Cx) -> Option<bool> {
        let units = std::mem::take(run);
        let mut taken = None;
        if !units.is_empty() {
            taken = Some(false);
            let saved = self.clone();
            match self.segment(units, cx, false) {
                Ok((before, rebuilt, after)) => {
                    out.extend(before);
                    out.extend(rebuilt);
                    aside.extend(after);
                    taken = Some(true);
                }
                Err(units) => {
                    *self = saved;
                    out.extend(units);
                }
            }
        }
        out.append(aside);
        taken
    }

    /// Lists outside any section: a group of units in one column whose lines read as
    /// entries (numbered, or with authors and years) is rebuilt like a section's
    /// run. Returns the units, whether a list was found, and whether it is the last
    /// text of the page (the section then goes on over the pages that follow).
    fn detect(&mut self, units: Vec<Unit>, cx: &Cx) -> (Vec<Unit>, bool, bool) {
        // A list found here has nothing to do with the entries of an earlier one.
        self.numbered = false;
        self.last_no = None;
        self.open = false;
        self.have_entry = false;
        let candidate = |u: &Unit| self.eligible(u, cx) && !matches!(u.class, Some(TITLE | SECTION_HEADER));
        let idx: Vec<usize> = (0..units.len()).filter(|&i| candidate(&units[i])).collect();
        // Units side by side in one column, transitively.
        let mut groups: Vec<(Vec<usize>, f32, f32)> = Vec::new();
        for &i in &idx {
            let b = units[i].bbox;
            let hit = groups.iter().position(|g| b.x1.min(g.2) - b.x0.max(g.1) >= 0.3 * b.width().min(g.2 - g.1));
            match hit {
                Some(k) => {
                    groups[k].0.push(i);
                    groups[k].1 = groups[k].1.min(b.x0);
                    groups[k].2 = groups[k].2.max(b.x1);
                }
                None => groups.push((vec![i], b.x0, b.x1)),
            }
        }
        groups.sort_by(|a, b| a.1.total_cmp(&b.1));
        let last = idx.last().copied();
        let mut slots: Vec<Option<Unit>> = units.into_iter().map(Some).collect();
        let mut made: Vec<(usize, Vec<Unit>)> = Vec::new();
        let (mut found, mut at_end) = (false, false);
        for (members, ..) in groups {
            // Entries have leads or markers: a first look before the work.
            let hints = members.iter().flat_map(|&i| slots[i].iter().flat_map(|u| u.lines.iter())).filter(|l| marker(&l.text()).is_some() || lead(&l.text()) != Lead::No).count();
            if hints < 3 {
                continue;
            }
            // Only the units in the list's type size; the others stay where they are.
            let all: Vec<&Unit> = members.iter().filter_map(|&i| slots[i].as_ref()).collect();
            let (_, keep) = self.same_size(&all, cx);
            let members: Vec<usize> = members.into_iter().zip(keep).filter(|(_, k)| *k).map(|(i, _)| i).collect();
            if members.is_empty() {
                continue;
            }
            let group: Vec<Unit> = members.iter().filter_map(|&i| slots[i].take()).collect();
            let saved = self.clone();
            match self.segment(group, cx, true) {
                Ok((before, rebuilt, after)) => {
                    let mut all = before;
                    all.extend(rebuilt);
                    all.extend(after);
                    made.push((members[0], all));
                    found = true;
                    at_end |= last.is_some_and(|l| members.contains(&l));
                }
                Err(group) => {
                    *self = saved;
                    for (&i, u) in members.iter().zip(group) {
                        slots[i] = Some(u);
                    }
                }
            }
        }
        let mut out: Vec<Unit> = Vec::with_capacity(slots.len());
        for (i, slot) in slots.into_iter().enumerate() {
            if let Some(pos) = made.iter().position(|m| m.0 == i) {
                out.extend(made.remove(pos).1);
            }
            out.extend(slot);
        }
        (out, found, at_end)
    }

    /// Can the unit be part of the list's text (not a float, a heading or a caption)?
    /// The model takes some lists for tables, figure text, captions or page footers:
    /// they are lists if their lines read as entries (and, as footers, lie in the body).
    fn eligible(&self, u: &Unit, cx: &Cx) -> bool {
        if u.kind != UnitKind::Text || u.lines.is_empty() || caption_kind(&u.text()).is_some() {
            return false;
        }
        if !matches!(u.class, Some(TABLE | PICTURE | CAPTION | PAGE_HEADER | PAGE_FOOTER | FORMULA)) {
            return true;
        }
        let margin = matches!(u.class, Some(PAGE_HEADER | PAGE_FOOTER)) && (u.bbox.y1 < cx.height * 0.12 || u.bbox.y0 > cx.height * 0.88);
        !margin && u.lines.iter().filter(|l| lead(&l.text()) != Lead::No || has_year(&l.text())).count() * 4 >= u.lines.len()
    }

    /// A heading (not a reference heading) closes the section. Headings from the
    /// layout model count when their type stands out or their words say so:
    /// entries the model took for headings do not end the list.
    fn ends_section(&self, u: &Unit, cx: &Cx) -> bool {
        if u.kind != UnitKind::Text || u.lines.is_empty() {
            return false;
        }
        let t = u.text();
        let t = t.trim();
        let wordy = t.split_whitespace().any(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 2) || t.chars().filter(|&c| is_cjk(c)).count() >= 2;
        if u.lines.len() > 3 || t.chars().count() > 120 || !wordy || entry_like(t) {
            return false;
        }
        let size = u.size();
        let list = self.size.unwrap_or(cx.body);
        let bold = u.frac(|c| c.bold || cx.fonts.get(c.font as usize).is_some_and(|f| super::font_bold(f)));
        let stands_out = size >= list * 1.08 || bold >= 0.8 || caps_heading(t) || heading_number(t).is_some();
        let named = section_word(t) && u.lines.len() == 1;
        (matches!(u.class, Some(TITLE | SECTION_HEADER)) && (stands_out || named)) || (named && (stands_out || t.chars().count() <= 40)) || (caps_heading(t) && u.lines.len() == 1 && size >= list * 0.95)
    }

    /// The type size of the list, and which units are set in it (within 15%, 30% on scans).
    fn same_size(&self, units: &[&Unit], cx: &Cx) -> (f32, Vec<bool>) {
        let size = self.list_size(units);
        let tol = if cx.scan { 0.3 } else { 0.15 };
        (size, units.iter().map(|u| (u.size() - size).abs() <= size * tol).collect())
    }

    /// Type size of the list: the learned one, else the one carrying most characters.
    fn list_size(&self, units: &[&Unit]) -> f32 {
        if let Some(s) = self.size
            && units.iter().any(|u| (u.size() - s).abs() <= s * 0.15)
        {
            return s;
        }
        let mut hist: HashMap<i32, usize> = HashMap::new();
        for u in units.iter() {
            *hist.entry((u.size() * 2.0).round() as i32).or_default() += u.chars();
        }
        hist.into_iter().max_by_key(|e| (e.1, e.0)).map_or(10.0, |e| e.0 as f32 / 2.0)
    }

    /// Cut the lines of the units into entries. Err(units) hands the units back
    /// when the result does not look like a reference list.
    fn segment(&mut self, units: Vec<Unit>, cx: &Cx, strict: bool) -> Result<(Vec<Unit>, Vec<Unit>, Vec<Unit>), Vec<Unit>> {
        let (size, keep) = self.same_size(&units.iter().collect::<Vec<_>>(), cx);
        let mut raw: Vec<(RichLine, Option<usize>, Option<usize>)> = Vec::new();
        let mut notes: Vec<Unit> = Vec::new();
        // Publisher notes, and the dates of the manuscript: one note, top to bottom.
        let mut dated: Vec<(RichLine, Option<usize>, Option<usize>)> = Vec::new();
        for (u, _) in units.iter().zip(&keep).filter(|(_, k)| **k) {
            // A licence or copyright notice ends the unit ("Printed in the USA" under the
            // last entry) or is the unit (a paragraph: keep it whole).
            let boiler = u.lines.iter().position(|l| !l.vertical && is_boilerplate(&plain(&l.text())));
            let notice_from = boiler.map(|k| if k <= 3 && u.lines.len() <= 10 { 0 } else { k });
            if let Some(k) = notice_from {
                let lines: Vec<RichLine> = u.lines[k..].to_vec();
                let bbox = lines.iter().skip(1).fold(lines[0].bbox, |a, l| a.union(&l.bbox));
                notes.push(Unit { kind: UnitKind::Text, bbox, lines, class: u.class, group: u.group, refs: Ref::No });
            }
            for l in &u.lines[..notice_from.unwrap_or(u.lines.len())] {
                if l.chars.iter().all(|c| c.c.is_whitespace()) {
                    continue;
                }
                let t = plain(&l.text());
                if l.vertical {
                    notes.push(Unit { kind: UnitKind::Text, bbox: l.bbox, lines: vec![l.clone()], class: u.class, group: u.group, refs: Ref::No });
                } else if dated_note(&t) {
                    dated.push((l.clone(), u.class, u.group));
                } else {
                    raw.extend(split_wide_gaps(l).into_iter().map(|p| (p, u.class, u.group)));
                }
            }
        }
        if !dated.is_empty() {
            dated.sort_by(|a, b| a.0.bbox.y0.total_cmp(&b.0.bbox.y0).then(a.0.bbox.x0.total_cmp(&b.0.bbox.x0)));
            let (class, group) = (dated[0].1, dated[0].2);
            let lines: Vec<RichLine> = dated.into_iter().map(|d| d.0).collect();
            let bbox = lines.iter().skip(1).fold(lines[0].bbox, |a, l| a.union(&l.bbox));
            notes.push(Unit { kind: UnitKind::Text, bbox, lines, class, group, refs: Ref::No });
        }
        if raw.is_empty() {
            return Err(units);
        }
        let mut cols: Vec<Vec<Ln>> = Vec::new();
        for col in ink_columns(raw) {
            let mut lines: Vec<Ln> = Vec::new();
            for (l, class, group) in col {
                let ln = Ln::new(l, class, group);

                if self.width.is_some_and(|w| ln.width() > 1.7 * w) {
                    notes.push(unit_of(vec![ln], Ref::No));
                } else {
                    lines.push(ln);
                }
            }
            cols.push(join_fragments(lines, cx.lex));
        }
        cols.retain(|c| !c.is_empty());
        if cols.is_empty() {
            return Err(units);
        }
        let numbered = self.numbering(&cols);
        let debug = std::env::var("STRATA_DEBUG_REFS").ok().and_then(|v| v.parse::<u32>().ok()) == Some(cx.page + 1);
        let mut entries: Vec<(Vec<Ln>, Ref)> = Vec::new();
        let mut sure = false;
        let mut styles: Vec<Style> = Vec::new();
        for (ci, col) in cols.into_iter().enumerate() {
            let cut = self.cut(&col, numbered, cx);
            sure |= cut.sure;
            styles.push(cut.style);
            if debug {
                eprintln!("column {ci}: {} lines, {:?}, edge {:.1}, indent {:.1}, sure {}", col.len(), cut.style, cut.edge, cut.indent, cut.sure);
                for (l, s) in col.iter().zip(&cut.starts) {
                    eprintln!("  {} x0={:.1} x1={:.1} y={:.1} | {}", if *s { "S" } else { " " }, l.x0(), l.x1(), l.key, l.text.chars().take(70).collect::<String>());
                }
            }
            let mut cur: Vec<Ln> = Vec::new();
            let mut role = Ref::Entry;
            for (l, s) in col.into_iter().zip(cut.starts.iter().copied()) {
                if s && !cur.is_empty() {
                    entries.push((std::mem::take(&mut cur), role));
                    role = Ref::Entry;
                } else if cur.is_empty() && !s {
                    role = Ref::Cont;
                }
                cur.push(l);
            }
            if !cur.is_empty() {
                entries.push((cur, role));
            }
        }
        if !self.accept(&entries, sure, strict) {
            return Err(units);
        }
        if std::env::var("STRATA_DEBUG_REFS_EVENTS").is_ok() {
            eprintln!("refs: page {} taken: {} entries, columns {:?}", cx.page + 1, entries.len(), styles);
        }
        self.size = Some(size);
        let rebuilt: Vec<Unit> = entries.into_iter().map(|(lines, role)| unit_of(lines, role)).collect();
        // Units of another type size lie before the list or after it.
        let first = keep.iter().position(|&k| k).unwrap_or(0);
        let (mut before, mut after): (Vec<Unit>, Vec<Unit>) = (Vec::new(), Vec::new());

        for (i, (u, k)) in units.into_iter().zip(keep).enumerate() {
            if !k {
                if i < first { before.push(u) } else { after.push(u) }
            }
        }
        after.extend(notes);
        Ok((before, rebuilt, after))
    }

    /// Does the cut look like a reference list? Inside a section (`strict` false)
    /// some of the entries must read as references (a year, an author or a number
    /// in front); otherwise most of them, and short ones.
    fn accept(&self, entries: &[(Vec<Ln>, Ref)], sure: bool, strict: bool) -> bool {
        // Text, not the numbers of a table.
        let (mut letters, mut all) = (0usize, 0usize);
        for l in entries.iter().flat_map(|e| e.0.iter()) {
            letters += l.text.chars().filter(|c| c.is_alphabetic()).count();
            all += l.text.chars().filter(|c| !c.is_whitespace()).count();
        }
        if letters * 10 < all * 6 {
            return false;
        }
        let real: Vec<String> = entries.iter().filter(|e| e.1 == Ref::Entry).map(|e| e.0.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ")).collect();
        let n = real.len();
        if n == 0 {
            // Only the rest of an entry: it must end like one.
            let text: String = entries.iter().flat_map(|e| e.0.iter()).map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ");
            return !strict && (has_year(&text) || entries.iter().any(|e| e.0.iter().any(|l| entry_end(&l.text) == 2)));
        }
        let years = real.iter().filter(|t| has_year(t)).count();
        // Openings only a reference has: a number, or authors in the form of a list.
        let leads = real.iter().filter(|t| marker(t).is_some() || lead(t) == Lead::Strong).count();
        if strict {
            // Body text with citations has years and names too, but in paragraphs.
            let lines: usize = entries.iter().filter(|e| e.1 == Ref::Entry).map(|e| e.0.len()).sum();
            // Text before the first entry is not a list's: a numbered point in an argument follows its paragraph.
            let rest: usize = entries.iter().filter(|e| e.1 == Ref::Cont).map(|e| e.0.len()).sum();
            n >= 3 && years * 10 >= n * 7 && leads * 10 >= n * 6 && lines <= n * 8 && rest * 10 <= (lines + rest) * 3
        } else {
            // A layout that shows two positions in many lines counts for a quarter of the
            // evidence; a tail of one or two entries needs one year or one author.
            years * 10 >= n * 4 || leads * 10 >= n * 4 || (sure && (years * 4 >= n || leads * 4 >= n)) || (n < 3 && (years >= 1 || leads >= 1))
        }
    }

    /// Whether the lines of the page carry a running list of numbers: at least
    /// three markers counting up, or one continuing the list before.
    fn numbering(&self, cols: &[Vec<Ln>]) -> bool {
        let nums: Vec<u32> = cols.iter().flatten().filter_map(|l| marker(&l.text)).collect();
        let mut prev = self.last_no;
        let mut hits = 0;
        for &n in &nums {
            if prev.is_none_or(|p| n == p + 1) {
                hits += 1;
            }
            prev = Some(n);
        }
        !nums.is_empty() && hits * 2 >= nums.len() && (hits >= 3 || (self.numbered && hits >= 1) || (self.last_no.is_some() && hits >= 1))
    }

    /// The entry starts of one column: by marker, by indent (learned from this
    /// column if it shows the layout, else from the ones before), else by author.
    fn cut(&mut self, col: &[Ln], numbered: bool, cx: &Cx) -> Cut {
        let n = col.len();
        let em = median(col.iter().map(|l| l.size).collect()).max(1.0);
        let (tol, ind) = if cx.scan { (0.4 * em, 0.8 * em) } else { (0.2 * em, 0.4 * em) };
        if numbered {
            let mut starts = Vec::with_capacity(n);
            for l in col {
                let ok = marker(&l.text).is_some_and(|k| self.last_no.is_none_or(|p| k == 1 || (k >= p && k <= p + 10)));
                if ok {
                    self.last_no = marker(&l.text);
                }
                starts.push(ok);
            }
            self.numbered = true;
            self.have_entry = true;
            self.open = col.last().is_some_and(|l| entry_end(&l.text) == 0);
            return Cut { starts, style: Style::Numbered, sure: true, edge: 0.0, indent: 0.0 };
        }
        // The column's left edge: the leftmost x with several lines on it.
        let mut xs: Vec<f32> = col.iter().map(Ln::x0).collect();
        xs.sort_by(f32::total_cmp);
        let mut clusters: Vec<(f32, usize)> = Vec::new();
        for x in xs {
            match clusters.last_mut() {
                Some(c) if x - c.0 <= tol => c.1 += 1,
                _ => clusters.push((x, 1)),
            }
        }
        let need = if n <= 2 { 1 } else { 2.max(n * 12 / 100) };
        let mut edge = clusters.iter().find(|c| c.1 >= need).unwrap_or(&clusters[0]).0;
        // A column whose lines all sit at one x, indented against the list's edge:
        // the tail of an entry (or two) carried over from the column before.
        if clusters.len() == 1
            && n <= 6
            && self.style == Some(Style::Hanging)
            && let Some(e) = self.edges.iter().find(|e| ((edge - **e) - self.indent).abs() <= tol * 1.5)
        {
            edge = *e;
        }
        let pos: Vec<Pos> = col
            .iter()
            .map(|l| {
                let d = l.x0() - edge;
                if d < ind {
                    Pos::Edge
                } else if d <= 8.0 * em {
                    Pos::Indent
                } else {
                    Pos::Far
                }
            })
            .collect();
        let e_n = pos.iter().filter(|p| **p == Pos::Edge).count();
        let i_n = pos.iter().filter(|p| **p == Pos::Indent).count();
        let indent = median(col.iter().zip(&pos).filter(|(_, p)| **p == Pos::Indent).map(|(l, _)| l.x0() - edge).collect());
        // What line comes before: how it ended.
        let ended = |i: usize| if i == 0 { if self.open { 0 } else { 2 } } else { entry_end(&col[i - 1].text) };
        let shaped = e_n >= 2 && i_n >= 2 && (e_n + i_n) * 10 >= n * 7;
        let style = if shaped {
            // Hanging: the edge lines follow a line that ended and open with an author;
            // first-line: the indented ones do.
            let rate = |p: Pos| {
                let idx: Vec<usize> = (0..n).filter(|&i| pos[i] == p).collect();
                let k = idx.len().max(1) as f32;
                let leads = idx.iter().filter(|&&i| lead(&col[i].text) == Lead::Strong).count() as f32;
                let ends = idx.iter().filter(|&&i| ended(i) >= 1).count() as f32;
                (leads + ends) / k
            };
            if rate(Pos::Edge) - rate(Pos::Indent) < -0.15 { Style::FirstLine } else { Style::Hanging }
        } else if i_n == 0 && n >= 4 {
            Style::Flush
        } else if let Some(s) = self.style.filter(|s| *s != Style::Numbered) {
            s
        } else if e_n >= 1 && i_n >= 1 {
            // Too few lines to tell: whichever position holds an author.
            let author = |p: Pos| (0..n).any(|i| pos[i] == p && lead(&col[i].text) == Lead::Strong);
            match (author(Pos::Edge), author(Pos::Indent)) {
                (true, false) => Style::Hanging,
                (false, true) => Style::FirstLine,
                _ => Style::Flush,
            }
        } else {
            Style::Flush
        };
        // Line pitch inside an entry: the smallest distance that lines keep, twice at least;
        // a gap much larger than that separates entries.
        let mut steps: Vec<f32> = (1..n).map(|i| col[i].key - col[i - 1].key).filter(|d| *d > 0.3 * em).collect();
        steps.sort_by(f32::total_cmp);
        let pitch = steps.iter().find(|&&d| steps.iter().filter(|&&e| e >= d && e <= d * 1.12).count() >= 2).copied().unwrap_or_else(|| median(steps.clone()));
        let gap = |i: usize| i > 0 && n >= 4 && pitch > 0.0 && col[i].key - col[i - 1].key > pitch * 1.3;
        // Entries set apart by space alone: the gaps come again and again.
        let spaced = (1..n).filter(|&i| gap(i)).count() >= 2;
        let mut starts = Vec::with_capacity(n);
        for i in 0..n {
            let l = lead(&col[i].text);
            let e = ended(i);
            // A line at the entry position that neither opens like an entry nor follows the
            // end of one continues its entry (the extractor's or an OCR page's stray indent);
            // so does the text of a note that follows the list.
            let opens = l != Lead::No || e >= 1 || gap(i);
            let s = match style {
                Style::Hanging => pos[i] == Pos::Edge && opens,
                Style::FirstLine => pos[i] == Pos::Indent && opens,
                _ => (l == Lead::Strong && (e >= 1 || gap(i))) || (l == Lead::Weak && (e == 2 || (e >= 1 && gap(i)))) || (gap(i) && (spaced || e >= 1 || l != Lead::No)),
            };
            // The first line of a column: with no indent or author to go by, an
            // entry still open goes on; with nothing before it, it starts one.
            let s = if i == 0 {
                match style {
                    Style::Hanging | Style::FirstLine => s || !self.have_entry,
                    _ => l != Lead::No || !self.open || !self.have_entry,
                }
            } else {
                s
            };
            starts.push(s);
        }
        let sure = shaped && n >= 6;
        if sure {
            self.style = Some(style);
            self.indent = indent;
            self.width = Some(median(col.iter().map(Ln::width).filter(|w| *w >= 0.6 * col.iter().map(Ln::width).fold(0.0, f32::max)).collect()));
            if !self.edges.iter().any(|e| (e - edge).abs() <= tol * 1.5) {
                self.edges.push(edge);
                if self.edges.len() > 6 {
                    self.edges.remove(0);
                }
            }
        }
        self.have_entry = true;
        self.open = col.last().is_some_and(|l| entry_end(&l.text) == 0);
        Cut { starts, style, sure, edge, indent }
    }
}

/// A unit made of lines.
fn unit_of(lines: Vec<Ln>, refs: Ref) -> Unit {
    let (class, group) = (lines[0].class, lines[0].group);
    let bbox = lines.iter().skip(1).fold(lines[0].line.bbox, |a, l| a.union(&l.line.bbox));
    Unit { kind: UnitKind::Text, bbox, lines: lines.into_iter().map(|l| l.line).collect(), class, group, refs }
}

/// A heading looks like one: set in the layout model's heading class, in capitals, in bold or larger.
fn heading_looks(class: Option<usize>, text: &str, lines: &[RichLine], cx: &Cx) -> bool {
    let chars = lines.iter().flat_map(|l| l.chars.iter()).filter(|c| !c.c.is_whitespace());
    let (n, bold) = chars.fold((0usize, 0usize), |(n, b), c| (n + 1, b + (c.c != ' ' && (c.bold || cx.fonts.get(c.font as usize).is_some_and(super::font_bold))) as usize));
    let size = median(lines.iter().map(line_size).collect());
    matches!(class, Some(TITLE | SECTION_HEADER)) || caps_heading(text) || (n > 0 && bold * 10 >= n * 6) || size >= cx.body * 1.05
}

/// A unit that is, or opens with, the heading of a reference section (a block the
/// extractor did not split): the heading and the rest apart. The words alone do not
/// make a heading ("References" heads a column of a table): it must look like one, or
/// entries must follow.
fn split_heading(mut u: Unit, follows: bool, cx: &Cx) -> Result<(Unit, Option<Unit>), Unit> {
    if u.kind != UnitKind::Text || u.lines.is_empty() {
        return Err(u);
    }
    if u.lines.len() <= 2 && is_refs_heading(&u.text()) {
        if u.class != Some(TABLE) && (follows || heading_looks(u.class, &u.text(), &u.lines, cx)) {
            return Ok((u, None));
        }
        return Err(u);
    }
    if u.lines.len() >= 2 && is_refs_heading(&u.lines[0].text()) {
        let entries = u.lines[1..].iter().take(3).any(|l| marker(&l.text()).is_some() || lead(&l.text()) == Lead::Strong);
        if u.class != Some(TABLE) && (entries || heading_looks(u.class, &u.lines[0].text(), &u.lines[..1], cx)) {
            let rest = u.lines.split_off(1);
            let bbox = rest.iter().skip(1).fold(rest[0].bbox, |a, l| a.union(&l.bbox));
            let head = Unit { kind: UnitKind::Text, bbox: u.lines[0].bbox, lines: u.lines, class: u.class, group: u.group, refs: Ref::No };
            return Ok((head, Some(Unit { kind: UnitKind::Text, bbox, lines: rest, class: u.class, group: u.group, refs: Ref::No })));
        }
    }
    Err(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line of `text` from `x0`, top `y0`, size 8: 4 pt per character.
    fn line(text: &str, x0: f32, y0: f32) -> RichLine {
        let mut x = x0;
        let chars: Vec<RichChar> = text
            .chars()
            .map(|c| {
                let bbox = RectF { x0: x, y0, x1: x + 4.0, y1: y0 + 8.0 };
                x += 4.0;
                RichChar { c, bbox, size: 8.0, font: 0, bold: false, argb: 0 }
            })
            .collect();
        RichLine { bbox: RectF { x0, y0, x1: x, y1: y0 + 8.0 }, vertical: false, dir: [1.0, 0.0], joined: false, chars }
    }

    /// One line from several pieces: (text, x0), 4 pt per character, spaces between the pieces left to the gap.
    fn pieces(parts: &[(&str, f32)], y0: f32) -> RichLine {
        let mut chars: Vec<RichChar> = Vec::new();
        for (t, x0) in parts {
            let mut x = *x0;
            for c in t.chars() {
                chars.push(RichChar { c, bbox: RectF { x0: x, y0, x1: x + 4.0, y1: y0 + 8.0 }, size: 8.0, font: 0, bold: false, argb: 0 });
                x += 4.0;
            }
        }
        let bbox = RectF { x0: chars[0].bbox.x0, y0, x1: chars.last().unwrap().bbox.x1, y1: y0 + 8.0 };
        RichLine { bbox, vertical: false, dir: [1.0, 0.0], joined: false, chars }
    }

    fn cx() -> Cx<'static> {
        Cx { page: 0, body: 9.0, height: 800.0, scan: false, fonts: &[], lex: None }
    }

    /// Lines of a column as text with the start flags of `cut` ("S" or ".").
    fn cut_of(refs: &mut Refs, ls: &[(&str, f32, f32)]) -> String {
        let col: Vec<Ln> = ls.iter().map(|(t, x, y)| Ln::new(line(t, *x, *y), None, None)).collect();
        let numbered = refs.numbering(&[col.clone()]);
        refs.cut(&col, numbered, &cx()).starts.iter().map(|s| if *s { 'S' } else { '.' }).collect()
    }

    #[test]
    fn markers() {
        assert_eq!(marker("[12] Smith, J."), Some(12));
        assert_eq!(marker("12. Smith, J."), Some(12));
        assert_eq!(marker("3) Smith"), Some(3));
        assert_eq!(marker("(4) Smith"), Some(4));
        assert_eq!(marker("12."), Some(12));
        assert_eq!(marker("2010. Title"), None);
        assert_eq!(marker("12.5 mm"), None);
        assert_eq!(marker("12 Smith"), None);
        assert_eq!(marker("12 Smith, J., 2010"), Some(12));
        assert_eq!(marker("Smith 12."), None);
        assert!(marker_only("[3]"));
        assert!(!marker_only("3. Smith"));
    }

    #[test]
    fn author_starts() {
        // Comma after the surname, initials with stops.
        assert_eq!(lead("Almendros, J., Wilcock, W., Soule, D."), Lead::Strong);
        assert_eq!(lead("Av´e Lallemant, H.G., Oldow, J.S., 2000."), Lead::Strong);
        assert_eq!(lead("Amante, C., and Eakins, B.W., 2009, ETOPO1"), Lead::Strong);
        // Springer: no comma after the surname, no stops.
        assert_eq!(lead("Angerer T, Hagemann SG, Danyushevsky LV (2013) Geochemical"), Lead::Strong);
        assert_eq!(lead("Bell A (2010) Title of the paper"), Lead::Strong);
        // Initials with stops, no comma after the surname.
        assert_eq!(lead("Aki K., Richards P., 2002: Quantitative seismology."), Lead::Strong);
        assert_eq!(lead("Bonali F., 2013: Earthquake-induced static stress"), Lead::Strong);
        assert_eq!(lead("de Ronde CEJ, Stucker VK (2015) Seafloor"), Lead::Strong);
        // With a marker in front.
        assert_eq!(lead("1. Audhkhasi, P. & Singh, S. C. Discovery"), Lead::Strong);
        assert_eq!(lead("[12] Kelley, K.A., Plank, T., 2006"), Lead::Strong);
        // Initials first.
        assert_eq!(lead("R. J. Brown, L. Civetta, I. Arienzo"), Lead::Weak);
        // Organisations and Japanese names with a year.
        assert_eq!(lead("USGS (2010) Volcano hazards"), Lead::Weak);
        assert_eq!(lead("National Research Council, 2010. Title"), Lead::Weak);
        assert_eq!(lead("荒巻重雄，1977：東伊豆単成火山群の地質"), Lead::Strong);
        assert_eq!(lead("気象庁，1978－1991：気象庁地震月報"), Lead::Strong);
        assert_eq!(lead("勝間田明男，1993＝ベッセルディジタル"), Lead::Strong);
        assert_eq!(lead("山田(2010)は次のように述べた"), Lead::Weak);
        // Not entries.
        assert_eq!(lead("Geophysical investigation of rifting and volcanism"), Lead::No);
        assert_eq!(lead("In contrast, the northern part"), Lead::No);
        assert_eq!(lead("Table S1 lists the samples"), Lead::No);
        assert_eq!(lead("Figure A, the map"), Lead::No);
        assert_eq!(lead("However, Kim reported"), Lead::No);
        assert_eq!(lead("Geophys. J. Int., 12, 34-56."), Lead::No);
    }

    #[test]
    fn entry_ends() {
        assert_eq!(entry_end("... Geochem. Geophys. Geosyst., v. 16, p. 681–704, https://doi.org/10.1002/2014GC005627."), 2);
        assert_eq!(entry_end("... doi:10.1029/2001GC000252"), 2);
        assert_eq!(entry_end("Nature 421, 123–130 (2003)."), 2);
        assert_eq!(entry_end("Phys. Earth, 39, 197–218."), 2);
        assert_eq!(entry_end("Geophys. J. Int."), 1);
        assert_eq!(entry_end("Nature 495, 356"), 1);
        assert_eq!(entry_end("Hydrothermal Systems of the"), 0);
        assert_eq!(entry_end("Kelley, K. A., Plank, T.,"), 0);
        assert_eq!(entry_end("地震研究所彙報，52，235－278．"), 2);
    }

    #[test]
    fn headings() {
        assert!(is_refs_heading("References"));
        assert!(is_refs_heading("REFERENCES CITED"));
        assert!(is_refs_heading("7. References"));
        assert!(is_refs_heading("VII. REFERENCES"));
        assert!(is_refs_heading("Literature Cited"));
        assert!(is_refs_heading("参考文献"));
        assert!(is_refs_heading("参 考 文 献"));
        assert!(is_refs_heading("引用文献"));
        assert!(is_refs_heading("参考文献一覧"));
        assert!(!is_refs_heading("には文献番号"));
        assert!(!is_refs_heading("文献を参照"));
        assert!(!is_refs_heading("References to the table are given"));
        assert!(!is_refs_heading("Introduction"));
        assert!(!is_refs_heading("1. Introduction"));
    }

    #[test]
    fn dated_notes() {
        assert!(dated_note("Manuscript received 14 November 2005"));
        assert!(dated_note("Revised manuscript received 20 March 2006"));
        assert!(dated_note("Accepted 3 May 2010."));
        assert!(dated_note("(Received October 14, 1997; revised January 13, 1998;"));
        assert!(!dated_note("Smith, J., 2010, Accepted models of flow"));
    }

    #[test]
    fn columns_from_ink() {
        // Two columns whose lines the extractor ran into one, a 6 pt gutter between.
        let left = "Bird, P., 2003, An updated digital model of plate";
        let right = "Fryer, P., 2012, Serpentinite mud volcanism: obser";
        // The word gaps differ from line to line, as in running text.
        let shifted = |t: &str, i: usize| -> String { t.chars().cycle().skip(i * 5).take(t.len()).collect() };
        let fused: Vec<_> = (0..10).map(|i| (pieces(&[(&shifted(left, i), 50.0), (&shifted(right, i), 50.0 + 4.0 * left.len() as f32 + 6.0)], 100.0 + 10.0 * i as f32), None, None)).collect();
        let cols = ink_columns(fused);
        assert_eq!(cols.len(), 2);
        assert!(cols.iter().all(|c| c.len() == 10));
        // Number markers 12 pt in front of their text are no column.
        let numbered: Vec<_> = (0..10).map(|i| (pieces(&[("12.", 50.0), (&shifted(left, i), 74.0)], 100.0 + 10.0 * i as f32), None, None)).collect();
        assert_eq!(ink_columns(numbered).len(), 1);
    }

    #[test]
    fn fragments_join() {
        // A number and its text on one baseline, the text a little higher: one line.
        let a = Ln::new(pieces(&[("7.", 50.0)], 100.0), None, None);
        let b = Ln::new(pieces(&[("Parker, R. L. & Oldenburg, D. W. Thermal model", 74.0)], 98.5), None, None);
        let c = Ln::new(pieces(&[("of ocean ridges. Nat. Phys. Sci. 242,", 74.0)], 110.0), None, None);
        let joined = join_fragments(vec![c, b, a], None);
        assert_eq!(joined.len(), 2);
        assert!(joined[0].text.starts_with("7. Parker"));
        assert!(joined[1].text.starts_with("of ocean"));
    }

    #[test]
    fn hanging_indent() {
        let mut r = Refs::default();
        let col = [
            ("Almendros, J., Wilcock, W., 2001, Title of the paper. Journal 1, 2-3.", 50.0, 100.0),
            ("Bird, P., 2003, An updated digital model of plate boundaries:", 50.0, 110.0),
            ("Geochemistry, Geophysics, Geosystems, v. 4, 1027.", 58.0, 120.0),
            ("Fryer, P., 2012, Serpentinite mud volcanism: observations,", 50.0, 130.0),
            ("processes, and implications: Annual Review, v. 4, 345-373.", 58.0, 140.0),
            ("Hall, P.S., and Kincaid, C., 2001, Diapiric flow at subduction", 50.0, 150.0),
            ("zones: A recipe for rapid transport: Science, 292, 2472.", 58.0, 160.0),
        ];
        assert_eq!(cut_of(&mut r, &col), "SS.S.S.");
        assert_eq!(r.style, Some(Style::Hanging));
        // The next column, an entry's tail then two entries: the learned indent decides.
        let tail = [("continued text of the entry, 2010.", 58.0, 100.0), ("Kelley, K.A., 2006, Mantle melting. Journal 1, 2.", 50.0, 110.0), ("Kennett, B.L., 1995, Constraints. Journal 3.", 50.0, 120.0)];
        assert_eq!(cut_of(&mut r, &tail), ".SS");
        // A column that is all tail.
        let only = [("more of the entry", 58.0, 100.0), ("and its end, 5-6.", 58.0, 110.0)];
        assert_eq!(cut_of(&mut r, &only), "..");
    }

    #[test]
    fn first_line_indent() {
        let mut r = Refs::default();
        let col = [
            ("Almendros, J., Wilcock, W., 2001, Title of the paper. Journal 1, 2-3.", 58.0, 100.0),
            ("Bird, P., 2003, An updated digital model of plate boundaries:", 58.0, 110.0),
            ("Geochemistry, Geophysics, Geosystems, v. 4, 1027.", 50.0, 120.0),
            ("Fryer, P., 2012, Serpentinite mud volcanism: observations,", 58.0, 130.0),
            ("processes, and implications: Annual Review, v. 4, 345-373.", 50.0, 140.0),
            ("Hall, P.S., and Kincaid, C., 2001, Diapiric flow at subduction", 58.0, 150.0),
            ("zones: A recipe for rapid transport: Science, 292, 2472.", 50.0, 160.0),
        ];
        assert_eq!(cut_of(&mut r, &col), "SS.S.S.");
        assert_eq!(r.style, Some(Style::FirstLine));
    }

    #[test]
    fn numbered_list() {
        let mut r = Refs::default();
        let col = [
            ("1. Audhkhasi, P. & Singh, S. C. Discovery of distinct lithosphere.", 50.0, 100.0),
            ("Sci. Adv. 8, eabn5404 (2022).", 58.0, 110.0),
            ("2. Kent, G. M. et al. Evidence from three-dimensional seismic", 50.0, 120.0),
            ("reflectivity. Nature 396, 58-61 (1998).", 50.0, 130.0),
            ("3. Lin, J. & Parmentier, E. M. Mechanisms of lithospheric extension.", 50.0, 140.0),
            ("Geophys. J. Int. 96, 1-22 (1989).", 50.0, 150.0),
        ];
        assert_eq!(cut_of(&mut r, &col), "S.S.S.");
        assert!(r.numbered);
        // The next column continues the count; its first line has no number.
        let next = [("more of it, 2003.", 50.0, 100.0), ("4. Dunn, R. A dual-level magmatic system. Geophys. Res. Lett. 49 (2022).", 50.0, 110.0)];
        assert_eq!(cut_of(&mut r, &next), ".S");
    }

    #[test]
    fn vertical_space() {
        // No indent, no author: entries told by the space between them.
        let mut r = Refs::default();
        let col = [
            ("World Meteorological Organization, Guide to instruments", 50.0, 100.0),
            ("and methods of observation, Geneva", 50.0, 108.0),
            ("Some Institute of Something, Report on things", 50.0, 120.0),
            ("continued line of the report text", 50.0, 128.0),
            ("Another Body, Handbook of stuff and more things", 50.0, 140.0),
            ("continued line of the handbook", 50.0, 148.0),
        ];
        assert_eq!(cut_of(&mut r, &col), "S.S.S.");
    }

    #[test]
    fn spaced_list() {
        // No indent: entries told by the author after a line that ended one.
        let mut r = Refs::default();
        let col = [
            ("Angerer T, Hagemann SG, Danyushevsky LV (2013) Geochemical evolution of", 50.0, 100.0),
            ("the Banded Iron Formation, Australia. Econ Geol 108:1-20", 50.0, 108.0),
            ("Bell A, Hernandez S (2020) Title of the second paper of the list. Nature 1:1-9", 50.0, 116.0),
            ("Cai C (2018) Third. Nature 563:389-392", 50.0, 124.0),
        ];
        assert_eq!(cut_of(&mut r, &col), "S.SS");
    }
}
