//! Annotations: listing, creation, modification and deletion on the document
//! thread, with undo/redo, and saving (incremental or full, optionally without
//! encryption). Coordinates are MuPDF page space (points, y down).

use mupdf::pdf::{LineEndingStyle, PdfAnnotation, PdfAnnotationType, PdfDocument, PdfPage};
use mupdf::color::AnnotationColor;
use mupdf::{Point, Quad, Rect};
use serde::{Deserialize, Serialize};

use crate::geom::{QuadF, RectF};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AnnotKind {
    Highlight,
    Underline,
    StrikeOut,
    Squiggly,
    Square,
    Circle,
    Line,
    Ink,
    FreeText,
    /// Sticky note.
    Text,
    /// Any other type; shown and deletable but not editable here.
    Other(String),
}

impl AnnotKind {
    pub fn label(&self) -> &str {
        match self {
            AnnotKind::Highlight => "ハイライト",
            AnnotKind::Underline => "下線",
            AnnotKind::StrikeOut => "取り消し線",
            AnnotKind::Squiggly => "波線",
            AnnotKind::Square => "矩形",
            AnnotKind::Circle => "楕円",
            AnnotKind::Line => "線",
            AnnotKind::Ink => "手書き",
            AnnotKind::FreeText => "テキスト",
            AnnotKind::Text => "付箋",
            AnnotKind::Other(s) => s,
        }
    }

    pub fn is_markup(&self) -> bool {
        matches!(self, AnnotKind::Highlight | AnnotKind::Underline | AnnotKind::StrikeOut | AnnotKind::Squiggly)
    }

    fn pdf_type(&self) -> Option<PdfAnnotationType> {
        Some(match self {
            AnnotKind::Highlight => PdfAnnotationType::Highlight,
            AnnotKind::Underline => PdfAnnotationType::Underline,
            AnnotKind::StrikeOut => PdfAnnotationType::StrikeOut,
            AnnotKind::Squiggly => PdfAnnotationType::Squiggly,
            AnnotKind::Square => PdfAnnotationType::Square,
            AnnotKind::Circle => PdfAnnotationType::Circle,
            AnnotKind::Line => PdfAnnotationType::Line,
            AnnotKind::Ink => PdfAnnotationType::Ink,
            AnnotKind::FreeText => PdfAnnotationType::FreeText,
            AnnotKind::Text => PdfAnnotationType::Text,
            AnnotKind::Other(_) => return None,
        })
    }

