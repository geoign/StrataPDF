//! Annotations: listing, creation, modification and deletion on the document
//! thread, with undo/redo, and saving (incremental or full, optionally without
//! encryption). Coordinates are MuPDF page space (points, y down).

use mupdf::pdf::{Intent, LineEndingStyle, PdfAnnotation, PdfAnnotationType, PdfDocument, PdfPage};
use mupdf::color::AnnotationColor;
use mupdf::{Point, Quad, Rect};
use serde::{Deserialize, Serialize};

use crate::appearance::{self, PageGeom, PAD};
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
    /// A stamp showing an image.
    Stamp,
    /// Any other type; shown and deletable but not editable here.
    Other(String),
}

/// Typeface of free text: Helvetica with Gothic, or Times with Mincho.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextFont {
    #[default]
    Sans,
    Serif,
}

impl TextFont {
    pub fn label(self) -> &'static str {
        match self {
            TextFont::Sans => "ゴシック",
            TextFont::Serif => "明朝",
        }
    }

    /// Font name in the default appearance string, for readers that redraw the text.
    fn da_name(self) -> &'static str {
        match self {
            TextFont::Sans => "Helv",
            TextFont::Serif => "TiRo",
        }
    }
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
            AnnotKind::Stamp => "画像",
            AnnotKind::Other(s) => s,
        }
    }

    pub fn is_markup(&self) -> bool {
        matches!(self, AnnotKind::Highlight | AnnotKind::Underline | AnnotKind::StrikeOut | AnnotKind::Squiggly)
    }

    /// Kinds whose rectangle can be resized.
    pub fn is_resizable(&self) -> bool {
        matches!(self, AnnotKind::Square | AnnotKind::Circle | AnnotKind::FreeText | AnnotKind::Stamp)
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
            AnnotKind::Stamp => PdfAnnotationType::Stamp,
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
            PdfAnnotationType::Stamp => AnnotKind::Stamp,
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
    /// Free text: typeface.
    #[serde(default)]
    pub font: TextFont,
    /// Free text: a click-to-type box (intent FreeTextTypeWriter) whose width
    /// follows the text, up to the right edge of the page. Otherwise the width
    /// is fixed and the text wraps. The height always grows to fit the text.
    #[serde(default)]
    pub typewriter: bool,
    /// Image stamp: object number of the image XObject.
    #[serde(default)]
    pub image: i32,
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
            font: TextFont::Sans,
            typewriter: false,
            image: 0,
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
    let mut kind = AnnotKind::from_pdf(a.r#type()?);
    let mut image = 0;
    if kind == AnnotKind::Stamp {
        // Rubber stamps and drawn stamps can only be shown and deleted.
        match appearance::stamp_image(a) {
            Some(n) => image = n,
            None => kind = AnnotKind::Other("Stamp".into()),
        }
    }
    let mut s = AnnotSpec::new(kind.clone(), from_color(a.color()?).unwrap_or([0.0, 0.0, 0.0]));
    s.image = image;
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
    if kind == AnnotKind::FreeText {
        // The colour of a free text annotation is its background.
        s.fill = from_color(a.color()?);
        s.color = [0.0, 0.0, 0.0];
        if let Ok(Some(da)) = a.default_appearance() {
            s.font_size = da.size;
            if let Some(c) = from_color(da.color) {
                s.color = c;
            }
            let f = da.font_name.to_ascii_lowercase();
            if f.starts_with("tiro") || f.starts_with("times") || f.contains("mincho") || f.contains("serif") {
                s.font = TextFont::Serif;
            }
        }
        let it = a.object().get_dict("IT")?.and_then(|n| n.as_name().ok());
        s.typewriter = it.as_deref() == Some(b"FreeTextTypeWriter".as_slice());
    }
    Ok(s)
}

/// Fit a free text box to its text: the width follows the text for a
/// typewriter box (up to the page edge) and is kept otherwise; the height fits
/// the lines, and a fixed box never shrinks below the height it was given.
fn fit_free_text(s: &AnnotSpec, g: &PageGeom) -> (RectF, Vec<appearance::Line>) {
    let size = s.font_size.max(1.0);
    let x0 = s.rect.x0;
    let y0 = s.rect.y0;
    let min_w = size + 2.0 * PAD;
    let (lines, w) = if s.typewriter {
        let cap = (g.width - x0).max(min_w);
        let lines = appearance::layout(&s.contents, s.font, size, cap - 2.0 * PAD);
        let (w, _) = appearance::text_size(&lines, size);
        (lines, w.clamp(min_w.min(cap), cap))
    } else {
        let w = s.rect.width().max(min_w);
        (appearance::layout(&s.contents, s.font, size, w - 2.0 * PAD), w)
    };
    let (_, need) = appearance::text_size(&lines, size);
    let h = if s.typewriter { need } else { need.max(s.rect.height()) };
    (RectF { x0, y0, x1: x0 + w, y1: y0 + h }, lines)
}

fn apply_spec(pdf: &mut PdfDocument, page: &PdfPage, a: &mut PdfAnnotation, s: &AnnotSpec) -> Result<(), mupdf::Error> {
    a.set_author(&s.author)?;
    // Always set, so that it can go back to opaque.
    a.set_opacity(s.opacity.clamp(0.0, 1.0))?;
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
            a.set_default_appearance(s.font.da_name(), s.font_size, Some(rgb(s.color)))?;
            a.set_border_width(0.0)?;
            match s.fill {
                Some(f) => a.set_color(rgb(f))?,
                None => {
                    a.object().dict_delete("C")?;
                }
            }
            a.set_intent(if s.typewriter { Intent::FreetextTypewriter } else { Intent::Default })?;
        }
        AnnotKind::Text => {
            // Sticky notes are anchored by their top-left corner.
            a.set_rect(Rect { x0: s.rect.x0, y0: s.rect.y0, x1: s.rect.x0 + 20.0, y1: s.rect.y0 + 20.0 })?;
            a.set_color(rgb(s.color))?;
        }
        AnnotKind::Stamp => {
            if s.image <= 0 {
                return Err(mupdf::Error::InvalidArgument("stamp without an image".into()));
            }
            // With the image already in the appearance, MuPDF's synthesis draws
            // the image; without it, a "Draft" rubber stamp that reshapes the rectangle.
            appearance::write_stamp(pdf, a, s, &PageGeom::of(page)?)?;
            a.set_rect(rect(s.rect))?;
        }
        AnnotKind::Other(_) => {}
    }
    a.set_contents(&s.contents)?;
    match &s.kind {
        AnnotKind::FreeText => {
            // MuPDF draws Japanese only in Mincho and ignores page rotation:
            // after its synthesis, the appearance is replaced by ours.
            let g = PageGeom::of(page)?;
            let (r, lines) = fit_free_text(s, &g);
            a.set_rect(rect(r))?;
            a.update()?;
            appearance::write_free_text(pdf, a, s, &lines, r.width(), r.height(), &g)?;
        }
        AnnotKind::Stamp => {
            a.update()?;
            // MuPDF's image appearance ignores page rotation.
            appearance::write_stamp(pdf, a, s, &PageGeom::of(page)?)?;
        }
        _ => {
            a.update()?;
        }
    }
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
    if let Err(e) = apply_spec(pdf, &p, &mut a, s) {
        // Leave no half-made annotation behind.
        let _ = p.delete_annotation(a);
        return Err(e);
    }
    let id = a.xref()?;
    p.update()?;
    Ok(id)
}

