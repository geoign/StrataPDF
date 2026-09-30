//! Lists that the extractor ran together or cut apart. Dictionary and glossary
//! entries, numbered and bulleted list items and bibliographies outside a reference
//! section are set with a hanging indent: each entry starts at the column's left edge
//! and its other lines are indented. The extractor and the layout model put a whole
//! column of them in one unit, which reads as a single paragraph (and, at the top of a
//! page, joins the last entry of the page before), or cut it into pieces that follow
//! neither the entries nor the rows (the headword of an entry in a unit of its own, the
//! tail of the last line in another). Here the units of one column are cut into entries.
//!
//! The layout is told from the lines themselves. Lines are put in visual rows first (a
//! bold headword, its label and its text are pieces of one row). A column is a hanging
//! list if its rows start at two places only, the edge and an indent, with rows at both
//! and the rows before those at the edge short: an entry's last line stops short of the
//! right edge, the lines inside an entry do not. (A paragraph with an indented first
//! line has the mirror image: the rows before the indented ones are short.) Ragged text
//! stops short everywhere and shows no such difference.

use super::refs::Ref;
use super::{Unit, UnitKind, ink_box, line_size, row_order};
use crate::rich::{RichChar, RichLine};
use crate::text::FontInfo;
use strata_ocr::layout::{CAPTION, FORMULA, PAGE_FOOTER, PAGE_HEADER, PICTURE, SECTION_HEADER, TABLE, TITLE};

/// One visual row of a unit: its first line, where its ink starts and ends, and its last
/// character.
struct Row {
    first: usize,
    x0: f32,
    x1: f32,
    end: char,
    /// It opens like an entry: with a capital, a digit or a mark, or with a term in bold or italic.
    opens: bool,
}

/// A line opens like the first line of an entry. A row that carries on a sentence starts in
/// lowercase, in the type of the text.
fn opens_entry(l: &RichLine, fonts: &[FontInfo], scan: bool) -> bool {
    let Some(first) = l.chars.iter().find(|c| !c.c.is_whitespace()) else { return false };
    // (OCR text has no styles to go by.)
    if !first.c.is_lowercase() || scan {
        return !first.c.is_lowercase();
    }
    let styled = |c: &RichChar| c.bold || fonts.get(c.font as usize).is_some_and(|f| super::font_bold(f) || super::font_italic(f));
    l.chars.iter().skip_while(|c| c.c.is_whitespace()).take_while(|c| !c.c.is_whitespace()).all(styled)
}

/// The rows of the lines, which lie in reading order (pieces of one row side by side).
/// Lines of blanks only belong to no row.
fn rows(lines: &[RichLine], fonts: &[FontInfo], scan: bool) -> Vec<Row> {
    let mut out: Vec<(Row, f32, f32)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.chars.iter().all(|c| c.c.is_whitespace()) {
            continue;
        }
        let (b, ink) = (l.bbox, ink_box(l));
        let end = l.chars.iter().rev().find(|c| !c.c.is_whitespace()).map_or(' ', |c| c.c);
        match out.last_mut() {
            Some((r, y0, y1)) if b.y1.min(*y1) - b.y0.max(*y0) >= 0.5 * b.height().min(*y1 - *y0) => {
                r.x0 = r.x0.min(ink.x0);
                if ink.x1 >= r.x1 {
                    r.x1 = ink.x1;
                    r.end = end;
                }
                *y0 = y0.min(b.y0);
                *y1 = y1.max(b.y1);
            }
            _ => out.push((Row { first: i, x0: ink.x0, x1: ink.x1, end, opens: opens_entry(l, fonts, scan) }, b.y0, b.y1)),
        }
    }
    out.into_iter().map(|r| r.0).collect()
}

/// Where a row starts against the column's edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pos {
    Edge,
    Indent,
    /// Far from the edge, or left of it: no part of the pattern.
    Off,
}

