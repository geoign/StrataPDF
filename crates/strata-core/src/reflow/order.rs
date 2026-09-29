//! Reading order by recursive XY-cut.
//!
//! Horizontal text: a full-height gutter (column break) is cut first, left to
//! right; otherwise the region is cut into horizontal bands, top to bottom.
//! Vertical text (Japanese books): bands (tiers, 段) are cut first, top to
//! bottom, then columns right to left.

use crate::geom::RectF;

const MIN_GAP: f32 = 3.0;

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

pub fn reading_order(rects: &[RectF], vertical_text: bool) -> Vec<usize> {
    let idx: Vec<usize> = (0..rects.len()).collect();
    let mut out = Vec::with_capacity(rects.len());
    cut(rects, &idx, vertical_text, &mut out, 0);
    out
}

fn cut(rects: &[RectF], idx: &[usize], vertical_text: bool, out: &mut Vec<usize>, depth: u32) {
    if idx.len() <= 1 || depth > 64 {
        out.extend_from_slice(idx);
        return;
    }
    let (first_is_columns, gap_cols, gap_rows) = if vertical_text { (false, MIN_GAP, MIN_GAP * 2.0) } else { (true, MIN_GAP * 2.0, MIN_GAP) };
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
    let groups = if a.len() > 1 { a } else { b };
    if groups.len() > 1 {
        for g in groups {
            cut(rects, &g, vertical_text, out, depth + 1);
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

    #[test]
    fn two_columns_under_a_title() {
        // title, then left column (2 paras) and right column (2 paras) with aligned gaps
        let rects = [r(50.0, 10.0, 550.0, 40.0), r(300.0, 60.0, 550.0, 200.0), r(50.0, 60.0, 280.0, 200.0), r(50.0, 210.0, 280.0, 400.0), r(300.0, 210.0, 550.0, 400.0)];
        assert_eq!(reading_order(&rects, false), vec![0, 2, 3, 1, 4]);
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
        assert_eq!(reading_order(&rects, false), vec![0, 1, 2, 5, 6, 3, 4, 7, 8]);
    }

    #[test]
    fn vertical_text_runs_right_to_left_by_tier() {
        // upper tier: two blocks (right one first), lower tier: one block
        let rects = [r(10.0, 10.0, 100.0, 200.0), r(120.0, 10.0, 200.0, 200.0), r(10.0, 230.0, 200.0, 400.0)];
        assert_eq!(reading_order(&rects, true), vec![1, 0, 2]);
    }
}
