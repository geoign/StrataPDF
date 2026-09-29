//! Application shell: tabs (egui_dock), menus, opening files, dialogs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{Id, Key, Modifiers, Ui, WidgetText};
use egui_dock::tab_viewer::OnCloseResponse;
use egui_dock::{DockArea, DockState, SurfaceIndex, TabPath, TabViewer};
use serde::{Deserialize, Serialize};
use strata_core::{DocId, DocInfo, Document, OpenError, RenderPool, ViewId, Waker};

use crate::layout::Spread;
use crate::tiles::TileCache;
use crate::view::{DocView, Fit, Services, Sidebar, ViewPrefs, ViewRequest};

const SETTINGS_KEY: &str = "strata-settings";
const MAX_RECENT: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub recent: Vec<PathBuf>,
    pub prefs: ViewPrefs,
    pub theme: ThemeChoice,
    /// GPU memory for cached tiles.
    pub tile_budget_mb: usize,
    pub ocr_device: strata_core::ocr::Device,
    /// Convert display formulas to LaTeX in the text view.
    pub formula_latex: bool,
    /// Font size of the text view relative to the default.
    pub text_scale: f32,
    pub translate: crate::translate_ui::TranslateSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { recent: Vec::new(), prefs: ViewPrefs::default(), theme: ThemeChoice::System, tile_budget_mb: 1024, ocr_device: strata_core::ocr::Device::Gpu, formula_latex: true, text_scale: 1.0, translate: Default::default() }
    }
}

struct OpenResult {
    path: PathBuf,
    result: Result<Document, OpenError>,
}

struct PasswordPrompt {
    path: PathBuf,
    input: String,
    error: Option<String>,
    focus: bool,
}

enum Action {
    ConfirmClose(ViewId),
    DuplicateRight(ViewId),
    MoveRight(ViewId),
    MoveDown(ViewId),
    Properties(ViewId),
    Print(ViewId),
}

pub struct StrataApp {
    dock: DockState<DocView>,
    pool: RenderPool,
    tiles: TileCache,
    settings: Settings,
    open_tx: Sender<OpenResult>,
    open_rx: Receiver<OpenResult>,
    msg_tx: Sender<String>,
    msg_rx: Receiver<String>,
    ipc_rx: Receiver<Vec<PathBuf>>,
    opening: Vec<PathBuf>,
    password: Option<PasswordPrompt>,
    errors: Vec<String>,
    registered: HashSet<DocId>,
    actions: Vec<Action>,
    props: Option<Arc<DocInfo>>,
    show_about: bool,
    fullscreen: bool,
    title: String,
    waker: Waker,
    web: wry::WebContext,
    ocr: crate::ocr_ui::OcrManager,
    translate: crate::translate_ui::TranslateManager,
    /// Tab awaiting a decision about unsaved changes.
    confirm_close: Option<ViewId>,
    /// Window close requested while documents have unsaved changes.
    confirm_quit: bool,
    allow_quit: bool,
    quit_after_save: bool,
    /// A menu or combo box was open at the end of the last frame.
    popup_open: bool,
}