/// The lines of a hanging list where entries start (the first line of each row at the
/// edge), and the first lines of the indented rows, which carry on an entry.
struct Layout {
    starts: Vec<usize>,
    indented: Vec<usize>,
}

/// The layout of the lines, in rows, if they are a hanging list.
fn hanging_layout(lines: &[RichLine], size: f32, scan: bool, fonts: &[FontInfo], debug: bool) -> Option<Layout> {
    let rows = rows(lines, fonts, scan);
    let n = rows.len();
    if n < 4 {
        return None;
    }
    let em = size.max(1.0);
    // OCR positions jitter.
    let tol = if scan { 0.5 * em } else { 0.3 * em };
    // The places where rows start (rows within a tolerance of each other start in one).
    let mut xs: Vec<f32> = rows.iter().map(|r| r.x0).collect();
    xs.sort_by(f32::total_cmp);
    let mut clusters: Vec<(f32, usize)> = Vec::new();
    for x in xs {
        match clusters.last_mut() {
            Some(c) if x - c.0 <= tol => c.1 += 1,
            _ => clusters.push((x, 1)),
        }
    }
    // The edge: the leftmost place with several rows on it. The indent: the place within
    // six ems of it that most rows start at (a row is at the edge or at the indent, by
    // whichever it is nearer to).
    let need = 2.max(n * 12 / 100);
    let edge = clusters.iter().find(|c| c.1 >= need)?.0;
    let indent = clusters.iter().filter(|c| c.0 - edge > tol && c.0 - edge <= 6.0 * em).max_by_key(|c| c.1)?.0;
    let middle = (edge + indent) / 2.0;
    let pos: Vec<Pos> = rows
        .iter()
        .map(|r| {
            if r.x0 < edge - tol {
                Pos::Off
            } else if r.x0 < middle {
                Pos::Edge
            } else if r.x0 <= edge + 6.0 * em {
                Pos::Indent
            } else {
                Pos::Off
            }
        })
        .collect();
    let edges = pos.iter().filter(|p| **p == Pos::Edge).count();
    let indents = pos.iter().filter(|p| **p == Pos::Indent).count();
    if debug {
        eprintln!("entries: {n} rows, edge {edge:.1}, {edges} at the edge, {indents} indented, {} elsewhere", n - edges - indents);
    }
    if edges < 2 || indents < 2 || (edges + indents) * 100 < n * 85 {
        return None;
    }
    // How often the row before stops short of the right edge, for the rows at the edge
    // and for the indented ones.
    let mut rights: Vec<f32> = rows.iter().map(|r| r.x1).collect();
    rights.sort_by(f32::total_cmp);
    let right = rights[rights.len() * 85 / 100];
    let short = |k: usize| rows[k].x1 < right - 1.0 * em;
    let rate = |p: Pos| {
        let idx: Vec<usize> = (1..n).filter(|&k| pos[k] == p).collect();
        idx.iter().filter(|&&k| short(k - 1)).count() as f32 / idx.len().max(1) as f32
    };
    let (at_edge, at_indent) = (rate(Pos::Edge), rate(Pos::Indent));
    if debug {
        eprintln!("entries: rows before those at the edge stop short {at_edge:.2}, before the indented ones {at_indent:.2}");
        for k in 0..n {
            let t: String = lines[rows[k].first].text().chars().take(40).collect();
            eprintln!("  {:?}{} x0={:.1} x1={:.1}{} | {t}", pos[k], if rows[k].opens { " opens" } else { "" }, rows[k].x0, rows[k].x1, if k > 0 && short(k - 1) { " (row before is short)" } else { "" });
        }
    }
    if at_edge < 0.6 || at_edge - at_indent < 0.3 {
        return None;
    }
    // The rows at the edge open entries: with a capital, a digit, a mark or a headword in
    // type of its own. (Text set ragged and flush left has rows at the edge that carry on
    // a sentence, and its short rows tell nothing.)
    let at_edge_rows: Vec<usize> = (1..n).filter(|&k| pos[k] == Pos::Edge).collect();
    if at_edge_rows.iter().filter(|&&k| rows[k].opens).count() * 4 < at_edge_rows.len() * 3 {
        return None;
    }
    // An entry starts at each row at the edge that opens like one (the first row of a column
    // that carries on a sentence does not) and follows the last row of an entry, which stops
    // short of the edge. (After a full row it carries on a paragraph, as the second row of
    // a paragraph with an indented first line does; a one-line entry that fills its line
    // closes with the bracket of its last part, a pronunciation or a date.)
    let closes = |k: usize| matches!(rows[k].end, '}' | ']');
    let starts = (0..n).filter(|&k| pos[k] == Pos::Edge && rows[k].opens && (k == 0 || short(k - 1) || closes(k - 1))).map(|k| rows[k].first).collect();
    let indented = (0..n).filter(|&k| pos[k] == Pos::Indent).map(|k| rows[k].first).collect();
    Some(Layout { starts, indented })
}

