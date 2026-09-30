//! One document view (a tab): scrolling, zoom, spreads, tile requests, text
//! selection, links, search, outline and thumbnails.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::Receiver;
use egui::{
    Align, Color32, CursorIcon, Id, Key, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, TextEdit, Ui, Vec2, pos2, vec2,
};
use serde::{Deserialize, Serialize};
use strata_core::render::{TILE_PX, level_for_scale, level_scale, tile_grid};
use strata_core::images::PageImage;
use strata_core::text::CharPos;
use strata_core::{
    DocId, Document, LinkInfo, LinkTarget, OutlineItem, PageText, Pending, QuadF, RenderPool, SearchEvent, SizeF, TileKey, ViewId,
};

use crate::layout::{Layout, LayoutParams, Spread};
use crate::reflow_view::{ReflowPane, WebMsg};

mod annot_ui;
mod table_ui;
mod tr_ui;
use crate::tiles::TileCache;

static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);

const GAP: f32 = 10.0;
const MARGIN: f32 = 12.0;
const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;
const CHAR_POS_END: CharPos = CharPos { block: u32::MAX, line: 0, ch: 0 };
const CHAR_POS_START: CharPos = CharPos { block: 0, line: 0, ch: 0 };

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Fit {
    #[default]
    Width,
    Page,
    Free,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Sidebar {
    #[default]
    None,
    Outline,
    Thumbs,
}

/// Defaults applied to newly opened views.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewPrefs {
    pub fit: Fit,
    pub spread: Spread,
    pub paged: bool,
    pub sidebar: Sidebar,
}

impl Default for ViewPrefs {
    fn default() -> Self {
        ViewPrefs { fit: Fit::Width, spread: Spread::Single, paged: false, sidebar: Sidebar::None }
    }
}

/// Everything a view needs from the application for one frame.
pub struct Services<'a> {
    pub pool: &'a RenderPool,
    pub tiles: &'a mut TileCache,
    pub focused: bool,
    pub window: Option<&'a winit::window::Window>,
    pub web: Option<&'a mut wry::WebContext>,
    pub theme: strata_core::reflow::output::Theme,
    /// A popup or dialog covers the UI: native child windows must hide.
    pub overlay: bool,
    pub ocr: &'a mut crate::ocr_ui::OcrManager,
    /// Convert display formulas to LaTeX in the text view.
    pub latex: bool,
    /// Font size of the text view (shared by all tabs, saved in the settings).
    pub text_scale: &'a mut f32,
    pub translate: &'a mut crate::translate_ui::TranslateManager,
}

struct OcrJob {
    rx: Receiver<strata_core::ocr::OcrEvent>,
    cancel: Arc<AtomicBool>,
    done: usize,
    total: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Pdf,
    Reflow,
}

/// Requests a view makes to the application (e.g. shortcuts typed in the webview).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewRequest {
    CloseTab,
    NextTab,
    PrevTab,
    Open,
    Fullscreen,
    Print,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DocPos {
    page: u32,
    pos: CharPos,
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor: DocPos,
    head: DocPos,
}

impl Selection {
    fn ordered(&self) -> (DocPos, DocPos) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    None,
    Pan,
    Select,
}

#[derive(Default)]
struct SearchState {
    open: bool,
    focus: bool,
    query: String,
    ran_query: String,
    cancel: Option<Arc<AtomicBool>>,
    rx: Option<Receiver<SearchEvent>>,
    hits: BTreeMap<u32, Vec<QuadF>>,
    total: usize,
    progress: (usize, usize),
    done: bool,
    current: Option<(u32, usize)>,
    /// Jump to the next hit once one arrives (set when a new search starts).
    pending_jump: Option<bool>,
    error: Option<String>,
}

impl SearchState {
    fn stop(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        self.rx = None;
    }
}

pub struct DocView {
    pub id: ViewId,
    pub doc: Arc<Document>,
    pub title: String,
    sizes: Vec<SizeF>,
    sizes_rev: u64,
    layout: Layout,
    layout_params: Option<LayoutParams>,
    pub zoom: f32,
    pub fit: Fit,
    pub spread: Spread,
    pub r2l: bool,
    pub paged: bool,
    pub sidebar: Sidebar,
    /// Content-space offset of the viewport's top-left corner.
    scroll: Vec2,
    cur_row: usize,
    wheel_acc: f32,
    viewport: Rect,
    texts: HashMap<u32, Pending<Arc<PageText>>>,
    links: HashMap<u32, Pending<Arc<Vec<LinkInfo>>>>,
    images: HashMap<u32, Pending<Arc<Vec<PageImage>>>>,
    /// Page and page-space point of the last right click (for the context menu).
    menu_at: Option<(u32, f32, f32)>,
    image_save: Option<(Pending<(u32, u32)>, std::path::PathBuf)>,
    outline: Option<Pending<Arc<Vec<OutlineItem>>>>,
    sel: Option<Selection>,
    drag: DragMode,
    pending_copy: bool,
    search: SearchState,
    page_input: String,
    page_input_focus: bool,
    thumbs_follow: bool,
    last_page: u32,
    /// Navigation requested before the first layout (e.g. restored position).
    pending_goto: Option<(u32, Option<f32>)>,
    pub status: String,
    pub mode: ViewMode,
    reflow: Option<ReflowPane>,
    pub requests: Vec<ViewRequest>,
    ocr_job: Option<OcrJob>,
    /// OCR asked for; starts once the engine is loaded.
    ocr_request: Option<strata_core::ocr::OcrScope>,
    /// The current reflow was built with the formula engine.
    reflow_has_latex: bool,
    save_job: Option<Receiver<Result<(usize, std::path::PathBuf), String>>>,
    annot: annot_ui::AnnotState,
    table: table_ui::TableState,
    tr: tr_ui::TrState,
}

