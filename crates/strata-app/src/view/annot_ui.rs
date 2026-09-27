//! Annotation tools of a document view: toolbar, drawing, selection, moving,
//! properties, text editing, undo/redo and saving.

use std::collections::HashMap;
use std::sync::Arc;

use egui::{Color32, CursorIcon, Id, Key, Modifiers, Pos2, Rect, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use strata_core::annot::{AnnotInfo, AnnotKind, AnnotSpec};
use strata_core::{AnnotOp, AnnotResult, Pending, QuadF, RectF, SaveOptions};

use super::{CHAR_POS_END, CHAR_POS_START, DocPos, DocView};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tool {
    /// Text selection, panning, selecting and moving annotations.
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
            Tool::TextBox => "テキスト",
            Tool::Note => "付箋",
        }
    }

    fn default_color(self) -> [f32; 3] {
        match self {
            Tool::Highlight => [1.0, 0.85, 0.1],
            Tool::Underline | Tool::Line | Tool::Arrow => [0.1, 0.4, 0.9],
            Tool::StrikeOut | Tool::Rect | Tool::Ellipse | Tool::Ink => [0.9, 0.15, 0.15],
            Tool::TextBox => [0.1, 0.1, 0.1],
            Tool::Note => [1.0, 0.8, 0.1],
            Tool::Select => [0.0, 0.0, 0.0],
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

struct TextEditor {
    page: u32,
    id: i32,
    spec: AnnotSpec,
    text: String,
    focus: bool,
}

pub struct AnnotState {
    pub bar: bool,
    pub tool: Tool,
    color: Option<[f32; 3]>,
    width: f32,
    /// Per page: (revision the list belongs to, list request).
    lists: HashMap<u32, (u32, Pending<Arc<Vec<AnnotInfo>>>)>,
    pub selected: Option<(u32, i32)>,
    draft: Option<Draft>,
    moving: Option<Moving>,
    editor: Option<TextEditor>,
    pending: Vec<(Pending<AnnotResult>, bool)>,
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
            lists: HashMap::new(),
            selected: None,
            draft: None,
            moving: None,
            editor: None,
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

impl DocView {
    fn tool_color(&self) -> [f32; 3] {
        self.annot.color.unwrap_or(self.annot.tool.default_color())
    }

    fn tool_width(&self) -> f32 {
        if self.annot.width > 0.0 { self.annot.width } else { 2.0 }
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

    fn annot_at(&mut self, screen: Pos2) -> Option<(u32, AnnotInfo)> {
        let c = self.to_content(screen);
        let page = self.layout.page_at(c)?;
        let (x, y) = self.screen_to_page(page, screen);
        let list = self.page_annots(page)?;
        let slack = 3.0 / self.zoom;
        // Topmost (last drawn) first.
        list.iter()
            .rev()
            .find(|a| {
                let b = a.bounds;
                x >= b.x0 - slack && x <= b.x1 + slack && y >= b.y0 - slack && y <= b.y1 + slack
            })
            .map(|a| (page, a.clone()))
    }

    fn submit(&mut self, op: AnnotOp, open_editor: bool) {
        let p = self.doc.edit(op);
        self.annot.pending.push((p, open_editor));
    }

    pub(super) fn annot_toolbar(&mut self, ui: &mut Ui) {
        egui::Panel::top(Id::new(("annotbar", self.id))).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for t in [Tool::Select, Tool::Highlight, Tool::Underline, Tool::StrikeOut, Tool::Rect, Tool::Ellipse, Tool::Line, Tool::Arrow, Tool::Ink, Tool::TextBox, Tool::Note] {
                    if ui.selectable_label(self.annot.tool == t, t.label()).clicked() {
                        self.annot.tool = t;
                        self.annot.color = None;
                        // Markup applies to an existing text selection right away.
                        if matches!(t, Tool::Highlight | Tool::Underline | Tool::StrikeOut) && self.sel.is_some() {
                            self.markup_selection(t);
                        }
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
                ui.add(egui::Slider::new(&mut self.annot.width, 0.5..=12.0).text("線幅").fixed_decimals(1));
                ui.separator();
                if ui.add_enabled(self.doc.can_undo(), egui::Button::new("元に戻す")).on_hover_text("Ctrl+Z").clicked() {
                    self.submit(AnnotOp::Undo, false);
                }
                if ui.add_enabled(self.doc.can_redo(), egui::Button::new("やり直し")).on_hover_text("Ctrl+Y").clicked() {
                    self.submit(AnnotOp::Redo, false);
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

    /// Apply a colour to the tool and to the selected annotation.
    fn set_color(&mut self, c: [f32; 3]) {
        self.annot.color = Some(c);
        if let Some((page, id)) = self.annot.selected
            && let Some(a) = self.page_annots(page).and_then(|l| l.iter().find(|a| a.id == id).cloned())
        {
            let mut spec = a.spec.clone();
            spec.color = c;
            self.submit(AnnotOp::Modify { page, id, spec }, false);
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
            self.submit(AnnotOp::Create { page: p, spec }, false);
        }
        self.sel = None;
    }

    /// Pointer handling for annotation tools. Returns true when the input was
    /// consumed (the default text selection / panning must not run).
    pub(super) fn annot_input(&mut self, ui: &Ui, resp: &egui::Response) -> bool {
        let ctx = ui.ctx().clone();
        let tool = self.annot.tool;
        let pointer = resp.interact_pointer_pos().or(ctx.pointer_hover_pos());

        if tool == Tool::Select {
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
            if resp.double_clicked()
                && let Some(p) = pointer
                && let Some((page, a)) = self.annot_at(p)
                && matches!(a.spec.kind, AnnotKind::FreeText | AnnotKind::Text)
            {
                self.annot.editor = Some(TextEditor { page, id: a.id, text: a.spec.contents.clone(), spec: a.spec, focus: true });
                return true;
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
                        self.submit(AnnotOp::Modify { page: m.page, id: m.id, spec }, false);
                    }
                }
                return true;
            }
            if let Some(h) = ctx.pointer_hover_pos()
                && resp.hovered()
                && self.annot.selected.is_some_and(|(pg, id)| self.annot_at(h).is_some_and(|(p2, a)| p2 == pg && a.id == id))
            {
                ctx.set_cursor_icon(CursorIcon::Move);
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
                let mut s = AnnotSpec::new(AnnotKind::FreeText, color);
                s.rect = if big { r } else { RectF { x0: d.start[0], y0: d.start[1], x1: d.start[0] + 200.0, y1: d.start[1] + 40.0 } };
                s.font_size = (self.tool_width() * 4.0).clamp(8.0, 36.0);
                s
            }
            Tool::Note => {
                let mut s = AnnotSpec::new(AnnotKind::Text, color);
                s.rect = RectF { x0: d.start[0], y0: d.start[1], x1: d.start[0] + 20.0, y1: d.start[1] + 20.0 };
                s
            }
            _ => return,
        };
        let open_editor = matches!(spec.kind, AnnotKind::FreeText | AnnotKind::Text);
        self.submit(AnnotOp::Create { page: d.page, spec }, open_editor);
    }

    /// Selection outline, move preview and the shape being drawn.
    pub(super) fn draw_annot_overlay(&mut self, painter: &egui::Painter, page: u32) {
        let accent = Color32::from_rgb(40, 120, 255);
        if let Some((sp, id)) = self.annot.selected
            && sp == page
            && let Some(a) = self.page_annots(page).and_then(|l| l.iter().find(|a| a.id == id).cloned())
        {
            let off = self.annot.moving.as_ref().filter(|m| m.id == id).map(|m| m.delta).unwrap_or_default();
            let r = Rect::from_min_max(self.page_to_screen(page, [a.bounds.x0, a.bounds.y0]), self.page_to_screen(page, [a.bounds.x1, a.bounds.y1])).translate(off).expand(2.0);
            painter.add(Shape::dashed_line(&[r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()], Stroke::new(1.5, accent), 5.0, 3.0));
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
            Tool::Rect | Tool::TextBox => {
                painter.rect_stroke(Rect::from_two_pos(a, b), 0.0, stroke, StrokeKind::Middle);
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
        let consume = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        if consume(Modifiers::COMMAND, Key::Z) {
            self.submit(AnnotOp::Undo, false);
        }
        if consume(Modifiers::COMMAND, Key::Y) || consume(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) {
            self.submit(AnnotOp::Redo, false);
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
            self.submit(AnnotOp::Delete { page, id }, false);
            self.annot.selected = None;
        }
        if self.annot.tool != Tool::Select && consume(Modifiers::NONE, Key::Escape) {
            self.annot.tool = Tool::Select;
            self.annot.draft = None;
        }
    }

    /// Save: incremental into the same file, or a full copy elsewhere.
    pub fn save(&mut self, save_as: bool) {
        if !self.doc.info().is_pdf {
            self.status = "PDF 以外は保存できません".into();
            return;
        }
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
        self.annot.pending.retain_mut(|(p, open)| match p.poll() {
            Some(r) => {
                done.push((r.clone(), *open));
                false
            }
            None => true,
        });
        for (r, open) in done {
            match r {
                Ok(AnnotResult { page: Some(page), id: Some(id) }) => {
                    self.annot.selected = Some((page, id));
                    if open {
                        let kind = if self.annot.tool == Tool::Note { AnnotKind::Text } else { AnnotKind::FreeText };
                        self.annot.editor = Some(TextEditor { page, id, spec: AnnotSpec::new(kind, self.tool_color()), text: String::new(), focus: true });
                    }
                }
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
        // Text of a FreeText box or sticky note.
        if let Some(ed) = &mut self.annot.editor {
            let mut close = None;
            egui::Window::new(if ed.spec.kind == AnnotKind::Text { "付箋の内容" } else { "テキスト" })
                .id(Id::new(("annot-editor", self.id)))
                .collapsible(false)
                .default_width(360.0)
                .show(ctx, |ui| {
                    let r = ui.add(egui::TextEdit::multiline(&mut ed.text).desired_rows(4).desired_width(f32::INFINITY));
                    if ed.focus {
                        r.request_focus();
                        ed.focus = false;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("確定 (Ctrl+Enter)").clicked() || ui.input(|i| i.modifiers.command && i.key_pressed(Key::Enter)) {
                            close = Some(true);
                        }
                        if ui.button("キャンセル").clicked() {
                            close = Some(false);
                        }
                    });
                });
            if let Some(ok) = close {
                let ed = self.annot.editor.take().unwrap();
                if ok {
                    // Re-read the stored properties so only the text changes.
                    let base = self.page_annots(ed.page).and_then(|l| l.iter().find(|a| a.id == ed.id).map(|a| a.spec.clone()));
                    let mut spec = base.unwrap_or(ed.spec);
                    spec.contents = ed.text;
                    self.submit(AnnotOp::Modify { page: ed.page, id: ed.id, spec }, false);
                }
            }
        }

        // Properties of the selected annotation.
        if self.annot.editor.is_none()
            && let Some((page, id)) = self.annot.selected
            && let Some(a) = self.page_annots(page).and_then(|l| l.iter().find(|a| a.id == id).cloned())
        {
            let mut spec = a.spec.clone();
            let mut changed = false;
            let mut delete = false;
            let mut close = false;
            egui::Window::new(format!("注釈：{}", a.spec.kind.label()))
                .id(Id::new(("annot-props", self.id)))
                .collapsible(true)
                .resizable(false)
                .anchor(egui::Align2::RIGHT_TOP, [-16.0, 90.0])
                .show(ctx, |ui| {
                    if !matches!(spec.kind, AnnotKind::Other(_)) {
                        ui.horizontal(|ui| {
                            ui.label("色");
                            changed |= egui::color_picker::color_edit_button_rgb(ui, &mut spec.color).changed();
                        });
                        if matches!(spec.kind, AnnotKind::Square | AnnotKind::Circle | AnnotKind::Line | AnnotKind::Ink) {
                            changed |= ui.add(egui::Slider::new(&mut spec.width, 0.5..=12.0).text("線幅")).drag_stopped();
                        }
                        if matches!(spec.kind, AnnotKind::Square | AnnotKind::Circle) {
                            let mut fill = spec.fill.is_some();
                            if ui.checkbox(&mut fill, "塗りつぶし").changed() {
                                spec.fill = fill.then_some([1.0, 1.0, 0.8]);
                                changed = true;
                            }
                            if let Some(f) = &mut spec.fill {
                                changed |= egui::color_picker::color_edit_button_rgb(ui, f).changed();
                            }
                        }
                        if spec.kind == AnnotKind::FreeText {
                            changed |= ui.add(egui::Slider::new(&mut spec.font_size, 6.0..=48.0).text("文字サイズ")).drag_stopped();
                        }
                        changed |= ui.add(egui::Slider::new(&mut spec.opacity, 0.1..=1.0).text("不透明度")).drag_stopped();
                        if matches!(spec.kind, AnnotKind::FreeText | AnnotKind::Text) && ui.button("テキストを編集…").clicked() {
                            self.annot.editor = Some(TextEditor { page, id, text: spec.contents.clone(), spec: spec.clone(), focus: true });
                        } else if !spec.contents.is_empty() {
                            ui.label(&spec.contents);
                        }
                    } else {
                        ui.weak("この種類の注釈は削除だけできます");
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
                self.submit(AnnotOp::Delete { page, id }, false);
                self.annot.selected = None;
            } else if changed && spec != a.spec {
                self.submit(AnnotOp::Modify { page, id, spec }, false);
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