/// A line that opens with a list marker ("(ii)", "3.", a bullet) and has words after it.
fn item_marker(t: &str) -> bool {
    (super::is_list_marker(t) || super::refs::marker(t).is_some()) && t.chars().filter(|c| c.is_alphabetic()).count() >= 2
}

/// A piece of a column: its lines, the unit that its first line came from, and whether it
/// opens with an entry.
struct Piece {
    lines: Vec<RichLine>,
    owner: usize,
    start: bool,
}

/// The lines of a column (in rows, `owner` telling the unit of each) cut into pieces:
/// before each line that starts an entry, and where the lines pass to another unit,
/// except at an indented row that opens no item and lies close under the row before it (the
/// tail of an entry that the extractor left in a unit of its own belongs to the entry above
/// it, and a paragraph with space above it is a paragraph). Pieces of one row that
/// different units hold (a headword the extractor left out of its line) stay together, so
/// that the row is read across.
fn pieces(lines: Vec<RichLine>, owner: &[usize], layout: &Layout) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut owners: Vec<usize> = Vec::new();
    for (i, l) in lines.into_iter().enumerate() {
        let opens = layout.starts.contains(&i);
        let same_row = out.last().and_then(|p| p.lines.last()).is_some_and(|prev| l.bbox.y1.min(prev.bbox.y1) - l.bbox.y0.max(prev.bbox.y0) >= 0.5 * l.bbox.height().min(prev.bbox.height()));
        // A tail lies close under the row before it; a paragraph of its own has space above.
        let close = out.last().and_then(|p| p.lines.last()).is_some_and(|prev| l.bbox.y0 - prev.bbox.y1 < 0.5 * line_size(&l));
        if opens || out.is_empty() || (!owners.contains(&owner[i]) && !same_row && (!layout.indented.contains(&i) || !close || item_marker(&l.text()))) {
            owners.clear();
            out.push(Piece { lines: Vec::new(), owner: owner[i], start: opens });
        }
        owners.push(owner[i]);
        out.last_mut().unwrap().lines.push(l);
    }
    out
}

/// A unit that can be part of a list's column.
fn eligible(u: &Unit) -> bool {
    u.kind == UnitKind::Text
        && u.refs == Ref::No
        && !u.lines.is_empty()
        && !u.lines.iter().any(|l| l.vertical)
        && !matches!(u.class, Some(TABLE | PICTURE | CAPTION | FORMULA | PAGE_HEADER | PAGE_FOOTER | TITLE | SECTION_HEADER))
}

