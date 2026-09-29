//! Side-by-side translation in the text view: the toggle, the translation job
//! and the messages it produces.

use std::sync::Arc;

use egui::Ui;
use strata_core::reflow::ReflowDoc;
use strata_translate::{Event, Job, Plan, Store};

use super::{DocView, Services};
use crate::translate_ui::EngineState;

#[derive(Default)]
pub struct TrState {
    /// The user wants the translation shown.
    pub on: bool,
    session: Option<Session>,
    /// (translated, total) segments.
    progress: (usize, usize),
}

struct Session {
    /// The reflow it was built for: a rebuilt reflow (after OCR) starts a new session.
    doc: Arc<ReflowDoc>,
    model: String,
    #[allow(dead_code)]
    plan: Arc<Plan>,
    job: Option<Job>,
}

/// First node at or after a reading position (1-based page, page-space y).
fn node_at(doc: &ReflowDoc, page: u32, y: f32) -> usize {
    doc.anchors.iter().position(|&(p, ay)| p + 1 > page || (p + 1 == page && ay >= y - 4.0)).unwrap_or(0)
}

impl DocView {
    pub(super) fn translation_toolbar(&mut self, ui: &mut Ui, svc: &mut Services) {
        if ui.selectable_label(self.tr.on, "翻訳").on_hover_text("原文と訳文を左右に並べる（英語・中国語 → 日本語）").clicked() {
            self.tr.on = !self.tr.on;
        }
        // Always shown, so that the engine can be chosen before anything is sent.
        let tm = &mut *svc.translate;
        egui::ComboBox::from_id_salt(("tr-model", self.id)).selected_text(tm.model_label()).width(200.0).show_ui(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            for (id, label, note) in crate::translate_ui::TranslateManager::choices() {
                ui.selectable_value(&mut tm.settings.model, id.to_string(), format!("{label}　{note}"));
            }
            ui.separator();
            if ui.button("API キーを変更…").clicked() {
                tm.change_key();
                ui.close();
            }
        });
        let (done, total) = self.tr.progress;
        if total > 0 && done < total {
            if self.tr.session.as_ref().is_some_and(|s| s.job.is_some()) {
                ui.spinner();
            }
            ui.weak(format!("{done}/{total}"));
        }
    }

    /// Start, restart or stop the translation to match the toggle, and apply its results.
    pub(super) fn update_translation(&mut self, ctx: &egui::Context, svc: &mut Services) {
        let Some(pane) = &mut self.reflow else { return };
        if !self.tr.on {
            if pane.is_bilingual() {
                pane.set_bilingual(None);
            }
            self.tr.session = None;
            self.tr.progress = (0, 0);
            return;
        }
        let Some(doc) = pane.doc.clone() else { return };
        let model = svc.translate.settings.model.clone();
        let stale = self.tr.session.as_ref().is_none_or(|s| !Arc::ptr_eq(&s.doc, &doc) || s.model != model);
        if stale {
            let path = self.doc.info().path.clone();
            match svc.translate.engine(ctx, &path) {
                EngineState::Waiting => {
                    if let Some(s) = svc.translate.status() {
                        self.status = s;
                    }
                    return;
                }
                EngineState::Declined => {
                    self.tr.on = false;
                    return;
                }
                EngineState::Failed(e) => {
                    self.status = e;
                    self.tr.on = false;
                    return;
                }
                EngineState::Ready(engine) => {
                    let plan = Arc::new(strata_translate::plan(&doc));
                    if plan.segments.is_empty() {
                        self.status = "翻訳する段落がありません（日本語の文書か、文字のないページだけです）".into();
                    }
                    pane.set_bilingual(Some(plan.nodes()));
                    let store = Arc::new(Store::open(&path, &engine.id()));
                    let from = node_at(&doc, pane.at_page, pane.at_y);
                    let c = ctx.clone();
                    let job = strata_translate::start(plan.clone(), store, engine, from, Arc::new(move || c.request_repaint()));
                    self.tr.progress = (0, plan.segments.len());
                    if self.status.starts_with("翻訳モデル") {
                        self.status.clear();
                    }
                    self.tr.session = Some(Session { doc, model: model.clone(), plan, job: Some(job) });
                }
            }
        }
        let Some(s) = &mut self.tr.session else { return };
        let Some(job) = &s.job else { return };
        let mut finished = false;
        while let Ok(ev) = job.rx.try_recv() {
            match ev {
                Event::Translated(v) => pane.add_translations(v),
                Event::Progress { done, total } => self.tr.progress = (done, total),
                Event::Waiting { seconds, .. } => self.status = format!("利用上限に達したため {seconds} 秒待ってから続けます"),
                Event::Failed(ids) => {
                    self.status = format!("{} 段落を訳せませんでした", ids.len());
                    pane.mark_failed(ids);
                }
                Event::Stopped(e) => {
                    self.status = match e {
                        strata_translate::Error::RateLimited { daily: true, .. } => format!(
                            "{} の本日の無料枠を使い切りました。モデルを切り替えるか、明日に続きを翻訳してください（訳した分は保存済み）",
                            svc.translate.model_label()
                        ),
                        strata_translate::Error::Auth(_) => {
                            svc.translate.key_rejected();
                            "API キーが受け付けられませんでした。「API キーを変更…」から入れ直してください".into()
                        }
                        e => format!("翻訳を中断しました: {e}"),
                    };
                    finished = true;
                }
                Event::Done => {
                    finished = true;
                    if self.status.starts_with("利用上限") {
                        self.status.clear();
                    }
                }
            }
        }
        if finished {
            s.job = None;
        }
    }
}
