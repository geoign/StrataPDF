//! Translation engines shared by all tabs: the API key (kept in the Windows
//! Credential Manager), the confirmation before a document's text is sent, and
//! engine construction.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::{Receiver, unbounded};
use egui::{Id, Key, Ui};
use serde::{Deserialize, Serialize};
use strata_translate::Translator;
use strata_translate::gemini::{Gemini, MODELS};
use strata_translate::llama::{self, Llama, LocalModel};

const CRED_TARGET: &str = "StrataPDF/gemini-api-key";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslateSettings {
    pub model: String,
    /// Ask before sending a document's text (once per document and session).
    pub confirm: bool,
}

impl Default for TranslateSettings {
    fn default() -> Self {
        TranslateSettings { model: MODELS[0].0.to_string(), confirm: true }
    }
}

pub enum EngineState {
    Ready(Arc<dyn Translator>),
    /// A dialog is open, or a model is downloading or loading.
    Waiting,
    /// The user cancelled.
    Declined,
    Failed(String),
}

enum Dialog {
    None,
    Key { input: String, error: Option<String>, focus: bool, for_doc: Option<PathBuf> },
    Confirm { doc: PathBuf, skip: bool },
    Download { spec: LocalModel, for_doc: PathBuf },
}

/// The local model: at most one is kept loaded.
enum Local {
    Idle,
    Downloading { id: &'static str, done: Arc<AtomicU64>, total: u64, rx: Receiver<Result<(), String>> },
    Loading { id: &'static str, rx: Receiver<Result<Arc<Llama>, String>> },
    Ready { id: &'static str, engine: Arc<Llama> },
    Failed { id: &'static str, error: String },
}

fn local_spec(id: &str) -> Option<LocalModel> {
    llama::MODELS.iter().find(|m| m.id == id).copied()
}

fn model_path(spec: &LocalModel) -> Option<PathBuf> {
    llama::models_dir().map(|d| d.join(spec.file))
}


pub struct TranslateManager {
    pub settings: TranslateSettings,
    key: Option<String>,
    dialog: Dialog,
    /// Documents the user agreed to send in this session.
    approved: HashSet<PathBuf>,
    /// Documents whose request the user cancelled (reported once).
    declined: HashSet<PathBuf>,
    local: Local,
}

impl TranslateManager {
    pub fn new(settings: TranslateSettings) -> TranslateManager {
        TranslateManager { settings, key: None, dialog: Dialog::None, approved: HashSet::new(), declined: HashSet::new(), local: Local::Idle }
    }

    fn key(&mut self) -> Option<String> {
        if self.key.is_none() {
            self.key = cred::read(CRED_TARGET).or_else(|| std::env::var("GEMINI_API_KEY").ok()).map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
        }
        self.key.clone()
    }

    pub fn model_label(&self) -> &'static str {
        if let Some(m) = local_spec(&self.settings.model) {
            return m.label;
        }
        MODELS.iter().find(|m| m.0 == self.settings.model).map(|m| m.1).unwrap_or(MODELS[0].1)
    }

    /// Models to choose from: (id, label, note).
    pub fn choices() -> Vec<(&'static str, &'static str, String)> {
        let mut v: Vec<_> = MODELS.iter().map(|(id, label)| (*id, *label, "Google、無料枠".to_string())).collect();
        for m in &llama::MODELS {
            let have = model_path(m).is_some_and(|p| p.is_file());
            v.push((m.id, m.label, if have { "この PC で実行".into() } else { format!("未取得、{} MB", m.size_mb) }));
        }
        v
    }

    /// Download or load progress of a local model.
    pub fn status(&self) -> Option<String> {
        match &self.local {
            Local::Downloading { done, total, .. } => Some(format!("翻訳モデルをダウンロード中 {:.0}%", done.load(Ordering::Relaxed) as f64 * 100.0 / (*total).max(1) as f64)),
            Local::Loading { .. } => Some("翻訳モデルを読み込み中…".into()),
            _ => None,
        }
    }

    fn poll_local(&mut self) {
        let next = match &self.local {
            Local::Downloading { id, rx, .. } => match rx.try_recv() {
                Ok(Ok(())) => Some(Local::Idle),
                Ok(Err(e)) => Some(Local::Failed { id, error: format!("ダウンロードに失敗しました: {e}") }),
                Err(_) => None,
            },
            Local::Loading { id, rx } => match rx.try_recv() {
                Ok(Ok(engine)) => Some(Local::Ready { id, engine }),
                Ok(Err(e)) => Some(Local::Failed { id, error: e }),
                Err(_) => None,
            },
            _ => None,
        };
        if let Some(n) = next {
            self.local = n;
        }
    }

    fn local_engine(&mut self, ctx: &egui::Context, spec: LocalModel, doc: &Path) -> EngineState {
        self.poll_local();
        match &self.local {
            Local::Ready { id, engine } if *id == spec.id => return EngineState::Ready(engine.clone()),
            Local::Downloading { .. } | Local::Loading { .. } => return EngineState::Waiting,
            Local::Failed { id, error } if *id == spec.id => {
                let e = error.clone();
                self.local = Local::Idle;
                return EngineState::Failed(e);
            }
            _ => {}
        }
        let Some(path) = model_path(&spec) else { return EngineState::Failed("モデルの保存先がありません".into()) };
        if !path.is_file() {
            self.dialog = Dialog::Download { spec, for_doc: doc.to_path_buf() };
            return EngineState::Waiting;
        }
        // Loading takes a second or two: do it off the UI thread.
        let (tx, rx) = unbounded();
        let c = ctx.clone();
        std::thread::Builder::new()
            .name("strata-llama-load".into())
            .spawn(move || {
                let _ = tx.send(Llama::load(spec, &path, 999).map(Arc::new).map_err(|e| e.to_string()));
                c.request_repaint();
            })
            .ok();
        self.local = Local::Loading { id: spec.id, rx };
        EngineState::Waiting
    }

    /// The engine for translating `doc`: asks for the key and for permission first
    /// (API), or for the download (local model).
    pub fn engine(&mut self, ctx: &egui::Context, doc: &Path) -> EngineState {
        if self.declined.remove(doc) {
            return EngineState::Declined;
        }
        if !matches!(self.dialog, Dialog::None) {
            return EngineState::Waiting;
        }
        if let Some(spec) = local_spec(&self.settings.model) {
            return self.local_engine(ctx, spec, doc);
        }
        let Some(key) = self.key() else {
            self.dialog = Dialog::Key { input: String::new(), error: None, focus: true, for_doc: Some(doc.to_path_buf()) };
            return EngineState::Waiting;
        };
        if self.settings.confirm && !self.approved.contains(doc) {
            self.dialog = Dialog::Confirm { doc: doc.to_path_buf(), skip: false };
            return EngineState::Waiting;
        }
        EngineState::Ready(Arc::new(Gemini::new(key, &self.settings.model)))
    }

    pub fn change_key(&mut self) {
        self.dialog = Dialog::Key { input: String::new(), error: None, focus: true, for_doc: None };
    }

    /// The stored key was rejected by the service.
    pub fn key_rejected(&mut self) {
        self.key = None;
    }

    pub fn dialog_open(&self) -> bool {
        !matches!(self.dialog, Dialog::None)
    }

    pub fn ui(&mut self, ctx: &egui::Context) {
        self.poll_local();
        if matches!(self.local, Local::Downloading { .. } | Local::Loading { .. }) {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        let model = self.model_label();
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::Download { spec, for_doc } => {
                let mut done = None;
                egui::Modal::new(Id::new("translate-download")).show(ctx, |ui: &mut Ui| {
                    ui.set_width(480.0);
                    ui.heading("翻訳モデルのダウンロード");
                    ui.label(format!("{}　容量 {} MB、ライセンス {}", spec.label, spec.size_mb, spec.license));
                    ui.hyperlink(spec.url);
                    if let Some(d) = llama::models_dir() {
                        ui.label(format!("保存先: {}", d.display()));
                    }
                    ui.label("翻訳はこの PC の中で行い、本文は外部に送りません。");
                    ui.horizontal(|ui| {
                        if ui.button("ダウンロードする").clicked() {
                            done = Some(true);
                        }
                        if ui.button("やめる").clicked() {
                            done = Some(false);
                        }
                    });
                });
                match done {
                    Some(true) => {
                        let spec = *spec;
                        if let Some(dest) = model_path(&spec) {
                            let done = Arc::new(AtomicU64::new(0));
                            let (tx, rx) = unbounded();
                            let (d, cx) = (done.clone(), ctx.clone());
                            std::thread::Builder::new()
                                .name("strata-model-download".into())
                                .spawn(move || {
                                    let r = llama::download(spec.url, &dest, &d, &|| cx.request_repaint());
                                    let _ = tx.send(r);
                                    cx.request_repaint();
                                })
                                .ok();
                            self.local = Local::Downloading { id: spec.id, done, total: spec.size_mb as u64 * 1_048_576, rx };
                        }
                        self.dialog = Dialog::None;
                    }
                    Some(false) => {
                        self.declined.insert(for_doc.clone());
                        self.dialog = Dialog::None;
                    }
                    None => {}
                }
            }
            Dialog::Key { input, error, focus, for_doc } => {
                let mut done = None;
                egui::Modal::new(Id::new("translate-key")).show(ctx, |ui: &mut Ui| {
                    ui.set_width(460.0);
                    ui.heading("Gemini API キー");
                    ui.label("翻訳には Google Gemini API のキーが要ります。無料枠のキーは次のページで作れます。");
                    ui.hyperlink("https://aistudio.google.com/apikey");
                    let r = ui.add(egui::TextEdit::singleline(input).password(true).desired_width(f32::INFINITY).hint_text("AIza…"));
                    if *focus {
                        r.request_focus();
                        *focus = false;
                    }
                    ui.weak("キーは Windows の資格情報マネージャーに保存します。");
                    if let Some(e) = error {
                        ui.colored_label(egui::Color32::RED, e.as_str());
                    }
                    ui.horizontal(|ui| {
                        if ui.button("保存").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) {
                            done = Some(true);
                        }
                        if ui.button("やめる").clicked() {
                            done = Some(false);
                        }
                    });
                });
                match done {
                    Some(true) => {
                        let k = input.trim().to_string();
                        if k.is_empty() {
                            *error = Some("キーを入力してください".into());
                        } else if let Err(e) = cred::write(CRED_TARGET, &k) {
                            *error = Some(format!("保存できません: {e}"));
                        } else {
                            self.key = Some(k);
                            self.dialog = Dialog::None;
                        }
                    }
                    Some(false) => {
                        if let Some(d) = for_doc.take() {
                            self.declined.insert(d);
                        }
                        self.dialog = Dialog::None;
                    }
                    None => {}
                }
            }
            Dialog::Confirm { doc, skip } => {
                let mut done = None;
                egui::Modal::new(Id::new("translate-confirm")).show(ctx, |ui: &mut Ui| {
                    ui.set_width(520.0);
                    ui.heading("翻訳のために本文を送信します");
                    ui.add_space(4.0);
                    egui::Grid::new("tr-confirm").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label("送信先");
                        ui.label(format!("Google Gemini API（無料枠、{model}）"));
                        ui.end_row();
                        ui.label("送るもの");
                        ui.label(format!("{} の本文（参考文献を除く）", doc.file_name().map(|s| s.to_string_lossy()).unwrap_or_default()));
                        ui.end_row();
                    });
                    ui.add_space(4.0);
                    ui.label("無料枠では、送った本文と訳文が Google の製品改善に使われ、人が読む場合があります。機密の文書を送るかどうかはご自身で判断してください。");
                    ui.checkbox(skip, "今後この確認を出さない");
                    ui.horizontal(|ui| {
                        if ui.button("送信して翻訳").clicked() {
                            done = Some(true);
                        }
                        if ui.button("やめる").clicked() {
                            done = Some(false);
                        }
                    });
                });
                match done {
                    Some(true) => {
                        if *skip {
                            self.settings.confirm = false;
                        }
                        self.approved.insert(doc.clone());
                        self.dialog = Dialog::None;
                    }
                    Some(false) => {
                        self.declined.insert(doc.clone());
                        self.dialog = Dialog::None;
                    }
                    None => {}
                }
            }
        }
    }
}

/// Generic credentials of the current user.
mod cred {
    use windows::Win32::Security::Credentials::{CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW, CredWriteW};
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
}
