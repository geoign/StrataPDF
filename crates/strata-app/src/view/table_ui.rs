//! Table extraction: select a region, extract (OCR when the page has no text),
//! copy as CSV, and review/edit in a grid when the structure looks doubtful.

use std::sync::Arc;

use crossbeam_channel::Receiver;
use egui::{Color32, CursorIcon, Id, Rect, Shape, Stroke, Ui};
use strata_core::RectF;
use strata_core::ocr::OcrEngine;
use strata_core::table::{TableResult, TableSource};

use super::DocView;

struct Job {
    rx: Receiver<Result<TableResult, String>>,
    page: u32,
    region: RectF,
}

struct Editor {
    cells: Vec<Vec<String>>,
    issues: Vec<String>,
    source: TableSource,
}

#[derive(Default)]
pub struct TableState {
    pub mode: bool,
    draft: Option<(u32, [f32; 2], [f32; 2])>,
    job: Option<Job>,
    /// Region waiting for the OCR engine.
    needs_ocr: Option<(u32, RectF)>,
    /// OCR engine if already loaded (refreshed every frame).
    pub(super) ocr: Option<Arc<dyn OcrEngine>>,
    editor: Option<Editor>,
    last: Option<TableResult>,
}

fn delimited(cells: &[Vec<String>], sep: char) -> String {
    let r = TableResult { rows: cells.to_vec(), issues: Vec::new(), source: TableSource::TextLayer };
    r.to_delimited(sep)
}

impl DocView {
    pub(super) fn table_toolbar_button(&mut self, ui: &mut Ui) {
        if ui.selectable_label(self.table.mode, "表").on_hover_text("ドラッグで表の範囲を選ぶと CSV でコピーします").clicked() {
            self.table.mode = !self.table.mode;
            if self.table.mode {
                self.annot.tool = super::annot_ui::Tool::Select;
            }
        }
    }

    /// Pointer input in table mode; always consumes the input.
    pub(super) fn table_input(&mut self, ui: &Ui, resp: &egui::Response) -> bool {
        let ctx = ui.ctx().clone();
        ctx.set_cursor_icon(CursorIcon::Crosshair);
        let to_page = |v: &DocView, s: egui::Pos2| -> Option<(u32, [f32; 2])> {
            let page = v.layout.page_at(v.to_content(s))?;
            let (x, y) = v.screen_to_page(page, s);
            Some((page, [x, y]))
        };
        if resp.drag_started_by(egui::PointerButton::Primary)
            && let Some(start) = ctx.input(|i| i.pointer.press_origin())
            && let Some((page, p)) = to_page(self, start)
        {
            self.table.draft = Some((page, p, p));
        }
        if resp.dragged()
            && let Some(pos) = resp.interact_pointer_pos()
            && let Some((page, a, _)) = self.table.draft
        {
            let (x, y) = self.screen_to_page(page, pos);
            self.table.draft = Some((page, a, [x, y]));
        }
        if resp.drag_stopped()
            && let Some((page, a, b)) = self.table.draft.take()
        {
            let region = RectF { x0: a[0].min(b[0]), y0: a[1].min(b[1]), x1: a[0].max(b[0]), y1: a[1].max(b[1]) };
            if region.width() > 10.0 && region.height() > 10.0 {
                self.start_table(page, region, self.table.ocr.clone());
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.table.mode = false;
            self.table.draft = None;
        }
        true
    }

    fn start_table(&mut self, page: u32, region: RectF, ocr: Option<Arc<dyn OcrEngine>>) {
        let rx = self.doc.extract_table(page, region, ocr);
        self.table.job = Some(Job { rx, page, region });
        self.status = "表を解析しています…".into();
    }

    pub(super) fn draw_table_overlay(&self, painter: &egui::Painter, page: u32) {
        let region = match (&self.table.draft, &self.table.job) {
            (Some((p, a, b)), _) if *p == page => Some((*a, *b)),
            (_, Some(j)) if j.page == page => Some(([j.region.x0, j.region.y0], [j.region.x1, j.region.y1])),
            _ => None,
        };
        let Some((a, b)) = region else { return };
        let r = Rect::from_two_pos(self.page_to_screen(page, a), self.page_to_screen(page, b));
        painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(40, 160, 90, 30));
        let c = Color32::from_rgb(40, 160, 90);
        painter.add(Shape::dashed_line(&[r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()], Stroke::new(1.5, c), 6.0, 3.0));
    }

