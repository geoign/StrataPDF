//! Annotation and editing tools of a document view: toolbar, drawing shapes,
//! writing text in place, sticky notes, placing and pasting images, selecting,
//! moving and resizing, properties, undo/redo and saving.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use egui::{Color32, CursorIcon, FontId, Id, Key, Modifiers, Pos2, Rect, RichText, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use strata_core::annot::{AnnotInfo, AnnotKind, AnnotSpec, ImagePlace, TextFont};
use strata_core::appearance::{LEADING, PAD};
use strata_core::{AnnotOp, AnnotResult, ImageBytes, Pending, QuadF, RectF, SaveOptions};

use super::{CHAR_POS_END, CHAR_POS_START, DocPos, DocView};

/// Image files MuPDF reads.
const IMAGE_EXT: [&str; 10] = ["png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "jp2", "jpx", "j2k"];
/// Half the size of a resize handle, in screen points.
const HANDLE: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tool {
    /// Text selection, panning, selecting, moving and resizing annotations.
    #[default]
    Select,
    Highlight,
    Underline,
    StrikeOut,
    Rect,
    Ellipse,
    Line,
    Arrow,
    Ink,
    TextBox,
    Note,
    /// Placing the chosen image: a click puts it at its natural size, a drag fits it in a box.
    Image,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Select => "選択",
            Tool::Highlight => "ハイライト",
            Tool::Underline => "下線",
            Tool::StrikeOut => "取り消し線",
            Tool::Rect => "矩形",
            Tool::Ellipse => "楕円",
            Tool::Line => "直線",
            Tool::Arrow => "矢印",
            Tool::Ink => "手書き",
            Tool::TextBox => "文字",
            Tool::Note => "付箋",
            Tool::Image => "画像",
        }
    }

    fn tip(self) -> &'static str {
        match self {
            Tool::Select => "注釈を選んで動かす。角をドラッグすると大きさを変える",
            Tool::TextBox => "クリックした所に文字を書く。ドラッグすると幅を決めて折り返す",
            Tool::Note => "クリックした所に付箋を貼る",
            _ => "",
        }
    }

    fn default_color(self) -> [f32; 3] {
        match self {
            Tool::Highlight => [1.0, 0.85, 0.1],
            Tool::Underline | Tool::Line | Tool::Arrow => [0.1, 0.4, 0.9],
            Tool::StrikeOut | Tool::Rect | Tool::Ellipse | Tool::Ink => [0.9, 0.15, 0.15],
            Tool::TextBox => [0.1, 0.1, 0.1],
            Tool::Note => [1.0, 0.8, 0.1],
            Tool::Select | Tool::Image => [0.0, 0.0, 0.0],
        }
    }
}

const PALETTE: [[f32; 3]; 8] = [
    [1.0, 0.85, 0.1],
    [0.9, 0.15, 0.15],
    [0.1, 0.4, 0.9],
    [0.15, 0.65, 0.25],
    [0.6, 0.2, 0.8],
    [1.0, 0.5, 0.1],
    [0.1, 0.1, 0.1],
    [1.0, 1.0, 1.0],
];

/// A shape being drawn, in page space.
struct Draft {
    page: u32,
    start: [f32; 2],
    cur: [f32; 2],
    points: Vec<[f32; 2]>,
    /// Text markup: selection anchor.
    anchor: Option<DocPos>,
}

struct Moving {
    page: u32,
    id: i32,
    spec: AnnotSpec,
    start: Pos2,
    delta: egui::Vec2,
}

/// A corner of the selected annotation being dragged.
struct Resizing {
    page: u32,
    id: i32,
    spec: AnnotSpec,
    /// 0 top left, 1 top right, 2 bottom right, 3 bottom left.
    corner: usize,
    rect: RectF,
}

/// Text being written: free text in place on the page, or a sticky note's popup.
struct TextEditor {
    page: u32,
    /// `None` for a new annotation, which is made when the text is confirmed.
    id: Option<i32>,
    spec: AnnotSpec,
    text: String,
    focus: bool,
    serial: u64,
    /// The free text being edited, hidden under the editor.
    cover: Option<RectF>,
}

pub struct AnnotState {
    pub bar: bool,
    pub tool: Tool,
    color: Option<[f32; 3]>,
    width: f32,
    /// Size (pt) and typeface for new free text.
    text_size: f32,
    text_font: TextFont,
    /// The text box (page, id, size, typeface) the text settings were last taken from.
    text_synced: Option<(u32, i32, f32, TextFont)>,
    /// Per page: (revision the list belongs to, list request).
    lists: HashMap<u32, (u32, Pending<Arc<Vec<AnnotInfo>>>)>,
    pub selected: Option<(u32, i32)>,
    draft: Option<Draft>,
    moving: Option<Moving>,
    resizing: Option<Resizing>,
    editor: Option<TextEditor>,
    serial: u64,
    /// The image the image tool places.
    image: Option<ImageBytes>,
    /// Ctrl+V was held at the last look (egui sends no event for it when the clipboard holds no text).
    paste_down: bool,
    pending: Vec<Pending<AnnotResult>>,
    saving: Option<Pending<()>>,
    pub confirm_save_as: bool,
    decrypt: bool,
}

impl Default for AnnotState {
    fn default() -> Self {
        AnnotState {
            bar: false,
            tool: Tool::Select,
            color: None,
            width: 2.0,
            text_size: 12.0,
            text_font: TextFont::Sans,
            text_synced: None,
            lists: HashMap::new(),
            selected: None,
            draft: None,
            moving: None,
            resizing: None,
            editor: None,
            serial: 0,
            image: None,
            paste_down: false,
            pending: Vec::new(),
            saving: None,
            confirm_save_as: false,
            decrypt: true,
        }
    }
}

fn c32(c: [f32; 3]) -> Color32 {
    Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)
}

fn norm(a: [f32; 2], b: [f32; 2]) -> RectF {
    RectF { x0: a[0].min(b[0]), y0: a[1].min(b[1]), x1: a[0].max(b[0]), y1: a[1].max(b[1]) }
}

fn corners(r: Rect) -> [Pos2; 4] {
    [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()]
}

fn corner_cursor(corner: usize) -> CursorIcon {
    if corner.is_multiple_of(2) { CursorIcon::ResizeNwSe } else { CursorIcon::ResizeNeSw }
}

fn dashed(painter: &egui::Painter, r: Rect, stroke: Stroke) {
    let c = corners(r);
    painter.add(Shape::dashed_line(&[c[0], c[1], c[2], c[3], c[0]], stroke, 5.0, 3.0));
}

