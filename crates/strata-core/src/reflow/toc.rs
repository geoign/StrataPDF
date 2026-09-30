//! Tables of contents and indexes. The extractor cuts such a page into pieces that follow
//! neither its rows nor its entries: the numbers of the chapters, the titles and the page
//! numbers in columns of their own, a whole column of an index in one block, dots that lead
//! the eye from a title to its page as text. The layout model takes many for tables. Here
//! the page is rebuilt from its lines and cut into entries, one list item each.
//!
//! A page is such a list if its rows (the pieces of a visual line, side by side, whatever
//! the gap) end in page references (a number, a range "48–50", a list "5, 15, 18", a roman
//! numeral) after a gap or dots (a table of contents) or after a comma (an index), in a
//! third of the rows at least, and the references make an order: the pages of a table of
//! contents go up, the terms of an index are in alphabetical order. (A table has numbers
//! in every row, in no order; a reference list ends its entries in page ranges after
//! authors and years, and stops.)
//!
//! Columns are the units side by side and, in each, the columns its lines show; a column
//! of nothing but numbers (the pages, or the chapters, set apart from the titles) joins
//! the column of words it belongs to. In each column an entry ends
//! * where the space between entries begins, if the entries of the page are set apart by
//!   space (a title, its page and the authors below it are one entry);
//! * else at a row that ends in page references (a title of several rows has them in the
//!   last) or, in an index, at any row that does not stop on a comma or a word that wants
//!   another (a head term over its sub-entries, "see also").
//!
//! `STRATA_DEBUG_TOC=<page>` (1-based) prints the columns, rows and cuts of a page;
//! `STRATA_DEBUG_TOC_LINES` adds the lines of the columns.

use super::refs::{Lead, Ref, ink_columns, lead};
use super::{Unit, UnitKind, line_size, row_order};
use crate::geom::RectF;
use crate::rich::{RichChar, RichLine};
use strata_ocr::layout::{CAPTION, FORMULA, PAGE_FOOTER, PAGE_HEADER, PICTURE, SECTION_HEADER, TITLE};

// ------------------------------------------------------------------ page references

