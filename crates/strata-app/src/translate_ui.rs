//! Translation services shared by all tabs (an experimental feature): the
//! settings window (service, key, model, connection test), API keys kept in the
//! Windows Credential Manager, the confirmation before a document's text is
//! sent, and the notice for long documents.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crossbeam_channel::{Receiver, unbounded};
use egui::{Id, RichText, Ui};
use serde::{Deserialize, Serialize};
use strata_translate::providers::{self, PROVIDERS};
use strata_translate::{Batch, Translator};

/// Above this many characters, suggest translating through a subscription instead.
const LARGE_CHARS: usize = 100_000;
/// For estimates shown in yen.
const JPY_PER_USD: f64 = 150.0;
const TEST_TEXT: &str = "Tephra fallout deposits provide the main record of past explosive eruptions; their thickness decays exponentially with distance from the vent.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslateSettings {
    pub provider: String,
    /// Chosen model per service.
    pub models: BTreeMap<String, String>,
    /// Base URL of the OpenAI-compatible server.
    pub custom_base: String,
    /// Services the user no longer wants to confirm before sending.
    pub no_confirm: Vec<String>,
    /// How often the long-document notice has been shown.
    pub large_notice_seen: u32,
    pub large_notice_off: bool,
}

impl Default for TranslateSettings {
    fn default() -> Self {
        TranslateSettings {
            provider: "gemini".into(),
            models: BTreeMap::new(),
            custom_base: "http://localhost:8080/v1".into(),
            no_confirm: Vec::new(),
            large_notice_seen: 0,
            large_notice_off: false,
        }
    }
}

impl TranslateSettings {
    pub fn model(&self) -> String {
        let p = providers::provider(&self.provider);
        self.models.get(p.id).cloned().filter(|m| !m.is_empty()).unwrap_or_else(|| p.models.first().map(|m| m.id.to_string()).unwrap_or_default())
    }
}

pub enum EngineState {
    Ready(Arc<dyn Translator>),
    /// A dialog or the settings window is open.
    Waiting,
    /// The user cancelled.
    Declined,
    /// The user chose to export Markdown instead.
    Export,
}

enum Dialog {
    None,
    Confirm { doc: PathBuf, chars: usize, skip: bool },
    Large { doc: PathBuf, chars: usize, hide: bool },
}

pub struct TranslateManager {
    pub settings: TranslateSettings,
    /// Key per service, and whether it came from an environment variable.
    keys: HashMap<&'static str, Option<(String, bool)>>,
    dialog: Dialog,
    settings_open: bool,
    /// The document waiting for the settings to be completed.
    waiting_doc: Option<PathBuf>,
    key_input: String,
    key_note: Option<String>,
    test: Option<Receiver<Result<String, String>>>,
    test_result: Option<Result<String, String>>,
    /// (document, service) the user agreed to send in this session.
    approved: HashSet<(PathBuf, String)>,
    /// Documents whose long-document notice was answered in this session.
    large_seen: HashSet<PathBuf>,
    declined: HashSet<PathBuf>,
    export: HashSet<PathBuf>,
}

fn yen(usd: f64) -> String {
    let y = usd * JPY_PER_USD;
    if y < 1.0 { "1 円未満".into() } else { format!("約 {:.0} 円", y) }
}

impl TranslateManager {
    pub fn new(settings: TranslateSettings) -> TranslateManager {
        TranslateManager {
            settings,
            keys: HashMap::new(),
            dialog: Dialog::None,
            settings_open: false,
            waiting_doc: None,
            key_input: String::new(),
            key_note: None,
            test: None,
            test_result: None,
            approved: HashSet::new(),
            large_seen: HashSet::new(),
            declined: HashSet::new(),
            export: HashSet::new(),
        }
    }