/// Where to put a new image: centred on a point at its natural size (at most
/// 60% of the page each way), or fitted into a box the user drew.
#[derive(Clone, Copy, Debug)]
pub struct ImagePlace {
    pub center: [f32; 2],
    pub fit: Option<RectF>,
}

/// Add an image file's bytes to the document and place it on a page as a
/// stamp annotation. Returns the annotation id and its properties.
pub(crate) fn create_image(pdf: &mut PdfDocument, page: u32, data: &[u8], place: ImagePlace, author: &str) -> Result<(i32, AnnotSpec), mupdf::Error> {
    let img = mupdf::Image::from_bytes(data)?;
    let (pw, ph) = (img.width() as f32, img.height() as f32);
    if pw < 1.0 || ph < 1.0 {
        return Err(mupdf::Error::InvalidArgument("empty image".into()));
    }
    let (xres, yres) = img.resolution();
    let dpi = |r: i32| if (10..=10000).contains(&r) { r as f32 } else { 96.0 };
    let (nw, nh) = (pw * 72.0 / dpi(xres), ph * 72.0 / dpi(yres));
    let g = PageGeom::of(&pdf.load_pdf_page(page as i32)?)?;
    let r = match place.fit.filter(|b| b.width() > 4.0 && b.height() > 4.0) {
        Some(b) => {
            let k = (b.width() / nw).min(b.height() / nh);
            let (w, h) = (nw * k, nh * k);
            let (cx, cy) = ((b.x0 + b.x1) * 0.5, (b.y0 + b.y1) * 0.5);
            RectF { x0: cx - w * 0.5, y0: cy - h * 0.5, x1: cx + w * 0.5, y1: cy + h * 0.5 }
        }
        None => {
            let k = (0.6 * g.width / nw).min(0.6 * g.height / nh).min(1.0);
            let (w, h) = (nw * k, nh * k);
            let x0 = (place.center[0] - w * 0.5).clamp(0.0, (g.width - w).max(0.0));
            let y0 = (place.center[1] - h * 0.5).clamp(0.0, (g.height - h).max(0.0));
            RectF { x0, y0, x1: x0 + w, y1: y0 + h }
        }
    };
    let obj = pdf.add_image(&img)?;
    let mut spec = AnnotSpec::new(AnnotKind::Stamp, [0.0, 0.0, 0.0]);
    spec.rect = r;
    spec.image = obj.as_indirect()?;
    if !author.is_empty() {
        spec.author = author.into();
    }
    let id = create(pdf, page, &spec)?;
    Ok((id, spec))
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
    apply_spec(pdf, &p, &mut a, s)?;
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

#[cfg(test)]
#[path = "annot_tests.rs"]
mod tests;