/// A roman numeral in its usual spelling.
fn roman(w: &str) -> bool {
    let upper = w.chars().all(|c| matches!(c, 'I' | 'V' | 'X' | 'L' | 'C'));
    let lower = w.chars().all(|c| matches!(c, 'i' | 'v' | 'x' | 'l' | 'c'));
    if w.is_empty() || w.len() > 6 || !(upper || lower) || (upper && w.len() < 2) {
        return false;
    }
    let value = |c: char| match c.to_ascii_lowercase() {
        'i' => 1,
        'v' => 5,
        'x' => 10,
        'l' => 50,
        _ => 100,
    };
    let v: Vec<i32> = w.chars().map(value).collect();
    let n: i32 = v.iter().enumerate().map(|(i, &x)| if v.get(i + 1).is_some_and(|&y| y > x) { -x } else { x }).sum();
    // The numeral written out again must be the word ("civil" is no numeral).
    let mut out = String::new();
    let mut rest = n;
    for (val, s) in [(100, "c"), (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i")] {
        while rest >= val {
            out.push_str(s);
            rest -= val;
        }
    }
    n > 0 && out == w.to_lowercase()
}

/// One page reference: "12", "12a", "140t", "48–50", "xii".
fn page_token(w: &str) -> bool {
    let w = w.trim_end_matches([',', ';', '.']);
    let number = |s: &str| {
        let d = s.chars().take_while(char::is_ascii_digit).count();
        (1..=4).contains(&d) && s.len() - d <= 1 && s[d..].chars().all(|c| c.is_ascii_lowercase())
    };
    match w.split_once(['–', '—', '−', '-']) {
        Some((a, b)) => number(a) && number(b),
        None => number(w) || roman(w),
    }
}

/// Where the page references at the end of the text start (in bytes), and whether the list
/// goes on ("…, 128–133," is cut at the end of a row). References run side by side only
/// after commas: numbers set apart by spaces are a row of a table.
fn trailing_pages(t: &str) -> Option<(usize, bool)> {
    let t = t.trim_end();
    let open = t.ends_with([',', ';']);
    let core = t.trim_end_matches([',', ';']).trim_end();
    let mut toks: Vec<(usize, &str)> = Vec::new();
    let mut at = 0;
    for w in core.split_whitespace() {
        let off = core[at..].find(w).map_or(at, |o| at + o);
        toks.push((off, w));
        at = off + w.len();
    }
    let suffix = |w: &str| matches!(w.trim_end_matches([',', ';', '.']), "f" | "ff" | "n" | "nn" | "t");
    let mut i = toks.len();
    while i > 0 {
        let w = toks[i - 1].1;
        let ok = page_token(w) || (suffix(w) && i >= 2 && page_token(toks[i - 2].1));
        if !ok {
            break;
        }
        i -= 1;
    }
    // Only page references, at least one, with a comma between two of them.
    let region = &toks[i..];
    if region.is_empty() || !region.iter().any(|t| page_token(t.1)) {
        return None;
    }
    for k in 0..region.len().saturating_sub(1) {
        if page_token(region[k].1) && page_token(region[k + 1].1) && !region[k].1.ends_with([',', ';']) {
            return None;
        }
    }
    Some((region[0].0, open))
}

// ------------------------------------------------------------------ rows

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pages {
    No,
    /// References at the end, and the list goes on in the next row.
    Open,
    End,
}

/// One visual row of a column, its pieces as one line.
struct Row {
    line: RichLine,
    text: String,
    y0: f32,
    x0: f32,
    pages: Pages,
    /// Nothing but references: the rest of the row before.
    bare: bool,
    /// The references stand apart from the words, as in a table of contents.
    apart: bool,
    /// The row is a letter that heads a group of an index.
    letter: bool,
    /// The text ends where more is needed: a comma, a hyphen, a word that wants another.
    wants_more: bool,
}

/// Dots (and their like) that lead the eye across the page, and spaces among them, become one
/// space.
fn strip_leaders(chars: Vec<RichChar>) -> Vec<RichChar> {
    let mark = |c: char| matches!(c, '.' | '·' | '…' | '‧' | '∙' | '•' | '_');
    let mut out: Vec<RichChar> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if mark(chars[i].c) {
            let mut j = i;
            let mut marks = 0;
            while j < chars.len() && (mark(chars[j].c) || chars[j].c.is_whitespace()) {
                marks += if chars[j].c == '…' { 3 } else { mark(chars[j].c) as usize };
                j += 1;
            }
            if marks >= 4 {
                if out.last().is_some_and(|c| !c.c.is_whitespace()) {
                    let first = chars[i];
                    out.push(RichChar { c: ' ', bbox: RectF { x1: chars[j - 1].bbox.x1, ..first.bbox }, ..first });
                }
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Pieces of a row as one line, a space between them where they do not touch.
fn merge(frags: &[RichLine]) -> RichLine {
    let mut chars: Vec<RichChar> = Vec::new();
    for f in frags {
        let mut c: Vec<RichChar> = f.chars.clone();
        while c.first().is_some_and(|c| c.c.is_whitespace()) {
            c.remove(0);
        }
        while c.last().is_some_and(|c| c.c.is_whitespace()) {
            c.pop();
        }
        if c.is_empty() {
            continue;
        }
        // (A gap of a fraction of an em is no word space: the end of a word that the extractor
        // cut off the line.)
        if let Some(last) = chars.last().copied()
            && !last.c.is_whitespace()
            && c[0].bbox.x0 - last.bbox.x1 > 0.2 * last.size.max(c[0].size)
        {
            chars.push(RichChar { c: ' ', bbox: RectF { x0: last.bbox.x1, x1: c[0].bbox.x0.max(last.bbox.x1), ..last.bbox }, ..last });
        }
        chars.extend(c);
    }
    let chars = strip_leaders(chars);
    let bbox = frags.iter().skip(1).fold(frags[0].bbox, |a, l| a.union(&l.bbox));
    RichLine { bbox, vertical: false, dir: frags[0].dir, joined: false, chars }
}

/// Words that cannot end an entry.
fn wants_more(t: &str) -> bool {
    let t = t.trim_end();
    if t.ends_with([',', ';', '(', '-', '–', '—', '/']) || t.chars().filter(|&c| c == '(').count() > t.chars().filter(|&c| c == ')').count() {
        return true;
    }
    const WORDS: [&str; 22] = ["and", "or", "of", "the", "a", "an", "to", "in", "with", "for", "by", "from", "on", "at", "than", "as", "is", "are", "its", "their", "&", "vs."];
    t.rsplit(char::is_whitespace).next().is_some_and(|w| WORDS.contains(&w.to_lowercase().as_str()))
}

/// The rows of a column: lines side by side on one baseline, whatever the gap.
fn rows_of(lines: Vec<RichLine>) -> Vec<Row> {
    let order = row_order(&lines);
    let mut groups: Vec<(Vec<RichLine>, f32, f32)> = Vec::new();
    for i in order {
        let l = lines[i].clone();
        if l.chars.iter().all(|c| c.c.is_whitespace()) {
            continue;
        }
        let b = l.bbox;
        match groups.last_mut() {
            Some((g, y0, y1)) if b.y1.min(*y1) - b.y0.max(*y0) >= 0.5 * b.height().min(*y1 - *y0) => {
                *y0 = y0.min(b.y0);
                *y1 = y1.max(b.y1);
                g.push(l);
            }
            _ => groups.push((vec![l], b.y0, b.y1)),
        }
    }
    groups
        .into_iter()
        .filter_map(|(mut frags, y0, _)| {
            frags.sort_by(|a, b| a.bbox.x0.total_cmp(&b.bbox.x0));
            // Pieces of dots only are the leaders themselves.
            frags.retain(|f| f.chars.iter().any(|c| !c.c.is_whitespace() && !matches!(c.c, '.' | '·' | '…' | '‧' | '∙' | '•' | '_')) || f.chars.iter().filter(|c| c.c == '.').count() < 4);
            if frags.is_empty() {
                return None;
            }
            let line = merge(&frags);
            // (A row of nothing but leaders is no row.)
            if line.chars.iter().all(|c| c.c.is_whitespace()) {
                return None;
            }
            let text: String = line.chars.iter().map(|c| c.c).collect();
            let ink: Vec<&RichChar> = line.chars.iter().filter(|c| !c.c.is_whitespace()).collect();
            let x0 = ink.first().map_or(line.bbox.x0, |c| c.bbox.x0);
            let pages = match trailing_pages(&text) {
                Some((_, true)) => Pages::Open,
                Some((_, false)) => Pages::End,
                None => Pages::No,
            };
            let bare = trailing_pages(&text).is_some_and(|(at, _)| at == 0);
            let em = line_size(&line).max(1.0);
            // The last piece is nothing but references, set apart from the piece before it.
            let apart = frags.len() >= 2 && {
                let last = frags.last().unwrap();
                let before = &frags[frags.len() - 2];
                let lt: String = last.chars.iter().map(|c| c.c).collect();
                trailing_pages(&lt).is_some_and(|(at, _)| at == 0) && last.bbox.x0 - before.bbox.x1 >= 1.5 * em
            };
            let t = text.trim();
            let letter = t.chars().filter(|c| c.is_alphabetic()).count() <= 2 && t.chars().count() <= 4 && t.chars().next().is_some_and(char::is_uppercase) && t.chars().all(|c| c.is_alphabetic() || matches!(c, '–' | '-' | ' '));
            let wants_more = wants_more(&text);
            Some(Row { line, text, y0, x0, pages, bare, apart, letter, wants_more })
        })
        .collect()
}

// ------------------------------------------------------------------ the page

/// A unit that can be a row of a list: not a running head or a figure, and no heading
/// unless it ends in a page number.
fn candidate(u: &Unit) -> bool {
    // (A column of page numbers can come as one line of digits that the extractor takes for
    // vertical writing.)
    let digits = |l: &RichLine| l.chars.len() >= 2 && l.chars.iter().all(|c| c.c.is_whitespace() || matches!(c.c, '0'..='9' | 'i' | 'v' | 'x' | 'l' | 'c' | 'I' | 'V' | 'X' | 'L' | 'C'));
    if u.kind != UnitKind::Text || u.refs != Ref::No || u.lines.is_empty() || u.lines.iter().any(|l| l.vertical && !digits(l)) {
        return false;
    }
    if matches!(u.class, Some(PAGE_HEADER | PAGE_FOOTER | PICTURE | CAPTION | FORMULA)) {
        return false;
    }
    let t = u.text();
    let t = t.trim();
    if matches!(u.class, Some(TITLE | SECTION_HEADER)) && trailing_pages(t).is_none() {
        return false;
    }
    !heading_word(t)
}

/// "Contents", "Index", "List of Figures": the heading over the list.
fn heading_word(t: &str) -> bool {
    let l = t.trim().trim_end_matches([':', '.']).to_lowercase();
    let l: String = l.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(l.as_str(), "contents" | "content" | "table of contents" | "index" | "subject index" | "author index" | "general index" | "list of figures" | "list of tables" | "detailed contents" | "brief contents" | "contents in brief" | "keyword index")
        || l.starts_with("index of ")
        || l.starts_with("list of ")
}

/// How the entries of a list are set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Style {
    /// Entries end in page references, set apart or inline; the title of an entry can
    /// take several rows.
    Contents,
    /// An index: page lists after commas, head terms and sub-entries with no page.
    Index,
}

/// The number of the first page reference of a row.
fn first_page(text: &str) -> Option<u32> {
    let (at, _) = trailing_pages(text)?;
    let w = text[at..].split_whitespace().next()?;
    let digits: String = w.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Nearly all steps of the sequence go up (or stay): a table of contents runs on through the
/// book, and an index is in alphabetical order. A table has no such order in its numbers.
fn ascending<T: PartialOrd>(v: &[T]) -> bool {
    v.len() >= 6 && v.windows(2).filter(|w| w[0] <= w[1]).count() * 100 >= (v.len() - 1) * 80
}

/// Whether the rows of the page make a list, and how it is set.
fn judge(cols: &[Vec<Row>], debug: bool) -> Option<Style> {
    let rows: Vec<&Row> = cols.iter().flatten().collect();
    let n = rows.len();
    if n < 8 {
        return None;
    }
    let with: Vec<&&Row> = rows.iter().filter(|r| r.pages != Pages::No).collect();
    if debug {
        eprintln!("toc: {} rows, {} end in page references", n, with.len());
    }
    if with.len() < 6 || with.len() * 100 < n * 30 {
        return None;
    }
    // Rows of a table hold numbers among their words (dates, coordinates); a page reference
    // stands after a term, which is words.
    let termed = rows
        .iter()
        .filter(|r| r.pages != Pages::No && !r.bare)
        .filter(|r| {
            let term = trailing_pages(&r.text).map_or(&r.text[..], |(at, _)| &r.text[..at]);
            let (letters, marks) = term.chars().filter(|c| !c.is_whitespace()).fold((0, 0), |(l, m), c| (l + c.is_alphabetic() as usize, m + 1));
            letters >= 3 && letters * 2 >= marks
        })
        .count();
    let ref_rows = rows.iter().filter(|r| r.pages != Pages::No && !r.bare).count();
    if ref_rows == 0 || termed * 100 < ref_rows * 85 {
        return None;
    }
    // Reference lists end their entries in page ranges too, after authors and years, and stop.
    let authors = rows.iter().filter(|r| lead(&r.text) == Lead::Strong).count();
    let stops = rows.iter().filter(|r| r.text.trim_end().ends_with('.')).count();
    if authors * 100 >= n * 25 || stops * 100 >= n * 30 {
        return None;
    }
    // The pages of the entries go up (a table of contents), or the terms at the left edge of
    // each column are in alphabetical order (an index).
    // (A page of a table has a column that stays the same or has a few steps, as the
    // magnitudes of earthquakes sorted by date do: the pages go up in at least three steps.)
    let pages: Vec<u32> = rows.iter().filter(|r| !r.bare).filter_map(|r| first_page(&r.text)).collect();
    if ascending(&pages) && pages.windows(2).filter(|w| w[0] < w[1]).count() >= 3 {
        return Some(Style::Contents);
    }
    let mut terms: Vec<String> = Vec::new();
    for c in cols {
        let edge = c.iter().map(|r| r.x0).fold(f32::INFINITY, f32::min);
        let em = c.first().map_or(8.0, |r| line_size(&r.line));
        terms.extend(c.iter().filter(|r| r.x0 < edge + 0.6 * em && !r.bare).map(|r| r.text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>()).filter(|t| !t.is_empty()));
    }
    ascending(&terms).then_some(Style::Index)
}

/// The rows of a column cut into entries (row ranges).
fn cut(rows: &[Row], style: Style, debug: bool) -> Vec<(usize, usize)> {
    let n = rows.len();
    // The space between entries: steps between rows well above the usual step.
    let mut steps: Vec<f32> = (1..n).map(|k| rows[k].y0 - rows[k - 1].y0).filter(|s| *s > 0.5).collect();
    steps.sort_by(f32::total_cmp);
    let pitch = if steps.is_empty() { 0.0 } else { steps[(steps.len() * 3 / 10).min(steps.len() - 1)] };
    let extra = |k: usize| pitch > 0.0 && rows[k].y0 - rows[k - 1].y0 > 1.3 * pitch;
    // Entries set apart by space: there is a space for most entries, after the row that ends
    // in references or, if the authors of a chapter follow, after that.
    let ends = rows.iter().filter(|r| r.pages == Pages::End).count();
    let gaps: Vec<usize> = (1..n).filter(|&k| extra(k)).collect();
    let after_entry = gaps.iter().filter(|&&k| rows[k - 1].pages == Pages::End || (k >= 2 && rows[k - 1].pages == Pages::No && rows[k - 2].pages == Pages::End)).count();
    let spaced = gaps.len() >= 4 && gaps.len() * 5 >= ends * 2 && after_entry * 100 >= gaps.len() * 80;
    if debug {
        eprintln!("toc: column of {n} rows, {style:?}, pitch {pitch:.1}, entries set apart by space: {spaced}");
    }
    let starts = |k: usize| -> bool {
        let (prev, row) = (&rows[k - 1], &rows[k]);
        if prev.letter || row.letter {
            return true;
        }
        if spaced {
            // After a row that ends in references the next entry begins at once: a row of its
            // own, or the first row of a title (the rows to the one that ends in references
            // are close together), and not the authors of the chapter (rows with no
            // references, and space after them).
            let mut j = k;
            while j + 1 < n && rows[j].pages == Pages::No && !extra(j + 1) {
                j += 1;
            }
            return extra(k) || (prev.pages == Pages::End && !row.bare && rows[j].pages != Pages::No);
        }
        // The rest of a page list, or of a title, goes on with the entry.
        if row.bare {
            return false;
        }
        match style {
            Style::Contents => prev.pages == Pages::End,
            Style::Index => prev.pages == Pages::End || (prev.pages == Pages::No && !prev.wants_more),
        }
    };
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut from = 0;
    for k in 1..n {
        if starts(k) {
            out.push((from, k));
            from = k;
        }
    }
    out.push((from, n));
    out
}

/// A line whose characters stand one above the other (the extractor ran a column of page
/// numbers into one line, "1223") is a line for each.
fn unstack(l: &RichLine) -> Vec<RichLine> {
    let size = line_size(l).max(1.0);
    if l.bbox.height() < 1.8 * size {
        return vec![l.clone()];
    }
    let mut groups: Vec<(Vec<RichChar>, f32, f32)> = Vec::new();
    for c in &l.chars {
        match groups.last_mut() {
            Some((g, y0, y1)) if c.bbox.y1.min(*y1) - c.bbox.y0.max(*y0) >= 0.5 * c.bbox.height().min(*y1 - *y0) => {
                *y0 = y0.min(c.bbox.y0);
                *y1 = y1.max(c.bbox.y1);
                g.push(*c);
            }
            _ => groups.push((vec![*c], c.bbox.y0, c.bbox.y1)),
        }
    }
    groups
        .into_iter()
        .map(|(chars, ..)| {
            let bbox = chars.iter().skip(1).fold(chars[0].bbox, |a, c| a.union(&c.bbox));
            RichLine { bbox, vertical: false, dir: l.dir, joined: false, chars }
        })
        .collect()
}

/// A column of nothing but page numbers, or of the numbers of chapters, that the titles it
/// belongs to are set apart from, is no column: its lines join the nearest column of words.
fn attach_margins(cols: Vec<Vec<RichLine>>) -> Vec<Vec<RichLine>> {
    let text = |l: &RichLine| l.chars.iter().map(|c| c.c).collect::<String>();
    let is_margin = |c: &Vec<RichLine>| c.iter().filter(|l| l.chars.iter().filter(|c| !c.c.is_whitespace()).count() <= 30 && trailing_pages(&text(l)).is_some_and(|(at, _)| at == 0)).count() * 10 >= c.len() * 8;
    let extent = |c: &Vec<RichLine>| c.iter().skip(1).fold(c[0].bbox, |a, l| a.union(&l.bbox));
    let (margins, mut words): (Vec<Vec<RichLine>>, Vec<Vec<RichLine>>) = cols.into_iter().partition(is_margin);
    for m in margins {
        let mb = extent(&m);
        // The numbers of pages stand at the end of a row: they belong to the column of words
        // at their left (the nearest, on a row of their own height); the numbers of chapters
        // that stand before the titles have none there, and go to the column at their right.
        let dist = |w: &Vec<RichLine>, left: bool| {
            let wb = extent(w);
            let v = wb.y1.min(mb.y1) - wb.y0.max(mb.y0);
            let side = if left { wb.x0 < mb.x0 } else { wb.x1 > mb.x0 };
            (v >= 0.5 * mb.height().min(wb.height()) && side).then(|| (mb.x0 - wb.x1).max(wb.x0 - mb.x1).max(0.0))
        };
        let nearest = |left: bool| (0..words.len()).filter_map(|k| dist(&words[k], left).map(|d| (k, d))).min_by(|a, b| a.1.total_cmp(&b.1)).map(|t| t.0);
        match nearest(true).or_else(|| nearest(false)) {
            Some(k) => words[k].extend(m),
            None => words.push(m),
        }
    }
    words
}

fn at(slots: &[Option<Unit>], i: usize) -> &Unit {
    slots[i].as_ref().unwrap()
}

/// The units of a page with a table of contents or an index in them rebuilt as entries. The
/// units of the page that can be rows are taken together, so that a page that is a list has
/// all of it rebuilt, and other pages stay as they are.
pub(super) fn lists(units: Vec<Unit>, debug: bool) -> Vec<Unit> {
    let mut slots: Vec<Option<Unit>> = units.into_iter().map(Some).collect();
    let mut idx: Vec<usize> = (0..slots.len()).filter(|&i| candidate(at(&slots, i))).collect();
    // Running text across the columns (the note over an index) is no part of the list, and
    // would be cut at the gutters.
    let (lo, hi) = idx.iter().flat_map(|&i| at(&slots, i).lines.iter()).fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), l| (lo.min(l.bbox.x0), hi.max(l.bbox.x1)));
    let prose = |l: &RichLine| {
        let t: String = l.chars.iter().map(|c| c.c).collect();
        l.bbox.width() >= 0.85 * (hi - lo) && t.split_whitespace().count() >= 8 && trailing_pages(&t).is_none() && super::prose_like(&t)
    };
    idx.retain(|&i| at(&slots, i).lines.iter().filter(|l| prose(l)).count() * 10 < at(&slots, i).lines.len() * 6);
    let n: usize = idx.iter().map(|&i| at(&slots, i).lines.len()).sum();
    if n < 8 {
        return slots.into_iter().flatten().collect();
    }
    // The columns: the units side by side, and in each, the columns that its lines show (an
    // extractor block can hold several). The numbers of the pages, set apart from the titles,
    // show as columns too: they are joined to the titles again.
    let mut cols: Vec<Vec<RichLine>> = Vec::new();
    for group in super::entries::columns(&slots, &idx) {
        let lines: Vec<RichLine> = group.iter().flat_map(|&i| at(&slots, i).lines.iter().flat_map(unstack)).collect();
        if lines.len() >= 8 {
            let raw: Vec<(RichLine, Option<usize>, Option<usize>)> = lines.into_iter().map(|l| (l, None, None)).collect();
            cols.extend(ink_columns(raw).into_iter().map(|c| c.into_iter().map(|l| l.0).collect::<Vec<RichLine>>()));
        } else {
            cols.push(lines);
        }
    }
    if debug {
        for (ci, c) in cols.iter().enumerate() {
            let b = c.iter().skip(1).fold(c[0].bbox, |a, l| a.union(&l.bbox));
            eprintln!("toc: raw column {ci}: {} lines, x {:.0}..{:.0}, y {:.0}..{:.0}", c.len(), b.x0, b.x1, b.y0, b.y1);
            if std::env::var("STRATA_DEBUG_TOC_LINES").is_ok() {
                for l in c {
                    eprintln!("    [{:.0},{:.0},{:.0},{:.0}] {}", l.bbox.x0, l.bbox.y0, l.bbox.x1, l.bbox.y1, l.chars.iter().map(|c| c.c).collect::<String>());
                }
            }
        }
    }
    let cols: Vec<Vec<Row>> = attach_margins(cols).into_iter().map(rows_of).collect();
    if debug {
        for (ci, c) in cols.iter().enumerate() {
            eprintln!("toc: column {ci}: {} rows", c.len());
            for r in c {
                eprintln!("  {:?}{}{}{}{} x0={:.1} y={:.1} | {}", r.pages, if r.bare { " bare" } else { "" }, if r.apart { " apart" } else { "" }, if r.letter { " letter" } else { "" }, if r.wants_more { " more" } else { "" }, r.x0, r.y0, r.text.chars().take(60).collect::<String>());
            }
        }
    }
    let Some(style) = judge(&cols, debug) else { return slots.into_iter().flatten().collect() };
    for &i in &idx {
        slots[i] = None;
    }
    let mut out: Vec<Unit> = slots.into_iter().flatten().collect();
    for col in cols {
        for (a, b) in cut(&col, style, debug) {
            let rows = &col[a..b];
            let lines: Vec<RichLine> = rows.iter().map(|r| r.line.clone()).collect();
            let bbox = lines.iter().skip(1).fold(lines[0].bbox, |acc, l| acc.union(&l.bbox));
            // A letter that heads a group is a paragraph of its own.
            let refs = if rows.len() == 1 && rows[0].letter { Ref::Start } else { Ref::Item };
            out.push(Unit { kind: UnitKind::Text, bbox, lines, class: None, group: None, refs });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(t: &str) -> Vec<RichChar> {
        t.chars().enumerate().map(|(i, c)| RichChar { c, bbox: RectF { x0: i as f32 * 4.0, y0: 0.0, x1: i as f32 * 4.0 + 4.0, y1: 8.0 }, size: 8.0, font: 0, bold: false, argb: 0 }).collect()
    }

    /// A line of `text` from `x0`, top `y`, size 8, four points a character.
    fn line(text: &str, x0: f32, y: f32) -> RichLine {
        let mut cs = chars(text);
        for c in cs.iter_mut() {
            c.bbox.x0 += x0;
            c.bbox.x1 += x0;
            c.bbox.y0 = y;
            c.bbox.y1 = y + 8.0;
        }
        let bbox = RectF { x0, y0: y, x1: x0 + 4.0 * cs.len() as f32, y1: y + 8.0 };
        RichLine { bbox, vertical: false, dir: [1.0, 0.0], joined: false, chars: cs }
    }

    /// The entries of a column of rows `(title, page, y)` (the page as a piece of its own).
    fn entries(rows: &[(&str, &str, f32)], x0: f32) -> Vec<String> {
        let mut lines: Vec<RichLine> = Vec::new();
        for (t, p, y) in rows {
            lines.push(line(t, x0, *y));
            if !p.is_empty() {
                lines.push(line(p, 250.0, *y));
            }
        }
        let col = rows_of(lines);
        let style = judge(&[col.iter().map(|r| Row { line: r.line.clone(), text: r.text.clone(), y0: r.y0, x0: r.x0, pages: r.pages, bare: r.bare, apart: r.apart, letter: r.letter, wants_more: r.wants_more }).collect()], false);
        assert!(style.is_some(), "the rows are a list");
        cut(&col, style.unwrap(), false).into_iter().map(|(a, b)| col[a..b].iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(" | ")).collect()
    }

    #[test]
    fn page_references() {
        assert!(page_token("12") && page_token("140t") && page_token("48–50") && page_token("xii") && page_token("XII") && page_token("128,"));
        assert!(!page_token("civil") && !page_token("I") && !page_token("12345") && !page_token("in") && !page_token("2.1"));
        assert_eq!(trailing_pages("Filters, 48–50, 98, 103"), Some((9, false)));
        assert_eq!(trailing_pages("Migration, 69, 123–124, 127–128,"), Some((11, true)));
        assert_eq!(trailing_pages("Hadean 139, 140 f, 144 ff, 146"), Some((7, false)));
        assert_eq!(trailing_pages("Preface XII"), Some((8, false)));
        assert_eq!(trailing_pages("1.1 Outline and Approach 1"), Some((25, false)));
        assert_eq!(trailing_pages("132–133, 249"), Some((0, false)));
        // Numbers side by side are a table's.
        assert_eq!(trailing_pages("Item 12 15 20"), None);
        assert_eq!(trailing_pages("Hanning function"), None);
    }

    #[test]
    fn leaders_go() {
        let s = |t: &str| strip_leaders(chars(t)).into_iter().map(|c| c.c).collect::<String>();
        assert_eq!(s("Preface ........ XII"), "Preface XII");
        assert_eq!(s("Preface . . . . . . XII"), "Preface XII");
        assert_eq!(s("U.S.A. and Fig. 1."), "U.S.A. and Fig. 1.");
        assert_eq!(s("Title…………24"), "Title 24");
    }

    #[test]
    fn a_row_of_leaders_is_no_row() {
        let lines = vec![line("Preface", 50.0, 100.0), line("……", 120.0, 100.0), line("ix", 250.0, 100.0), line("……", 60.0, 120.0)];
        let rows = rows_of(lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "Preface ix");
    }

    #[test]
    fn a_table_of_contents_is_one_entry_a_title_whatever_the_rows() {
        let e = entries(
            &[
                ("Preface", "ix", 100.0),
                ("1 Introduction", "1", 110.0),
                ("1.1 Outline and approach", "1", 120.0),
                ("1.2 A title that is so long that it", "", 130.0),
                ("takes a second row", "3", 140.0),
                ("2 Field equipment", "5", 150.0),
                ("2.1 Hammers and chisels", "5", 160.0),
                ("2.2 Compasses", "7", 170.0),
                ("3 Maps", "21", 180.0),
                ("3.1 Types of map", "21", 190.0),
            ],
            50.0,
        );
        assert_eq!(e.len(), 9);
        assert_eq!(e[0], "Preface ix");
        assert_eq!(e[3], "1.2 A title that is so long that it | takes a second row 3");
    }

    #[test]
    fn entries_set_apart_by_space_keep_the_authors_of_a_chapter() {
        let e = entries(
            &[
                ("Chapter 1 Volcanoes and the", "", 100.0),
                ("way they erupt", "3", 110.0),
                ("A. Author and B. Author", "", 120.0),
                ("Chapter 2 Lava", "27", 145.0),
                ("C. Author", "", 155.0),
                ("Chapter 3 Ash", "53", 180.0),
                ("Chapter 4 Gas and the", "", 205.0),
                ("air", "71", 215.0),
                ("D. Author, E. Author", "", 225.0),
                ("Chapter 5 Ice", "99", 250.0),
                ("Chapter 6 Rock", "125", 275.0),
                ("Chapter 7 Sea", "153", 300.0),
            ],
            50.0,
        );
        assert_eq!(
            e,
            ["Chapter 1 Volcanoes and the | way they erupt 3 | A. Author and B. Author", "Chapter 2 Lava 27 | C. Author", "Chapter 3 Ash 53", "Chapter 4 Gas and the | air 71 | D. Author, E. Author", "Chapter 5 Ice 99", "Chapter 6 Rock 125", "Chapter 7 Sea 153"]
        );
    }

    #[test]
    fn an_index_has_head_terms_sub_entries_and_lists_that_run_over() {
        // Terms at 50, sub-entries at 60, the rest of a page list at 70.
        let mut lines: Vec<RichLine> = Vec::new();
        let rows: [(&str, f32, f32); 10] = [
            ("Geophone", 50.0, 100.0),
            ("arrays, 183, 192", 60.0, 110.0),
            ("coupling, 75", 60.0, 120.0),
            ("Geophysicist, 16, 79, 134, 144,", 50.0, 130.0),
            ("198, 217, 224, 228", 70.0, 140.0),
            ("Ghost, 106, 109", 50.0, 150.0),
            ("See also Rayleigh wave", 60.0, 160.0),
            ("Group interval, 60, 62", 50.0, 170.0),
            ("Gulf Coast, 133", 50.0, 180.0),
            ("Harmonic distortion, 52", 50.0, 190.0),
        ];
        for (t, x, y) in rows {
            lines.push(line(t, x, y));
        }
        let col = rows_of(lines);
        let style = judge(&[col.iter().map(|r| Row { line: r.line.clone(), text: r.text.clone(), y0: r.y0, x0: r.x0, pages: r.pages, bare: r.bare, apart: r.apart, letter: r.letter, wants_more: r.wants_more }).collect()], false);
        assert_eq!(style, Some(Style::Index));
        let e: Vec<String> = cut(&col, Style::Index, false).into_iter().map(|(a, b)| col[a..b].iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(" ")).collect();
        assert_eq!(e[0], "Geophone");
        assert_eq!(e[1], "arrays, 183, 192");
        assert_eq!(e[3], "Geophysicist, 16, 79, 134, 144, 198, 217, 224, 228");
        assert_eq!(e[5], "See also Rayleigh wave");
        assert_eq!(e.len(), 9);
    }

    #[test]
    fn rows_of_numbers_are_no_list() {
        // A table: a term and several numbers in every row, in no order.
        let mut lines: Vec<RichLine> = Vec::new();
        for (k, (t, n)) in [("Sample A", "12 15 20"), ("Sample B", "9 30 41"), ("Sample C", "40 2 7"), ("Sample D", "3 3 1"), ("Sample E", "88 19 5"), ("Sample F", "1 2 3"), ("Sample G", "30 20 10"), ("Sample H", "4 5 6")].iter().enumerate() {
            lines.push(line(t, 50.0, 100.0 + 10.0 * k as f32));
            lines.push(line(n, 150.0, 100.0 + 10.0 * k as f32));
        }
        let col = rows_of(lines);
        assert!(judge(&[col], false).is_none());
    }
}