impl DocView {
    pub fn new(doc: Arc<Document>, prefs: &ViewPrefs) -> DocView {
        let info = doc.info().clone();
        let title = info.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "(無題)".into());
        let mut spread = prefs.spread;
        match info.page_layout.as_deref() {
            Some("TwoPageRight" | "TwoColumnRight") => spread = Spread::DoubleCover,
            Some("TwoPageLeft" | "TwoColumnLeft") => spread = Spread::Double,
            _ => {}
        }
        DocView {
            id: NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed),
            sizes: doc.page_sizes(),
            sizes_rev: doc.revision(),
            doc,
            title,
            layout: Layout::default(),
            layout_params: None,
            zoom: 1.0,
            fit: prefs.fit,
            spread,
            r2l: info.right_to_left,
            paged: prefs.paged,
            sidebar: prefs.sidebar,
            scroll: Vec2::ZERO,
            cur_row: 0,
            wheel_acc: 0.0,
            viewport: Rect::NOTHING,
            texts: HashMap::new(),
            links: HashMap::new(),
            images: HashMap::new(),
            menu_at: None,
            image_save: None,
            outline: None,
            sel: None,
            drag: DragMode::None,
            pending_copy: false,
            search: SearchState::default(),
            page_input: String::new(),
            page_input_focus: false,
            thumbs_follow: true,
            last_page: 0,
            pending_goto: None,
            status: String::new(),
            mode: ViewMode::Pdf,
            reflow: None,
            requests: Vec::new(),
            ocr_job: None,
            ocr_request: None,
            reflow_has_latex: false,
            save_job: None,
            annot: annot_ui::AnnotState::default(),
            table: table_ui::TableState::default(),
            tr: tr_ui::TrState::default(),
        }
    }

    /// A second view on the same document (for side-by-side reading).
    pub fn duplicate(&self) -> DocView {
        let prefs = ViewPrefs { fit: self.fit, spread: self.spread, paged: self.paged, sidebar: Sidebar::None };
        let mut v = DocView::new(self.doc.clone(), &prefs);
        v.r2l = self.r2l;
        v.zoom = self.zoom;
        v.pending_goto = Some((self.current_page(), None));
        v
    }

    pub fn doc_id(&self) -> DocId {
        self.doc.id()
    }

    pub fn page_count(&self) -> usize {
        self.sizes.len()
    }

    pub fn current_page(&self) -> u32 {
        if self.layout.rows.is_empty() {
            return 0;
        }
        let row = if self.paged {
            self.cur_row.min(self.layout.rows.len() - 1)
        } else {
            self.layout.row_at(self.scroll.y + self.viewport.height().max(1.0) * 0.3)
        };
        self.layout.rows[row].pages.first().copied().unwrap_or(0)
    }

    // ------------------------------------------------------------------ layout

    fn params(&self, viewport_w: f32) -> LayoutParams {
        // Fitting sizes one spread to the view, so only free zoom can show several.
        LayoutParams { zoom: self.zoom, spread: self.spread, r2l: self.r2l, viewport_w, gap: GAP, margin: MARGIN, multi: self.fit == Fit::Free }
    }

    fn ensure_layout(&mut self, viewport_w: f32) {
        let rev = self.doc.revision();
        if rev != self.sizes_rev {
            self.sizes = self.doc.page_sizes();
            self.sizes_rev = rev;
            self.layout_params = None;
        }
        let p = self.params(viewport_w);
        if self.layout_params != Some(p) {
            // Keep the page under the viewport's top edge in place across re-layouts.
            let anchor = self.anchor_at(self.viewport.min + vec2(self.viewport.width() * 0.5, 0.0));
            self.layout = Layout::build(&self.sizes, &p);
            self.layout_params = Some(p);
            if let Some(a) = anchor {
                self.restore_anchor(a, self.viewport.min + vec2(self.viewport.width() * 0.5, 0.0));
            }
        }
    }

    /// (page, point in page space) under a screen position.
    fn anchor_at(&self, screen: Pos2) -> Option<(u32, Vec2)> {
        if self.layout.page_rects.is_empty() || !self.viewport.is_positive() {
            return None;
        }
        let c = self.to_content(screen);
        let p = self.layout.page_at(c)?;
        let r = self.layout.page_rects[p as usize];
        Some((p, (c - r.min) / self.zoom))
    }

    fn restore_anchor(&mut self, (page, off): (u32, Vec2), screen: Pos2) {
        let Some(r) = self.layout.page_rects.get(page as usize) else { return };
        let content = r.min + off * self.zoom;
        self.scroll = (content - (screen - self.viewport.min)).to_vec2();
        if self.paged {
            self.cur_row = self.layout.page_row[page as usize] as usize;
        }
    }

    fn to_content(&self, screen: Pos2) -> Pos2 {
        screen - self.viewport.min.to_vec2() + self.scroll
    }

    fn to_screen(&self, content: Pos2) -> Pos2 {
        content + self.viewport.min.to_vec2() - self.scroll
    }

    fn page_screen_rect(&self, page: u32) -> Rect {
        let r = self.layout.page_rects[page as usize];
        Rect::from_min_max(self.to_screen(r.min), self.to_screen(r.max))
    }

    /// Screen position of a point in page space.
    fn page_to_screen(&self, page: u32, p: [f32; 2]) -> Pos2 {
        let r = self.page_screen_rect(page);
        r.min + vec2(p[0], p[1]) * self.zoom
    }

    fn screen_to_page(&self, page: u32, s: Pos2) -> (f32, f32) {
        let r = self.page_screen_rect(page);
        let v = (s - r.min) / self.zoom;
        (v.x, v.y)
    }

    /// Pages used for fit-to-width/page: the current spread.
    fn fit_row_size(&self) -> Option<(f32, f32)> {
        if self.sizes.is_empty() {
            return None;
        }
        // The spread, not the layout row: a zoomed-out row holds several spreads.
        let row = crate::layout::group_of(self.current_page(), self.sizes.len(), self.spread);
        let w: f32 = row.iter().map(|&p| self.sizes[p as usize].w).sum::<f32>();
        let h = row.iter().map(|&p| self.sizes[p as usize].h).fold(0.0, f32::max);
        Some((w, h))
    }

    fn apply_fit(&mut self, vp: Vec2) {
        if self.fit == Fit::Free {
            return;
        }
        let Some((w, h)) = self.fit_row_size() else { return };
        let n_gaps = if self.spread == Spread::Single { 0.0 } else { 1.0 };
        let avail_w = (vp.x - 2.0 * MARGIN - GAP * n_gaps).max(50.0);
        let avail_h = (vp.y - 2.0 * MARGIN).max(50.0);
        let z = match self.fit {
            Fit::Width => avail_w / w,
            Fit::Page => (avail_w / w).min(avail_h / h),
            Fit::Free => self.zoom,
        };
        let z = z.clamp(MIN_ZOOM, MAX_ZOOM);
        if (z - self.zoom).abs() > 1e-4 {
            self.zoom = z;
        }
    }

    pub fn set_zoom(&mut self, z: f32, focus: Option<Pos2>) {
        let z = z.clamp(MIN_ZOOM, MAX_ZOOM);
        let focus = focus.unwrap_or(self.viewport.center());
        let anchor = self.anchor_at(focus);
        self.fit = Fit::Free;
        self.zoom = z;
        self.ensure_layout(self.viewport.width());
        if let Some(a) = anchor {
            self.restore_anchor(a, focus);
        }
    }

    pub fn zoom_step(&mut self, dir: f32, focus: Option<Pos2>) {
        const STEPS: [f32; 17] = [0.1, 0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0, 16.0];
        let z = self.zoom;
        let next = if dir > 0.0 { STEPS.iter().copied().find(|&s| s > z * 1.01) } else { STEPS.iter().rev().copied().find(|&s| s < z * 0.99) };
        if let Some(n) = next {
            self.set_zoom(n, focus);
        }
    }

    pub fn set_fit(&mut self, fit: Fit) {
        self.fit = fit;
        self.layout_params = None;
    }

    pub fn set_spread(&mut self, s: Spread) {
        self.spread = s;
        self.layout_params = None;
    }

    // -------------------------------------------------------------- navigation

    pub fn goto_page(&mut self, page: u32, y: Option<f32>) {
        if self.layout.rows.is_empty() {
            self.pending_goto = Some((page, y));
            return;
        }
        let page = page.min(self.sizes.len().saturating_sub(1) as u32);
        let row = self.layout.page_row[page as usize] as usize;
        if self.paged {
            self.cur_row = row;
            self.scroll.y = self.layout.rows[row].y0 - MARGIN;
        } else {
            match y {
                Some(y) => {
                    let r = self.layout.page_rects[page as usize];
                    self.scroll.y = r.min.y + y * self.zoom - MARGIN;
                }
                None => self.scroll.y = self.layout.rows[row].y0 - GAP * 0.5,
            }
        }
        let pr = self.layout.page_rects[page as usize];
        if pr.width() <= self.viewport.width() || self.fit != Fit::Free {
            let row = &self.layout.rows[row];
            let x0 = row.pages.iter().map(|&p| self.layout.page_rects[p as usize].min.x).fold(f32::INFINITY, f32::min);
            let x1 = row.pages.iter().map(|&p| self.layout.page_rects[p as usize].max.x).fold(f32::NEG_INFINITY, f32::max);
            self.scroll.x = (x0 + x1) * 0.5 - self.viewport.width() * 0.5;
        }
        self.thumbs_follow = true;
    }

    fn goto_row(&mut self, row: usize) {
        if let Some(r) = self.layout.rows.get(row) {
            let p = r.pages[0];
            self.goto_page(p, None);
        }
    }

    fn step_rows(&mut self, d: i64) {
        let cur = if self.paged { self.cur_row } else { self.layout.row_at(self.scroll.y + self.viewport.height() * 0.3) };
        let n = self.layout.rows.len() as i64;
        let target = (cur as i64 + d).clamp(0, (n - 1).max(0)) as usize;
        self.goto_row(target);
    }

    fn follow_link(&mut self, ctx: &egui::Context, t: &LinkTarget) {
        match t {
            LinkTarget::Page { page, y } => self.goto_page(*page, *y),
            LinkTarget::Uri(u) => ctx.open_url(egui::OpenUrl::new_tab(u)),
        }
    }

    // --------------------------------------------------------------- text/links

    fn text(&mut self, page: u32) -> Option<Arc<PageText>> {
        let doc = &self.doc;
        let p = self.texts.entry(page).or_insert_with(|| doc.text(page));
        match p.poll() {
            Some(Ok(t)) => Some(t.clone()),
            _ => None,
        }
    }

    fn page_links(&mut self, page: u32) -> Option<Arc<Vec<LinkInfo>>> {
        let doc = &self.doc;
        let p = self.links.entry(page).or_insert_with(|| doc.links(page));
        match p.poll() {
            Some(Ok(l)) => Some(l.clone()),
            _ => None,
        }
    }

    fn page_images(&mut self, page: u32) -> Option<Arc<Vec<PageImage>>> {
        let doc = &self.doc;
        let p = self.images.entry(page).or_insert_with(|| doc.images(page));
        match p.poll() {
            Some(Ok(l)) => Some(l.clone()),
            _ => None,
        }
    }

    /// Ask where to save an embedded image, then save it on the document thread.
    fn save_image_as(&mut self, page: u32, index: usize, im: &PageImage) {
        let stem = self.doc.info().path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let name = format!("{stem}_p{}_{}", page + 1, index + 1);
        let dialog = if im.jpeg {
            rfd::FileDialog::new().add_filter("JPEG（埋め込みデータのまま）", &["jpg", "jpeg"]).add_filter("PNG", &["png"]).set_file_name(format!("{name}.jpg"))
        } else {
            rfd::FileDialog::new().add_filter("PNG", &["png"]).set_file_name(format!("{name}.png"))
        };
        let Some(mut path) = dialog.save_file() else { return };
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let as_jpeg = im.jpeg && (ext == "jpg" || ext == "jpeg");
        if !as_jpeg && ext != "png" {
            path.set_extension("png");
        }
        self.image_save = Some((self.doc.save_image(page, index, as_jpeg, path.clone()), path));
        self.status = "画像を保存しています…".into();
    }

    fn poll_image_save(&mut self) {
        let Some((pending, path)) = &mut self.image_save else { return };
        let Some(r) = pending.poll() else { return };
        self.status = match r {
            Ok((w, h)) => format!("画像（{w}×{h}）を保存しました: {}", path.display()),
            Err(e) => format!("画像を保存できませんでした: {e}"),
        };
        self.image_save = None;
    }

    fn trim_caches(&mut self, center: u32) {
        let keep = |p: &u32| p.abs_diff(center) <= 40;
        if self.texts.len() > 200 && self.sel.is_none() && !self.pending_copy {
            self.texts.retain(|p, _| keep(p));
        }
        if self.links.len() > 200 {
            self.links.retain(|p, _| keep(p));
        }
        if self.images.len() > 100 {
            self.images.retain(|p, _| keep(p));
        }
    }

    fn hit_text(&mut self, screen: Pos2) -> Option<(DocPos, f32)> {
        let c = self.to_content(screen);
        let page = self.layout.page_at(c)?;
        let (x, y) = self.screen_to_page(page, screen);
        let t = self.text(page)?;
        let pos = t.hit(x, y)?;
        let ch = t.char_at(CharPos { ch: pos.ch.saturating_sub(1), ..pos }).or_else(|| t.char_at(pos))?;
        let d = ch.quad.bbox().dist2(x, y).sqrt() * self.zoom;
        Some((DocPos { page, pos }, d))
    }

    fn link_at(&mut self, screen: Pos2) -> Option<LinkTarget> {
        let c = self.to_content(screen);
        let page = self.layout.page_at(c)?;
        if !self.layout.page_rects[page as usize].contains(c) {
            return None;
        }
        let (x, y) = self.screen_to_page(page, screen);
        let links = self.page_links(page)?;
        links.iter().find(|l| l.rect.contains(x, y)).map(|l| l.target.clone())
    }

    fn select_word(&mut self, screen: Pos2) {
        let Some((dp, d)) = self.hit_text(screen) else { return };
        if d > 6.0 {
            return;
        }
        let Some(t) = self.text(dp.page) else { return };
        let Some(line) = t.lines().find(|(b, l, _)| *b == dp.pos.block && *l == dp.pos.line).map(|x| x.2.clone()) else { return };
        let chars: Vec<char> = line.chars.iter().map(|c| c.c).collect();
        let i = (dp.pos.ch as usize).min(chars.len().saturating_sub(1));
        let class = |c: char| -> u8 {
            if c.is_whitespace() {
                0
            } else if c.is_alphanumeric() && c.is_ascii() {
                1
            } else if ('\u{3040}'..='\u{309f}').contains(&c) {
                2
            } else if ('\u{30a0}'..='\u{30ff}').contains(&c) {
                3
            } else if ('\u{4e00}'..='\u{9fff}').contains(&c) {
                4
            } else if c.is_alphanumeric() {
                5
            } else {
                6
            }
        };
        let k = class(chars[i]);
        let mut s = i;
        while s > 0 && class(chars[s - 1]) == k {
            s -= 1;
        }
        let mut e = i + 1;
        while e < chars.len() && class(chars[e]) == k {
            e += 1;
        }
        let mk = |ch: usize| DocPos { page: dp.page, pos: CharPos { ch: ch as u32, ..dp.pos } };
        self.sel = Some(Selection { anchor: mk(s), head: mk(e) });
    }

    /// Selected text; `None` while some page texts are still being extracted.
    fn selected_text(&mut self) -> Option<String> {
        let (a, b) = self.sel?.ordered();
        let mut out = String::new();
        let mut missing = false;
        for p in a.page..=b.page {
            let from = if p == a.page { a.pos } else { CHAR_POS_START };
            let to = if p == b.page { b.pos } else { CHAR_POS_END };
            match self.text(p) {
                Some(t) => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&t.text_between(from, to));
                }
                None => missing = true,
            }
        }
        (!missing).then_some(out)
    }

    pub fn copy_selection(&mut self, ctx: &egui::Context) {
        match self.selected_text() {
            Some(s) => {
                self.status = format!("{} 文字をコピーしました", s.chars().count());
                ctx.copy_text(s);
                self.pending_copy = false;
            }
            None if self.sel.is_some() => {
                self.pending_copy = true;
                self.status = "テキストを抽出中…".into();
            }
            None => {}
        }
    }

    pub fn select_all(&mut self) {
        let n = self.sizes.len() as u32;
        if n > 0 {
            self.sel = Some(Selection {
                anchor: DocPos { page: 0, pos: CHAR_POS_START },
                head: DocPos { page: n - 1, pos: CHAR_POS_END },
            });
        }
    }

    // ------------------------------------------------------------------ search

    pub fn open_search(&mut self) {
        self.search.open = true;
        self.search.focus = true;
        if let Some(s) = self.selected_text().filter(|s| !s.is_empty() && !s.contains('\n') && s.chars().count() < 100) {
            self.search.query = s;
        }
    }

    fn start_search(&mut self, forward: bool) {
        self.search.stop();
        let q = self.search.query.trim().to_string();
        self.search.hits.clear();
        self.search.total = 0;
        self.search.current = None;
        self.search.done = false;
        self.search.error = None;
        self.search.ran_query = self.search.query.clone();
        if q.is_empty() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.search.rx = Some(self.doc.search(q, self.current_page(), cancel.clone()));
        self.search.cancel = Some(cancel);
        self.search.pending_jump = Some(forward);
    }

    fn poll_search(&mut self) {
        let Some(rx) = &self.search.rx else { return };
        let mut done = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SearchEvent::Hits { page, quads } => {
                    self.search.total += quads.len();
                    self.search.hits.insert(page, quads);
                }
                SearchEvent::Progress { done, total } => self.search.progress = (done, total),
                SearchEvent::Done => done = true,
                SearchEvent::Error(e) => {
                    self.search.error = Some(e);
                    done = true;
                }
            }
        }
        if done {
            self.search.done = true;
            self.search.rx = None;
            self.search.cancel = None;
        }
        if let Some(fwd) = self.search.pending_jump
            && !self.search.hits.is_empty()
        {
            self.search.pending_jump = None;
            self.search_step(fwd);
        }
    }

    fn search_step(&mut self, forward: bool) {
        if self.search.hits.is_empty() {
            return;
        }
        let cur = self.search.current.unwrap_or_else(|| {
            let p = self.current_page();
            if forward { (p, usize::MAX) } else { (p, 0) }
        });
        let cur = if self.search.current.is_none() && forward {
            // Before any hit on the current page.
            (cur.0.wrapping_sub(1), usize::MAX)
        } else {
            cur
        };
        let flat: Vec<(u32, usize)> = self.search.hits.iter().flat_map(|(p, q)| (0..q.len()).map(move |i| (*p, i))).collect();
        let next = if forward {
            flat.iter().copied().find(|h| *h > cur).or(flat.first().copied())
        } else {
            flat.iter().rev().copied().find(|h| *h < cur).or(flat.last().copied())
        };
        if let Some((p, i)) = next {
            self.search.current = Some((p, i));
            self.reveal(p, self.search.hits[&p][i].bbox());
        }
    }

    /// Scroll so that a page-space rectangle is visible.
    fn reveal(&mut self, page: u32, r: strata_core::RectF) {
        if self.layout.rows.is_empty() {
            self.goto_page(page, Some(r.y0));
            return;
        }
        let pr = self.layout.page_rects[page as usize];
        let top = pr.min.y + r.y0 * self.zoom;
        let bottom = pr.min.y + r.y1 * self.zoom;
        let left = pr.min.x + r.x0 * self.zoom;
        let right = pr.min.x + r.x1 * self.zoom;
        let vh = self.viewport.height();
        let vw = self.viewport.width();
        if self.paged {
            self.cur_row = self.layout.page_row[page as usize] as usize;
        }
        if top < self.scroll.y || bottom > self.scroll.y + vh {
            self.scroll.y = (top + bottom) * 0.5 - vh * 0.4;
        }
        if left < self.scroll.x || right > self.scroll.x + vw {
            self.scroll.x = (left + right) * 0.5 - vw * 0.5;
        }
    }

    // ---------------------------------------------------------------------- ui

    pub fn set_mode(&mut self, mode: ViewMode) {
        if mode == self.mode {
            return;
        }
        match mode {
            ViewMode::Reflow => {
                // The page-space position at the top of the viewport.
                let top = self.viewport.min + vec2(self.viewport.width() * 0.5, 4.0);
                let (page, y) = match self.anchor_at(top) {
                    Some((p, off)) => (p, off.y.max(0.0)),
                    None => (self.current_page(), 0.0),
                };
                let doc = self.doc.clone();
                self.reflow.get_or_insert_with(|| ReflowPane::start(&doc, None)).goto_pos(page + 1, y);
            }
            ViewMode::Pdf => {
                if let Some(r) = &mut self.reflow {
                    r.hide();
                    let (p, y) = (r.at_page.saturating_sub(1), r.at_y);
                    self.mode = mode;
                    self.goto_page(p, Some(y));
                    return;
                }
            }
        }
        self.mode = mode;
    }

    /// Hide native child windows (the tab is not visible this frame).
    pub fn hide_native(&mut self) {
        if let Some(r) = &mut self.reflow {
            r.hide();
        }
    }

    fn rebuild_reflow(&mut self, formula: Option<Arc<dyn strata_ocr::formula::FormulaEngine>>) {
        let doc = self.doc.clone();
        let (at, y) = self.reflow.as_ref().map(|r| (r.at_page, r.at_y)).unwrap_or((self.current_page() + 1, 0.0));
        self.reflow_has_latex = formula.is_some();
        let mut pane = ReflowPane::start(&doc, formula);
        pane.goto_pos(at, y);
        if let Some(old) = &mut self.reflow {
            old.hide();
        }
        self.reflow = Some(pane);
    }

    fn reflow_ui(&mut self, ui: &mut Ui, svc: &mut Services) {
        svc.pool.remove_view(self.id);
        // Formula recognition: rebuild once the engine is available.
        if svc.latex && !self.reflow_has_latex {
            let has_formulas = self.reflow.as_ref().and_then(|r| r.doc.as_ref()).is_some_and(|d| d.nodes.iter().any(|n| matches!(n, strata_core::reflow::Node::Formula { .. })));
            if has_formulas && let Some(f) = svc.ocr.formula.engine(ui.ctx()) {
                self.rebuild_reflow(Some(f));
            }
        }
        self.reflow_toolbar(ui, svc);
        if self.mode != ViewMode::Reflow {
            // Switched to the PDF view from the toolbar: drawing the text view in
            // the rest of this frame would show the webview again.
            return;
        }
        self.update_translation(&ui.ctx().clone(), svc);
        self.status_bar(ui);
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            ui.allocate_rect(rect, Sense::hover());
            let Some(pane) = &mut self.reflow else { return };
            if svc.overlay {
                pane.hide();
                ui.painter().rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
            } else {
                pane.ui(ui, rect, svc.window, svc.web.as_deref_mut(), svc.theme, *svc.text_scale, svc.focused);
            }
        });
        let msgs = self.reflow.as_mut().map(|r| r.messages()).unwrap_or_default();
        for m in msgs {
            match m {
                WebMsg::GotoPdfPage(p) => {
                    self.set_mode(ViewMode::Pdf);
                    self.goto_page(p.saturating_sub(1), None);
                }
                WebMsg::OpenUrl(u) => ctx.open_url(egui::OpenUrl::new_tab(u)),
                WebMsg::Font(d) => step_text_scale(svc.text_scale, d),
                WebMsg::Key(k) => match k.as_str() {
                    "ctrl+w" | "ctrl+f4" => self.requests.push(ViewRequest::CloseTab),
                    "ctrl+tab" => self.requests.push(ViewRequest::NextTab),
                    "ctrl+shift+tab" => self.requests.push(ViewRequest::PrevTab),
                    "ctrl+o" => self.requests.push(ViewRequest::Open),
                    "f11" => self.requests.push(ViewRequest::Fullscreen),
                    "ctrl+p" => self.requests.push(ViewRequest::Print),
                    "ctrl+e" => self.set_mode(ViewMode::Pdf),
                    _ => {}
                },
            }
        }
        // Keys that reach the main window while the webview does not have the focus.
        if svc.focused && !ctx.egui_wants_keyboard_input() {
            use egui::Modifiers;
            let consume = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
            if consume(Modifiers::COMMAND, Key::E) {
                self.set_mode(ViewMode::Pdf);
                return;
            }
            if consume(Modifiers::COMMAND, Key::Plus) || consume(Modifiers::COMMAND, Key::Equals) || consume(Modifiers::COMMAND | Modifiers::SHIFT, Key::Semicolon) {
                step_text_scale(svc.text_scale, 1);
            }
            if consume(Modifiers::COMMAND, Key::Minus) {
                step_text_scale(svc.text_scale, -1);
            }
            if consume(Modifiers::COMMAND, Key::Num0) {
                step_text_scale(svc.text_scale, 0);
            }
            let back = consume(Modifiers::SHIFT, Key::Space) || consume(Modifiers::NONE, Key::PageUp);
            let fwd = consume(Modifiers::NONE, Key::Space) || consume(Modifiers::NONE, Key::PageDown);
            if let Some(pane) = &self.reflow
                && (back || fwd)
            {
                pane.page(if fwd { 1 } else { -1 });
            }
        }
    }

    pub fn request_ocr(&mut self, scope: strata_core::ocr::OcrScope) {
        if self.ocr_job.is_none() {
            self.ocr_request = Some(scope);
        }
    }

    fn cancel_ocr(&mut self) {
        if let Some(j) = self.ocr_job.take() {
            j.cancel.store(true, Ordering::Relaxed);
        }
        self.ocr_request = None;
    }

    fn save_searchable(&mut self) {
        let stem = self.doc.info().path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(dest) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(format!("{stem}_ocr.pdf")).save_file() else { return };
        if dest == self.doc.info().path {
            self.status = "開いているファイル自体には上書きできません。別の名前で保存してください".into();
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let doc = self.doc.clone();
        std::thread::Builder::new()
            .name("strata-save-ocr".into())
            .spawn(move || {
                let _ = tx.send(doc.save_searchable_pdf(&dest).map(|n| (n, dest)));
            })
            .ok();
        self.save_job = Some(rx);
        self.status = "検索可能な PDF を書き出しています…".into();
    }

    fn poll_ocr(&mut self, ctx: &egui::Context, svc: &mut Services) {
        if let Some(rx) = &self.save_job {
            match rx.try_recv() {
                Ok(Ok((n, path))) => {
                    self.status = format!("{n} ページにテキスト層を付けて保存しました: {}", path.display());
                    self.save_job = None;
                }
                Ok(Err(e)) => {
                    self.status = format!("保存に失敗しました: {e}");
                    self.save_job = None;
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(200)),
            }
        }
        if let Some(scope) = self.ocr_request
            && let Some(engine) = svc.ocr.engine(ctx)
        {
            self.ocr_request = None;
            let cancel = Arc::new(AtomicBool::new(false));
            let rx = self.doc.run_ocr(scope, engine, cancel.clone());
            self.ocr_job = Some(OcrJob { rx, cancel, done: 0, total: 0 });
            self.status = "OCR を準備しています…".into();
        }
        if self.ocr_request.is_some() {
            self.status = svc.ocr.status().unwrap_or("OCR の準備中…").to_string();
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        let Some(job) = &mut self.ocr_job else { return };
        let mut finished = None;
        let mut pages = Vec::new();
        while let Ok(ev) = job.rx.try_recv() {
            use strata_core::ocr::OcrEvent;
            match ev {
                OcrEvent::Progress { done, total } => {
                    job.done = done;
                    job.total = total;
                }
                OcrEvent::Page(p) => pages.push(p),
                OcrEvent::Done { pages } => finished = Some(Ok(pages)),
                OcrEvent::Error(e) => finished = Some(Err(e)),
            }
        }
        for p in pages {
            // Re-request text so selection and search see the OCR result.
            self.texts.remove(&p);
        }
        if job.total > 0 {
            self.status = format!("OCR 中 {}/{} ページ", job.done, job.total);
        }
        match finished {
            Some(Ok(n)) => {
                self.ocr_job = None;
                self.status = if n == 0 { "OCR が必要なページはありませんでした".into() } else { format!("OCR 完了：{n} ページ") };
                // Rebuild the reflowed text with the new OCR results.
                if self.reflow.is_some() {
                    let f = if svc.latex { svc.ocr.formula.ready() } else { None };
                    self.rebuild_reflow(f);
                }
            }
            Some(Err(e)) => self.status = format!("OCR エラー: {e}"),
            None => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
        }
    }

    fn ocr_menu(&mut self, ui: &mut Ui) {
        use strata_core::ocr::OcrScope;
        let busy = self.ocr_job.is_some() || self.ocr_request.is_some();
        ui.menu_button(if busy { "OCR…" } else { "OCR" }, |ui| {
            if busy {
                if ui.button("OCR を中止").clicked() {
                    self.cancel_ocr();
                    self.status = "OCR を中止しました".into();
                    ui.close();
                }
                return;
            }
            if ui.button("文字のないページを OCR").on_hover_text("スキャンページや文字化けするページだけを読み取る").clicked() {
                self.request_ocr(OcrScope::Needed);
                ui.close();
            }
            if ui.button("スキャンページを OCR し直す").on_hover_text("他のソフトの OCR 文字が付いたスキャンページも読み取り、その文字を置き換える").clicked() {
                self.request_ocr(OcrScope::Scans);
                ui.close();
            }
            if ui.button("全ページを OCR").on_hover_text("文字のあるページも読み取り、既存の文字を OCR の結果で置き換える").clicked() {
                self.request_ocr(OcrScope::All);
                ui.close();
            }
            let n = self.doc.ocr_store().len();
            if n > 0 {
                ui.separator();
                ui.weak(format!("OCR 済み {n} ページ（キャッシュ済み）"));
                if ui.add_enabled(self.doc.info().is_pdf && self.save_job.is_none(), egui::Button::new("検索可能な PDF として保存…"))
                    .on_hover_text("OCR 結果を透明なテキスト層として埋め込む。暗号は外れます")
                    .clicked()
                {
                    ui.close();
                    self.save_searchable();
                }
            }
        });
    }

    fn mode_switch(&mut self, ui: &mut Ui) {
        let mut m = self.mode;
        ui.selectable_value(&mut m, ViewMode::Pdf, "PDF").on_hover_text("ページをそのまま表示");
        ui.selectable_value(&mut m, ViewMode::Reflow, "テキスト").on_hover_text("段落をつなげて表示 (Ctrl+E)");
        if m != self.mode {
            self.set_mode(m);
        }
    }

    /// Ask for a file name and save the text view as Markdown (images beside it).
    fn export_markdown(&mut self) {
        let Some(pane) = &self.reflow else { return };
        if !pane.is_ready() {
            return;
        }
        let stem = self.doc.info().path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(path) = rfd::FileDialog::new().add_filter("Markdown", &["md"]).set_file_name(format!("{stem}.md")).save_file() {
            self.status = match pane.export_markdown(&path) {
                Ok(()) => format!("保存しました: {}", path.display()),
                Err(e) => format!("保存に失敗しました: {e}"),
            };
        }
    }

    fn reflow_toolbar(&mut self, ui: &mut Ui, svc: &mut Services) {
        egui::Panel::top(Id::new(("rtoolbar", self.id))).show(ui, |ui| {
            ui.horizontal(|ui| {
                self.mode_switch(ui);
                ui.separator();
                self.translation_toolbar(ui, svc);
                ui.separator();
                let text_scale = &mut *svc.text_scale;
                if ui.button("A－").on_hover_text("文字を小さく (Ctrl+- / Ctrl+ホイール)").clicked() {
                    step_text_scale(text_scale, -1);
                }
                if ui.button(format!("{:.0}%", *text_scale * 100.0)).on_hover_text("標準の大きさに戻す (Ctrl+0)").clicked() {
                    step_text_scale(text_scale, 0);
                }
                if ui.button("A＋").on_hover_text("文字を大きく (Ctrl++ / Ctrl+ホイール)").clicked() {
                    step_text_scale(text_scale, 1);
                }
                ui.separator();
                let ready = self.reflow.as_ref().is_some_and(|r| r.is_ready());
                let stem = self.doc.info().path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                if ui.add_enabled(ready, egui::Button::new("Markdown で保存…")).clicked() {
                    self.export_markdown();
                }
                if ui.add_enabled(ready, egui::Button::new("HTML で保存…")).on_hover_text("画像を埋め込んだ 1 ファイルの HTML").clicked()
                    && let Some(path) = rfd::FileDialog::new().add_filter("HTML", &["html"]).set_file_name(format!("{stem}.html")).save_file()
                {
                    self.status = match self.reflow.as_ref().unwrap().export_html(&path, strata_core::reflow::output::Theme::Auto) {
                        Ok(()) => format!("保存しました: {}", path.display()),
                        Err(e) => format!("保存に失敗しました: {e}"),
                    };
                }
                if let Some(d) = self.reflow.as_ref().and_then(|r| r.doc.as_ref()) {
                    ui.separator();
                    let figs = d.images.len();
                    let ocr = d.nodes.iter().filter(|n| matches!(n, strata_core::reflow::Node::PageImage { .. })).count();
                    let extra = if ocr > 0 { format!("・要OCR {ocr} ページ") } else { String::new() };
                    ui.weak(format!("{} 要素・画像 {figs}{extra}", d.nodes.len()));
                    let idle = self.ocr_job.is_none() && self.ocr_request.is_none();
                    // Another program's OCR text that reads badly: offer ours instead.
                    let poor_layer = d.ocr_layer.filter(|q| q.poor());
                    if let Some(q) = poor_layer {
                        let rate = |r: Option<f32>| r.map(|r| format!("{:.0}%", r * 100.0));
                        let detail = [rate(q.en_rate()).map(|r| format!("英単語の誤認識らしいもの {r}")), rate(q.ja_rate()).map(|r| format!("日本語の誤認識らしい文字 {r}"))]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join("、");
                        ui.colored_label(Color32::from_rgb(230, 160, 40), "⚠ 既存の OCR 文字の精度が低いようです")
                            .on_hover_text(format!("スキャン {} ページに付いている OCR 文字の推定: {detail}", q.pages));
                        if idle && ui.button("内蔵 OCR で読み直す").on_hover_text("スキャンページを内蔵 OCR で読み取り、既存の OCR 文字の代わりに使う").clicked() {
                            self.request_ocr(strata_core::ocr::OcrScope::Scans);
                        }
                    } else if d.ocr_layer.is_some_and(|q| q.japanese()) {
                        // A Japanese scan whose text layer passes: ours still reads most better.
                        if idle
                            && ui
                                .button("内蔵 OCR で読み直す")
                                .on_hover_text("スキャンに付いている OCR 文字を表示しています。日本語のスキャンは、内蔵 OCR で読み直すと誤字や段落の区切りがよくなることが多い")
                                .clicked()
                        {
                            self.request_ocr(strata_core::ocr::OcrScope::Scans);
                        }
                    } else if ocr > 0 && idle && ui.button("OCR を実行").clicked() {
                        self.request_ocr(strata_core::ocr::OcrScope::Needed);
                    }
                }
                ui.separator();
                self.ocr_menu(ui);
            });
        });
    }

    pub fn ui(&mut self, ui: &mut Ui, svc: &mut Services) {
        let ctx = ui.ctx().clone();
        self.poll_ocr(&ctx, svc);
        self.poll_annot(&ctx);
        self.poll_table(&ctx, svc.ocr);
        self.poll_image_save();
        if self.mode == ViewMode::Reflow {
            self.reflow_ui(ui, svc);
            return;
        }
        self.poll_search();
        if self.pending_copy {
            self.copy_selection(ui.ctx());
        }
        self.toolbar(ui);
        if self.annot.bar {
            self.annot_toolbar(ui);
        }
        if self.search.open {
            self.search_bar(ui);
        }
        self.status_bar(ui);
        let mut wanted_thumbs = Vec::new();
        match self.sidebar {
            Sidebar::None => {}
            Sidebar::Outline => self.outline_panel(ui),
            Sidebar::Thumbs => self.thumbs_panel(ui, svc, &mut wanted_thumbs),
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.canvas(ui, svc, wanted_thumbs));
        self.annot_dialogs(&ctx);
        self.table_dialog(&ctx);
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        egui::Panel::top(Id::new(("toolbar", self.id))).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                self.mode_switch(ui);
                ui.separator();
                let sb = |s: Sidebar| if s == Sidebar::None { "☰" } else { "☰" };
                if ui.selectable_label(self.sidebar != Sidebar::None, sb(self.sidebar)).on_hover_text("サイドバー (F9)").clicked() {
                    self.sidebar = if self.sidebar == Sidebar::None { Sidebar::Thumbs } else { Sidebar::None };
                }
                ui.separator();
                // Page number field.
                let n = self.page_count();
                if !self.page_input_focus {
                    self.page_input = (self.current_page() + 1).to_string();
                }
                let r = ui.add(TextEdit::singleline(&mut self.page_input).desired_width(48.0).horizontal_align(Align::RIGHT).id(Id::new(("page_input", self.id))));
                self.page_input_focus = r.has_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    if let Ok(p) = self.page_input.trim().parse::<u32>() {
                        self.goto_page(p.saturating_sub(1), None);
                    }
                }
                ui.label(format!("/ {n}"));
                ui.separator();
                if ui.button("－").on_hover_text("縮小 (Ctrl+-)").clicked() {
                    self.zoom_step(-1.0, None);
                }
                egui::ComboBox::from_id_salt(("zoom", self.id))
                    .width(96.0)
                    .selected_text(match self.fit {
                        Fit::Width => format!("幅 {:.0}%", self.zoom * 100.0),
                        Fit::Page => format!("全体 {:.0}%", self.zoom * 100.0),
                        Fit::Free => format!("{:.0}%", self.zoom * 100.0),
                    })
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(self.fit == Fit::Width, "幅に合わせる (Ctrl+2)").clicked() {
                            self.set_fit(Fit::Width);
                        }
                        if ui.selectable_label(self.fit == Fit::Page, "ページ全体 (Ctrl+0)").clicked() {
                            self.set_fit(Fit::Page);
                        }
                        ui.separator();
                        for z in [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 8.0] {
                            if ui.selectable_label(self.fit == Fit::Free && (self.zoom - z).abs() < 1e-3, format!("{:.0}%", z * 100.0)).clicked() {
                                self.set_zoom(z, None);
                            }
                        }
                    });
                if ui.button("＋").on_hover_text("拡大 (Ctrl++)").clicked() {
                    self.zoom_step(1.0, None);
                }
                ui.separator();
                let mut sp = self.spread;
                ui.selectable_value(&mut sp, Spread::Single, "単").on_hover_text("単ページ");
                ui.selectable_value(&mut sp, Spread::Double, "見開").on_hover_text("見開き");
                ui.selectable_value(&mut sp, Spread::DoubleCover, "表紙+見開").on_hover_text("表紙を単独にした見開き");
                if sp != self.spread {
                    self.set_spread(sp);
                }
                if ui.selectable_label(self.r2l, "右綴じ").on_hover_text("右から左へ読む（縦書きの本）").clicked() {
                    self.r2l = !self.r2l;
                    self.layout_params = None;
                }
                if ui.selectable_label(self.paged, "ページ送り").on_hover_text("1画面ずつめくる表示").clicked() {
                    let p = self.current_page();
                    self.paged = !self.paged;
                    self.layout_params = None;
                    self.goto_page(p, None);
                }
                ui.separator();
                self.ocr_menu(ui);
                self.table_toolbar_button(ui);
                if ui.selectable_label(self.annot.bar, "注釈").on_hover_text("注釈ツール").clicked() {
                    self.annot.bar = !self.annot.bar;
                    if !self.annot.bar {
                        self.annot.tool = annot_ui::Tool::Select;
                    }
                }
                if ui.selectable_label(self.search.open, "🔍").on_hover_text("検索 (Ctrl+F)").clicked() {
                    if self.search.open {
                        self.search.open = false;
                        self.search.stop();
                    } else {
                        self.open_search();
                    }
                }
            });
        });
    }

    fn search_bar(&mut self, ui: &mut Ui) {
        egui::Panel::top(Id::new(("search", self.id))).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("検索");
                let r = ui.add(TextEdit::singleline(&mut self.search.query).desired_width(260.0).id(Id::new(("search_input", self.id))));
                if self.search.focus {
                    r.request_focus();
                    self.search.focus = false;
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                let shift = ui.input(|i| i.modifiers.shift);
                if enter {
                    if self.search.query != self.search.ran_query {
                        self.start_search(!shift);
                    } else {
                        self.search_step(!shift);
                    }
                    r.request_focus();
                }
                if ui.button("▲").on_hover_text("前へ (Shift+Enter / Shift+F3)").clicked() {
                    self.search_step(false);
                }
                if ui.button("▼").on_hover_text("次へ (Enter / F3)").clicked() {
                    if self.search.query != self.search.ran_query {
                        self.start_search(true);
                    } else {
                        self.search_step(true);
                    }
                }
                let s = &self.search;
                if let Some(e) = &s.error {
                    ui.colored_label(Color32::RED, e);
                } else if !s.ran_query.is_empty() {
                    let idx = s.current.map(|(p, i)| s.hits.range(..p).map(|(_, q)| q.len()).sum::<usize>() + i + 1);
                    let pos = idx.map(|i| format!("{i} / ")).unwrap_or_default();
                    let prog = if s.done { String::new() } else { format!("（検索中 {}/{}）", s.progress.0, s.progress.1) };
                    ui.label(format!("{pos}{} 件{prog}", s.total));
                }
                if ui.button("×").on_hover_text("閉じる (Esc)").clicked() {
                    self.search.open = false;
                    self.search.stop();
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        egui::Panel::bottom(Id::new(("status", self.id))).show(ui, |ui| {
            ui.horizontal(|ui| {
                let info = self.doc.info();
                ui.label(format!("{}  ・  {} ページ", info.format, info.page_count));
                if !info.encryption.is_empty() && info.encryption != "None" {
                    ui.label(format!("・ 暗号: {}", info.encryption)).on_hover_text("権限フラグは無視しています");
                }
                for n in &info.notes {
                    ui.colored_label(Color32::from_rgb(230, 160, 40), format!("⚠ {n}"));
                }
                if !self.doc.sizes_complete() {
                    ui.spinner();
                    ui.label("ページ寸法を走査中");
                }
                if !self.status.is_empty() {
                    ui.separator();
                    ui.label(&self.status);
                }
                self.table_status_button(ui);
            });
        });
    }

    fn outline_panel(&mut self, ui: &mut Ui) {
        let doc = self.doc.clone();
        let pending = self.outline.get_or_insert_with(|| doc.outline());
        let items = match pending.poll() {
            Some(Ok(o)) => Some(o.clone()),
            _ => None,
        };
        let mut target = None;
        egui::Panel::left(Id::new(("outline", self.id))).default_size(240.0).resizable(true).show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.selectable_label(false, "サムネイル").clicked() {
                    self.sidebar = Sidebar::Thumbs;
                }
                let _ = ui.selectable_label(true, "目次");
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match &items {
                None => {
                    ui.spinner();
                }
                Some(items) if items.is_empty() => {
                    ui.weak("目次はありません");
                }
                Some(items) => outline_items(ui, items, Id::new(("ol", self.id)), &mut target),
            });
        });
        if let Some(t) = target {
            self.follow_link(ui.ctx(), &t);
        }
    }

    fn thumbs_panel(&mut self, ui: &mut Ui, svc: &mut Services, wanted: &mut Vec<TileKey>) {
        let n = self.page_count();
        let cur = self.current_page();
        let mut clicked = None;
        let follow = std::mem::take(&mut self.thumbs_follow) || cur != self.last_page;
        self.last_page = cur;
        egui::Panel::left(Id::new(("thumbs", self.id))).default_size(170.0).resizable(true).show(ui, |ui| {
            ui.horizontal(|ui| {
                let _ = ui.selectable_label(true, "サムネイル");
                if ui.selectable_label(false, "目次").clicked() {
                    self.sidebar = Sidebar::Outline;
                }
            });
            ui.separator();
            let row_h = 190.0;
            let mut sa = egui::ScrollArea::vertical().auto_shrink(false);
            if follow {
                let spacing = ui.spacing().item_spacing.y;
                sa = sa.vertical_scroll_offset(((cur as f32) * (row_h + spacing) - 60.0).max(0.0));
            }
            sa.show_rows(ui, row_h, n, |ui, range| {
                for p in range {
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                    let size = self.sizes[p];
                    let label_h = 16.0;
                    let box_ = Rect::from_min_max(rect.min + vec2(8.0, 4.0), rect.max - vec2(8.0, label_h + 2.0));
                    let s = (box_.width() / size.w).min(box_.height() / size.h);
                    let pr = Rect::from_center_size(box_.center(), vec2(size.w * s, size.h * s));
                    let painter = ui.painter_at(rect);
                    if p as u32 == cur {
                        painter.rect_filled(rect.shrink(1.0), 4.0, ui.visuals().selection.bg_fill.gamma_multiply(0.5));
                    }
                    painter.rect_filled(pr, 0.0, Color32::WHITE);
                    draw_base(&painter, svc.tiles, self.doc.id(), p as u32, self.doc.page_rev(p as u32), size, pr, wanted);
                    painter.rect_stroke(pr, 0.0, Stroke::new(1.0, Color32::from_gray(150)), StrokeKind::Outside);
                    painter.text(pos2(rect.center().x, rect.max.y - label_h * 0.5), egui::Align2::CENTER_CENTER, (p + 1).to_string(), egui::FontId::proportional(12.0), ui.visuals().text_color());
                    if resp.clicked() {
                        clicked = Some(p as u32);
                    }
                }
            });
        });
        if let Some(p) = clicked {
            self.goto_page(p, None);
            self.thumbs_follow = false;
        }
    }

    fn canvas(&mut self, ui: &mut Ui, svc: &mut Services, thumb_wanted: Vec<TileKey>) {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let first_frame = !self.viewport.is_positive();
        self.viewport = rect;
        let ctx = ui.ctx().clone();
        let ppp = ctx.pixels_per_point();
        if self.sizes.is_empty() {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "ページがありません", egui::FontId::proportional(16.0), Color32::GRAY);
            return;
        }
        self.apply_fit(rect.size());
        self.ensure_layout(rect.width());
        if first_frame || self.pending_goto.is_some() {
            let (p, y) = self.pending_goto.take().unwrap_or((0, None));
            if first_frame && p == 0 && y.is_none() {
                self.scroll = Vec2::ZERO;
            } else {
                self.goto_page(p, y);
            }
        }

        self.handle_input(ui, &resp, svc.focused);
        if self.fit != Fit::Free {
            self.center_current_row();
        }
        self.clamp_scroll();

        // ---- draw
        let painter = ui.painter_at(rect);
        let bg = if ui.visuals().dark_mode { Color32::from_gray(38) } else { Color32::from_gray(200) };
        painter.rect_filled(rect, 0.0, bg);
        let rows = if self.paged {
            let r = self.cur_row.min(self.layout.rows.len() - 1);
            r..r + 1
        } else {
            self.layout.rows_in(self.scroll.y, self.scroll.y + rect.height())
        };
        let mut visible_pages: Vec<u32> = Vec::new();
        for r in rows.clone() {
            visible_pages.extend(self.layout.rows[r].pages.iter().copied());
        }
        let mut want_base = Vec::new();
        let mut want_target: Vec<(f32, TileKey)> = Vec::new();
        let doc_id = self.doc.id();
        for &p in &visible_pages {
            let pr = self.page_screen_rect(p);
            if !pr.intersects(rect) {
                continue;
            }
            painter.rect_filled(pr.translate(vec2(2.0, 2.0)), 0.0, Color32::from_black_alpha(60));
            painter.rect_filled(pr, 0.0, Color32::WHITE);
            let size = self.sizes[p as usize];
            let base = base_level(size);
            let target = level_for_scale(self.zoom * ppp).max(base);
            // Coarse to fine: every cached level fills what finer levels have not yet covered.
            let mut levels = svc.tiles.levels(doc_id, p);
            levels.retain(|&l| l != target && (l == base || (l - target).abs() <= 8));
            if !levels.contains(&base) {
                levels.insert(0, base);
            }
            for l in levels {
                draw_level(&painter, svc.tiles, doc_id, p, self.doc.page_rev(p), size, pr, rect, l, if l == base { Some(&mut want_base) } else { None }, None);
            }
            draw_level(&painter, svc.tiles, doc_id, p, self.doc.page_rev(p), size, pr, rect, target, None, Some(&mut want_target));
            if let Some(e) = svc.tiles.error(doc_id, p) {
                painter.rect_filled(pr, 0.0, Color32::from_rgb(255, 235, 235));
                painter.text(pr.center(), egui::Align2::CENTER_CENTER, format!("このページを描画できません\n{e}"), egui::FontId::proportional(13.0), Color32::DARK_RED);
            }
            self.draw_overlays(&painter, p);
            // Prefetch the page text so the first drag already selects text.
            let _ = self.text(p);
        }
        want_target.sort_by(|a, b| a.0.total_cmp(&b.0));

        // Prefetch: low-res for nearby rows, full-res for the next screen.
        let mut want_prefetch = Vec::new();
        if let (Some(first), Some(last)) = (visible_pages.first(), visible_pages.last()) {
            let fr = self.layout.page_row[*first as usize] as usize;
            let lr = self.layout.page_row[*last as usize] as usize;
            let lo = fr.saturating_sub(3);
            let hi = (lr + 6).min(self.layout.rows.len() - 1);
            for r in lo..=hi {
                for &p in &self.layout.rows[r].pages {
                    let size = self.sizes[p as usize];
                    let k = base_level(size);
                    let (c, rr) = tile_grid(size, k);
                    for ty in 0..rr {
                        for tx in 0..c {
                            let key = TileKey { doc: doc_id, page: p, rev: self.doc.page_rev(p), level: k, tx, ty };
                            if !svc.tiles.contains(&key) {
                                want_prefetch.push(key);
                            }
                        }
                    }
                }
            }
            // Full resolution one screen ahead in reading direction.
            if !self.paged {
                let ahead = rect.translate(vec2(0.0, rect.height()));
                for r in self.layout.rows_in(self.scroll.y + rect.height(), self.scroll.y + rect.height() * 2.0) {
                    for &p in &self.layout.rows[r].pages {
                        let size = self.sizes[p as usize];
                        let pr = self.page_screen_rect(p);
                        let target = level_for_scale(self.zoom * ppp).max(base_level(size));
                        let mut v = Vec::new();
                        collect_level(svc.tiles, doc_id, p, self.doc.page_rev(p), size, pr, ahead, target, &mut v);
                        want_prefetch.extend(v);
                    }
                }
            } else if lr + 1 < self.layout.rows.len() {
                // Next spread at the zoom it will be shown with.
                for &p in &self.layout.rows[lr + 1].pages {
                    let size = self.sizes[p as usize];
                    let target = level_for_scale(self.zoom * ppp).max(base_level(size));
                    let (c, rr) = tile_grid(size, target);
                    if (c as u32) * (rr as u32) <= 64 {
                        for ty in 0..rr {
                            for tx in 0..c {
                                let key = TileKey { doc: doc_id, page: p, rev: self.doc.page_rev(p), level: target, tx, ty };
                                if !svc.tiles.contains(&key) {
                                    want_prefetch.push(key);
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut wanted: Vec<TileKey> = want_base;
        wanted.extend(want_target.into_iter().map(|e| e.1));
        wanted.extend(thumb_wanted);
        wanted.extend(want_prefetch);
        dedup_keep_order(&mut wanted);
        svc.pool.set_wanted(self.id, wanted);

        let cur = self.current_page();
        self.trim_caches(cur);
    }

    fn draw_overlays(&mut self, painter: &egui::Painter, p: u32) {
        let z = self.zoom;
        let quad_shape = |v: &DocView, q: &QuadF, fill: Color32| {
            let pts = vec![v.page_to_screen(p, q.ul), v.page_to_screen(p, q.ur), v.page_to_screen(p, q.lr), v.page_to_screen(p, q.ll)];
            Shape::convex_polygon(pts, fill, Stroke::NONE)
        };
        let _ = z;
        if let Some(hits) = self.search.hits.get(&p) {
            for (i, q) in hits.iter().enumerate() {
                let cur = self.search.current == Some((p, i));
                let fill = if cur { Color32::from_rgba_unmultiplied(255, 120, 0, 110) } else { Color32::from_rgba_unmultiplied(255, 220, 0, 90) };
                painter.add(quad_shape(self, q, fill));
            }
        }
        if let Some(sel) = self.sel {
            let (a, b) = sel.ordered();
            if p >= a.page && p <= b.page {
                let from = if p == a.page { a.pos } else { CHAR_POS_START };
                let to = if p == b.page { b.pos } else { CHAR_POS_END };
                if let Some(t) = self.text(p) {
                    for q in t.selection_quads(from, to) {
                        painter.add(quad_shape(self, &q, Color32::from_rgba_unmultiplied(40, 110, 255, 80)));
                    }
                }
            }
        }
        self.draw_annot_overlay(painter, p);
        self.draw_table_overlay(painter, p);
    }

    /// Horizontally center the current row (documents mixing portrait and
    /// landscape pages are wider than any single row).
    fn center_current_row(&mut self) {
        let row = if self.paged { self.cur_row } else { self.layout.row_at(self.scroll.y + self.viewport.height() * 0.3) };
        let Some(r) = self.layout.rows.get(row) else { return };
        let (x0, x1) = r.pages.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &p| {
            let pr = self.layout.page_rects[p as usize];
            (a.min(pr.min.x), b.max(pr.max.x))
        });
        self.scroll.x = (x0 + x1) * 0.5 - self.viewport.width() * 0.5;
    }

    fn clamp_scroll(&mut self) {
        let vw = self.viewport.width();
        let vh = self.viewport.height();
        let max_x = (self.layout.width - vw).max(0.0);
        self.scroll.x = self.scroll.x.clamp(0.0, max_x);
        if self.paged && !self.layout.rows.is_empty() {
            let r = &self.layout.rows[self.cur_row.min(self.layout.rows.len() - 1)];
            let h = r.y1 - r.y0 + 2.0 * MARGIN;
            if h <= vh {
                self.scroll.y = r.y0 - MARGIN - (vh - h) * 0.5;
            } else {
                self.scroll.y = self.scroll.y.clamp(r.y0 - MARGIN, r.y1 + MARGIN - vh);
            }
        } else {
            let max_y = (self.layout.height - vh).max(0.0);
            self.scroll.y = self.scroll.y.clamp(0.0, max_y);
        }
    }

    fn handle_input(&mut self, ui: &mut Ui, resp: &egui::Response, focused: bool) {
        let ctx = ui.ctx().clone();
        let vp = self.viewport;
        let hover = ctx.pointer_hover_pos().filter(|p| vp.contains(*p) && ui.rect_contains_pointer(vp));

        // Wheel and pinch.
        if hover.is_some() || resp.dragged() {
            let (scroll, zoom) = ctx.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            if zoom != 1.0 {
                self.set_zoom(self.zoom * zoom, hover);
            } else if scroll != Vec2::ZERO {
                if self.paged && self.row_fits() {
                    self.wheel_acc += scroll.y;
                    if self.wheel_acc.abs() > 60.0 {
                        self.step_rows(if self.wheel_acc < 0.0 { 1 } else { -1 });
                        self.wheel_acc = 0.0;
                    }
                } else {
                    self.scroll -= scroll;
                    if self.paged {
                        self.flip_at_edges(scroll.y);
                    }
                }
            }
        }

        let annot_consumed = if self.table.mode { self.table_input(ui, resp) } else { self.annot_input(ui, resp) };
        if !annot_consumed {
            // Pointer: pan / select / links.
            let primary_start = resp.drag_started_by(egui::PointerButton::Primary);
            if primary_start || resp.drag_started_by(egui::PointerButton::Middle) {
                let start = ctx.input(|i| i.pointer.press_origin()).unwrap_or_default();
                self.drag = DragMode::Pan;
                if primary_start && !ctx.input(|i| i.key_down(Key::Space)) {
                    if let Some((dp, d)) = self.hit_text(start)
                        && d < 12.0
                    {
                        self.drag = DragMode::Select;
                        self.sel = Some(Selection { anchor: dp, head: dp });
                    }
                }
            }
            if resp.dragged() {
                match self.drag {
                    DragMode::Pan => {
                        self.scroll -= resp.drag_delta();
                        ctx.set_cursor_icon(CursorIcon::Grabbing);
                    }
                    DragMode::Select => {
                        if let Some(pos) = resp.interact_pointer_pos() {
                            if let Some((dp, _)) = self.hit_text(pos)
                                && let Some(s) = &mut self.sel
                            {
                                s.head = dp;
                            }
                            // Auto-scroll near the edges.
                            let dy = if pos.y < vp.min.y { pos.y - vp.min.y } else if pos.y > vp.max.y { pos.y - vp.max.y } else { 0.0 };
                            if dy != 0.0 {
                                self.scroll.y += dy * 0.3;
                                ctx.request_repaint();
                            }
                        }
                        ctx.set_cursor_icon(CursorIcon::Text);
                    }
                    DragMode::None => {}
                }
            }
            if resp.drag_stopped() {
                self.drag = DragMode::None;
            }
            if resp.double_clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    self.select_word(p);
                }
            } else if resp.clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    if let Some(t) = self.link_at(p) {
                        self.follow_link(&ctx, &t);
                    } else {
                        self.sel = None;
                    }
                }
            }
            if let Some(h) = hover
                && self.drag == DragMode::None
            {
                if self.link_at(h).is_some() {
                    ctx.set_cursor_icon(CursorIcon::PointingHand);
                } else if matches!(self.hit_text(h), Some((_, d)) if d < 4.0) {
                    ctx.set_cursor_icon(CursorIcon::Text);
                }
            }
            if resp.secondary_clicked() {
                self.menu_at = resp.interact_pointer_pos().and_then(|s| {
                    let c = self.to_content(s);
                    let page = self.layout.page_at(c)?;
                    let (x, y) = self.screen_to_page(page, s);
                    self.layout.page_rects[page as usize].contains(c).then_some((page, x, y))
                });
            }
            resp.context_menu(|ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                // The image under the click (the topmost one); the list arrives a frame or two later.
                if let Some((page, x, y)) = self.menu_at
                    && let Some(list) = self.page_images(page)
                    && let Some(i) = list.iter().rposition(|im| im.bbox.contains(x, y))
                {
                    let im = &list[i];
                    if ui.button("名前を付けて画像を保存…").on_hover_text(format!("{}×{} ピクセル", im.width, im.height)).clicked() {
                        ui.close();
                        self.save_image_as(page, i, im);
                    }
                    ui.separator();
                }
                if ui.add_enabled(self.sel.is_some(), egui::Button::new("コピー (Ctrl+C)")).clicked() {
                    self.copy_selection(ui.ctx());
                    ui.close();
                }
                if ui.button("すべて選択 (Ctrl+A)").clicked() {
                    self.select_all();
                    ui.close();
                }
                if ui.button("このページのテキストをコピー").clicked() {
                    let p = self.current_page();
                    self.sel = Some(Selection { anchor: DocPos { page: p, pos: CHAR_POS_START }, head: DocPos { page: p, pos: CHAR_POS_END } });
                    self.copy_selection(ui.ctx());
                    ui.close();
                }
            });
        }

        if focused && !ctx.egui_wants_keyboard_input() {
            self.handle_keys(&ctx);
        } else if focused && self.search.open {
            // F3 works while typing in the search box.
            let (f3, shift) = ctx.input(|i| (i.key_pressed(Key::F3), i.modifiers.shift));
            if f3 {
                self.search_step(!shift);
            }
        }
    }

    fn row_fits(&self) -> bool {
        let Some(r) = self.layout.rows.get(self.cur_row) else { return true };
        r.y1 - r.y0 + 2.0 * MARGIN <= self.viewport.height() + 0.5
    }

    fn flip_at_edges(&mut self, dy: f32) {
        let Some(r) = self.layout.rows.get(self.cur_row) else { return };
        let vh = self.viewport.height();
        if dy < 0.0 && self.scroll.y > r.y1 + MARGIN - vh + 1.0 && self.cur_row + 1 < self.layout.rows.len() {
            self.cur_row += 1;
            self.scroll.y = self.layout.rows[self.cur_row].y0 - MARGIN;
        } else if dy > 0.0 && self.scroll.y < r.y0 - MARGIN - 1.0 && self.cur_row > 0 {
            self.cur_row -= 1;
            let r = &self.layout.rows[self.cur_row];
            self.scroll.y = r.y1 + MARGIN - vh;
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        use egui::Modifiers;
        let vh = self.viewport.height();
        let vw = self.viewport.width();
        let line = 48.0;
        let page_step = (vh - 48.0).max(vh * 0.5);
        self.annot_keys(ctx);
        let consume = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));

        // egui turns Ctrl+C into an `Event::Copy` instead of a key press.
        let copy = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if copy || consume(Modifiers::COMMAND, Key::C) {
            self.copy_selection(ctx);
        }
        if consume(Modifiers::COMMAND, Key::A) {
            self.select_all();
        }
        if consume(Modifiers::COMMAND, Key::F) {
            self.open_search();
        }
        if consume(Modifiers::COMMAND, Key::E) {
            self.set_mode(ViewMode::Reflow);
        }
        if consume(Modifiers::COMMAND, Key::G) {
            ctx.memory_mut(|m| m.request_focus(Id::new(("page_input", self.id))));
        }
        if consume(Modifiers::NONE, Key::F9) {
            self.sidebar = if self.sidebar == Sidebar::None { Sidebar::Thumbs } else { Sidebar::None };
        }
        if consume(Modifiers::SHIFT, Key::F3) {
            self.search_step(false);
        }
        if consume(Modifiers::NONE, Key::F3) {
            self.search_step(true);
        }
        if consume(Modifiers::NONE, Key::Escape) {
            if self.search.open {
                self.search.open = false;
                self.search.stop();
            } else {
                self.sel = None;
            }
        }
        // Zoom.
        if consume(Modifiers::COMMAND, Key::Plus) || consume(Modifiers::COMMAND, Key::Equals) || consume(Modifiers::COMMAND | Modifiers::SHIFT, Key::Semicolon) {
            self.zoom_step(1.0, None);
        }
        if consume(Modifiers::COMMAND, Key::Minus) {
            self.zoom_step(-1.0, None);
        }
        if consume(Modifiers::COMMAND, Key::Num0) {
            self.set_fit(Fit::Page);
        }
        if consume(Modifiers::COMMAND, Key::Num1) {
            self.set_zoom(1.0, None);
        }
        if consume(Modifiers::COMMAND, Key::Num2) {
            self.set_fit(Fit::Width);
        }
        // Movement.
        let wider = self.layout.width > vw + 1.0;
        let fits = !self.paged || self.row_fits();
        let (next_key, prev_key) = if self.r2l && self.spread != Spread::Single { (Key::ArrowLeft, Key::ArrowRight) } else { (Key::ArrowRight, Key::ArrowLeft) };
        if consume(Modifiers::COMMAND, Key::Home) || consume(Modifiers::NONE, Key::Home) {
            self.goto_page(0, None);
        }
        if consume(Modifiers::COMMAND, Key::End) || consume(Modifiers::NONE, Key::End) {
            let n = self.page_count() as u32;
            self.goto_page(n.saturating_sub(1), None);
        }
        // Shift+Space first: a shortcut without Shift also matches it pressed with Shift.
        if consume(Modifiers::NONE, Key::PageUp) || consume(Modifiers::SHIFT, Key::Space) {
            if self.paged && fits { self.step_rows(-1) } else { self.scroll_by(-page_step) }
        }
        if consume(Modifiers::NONE, Key::PageDown) || consume(Modifiers::NONE, Key::Space) {
            if self.paged && fits { self.step_rows(1) } else { self.scroll_by(page_step) }
        }
        if consume(Modifiers::NONE, Key::ArrowDown) {
            if self.paged && fits { self.step_rows(1) } else { self.scroll_by(line) }
        }
        if consume(Modifiers::NONE, Key::ArrowUp) {
            if self.paged && fits { self.step_rows(-1) } else { self.scroll_by(-line) }
        }
        if consume(Modifiers::NONE, next_key) {
            if wider { self.scroll.x += if next_key == Key::ArrowRight { line } else { -line } } else { self.step_rows(1) }
        }
        if consume(Modifiers::NONE, prev_key) {
            if wider { self.scroll.x += if prev_key == Key::ArrowRight { line } else { -line } } else { self.step_rows(-1) }
        }
    }

    fn scroll_by(&mut self, dy: f32) {
        self.scroll.y += dy;
        if self.paged {
            self.flip_at_edges(-dy);
        }
    }

    pub fn close(&mut self, pool: &RenderPool) {
        self.cancel_ocr();
        self.search.stop();
        pool.remove_view(self.id);
    }
}