impl StrataApp {
    pub fn new(cc: &eframe::CreationContext, files: Vec<PathBuf>) -> StrataApp {
        let ctx = cc.egui_ctx.clone();
        crate::setup_fonts(&ctx);
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, SETTINGS_KEY)).unwrap_or_default();
        apply_theme(&ctx, settings.theme);
        let wctx = ctx.clone();
        let waker: Waker = Arc::new(move || wctx.request_repaint());
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_sub(2).clamp(2, 16);
        let settings_device = settings.ocr_device;
        let settings_translate = settings.translate.clone();
        let (open_tx, open_rx) = unbounded();
        let (msg_tx, msg_rx) = unbounded();
        let (ipc_tx, ipc_rx) = unbounded();
        let ictx = ctx.clone();
        crate::instance::serve(ipc_tx, move || ictx.request_repaint());
        let mut app = StrataApp {
            dock: DockState::new(Vec::new()),
            pool: RenderPool::new(threads, waker.clone()),
            tiles: TileCache::new(settings.tile_budget_mb.max(128) << 20),
            settings,
            open_tx,
            open_rx,
            msg_tx,
            msg_rx,
            ipc_rx,
            opening: Vec::new(),
            password: None,
            errors: Vec::new(),
            registered: HashSet::new(),
            actions: Vec::new(),
            props: None,
            show_about: false,
            fullscreen: false,
            title: String::new(),
            waker,
            ocr: crate::ocr_ui::OcrManager::new(settings_device),
            translate: crate::translate_ui::TranslateManager::new(settings_translate),
            confirm_close: None,
            confirm_quit: false,
            allow_quit: false,
            quit_after_save: false,
            popup_open: false,
            web: wry::WebContext::new(directories::ProjectDirs::from("", "", "StrataPDF").map(|d| d.data_local_dir().join("WebView2"))),
        };
        for f in files {
            app.open(f, None);
        }
        app
    }

    pub fn open(&mut self, path: PathBuf, password: Option<String>) {
        let path = std::fs::canonicalize(&path).map(strip_verbatim).unwrap_or(path);
        // Already open: focus that tab.
        if password.is_none()
            && let Some(tp) = self.dock.find_tab_from(|t| t.doc.info().path == path)
        {
            let _ = self.dock.set_active_tab(tp);
            self.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: tp.surface, node: tp.node });
            return;
        }
        if self.opening.contains(&path) {
            return;
        }
        self.opening.push(path.clone());
        let tx = self.open_tx.clone();
        let waker = self.waker.clone();
        std::thread::Builder::new()
            .name("strata-open".into())
            .spawn(move || {
                let result = Document::open(&path, password, waker.clone());
                let _ = tx.send(OpenResult { path, result });
                waker();
            })
            .ok();
    }

    fn poll_opens(&mut self) {
        while let Ok(OpenResult { path, result }) = self.open_rx.try_recv() {
            self.opening.retain(|p| p != &path);
            match result {
                Ok(doc) => {
                    self.password = None;
                    let doc = Arc::new(doc);
                    self.pool.register(doc.client());
                    self.registered.insert(doc.id());
                    let view = DocView::new(doc, &self.settings.prefs);
                    let id = view.id;
                    self.dock.push_to_focused_leaf(view);
                    self.focus_tab(id);
                    self.remember(&path);
                }
                Err(OpenError::PasswordRequired) => {
                    self.password = Some(PasswordPrompt { path, input: String::new(), error: None, focus: true });
                }
                Err(OpenError::WrongPassword) => {
                    self.password = Some(PasswordPrompt { path, input: String::new(), error: Some("パスワードが違います".into()), focus: true });
                }
                Err(OpenError::Failed(e)) => {
                    self.errors.push(format!("{} を開けません: {e}", path.display()));
                }
            }
        }
    }

    fn remember(&mut self, path: &Path) {
        let r = &mut self.settings.recent;
        r.retain(|p| p != path);
        r.insert(0, path.to_path_buf());
        r.truncate(MAX_RECENT);
    }

    fn open_dialog(&mut self) {
        let files = rfd::FileDialog::new()
            .add_filter("文書", &["pdf", "epub", "xps", "oxps", "cbz", "fb2", "mobi", "svg", "png", "jpg", "jpeg", "tif", "tiff", "bmp", "gif", "jxr", "pnm"])
            .add_filter("すべてのファイル", &["*"])
            .pick_files();
        for f in files.into_iter().flatten() {
            self.open(f, None);
        }
    }

    fn focused_view(&mut self) -> Option<&mut DocView> {
        self.dock.find_active_focused().map(|(_, t)| t)
    }

    /// Make a tab active and give its leaf keyboard focus (egui_dock only
    /// focuses a leaf after it is clicked).
    fn focus_tab(&mut self, id: ViewId) {
        if let Some(tp) = self.tab_path(id) {
            let _ = self.dock.set_active_tab(tp);
            self.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: tp.surface, node: tp.node });
        }
    }

    fn tab_path(&self, id: ViewId) -> Option<TabPath> {
        self.dock.find_tab_from(|t| t.id == id)
    }

    fn close_focused(&mut self) {
        let Some((id, doc, dirty)) = self.focused_view().map(|v| (v.id, v.doc_id(), v.dirty())) else { return };
        // Ask only when this is the last tab showing the edited document.
        let views_of_doc = self.dock.iter_all_tabs().filter(|(_, t)| t.doc_id() == doc).count();
        if dirty && views_of_doc == 1 {
            self.confirm_close = Some(id);
            return;
        }
        self.close_tab(id);
    }

    fn close_tab(&mut self, id: ViewId) {
        if let Some(tp) = self.tab_path(id)
            && let Some(mut v) = self.dock.remove_tab(tp)
        {
            v.close(&self.pool);
        }
    }

    fn cycle_tabs(&mut self, dir: i32) {
        let paths: Vec<TabPath> = self.dock.iter_all_tabs().map(|(p, _)| p).collect();
        if paths.len() < 2 {
            return;
        }
        let Some(id) = self.focused_view().map(|v| v.id) else { return };
        let Some(cur) = self.tab_path(id).and_then(|tp| paths.iter().position(|p| *p == tp)) else { return };
        let n = paths.len() as i32;
        let next = paths[((cur as i32 + dir).rem_euclid(n)) as usize];
        let _ = self.dock.set_active_tab(next);
        self.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: next.surface, node: next.node });
    }

    fn run_actions(&mut self) {
        for a in std::mem::take(&mut self.actions) {
            match a {
                Action::DuplicateRight(id) => {
                    let Some(tp) = self.tab_path(id) else { continue };
                    let Some(dup) = self.dock.iter_all_tabs().find(|(_, t)| t.id == id).map(|(_, t)| t.duplicate()) else { continue };
                    self.split_into(tp, dup, true);
                }
                Action::MoveRight(id) | Action::MoveDown(id) => {
                    let right = matches!(a, Action::MoveRight(_));
                    let Some(tp) = self.tab_path(id) else { continue };
                    let alone = self.dock.leaf(egui_dock::NodePath { surface: tp.surface, node: tp.node }).map(|l| l.tabs.len() <= 1).unwrap_or(true);
                    if alone {
                        continue;
                    }
                    if let Some(tab) = self.dock.remove_tab(tp) {
                        self.split_into(tp, tab, right);
                    }
                }
                Action::ConfirmClose(id) => self.confirm_close = Some(id),
                Action::Properties(id) => {
                    self.props = self.dock.iter_all_tabs().find(|(_, t)| t.id == id).map(|(_, t)| t.doc.info().clone());
                }
                Action::Print(id) => {
                    if let Some((_, t)) = self.dock.iter_all_tabs().find(|(_, t)| t.id == id) {
                        let doc = t.doc.clone();
                        let page = t.current_page();
                        if let Err(e) = crate::print::print_dialog(&doc, page, self.msg_tx.clone()) {
                            self.errors.push(format!("印刷できません: {e}"));
                        }
                    }
                }
            }
        }
    }

    fn split_into(&mut self, tp: TabPath, tab: DocView, right: bool) {
        let id = tab.id;
        if tp.surface == SurfaceIndex::main() && self.dock.main_surface().root_node().is_some() {
            let node = tp.node;
            let tree = self.dock.main_surface_mut();
            if right {
                tree.split_right(node, 0.5, vec![tab]);
            } else {
                tree.split_below(node, 0.5, vec![tab]);
            }
        } else {
            self.dock.push_to_focused_leaf(tab);
        }
        self.focus_tab(id);
    }

    /// Unregister documents that no tab uses any more.
    fn gc_documents(&mut self) {
        let live: HashSet<DocId> = self.dock.iter_all_tabs().map(|(_, t)| t.doc_id()).collect();
        let dead: Vec<DocId> = self.registered.difference(&live).copied().collect();
        for d in dead {
            self.pool.unregister(d);
            self.tiles.forget_doc(d);
            self.registered.remove(&d);
        }
    }

    fn global_keys(&mut self, ctx: &egui::Context) {
        let consume = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        if consume(Modifiers::COMMAND, Key::O) {
            self.open_dialog();
        }
        if consume(Modifiers::COMMAND, Key::W) || consume(Modifiers::COMMAND, Key::F4) {
            self.close_focused();
        }
        if consume(Modifiers::COMMAND | Modifiers::SHIFT, Key::Tab) {
            self.cycle_tabs(-1);
        }
        if consume(Modifiers::COMMAND, Key::Tab) {
            self.cycle_tabs(1);
        }
        if consume(Modifiers::COMMAND, Key::P)
            && let Some(id) = self.focused_view().map(|v| v.id)
        {
            self.actions.push(Action::Print(id));
        }
        if consume(Modifiers::NONE, Key::F11) {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("ファイル", |ui| {
                    if ui.button("開く… (Ctrl+O)").clicked() {
                        self.open_dialog();
                    }
                    ui.menu_button("最近使ったファイル", |ui| {
                        if self.settings.recent.is_empty() {
                            ui.weak("なし");
                        }
                        let mut pick = None;
                        for p in &self.settings.recent {
                            if ui.button(p.display().to_string()).clicked() {
                                pick = Some(p.clone());
                            }
                        }
                        if !self.settings.recent.is_empty() {
                            ui.separator();
                            if ui.button("履歴を消去").clicked() {
                                self.settings.recent.clear();
                            }
                        }
                        if let Some(p) = pick {
                            self.open(p, None);
                            ui.close();
                        }
                    });
                    ui.separator();
                    let id = self.focused_view().map(|v| v.id);
                    if ui.add_enabled(id.is_some(), egui::Button::new("印刷… (Ctrl+P)")).clicked() {
                        self.actions.push(Action::Print(id.unwrap()));
                    }
                    if ui.add_enabled(id.is_some(), egui::Button::new("文書のプロパティ")).clicked() {
                        self.actions.push(Action::Properties(id.unwrap()));
                    }
                    ui.separator();
                    if ui.add_enabled(id.is_some(), egui::Button::new("タブを閉じる (Ctrl+W)")).clicked() {
                        self.close_focused();
                    }
                    if ui.button("終了").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("表示", |ui| {
                    let prefs = &mut self.settings.prefs;
                    ui.label("新しく開く文書の既定");
                    ui.radio_value(&mut prefs.fit, Fit::Width, "幅に合わせる");
                    ui.radio_value(&mut prefs.fit, Fit::Page, "ページ全体");
                    ui.separator();
                    ui.radio_value(&mut prefs.spread, Spread::Single, "単ページ");
                    ui.radio_value(&mut prefs.spread, Spread::Double, "見開き");
                    ui.radio_value(&mut prefs.spread, Spread::DoubleCover, "表紙+見開き");
                    ui.checkbox(&mut prefs.paged, "ページ送り表示");
                    let mut thumbs = prefs.sidebar != Sidebar::None;
                    if ui.checkbox(&mut thumbs, "サイドバーを表示").changed() {
                        prefs.sidebar = if thumbs { Sidebar::Thumbs } else { Sidebar::None };
                    }
                    ui.separator();
                    ui.label("テーマ");
                    let mut t = self.settings.theme;
                    ui.radio_value(&mut t, ThemeChoice::System, "システムに合わせる");
                    ui.radio_value(&mut t, ThemeChoice::Light, "ライト");
                    ui.radio_value(&mut t, ThemeChoice::Dark, "ダーク");
                    if t != self.settings.theme {
                        self.settings.theme = t;
                        apply_theme(ui.ctx(), t);
                    }
                    ui.separator();
                    ui.label("OCR の実行装置");
                    let mut d = self.settings.ocr_device;
                    ui.radio_value(&mut d, strata_core::ocr::Device::Gpu, "GPU（DirectML、使えなければ CPU）");
                    ui.radio_value(&mut d, strata_core::ocr::Device::Cpu, "CPU");
                    if d != self.settings.ocr_device {
                        self.settings.ocr_device = d;
                        self.ocr.set_device(d);
                    }
                    if ui.checkbox(&mut self.settings.formula_latex, "テキスト表示で数式を LaTeX に変換").on_hover_text("Pix2Text MFR（約 120 MB、初回ダウンロード）").changed() && self.settings.formula_latex {
                        self.ocr.formula.declined = false;
                    }
                    ui.separator();
                    if ui.checkbox(&mut self.fullscreen, "全画面 (F11)").changed() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                    }
                });
                ui.menu_button("タブ", |ui| {
                    let id = self.focused_view().map(|v| v.id);
                    if ui.add_enabled(id.is_some(), egui::Button::new("同じ文書を右に並べる")).clicked() {
                        self.actions.push(Action::DuplicateRight(id.unwrap()));
                    }
                    if ui.add_enabled(id.is_some(), egui::Button::new("このタブを右に分割")).clicked() {
                        self.actions.push(Action::MoveRight(id.unwrap()));
                    }
                    if ui.add_enabled(id.is_some(), egui::Button::new("このタブを下に分割")).clicked() {
                        self.actions.push(Action::MoveDown(id.unwrap()));
                    }
                    ui.separator();
                    if ui.button("次のタブ (Ctrl+Tab)").clicked() {
                        self.cycle_tabs(1);
                    }
                    if ui.button("前のタブ (Ctrl+Shift+Tab)").clicked() {
                        self.cycle_tabs(-1);
                    }
                    ui.weak("タブはドラッグして自由に分割できます");
                });
                ui.menu_button("ヘルプ", |ui| {
                    if ui.button("キー操作と情報").clicked() {
                        self.show_about = true;
                    }
                });
                if !self.opening.is_empty() {
                    ui.separator();
                    ui.spinner();
                    ui.label(format!("{} を開いています…", self.opening.len()));
                }
            });
        });
    }

    fn welcome(&mut self, ui: &mut Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.2);
                ui.heading("StrataPDF");
                ui.add_space(8.0);
                ui.label("ファイルをドロップするか、Ctrl+O で開いてください。");
                ui.add_space(16.0);
                if ui.button("ファイルを開く…").clicked() {
                    self.open_dialog();
                }
                ui.add_space(16.0);
                let mut pick = None;
                for p in self.settings.recent.iter().take(10) {
                    if ui.link(p.display().to_string()).clicked() {
                        pick = Some(p.clone());
                    }
                }
                if let Some(p) = pick {
                    self.open(p, None);
                }
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(pw) = &mut self.password {
            let mut submit = false;
            let mut cancel = false;
            egui::Modal::new(Id::new("password")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("パスワードが必要です");
                ui.label(pw.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
                let r = ui.add(egui::TextEdit::singleline(&mut pw.input).password(true).desired_width(f32::INFINITY));
                if pw.focus {
                    r.request_focus();
                    pw.focus = false;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    submit = true;
                }
                if let Some(e) = &pw.error {
                    ui.colored_label(egui::Color32::RED, e);
                }
                ui.horizontal(|ui| {
                    if ui.button("開く").clicked() {
                        submit = true;
                    }
                    if ui.button("キャンセル").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        cancel = true;
                    }
                });
            });
            if submit {
                let (path, input) = (pw.path.clone(), pw.input.clone());
                self.password = None;
                self.open(path, Some(input));
            } else if cancel {
                self.password = None;
            }
        }

        if let Some(info) = self.props.clone() {
            let mut open = true;
            egui::Window::new("文書のプロパティ").open(&mut open).resizable(true).default_width(460.0).show(ctx, |ui| {
                let size = std::fs::metadata(&info.path).map(|m| m.len()).unwrap_or(0);
                egui::Grid::new("props").num_columns(2).striped(true).show(ui, |ui| {
                    let mut row = |k: &str, v: String| {
                        ui.label(k);
                        ui.add(egui::Label::new(v).wrap());
                        ui.end_row();
                    };
                    row("ファイル", info.path.display().to_string());
                    row("サイズ", format!("{:.1} MB ({size} バイト)", size as f64 / 1048576.0));
                    row("形式", info.format.clone());
                    row("暗号", info.encryption.clone());
                    row("ページ数", info.page_count.to_string());
                    row("タイトル", info.title.clone());
                    row("作成者", info.author.clone());
                    row("サブタイトル", info.subject.clone());
                    row("キーワード", info.keywords.clone());
                    row("作成アプリ", info.creator.clone());
                    row("PDF 変換", info.producer.clone());
                    row("作成日", info.creation_date.clone());
                    row("更新日", info.mod_date.clone());
                    row("綴じ方向", if info.right_to_left { "右綴じ (R2L)".into() } else { "左綴じ".into() });
                    row("ページ配置", info.page_layout.clone().unwrap_or_default());
                });
            });
            if !open {
                self.props = None;
            }
        }

        if self.show_about {
            egui::Window::new("StrataPDF について").open(&mut self.show_about).default_width(420.0).show(ctx, |ui| {
                ui.label(format!("StrataPDF {}", env!("CARGO_PKG_VERSION")));
                ui.label("描画エンジン: MuPDF (AGPL-3.0)。私的利用に限る。");
                ui.separator();
                egui::Grid::new("keys").num_columns(2).show(ui, |ui| {
                    for (k, v) in [
                        ("↓ ↑ / PgDn PgUp / Space Shift+Space", "スクロール（ページ送り表示ではめくる）"),
                        ("← →", "前後のページ（右綴じ見開きでは逆向き）"),
                        ("Home / End", "先頭 / 末尾"),
                        ("Ctrl+ホイール / Ctrl + −", "拡大・縮小"),
                        ("Ctrl+0 / 1 / 2", "ページ全体 / 100% / 幅に合わせる"),
                        ("Ctrl+F, F3, Shift+F3", "検索、次へ、前へ"),
                        ("Ctrl+G", "ページ番号へ移動"),
                        ("Ctrl+C / Ctrl+A", "コピー / すべて選択"),
                        ("ドラッグ", "文字の上なら選択、それ以外はスクロール"),
                        ("中ボタン / Space+ドラッグ", "スクロール"),
                        ("テキスト表示: Ctrl+ホイール / Ctrl + −", "文字の拡大・縮小（Ctrl+0 で標準）"),
                        ("Ctrl+Tab", "タブ切り替え"),
                        ("Ctrl+W", "タブを閉じる"),
                        ("F9", "サイドバー"),
                        ("F11", "全画面"),
                        ("Ctrl+P", "印刷"),
                    ] {
                        ui.monospace(k);
                        ui.label(v);
                        ui.end_row();
                    }
                });
            });
        }

        if !self.errors.is_empty() {
            let mut clear = false;
            egui::Window::new("エラー").collapsible(false).resizable(true).anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0]).show(ctx, |ui| {
                for e in &self.errors {
                    ui.label(e);
                }
                if ui.button("閉じる").clicked() {
                    clear = true;
                }
            });
            if clear {
                self.errors.clear();
            }
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let t = match self.dock.find_active_focused() {
            Some((_, v)) => format!("{} — StrataPDF", v.title),
            None => "StrataPDF".to_string(),
        };
        if t != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(t.clone()));
            self.title = t;
        }
    }
}

