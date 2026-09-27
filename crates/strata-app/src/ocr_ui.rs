//! Downloadable models (OCR, formula recognition): first-use download with a
//! confirmation dialog and progress, then background loading.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::{Receiver, unbounded};
use egui::{Id, Ui};
use strata_core::ocr::{Device, OcrEngine, models};
use strata_ocr::formula::FormulaEngine;

enum State<T: ?Sized> {
    Idle,
    Confirm,
    Downloading { done: Arc<AtomicU64>, total: u64, cancel: Arc<AtomicBool>, rx: Receiver<Result<(), String>> },
    Loading(Receiver<Result<Arc<T>, String>>),
    Ready(Arc<T>),
}

type Loader<T> = Arc<dyn Fn(&models::ModelSet, Device) -> Result<Arc<T>, String> + Send + Sync>;

/// One downloadable model set and the engine built from it.
pub struct ModelSlot<T: ?Sized + Send + Sync + 'static> {
    set_id: &'static str,
    state: State<T>,
    loader: Loader<T>,
    pub device: Device,
    pub waiting: bool,
    /// The user declined the download; do not ask again until re-enabled.
    pub declined: bool,
}

impl<T: ?Sized + Send + Sync + 'static> ModelSlot<T> {
    fn new(set_id: &'static str, device: Device, loader: Loader<T>) -> Self {
        ModelSlot { set_id, state: State::Idle, loader, device, waiting: false, declined: false }
    }

    fn set(&self) -> models::ModelSet {
        models::set(self.set_id).expect("built-in model set")
    }

    pub fn is_installed(&self) -> bool {
        self.set().is_installed()
    }

    /// The engine if ready; otherwise starts download (after confirmation) or loading.
    pub fn engine(&mut self, ctx: &egui::Context) -> Option<Arc<T>> {
        self.poll(ctx);
        match &self.state {
            State::Ready(e) => Some(e.clone()),
            State::Idle if self.declined => None,
            State::Idle => {
                if self.is_installed() {
                    self.start_loading(ctx);
                } else {
                    self.state = State::Confirm;
                }
                self.waiting = true;
                None
            }
            _ => {
                self.waiting = true;
                None
            }
        }
    }

    pub fn ready(&self) -> Option<Arc<T>> {
        match &self.state {
            State::Ready(e) => Some(e.clone()),
            _ => None,
        }
    }

    pub fn reset(&mut self) {
        if matches!(self.state, State::Ready(_)) {
            self.state = State::Idle;
        }
    }

    fn start_loading(&mut self, ctx: &egui::Context) {
        let (tx, rx) = unbounded();
        let (set, device, loader, c) = (self.set(), self.device, self.loader.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("strata-model-load".into())
            .spawn(move || {
                let _ = tx.send(loader(&set, device));
                c.request_repaint();
            })
            .ok();
        self.state = State::Loading(rx);
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        let set = self.set();
        let total = set.total_size();
        let done = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = unbounded();
        let (d, c, cx) = (done.clone(), cancel.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("strata-model-download".into())
            .spawn(move || {
                let r = models::install(&set, &|n, _| {
                    d.store(n, Ordering::Relaxed);
                    cx.request_repaint();
                }, &c)
                .map_err(|e| e.to_string());
                let _ = tx.send(r);
                cx.request_repaint();
            })
            .ok();
        self.state = State::Downloading { done, total, cancel, rx };
    }

    fn poll(&mut self, ctx: &egui::Context) -> Option<String> {
        let mut err = None;
        let next = match &self.state {
            State::Downloading { rx, .. } => match rx.try_recv() {
                Ok(Ok(())) => Some(None),
                Ok(Err(e)) => {
                    err = Some(format!("モデルのダウンロードに失敗しました: {e}"));
                    Some(Some(State::Idle))
                }
                Err(_) => None,
            },
            State::Loading(rx) => match rx.try_recv() {
                Ok(Ok(e)) => Some(Some(State::Ready(e))),
                Ok(Err(e)) => {
                    err = Some(format!("モデルを読み込めません: {e}"));
                    Some(Some(State::Idle))
                }
                Err(_) => None,
            },
            _ => None,
        };
        match next {
            Some(Some(s)) => {
                if matches!(s, State::Idle) {
                    self.waiting = false;
                }
                self.state = s;
            }
            Some(None) => self.start_loading(ctx),
            None => {}
        }
        err
    }

    /// Dialogs and progress; returns an error message to surface, if any.
    pub fn ui(&mut self, ctx: &egui::Context) -> Option<String> {
        let err = self.poll(ctx);
        match &self.state {
            State::Confirm => {
                let set = self.set();
                let mut decision = None;
                egui::Modal::new(Id::new(("model-download", self.set_id))).show(ctx, |ui: &mut Ui| {
                    ui.set_width(460.0);
                    ui.heading("モデルのダウンロード");
                    ui.label(format!("{}\n容量 {:.0} MB・ライセンス {}", set.title, set.total_size() as f64 / 1e6, set.license));
                    ui.hyperlink(&set.source);
                    ui.label(format!("保存先: {}", set.dir().display()));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("ダウンロードする").clicked() {
                            decision = Some(true);
                        }
                        if ui.button("やめる").clicked() {
                            decision = Some(false);
                        }
                    });
                });
                match decision {
                    Some(true) => self.start_download(ctx),
                    Some(false) => {
                        self.state = State::Idle;
                        self.waiting = false;
                        self.declined = true;
                    }
                    None => {}
                }
            }
            State::Downloading { done, total, cancel, .. } => {
                let frac = done.load(Ordering::Relaxed) as f32 / (*total).max(1) as f32;
                let cancel = cancel.clone();
                let title = self.set().title;
                egui::Window::new("モデルをダウンロード中")
                    .id(Id::new(("model-dl", self.set_id)))
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label(title);
                        ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(320.0));
                        if ui.button("中止").clicked() {
                            cancel.store(true, Ordering::Relaxed);
                        }
                    });
            }
            _ => {}
        }
        err
    }

    pub fn busy(&self) -> bool {
        matches!(self.state, State::Confirm | State::Downloading { .. })
    }

    pub fn status(&self) -> Option<&'static str> {
        match self.state {
            State::Downloading { .. } => Some("モデルをダウンロード中"),
            State::Loading(_) => Some("モデルを読み込み中"),
            _ => None,
        }
    }
}