    fn key_entry(&mut self, provider: &'static str) -> Option<(String, bool)> {
        let p = providers::provider(provider);
        self.keys
            .entry(p.id)
            .or_insert_with(|| {
                let clean = |k: String| Some(k.trim().to_string()).filter(|k| !k.is_empty());
                cred::read(&cred_target(p.id)).and_then(clean).map(|k| (k, false)).or_else(|| std::env::var(p.env).ok().and_then(clean).map(|k| (k, true)))
            })
            .clone()
    }

    fn key(&mut self, provider: &'static str) -> Option<String> {
        self.key_entry(provider).map(|k| k.0)
    }

    fn current(&self) -> &'static str {
        providers::provider(&self.settings.provider).id
    }

    /// "Service · model", for the toolbar.
    pub fn summary(&self) -> String {
        let p = providers::provider(&self.settings.provider);
        let model = self.settings.model();
        let label = p.models.iter().find(|m| m.id == model).map(|m| m.label.to_string()).unwrap_or(model);
        let service = match p.id {
            "gemini" => "Gemini",
            "openai" => "OpenAI",
            "anthropic" => "Anthropic",
            "openrouter" => "OpenRouter",
            _ => "互換サーバー",
        };
        if label.is_empty() {
            service.to_string()
        } else if label.starts_with(service) {
            label
        } else {
            format!("{service} · {label}")
        }
    }

    pub fn open_settings(&mut self) {
        self.settings_open = true;
        self.key_input.clear();
        self.key_note = None;
        self.test_result = None;
    }

    /// The service rejected the key: forget it so that the settings ask again.
    pub fn key_rejected(&mut self) {
        self.keys.remove(self.current());
    }

    fn engine_now(&mut self) -> Option<Arc<dyn Translator>> {
        let p = providers::provider(&self.settings.provider);
        let key = self.key(p.id);
        let model = self.settings.model();
        let base = self.settings.custom_base.trim().to_string();
        let ready = (!p.needs_key || key.is_some()) && !model.is_empty() && (p.id != "custom" || !base.is_empty());
        ready.then(|| providers::build(p.id, key, &model, &base))
    }

    /// The engine for translating `doc` (`chars` characters): shows the notice for
    /// long documents, asks for missing settings and confirms sending first.
    pub fn engine(&mut self, doc: &Path, chars: usize) -> EngineState {
        if self.declined.remove(doc) {
            return EngineState::Declined;
        }
        if self.export.remove(doc) {
            return EngineState::Export;
        }
        if !matches!(self.dialog, Dialog::None) || self.settings_open {
            return EngineState::Waiting;
        }
        if chars > LARGE_CHARS && !self.settings.large_notice_off && !self.large_seen.contains(doc) {
            self.dialog = Dialog::Large { doc: doc.to_path_buf(), chars, hide: false };
            return EngineState::Waiting;
        }
        let Some(engine) = self.engine_now() else {
            self.waiting_doc = Some(doc.to_path_buf());
            self.open_settings();
            return EngineState::Waiting;
        };
        let p = self.current();
        let local = p == "custom" && providers::is_local(&self.settings.custom_base);
        let key = (doc.to_path_buf(), p.to_string());
        if !local && !self.settings.no_confirm.iter().any(|s| s == p) && !self.approved.contains(&key) {
            self.dialog = Dialog::Confirm { doc: doc.to_path_buf(), chars, skip: false };
            return EngineState::Waiting;
        }
        EngineState::Ready(engine)
    }

    pub fn dialog_open(&self) -> bool {
        !matches!(self.dialog, Dialog::None) || self.settings_open
    }

    /// Estimated cost of `chars` characters with the current model, in yen.
    fn estimate(&self, chars: usize) -> Option<String> {
        let price = providers::price_of(&self.settings.provider, &self.settings.model())?;
        let mut s = yen(providers::estimate_usd(chars, price));
        if self.current() == "gemini" {
            s.push_str("（無料枠のキーなら無料）");
        }
        Some(s)
    }

    pub fn ui(&mut self, ctx: &egui::Context) {
        self.dialogs(ctx);
        if self.settings_open {
            self.settings_window(ctx);
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let summary = self.summary();
        let (doc, chars) = match &self.dialog {
            Dialog::None => return,
            Dialog::Large { doc, chars, .. } | Dialog::Confirm { doc, chars, .. } => (doc.clone(), *chars),
        };
        let estimate = self.estimate(chars);
        let can_hide = self.settings.large_notice_seen >= 1;
        let p = providers::provider(&self.settings.provider);
        let mut choice = None;
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::Large { hide, .. } => {
                egui::Modal::new(Id::new("translate-large")).show(ctx, |ui: &mut Ui| {
                    ui.set_width(540.0);
                    ui.heading(format!("長い文書です（約 {:.0} 万字）", chars as f64 / 10_000.0));
                    ui.add_space(4.0);
                    match &estimate {
                        Some(e) => ui.label(format!("API での翻訳は従量課金です。{summary} では、この文書に{e}かかる見込みです。時間も数分かかります。")),
                        None => ui.label("API での翻訳は従量課金で、時間も数分かかります。"),
                    };
                    ui.label("ChatGPT、Claude、Gemini などの月額プランをお使いなら、本文を Markdown に書き出して、Web 版やアプリ、CLI に翻訳を頼む方法もあります。プランの範囲内で済みます。");
                    if can_hide {
                        ui.checkbox(hide, "次回から表示しない");
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Markdown で書き出す…").clicked() {
                            choice = Some(0);
                        }
                        if ui.button("このまま翻訳する").clicked() {
                            choice = Some(1);
                        }
                        if ui.button("やめる").clicked() {
                            choice = Some(2);
                        }
                    });
                });
                if let Some(c) = choice {
                    self.settings.large_notice_seen += 1;
                    self.settings.large_notice_off |= *hide;
                    self.large_seen.insert(doc.clone());
                    match c {
                        0 => {
                            self.export.insert(doc);
                        }
                        2 => {
                            self.declined.insert(doc);
                        }
                        _ => {}
                    }
                    self.dialog = Dialog::None;
                }
            }
            Dialog::Confirm { skip, .. } => {
                egui::Modal::new(Id::new("translate-confirm")).show(ctx, |ui: &mut Ui| {
                    ui.set_width(560.0);
                    ui.heading("翻訳のために本文を送信します");
                    ui.add_space(4.0);
                    egui::Grid::new("tr-confirm").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label("送信先");
                        ui.label(&summary);
                        ui.end_row();
                        ui.label("送るもの");
                        ui.label(format!(
                            "{} の本文（参考文献を除く、約 {:.1} 万字）",
                            doc.file_name().map(|s| s.to_string_lossy()).unwrap_or_default(),
                            chars as f64 / 10_000.0
                        ));
                        ui.end_row();
                        if let Some(e) = &estimate {
                            ui.label("費用の目安");
                            ui.label(e);
                            ui.end_row();
                        }
                        ui.label("送信先での扱い");
                        ui.add(egui::Label::new(p.privacy).wrap());
                        ui.end_row();
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new("翻訳は実験的な機能です。訳文には誤訳が含まれるので、原文と照らし合わせて読んでください。").strong());
                    ui.label("機密の文書を送るかどうかはご自身で判断してください。");
                    ui.checkbox(skip, format!("{} では今後この確認を出さない", p.label));
                    ui.horizontal(|ui| {
                        if ui.button("送信して翻訳").clicked() {
                            choice = Some(1);
                        }
                        if ui.button("やめる").clicked() {
                            choice = Some(0);
                        }
                    });
                });
                match choice {
                    Some(1) => {
                        if *skip {
                            self.settings.no_confirm.push(p.id.to_string());
                        }
                        self.approved.insert((doc, p.id.to_string()));
                        self.dialog = Dialog::None;
                    }
                    Some(_) => {
                        self.declined.insert(doc);
                        self.dialog = Dialog::None;
                    }
                    None => {}
                }
            }
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.test
            && let Ok(r) = rx.try_recv()
        {
            self.test_result = Some(r);
            self.test = None;
        }
        let mut open = true;
        egui::Window::new("翻訳の設定（実験的機能）")
            .id(Id::new("translate-settings"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| self.settings_body(ui));
        if !open {
            self.settings_open = false;
            // Opened because settings were missing: without them, give up.
            if let Some(doc) = self.waiting_doc.take()
                && self.engine_now().is_none()
            {
                self.declined.insert(doc);
            }
        }
        if self.test.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }

    fn settings_body(&mut self, ui: &mut Ui) {
        ui.set_width(560.0);
        ui.label("訳文には誤訳が含まれます。論文を読むときは、必ず原文と照らし合わせてください。");
        ui.add_space(6.0);
        ui.label(RichText::new("送信先").strong());
        let before = self.settings.provider.clone();
        for p in &PROVIDERS {
            ui.radio_value(&mut self.settings.provider, p.id.to_string(), p.label);
        }
        if self.settings.provider != before {
            self.key_input.clear();
            self.key_note = None;
            self.test_result = None;
        }
        let p = providers::provider(&self.settings.provider);
        ui.separator();

        // Key.
        ui.label(RichText::new(if p.needs_key { "API キー" } else { "API キー（要る場合だけ）" }).strong());
        match self.key_entry(p.id) {
            Some((k, from_env)) => {
                ui.horizontal(|ui| {
                    let tail: String = k.chars().skip(k.chars().count().saturating_sub(4)).collect();
                    if from_env {
                        ui.label(format!("環境変数 {} のキーを使用中（末尾 …{tail}）", p.env));
                    } else {
                        ui.label(format!("保存済み（末尾 …{tail}）"));
                        if ui.button("削除").clicked() {
                            cred::delete(&cred_target(p.id));
                            self.keys.remove(p.id);
                        }
                    }
                });
            }
            None => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.key_input).password(true).desired_width(380.0).hint_text("キーを貼り付け"));
                    if ui.button("保存").clicked() {
                        self.save_key(p.id);
                    }
                });
            }
        }
        if let Some(n) = &self.key_note {
            ui.colored_label(egui::Color32::from_rgb(220, 150, 40), n);
        }
        let open_steps = p.needs_key && self.key(p.id).is_none();
        egui::CollapsingHeader::new("キーの取得方法").id_salt(("tr-steps", p.id)).default_open(open_steps).show(ui, |ui| {
            if let Some(u) = p.key_url {
                ui.hyperlink(u);
            }
            for (i, s) in p.steps.iter().enumerate() {
                ui.add(egui::Label::new(format!("{}. {s}", i + 1)).wrap());
            }
        });

        // Server and model.
        if p.id == "custom" {
            ui.label(RichText::new("サーバーの URL").strong());
            ui.add(egui::TextEdit::singleline(&mut self.settings.custom_base).desired_width(f32::INFINITY));
        }
        ui.label(RichText::new("モデル").strong());
        let mut model = self.settings.model();
        if !p.models.is_empty() {
            egui::ComboBox::from_id_salt(("tr-preset", p.id))
                .width(380.0)
                .selected_text(p.models.iter().find(|m| m.id == model).map(|m| m.label).unwrap_or("（一覧にないモデル）"))
                .show_ui(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    for m in p.models {
                        let cost = m.price.map(|pr| format!("　論文 1 本{}", yen(providers::estimate_usd(50_000, pr)))).unwrap_or_default();
                        ui.selectable_value(&mut model, m.id.to_string(), format!("{}{cost}", m.label));
                    }
                });
        }
        ui.horizontal(|ui| {
            ui.label("ID");
            ui.add(egui::TextEdit::singleline(&mut model).desired_width(360.0).hint_text("モデル ID"));
        });
        if model != self.settings.model() {
            self.settings.models.insert(p.id.to_string(), model);
            self.test_result = None;
        }
        if let Some(e) = self.estimate(50_000) {
            ui.weak(format!("論文 1 本（5 万字）の費用の目安: {e}。定価からの概算です。"));
        }
        ui.add_space(4.0);
        ui.label(RichText::new("送信先での扱い").strong());
        ui.add(egui::Label::new(RichText::new(p.privacy).weak()).wrap());
        ui.separator();

        // Connection test.
        ui.horizontal(|ui| {
            let can = self.test.is_none() && self.engine_now().is_some();
            if ui.add_enabled(can, egui::Button::new("接続テスト")).on_hover_text("短い英文を一つ訳してもらう").clicked()
                && let Some(engine) = self.engine_now()
            {
                let (tx, rx) = unbounded();
                let c = ui.ctx().clone();
                std::thread::Builder::new()
                    .name("strata-translate-test".into())
                    .spawn(move || {
                        let batch = Batch { title: "Connection test", context: "", items: vec![(0, TEST_TEXT)] };
                        let r = engine
                            .translate(&batch, &AtomicBool::new(false))
                            .map_err(|e| e.to_string())
                            .and_then(|v| v.into_iter().next().map(|(_, t)| t).ok_or_else(|| "訳文が返りませんでした".to_string()));
                        let _ = tx.send(r);
                        c.request_repaint();
                    })
                    .ok();
                self.test = Some(rx);
                self.test_result = None;
            }
            if self.test.is_some() {
                ui.spinner();
            }
        });
        match &self.test_result {
            Some(Ok(t)) => {
                ui.colored_label(egui::Color32::from_rgb(60, 170, 90), "つながりました");
                ui.add(egui::Label::new(RichText::new(t).weak()).wrap());
            }
            Some(Err(e)) => {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 60), "失敗しました");
                ui.add(egui::Label::new(RichText::new(e).weak()).wrap());
            }
            None => {}
        }
    }

    fn save_key(&mut self, provider: &'static str) {
        let k = self.key_input.trim().to_string();
        if k.is_empty() {
            self.key_note = Some("キーを貼り付けてください".into());
            return;
        }
        // A key of another service: save it there and switch, rather than store it in the wrong place.
        let target = match providers::detect(&k) {
            Some(other) if other != provider && provider != "custom" => {
                self.key_note = Some(format!("キーの形式から {} のキーと判断し、そちらに保存して切り替えました", providers::provider(other).label));
                self.settings.provider = other.to_string();
                other
            }
            _ => {
                self.key_note = None;
                provider
            }
        };
        match cred::write(&cred_target(target), &k) {
            Ok(()) => {
                self.keys.insert(providers::provider(target).id, Some((k, false)));
                self.key_input.clear();
            }
            Err(e) => self.key_note = Some(format!("保存できません: {e}")),
        }
    }
}