/// The units of one column, as lists of indices: units that overlap horizontally and lie
/// no more than a line and a half apart, transitively (the entries of a list follow one
/// another; text with a stretch of page between it and the list is another matter). A
/// unit across two columns does not tie them together: it joins the one it overlaps most.
pub(super) fn columns(units: &[Option<Unit>], idx: &[usize]) -> Vec<Vec<usize>> {
    let unit = |i: usize| units[i].as_ref().unwrap();
    // The narrow units first: the columns form before a wide unit meets them.
    let mut idx: Vec<usize> = idx.to_vec();
    idx.sort_by(|&a, &b| unit(a).bbox.width().total_cmp(&unit(b).bbox.width()));
    // The groups with the horizontal extent of their units.
    let mut groups: Vec<(Vec<usize>, f32, f32)> = Vec::new();
    for i in idx {
        let b = unit(i).bbox;
        let overlap = |g: &(Vec<usize>, f32, f32)| b.x1.min(g.2) - b.x0.max(g.1);
        // The groups it touches: they overlap it, and some unit of them lies near.
        let touched: Vec<usize> = (0..groups.len())
            .filter(|&k| {
                groups[k].0.iter().any(|&j| {
                    let o = unit(j).bbox;
                    let em = unit(i).size().max(unit(j).size());
                    let gap = (b.y0 - o.y1).max(o.y0 - b.y1);
                    // Side by side on one row: a marker or a number ("2.", "•", "(a)") and its text.
                    let mark = |u: &Unit| u.lines.iter().all(|l| l.chars.iter().filter(|c| !c.c.is_whitespace()).count() <= 4);
                    let beside = gap < 0.0 && b.y1.min(o.y1) - b.y0.max(o.y0) >= 0.5 * b.height().min(o.height()) && (b.x0 - o.x1).max(o.x0 - b.x1) <= 1.5 * em && (mark(unit(i)) || mark(unit(j)));
                    beside || (b.x1.min(o.x1) - b.x0.max(o.x0) >= 0.3 * b.width().min(o.width()) && gap <= 1.5 * em)
                })
            })
            .collect();
        // Groups side by side (no overlap between them) are columns: only the one that
        // overlaps the unit most is joined.
        let beside = touched.iter().any(|&a| touched.iter().any(|&c| a != c && groups[a].2.min(groups[c].2) - groups[a].1.max(groups[c].1) <= 0.0));
        let join: Vec<usize> = if beside { touched.iter().copied().max_by(|&a, &c| overlap(&groups[a]).total_cmp(&overlap(&groups[c]))).into_iter().collect() } else { touched };
        let mut mine = (vec![i], b.x0, b.x1);
        for &k in join.iter().rev() {
            let g = groups.remove(k);
            mine.0.extend(g.0);
            mine.1 = mine.1.min(g.1);
            mine.2 = mine.2.max(g.2);
        }
        groups.push(mine);
    }
    groups.into_iter().map(|g| g.0).collect()
}

