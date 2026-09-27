//! Reflowed (Markdown/HTML) view of a document, shown in a WebView2 child window
//! laid over the tab's content area.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::RwLock;
use strata_core::Document;
use strata_core::reflow::output::{self, HtmlOptions, Theme};
use strata_core::reflow::{ReflowDoc, ReflowEvent, ReflowImage, ReflowOptions};

type Store = Arc<RwLock<HashMap<String, (&'static str, Arc<Vec<u8>>)>>>;

/// Messages from the page script.
pub enum WebMsg {
    /// Show this 1-based page in the PDF view.
    GotoPdfPage(u32),
    OpenUrl(String),
    Key(String),
}

pub struct ReflowPane {
    rx: Option<Receiver<ReflowEvent>>,
    cancel: Arc<AtomicBool>,
    progress: (usize, usize),
    pub doc: Option<Arc<ReflowDoc>>,
    error: Option<String>,
    webview: Option<wry::WebView>,
    store: Store,
    msg_tx: Sender<String>,
    msg_rx: Receiver<String>,
    bounds: Option<egui::Rect>,
    visible: bool,
    theme: Option<Theme>,
    /// 1-based page the reflowed view currently shows.
    pub at_page: u32,
    pending_goto: Option<u32>,
}

const JS_BRIDGE: &str = r#"
(() => {
  const send = m => window.ipc && window.ipc.postMessage(m);
  document.addEventListener('keydown', e => {
    const k = (e.ctrlKey ? 'ctrl+' : '') + (e.shiftKey ? 'shift+' : '') + e.key.toLowerCase();
    if (['ctrl+w', 'ctrl+tab', 'ctrl+shift+tab', 'ctrl+o', 'ctrl+e', 'f11', 'ctrl+p', 'ctrl+f4'].includes(k)) { e.preventDefault(); send('key:' + k); }
  });
  let last = 0;
  const report = () => {
    const vertical = document.body.classList.contains('vertical');
    let cur = 1;
    // Markers already scrolled past: above the top (horizontal) or right of the view (vertical-rl).
    for (const pm of document.querySelectorAll('.pm, .pm-anchor')) {
      const r = pm.getBoundingClientRect();
      if (vertical ? r.left > window.innerWidth - 80 : r.top < 80) cur = +pm.id.slice(5); else break;
    }
    if (cur !== last) { last = cur; send('at:' + cur); }
  };
  let t = null;
  window.addEventListener('scroll', () => { if (!t) t = setTimeout(() => { t = null; report(); }, 150); }, {passive: true});
})();
"#;

impl ReflowPane {
    pub fn start(doc: &Document) -> ReflowPane {
        let cancel = Arc::new(AtomicBool::new(false));
        let rx = doc.reflow(ReflowOptions::default(), cancel.clone());
        let (msg_tx, msg_rx) = unbounded();
        ReflowPane {
            rx: Some(rx),
            cancel,
            progress: (0, 0),
            doc: None,
            error: None,
            webview: None,
            store: Arc::new(RwLock::new(HashMap::new())),
            msg_tx,
            msg_rx,
            bounds: None,
            visible: false,
            theme: None,
            at_page: 1,
            pending_goto: None,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.doc.is_some()
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut done = None;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ReflowEvent::Progress { done, total } => self.progress = (done, total),
                ReflowEvent::Done(d) => done = Some(Ok(d)),
                ReflowEvent::Error(e) => done = Some(Err(e)),
            }
        }
        match done {
            Some(Ok(d)) => {
                self.rx = None;
                self.publish(&d);
                self.doc = Some(d);
            }
            Some(Err(e)) => {
                self.rx = None;
                self.error = Some(e);
            }
            None => {}
        }
    }

    fn publish(&mut self, d: &ReflowDoc) {
        let mut store = self.store.write();
        store.clear();
        for im in &d.images {
            store.insert(format!("/img/{}", im.id), ("image/png", Arc::new(im.png.clone())));
        }
        let src = |im: &ReflowImage| format!("img/{}", im.id);
        let html = output::to_html(d, &HtmlOptions { theme: Theme::Auto, page_markers: true, image_src: &src, extra_css: "" });
        let html = html.replace("</body>", &format!("<script>{JS_BRIDGE}</script></body>"));
        store.insert("/index.html".into(), ("text/html; charset=utf-8", Arc::new(html.into_bytes())));
    }

    /// Scroll the reflowed view to a 1-based page once it is ready.
    pub fn goto_page(&mut self, page: u32) {
        self.pending_goto = Some(page);
    }

    pub fn messages(&mut self) -> Vec<WebMsg> {
        let mut out = Vec::new();
        while let Ok(m) = self.msg_rx.try_recv() {
            if let Some(p) = m.strip_prefix("page:").and_then(|p| p.parse().ok()) {
                out.push(WebMsg::GotoPdfPage(p));
            } else if let Some(p) = m.strip_prefix("at:").and_then(|p| p.parse().ok()) {
                self.at_page = p;
            } else if let Some(u) = m.strip_prefix("open:") {
                out.push(WebMsg::OpenUrl(u.to_string()));
            } else if let Some(k) = m.strip_prefix("key:") {
                out.push(WebMsg::Key(k.to_string()));
            }
        }
        out
    }

    fn build_webview(&mut self, window: &winit::window::Window, ctx: &mut wry::WebContext, rect: egui::Rect) -> Result<wry::WebView, String> {
        let store = self.store.clone();
        let tx = self.msg_tx.clone();
        wry::WebViewBuilder::new_with_web_context(ctx)
            .with_bounds(to_wry(rect))
            .with_custom_protocol("strata".into(), move |_id, req| {
                let path = req.uri().path().to_string();
                let store = store.read();
                match store.get(&path) {
                    Some((mime, data)) => wry::http::Response::builder()
                        .header("Content-Type", *mime)
                        .body(Cow::Owned(data.as_ref().clone()))
                        .unwrap(),
                    None => wry::http::Response::builder().status(404).body(Cow::Borrowed(&b"not found"[..])).unwrap(),
                }
            })
            .with_ipc_handler(move |req| {
                let _ = tx.send(req.body().clone());
            })
            .with_navigation_handler(|url| url.starts_with("http://strata.") || url.starts_with("about:"))
            .with_devtools(cfg!(debug_assertions))
            .with_url("http://strata.doc/index.html")
            .build_as_child(window)
            .map_err(|e| e.to_string())
    }

    /// Draw progress in egui, or place the webview over `rect`.
    pub fn ui(&mut self, ui: &mut egui::Ui, rect: egui::Rect, window: Option<&winit::window::Window>, ctx: Option<&mut wry::WebContext>, theme: Theme) {
        self.poll();
        if let Some(e) = &self.error {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, format!("テキスト化できませんでした\n{e}"), egui::FontId::proportional(15.0), egui::Color32::RED);
            self.hide();
            return;
        }
        if self.doc.is_none() {
            let (d, t) = self.progress;
            let frac = if t > 0 { d as f32 / t as f32 } else { 0.0 };
            let area = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width().min(360.0), 60.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
                ui.label("テキストを組み立てています…");
                ui.add(egui::ProgressBar::new(frac).show_percentage());
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        if self.webview.is_none() {
            let (Some(window), Some(ctx)) = (window, ctx) else { return };
            match self.build_webview(window, ctx, rect) {
                Ok(w) => {
                    self.webview = Some(w);
                    self.bounds = Some(rect);
                    self.visible = true;
                }
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            }
        }
        let Some(w) = &self.webview else { return };
        if self.bounds != Some(rect) {
            let _ = w.set_bounds(to_wry(rect));
            self.bounds = Some(rect);
        }
        if !self.visible {
            let _ = w.set_visible(true);
            self.visible = true;
        }
        if self.theme != Some(theme) {
            let t = match theme {
                Theme::Auto => "",
                Theme::Light => "light",
                Theme::Dark => "dark",
            };
            let _ = w.evaluate_script(&format!("window.strataSetTheme && window.strataSetTheme('{t}')"));
            self.theme = Some(theme);
        }
        if let Some(p) = self.pending_goto.take() {
            // The page may still be loading; the script defines strataGotoPage at the end.
            let _ = w.evaluate_script(&format!(
                "(function g(n){{ if (window.strataGotoPage) window.strataGotoPage({p}); else if (n < 50) setTimeout(() => g(n + 1), 100); }})(0)"
            ));
        }
    }

    pub fn hide(&mut self) {
        if self.visible
            && let Some(w) = &self.webview
        {
            let _ = w.set_visible(false);
            let _ = w.focus_parent();
        }
        self.visible = false;
    }

    pub fn export_markdown(&self, path: &Path) -> std::io::Result<()> {
        let Some(d) = &self.doc else { return Ok(()) };
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
        let dir_name = format!("{stem}_files");
        let dir = path.with_file_name(&dir_name);
        if !d.images.is_empty() {
            std::fs::create_dir_all(&dir)?;
            for im in &d.images {
                std::fs::write(dir.join(&im.id), &im.png)?;
            }
        }
        let md = output::to_markdown(d, &|im| format!("{dir_name}/{}", im.id).replace(' ', "%20"));
        std::fs::write(path, md)
    }

    pub fn export_html(&self, path: &Path, theme: Theme) -> std::io::Result<()> {
        let Some(d) = &self.doc else { return Ok(()) };
        let src = |im: &ReflowImage| format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&im.png));
        let html = output::to_html(d, &HtmlOptions { theme, page_markers: false, image_src: &src, extra_css: "" });
        std::fs::write(path, html)
    }
}

impl Drop for ReflowPane {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn to_wry(r: egui::Rect) -> wry::Rect {
    wry::Rect {
        position: wry::dpi::LogicalPosition::new(r.min.x as f64, r.min.y as f64).into(),
        size: wry::dpi::LogicalSize::new(r.width().max(1.0) as f64, r.height().max(1.0) as f64).into(),
    }
}