struct Viewer<'a> {
    pool: &'a RenderPool,
    tiles: &'a mut TileCache,
    focused: Option<ViewId>,
    drawn: &'a mut Vec<ViewId>,
    actions: &'a mut Vec<Action>,
    window: Option<&'a winit::window::Window>,
    web: &'a mut wry::WebContext,
    theme: strata_core::reflow::output::Theme,
    overlay: bool,
    ocr: &'a mut crate::ocr_ui::OcrManager,
    latex: bool,
    text_scale: &'a mut f32,
    translate: &'a mut crate::translate_ui::TranslateManager,
}

impl TabViewer for Viewer<'_> {
    type Tab = DocView;

    fn id(&mut self, tab: &mut DocView) -> Id {
        Id::new(("tab", tab.id))
    }

    fn title(&mut self, tab: &mut DocView) -> WidgetText {
        if tab.dirty() { format!("● {}", tab.title).into() } else { tab.title.clone().into() }
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut DocView) {
        self.drawn.push(tab.id);
        let mut svc = Services {
            pool: self.pool,
            tiles: self.tiles,
            focused: self.focused == Some(tab.id),
            window: self.window,
            web: Some(&mut *self.web),
            theme: self.theme,
            overlay: self.overlay,
            ocr: &mut *self.ocr,
            latex: self.latex,
            text_scale: &mut *self.text_scale,
            translate: &mut *self.translate,
        };
        tab.ui(ui, &mut svc);
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: &mut DocView, _path: egui_dock::NodePath) {
        if ui.button("同じ文書を右に並べる").clicked() {
            self.actions.push(Action::DuplicateRight(tab.id));
            ui.close();
        }
        if ui.button("右に分割").clicked() {
            self.actions.push(Action::MoveRight(tab.id));
            ui.close();
        }
        if ui.button("下に分割").clicked() {
            self.actions.push(Action::MoveDown(tab.id));
            ui.close();
        }
        ui.separator();
        if ui.button("文書のプロパティ").clicked() {
            self.actions.push(Action::Properties(tab.id));
            ui.close();
        }
        if ui.button("パスをコピー").clicked() {
            ui.ctx().copy_text(tab.doc.info().path.display().to_string());
            ui.close();
        }
    }

    fn on_tab_button(&mut self, tab: &mut DocView, response: &egui::Response) {
        response.clone().on_hover_text(tab.doc.info().path.display().to_string());
    }

    fn on_close(&mut self, tab: &mut DocView) -> OnCloseResponse {
        if tab.dirty() {
            self.actions.push(Action::ConfirmClose(tab.id));
            return OnCloseResponse::Ignore;
        }
        tab.close(self.pool);
        OnCloseResponse::Close
    }

    fn scroll_bars(&self, _tab: &DocView) -> [bool; 2] {
        [false, false]
    }
}