/// Step the text view's font size up (`dir` > 0), down (< 0) or back to 100% (0).
fn step_text_scale(scale: &mut f32, dir: i32) {
    const STEPS: [f32; 14] = [0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.25, 1.4, 1.6, 1.8, 2.0, 2.4, 2.8, 3.2];
    let s = *scale;
    *scale = match dir.signum() {
        1 => STEPS.iter().copied().find(|&v| v > s * 1.01).unwrap_or(s),
        -1 => STEPS.iter().rev().copied().find(|&v| v < s * 0.99).unwrap_or(s),
        _ => 1.0,
    };
}

fn outline_items(ui: &mut Ui, items: &[OutlineItem], parent: Id, target: &mut Option<LinkTarget>) {
    for (i, it) in items.iter().enumerate() {
        let id = parent.with(i);
        if it.children.is_empty() {
            if ui.selectable_label(false, &it.title).clicked() {
                *target = it.target.clone();
            }
        } else {
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
                .show_header(ui, |ui| {
                    if ui.selectable_label(false, &it.title).clicked() {
                        *target = it.target.clone();
                    }
                })
                .body(|ui| outline_items(ui, &it.children, id, target));
        }
    }
}

/// Level at which the whole page fits in one tile; always requested first.
pub fn base_level(size: SizeF) -> i16 {
    let s = TILE_PX as f32 / size.w.max(size.h).max(1.0);
    (s.log2() * strata_core::render::LEVELS_PER_OCTAVE).floor() as i16
}