fn cred_target(provider: &str) -> String {
    format!("StrataPDF/{provider}-api-key")
}

/// Generic credentials of the current user.
mod cred {
    use windows::Win32::Security::Credentials::{CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW};
    use windows::core::{HSTRING, PWSTR};

    pub fn read(target: &str) -> Option<String> {
        let mut p: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: CredReadW allocates `p`, freed below with CredFree.
        unsafe {
            CredReadW(&HSTRING::from(target), CRED_TYPE_GENERIC, None, &mut p).ok()?;
            let c = &*p;
            let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec();
            CredFree(p as *const _);
            String::from_utf8(bytes).ok()
        }
    }

    pub fn write(target: &str, secret: &str) -> windows::core::Result<()> {
        let mut name: Vec<u16> = target.encode_utf16().chain([0]).collect();
        let mut user: Vec<u16> = "StrataPDF".encode_utf16().chain([0]).collect();
        let mut blob = secret.as_bytes().to_vec();
        let c = CREDENTIALW {
            Flags: CRED_FLAGS(0),
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(name.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(user.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: every pointer in `c` outlives the call.
        unsafe { CredWriteW(&c, 0) }
    }

    pub fn delete(target: &str) {
        // SAFETY: plain call with an owned string.
        let _ = unsafe { CredDeleteW(&HSTRING::from(target), CRED_TYPE_GENERIC, None) };
    }
}
