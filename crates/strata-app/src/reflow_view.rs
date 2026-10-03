//! Reflowed (Markdown/HTML) view of a document, shown in a WebView2 child window
//! laid over the tab's content area.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
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
    /// Change the font size: +1 larger, -1 smaller, 0 default.
    Font(i32),
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
    font: Option<f32>,
    /// Font families last sent to the page (JSON of CSS variables).
    fonts: Option<String>,
    /// 1-based page and page-space y of the first visible block.
    pub at_page: u32,
    pub at_y: f32,
    pending_goto: Option<(u32, f32)>,
    /// Side-by-side translation page: the nodes with a translation cell.
    bilingual: Option<Arc<HashSet<usize>>>,
    translations: HashMap<usize, String>,
    failed: HashSet<usize>,
    /// Translations not yet sent to the page.
    unsent: Vec<usize>,
    /// The current page has loaded (it reports `ready:`).
    page_ready: bool,
    /// Load this page into the webview.
    navigate: bool,
}

const JS_BRIDGE: &str = r#"
(() => {
  const send = m => window.ipc && window.ipc.postMessage(m);
  const vertical = () => document.body.classList.contains('vertical');
  // One screen forward (dir 1) or back (-1); vertical-rl text advances to the left.
  window.strataPage = dir => {
    if (vertical()) window.scrollBy({left: -dir * window.innerWidth * 0.9, behavior: 'smooth'});
    else window.scrollBy({top: dir * Math.max(window.innerHeight - 48, window.innerHeight * 0.5), behavior: 'smooth'});
  };
  document.addEventListener('keydown', e => {
    const k = (e.ctrlKey ? 'ctrl+' : '') + (e.shiftKey ? 'shift+' : '') + e.key.toLowerCase();
    if (['ctrl+w', 'ctrl+tab', 'ctrl+shift+tab', 'ctrl+o', 'ctrl+e', 'f11', 'ctrl+p', 'ctrl+f4'].includes(k)) { e.preventDefault(); send('key:' + k); return; }
    if (e.ctrlKey && !e.altKey) {
      const f = {'+': 1, '=': 1, ';': 1, '-': -1, '0': 0}[e.key];
      if (f !== undefined) { e.preventDefault(); send('font:' + f); }
      return;
    }
    // Horizontal text scrolls natively; vertical text needs the page keys mapped.
    if (vertical() && !e.altKey && [' ', 'PageDown', 'PageUp'].includes(e.key)) {
      e.preventDefault();
      strataPage(e.key === 'PageUp' || (e.key === ' ' && e.shiftKey) ? -1 : 1);
    }
  });
  let zoomAcc = 0;
  window.addEventListener('wheel', e => {
    if (!e.ctrlKey) return;
    e.preventDefault();
    zoomAcc += e.deltaY;
    if (Math.abs(zoomAcc) >= 50) { send('font:' + (zoomAcc < 0 ? 1 : -1)); zoomAcc = 0; }
  }, {passive: false});
  // First block whose leading edge is inside the view (right edge for vertical-rl).
  const firstVisible = () => {
    for (const el of document.querySelectorAll('[data-p]')) {
      const r = el.getBoundingClientRect();
      if (vertical() ? r.right <= window.innerWidth + 1 : r.bottom > 4) return el;
    }
    return null;
  };
  let scale = 1;
  window.strataSetFont = s => {
    if (s === scale) return;
    const keep = firstVisible();
    scale = s;
    document.body.style.fontSize = (17 * s) + 'px';
    if (keep) keep.scrollIntoView({block: 'start', inline: 'start'});
  };
  // Font families as CSS variables: {"--font-body": "...", ...}.
  window.strataSetFonts = v => {
    const keep = firstVisible();
    for (const k in v) document.documentElement.style.setProperty(k, v[k]);
    if (keep) keep.scrollIntoView({block: 'start', inline: 'start'});
  };
  let last = 0;
  const report = () => {
    const el = firstVisible();
    const pos = el && el.dataset.p + ':' + el.dataset.y;
    if (pos && pos !== last) { last = pos; send('pos:' + pos); }
  };
  let t = null;
  window.addEventListener('scroll', () => { if (!t) t = setTimeout(() => { t = null; report(); }, 150); }, {passive: true});
  // Fill translation cells: [[node, text, failed], ...].
  window.strataSetTr = list => {
    for (const [id, text, failed] of list) {
      const el = document.getElementById('tr-' + id);
      if (!el) continue;
      el.classList.remove('pending');
      if (failed) { el.classList.add('failed'); continue; }
      el.classList.remove('failed');
      const [name, cls] = (el.dataset.tag || 'p').split('.');
      const e = document.createElement(name);
      if (cls) e.className = cls;
      e.textContent = text;
      el.replaceChildren(e);
    }
  };
  send('ready:' + location.pathname);
})();
"#;

