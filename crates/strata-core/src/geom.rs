//! Plain geometry types that are `Send` and independent of MuPDF handles.
//! Coordinates are PDF user space as MuPDF reports them: points, y down.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SizeF {
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RectF {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl RectF {
    pub const EMPTY: RectF = RectF { x0: f32::INFINITY, y0: f32::INFINITY, x1: f32::NEG_INFINITY, y1: f32::NEG_INFINITY };

    pub fn width(&self) -> f32 {
        self.x1 - self.x0
    }
    pub fn height(&self) -> f32 {
        self.y1 - self.y0
    }
    pub fn is_empty(&self) -> bool {
        !(self.x1 > self.x0 && self.y1 > self.y0)
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
    pub fn union(&self, o: &RectF) -> RectF {
        RectF { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }
    pub fn center(&self) -> (f32, f32) {
        ((self.x0 + self.x1) * 0.5, (self.y0 + self.y1) * 0.5)
    }
    /// Squared distance from a point to the rectangle (0 inside).
    pub fn dist2(&self, x: f32, y: f32) -> f32 {
        let dx = (self.x0 - x).max(0.0).max(x - self.x1);
        let dy = (self.y0 - y).max(0.0).max(y - self.y1);
        dx * dx + dy * dy
    }
}

impl From<mupdf::Rect> for RectF {
    fn from(r: mupdf::Rect) -> Self {
        RectF { x0: r.x0, y0: r.y0, x1: r.x1, y1: r.y1 }
    }
}

/// Quadrilateral in the order MuPDF uses: upper-left, upper-right, lower-left, lower-right.
/// Vertical text and rotated text produce non-axis-aligned quads.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QuadF {
    pub ul: [f32; 2],
    pub ur: [f32; 2],
    pub ll: [f32; 2],
    pub lr: [f32; 2],
}

impl QuadF {
    pub fn bbox(&self) -> RectF {
        let xs = [self.ul[0], self.ur[0], self.ll[0], self.lr[0]];
        let ys = [self.ul[1], self.ur[1], self.ll[1], self.lr[1]];
        RectF {
            x0: xs.iter().copied().fold(f32::INFINITY, f32::min),
            y0: ys.iter().copied().fold(f32::INFINITY, f32::min),
            x1: xs.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            y1: ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
        }
    }
}

impl From<mupdf::Quad> for QuadF {
    fn from(q: mupdf::Quad) -> Self {
        QuadF { ul: [q.ul.x, q.ul.y], ur: [q.ur.x, q.ur.y], ll: [q.ll.x, q.ll.y], lr: [q.lr.x, q.lr.y] }
    }
}
