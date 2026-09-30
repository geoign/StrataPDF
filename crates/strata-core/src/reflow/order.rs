//! Reading order by recursive XY-cut.
//!
//! Horizontal text: a full-height gutter (column break) is cut first, left to
//! right; failing that, a soft gutter in the projection of the lines (columns
//! whose boxes touch); otherwise the region is cut into horizontal bands, top
//! to bottom.
//! Vertical text (Japanese books): bands (tiers, 段) are cut first, top to
//! bottom, then columns right to left.

use crate::geom::RectF;

const MIN_GAP: f32 = 3.0;
/// Lines on each side of a soft gutter, at least, for the sides to be columns.
const MIN_COLUMN_LINES: usize = 8;

/// Groups of indices separated by empty bands along one axis, in ascending order.
fn split(rects: &[RectF], idx: &[usize], vertical_axis: bool, min_gap: f32) -> Vec<Vec<usize>> {
    let key = |r: &RectF| if vertical_axis { (r.y0, r.y1) } else { (r.x0, r.x1) };
    let mut sorted: Vec<usize> = idx.to_vec();
    sorted.sort_by(|&a, &b| key(&rects[a]).0.total_cmp(&key(&rects[b]).0));
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut end = f32::NEG_INFINITY;
    for i in sorted {
        let (s, e) = key(&rects[i]);
        if groups.is_empty() || s > end + min_gap {
            groups.push(vec![i]);
            end = e;
        } else {
            groups.last_mut().unwrap().push(i);
            end = end.max(e);
        }
    }
    groups
}

/// Gutters (empty x-intervals between columns) of a group, left to right.
fn gutters(rects: &[RectF], idx: &[usize], min_gap: f32) -> Vec<(f32, f32)> {
    let cols = split(rects, idx, false, min_gap);
    cols.windows(2)
        .map(|w| {
            let l = w[0].iter().map(|&i| rects[i].x1).fold(f32::NEG_INFINITY, f32::max);
            let r = w[1].iter().map(|&i| rects[i].x0).fold(f32::INFINITY, f32::min);
            (l, r)
        })
        .collect()
}

/// Horizontal bands that share the same column gutters belong to one
/// multi-column region; cutting between them would interleave the columns. A
/// band with a single column (a subheading in one column, level with a gap in
/// the other) belongs to the region too, as long as nothing in it crosses a
/// gutter; otherwise it would split the region and interleave the columns.
fn merge_column_bands(rects: &[RectF], bands: Vec<Vec<usize>>, min_gap: f32) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    for b in bands {
        if let Some(last) = out.last_mut() {
            let g1 = gutters(rects, last, min_gap);
            let g2 = gutters(rects, &b, min_gap);
            let same_gutters = g1.len() == g2.len() && g1.iter().zip(&g2).all(|(a, b)| a.0.max(b.0) < a.1.min(b.1));
            let within_columns = g2.is_empty() && b.iter().all(|&i| g1.iter().all(|g| rects[i].x1 <= g.1 || rects[i].x0 >= g.0));
            let compatible = !g1.is_empty() && (same_gutters || within_columns);
            if compatible {
                last.extend(b);
                continue;
            }
        }
        out.push(b);
    }
    out
}

/// Area that two boxes share.
fn overlap_area(a: &RectF, b: &RectF) -> f32 {
    (a.x1.min(b.x1) - a.x0.max(b.x0)).max(0.0) * (a.y1.min(b.y1) - a.y0.max(b.y0)).max(0.0)
}

/// Groups of units to cut further, each with whether columns run beside it.
type Groups = Vec<(Vec<usize>, bool)>;

/// The lines of a box, for the gutters that its box alone does not show.
pub struct Lines {
    /// The line boxes (a figure has none; its box stands in).
    pub boxes: Vec<RectF>,
    /// The type size.
    pub size: f32,
}