impl ReflowPane {
    pub fn start(doc: &Document, formula: Option<Arc<dyn strata_ocr::formula::FormulaEngine>>) -> ReflowPane {
        let cancel = Arc::new(AtomicBool::new(false));
        let opts = ReflowOptions { formula, ..ReflowOptions::default() };
        let rx = doc.reflow(opts, cancel.clone());
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
            font: None,
            fonts: None,
            at_page: 1,
            at_y: 0.0,
            pending_goto: None,
            bilingual: None,
            translations: HashMap::new(),
            failed: HashSet::new(),
            unsent: Vec::new(),
            page_ready: false,
            navigate: false,
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
        let html = output::to_html(d, &HtmlOptions { theme: Theme::Auto, page_markers: true, image_src: &src, extra_css: &crate::text_font::font_faces(), bilingual: None });
        let html = html.replace("</body>", &format!("<script>{JS_BRIDGE}</script></body>"));
        store.insert("/index.html".into(), ("text/html; charset=utf-8", Arc::new(html.into_bytes())));
    }

    fn page_url(&self) -> &'static str {
        if self.bilingual.is_some() { "http://strata.doc/bi.html" } else { "http://strata.doc/index.html" }
    }

    /// Show the side-by-side translation page with cells for `nodes`, or the plain page.
    pub fn set_bilingual(&mut self, nodes: Option<HashSet<usize>>) {
        if nodes.as_ref() == self.bilingual.as_deref() {
            return;
        }
        self.translations.clear();
        self.failed.clear();
        self.unsent.clear();
        if let (Some(n), Some(d)) = (&nodes, &self.doc) {
            let src = |im: &ReflowImage| format!("img/{}", im.id);
            let html = output::to_html(d, &HtmlOptions { theme: Theme::Auto, page_markers: true, image_src: &src, extra_css: &crate::text_font::font_faces(), bilingual: Some(n) });
            let html = html.replace("</body>", &format!("<script>{JS_BRIDGE}</script></body>"));
            self.store.write().insert("/bi.html".into(), ("text/html; charset=utf-8", Arc::new(html.into_bytes())));
        }
        self.bilingual = nodes.map(Arc::new);
        self.navigate = true;
        self.page_ready = false;
    }

    pub fn is_bilingual(&self) -> bool {
        self.bilingual.is_some()
    }

    pub fn add_translations(&mut self, v: Vec<(usize, String)>) {
        for (id, t) in v {
            self.failed.remove(&id);
            self.translations.insert(id, t);
            self.unsent.push(id);
        }
    }

    pub fn mark_failed(&mut self, ids: Vec<usize>) {
        for id in ids {
            self.failed.insert(id);
            self.unsent.push(id);
        }
    }

    /// Send pending translations to the loaded page.
    fn flush_translations(&mut self) {
        let Some(w) = &self.webview else { return };
        if !self.page_ready || self.bilingual.is_none() || self.unsent.is_empty() {
            return;
        }
        let list: Vec<serde_json::Value> = self
            .unsent
            .drain(..)
            .map(|id| match self.translations.get(&id) {
                Some(t) => serde_json::json!([id, t, false]),
                None => serde_json::json!([id, "", true]),
            })
            .collect();
        let _ = w.evaluate_script(&format!("window.strataSetTr && window.strataSetTr({})", serde_json::Value::Array(list)));
    }

    /// Scroll the reflowed view to a 1-based page and page-space y once it is ready.
    pub fn goto_pos(&mut self, page: u32, y: f32) {
        self.pending_goto = Some((page, y));
        self.at_page = page;
        self.at_y = y;
    }

    pub fn messages(&mut self) -> Vec<WebMsg> {
        let mut out = Vec::new();
        while let Ok(m) = self.msg_rx.try_recv() {
            if let Some(p) = m.strip_prefix("page:").and_then(|p| p.parse().ok()) {
                out.push(WebMsg::GotoPdfPage(p));
            } else if let Some((p, y)) = m.strip_prefix("pos:").and_then(|s| s.split_once(':')) {
                self.at_page = p.parse().unwrap_or(self.at_page);
                self.at_y = y.parse().unwrap_or(0.0);
            } else if let Some(u) = m.strip_prefix("open:") {
                out.push(WebMsg::OpenUrl(u.to_string()));
            } else if let Some(k) = m.strip_prefix("key:") {
                out.push(WebMsg::Key(k.to_string()));
            } else if let Some(d) = m.strip_prefix("font:").and_then(|d| d.parse().ok()) {
                out.push(WebMsg::Font(d));
            } else if let Some(path) = m.strip_prefix("ready:") {
                // A page finished loading: restore theme, font, position and translations.
                if self.page_url().ends_with(path) {
                    self.page_ready = true;
                    self.theme = None;
                    self.font = None;
                    self.fonts = None;
                    self.pending_goto = Some((self.at_page, self.at_y));
                    self.unsent = self.translations.keys().chain(self.failed.iter()).copied().collect();
                }
            }
        }
        out
    }

    fn build_webview(&mut self, egui_ctx: &egui::Context, window: &winit::window::Window, ctx: &mut wry::WebContext, rect: egui::Rect) -> Result<wry::WebView, String> {
        let store = self.store.clone();
        let tx = self.msg_tx.clone();
        let egui_ctx = egui_ctx.clone();
        wry::WebViewBuilder::new_with_web_context(ctx)
            .with_bounds(to_wry(rect))
            .with_custom_protocol("strata".into(), move |_id, req| {
                let path = req.uri().path().to_string();
                if let Some(data) = crate::text_font::bundled_file(&path) {
                    return wry::http::Response::builder().header("Content-Type", "font/ttf").body(Cow::Owned(data)).unwrap();
                }
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
                // Handle it now, not on whatever input next wakes the UI.
                egui_ctx.request_repaint();
            })
            .with_navigation_handler(|url| url.starts_with("http://strata.") || url.starts_with("about:"))
            .with_devtools(cfg!(debug_assertions))
            .with_url(self.page_url())
            .build_as_child(window)
            .map_err(|e| e.to_string())
    }

    /// Scroll one screen forward (`dir` > 0) or back.
    pub fn page(&self, dir: i32) {
        if let Some(w) = &self.webview {
            let _ = w.evaluate_script(&format!("window.strataPage && window.strataPage({dir})"));
        }
    }

    /// Draw progress in egui, or place the webview over `rect` (and give it the
    /// keyboard when it appears in the focused tab).
    #[allow(clippy::too_many_arguments)]
    pub fn ui(&mut self, ui: &mut egui::Ui, rect: egui::Rect, window: Option<&winit::window::Window>, ctx: Option<&mut wry::WebContext>, theme: Theme, font: f32, fonts: &crate::text_font::TextFonts, focused: bool) {
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
            match self.build_webview(ui.ctx(), window, ctx, rect) {
                Ok(w) => {
                    // It opens on the current page already.
                    self.navigate = false;
                    if focused {
                        let _ = w.focus();
                    }
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
        if std::mem::take(&mut self.navigate)
            && let Some(w) = &self.webview
        {
            let _ = w.load_url(self.page_url());
        }
        self.flush_translations();
        let Some(w) = &self.webview else { return };
        if self.bounds != Some(rect) {
            let _ = w.set_bounds(to_wry(rect));
            self.bounds = Some(rect);
        }
        if !self.visible {
            let _ = w.set_visible(true);
            if focused {
                let _ = w.focus();
            }
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
        if self.font != Some(font) {
            // Before the position below, which it would otherwise move.
            let _ = w.evaluate_script(&format!(
                "(function g(n){{ if (window.strataSetFont) window.strataSetFont({font}); else if (n < 50) setTimeout(() => g(n + 1), 100); }})(0)"
            ));
            self.font = Some(font);
        }
        let fonts = fonts.json();
        if self.fonts.as_ref() != Some(&fonts) {
            let _ = w.evaluate_script(&format!(
                "(function g(n){{ if (window.strataSetFonts) window.strataSetFonts({fonts}); else if (n < 50) setTimeout(() => g(n + 1), 100); }})(0)"
            ));
            self.fonts = Some(fonts);
        }
        if let Some((p, y)) = self.pending_goto.take() {
            // The page may still be loading; the script defines strataGotoPos at the end.
            let _ = w.evaluate_script(&format!(
                "(function g(n){{ if (window.strataGotoPos) window.strataGotoPos({p}, {y}); else if (n < 50) setTimeout(() => g(n + 1), 100); }})(0)"
            ));
        }
    }

    pub fn hide(&mut self) {
        if self.visible
            && let Some(w) = &self.webview
        {
            let _ = w.focus_parent();
            let _ = w.set_visible(false);
        }
        self.visible = false;
    }

    pub fn export_markdown(&self, path: &Path) -> std::io::Result<()> {
        let Some(d) = &self.doc else { return Ok(()) };
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
        let dir_name = format!("{stem}_files");
        let dir = path.with_file_name(&dir_name);
        let used = d.used_images();
        if used.iter().any(|u| *u) {
            std::fs::create_dir_all(&dir)?;
            for (im, _) in d.images.iter().zip(&used).filter(|(_, u)| **u) {
                std::fs::write(dir.join(&im.id), &im.png)?;
            }
        }
        let md = output::to_markdown(d, &|im| output::image_link(&dir_name, &im.id));
        std::fs::write(path, md)
    }

    pub fn export_html(&self, path: &Path, theme: Theme, fonts: &crate::text_font::TextFonts) -> std::io::Result<()> {
        let Some(d) = &self.doc else { return Ok(()) };
        let src = |im: &ReflowImage| format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&im.png));
        let html = output::to_html(d, &HtmlOptions { theme, page_markers: false, image_src: &src, extra_css: &fonts.css_rule(), bilingual: None });
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
