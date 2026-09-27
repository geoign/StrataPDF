//! Application-wide OCR engine: model download on first use, background loading.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::{Receiver, unbounded};
use egui::{Id, Ui};
use strata_core::ocr::{Device, OcrEngine, models};

enum State {
    Idle,
    /// Asking the user to confirm the download.
    Confirm,
    Downloading { done: Arc<AtomicU64>, total: u64, cancel: Arc<AtomicBool>, rx: Receiver<Result<(), String>> },
    Loading(Receiver<Result<Arc<dyn OcrEngine>, String>>),
    Ready(Arc<dyn OcrEngine>),
    Failed(String),
}

pub struct OcrManager {
    state: State,
    pub device: Device,
    /// Views waiting for the engine (they start their job once it is ready).
    pub waiting: bool,
}

impl OcrManager {
    pub fn new(device: Device) -> OcrManager {
        OcrManager { state: State::Idle, device, waiting: false }
    }

    fn set() -> models::ModelSet {
        models::set("ndlocr-lite").expect("built-in model set")
    }

    /// The engine if ready; otherwise starts download/loading as needed.
    pub fn engine(&mut self, ctx: &egui::Context) -> Option<Arc<dyn OcrEngine>> {
        self.poll(ctx);
        match &self.state {
            State::Ready(e) => Some(e.clone()),
            State::Idle | State::Failed(_) => {
                if Self::set().is_installed() {
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

    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    /// Drop the loaded engine (e.g. after changing the device).
    pub fn reset(&mut self) {
        if matches!(self.state, State::Ready(_) | State::Failed(_)) {
            self.state = State::Idle;
        }
    }

    fn start_loading(&mut self, ctx: &egui::Context) {
        let (tx, rx) = unbounded();
        let device = self.device;
        let c = ctx.clone();
        std::thread::Builder::new()
            .name("strata-ocr-load".into())
            .spawn(move || {
                let r = strata_ocr::ndl::NdlOcr::load(&Self::set(), device).map(|e| Arc::new(e) as Arc<dyn OcrEngine>).map_err(|e| e.to_string());
                let _ = tx.send(r);
                c.request_repaint();
            })
            .ok();
        self.state = State::Loading(rx);
    }

    fn start_download(&mut self, ctx: &egui::Context) {
        let set = Self::set();
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

    fn poll(&mut self, ctx: &egui::Context) {
        let next = match &self.state {
            State::Downloading { rx, .. } => match rx.try_recv() {
                Ok(Ok(())) => Some(None),
                Ok(Err(e)) => Some(Some(State::Failed(format!("モデルのダウンロードに失敗しました: {e}")))),
                Err(_) => None,
            },
            State::Loading(rx) => match rx.try_recv() {
                Ok(Ok(e)) => Some(Some(State::Ready(e))),
                Ok(Err(e)) => Some(Some(State::Failed(format!("OCR エンジンを読み込めません: {e}")))),
                Err(_) => None,
            },
            _ => None,
        };
        match next {
            Some(Some(s)) => self.state = s,
            Some(None) => self.start_loading(ctx),
            None => {}
        }
    }

    /// Dialogs and progress; returns an error message to surface, if any.
    pub fn ui(&mut self, ctx: &egui::Context) -> Option<String> {
        self.poll(ctx);
        let mut err = None;
        match &self.state {
            State::Confirm => {
                let set = Self::set();
                let mut decision = None;
                egui::Modal::new(Id::new("ocr-download")).show(ctx, |ui: &mut Ui| {
                    ui.set_width(460.0);
                    ui.heading("OCR モデルのダウンロード");
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
                    }
                    None => {}
                }
            }
            State::Downloading { done, total, cancel, .. } => {
                let frac = done.load(Ordering::Relaxed) as f32 / (*total).max(1) as f32;
                let cancel = cancel.clone();
                egui::Window::new("OCR モデルをダウンロード中").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                    ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(320.0));
                    if ui.button("中止").clicked() {
                        cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            State::Failed(e) => {
                err = Some(e.clone());
                self.state = State::Idle;
                self.waiting = false;
            }
            _ => {}
        }
        err
    }

    pub fn status(&self) -> Option<&'static str> {
        match self.state {
            State::Downloading { .. } => Some("OCR モデルをダウンロード中"),
            State::Loading(_) => Some("OCR エンジンを読み込み中"),
            _ => None,
        }
    }
}