/// The gutter (empty x-interval) that the lines of a region show where its unit
/// boxes touch or overlap (ragged or OCR text) or a wide block (an abstract, a
/// table) joins them. In the projection of the lines onto x the gutter is a
/// valley that few lines cross, compared with the columns beside it: those need
/// many lines of running text, which cells of a table are not. `narrow`: the
/// lines to project; `all`: every line, for those that cross the gutter.
fn soft_gutter(narrow: &[&RectF], all: &[&RectF], region: &RectF, body: f32) -> Option<(f32, f32)> {
    // Number of lines over each point of the region's width.
    let mut cover = vec![0usize; region.width().ceil() as usize + 2];
    for l in narrow {
        let (a, b) = (((l.x0 - region.x0) as usize).min(cover.len() - 1), ((l.x1 - region.x0).ceil() as usize).min(cover.len()));
        cover[a..b.max(a)].iter_mut().for_each(|c| *c += 1);
    }
    let first = cover.iter().position(|&c| c > 0)?;
    let last = cover.iter().rposition(|&c| c > 0)?;
    let mean = |a: usize, b: usize| cover[a..b.max(a)].iter().sum::<usize>() as f32 / b.saturating_sub(a).max(1) as f32;
    // Runs of points that at most `limit` lines cover.
    let runs = |from: usize, to: usize, limit: f32| {
        let (mut out, mut k) = (Vec::new(), from);
        while k < to {
            if cover[k] as f32 > limit {
                k += 1;
                continue;
            }
            let s = k;
            while k < to && cover[k] as f32 <= limit {
                k += 1;
            }
            out.push((s, k));
        }
        out
    };
    // The one that fewest lines cross, then the widest.
    let mut best: Option<(usize, usize, (f32, f32))> = None;
    for (s, e) in runs(first, last + 1, narrow.len() as f32 * 0.15) {
        let limit = mean(first, s).min(mean(e, last + 1)) * 0.25;
        for (a, b) in runs(s, e, limit) {
            let (l, r) = (region.x0 + a as f32, region.x0 + b as f32);
            // Running text, in lines as long as 5 em, reaches the gutter from both sides.
            let near = |left: bool| {
                let n = narrow.iter().filter(|x| if left { x.center().0 < l && x.x1 > l - body * 4.0 } else { x.center().0 > r && x.x0 < r + body * 4.0 });
                n.fold((0, 0), |(n, t), x| (n + 1, t + (x.width() >= body * 5.0) as usize))
            };
            let ((nl, tl), (nr, tr)) = (near(true), near(false));
            if tl < MIN_COLUMN_LINES || tl * 2 < nl || tr < MIN_COLUMN_LINES || tr * 2 < nr {
                continue;
            }
            let crossing = all.iter().filter(|x| x.x0 < l && x.x1 > r).count();
            if best.is_none_or(|(c, w, _)| crossing < c || (crossing == c && b - a > w)) {
                best = Some((crossing, b - a, (l, r)));
            }
        }
    }
    best.map(|(.., g)| g)
}