/// OCR and formula engines shared by all tabs.
pub struct OcrManager {
    pub ocr: ModelSlot<dyn OcrEngine>,
    pub formula: ModelSlot<dyn FormulaEngine>,
}

impl OcrManager {
    pub fn new(device: Device) -> OcrManager {
        let ocr_loader: Loader<dyn OcrEngine> =
            Arc::new(|set, dev| strata_ocr::ndl::NdlOcr::load(set, dev).map(|e| Arc::new(e) as Arc<dyn OcrEngine>).map_err(|e| e.to_string()));
        let formula_loader: Loader<dyn FormulaEngine> =
            Arc::new(|set, _| strata_ocr::formula::Pix2TextMfr::load(set).map(|e| Arc::new(e) as Arc<dyn FormulaEngine>).map_err(|e| e.to_string()));
        OcrManager { ocr: ModelSlot::new("ndlocr-lite", device, ocr_loader), formula: ModelSlot::new("pix2text-mfr", Device::Cpu, formula_loader) }
    }

    pub fn engine(&mut self, ctx: &egui::Context) -> Option<Arc<dyn OcrEngine>> {
        self.ocr.engine(ctx)
    }

    pub fn set_device(&mut self, d: Device) {
        self.ocr.device = d;
        self.ocr.reset();
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Option<String> {
        let a = self.ocr.ui(ctx);
        let b = self.formula.ui(ctx);
        a.or(b)
    }

    pub fn status(&self) -> Option<&'static str> {
        self.ocr.status().or(self.formula.status())
    }

    /// A dialog of either slot is showing (native child windows must hide).
    pub fn dialog_open(&self) -> bool {
        self.ocr.busy() || self.formula.busy()
    }
}