/// The rectangle after dragging one corner to `p`; the opposite corner stays put.
fn resized(r: RectF, corner: usize, p: [f32; 2], keep_aspect: bool) -> RectF {
    let c = [[r.x0, r.y0], [r.x1, r.y0], [r.x1, r.y1], [r.x0, r.y1]];
    let o = c[(corner + 2) % 4];
    let side = |v: f32, orig: f32| if v != 0.0 { v.signum() } else { orig.signum() };
    let sx = side(p[0] - o[0], c[corner][0] - o[0]);
    let sy = side(p[1] - o[1], c[corner][1] - o[1]);
    let (mut w, mut h) = ((p[0] - o[0]).abs(), (p[1] - o[1]).abs());
    const MIN: f32 = 4.0;
    if keep_aspect && r.width() > 0.0 && r.height() > 0.0 {
        let k = (w / r.width()).max(h / r.height()).max(MIN / r.width().min(r.height()));
        w = r.width() * k;
        h = r.height() * k;
    } else {
        w = w.max(MIN);
        h = h.max(MIN);
    }
    norm(o, [o[0] + sx * w, o[1] + sy * h])
}

fn is_image_file(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| IMAGE_EXT.contains(&e.to_ascii_lowercase().as_str()))
}

/// An image on the clipboard: a copied image file's bytes, or a bitmap as PNG.
fn clipboard_image() -> Option<Result<Vec<u8>, String>> {
    let mut cb = arboard::Clipboard::new().ok()?;
    if let Ok(files) = cb.get().file_list()
        && let Some(f) = files.iter().find(|f| is_image_file(f))
    {
        return Some(std::fs::read(f).map_err(|e| format!("{}: {e}", f.display())));
    }
    let img = cb.get_image().ok()?;
    Some(png_from_rgba(img.width, img.height, img.bytes.into_owned()))
}

fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok().filter(|t| !t.trim().is_empty())
}

fn png_from_rgba(w: usize, h: usize, mut rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    // Bitmaps without transparency often arrive with every alpha byte zero.
    if rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        for p in rgba.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Ctrl+V is held down in this window.
fn paste_key_down(ctx: &egui::Context) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_V};
    // SAFETY: a plain query of the keyboard state.
    ctx.input(|i| i.modifiers.command && i.focused) && unsafe { GetAsyncKeyState(VK_V.0 as i32) } < 0
}

impl DocView {
    fn tool_color(&self) -> [f32; 3] {
        self.annot.color.unwrap_or(self.annot.tool.default_color())
    }

    fn tool_width(&self) -> f32 {
        if self.annot.width > 0.0 { self.annot.width } else { 2.0 }
    }

    fn is_pdf(&self) -> bool {
        self.doc.info().is_pdf
    }

    /// Annotations of a page (requested again after the page is edited).
    pub(super) fn page_annots(&mut self, page: u32) -> Option<Arc<Vec<AnnotInfo>>> {
        let rev = self.doc.page_rev(page);
        let doc = self.doc.clone();
        let entry = self.annot.lists.entry(page).or_insert_with(|| (rev, doc.annotations(page)));
        if entry.0 != rev {
            *entry = (rev, doc.annotations(page));
        }
        match entry.1.poll() {
            Some(Ok(l)) => Some(l.clone()),
            _ => None,
        }
    }

    /// The topmost annotation at a point of a page.
    fn annot_at_point(&mut self, page: u32, p: [f32; 2]) -> Option<AnnotInfo> {
        let list = self.page_annots(page)?;
        let slack = 3.0 / self.zoom;
        list.iter()
            .rev()
            .find(|a| {
                let b = a.bounds;
                p[0] >= b.x0 - slack && p[0] <= b.x1 + slack && p[1] >= b.y0 - slack && p[1] <= b.y1 + slack
            })
            .cloned()
    }

    fn annot_at(&mut self, screen: Pos2) -> Option<(u32, AnnotInfo)> {
        let page = self.layout.page_at(self.to_content(screen))?;
        let (x, y) = self.screen_to_page(page, screen);
        self.annot_at_point(page, [x, y]).map(|a| (page, a))
    }

    fn selected_annot(&mut self) -> Option<(u32, AnnotInfo)> {
        let (page, id) = self.annot.selected?;
        let a = self.page_annots(page)?.iter().find(|a| a.id == id).cloned()?;
        Some((page, a))
    }

    fn page_rect_to_screen(&self, page: u32, r: RectF) -> Rect {
        Rect::from_two_pos(self.page_to_screen(page, [r.x0, r.y0]), self.page_to_screen(page, [r.x1, r.y1]))
    }

    /// The resize handle of the selected annotation under a screen point.
    fn handle_at(&mut self, screen: Pos2) -> Option<(u32, AnnotInfo, usize)> {
        let (page, a) = self.selected_annot()?;
        if !a.spec.kind.is_resizable() {
            return None;
        }
        let r = self.page_rect_to_screen(page, a.spec.rect);
        corners(r).iter().position(|c| c.distance(screen) <= HANDLE + 3.0).map(|k| (page, a, k))
    }

    fn submit(&mut self, op: AnnotOp) {
        let p = self.doc.edit(op);
        self.annot.pending.push(p);
    }