fn tile_screen_rect(page_rect: Rect, size: SizeF, level: i16, tx: u16, ty: u16, w: u32, h: u32) -> Rect {
    // Page rect spans the page's full size; px -> screen via the page's scale.
    let sl = level_scale(level);
    let k = page_rect.width() / size.w / sl; // screen points per device pixel at this level
    let x0 = page_rect.min.x + (tx as u32 * TILE_PX) as f32 * k;
    let y0 = page_rect.min.y + (ty as u32 * TILE_PX) as f32 * k;
    Rect::from_min_size(pos2(x0, y0), vec2(w as f32 * k, h as f32 * k))
}

/// Range of tile indices of `level` that intersect `clip`.
fn visible_tiles(page_rect: Rect, size: SizeF, level: i16, clip: Rect) -> Option<(std::ops::Range<u16>, std::ops::Range<u16>)> {
    let vis = page_rect.intersect(clip);
    if !vis.is_positive() {
        return None;
    }
    let sl = level_scale(level);
    let k = page_rect.width() / size.w / sl;
    let t = TILE_PX as f32 * k;
    let (c, r) = tile_grid(size, level);
    let x0 = (((vis.min.x - page_rect.min.x) / t).floor().max(0.0) as u16).min(c);
    let x1 = (((vis.max.x - page_rect.min.x) / t).ceil().max(0.0) as u16).min(c);
    let y0 = (((vis.min.y - page_rect.min.y) / t).floor().max(0.0) as u16).min(r);
    let y1 = (((vis.max.y - page_rect.min.y) / t).ceil().max(0.0) as u16).min(r);
    Some((x0..x1, y0..y1))
}