    pub(super) fn poll_table(&mut self, ctx: &egui::Context, ocr: &mut crate::ocr_ui::OcrManager) {
        self.table.ocr = ocr.ocr.ready();
        if let Some((page, region)) = self.table.needs_ocr
            && let Some(e) = ocr.engine(ctx)
        {
            self.table.needs_ocr = None;
            self.start_table(page, region, Some(e));
        }
        let Some(job) = &self.table.job else { return };
        let Ok(r) = job.rx.try_recv() else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        };
        let (page, region) = (job.page, job.region);
        self.table.job = None;
        match r {
            Ok(t) if t.issues.is_empty() => {
                ctx.copy_text(t.to_csv());
                self.status = format!("表を CSV でコピーしました（{} 行 × {} 列{}）", t.rows.len(), t.cols(), if t.source == TableSource::Ocr { "、OCR" } else { "" });
                self.table.last = Some(t);
            }
            Ok(t) => {
                self.status = "表の構造に確認が必要な点があります".into();
                self.table.editor = Some(Editor { cells: t.rows.clone(), issues: t.issues.clone(), source: t.source });
                self.table.last = Some(t);
            }
            Err(e) if e == "OCR_REQUIRED" => {
                self.status = "この範囲には文字がないため OCR します…".into();
                self.table.needs_ocr = Some((page, region));
                if let Some(e) = ocr.engine(ctx) {
                    self.table.needs_ocr = None;
                    self.start_table(page, region, Some(e));
                }
            }
            Err(e) => self.status = format!("表を取り出せませんでした: {e}"),
        }
    }

    /// Reopen the last extracted table in the editor.
    pub(super) fn table_status_button(&mut self, ui: &mut Ui) {
        if self.table.editor.is_none()
            && let Some(t) = &self.table.last
            && ui.small_button("表を確認・編集").clicked()
        {
            self.table.editor = Some(Editor { cells: t.rows.clone(), issues: t.issues.clone(), source: t.source });
        }
    }

    pub(super) fn table_dialog(&mut self, ctx: &egui::Context) {
        let Some(ed) = &mut self.table.editor else { return };
        let mut open = true;
        let mut copied = None;
        egui::Window::new("表の確認と編集")
            .id(Id::new(("table-editor", self.id)))
            .open(&mut open)
            .default_size([760.0, 480.0])
            .resizable(true)
            .show(ctx, |ui| {
                if !ed.issues.is_empty() {
                    for i in &ed.issues {
                        ui.colored_label(Color32::from_rgb(230, 150, 40), format!("⚠ {i}"));
                    }
                    ui.label("セルを直接直してからコピーしてください。行や列の追加・削除もできます。");
                    ui.separator();
                }
                if ed.source == TableSource::Ocr {
                    ui.weak("この表は OCR で読み取りました。数字の読み違いに注意してください。");
                }
                let ncol = ed.cells.iter().map(Vec::len).max().unwrap_or(0);
                for r in &mut ed.cells {
                    r.resize(ncol, String::new());
                }
                let mut del_row = None;
                let mut del_col = None;
                let mut ins_row = None;
                let mut ins_col = None;
                egui::ScrollArea::both().max_height(ui.available_height() - 48.0).auto_shrink(false).show(ui, |ui| {
                    egui::Grid::new(("table-grid", self.id)).striped(true).show(ui, |ui| {
                        ui.label("");
                        for c in 0..ncol {
                            ui.horizontal(|ui| {
                                ui.weak(format!("{}", c + 1));
                                if ui.small_button("−").on_hover_text("この列を削除").clicked() {
                                    del_col = Some(c);
                                }
                                if ui.small_button("＋").on_hover_text("右に列を挿入").clicked() {
                                    ins_col = Some(c + 1);
                                }
                            });
                        }
                        ui.end_row();
                        // Column widths follow their longest cell.
                        let widths: Vec<f32> = (0..ncol)
                            .map(|c| {
                                let n = ed.cells.iter().filter_map(|r| r.get(c)).map(|v| v.chars().map(|ch| if (ch as u32) > 0x2E80 { 2 } else { 1 }).sum::<usize>()).max().unwrap_or(0);
                                (n as f32 * 7.5 + 24.0).clamp(60.0, 320.0)
                            })
                            .collect();
                        for (ri, row) in ed.cells.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                ui.weak(format!("{}", ri + 1));
                                if ui.small_button("−").on_hover_text("この行を削除").clicked() {
                                    del_row = Some(ri);
                                }
                                if ui.small_button("＋").on_hover_text("下に行を挿入").clicked() {
                                    ins_row = Some(ri + 1);
                                }
                            });
                            for (ci, cell) in row.iter_mut().enumerate() {
                                ui.add(egui::TextEdit::singleline(cell).desired_width(widths.get(ci).copied().unwrap_or(120.0)));
                            }
                            ui.end_row();
                        }
                    });
                });
                if let Some(r) = del_row {
                    ed.cells.remove(r);
                }
                if let Some(c) = del_col {
                    for r in &mut ed.cells {
                        if c < r.len() {
                            r.remove(c);
                        }
                    }
                }
                if let Some(r) = ins_row {
                    ed.cells.insert(r, vec![String::new(); ncol]);
                }
                if let Some(c) = ins_col {
                    for r in &mut ed.cells {
                        r.insert(c.min(r.len()), String::new());
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("CSV でコピー").clicked() {
                        copied = Some(delimited(&ed.cells, ','));
                    }
                    if ui.button("TSV でコピー（Excel に貼り付け）").on_hover_text("Excel はタブ区切りをセルに分けて貼り付けます").clicked() {
                        copied = Some(delimited(&ed.cells, '\t'));
                    }
                    ui.weak(format!("{} 行 × {} 列", ed.cells.len(), ncol));
                });
            });
        if let Some(text) = copied {
            ctx.copy_text(text);
            self.status = "表をコピーしました".into();
        }
        if !open {
            self.table.editor = None;
        }
    }
}