/// The units of a page with the hanging lists in them cut into their entries. The units
/// of a column that is a list keep their boundaries, except that a unit is cut at each
/// entry and pieces of one row are read together; the first lines of an entry come out as
/// [`Ref::Start`].
pub(super) fn split_hanging(units: Vec<Unit>, scan: bool, fonts: &[FontInfo], debug: bool) -> Vec<Unit> {
    let mut slots: Vec<Option<Unit>> = units.into_iter().map(Some).collect();
    let idx: Vec<usize> = (0..slots.len()).filter(|&i| eligible(slots[i].as_ref().unwrap())).collect();
    let mut made: Vec<Unit> = Vec::new();
    for col in columns(&slots, &idx) {
        let n: usize = col.iter().map(|&i| slots[i].as_ref().unwrap().lines.len()).sum();
        if n < 4 {
            continue;
        }
        // The column's lines in rows, each with the unit it came from.
        let mut all: Vec<RichLine> = Vec::with_capacity(n);
        let mut owner: Vec<usize> = Vec::with_capacity(n);
        for &i in &col {
            for l in &slots[i].as_ref().unwrap().lines {
                all.push(l.clone());
                owner.push(i);
            }
        }
        let order = row_order(&all);
        let sorted: Vec<RichLine> = order.iter().map(|&k| all[k].clone()).collect();
        let sorted_owner: Vec<usize> = order.iter().map(|&k| owner[k]).collect();
        let size = {
            let mut v: Vec<f32> = sorted.iter().flat_map(|l| l.chars.iter().map(|c| c.size)).collect();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        // The column as a whole, or else a unit of its own that holds a list among other text.
        let mut jobs: Vec<(Vec<usize>, Vec<RichLine>, Vec<usize>, Layout)> = Vec::new();
        if debug {
            eprintln!("entries: column of {} units, {n} lines", col.len());
        }
        match hanging_layout(&sorted, size, scan, fonts, debug) {
            Some(layout) => jobs.push((col.clone(), sorted, sorted_owner, layout)),
            None => {
                for &i in &col {
                    let u = slots[i].as_ref().unwrap();
                    if let Some(layout) = hanging_layout(&u.lines, u.size(), scan, fonts, debug) {
                        jobs.push((vec![i], u.lines.clone(), vec![i; u.lines.len()], layout));
                    }
                }
            }
        }
        for (members, lines, owner, layout) in jobs {
            let (class, group): (Vec<Option<usize>>, Vec<Option<usize>>) = members.iter().map(|&i| (slots[i].as_ref().unwrap().class, slots[i].as_ref().unwrap().group)).unzip();
            for p in pieces(lines, &owner, &layout) {
                let at = members.iter().position(|&i| i == p.owner).unwrap();
                let bbox = p.lines.iter().skip(1).fold(p.lines[0].bbox, |a, l| a.union(&l.bbox));
                made.push(Unit { kind: UnitKind::Text, bbox, lines: p.lines, class: class[at], group: group[at], refs: if p.start { Ref::Start } else { Ref::No } });
            }
            for &i in &members {
                slots[i] = None;
            }
        }
    }
    let mut out: Vec<Unit> = slots.into_iter().flatten().collect();
    out.extend(made);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::RectF;
    use crate::rich::RichChar;

    /// A line of `text` across `x0`..`x1`, top `y0`, size 8.
    fn line(text: &str, x0: f32, x1: f32, y0: f32) -> RichLine {
        let w = (x1 - x0) / text.chars().count() as f32;
        let chars: Vec<RichChar> = text
            .chars()
            .enumerate()
            .map(|(i, c)| RichChar { c, bbox: RectF { x0: x0 + w * i as f32, y0, x1: x0 + w * (i + 1) as f32, y1: y0 + 8.0 }, size: 8.0, font: 0, bold: false, argb: 0 })
            .collect();
        RichLine { bbox: RectF { x0, y0, x1, y1: y0 + 8.0 }, vertical: false, dir: [1.0, 0.0], joined: false, chars }
    }

    /// A unit of lines `(text, x0, x1)` from `y`, ten points apart.
    fn unit_at(y: f32, ls: &[(&str, f32, f32)]) -> Unit {
        let lines: Vec<RichLine> = ls.iter().enumerate().map(|(i, (t, x0, x1))| line(t, *x0, *x1, y + 10.0 * i as f32)).collect();
        let bbox = lines.iter().skip(1).fold(lines[0].bbox, |a, l| a.union(&l.bbox));
        Unit { kind: UnitKind::Text, bbox, lines, class: None, group: None, refs: Ref::No }
    }

    /// The units cut, top to bottom, as text ("S " in front of those that open an entry).
    fn cut(units: Vec<Unit>) -> Vec<String> {
        let mut v: Vec<(f32, String)> = split_hanging(units, false, &[], false).iter().map(|u| (u.bbox.y0, format!("{}{}", if u.refs == Ref::Start { "S " } else { "" }, u.text()))).collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v.into_iter().map(|c| c.1).collect()
    }

    #[test]
    fn hanging_entries_are_cut() {
        // Edge at 50, indent at 62, right edge at 250: an entry's last line stops short.
        let u = unit_at(
            100.0,
            &[
                ("Alpha [GEOL] a first entry that runs on over", 50.0, 250.0),
                ("a second line of the entry that is full too", 62.0, 250.0),
                ("and ends here. { al.fa }", 62.0, 160.0),
                ("Beta [HYD] one line only. { be.ta }", 50.0, 170.0),
                ("Gamma [GEOL] another entry that runs on over", 50.0, 250.0),
                ("a second line of the entry that is full too", 62.0, 250.0),
                ("end. { ga.ma }", 62.0, 110.0),
                ("Delta [GEOL] the last one starts here and", 50.0, 250.0),
                ("goes on. { del.ta }", 62.0, 140.0),
            ],
        );
        let c = cut(vec![u]);
        assert_eq!(c.len(), 4);
        assert!(c[0].starts_with("S Alpha") && c[0].ends_with("{ al.fa }"));
        assert!(c[1].starts_with("S Beta") && c[1].ends_with("{ be.ta }"));
        assert!(c[2].starts_with("S Gamma"));
        assert!(c[3].starts_with("S Delta"));
    }

    #[test]
    fn a_one_line_entry_that_fills_its_line_still_ends() {
        let u = unit_at(
            100.0,
            &[
                ("Alpha [GEOL] a first entry that runs on over", 50.0, 250.0),
                ("and ends here. { al.fa }", 62.0, 160.0),
                ("Beta [HYD] one line that goes right across. { be.ta }", 50.0, 250.0),
                ("Gamma [GEOL] another entry that runs on over", 50.0, 250.0),
                ("end. { ga.ma }", 62.0, 110.0),
                ("Delta [GEOL] the last one starts here and", 50.0, 250.0),
                ("goes on. { del.ta }", 62.0, 140.0),
            ],
        );
        let c = cut(vec![u]);
        assert_eq!(c.len(), 4);
        assert!(c[1].starts_with("S Beta") && c[2].starts_with("S Gamma"));
    }

    #[test]
    fn the_tail_of_an_entry_from_the_page_before_stays_apart() {
        let u = unit_at(
            100.0,
            &[
                ("the tail of an entry from the page before", 62.0, 250.0),
                ("and its end. { ti.ny }", 62.0, 150.0),
                ("Alpha [GEOL] an entry that runs on over the line", 50.0, 250.0),
                ("and ends here. { al.fa }", 62.0, 160.0),
                ("Beta [HYD] one line only. { be.ta }", 50.0, 170.0),
                ("Gamma [GEOL] another entry that runs on over", 50.0, 250.0),
                ("end. { ga.ma }", 62.0, 110.0),
            ],
        );
        let c = cut(vec![u]);
        assert_eq!(c.len(), 4);
        assert!(c[0].starts_with("the tail") && !c[0].starts_with("S "));
        assert!(c[1..].iter().all(|t| t.starts_with("S ")));
    }

    #[test]
    fn units_that_cut_the_entries_apart_keep_their_boundaries_but_rows_are_read_across() {
        let units = vec![
            unit_at(100.0, &[("Alpha [GEOL] a first entry that runs on over", 50.0, 250.0), ("and ends here. { al.fa }", 62.0, 160.0)]),
            // An entry whose last line is a unit of its own.
            unit_at(120.0, &[("Beta [HYD] a second entry that runs on over", 50.0, 250.0), ("and on until { be", 62.0, 250.0)]),
            unit_at(140.0, &[("ta }", 62.0, 80.0)]),
            // An entry whose first row is cut in two: the headword goes with the lines below.
            unit_at(150.0, &[("[GEOL] the rest of the first row of it that", 90.0, 250.0)]),
            unit_at(150.0, &[("Gamma", 50.0, 80.0), ("and a second line. { ga.ma }", 62.0, 170.0)]),
            unit_at(170.0, &[("Delta [GEOL] another entry that runs on over", 50.0, 250.0), ("and ends. { del.ta }", 62.0, 140.0)]),
        ];
        let c = cut(units);
        assert_eq!(c.len(), 4);
        assert!(c[0].starts_with("S Alpha") && c[0].ends_with("{ al.fa }"));
        // The tail that a unit of its own holds goes back to its entry.
        assert!(c[1].starts_with("S Beta") && c[1].ends_with("on until { be ta }"));
        assert_eq!(c[2], "S Gamma [GEOL] the rest of the first row of it that and a second line. { ga.ma }");
        assert!(c[3].starts_with("S Delta"));
    }

    #[test]
    fn units_far_apart_are_not_one_list() {
        // Two captions with a stretch of page between them are no column of entries.
        let units = vec![
            unit_at(100.0, &[("a caption that runs over the line and goes", 50.0, 250.0), ("on, and ends short.", 50.0, 120.0), ("another one starts here and goes on", 62.0, 250.0)]),
            unit_at(300.0, &[("a second one with rows at the edge and", 40.0, 240.0), ("one that is indented and long enough", 52.0, 240.0)]),
        ];
        assert_eq!(cut(units).len(), 2);
    }

    #[test]
    fn a_first_line_indent_is_no_hanging_list() {
        // Paragraphs with an indented first line: the rows before the indented ones are short.
        let u = unit_at(
            100.0,
            &[
                ("first paragraph opens here and goes on and on", 62.0, 250.0),
                ("a second line of the paragraph that is full", 50.0, 250.0),
                ("and it ends short.", 50.0, 130.0),
                ("second paragraph opens here and goes on and on", 62.0, 250.0),
                ("a second line of the paragraph that is full", 50.0, 250.0),
                ("and it ends short too.", 50.0, 140.0),
                ("third paragraph opens here and goes on and on", 62.0, 250.0),
                ("and it ends.", 50.0, 100.0),
            ],
        );
        assert_eq!(cut(vec![u]).len(), 1);
    }

    #[test]
    fn ragged_text_with_a_first_line_indent_is_left_alone() {
        // Rows stop short at random, and the rows before the indented ones most often.
        let u = unit_at(
            100.0,
            &[
                ("the first paragraph opens with an indent", 62.0, 250.0),
                ("and goes on a little", 50.0, 220.0),
                ("and ends short.", 50.0, 170.0),
                ("the second paragraph opens with one too", 62.0, 245.0),
                ("and goes on a little more", 50.0, 190.0),
                ("and on and on and on", 50.0, 235.0),
                ("and ends.", 50.0, 120.0),
                ("the third paragraph opens with an indent", 62.0, 250.0),
            ],
        );
        assert_eq!(cut(vec![u]).len(), 1);
    }

    #[test]
    fn lowercase_headwords_count_in_bold() {
        let mut u = unit_at(
            100.0,
            &[
                ("alpha [GEOL] a first entry that runs on over", 50.0, 250.0),
                ("and ends here. { al.fa }", 62.0, 160.0),
                ("beta [HYD] one line only. { be.ta }", 50.0, 170.0),
                ("gamma [GEOL] another entry that runs on over", 50.0, 250.0),
                ("end. { ga.ma }", 62.0, 110.0),
                ("delta [GEOL] the last one starts here and", 50.0, 250.0),
                ("goes on. { del.ta }", 62.0, 140.0),
            ],
        );
        // Plain lowercase rows at the edge carry on sentences: no entries.
        let plain = Unit { kind: u.kind, bbox: u.bbox, lines: u.lines.clone(), class: u.class, group: u.group, refs: u.refs };
        assert_eq!(cut(vec![plain]).len(), 1);
        // The headwords in bold open entries.
        for l in u.lines.iter_mut().filter(|l| l.bbox.x0 < 55.0) {
            for c in l.chars.iter_mut().take_while(|c| c.c != ' ') {
                c.bold = true;
            }
        }
        assert_eq!(cut(vec![u]).len(), 4);
    }

    #[test]
    fn text_without_indents_is_left_alone() {
        let u = unit_at(
            100.0,
            &[
                ("a paragraph set flush left with no indent", 50.0, 250.0),
                ("that ends short.", 50.0, 120.0),
                ("the next one starts flush left as well and", 50.0, 250.0),
                ("goes on.", 50.0, 100.0),
                ("and the third.", 50.0, 130.0),
            ],
        );
        assert_eq!(cut(vec![u]).len(), 1);
    }
}