#[allow(clippy::too_many_arguments)]
fn draw_level(
    painter: &egui::Painter,
    tiles: &mut TileCache,
    doc: DocId,
    page: u32,
    rev: u32,
    size: SizeF,
    page_rect: Rect,
    clip: Rect,
    level: i16,
    mut want_all: Option<&mut Vec<TileKey>>,
    mut want_by_dist: Option<&mut Vec<(f32, TileKey)>>,
) {
    let Some((xs, ys)) = visible_tiles(page_rect, size, level, clip) else { return };
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    let center = clip.center();
    for ty in ys {
        for tx in xs.clone() {
            let key = TileKey { doc, page, rev, level, tx, ty };
            let have = tiles.contains(&key);
            if !have {
                if let Some(v) = want_all.as_deref_mut() {
                    v.push(key);
                }
                if let Some(v) = want_by_dist.as_deref_mut() {
                    let r = tile_screen_rect(page_rect, size, level, tx, ty, TILE_PX, TILE_PX);
                    v.push((r.center().distance_sq(center), key));
                }
            }
            // While an edited page re-renders, show its previous revision.
            let shown = if have || rev == 0 { key } else { TileKey { rev: rev - 1, ..key } };
            if let Some(t) = tiles.get(&shown) {
                let r = tile_screen_rect(page_rect, size, level, tx, ty, t.w, t.h);
                painter.image(t.tex.id(), r, uv, Color32::WHITE);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_level(tiles: &TileCache, doc: DocId, page: u32, rev: u32, size: SizeF, page_rect: Rect, clip: Rect, level: i16, out: &mut Vec<TileKey>) {
    let Some((xs, ys)) = visible_tiles(page_rect, size, level, clip) else { return };
    for ty in ys {
        for tx in xs.clone() {
            let key = TileKey { doc, page, rev, level, tx, ty };
            if !tiles.contains(&key) {
                out.push(key);
            }
        }
    }
}

/// Draw the page's base-level tiles into `rect` (thumbnails).
fn draw_base(painter: &egui::Painter, tiles: &mut TileCache, doc: DocId, page: u32, rev: u32, size: SizeF, rect: Rect, wanted: &mut Vec<TileKey>) {
    let level = base_level(size);
    draw_level(painter, tiles, doc, page, rev, size, rect, rect, level, Some(wanted), None);
}

fn dedup_keep_order(v: &mut Vec<TileKey>) {
    let mut seen = std::collections::HashSet::with_capacity(v.len());
    v.retain(|k| seen.insert(*k));
}
