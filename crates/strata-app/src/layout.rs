//! Page placement in screen points for the current zoom. Pure computation, rebuilt
//! when zoom, page sizes, spread settings or viewport width change.

use egui::{Rect, pos2, vec2};
use serde::{Deserialize, Serialize};
use strata_core::SizeF;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Spread {
    #[default]
    Single,
    /// Two pages per row; pages (0,1), (2,3), ...
    Double,
    /// Two pages per row with the first page alone: 0, (1,2), (3,4), ...
    DoubleCover,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutParams {
    pub zoom: f32,
    pub spread: Spread,
    /// Right-to-left binding: the earlier page of a spread sits on the right.
    pub r2l: bool,
    pub viewport_w: f32,
    pub gap: f32,
    pub margin: f32,
    /// Put as many spreads side by side as fit the viewport (zoomed-out spread views).
    pub multi: bool,
}

/// Gap between spreads placed side by side, in multiples of `gap`.
const SPREAD_GAP: f32 = 4.0;

#[derive(Clone, Debug)]
pub struct Row {
    pub y0: f32,
    pub y1: f32,
    /// Pages in reading order (not screen order).
    pub pages: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// Page rectangles in content coordinates (origin = top-left of the scrollable content).
    pub page_rects: Vec<Rect>,
    pub rows: Vec<Row>,
    pub page_row: Vec<u32>,
    pub width: f32,
    pub height: f32,
}

pub fn rows_of(n: usize, spread: Spread) -> Vec<Vec<u32>> {
    let n = n as u32;
    match spread {
        Spread::Single => (0..n).map(|p| vec![p]).collect(),
        Spread::Double => (0..n).step_by(2).map(|p| (p..(p + 2).min(n)).collect()).collect(),
        Spread::DoubleCover => {
            let mut rows = Vec::new();
            if n > 0 {
                rows.push(vec![0]);
            }
            let mut p = 1;
            while p < n {
                rows.push((p..(p + 2).min(n)).collect());
                p += 2;
            }
            rows
        }
    }
}

/// The spread (reading-order pages shown together) containing `page`.
pub fn group_of(page: u32, n: usize, spread: Spread) -> Vec<u32> {
    let n = n as u32;
    let start = match spread {
        Spread::Single => page,
        Spread::Double => page & !1,
        Spread::DoubleCover if page == 0 => return vec![0],
        Spread::DoubleCover => page - (page - 1) % 2,
    };
    let len = if spread == Spread::Single { 1 } else { 2 };
    (start..(start + len).min(n)).collect()
}

impl Layout {
    pub fn build(sizes: &[SizeF], p: &LayoutParams) -> Layout {
        let groups = rows_of(sizes.len(), p.spread);
        let z = p.zoom;
        let row_w = |g: &[u32]| g.iter().map(|&i| sizes[i as usize].w * z).sum::<f32>() + p.gap * (g.len().saturating_sub(1)) as f32;
        let max_w = groups.iter().map(|g| row_w(g)).fold(0.0f32, f32::max);
        // Each spread gets a cell `max_w` wide; zoomed out, several cells share a row.
        let cell_gap = p.gap * SPREAD_GAP;
        let per_row = if p.multi && p.spread != Spread::Single && max_w > 0.0 {
            (((p.viewport_w - 2.0 * p.margin + cell_gap) / (max_w + cell_gap)).floor() as usize).max(1)
        } else {
            1
        };
        let row_span = per_row as f32 * max_w + (per_row - 1) as f32 * cell_gap;
        let width = (row_span + 2.0 * p.margin).max(p.viewport_w);
        let x_start = (width - row_span) * 0.5;
        let mut page_rects = vec![Rect::NOTHING; sizes.len()];
        let mut page_row = vec![0u32; sizes.len()];
        let mut rows = Vec::with_capacity(groups.len().div_ceil(per_row));
        let mut y = p.margin;
        for (ri, cells) in groups.chunks(per_row).enumerate() {
            let pages: Vec<u32> = cells.concat();
            let h = pages.iter().map(|&i| sizes[i as usize].h * z).fold(0.0f32, f32::max);
            for (ci, g) in cells.iter().enumerate() {
                // Right-to-left books fill a row from the right.
                let slot = if p.r2l { per_row - 1 - ci } else { ci };
                let cell_x = x_start + slot as f32 * (max_w + cell_gap);
                let w = row_w(g);
                let mut x = cell_x + (max_w - w) * 0.5;
                // A lone page in a spread layout keeps its side of the gutter, so that
                // pages do not jump sideways between rows.
                if g.len() == 1 && p.spread != Spread::Single && w < max_w {
                    // In reading order, which pages are the left-hand page of a spread?
                    let lone_left = (g[0] % 2 == 1) == (p.spread == Spread::DoubleCover);
                    let on_left = lone_left != p.r2l;
                    let center = cell_x + max_w * 0.5;
                    x = if on_left { center - p.gap * 0.5 - w } else { center + p.gap * 0.5 };
                }
                let order: Vec<u32> = if p.r2l { g.iter().rev().copied().collect() } else { g.clone() };
                for &i in &order {
                    let s = sizes[i as usize];
                    let (pw, ph) = (s.w * z, s.h * z);
                    let top = y + (h - ph) * 0.5;
                    page_rects[i as usize] = Rect::from_min_size(pos2(x, top), vec2(pw, ph));
                    page_row[i as usize] = ri as u32;
                    x += pw + p.gap;
                }
            }
            rows.push(Row { y0: y, y1: y + h, pages });
            y += h + p.gap;
        }
        let height = y - p.gap + p.margin;
        Layout { page_rects, rows, page_row, width, height: height.max(0.0) }
    }

    /// Rows intersecting the vertical range `[y0, y1)`.
    pub fn rows_in(&self, y0: f32, y1: f32) -> std::ops::Range<usize> {
        let start = self.rows.partition_point(|r| r.y1 < y0);
        let end = self.rows.partition_point(|r| r.y0 < y1);
        start..end.max(start)
    }

    /// Row containing (or nearest above) the content-space `y`.
    pub fn row_at(&self, y: f32) -> usize {
        self.rows.partition_point(|r| r.y1 < y).min(self.rows.len().saturating_sub(1))
    }

    /// Page nearest to a content-space point.
    pub fn page_at(&self, pos: egui::Pos2) -> Option<u32> {
        let r = self.rows.get(self.row_at(pos.y))?;
        r.pages
            .iter()
            .copied()
            .min_by(|&a, &b| {
                let da = self.page_rects[a as usize].distance_sq_to_pos(pos);
                let db = self.page_rects[b as usize].distance_sq_to_pos(pos);
                da.total_cmp(&db)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a4(n: usize) -> Vec<SizeF> {
        vec![SizeF { w: 595.0, h: 842.0 }; n]
    }

    fn params(spread: Spread, r2l: bool) -> LayoutParams {
        LayoutParams { zoom: 1.0, spread, r2l, viewport_w: 800.0, gap: 10.0, margin: 10.0, multi: false }
    }

    #[test]
    fn spread_of_a_page() {
        assert_eq!(group_of(3, 10, Spread::Double), vec![2, 3]);
        assert_eq!(group_of(3, 10, Spread::DoubleCover), vec![3, 4]);
        assert_eq!(group_of(0, 10, Spread::DoubleCover), vec![0]);
        assert_eq!(group_of(9, 10, Spread::DoubleCover), vec![9]);
        assert_eq!(group_of(4, 5, Spread::Double), vec![4]);
    }

    #[test]
    fn zoomed_out_spreads_share_rows() {
        // Spreads are 1200 wide: three fit a 4000-wide viewport.
        let p = LayoutParams { viewport_w: 4000.0, multi: true, ..params(Spread::DoubleCover, false) };
        let l = Layout::build(&a4(9), &p);
        assert_eq!(l.rows.iter().map(|r| r.pages.clone()).collect::<Vec<_>>(), vec![vec![0, 1, 2, 3, 4], vec![5, 6, 7, 8]]);
        // Reading order runs left to right, and the cover keeps its right-hand side.
        let x = |i: usize| l.page_rects[i].min.x;
        assert!(x(0) < x(1) && x(1) < x(2) && x(2) < x(3));
        assert!(x(1) - l.page_rects[0].max.x < 10.0 * SPREAD_GAP + 1.0);
        // Columns line up between rows.
        assert_eq!(x(1), x(7));
        // Right to left: the first spread sits at the right edge.
        let l = Layout::build(&a4(9), &LayoutParams { r2l: true, ..p });
        let x = |i: usize| l.page_rects[i].min.x;
        assert!(x(0) > x(1) && x(1) > x(2) && x(2) > x(3));
        // Not zoomed out enough: one spread per row, as before.
        let l = Layout::build(&a4(9), &LayoutParams { viewport_w: 2000.0, ..p });
        assert_eq!(l.rows.len(), 5);
    }

    #[test]
    fn cover_spread_groups() {
        assert_eq!(rows_of(5, Spread::DoubleCover), vec![vec![0], vec![1, 2], vec![3, 4]]);
        assert_eq!(rows_of(3, Spread::Double), vec![vec![0, 1], vec![2]]);
    }

    #[test]
    fn right_to_left_puts_first_page_on_right() {
        let l = Layout::build(&a4(4), &params(Spread::Double, true));
        assert!(l.page_rects[0].min.x > l.page_rects[1].min.x);
        let l = Layout::build(&a4(4), &params(Spread::Double, false));
        assert!(l.page_rects[0].min.x < l.page_rects[1].min.x);
    }

    #[test]
    fn cover_page_sits_on_the_reading_side() {
        // Left-to-right books open with the cover on the right; right-to-left on the left.
        let l = Layout::build(&a4(3), &params(Spread::DoubleCover, false));
        assert!(l.page_rects[0].min.x >= l.width * 0.5);
        let l = Layout::build(&a4(3), &params(Spread::DoubleCover, true));
        assert!(l.page_rects[0].max.x <= l.width * 0.5);
    }

    #[test]
    fn visible_rows() {
        let l = Layout::build(&a4(10), &params(Spread::Single, false));
        let r = l.rows_in(0.0, 900.0);
        assert_eq!(r, 0..2);
        assert_eq!(l.row_at(l.rows[5].y0 + 1.0), 5);
    }
}