impl eframe::App for StrataApp {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let window = frame.winit_window().cloned();
        self.tiles.begin_frame();
        for t in self.pool.drain() {
            self.tiles.insert(&ctx, t);
        }
        self.poll_opens();
        while let Ok(m) = self.msg_rx.try_recv() {
            if let Some((_, v)) = self.dock.find_active_focused() {
                v.status = m;
            } else {
                self.errors.push(m);
            }
        }
        while let Ok(files) = self.ipc_rx.try_recv() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            for f in files {
                self.open(f, None);
            }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for p in dropped {
            self.open(p, None);
        }
        if self.password.is_none() {
            self.global_keys(&ctx);
        }
        self.menu_bar(ui);

        if self.dock.find_active_focused().is_none() {
            let first = self.dock.iter_all_tabs().next().map(|(_, t)| t.id);
            if let Some(id) = first {
                self.focus_tab(id);
            }
        }
        let focused = self.dock.find_active_focused().map(|(_, t)| t.id);
        let mut drawn = Vec::new();
        if self.dock.iter_all_tabs().next().is_none() {
            self.welcome(ui);
        } else {
            let theme = match self.settings.theme {
                ThemeChoice::System => strata_core::reflow::output::Theme::Auto,
                ThemeChoice::Light => strata_core::reflow::output::Theme::Light,
                ThemeChoice::Dark => strata_core::reflow::output::Theme::Dark,
            };
            let overlay = self.popup_open || self.ocr.dialog_open() || self.translate.dialog_open() || self.password.is_some() || self.props.is_some() || self.show_about || !self.errors.is_empty();
            let mut viewer = Viewer {
                pool: &self.pool,
                tiles: &mut self.tiles,
                focused,
                drawn: &mut drawn,
                actions: &mut self.actions,
                window: window.as_deref(),
                web: &mut self.web,
                theme,
                overlay,
                ocr: &mut self.ocr,
                latex: self.settings.formula_latex,
                text_scale: &mut self.settings.text_scale,
                translate: &mut self.translate,
            };
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                DockArea::new(&mut self.dock)
                    .id(Id::new("dock"))
                    .style(egui_dock::Style::from_egui(ui.style().as_ref()))
                    .show_add_buttons(false)
                    .show_leaf_collapse_buttons(false)
                    .show_inside(ui, &mut viewer);
            });
        }
        // Hidden tabs must not keep their tile requests alive or show webviews.
        let mut requests = Vec::new();
        for (_, t) in self.dock.iter_all_tabs_mut() {
            if !drawn.contains(&t.id) {
                self.pool.remove_view(t.id);
                t.hide_native();
            }
            for r in t.requests.drain(..) {
                requests.push((t.id, r));
            }
        }
        for (id, r) in requests {
            match r {
                ViewRequest::CloseTab => {
                    self.focus_tab(id);
                    self.close_focused();
                }
                ViewRequest::NextTab => self.cycle_tabs(1),
                ViewRequest::PrevTab => self.cycle_tabs(-1),
                ViewRequest::Open => self.open_dialog(),
                ViewRequest::Fullscreen => {
                    self.fullscreen = !self.fullscreen;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                }
                ViewRequest::Print => self.actions.push(Action::Print(id)),
            }
        }
        self.run_actions();
        self.gc_documents();
        self.dialogs(&ctx);
        self.unsaved_dialogs(&ctx);
        if let Some(e) = self.ocr.ui(&ctx) {
            self.errors.push(e);
        }
        self.translate.ui(&ctx);
        self.update_title(&ctx);
        self.tiles.end_frame();
        // egui lists open popups while a frame is built, so `any_popup_open` is only
        // meaningful at its end. Webviews (native windows drawn over egui) hide in the
        // next frame, which must come even without further input.
        let popup = ctx.any_popup_open();
        if popup != self.popup_open {
            self.popup_open = popup;
            ctx.request_repaint();
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.translate = self.translate.settings.clone();
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }
}