/// Columns divided by a soft gutter, as groups of units in reading order, to be
/// cut further, each with whether columns run beside it. A unit that spans the
/// gutter (a title, a table, an abstract) or lies far above or below the text of
/// its side (the labels of a diagram) makes a band of its own between the bands
/// of columns: for each, its left column, its right column, then the spanning
/// unit. `part`: the region is one of several columns side by side; then a
/// spanning unit between the texts of the columns (a pull quote) is read in the
/// left column, at its place. `lines`: the lines of each unit.
fn soft_columns(rects: &[RectF], lines: &[Lines], idx: &[usize], body: f32, part: bool) -> Option<Groups> {
    let of = |i: usize| -> &[RectF] { if lines[i].boxes.is_empty() { std::slice::from_ref(&rects[i]) } else { &lines[i].boxes } };
    let all: Vec<&RectF> = idx.iter().flat_map(|&i| of(i)).collect();
    let region = all.iter().fold(**all.first()?, |a, l| a.union(l));
    // Wide units (a table across the columns) would fill the valley.
    let narrow: Vec<&RectF> = idx.iter().filter(|&&i| rects[i].width() <= region.width() * 0.6).flat_map(|&i| of(i)).collect();
    if narrow.len() < MIN_COLUMN_LINES * 2 || region.width() >= 1e4 {
        return None;
    }
    let (gl, gr) = soft_gutter(&narrow, &all, &region, body)?;
    let mid = (gl + gr) / 2.0;
    // The vertical extent of the running text on each side.
    let extent = |left: bool| {
        let t = narrow.iter().filter(|l| l.width() >= body * 5.0 && (l.center().0 < mid) == left);
        t.fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), l| (a.min(l.y0), b.max(l.y1)))
    };
    let (ext_l, ext_r) = (extent(true), extent(false));
    let (top, bottom) = (ext_l.0.min(ext_r.0), ext_l.1.max(ext_r.1));
    // A unit spans the gutter if half of its lines cross it, or if it lies on both sides of it.
    let (mut spans, mut sides): (Vec<usize>, Vec<(usize, bool)>) = (Vec::new(), Vec::new());
    for &i in idx {
        let ls = of(i);
        let crossing = ls.iter().filter(|l| l.x0 < gl && l.x1 > gr).count();
        let left = ls.iter().filter(|l| l.center().0 < mid).count();
        let both = rects[i].x0 < gl && rects[i].x1 > gr && left.min(ls.len() - left) * 4 >= ls.len();
        let is_left = left * 2 > ls.len() || (left * 2 == ls.len() && rects[i].center().0 < mid);
        let (t, b) = if is_left { ext_l } else { ext_r };
        if crossing * 2 >= ls.len() || both || rects[i].y0 >= b + body * 2.0 || rects[i].y1 <= t - body * 2.0 {
            spans.push(i);
        } else {
            sides.push((i, is_left));
        }
    }
    // Units on the two sides that make one row across the gutter, level with each
    // other: the columns of a table (short lines), or the halves of a caption or
    // title (a few short lines each, in one type size, not the body's) that touch.
    let level = |i: usize, j: usize| rects[j].y0 < rects[i].y1 && rects[i].y0 < rects[j].y1;
    let mut cells = vec![false; rects.len()];
    for &i in idx {
        let mut w: Vec<f32> = of(i).iter().map(|l| l.width()).collect();
        w.sort_by(f32::total_cmp);
        cells[i] = w.len() >= 4 && w[w.len() / 2] < body * 5.0;
    }
    let alike = |i: usize, j: usize| [i, j].iter().all(|&u| lines[u].size > 0.0 && (lines[u].size < body * 0.92 || lines[u].size > body * 1.1)) && (lines[i].size - lines[j].size).abs() <= lines[i].size.max(lines[j].size) * 0.1;
    let halves = |i: usize, j: usize| alike(i, j) && of(i).len() <= 3 && of(j).len() <= 3 && rects[j].x0 - rects[i].x1 < body * 0.5;
    let row = |(i, l): (usize, bool), (j, m): (usize, bool)| l != m && level(i, j) && ((cells[i] && cells[j]) || (l && halves(i, j)) || (m && halves(j, i)));
    let across: Vec<usize> = sides.iter().filter(|&&a| sides.iter().any(|&b| row(a, b))).map(|&(i, _)| i).collect();
    spans.extend(&across);
    sides.retain(|&(i, _)| !across.contains(&i));
    let small = |u: usize| lines[u].size > 0.0 && lines[u].size < body * 0.92;
    // The spanning units, in bands with their y-extent and units. Units in smaller
    // type next to one go with it: the caption and title of a table, the two halves
    // of a caption (level with each other across the gutter); so do the labels of a
    // diagram, inside it, and the columns of a table, level with it.
    let all_sides = sides.clone();
    let mut floats: Vec<(RectF, Vec<usize>)> = spans.iter().map(|&i| (rects[i], vec![i])).collect();
    while !floats.is_empty() {
        floats.sort_by(|a, b| a.0.y0.total_cmp(&b.0.y0));
        let mut merged: Vec<(RectF, Vec<usize>)> = Vec::new();
        for f in floats {
            match merged.last_mut() {
                Some(m) if f.0.y0 <= m.0.y1 + MIN_GAP => {
                    m.0 = m.0.union(&f.0);
                    m.1.extend(f.1);
                }
                _ => merged.push(f),
            }
        }
        floats = merged;
        let n = sides.len();
        sides.retain(|&(u, l)| {
            let pair = all_sides.iter().any(|&(j, m)| m != l && small(j) && level(u, j));
            let hit = floats.iter_mut().find(|f| {
                let gap = (rects[u].y0 - f.0.y1).max(f.0.y0 - rects[u].y1);
                let apart = (rects[u].x0 - f.0.x1).max(f.0.x0 - rects[u].x1);
                let inside = overlap_area(&rects[u], &f.0) >= rects[u].width() * rects[u].height() / 2.0;
                inside || (cells[u] && -gap >= rects[u].height() / 2.0) || (small(u) && apart <= body * 1.5 && gap <= if pair { body * 1.5 } else { MIN_GAP })
            });
            hit.map(|f| {
                f.0 = f.0.union(&rects[u]);
                f.1.push(u);
            })
            .is_none()
        });
        if sides.len() == n {
            break;
        }
    }
    let (mut lead, mut trail): (Groups, Groups) = (Vec::new(), Vec::new());
    if part {
        for f in std::mem::take(&mut floats) {
            if f.0.y1 <= top + MIN_GAP {
                lead.push((f.1, false));
            } else if f.0.y0 >= bottom - MIN_GAP {
                trail.push((f.1, false));
            } else {
                sides.extend(f.1.into_iter().map(|u| (u, true)));
            }
        }
    }
    let mut groups = lead;
    for band in 0..=floats.len() {
        // The units between two floats: those with as many floats above them.
        let above = |i: usize| floats.iter().filter(|f| f.0.y1 <= rects[i].center().1).count();
        for side in [true, false] {
            let g: Vec<usize> = sides.iter().filter(|&&(i, l)| l == side && above(i) == band).map(|&(i, _)| i).collect();
            if !g.is_empty() {
                groups.push((g, true));
            }
        }
        if let Some(f) = floats.get(band) {
            groups.push((f.1.clone(), false));
        }
    }
    groups.extend(trail);
    (groups.len() > 1).then_some(groups)
}