    fn from_pdf(t: PdfAnnotationType) -> AnnotKind {
        match t {
            PdfAnnotationType::Highlight => AnnotKind::Highlight,
            PdfAnnotationType::Underline => AnnotKind::Underline,
            PdfAnnotationType::StrikeOut => AnnotKind::StrikeOut,
            PdfAnnotationType::Squiggly => AnnotKind::Squiggly,
            PdfAnnotationType::Square => AnnotKind::Square,
            PdfAnnotationType::Circle => AnnotKind::Circle,
            PdfAnnotationType::Line => AnnotKind::Line,
            PdfAnnotationType::Ink => AnnotKind::Ink,
            PdfAnnotationType::FreeText => AnnotKind::FreeText,
            PdfAnnotationType::Text => AnnotKind::Text,
            other => AnnotKind::Other(format!("{other:?}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnnotSpec {
    pub kind: AnnotKind,
    pub rect: RectF,
    /// Text markup quads.
    pub quads: Vec<QuadF>,
    /// Line end points.
    pub line: Option<([f32; 2], [f32; 2])>,
    pub arrow: bool,
    pub ink: Vec<Vec<[f32; 2]>>,
    pub color: [f32; 3],
    pub fill: Option<[f32; 3]>,
    pub width: f32,
    pub opacity: f32,
    pub contents: String,
    pub font_size: f32,
    pub author: String,
}

impl AnnotSpec {
    pub fn new(kind: AnnotKind, color: [f32; 3]) -> AnnotSpec {
        AnnotSpec {
            kind,
            rect: RectF::default(),
            quads: Vec::new(),
            line: None,
            arrow: false,
            ink: Vec::new(),
            color,
            fill: None,
            width: 1.5,
            opacity: 1.0,
            contents: String::new(),
            font_size: 12.0,
            author: std::env::var("USERNAME").unwrap_or_default(),
        }
    }

    /// Move everything by (dx, dy).
    pub fn translate(&mut self, dx: f32, dy: f32) {
        let m = |p: &mut [f32; 2]| {
            p[0] += dx;
            p[1] += dy;
        };
        self.rect = RectF { x0: self.rect.x0 + dx, y0: self.rect.y0 + dy, x1: self.rect.x1 + dx, y1: self.rect.y1 + dy };
        for q in &mut self.quads {
            m(&mut q.ul);
            m(&mut q.ur);
            m(&mut q.ll);
            m(&mut q.lr);
        }
        if let Some((a, b)) = &mut self.line {
            m(a);
            m(b);
        }
        for s in &mut self.ink {
            for p in s {
                m(p);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct AnnotInfo {
    /// Object number of the annotation; stable until it is deleted.
    pub id: i32,
    pub spec: AnnotSpec,
    pub bounds: RectF,
}

fn rgb(c: [f32; 3]) -> AnnotationColor {
    AnnotationColor::Rgb { red: c[0], green: c[1], blue: c[2] }
}

fn from_color(c: Option<AnnotationColor>) -> Option<[f32; 3]> {
    match c? {
        AnnotationColor::Gray(g) => Some([g, g, g]),
        AnnotationColor::Rgb { red, green, blue } => Some([red, green, blue]),
        AnnotationColor::Cmyk { cyan, magenta, yellow, key } => Some([(1.0 - cyan) * (1.0 - key), (1.0 - magenta) * (1.0 - key), (1.0 - yellow) * (1.0 - key)]),
    }
}

fn rect(r: RectF) -> Rect {
    Rect { x0: r.x0, y0: r.y0, x1: r.x1, y1: r.y1 }
}

fn pt(p: [f32; 2]) -> Point {
    Point { x: p[0], y: p[1] }
}

fn quad(q: &QuadF) -> Quad {
    Quad::new(pt(q.ul), pt(q.ur), pt(q.ll), pt(q.lr))
}

pub(crate) fn read_spec(a: &PdfAnnotation) -> Result<AnnotSpec, mupdf::Error> {
    let kind = AnnotKind::from_pdf(a.r#type()?);
    let mut s = AnnotSpec::new(kind.clone(), from_color(a.color()?).unwrap_or([0.0, 0.0, 0.0]));
    s.author = a.author()?.unwrap_or_default().to_string();
    s.rect = a.rect().map(RectF::from).unwrap_or_default();
    s.fill = from_color(a.interior_color().ok().flatten());
    s.opacity = a.opacity().unwrap_or(1.0);
    s.contents = a.contents()?.unwrap_or_default().to_string();
    if matches!(kind, AnnotKind::Square | AnnotKind::Circle | AnnotKind::Line | AnnotKind::Ink | AnnotKind::FreeText) {
        s.width = a.border_width().unwrap_or(1.0);
    }
    if kind.is_markup() {
        s.quads = a.quad_points().unwrap_or_default().into_iter().map(QuadF::from).collect();
    }
    if kind == AnnotKind::Line
        && let Ok((p, q)) = a.line()
    {
        s.line = Some(([p.x, p.y], [q.x, q.y]));
        s.arrow = a.line_ending_styles().map(|(_, e)| e != LineEndingStyle::None).unwrap_or(false);
    }
    if kind == AnnotKind::Ink {
        s.ink = a.ink_list().unwrap_or_default().into_iter().map(|st| st.into_iter().map(|p| [p.x, p.y]).collect()).collect();
    }
    if kind == AnnotKind::FreeText
        && let Ok(Some(da)) = a.default_appearance()
    {
        s.font_size = da.size;
        if let Some(c) = from_color(da.color) {
            s.color = c;
        }
    }
    Ok(s)
}

fn apply_spec(a: &mut PdfAnnotation, s: &AnnotSpec) -> Result<(), mupdf::Error> {
    a.set_author(&s.author)?;
    if s.opacity < 1.0 {
        a.set_opacity(s.opacity)?;
    }
    match &s.kind {
        AnnotKind::Highlight | AnnotKind::Underline | AnnotKind::StrikeOut | AnnotKind::Squiggly => {
            a.set_color(rgb(s.color))?;
            a.set_quad_points(s.quads.iter().map(quad).collect::<Vec<_>>())?;
        }
        AnnotKind::Square | AnnotKind::Circle => {
            a.set_rect(rect(s.rect))?;
            a.set_color(rgb(s.color))?;
            if let Some(f) = s.fill {
                a.set_interior_color(rgb(f))?;
            }
            a.set_border_width(s.width)?;
        }
        AnnotKind::Line => {
            if let Some((p, q)) = s.line {
                a.set_line(pt(p), pt(q))?;
            }
            a.set_color(rgb(s.color))?;
            a.set_border_width(s.width)?;
            if s.arrow {
                a.set_line_ending_styles(LineEndingStyle::None, LineEndingStyle::OpenArrow)?;
            }
        }
        AnnotKind::Ink => {
            a.set_color(rgb(s.color))?;
            a.set_border_width(s.width)?;
            let strokes: Vec<Vec<Point>> = s.ink.iter().map(|st| st.iter().map(|p| pt(*p)).collect()).collect();
            a.set_ink_list(strokes)?;
        }
        AnnotKind::FreeText => {
            a.set_rect(rect(s.rect))?;
            a.set_default_appearance("Helv", s.font_size, Some(rgb(s.color)))?;
            a.set_border_width(0.0)?;
        }
        AnnotKind::Text => {
            // Sticky notes are anchored by their top-left corner.
            a.set_rect(Rect { x0: s.rect.x0, y0: s.rect.y0, x1: s.rect.x0 + 20.0, y1: s.rect.y0 + 20.0 })?;
            a.set_color(rgb(s.color))?;
        }
        AnnotKind::Other(_) => {}
    }
    a.set_contents(&s.contents)?;
    a.update()?;
    Ok(())
}

fn find(page: &PdfPage, id: i32) -> Option<PdfAnnotation> {
    page.annotations().find(|a| a.xref().ok() == Some(id))
}

pub(crate) fn list(pdf: &PdfDocument, page: u32) -> Result<Vec<AnnotInfo>, mupdf::Error> {
    let p = pdf.load_pdf_page(page as i32)?;
    let mut out = Vec::new();
    for a in p.annotations() {
        let Ok(t) = a.r#type() else { continue };
        if matches!(t, PdfAnnotationType::Popup | PdfAnnotationType::Link | PdfAnnotationType::Widget) {
            continue;
        }
        let Ok(spec) = read_spec(&a) else { continue };
        let bounds = a.bounds().map(RectF::from).unwrap_or(spec.rect);
        out.push(AnnotInfo { id: a.xref()?, spec, bounds });
    }
    Ok(out)
}

pub(crate) fn create(pdf: &mut PdfDocument, page: u32, s: &AnnotSpec) -> Result<i32, mupdf::Error> {
    let t = s.kind.pdf_type().ok_or(mupdf::Error::InvalidArgument("unsupported annotation".into()))?;
    let mut p = pdf.load_pdf_page(page as i32)?;
    let mut a = p.create_annotation(t)?;
    apply_spec(&mut a, s)?;
    let id = a.xref()?;
    p.update()?;
    Ok(id)
}

pub(crate) fn delete(pdf: &mut PdfDocument, page: u32, id: i32) -> Result<AnnotSpec, mupdf::Error> {
    let mut p = pdf.load_pdf_page(page as i32)?;
    let a = find(&p, id).ok_or(mupdf::Error::InvalidArgument("annotation not found".into()))?;
    let spec = read_spec(&a)?;
    p.delete_annotation(a)?;
    p.update()?;
    Ok(spec)
}

/// Replace an annotation's properties; returns the previous ones.
pub(crate) fn modify(pdf: &mut PdfDocument, page: u32, id: i32, s: &AnnotSpec) -> Result<AnnotSpec, mupdf::Error> {
    let mut p = pdf.load_pdf_page(page as i32)?;
    let mut a = find(&p, id).ok_or(mupdf::Error::InvalidArgument("annotation not found".into()))?;
    let old = read_spec(&a)?;
    if old.kind != s.kind {
        return Err(mupdf::Error::InvalidArgument("kind cannot change".into()));
    }
    // Lists (quads, ink) are rewritten entirely.
    if s.kind.is_markup() {
        a.clear_quad_points()?;
    }
    if s.kind == AnnotKind::Ink {
        a.clear_ink_list()?;
    }
    apply_spec(&mut a, s)?;
    p.update()?;
    Ok(old)
}

/// One user action and how to reverse it.
#[derive(Clone, Debug)]
pub(crate) enum Edit {
    Created { page: u32, id: i32, spec: AnnotSpec },
    Deleted { page: u32, id: i32, spec: AnnotSpec },
    Modified { page: u32, id: i32, before: AnnotSpec, after: AnnotSpec },
}

#[derive(Default)]
pub(crate) struct History {
    undo: Vec<Edit>,
    redo: Vec<Edit>,
}

impl History {
    pub fn push(&mut self, e: Edit) {
        self.undo.push(e);
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Returns the affected page.
    pub fn undo(&mut self, pdf: &mut PdfDocument) -> Result<Option<u32>, mupdf::Error> {
        let Some(e) = self.undo.pop() else { return Ok(None) };
        let (page, rev) = self.reverse(pdf, e)?;
        self.redo.push(rev);
        Ok(Some(page))
    }

    pub fn redo(&mut self, pdf: &mut PdfDocument) -> Result<Option<u32>, mupdf::Error> {
        let Some(e) = self.redo.pop() else { return Ok(None) };
        let (page, rev) = self.reverse(pdf, e)?;
        self.undo.push(rev);
        Ok(Some(page))
    }

    /// Apply the inverse of an edit; returns the edit that undoes that.
    fn reverse(&mut self, pdf: &mut PdfDocument, e: Edit) -> Result<(u32, Edit), mupdf::Error> {
        Ok(match e {
            Edit::Created { page, id, spec } => {
                delete(pdf, page, id)?;
                (page, Edit::Deleted { page, id, spec })
            }
            Edit::Deleted { page, id: old, spec } => {
                let id = create(pdf, page, &spec)?;
                // The recreated annotation has a new object number.
                for x in self.undo.iter_mut().chain(self.redo.iter_mut()) {
                    match x {
                        Edit::Created { page: p, id: i, .. } | Edit::Deleted { page: p, id: i, .. } | Edit::Modified { page: p, id: i, .. } if *p == page && *i == old => *i = id,
                        _ => {}
                    }
                }
                (page, Edit::Created { page, id, spec })
            }
            Edit::Modified { page, id, before, after } => {
                modify(pdf, page, id, &before)?;
                (page, Edit::Modified { page, id, before: after, after: before })
            }
        })
    }
}