impl StrataApp {
    fn unsaved_dialogs(&mut self, ctx: &egui::Context) {
        // Window close with unsaved changes.
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_quit && self.dock.iter_all_tabs().any(|(_, t)| t.dirty()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }
        if self.confirm_quit {
            let names: Vec<String> = {
                let mut seen = HashSet::new();
                self.dock.iter_all_tabs().filter(|(_, t)| t.dirty() && seen.insert(t.doc_id())).map(|(_, t)| t.title.clone()).collect()
            };
            let mut choice = None;
            egui::Modal::new(Id::new("confirm-quit")).show(ctx, |ui| {
                ui.heading("保存していない注釈があります");
                for n in &names {
                    ui.label(format!("・{n}"));
                }
                ui.horizontal(|ui| {
                    if ui.button("すべて上書き保存して終了").clicked() {
                        choice = Some(1);
                    }
                    if ui.button("保存せずに終了").clicked() {
                        choice = Some(2);
                    }
                    if ui.button("キャンセル").clicked() {
                        choice = Some(0);
                    }
                });
            });
            match choice {
                Some(1) => {
                    let mut seen = HashSet::new();
                    for (_, t) in self.dock.iter_all_tabs_mut() {
                        if t.dirty() && seen.insert(t.doc_id()) {
                            t.save(false);
                        }
                    }
                    self.quit_after_save = true;
                    self.confirm_quit = false;
                }
                Some(2) => {
                    self.allow_quit = true;
                    self.confirm_quit = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Some(_) => self.confirm_quit = false,
                None => {}
            }
        }
        if self.quit_after_save && !self.dock.iter_all_tabs().any(|(_, t)| t.dirty()) {
            self.allow_quit = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Closing one tab with unsaved changes.
        if let Some(id) = self.confirm_close {
            let name = self.dock.iter_all_tabs().find(|(_, t)| t.id == id).map(|(_, t)| t.title.clone());
            let Some(name) = name else {
                self.confirm_close = None;
                return;
            };
            let mut choice = None;
            egui::Modal::new(Id::new("confirm-close")).show(ctx, |ui| {
                ui.heading("注釈を保存しますか？");
                ui.label(&name);
                ui.horizontal(|ui| {
                    if ui.button("上書き保存して閉じる").clicked() {
                        choice = Some(1);
                    }
                    if ui.button("保存せずに閉じる").clicked() {
                        choice = Some(2);
                    }
                    if ui.button("キャンセル").clicked() {
                        choice = Some(0);
                    }
                });
            });
            match choice {
                Some(1) => {
                    if let Some((_, t)) = self.dock.iter_all_tabs_mut().find(|(_, t)| t.id == id) {
                        // The save is queued before the document thread shuts down.
                        t.save(false);
                    }
                    self.close_tab(id);
                    self.confirm_close = None;
                }
                Some(2) => {
                    self.close_tab(id);
                    self.confirm_close = None;
                }
                Some(_) => self.confirm_close = None,
                None => {}
            }
        }
    }
}

fn apply_theme(ctx: &egui::Context, t: ThemeChoice) {
    ctx.set_theme(match t {
        ThemeChoice::System => egui::ThemePreference::System,
        ThemeChoice::Light => egui::ThemePreference::Light,
        ThemeChoice::Dark => egui::ThemePreference::Dark,
    });
}

/// `canonicalize` on Windows returns `\\?\C:\...`; MuPDF and humans prefer `C:\...`.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}
