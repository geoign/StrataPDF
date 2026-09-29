//! Side-by-side translation in the text view (experimental): the toggle, the
//! translation job and the messages it produces.

use std::sync::Arc;

use egui::Ui;
use strata_core::reflow::ReflowDoc;
use strata_translate::{Event, Job, Store};

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
    /// Service, model and server it was started with.
    engine: String,
    job: Option<Job>,
}

/// First node at or after a reading position (1-based page, page-space y).
fn node_at(doc: &ReflowDoc, page: u32, y: f32) -> usize {
    doc.anchors.iter().position(|&(p, ay)| p + 1 > page || (p + 1 == page && ay >= y - 4.0)).unwrap_or(0)
}

fn engine_key(svc: &Services) -> String {
    let s = &svc.translate.settings;
    format!("{}|{}|{}", s.provider, s.model(), s.custom_base)
}

impl DocView {
    pub(super) fn translation_toolbar(&mut self, ui: &mut Ui, svc: &mut Services) {
        if ui
            .selectable_label(self.tr.on, "翻訳（実験的）")
            .on_hover_text("原文と訳文を左右に並べる（英語・中国語 → 日本語）。訳文には誤訳が含まれるので、原文と照らし合わせて読んでください")
            .clicked()
        {
            self.tr.on = !self.tr.on;
        }
        // Always shown, so that the service can be chosen before anything is sent.
        let tm = &mut *svc.translate;
        let provider = strata_translate::providers::provider(&tm.settings.provider);
        let mut model = tm.settings.model();
        let mut open_settings = false;
        egui::ComboBox::from_id_salt(("tr-model", self.id)).selected_text(tm.summary()).width(220.0).show_ui(ui, |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            for m in provider.models {
                ui.selectable_value(&mut model, m.id.to_string(), m.label);
            }
            if !provider.models.is_empty() {
                ui.separator();
            }
            if ui.button("翻訳の設定…").clicked() {
                open_settings = true;
                ui.close();
            }
        });
        if model != tm.settings.model() {
            tm.settings.models.insert(provider.id.to_string(), model);
        }
        if open_settings {
            tm.open_settings();
        }
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
        let want = engine_key(svc);
        let stale = self.tr.session.as_ref().is_none_or(|s| !Arc::ptr_eq(&s.doc, &doc) || s.engine != want);
        if stale {
            let path = self.doc.info().path.clone();
            let plan = Arc::new(strata_translate::plan(&doc));
            let chars: usize = plan.segments.iter().map(|s| s.text.chars().count()).sum();
            match svc.translate.engine(&path, chars) {
                EngineState::Waiting => return,
                EngineState::Declined => {
                    self.tr.on = false;
                    return;
                }
                EngineState::Export => {
                    self.tr.on = false;
                    self.export_markdown();
                    return;
                }
                EngineState::Ready(engine) => {
                    if plan.segments.is_empty() {
                        self.status = "翻訳する段落がありません（日本語の文書か、文字のないページだけです）".into();
                    }
                    let Some(pane) = &mut self.reflow else { return };
                    pane.set_bilingual(Some(plan.nodes()));
                    let store = Arc::new(Store::open(&path, &engine.id()));
                    let from = node_at(&doc, pane.at_page, pane.at_y);
                    let c = ctx.clone();
                    let job = strata_translate::start(plan.clone(), store, engine, from, Arc::new(move || c.request_repaint()));
                    self.tr.progress = (0, plan.segments.len());
                    self.tr.session = Some(Session { doc, engine: want, job: Some(job) });
                }
            }
        }
        let Some(pane) = &mut self.reflow else { return };
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
                        strata_translate::Error::RateLimited { daily: true, .. } => {
                            format!("{} の本日の利用上限に達しました。モデルを切り替えるか、明日に続きを翻訳してください（訳した分は保存済み）", svc.translate.summary())
                        }
                        strata_translate::Error::Auth(_) => {
                            svc.translate.key_rejected();
                            "API キーが受け付けられませんでした。「翻訳の設定…」から入れ直してください".into()
                        }
                        strata_translate::Error::Billing(m) => format!("残高または支払い設定を確認してください（{m}）"),
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