/// Reading order of the boxes. `lines`: the lines of each box and `body`: the
/// body type size, for the columns whose boxes leave no gutter (see
/// [`soft_columns`]). `imprecise`: the boxes come from OCR (a text layer over a
/// scan), whose lines run into the gutter (trailing spaces, stray marks):
/// columns may then touch or overlap by a few points.
pub fn reading_order(rects: &[RectF], lines: &[Lines], body: f32, vertical_text: bool, imprecise: bool) -> Vec<usize> {
    let idx: Vec<usize> = (0..rects.len()).collect();
    let mut out = Vec::with_capacity(rects.len());
    cut(rects, lines, body, &idx, false, vertical_text, imprecise, &mut out, 0);
    out
}

#[allow(clippy::too_many_arguments)]
fn cut(rects: &[RectF], lines: &[Lines], body: f32, idx: &[usize], part: bool, vertical_text: bool, imprecise: bool, out: &mut Vec<usize>, depth: u32) {
    if idx.len() <= 1 || depth > 64 {
        out.extend_from_slice(idx);
        return;
    }
    let (first_is_columns, gap_cols, gap_rows) = match (vertical_text, imprecise) {
        (true, _) => (false, MIN_GAP, MIN_GAP * 2.0),
        (false, false) => (true, MIN_GAP * 2.0, MIN_GAP),
        (false, true) => (true, -MIN_GAP, MIN_GAP),
    };
    let cols = || {
        let mut g = split(rects, idx, false, gap_cols);
        if vertical_text {
            g.reverse();
        }
        g
    };
    let rows = || {
        let g = split(rects, idx, true, gap_rows);
        if vertical_text { g } else { merge_column_bands(rects, g, gap_cols) }
    };
    let (a, b) = if first_is_columns { (cols(), rows()) } else { (rows(), cols()) };
    // Each group with whether columns run beside it (`part`).
    let with = |g: Vec<Vec<usize>>, part: bool| g.into_iter().map(|g| (g, part)).collect::<Vec<_>>();
    let groups = if a.len() > 1 {
        with(a, first_is_columns)
    } else if !vertical_text && let Some(g) = soft_columns(rects, lines, idx, body, part) {
        g
    } else {
        with(b, part)
    };
    if groups.len() > 1 {
        for (g, part) in groups {
            cut(rects, lines, body, &g, part, vertical_text, imprecise, out, depth + 1);
        }
        return;
    }
    // Overlapping items: fall back to position order.
    let mut v = idx.to_vec();
    if vertical_text {
        v.sort_by(|&a, &b| rects[b].x1.total_cmp(&rects[a].x1).then(rects[a].y0.total_cmp(&rects[b].y0)));
    } else {
        v.sort_by(|&a, &b| rects[a].y0.total_cmp(&rects[b].y0).then(rects[a].x0.total_cmp(&rects[b].x0)));
    }
    out.extend(v);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x0: f32, y0: f32, x1: f32, y1: f32) -> RectF {
        RectF { x0, y0, x1, y1 }
    }

    /// Reading order of units that are one line each.
    fn order(rects: &[RectF], vertical_text: bool, imprecise: bool) -> Vec<usize> {
        let lines: Vec<Lines> = rects.iter().map(|&b| Lines { boxes: vec![b], size: 10.0 }).collect();
        reading_order(rects, &lines, 10.0, vertical_text, imprecise)
    }

    #[test]
    fn two_columns_under_a_title() {
        // title, then left column (2 paras) and right column (2 paras) with aligned gaps
        let rects = [r(50.0, 10.0, 550.0, 40.0), r(300.0, 60.0, 550.0, 200.0), r(50.0, 60.0, 280.0, 200.0), r(50.0, 210.0, 280.0, 400.0), r(300.0, 210.0, 550.0, 400.0)];
        assert_eq!(order(&rects, false, false), vec![0, 2, 3, 1, 4]);
    }

    #[test]
    fn subheading_in_one_column_does_not_interleave() {
        // Carey et al. 2010, p. 263: figure on top; below it two columns where a
        // one-line subheading in the left column sits level with a gap in the right.
        let rects = [
            r(51.0, 53.5, 161.6, 170.8),  // 0 caption
            r(232.0, 53.9, 544.4, 341.0), // 1 figure
            r(51.0, 370.4, 289.1, 541.4), // 2 left: "the proximal area…"
            r(306.1, 370.4, 544.3, 454.2), // 3 right
            r(306.1, 457.6, 544.2, 553.8), // 4 right
            r(51.0, 557.2, 105.2, 566.3), // 5 left: "Unit D (Fall)"
            r(51.0, 582.1, 289.1, 715.7), // 6 left
            r(306.1, 582.4, 525.4, 591.2), // 7 right: heading
            r(306.1, 607.1, 544.4, 715.7), // 8 right
        ];
        assert_eq!(order(&rects, false, false), vec![0, 1, 2, 5, 6, 3, 4, 7, 8]);
    }

    /// A block of `n` text lines, one every 12 pt from `y`, between `x0` and `x1`;
    /// `ragged`: lines that run to other edges (line, x0, x1).
    fn block(x0: f32, x1: f32, y: f32, n: usize, ragged: &[(usize, f32, f32)]) -> (RectF, Lines) {
        let boxes: Vec<RectF> = (0..n)
            .map(|i| {
                let (a, b) = ragged.iter().find(|g| g.0 == i).map_or((x0, x1), |g| (g.1, g.2));
                r(a, y + 12.0 * i as f32, b, y + 12.0 * i as f32 + 10.0)
            })
            .collect();
        let bbox = boxes.iter().fold(boxes[0], |a, b| a.union(b));
        (bbox, Lines { boxes, size: 10.0 })
    }

    fn order_blocks(units: Vec<(RectF, Lines)>) -> Vec<usize> {
        let (rects, lines): (Vec<RectF>, Vec<Lines>) = units.into_iter().unzip();
        reading_order(&rects, &lines, 10.0, false, false)
    }

    #[test]
    fn ragged_columns_that_overlap_the_gutter() {
        // OCR text: a line of each column runs into the other, so the boxes overlap
        // (left up to 310, right from 296) although the lines leave a valley at 290-306.
        let left = |y| block(50.0, 290.0, y, 10, &[(3, 50.0, 310.0)]);
        let right = |y| block(306.0, 546.0, y, 10, &[(5, 296.0, 546.0)]);
        // input order: L1, R1, L2, R2 (bands would read them paragraph by paragraph)
        let units = vec![left(100.0), right(100.0), left(240.0), right(240.0)];
        assert_eq!(order_blocks(units), vec![0, 2, 1, 3]);
    }

    #[test]
    fn columns_that_touch() {
        // The boxes touch at x = 300; the lines end at 294 and start at 306.
        let left = |y| block(50.0, 294.0, y, 12, &[(4, 50.0, 300.0)]);
        let right = |y| block(306.0, 550.0, y, 12, &[(7, 300.0, 550.0)]);
        let units = vec![right(100.0), left(100.0), right(260.0), left(260.0)];
        assert_eq!(order_blocks(units), vec![1, 3, 0, 2]);
    }

    #[test]
    fn table_across_two_ragged_columns() {
        // Columns above and below a full-width table: read the upper columns (each
        // in full), the table, then the lower columns.
        let left = |y, n| block(50.0, 290.0, y, n, &[(3, 50.0, 310.0)]);
        let right = |y, n| block(306.0, 546.0, y, n, &[(2, 296.0, 546.0)]);
        let table = block(50.0, 546.0, 240.0, 4, &[]);
        // input order: L1a, R1a, L1b, R1b, table, L2, R2
        let units = vec![left(100.0, 5), right(100.0, 5), left(170.0, 5), right(170.0, 5), table, left(340.0, 10), right(340.0, 10)];
        assert_eq!(order_blocks(units), vec![0, 2, 1, 3, 4, 5, 6]);
    }

    #[test]
    fn pull_quote_across_two_of_three_columns() {
        // Dziak et al. 2012, p. 120: the text of the first two columns runs on below
        // the quote; the third column runs beside it.
        let a1 = block(50.0, 190.0, 100.0, 5, &[]);
        let quote = block(50.0, 346.0, 180.0, 5, &[]);
        let a2 = block(50.0, 190.0, 300.0, 10, &[]);
        let b1 = block(206.0, 350.0, 100.0, 5, &[(2, 206.0, 360.0)]);
        let b2 = block(206.0, 346.0, 300.0, 10, &[]);
        let c = block(356.0, 502.0, 100.0, 25, &[(9, 350.0, 502.0)]);
        // input order: c, b2, quote, a2, b1, a1
        let units = vec![c, b2, quote, a2, b1, a1];
        assert_eq!(order_blocks(units), vec![5, 2, 3, 4, 1, 0]);
    }

    #[test]
    fn one_column_is_not_split() {
        let units = vec![block(50.0, 546.0, 300.0, 10, &[]), block(50.0, 546.0, 100.0, 10, &[]), block(70.0, 546.0, 200.0, 8, &[])];
        assert_eq!(order_blocks(units), vec![1, 2, 0]);
    }

    #[test]
    fn vertical_text_runs_right_to_left_by_tier() {
        // upper tier: two blocks (right one first), lower tier: one block
        let rects = [r(10.0, 10.0, 100.0, 200.0), r(120.0, 10.0, 200.0, 200.0), r(10.0, 230.0, 200.0, 400.0)];
        assert_eq!(order(&rects, true, false), vec![1, 0, 2]);
    }
}