    pub(super) fn annot_toolbar(&mut self, ui: &mut Ui) {
        self.sync_text_settings();
        egui::Panel::top(Id::new(("annotbar", self.id))).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for t in [Tool::Select, Tool::TextBox, Tool::Note] {
                    let r = ui.selectable_label(self.annot.tool == t, t.label()).on_hover_text(t.tip());
                    if r.clicked() {
                        self.set_tool(t);
                    }
                }
                if ui
                    .selectable_label(self.annot.tool == Tool::Image, "画像…")
                    .on_hover_text("画像ファイルを選んでページに置く。Ctrl+V でクリップボードの画像を貼り付ける")
                    .clicked()
                {
                    self.choose_image_tool();
                }
                ui.separator();
                for t in [Tool::Highlight, Tool::Underline, Tool::StrikeOut, Tool::Rect, Tool::Ellipse, Tool::Line, Tool::Arrow, Tool::Ink] {
                    if ui.selectable_label(self.annot.tool == t, t.label()).clicked() {
                        self.set_tool(t);
                    }
                }
                ui.separator();
                let cur = self.tool_color();
                for c in PALETTE {
                    let (r, resp) = ui.allocate_exact_size(vec2(16.0, 16.0), egui::Sense::click());
                    ui.painter().rect_filled(r, 3.0, c32(c));
                    ui.painter().rect_stroke(r, 3.0, Stroke::new(1.0, Color32::from_gray(128)), StrokeKind::Inside);
                    if cur == c {
                        ui.painter().rect_stroke(r.expand(2.0), 3.0, Stroke::new(2.0, ui.visuals().selection.stroke.color), StrokeKind::Outside);
                    }
                    if resp.clicked() {
                        self.set_color(c);
                    }
                }
                let mut rgb = cur;
                if egui::color_picker::color_edit_button_rgb(ui, &mut rgb).changed() {
                    self.set_color(rgb);
                }
                let text_selected = self.selected_annot().is_some_and(|(_, a)| a.spec.kind == AnnotKind::FreeText);
                let editing_text = self.annot.editor.as_ref().is_some_and(|e| e.spec.kind == AnnotKind::FreeText);
                if self.annot.tool == Tool::TextBox || text_selected || editing_text {
                    self.text_controls(ui);
                } else if matches!(self.annot.tool, Tool::Rect | Tool::Ellipse | Tool::Line | Tool::Arrow | Tool::Ink) {
                    ui.add(egui::Slider::new(&mut self.annot.width, 0.5..=12.0).text("線幅").fixed_decimals(1));
                }
                ui.separator();
                if ui.add_enabled(self.doc.can_undo(), egui::Button::new("元に戻す")).on_hover_text("Ctrl+Z").clicked() {
                    self.submit(AnnotOp::Undo);
                }
                if ui.add_enabled(self.doc.can_redo(), egui::Button::new("やり直し")).on_hover_text("Ctrl+Y").clicked() {
                    self.submit(AnnotOp::Redo);
                }
                ui.separator();
                if ui.add_enabled(self.doc.is_dirty(), egui::Button::new("保存")).on_hover_text("上書き保存 (Ctrl+S)").clicked() {
                    self.save(false);
                }
                if ui.button("名前を付けて保存…").on_hover_text("Ctrl+Shift+S").clicked() {
                    self.save(true);
                }
            });
        });
    }

    /// Size and typeface of text: for new text, the text being written and the selected text box.
    fn text_controls(&mut self, ui: &mut Ui) {
        ui.separator();
        let r = ui
            .add(egui::DragValue::new(&mut self.annot.text_size).range(4.0..=144.0).speed(0.2).fixed_decimals(1).suffix(" pt"))
            .on_hover_text("文字の大きさ");
        let mut font_changed = false;
        for f in [TextFont::Sans, TextFont::Serif] {
            if ui.selectable_label(self.annot.text_font == f, f.label()).clicked() && self.annot.text_font != f {
                self.annot.text_font = f;
                font_changed = true;
            }
        }
        if let Some(ed) = &mut self.annot.editor
            && ed.spec.kind == AnnotKind::FreeText
        {
            ed.spec.font_size = self.annot.text_size;
            ed.spec.font = self.annot.text_font;
            return;
        }
        let done = (r.changed() && !r.dragged()) || r.drag_stopped() || font_changed;
        if done
            && let Some((page, a)) = self.selected_annot()
            && a.spec.kind == AnnotKind::FreeText
        {
            let mut spec = a.spec.clone();
            spec.font_size = self.annot.text_size;
            spec.font = self.annot.text_font;
            if spec != a.spec {
                self.submit(AnnotOp::Modify { page, id: a.id, spec });
            }
        }
    }

    /// Take the text settings from the selected text box when it is selected
    /// or its size or typeface changes.
    fn sync_text_settings(&mut self) {
        let Some((page, a)) = self.selected_annot() else {
            if self.annot.selected.is_none() {
                self.annot.text_synced = None;
            }
            return;
        };
        if a.spec.kind != AnnotKind::FreeText {
            return;
        }
        let now = Some((page, a.id, a.spec.font_size, a.spec.font));
        if now != self.annot.text_synced {
            self.annot.text_synced = now;
            self.annot.text_size = a.spec.font_size;
            self.annot.text_font = a.spec.font;
        }
    }

    fn set_tool(&mut self, t: Tool) {
        self.commit_editor(true);
        self.annot.tool = t;
        self.annot.color = None;
        self.annot.image = None;
        self.annot.draft = None;
        // Markup applies to an existing text selection right away.
        if matches!(t, Tool::Highlight | Tool::Underline | Tool::StrikeOut) && self.sel.is_some() {
            self.markup_selection(t);
        }
    }

    fn pick_image(&mut self) -> Option<ImageBytes> {
        let path = rfd::FileDialog::new().add_filter("画像", &IMAGE_EXT).pick_file()?;
        match std::fs::read(&path) {
            Ok(b) => Some(ImageBytes(Arc::new(b))),
            Err(e) => {
                self.status = format!("画像を読めませんでした: {e}");
                None
            }
        }
    }

    fn choose_image_tool(&mut self) {
        if !self.is_pdf() {
            self.status = "画像は PDF にだけ置けます".into();
            return;
        }
        if let Some(data) = self.pick_image() {
            self.set_tool(Tool::Image);
            self.annot.image = Some(data);
            self.status = "クリックした所に画像を置きます。ドラッグすると枠に合わせます (Esc で中止)".into();
        }
    }

    /// Apply a colour to the tool and to the selected annotation.
    fn set_color(&mut self, c: [f32; 3]) {
        self.annot.color = Some(c);
        if let Some(ed) = &mut self.annot.editor {
            ed.spec.color = c;
            return;
        }
        if let Some((page, a)) = self.selected_annot()
            && !matches!(a.spec.kind, AnnotKind::Stamp | AnnotKind::Other(_))
        {
            let mut spec = a.spec.clone();
            spec.color = c;
            self.submit(AnnotOp::Modify { page, id: a.id, spec });
        }
    }

    fn markup_selection(&mut self, t: Tool) {
        let Some(sel) = self.sel else { return };
        let (a, b) = sel.ordered();
        let kind = match t {
            Tool::Highlight => AnnotKind::Highlight,
            Tool::Underline => AnnotKind::Underline,
            _ => AnnotKind::StrikeOut,
        };
        for p in a.page..=b.page {
            let from = if p == a.page { a.pos } else { CHAR_POS_START };
            let to = if p == b.page { b.pos } else { CHAR_POS_END };
            let Some(text) = self.text(p) else { continue };
            let quads: Vec<QuadF> = text.selection_quads(from, to);
            if quads.is_empty() {
                continue;
            }
            let mut spec = AnnotSpec::new(kind.clone(), self.tool_color());
            spec.quads = quads;
            self.submit(AnnotOp::Create { page: p, spec });
        }
        self.sel = None;
    }

    // ------------------------------------------------------------ writing text

    /// A new click-to-type text box whose first line is centred on a point.
    fn new_text_at(&self, p: [f32; 2]) -> AnnotSpec {
        let mut s = AnnotSpec::new(AnnotKind::FreeText, if self.annot.tool == Tool::TextBox { self.tool_color() } else { Tool::TextBox.default_color() });
        s.font_size = self.annot.text_size;
        s.font = self.annot.text_font;
        s.typewriter = true;
        let x = (p[0] - PAD).max(0.0);
        let y = (p[1] - PAD - s.font_size * LEADING * 0.5).max(0.0);
        s.rect = RectF { x0: x, y0: y, x1: x, y1: y };
        s
    }

    fn new_note_at(&self, p: [f32; 2]) -> AnnotSpec {
        let mut s = AnnotSpec::new(AnnotKind::Text, if self.annot.tool == Tool::Note { self.tool_color() } else { Tool::Note.default_color() });
        s.rect = RectF { x0: p[0], y0: p[1], x1: p[0] + 20.0, y1: p[1] + 20.0 };
        s
    }

    fn open_editor(&mut self, page: u32, id: Option<i32>, spec: AnnotSpec, cover: Option<RectF>) {
        self.commit_editor(true);
        self.annot.serial += 1;
        let text = spec.contents.clone();
        self.annot.editor = Some(TextEditor { page, id, spec, text, focus: true, serial: self.annot.serial, cover });
    }

    fn edit_existing(&mut self, page: u32, a: AnnotInfo) {
        let cover = (a.spec.kind == AnnotKind::FreeText).then_some(a.bounds);
        self.annot.selected = Some((page, a.id));
        self.open_editor(page, Some(a.id), a.spec, cover);
    }

    /// Close the text editor, keeping what was written (`ok`) or not.
    fn commit_editor(&mut self, ok: bool) {
        let Some(ed) = self.annot.editor.take() else { return };
        if !ok {
            return;
        }
        let text = ed.text.trim_end().to_string();
        match ed.id {
            None => {
                if text.trim().is_empty() {
                    return;
                }
                let mut spec = ed.spec;
                spec.contents = text;
                self.submit(AnnotOp::Create { page: ed.page, spec });
            }
            Some(id) => {
                // Keep the stored properties; only the text and what the editor changed.
                let base = self.page_annots(ed.page).and_then(|l| l.iter().find(|a| a.id == id).map(|a| a.spec.clone()));
                let mut spec = base.clone().unwrap_or(ed.spec.clone());
                spec.font_size = ed.spec.font_size;
                spec.font = ed.spec.font;
                spec.color = ed.spec.color;
                if spec.kind == AnnotKind::FreeText && text.trim().is_empty() {
                    self.submit(AnnotOp::Delete { page: ed.page, id });
                    self.annot.selected = None;
                    return;
                }
                spec.contents = text;
                if base.as_ref() != Some(&spec) {
                    self.submit(AnnotOp::Modify { page: ed.page, id, spec });
                }
            }
        }
    }

    /// The text editor: free text typed in place, or a sticky note's popup.
    fn editor_ui(&mut self, ctx: &egui::Context) {
        let Some(ed) = &self.annot.editor else { return };
        let page = ed.page;
        if page as usize >= self.layout.page_rects.len() {
            return;
        }
        let (rect, size, typewriter, kind) = (ed.spec.rect, ed.spec.font_size, ed.spec.typewriter, ed.spec.kind.clone());
        let zoom = self.zoom;
        let accent = Color32::from_rgb(40, 120, 255);
        let mut outcome: Option<bool> = None;
        if kind == AnnotKind::FreeText {
            let fs = (size * zoom).max(3.0);
            let origin = self.page_to_screen(page, [rect.x0 + PAD, rect.y0 + PAD]);
            let page_w = self.sizes[page as usize].w;
            let wrap = if typewriter { page_w - rect.x0 - 2.0 * PAD } else { rect.width() - 2.0 * PAD }.max(size) * zoom;
            let ed = self.annot.editor.as_mut().unwrap();
            let color = c32(ed.spec.color);
            egui::Area::new(Id::new(("annot-text", self.id, ed.serial))).order(egui::Order::Foreground).fixed_pos(origin).show(ctx, |ui| {
                let commit = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
                let out = egui::TextEdit::multiline(&mut ed.text)
                    .font(FontId::proportional(fs))
                    .text_color(color)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::ZERO)
                    .desired_width(wrap)
                    .desired_rows(1)
                    .hint_text("文字を入力 (Ctrl+Enter で確定・Esc で取り消し)")
                    .show(ui);
                let r = &out.response.response;
                if ed.focus {
                    r.request_focus();
                    ed.focus = false;
                }
                let w = if ed.text.is_empty() { wrap.min(fs * 16.0) } else if typewriter { out.galley.rect.width().max(fs) } else { wrap };
                let h = out.galley.rect.height().max(fs * LEADING);
                dashed(ui.painter(), Rect::from_min_size(out.galley_pos, vec2(w, h)).expand(PAD * zoom), Stroke::new(1.0, accent));
                if commit {
                    outcome = Some(true);
                } else if r.lost_focus() {
                    outcome = Some(!ui.input(|i| i.key_pressed(Key::Escape)));
                }
            });
        } else {
            let anchor = self.page_to_screen(page, [rect.x0 + 22.0, rect.y0]);
            let ed = self.annot.editor.as_mut().unwrap();
            egui::Area::new(Id::new(("annot-note", self.id, ed.serial))).order(egui::Order::Foreground).fixed_pos(anchor).show(ctx, |ui| {
                egui::Frame::popup(ui.style()).fill(Color32::from_rgb(255, 248, 200)).show(ui, |ui| {
                    let commit = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
                    ui.label(RichText::new("付箋").small().color(Color32::from_gray(70)));
                    let out = egui::TextEdit::multiline(&mut ed.text)
                        .text_color(Color32::from_gray(20))
                        .frame(egui::Frame::NONE)
                        .desired_width(240.0)
                        .desired_rows(4)
                        .hint_text("メモを入力")
                        .show(ui);
                    ui.label(RichText::new("Ctrl+Enter で確定・Esc で取り消し").small().color(Color32::from_gray(90)));
                    let r = &out.response.response;
                    if ed.focus {
                        r.request_focus();
                        ed.focus = false;
                    }
                    if commit {
                        outcome = Some(true);
                    } else if r.lost_focus() {
                        outcome = Some(!ui.input(|i| i.key_pressed(Key::Escape)));
                    }
                });
            });
        }
        if let Some(ok) = outcome {
            self.commit_editor(ok);
        }
    }

    // ------------------------------------------------------------ images

    /// Paste the clipboard at a point of a page (by default under the pointer,
    /// or in the middle of the view): an image becomes an image stamp; text,
    /// while the editing bar is open or when asked from the menu, a text box.
    fn paste(&mut self, ctx: &egui::Context, at: Option<(u32, [f32; 2])>, text: Option<String>) {
        if !self.is_pdf() {
            return;
        }
        let Some((page, p)) = at.or_else(|| self.paste_target(ctx)) else { return };
        match clipboard_image() {
            Some(Ok(bytes)) => {
                self.commit_editor(true);
                self.submit(AnnotOp::AddImage { page, data: ImageBytes(Arc::new(bytes)), place: ImagePlace { center: p, fit: None } });
                self.annot.bar = true;
                self.status = "画像を貼り付けました。ドラッグで動かし、角で大きさを変えられます".into();
            }
            Some(Err(e)) => self.status = format!("クリップボードの画像を読めませんでした: {e}"),
            None => {
                let text = if at.is_some() { text.or_else(clipboard_text) } else { text.filter(|_| self.annot.bar) };
                match text {
                    Some(t) => {
                        let mut s = self.new_text_at(p);
                        s.contents = t.trim_end().to_string();
                        self.open_editor(page, None, s, None);
                        self.annot.bar = true;
                    }
                    None if at.is_some() => self.status = "クリップボードに画像も文字もありません".into(),
                    None => {}
                }
            }
        }
    }

    fn paste_target(&self, ctx: &egui::Context) -> Option<(u32, [f32; 2])> {
        let pos = ctx.pointer_hover_pos().filter(|p| self.viewport.contains(*p)).unwrap_or(self.viewport.center());
        let page = self.layout.page_at(self.to_content(pos)).unwrap_or(self.current_page());
        let s = self.sizes.get(page as usize)?;
        let (x, y) = self.screen_to_page(page, pos);
        Some((page, [x.clamp(0.0, s.w), y.clamp(0.0, s.h)]))
    }

    /// Editing entries at the top of the page's context menu.
    pub(super) fn edit_context_menu(&mut self, ui: &mut Ui, page: u32, x: f32, y: f32) {
        if !self.is_pdf() {
            return;
        }
        let ctx = ui.ctx().clone();
        if let Some(a) = self.annot_at_point(page, [x, y]) {
            if matches!(a.spec.kind, AnnotKind::FreeText | AnnotKind::Text) {
                let label = if a.spec.kind == AnnotKind::FreeText { "文字を書き直す" } else { "付箋を編集" };
                if ui.button(label).clicked() {
                    ui.close();
                    self.edit_existing(page, a.clone());
                }
            }
            if ui.button(format!("この{}を削除", a.spec.kind.label())).clicked() {
                ui.close();
                self.submit(AnnotOp::Delete { page, id: a.id });
                if self.annot.selected == Some((page, a.id)) {
                    self.annot.selected = None;
                }
            }
            ui.separator();
        }
        if ui.button("ここに文字を書く").clicked() {
            ui.close();
            let s = self.new_text_at([x, y]);
            self.open_editor(page, None, s, None);
            self.annot.bar = true;
        }
        if ui.button("ここに付箋を貼る").clicked() {
            ui.close();
            let s = self.new_note_at([x, y]);
            self.open_editor(page, None, s, None);
            self.annot.bar = true;
        }
        if ui.button("ここに貼り付け (Ctrl+V)").on_hover_text("クリップボードの画像か文字").clicked() {
            ui.close();
            self.paste(&ctx, Some((page, [x, y])), None);
        }
        if ui.button("ここに画像ファイルを置く…").clicked() {
            ui.close();
            if let Some(data) = self.pick_image() {
                self.submit(AnnotOp::AddImage { page, data, place: ImagePlace { center: [x, y], fit: None } });
                self.annot.bar = true;
            }
        }
        ui.separator();
    }

    // ------------------------------------------------------------ pointer

    /// Pointer handling for annotation tools. Returns true when the input was
    /// consumed (the default text selection / panning must not run).
    pub(super) fn annot_input(&mut self, ui: &Ui, resp: &egui::Response) -> bool {
        let ctx = ui.ctx().clone();
        let tool = self.annot.tool;
        let pointer = resp.interact_pointer_pos().or(ctx.pointer_hover_pos());

        if tool == Tool::Select {
            // A corner of the selected annotation resizes it.
            if resp.drag_started_by(egui::PointerButton::Primary)
                && let Some(start) = ctx.input(|i| i.pointer.press_origin())
                && let Some((page, a, corner)) = self.handle_at(start)
            {
                self.annot.resizing = Some(Resizing { page, id: a.id, rect: a.spec.rect, spec: a.spec, corner });
                return true;
            }
            if self.annot.resizing.is_some() {
                if resp.dragged()
                    && let Some(p) = pointer
                    && let Some(rz) = &self.annot.resizing
                {
                    let (x, y) = self.screen_to_page(rz.page, p);
                    let keep = rz.spec.kind == AnnotKind::Stamp && !ctx.input(|i| i.modifiers.shift);
                    let r = resized(rz.spec.rect, rz.corner, [x, y], keep);
                    ctx.set_cursor_icon(corner_cursor(rz.corner));
                    if let Some(rz) = &mut self.annot.resizing {
                        rz.rect = r;
                    }
                }
                if resp.drag_stopped() || !ctx.input(|i| i.pointer.primary_down()) {
                    let rz = self.annot.resizing.take().unwrap();
                    if rz.rect != rz.spec.rect {
                        let mut spec = rz.spec;
                        spec.rect = rz.rect;
                        if spec.kind == AnnotKind::FreeText {
                            // A width set by hand is kept, and the text wraps in it.
                            spec.typewriter = false;
                        }
                        self.submit(AnnotOp::Modify { page: rz.page, id: rz.id, spec });
                    }
                }
                return true;
            }
            // Double click edits text (checked first: the second click is also a click).
            if resp.double_clicked()
                && let Some(p) = pointer
                && let Some((page, a)) = self.annot_at(p)
                && matches!(a.spec.kind, AnnotKind::FreeText | AnnotKind::Text)
            {
                self.edit_existing(page, a);
                return true;
            }
            // Click selects an annotation; drag moves the selected one.
            if resp.clicked()
                && let Some(p) = pointer
            {
                match self.annot_at(p) {
                    Some((page, a)) => {
                        self.annot.selected = Some((page, a.id));
                        return true;
                    }
                    None => self.annot.selected = None,
                }
            }
            if resp.drag_started_by(egui::PointerButton::Primary)
                && let Some(start) = ctx.input(|i| i.pointer.press_origin())
                && let Some((page, a)) = self.annot_at(start)
                && self.annot.selected == Some((page, a.id))
                && !a.spec.kind.is_markup()
                && !matches!(a.spec.kind, AnnotKind::Other(_))
            {
                self.annot.moving = Some(Moving { page, id: a.id, spec: a.spec, start, delta: egui::Vec2::ZERO });
                return true;
            }
            if let Some(m) = &mut self.annot.moving {
                if resp.dragged() {
                    m.delta = pointer.unwrap_or(m.start) - m.start;
                    ctx.set_cursor_icon(CursorIcon::Move);
                }
                if resp.drag_stopped() {
                    let m = self.annot.moving.take().unwrap();
                    let mut spec = m.spec;
                    spec.translate(m.delta.x / self.zoom, m.delta.y / self.zoom);
                    if m.delta.length() > 1.0 {
                        self.submit(AnnotOp::Modify { page: m.page, id: m.id, spec });
                    }
                }
                return true;
            }
            if let Some(h) = ctx.pointer_hover_pos()
                && resp.hovered()
            {
                if let Some((_, _, corner)) = self.handle_at(h) {
                    ctx.set_cursor_icon(corner_cursor(corner));
                } else if let Some((pg, a)) = self.annot_at(h) {
                    if self.annot.selected == Some((pg, a.id)) && !a.spec.kind.is_markup() {
                        ctx.set_cursor_icon(CursorIcon::Move);
                    }
                    // A sticky note shows its text under the pointer.
                    if a.spec.kind == AnnotKind::Text && !a.spec.contents.is_empty() && self.annot.editor.is_none() {
                        resp.clone().on_hover_ui_at_pointer(|ui| {
                            ui.set_max_width(320.0);
                            ui.label(&a.spec.contents);
                            if !a.spec.author.is_empty() {
                                ui.weak(&a.spec.author);
                            }
                        });
                    }
                }
            }
            return false;
        }

        ctx.set_cursor_icon(CursorIcon::Crosshair);
        let to_page = |v: &DocView, s: Pos2| -> Option<(u32, [f32; 2])> {
            let page = v.layout.page_at(v.to_content(s))?;
            let (x, y) = v.screen_to_page(page, s);
            Some((page, [x, y]))
        };
        if resp.drag_started_by(egui::PointerButton::Primary)
            && let Some(start) = ctx.input(|i| i.pointer.press_origin())
            && let Some((page, p)) = to_page(self, start)
        {
            let anchor = if matches!(tool, Tool::Highlight | Tool::Underline | Tool::StrikeOut) { self.hit_text(start).map(|h| h.0) } else { None };
            self.annot.draft = Some(Draft { page, start: p, cur: p, points: vec![p], anchor });
        }
        if resp.dragged()
            && let Some(pos) = pointer
        {
            let (x, y) = self.annot.draft.as_ref().map(|d| self.screen_to_page(d.page, pos)).unwrap_or_default();
            let hit = if matches!(tool, Tool::Highlight | Tool::Underline | Tool::StrikeOut) { self.hit_text(pos).map(|h| h.0) } else { None };
            if let Some(d) = &mut self.annot.draft {
                d.cur = [x, y];
                if tool == Tool::Ink {
                    d.points.push([x, y]);
                }
                if let (Some(a), Some(h)) = (d.anchor, hit) {
                    self.sel = Some(super::Selection { anchor: a, head: h });
                }
            }
        }
        let released = resp.drag_stopped();
        let clicked = resp.clicked();
        if released || clicked {
            let draft = self.annot.draft.take().or_else(|| {
                let p = pointer?;
                let (page, pt) = to_page(self, p)?;
                Some(Draft { page, start: pt, cur: pt, points: vec![pt], anchor: None })
            });
            if let Some(d) = draft {
                self.finish_draft(tool, d);
            }
        }
        true
    }

    fn finish_draft(&mut self, tool: Tool, d: Draft) {
        let color = self.tool_color();
        let width = self.tool_width();
        let r = norm(d.start, d.cur);
        let big = r.width() > 3.0 || r.height() > 3.0;
        let spec = match tool {
            Tool::Highlight | Tool::Underline | Tool::StrikeOut => {
                self.markup_selection(tool);
                return;
            }
            Tool::Rect | Tool::Ellipse if big => {
                let mut s = AnnotSpec::new(if tool == Tool::Rect { AnnotKind::Square } else { AnnotKind::Circle }, color);
                s.rect = r;
                s.width = width;
                s
            }
            Tool::Line | Tool::Arrow if big => {
                let mut s = AnnotSpec::new(AnnotKind::Line, color);
                s.line = Some((d.start, d.cur));
                s.arrow = tool == Tool::Arrow;
                s.width = width;
                s
            }
            Tool::Ink if d.points.len() > 1 => {
                let mut s = AnnotSpec::new(AnnotKind::Ink, color);
                s.ink = vec![d.points];
                s.width = width;
                s
            }
            Tool::TextBox => {
                // A click on existing text edits it.
                if !big && let Some(a) = self.annot_at_point(d.page, d.start).filter(|a| a.spec.kind == AnnotKind::FreeText) {
                    self.edit_existing(d.page, a);
                    return;
                }
                let mut s = self.new_text_at(d.start);
                if big {
                    s.typewriter = false;
                    s.rect = r;
                }
                self.open_editor(d.page, None, s, None);
                return;
            }
            Tool::Note => {
                if let Some(a) = self.annot_at_point(d.page, d.start).filter(|a| a.spec.kind == AnnotKind::Text) {
                    self.edit_existing(d.page, a);
                    return;
                }
                let s = self.new_note_at(d.start);
                self.open_editor(d.page, None, s, None);
                return;
            }
            Tool::Image => {
                if let Some(data) = self.annot.image.take() {
                    self.submit(AnnotOp::AddImage { page: d.page, data, place: ImagePlace { center: d.start, fit: big.then_some(r) } });
                }
                self.annot.tool = Tool::Select;
                self.status.clear();
                return;
            }
            _ => return,
        };
        self.submit(AnnotOp::Create { page: d.page, spec });
    }

    /// Selection outline and handles, move and resize previews, the shape
    /// being drawn, and the annotation hidden under the text editor.
    pub(super) fn draw_annot_overlay(&mut self, painter: &egui::Painter, page: u32) {
        let accent = Color32::from_rgb(40, 120, 255);
        if let Some(ed) = &self.annot.editor
            && ed.page == page
        {
            if let Some(c) = ed.cover {
                painter.rect_filled(self.page_rect_to_screen(page, c).expand(1.0), 0.0, Color32::WHITE);
            }
            if ed.spec.kind == AnnotKind::Text && ed.id.is_none() {
                let r = self.page_rect_to_screen(page, ed.spec.rect);
                painter.rect(r, 2.0, c32(ed.spec.color), Stroke::new(1.0, Color32::from_gray(90)), StrokeKind::Inside);
            }
        }
        // The text being edited shows the editor's outline instead.
        let editing = self.annot.editor.as_ref().and_then(|e| e.id);
        if let Some((sp, id)) = self.annot.selected
            && sp == page
            && editing != Some(id)
            && let Some((_, a)) = self.selected_annot()
        {
            let resizing = self.annot.resizing.as_ref().filter(|r| r.id == id).map(|r| r.rect);
            let off = self.annot.moving.as_ref().filter(|m| m.id == id).map(|m| m.delta).unwrap_or_default();
            let outline = match resizing {
                Some(r) => self.page_rect_to_screen(page, r),
                None => self.page_rect_to_screen(page, a.bounds).translate(off),
            };
            dashed(painter, outline.expand(2.0), Stroke::new(1.5, accent));
            if a.spec.kind.is_resizable() && self.annot.moving.is_none() && self.annot.editor.is_none() {
                let hr = self.page_rect_to_screen(page, resizing.unwrap_or(a.spec.rect));
                for c in corners(hr) {
                    painter.rect(Rect::from_center_size(c, vec2(2.0 * HANDLE, 2.0 * HANDLE)), 1.0, Color32::WHITE, Stroke::new(1.5, accent), StrokeKind::Middle);
                }
            }
        }
        let Some(d) = &self.annot.draft else { return };
        if d.page != page {
            return;
        }
        let color = c32(self.tool_color());
        let stroke = Stroke::new((self.tool_width() * self.zoom).max(1.0), color);
        let a = self.page_to_screen(page, d.start);
        let b = self.page_to_screen(page, d.cur);
        match self.annot.tool {
            Tool::Rect => {
                painter.rect_stroke(Rect::from_two_pos(a, b), 0.0, stroke, StrokeKind::Middle);
            }
            Tool::TextBox | Tool::Image => {
                dashed(painter, Rect::from_two_pos(a, b), Stroke::new(1.5, accent));
            }
            Tool::Ellipse => {
                let r = Rect::from_two_pos(a, b);
                let pts: Vec<Pos2> = (0..=48)
                    .map(|i| {
                        let t = i as f32 / 48.0 * std::f32::consts::TAU;
                        pos2(r.center().x + r.width() * 0.5 * t.cos(), r.center().y + r.height() * 0.5 * t.sin())
                    })
                    .collect();
                painter.add(Shape::line(pts, stroke));
            }
            Tool::Line | Tool::Arrow => {
                painter.line_segment([a, b], stroke);
            }
            Tool::Ink => {
                let pts: Vec<Pos2> = d.points.iter().map(|p| self.page_to_screen(page, *p)).collect();
                painter.add(Shape::line(pts, stroke));
            }
            _ => {}
        }
    }

    pub(super) fn annot_keys(&mut self, ctx: &egui::Context) {
        // Ctrl+V: egui turns it into an event when the clipboard holds text, and
        // into nothing when it holds only an image.
        let text = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Paste(s) => Some(s.clone()),
                _ => None,
            })
        });
        let down = paste_key_down(ctx);
        let pressed = down && !self.annot.paste_down;
        self.annot.paste_down = down;
        if text.is_some() || pressed {
            self.paste(ctx, None, text);
        }
        let consume = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        if consume(Modifiers::COMMAND, Key::Z) {
            self.submit(AnnotOp::Undo);
        }
        if consume(Modifiers::COMMAND, Key::Y) || consume(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) {
            self.submit(AnnotOp::Redo);
        }
        if consume(Modifiers::COMMAND | Modifiers::SHIFT, Key::S) {
            self.save(true);
        }
        if consume(Modifiers::COMMAND, Key::S) {
            self.save(false);
        }
        if let Some((page, id)) = self.annot.selected
            && (consume(Modifiers::NONE, Key::Delete) || consume(Modifiers::NONE, Key::Backspace))
        {
            self.submit(AnnotOp::Delete { page, id });
            self.annot.selected = None;
        }
        if self.annot.tool != Tool::Select && consume(Modifiers::NONE, Key::Escape) {
            self.annot.tool = Tool::Select;
            self.annot.draft = None;
            self.annot.image = None;
            self.status.clear();
        }
    }

    /// Save: incremental into the same file, or a full copy elsewhere.
    pub fn save(&mut self, save_as: bool) {
        if !self.is_pdf() {
            self.status = "PDF 以外は保存できません".into();
            return;
        }
        self.commit_editor(true);
        if save_as {
            self.annot.confirm_save_as = true;
            self.annot.decrypt = true;
            return;
        }
        let path = self.doc.info().path.clone();
        self.annot.saving = Some(self.doc.save(SaveOptions { path, incremental: true, decrypt: false }));
        self.status = "保存しています…".into();
    }

    fn save_as_dialog(&mut self) {
        let stem = self.doc.info().path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(path) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(format!("{stem}_edited.pdf")).save_file() else { return };
        if path == self.doc.info().path {
            // Rewriting the open file in place is not possible; append instead.
            self.save(false);
            return;
        }
        self.annot.saving = Some(self.doc.save(SaveOptions { path, incremental: false, decrypt: self.annot.decrypt }));
        self.status = "保存しています…".into();
    }

    pub(super) fn poll_annot(&mut self, ctx: &egui::Context) {
        let mut done = Vec::new();
        self.annot.pending.retain_mut(|p| match p.poll() {
            Some(r) => {
                done.push(r.clone());
                false
            }
            None => true,
        });
        for r in done {
            match r {
                Ok(AnnotResult { page: Some(page), id: Some(id) }) => self.annot.selected = Some((page, id)),
                Ok(_) => {}
                Err(e) => self.status = format!("注釈の操作に失敗しました: {e}"),
            }
        }
        if let Some(s) = &mut self.annot.saving {
            match s.poll() {
                Some(Ok(())) => {
                    self.status = "保存しました".into();
                    self.annot.saving = None;
                }
                Some(Err(e)) => {
                    let e = e.clone();
                    self.annot.saving = None;
                    self.status = format!("上書き保存できませんでした（{e}）。名前を付けて保存してください");
                    self.annot.confirm_save_as = true;
                    self.annot.decrypt = true;
                }
                None => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        if !self.annot.pending.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
    }

    pub(super) fn annot_dialogs(&mut self, ctx: &egui::Context) {
        self.editor_ui(ctx);

        // Properties of the selected annotation.
        if self.annot.editor.is_none()
            && self.annot.resizing.is_none()
            && let Some((page, a)) = self.selected_annot()
        {
            let id = a.id;
            let mut spec = a.spec.clone();
            let mut changed = false;
            let mut delete = false;
            let mut close = false;
            let mut edit = false;
            egui::Window::new(format!("注釈：{}", a.spec.kind.label()))
                .id(Id::new(("annot-props", self.id)))
                .collapsible(true)
                .resizable(false)
                .anchor(egui::Align2::RIGHT_TOP, [-16.0, 90.0])
                .show(ctx, |ui| {
                    match &spec.kind {
                        AnnotKind::Other(_) => {
                            ui.weak("この種類の注釈は削除だけできます");
                        }
                        AnnotKind::Stamp => {
                            changed |= ui.add(egui::Slider::new(&mut spec.opacity, 0.1..=1.0).text("不透明度")).drag_stopped();
                            ui.weak("角をドラッグすると縦横比を保って大きさを変える (Shift で自由に)");
                        }
                        _ => {
                            ui.horizontal(|ui| {
                                ui.label(if spec.kind == AnnotKind::FreeText { "文字の色" } else { "色" });
                                changed |= egui::color_picker::color_edit_button_rgb(ui, &mut spec.color).changed();
                            });
                            if matches!(spec.kind, AnnotKind::Square | AnnotKind::Circle | AnnotKind::Line | AnnotKind::Ink) {
                                changed |= ui.add(egui::Slider::new(&mut spec.width, 0.5..=12.0).text("線幅")).drag_stopped();
                            }
                            if matches!(spec.kind, AnnotKind::Square | AnnotKind::Circle | AnnotKind::FreeText) {
                                let mut fill = spec.fill.is_some();
                                let label = if spec.kind == AnnotKind::FreeText { "背景を塗る" } else { "塗りつぶし" };
                                ui.horizontal(|ui| {
                                    if ui.checkbox(&mut fill, label).changed() {
                                        spec.fill = fill.then_some(if spec.kind == AnnotKind::FreeText { [1.0, 1.0, 1.0] } else { [1.0, 1.0, 0.8] });
                                        changed = true;
                                    }
                                    if let Some(f) = &mut spec.fill {
                                        changed |= egui::color_picker::color_edit_button_rgb(ui, f).changed();
                                    }
                                });
                            }
                            if spec.kind == AnnotKind::FreeText {
                                changed |= ui.add(egui::Slider::new(&mut spec.font_size, 4.0..=72.0).text("文字の大きさ")).drag_stopped();
                                ui.horizontal(|ui| {
                                    ui.label("書体");
                                    for f in [TextFont::Sans, TextFont::Serif] {
                                        if ui.selectable_label(spec.font == f, f.label()).clicked() && spec.font != f {
                                            spec.font = f;
                                            changed = true;
                                        }
                                    }
                                });
                                changed |= ui
                                    .checkbox(&mut spec.typewriter, "幅を文字に合わせる")
                                    .on_hover_text("外すと今の幅で折り返す。角をドラッグして幅を変えても外れる")
                                    .changed();
                            }
                            changed |= ui.add(egui::Slider::new(&mut spec.opacity, 0.1..=1.0).text("不透明度")).drag_stopped();
                            if matches!(spec.kind, AnnotKind::FreeText | AnnotKind::Text) {
                                if spec.kind == AnnotKind::Text && !spec.contents.is_empty() {
                                    ui.separator();
                                    ui.label(&spec.contents);
                                }
                                if ui.button(if spec.kind == AnnotKind::FreeText { "文字を書き直す (ダブルクリック)" } else { "内容を編集 (ダブルクリック)" }).clicked() {
                                    edit = true;
                                }
                            } else if !spec.contents.is_empty() {
                                ui.label(&spec.contents);
                            }
                        }
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("削除 (Delete)").clicked() {
                            delete = true;
                        }
                        if ui.button("閉じる").clicked() {
                            close = true;
                        }
                    });
                });
            if delete {
                self.submit(AnnotOp::Delete { page, id });
                self.annot.selected = None;
            } else if edit {
                self.edit_existing(page, a);
            } else if changed && spec != a.spec {
                self.submit(AnnotOp::Modify { page, id, spec });
            }
            if close {
                self.annot.selected = None;
            }
        }

        if self.annot.confirm_save_as {
            let mut go = None;
            egui::Modal::new(Id::new(("save-as", self.id))).show(ctx, |ui| {
                ui.heading("名前を付けて保存");
                let enc = &self.doc.info().encryption;
                if !enc.is_empty() && enc != "None" {
                    ui.checkbox(&mut self.annot.decrypt, format!("暗号を外して保存する（現在: {enc}）"));
                }
                ui.horizontal(|ui| {
                    if ui.button("保存先を選ぶ…").clicked() {
                        go = Some(true);
                    }
                    if ui.button("キャンセル").clicked() {
                        go = Some(false);
                    }
                });
            });
            if let Some(g) = go {
                self.annot.confirm_save_as = false;
                if g {
                    self.save_as_dialog();
                }
            }
        }
    }

    /// Unsaved annotation changes (shown in the tab title).
    pub fn dirty(&self) -> bool {
        self.doc.is_dirty()
    }
}
