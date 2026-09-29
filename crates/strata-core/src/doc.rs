//! Document access. MuPDF documents are not thread-safe, so each open file gets
//! one owner thread that answers requests (display lists, text, links). Long
//! scans (page sizes, search) open their own instance of the file on their own
//! thread so they never delay interactive requests.
//!
//! Permission flags in encrypted PDFs are deliberately ignored: once a file can
//! be decrypted, everything the engine can do is allowed.

use std::num::NonZeroUsize;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, unbounded};
use lru::LruCache;
use mupdf::pdf::PdfDocument;
use mupdf::{DestinationKind, DisplayList, MetadataName, TextPageFlags};
use parking_lot::RwLock;

use crate::geom::{QuadF, RectF, SizeF};
use crate::text::PageText;

pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocId(pub u64);

static NEXT_DOC_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, thiserror::Error)]
pub enum OpenError {
    #[error("password required")]
    PasswordRequired,
    #[error("wrong password")]
    WrongPassword,
    #[error("{0}")]
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub struct DocInfo {
    pub path: PathBuf,
    pub format: String,
    pub encryption: String,
    pub title: String,
    pub author: String,
    pub subject: String,
    pub keywords: String,
    pub creator: String,
    pub producer: String,
    pub creation_date: String,
    pub mod_date: String,
    pub page_count: usize,
    pub is_pdf: bool,
    pub reflowable: bool,
    /// `/ViewerPreferences /Direction /R2L` (typical for vertical Japanese books).
    pub right_to_left: bool,
    /// `/PageLayout` name, e.g. `TwoPageRight`.
    pub page_layout: Option<String>,
    /// Repairs applied while opening (shown to the user).
    pub notes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct OutlineItem {
    pub title: String,
    pub target: Option<LinkTarget>,
    pub children: Vec<OutlineItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    /// Page index and optional vertical position (page space, y down).
    Page { page: u32, y: Option<f32> },
    Uri(String),
}

#[derive(Clone, Debug)]
pub struct LinkInfo {
    pub rect: RectF,
    pub target: LinkTarget,
}

#[derive(Clone, Debug)]
pub enum SearchEvent {
    Hits { page: u32, quads: Vec<QuadF> },
    Progress { done: usize, total: usize },
    Done,
    Error(String),
}

type Reply<T> = Sender<Result<T, String>>;

/// An annotation edit request.
#[derive(Clone, Debug)]
pub enum AnnotOp {
    Create { page: u32, spec: crate::annot::AnnotSpec },
    Modify { page: u32, id: i32, spec: crate::annot::AnnotSpec },
    Delete { page: u32, id: i32 },
    Undo,
    Redo,
}

/// Page affected by an edit, and the id of a created annotation.
#[derive(Clone, Copy, Debug)]
pub struct AnnotResult {
    pub page: Option<u32>,
    pub id: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct SaveOptions {
    pub path: PathBuf,
    /// Append changes to the original file instead of rewriting it.
    pub incremental: bool,
    /// Write without encryption (full saves only).
    pub decrypt: bool,
}

enum Cmd {
    DisplayList(u32, Reply<Arc<DisplayList>>, bool),
    Annots(u32, Reply<Arc<Vec<crate::annot::AnnotInfo>>>, bool),
    Edit(AnnotOp, Reply<AnnotResult>, bool),
    Save(SaveOptions, Reply<()>, bool),
    Text(u32, Reply<Arc<PageText>>, bool),
    Links(u32, Reply<Arc<Vec<LinkInfo>>>, bool),
    Outline(Reply<Arc<Vec<OutlineItem>>>, bool),
    Images(u32, Reply<Arc<Vec<crate::images::PageImage>>>, bool),
    SaveImage { page: u32, index: usize, as_jpeg: bool, path: PathBuf, reply: Reply<(u32, u32)> },
}

/// Result of an asynchronous request; poll it once per frame.
pub struct Pending<T> {
    rx: Receiver<Result<T, String>>,
    value: Option<Result<T, String>>,
}

impl<T> Pending<T> {
    pub fn poll(&mut self) -> Option<&Result<T, String>> {
        if self.value.is_none() {
            match self.rx.try_recv() {
                Ok(v) => self.value = Some(v),
                Err(TryRecvError::Disconnected) => self.value = Some(Err("document closed".into())),
                Err(TryRecvError::Empty) => {}
            }
        }
        self.value.as_ref()
    }
}

struct Shared {
    path: PathBuf,
    password: Option<String>,
    sizes: RwLock<Vec<SizeF>>,
    /// Number of leading pages whose size is exact (the rest are estimates).
    exact_sizes: AtomicUsize,
    revision: AtomicU64,
    closed: AtomicBool,
    waker: Waker,
    ocr: Arc<crate::ocr::OcrStore>,
    edit: Arc<EditState>,
    /// Present when display lists must not run on two threads at once.
    serial: Option<Arc<parking_lot::Mutex<()>>>,
}

/// JBIG2 images whose shared symbol dictionary (JBIG2Globals) is used by two
/// decoders at once corrupt the heap: jbig2dec updates reference counts of the
/// shared symbols without locking. Such files are rendered one tile at a time.
/// Image dictionaries are never inside compressed object streams, so the filter
/// name is visible in the raw file.
pub(crate) fn needs_serial_rendering(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else { return false };
    // SAFETY: read-only view of a file we do not modify.
    let Ok(map) = (unsafe { memmap2::Mmap::map(&file) }) else { return false };
    const NEEDLE: &[u8] = b"JBIG2Decode";
    let mut i = 0;
    while let Some(off) = map[i..].iter().position(|&b| b == b'J') {
        let at = i + off;
        if map[at..].starts_with(NEEDLE) {
            return true;
        }
        i = at + 1;
    }
    false
}

/// Edit bookkeeping shared between the document thread and the UI.
#[derive(Default)]
pub(crate) struct EditState {
    /// Per-page edit revision (part of tile keys, so edited pages re-render).
    page_revs: RwLock<std::collections::HashMap<u32, u32>>,
    dirty: AtomicBool,
    can_undo: AtomicBool,
    can_redo: AtomicBool,
}

/// Cheap, clonable request channel to a document thread (used by render workers).
#[derive(Clone)]
pub struct DocClient {
    pub id: DocId,
    tx: Sender<Cmd>,
    serial: Option<Arc<parking_lot::Mutex<()>>>,
}

impl DocClient {
    /// Hold while running this document's display lists on another thread.
    /// `None` unless the document needs it (see [`needs_serial_rendering`]).
    pub fn serialize(&self) -> Option<parking_lot::MutexGuard<'_, ()>> {
        self.serial.as_ref().map(|m| m.lock())
    }

    /// Blocking fetch of a page's display list.
    pub fn display_list(&self, page: u32) -> Result<Arc<DisplayList>, String> {
        let (tx, rx) = bounded(1);
        self.tx.send(Cmd::DisplayList(page, tx, false)).map_err(|_| "document closed".to_string())?;
        rx.recv().map_err(|_| "document closed".to_string())?
    }
}

pub struct Document {
    id: DocId,
    info: Arc<DocInfo>,
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
}

impl Drop for Document {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
    }
}

pub(crate) enum Engine {
    Pdf(PdfDocument),
    Other(mupdf::Document),
}

impl Deref for Engine {
    type Target = mupdf::Document;
    fn deref(&self) -> &mupdf::Document {
        match self {
            Engine::Pdf(p) => p,
            Engine::Other(d) => d,
        }
    }
}

pub(crate) fn open_engine(path: &Path, password: Option<&str>) -> Result<(Engine, Vec<String>), OpenError> {
    let p = path.to_string_lossy();
    let mut doc = mupdf::Document::open(p.as_ref()).map_err(|e| OpenError::Failed(e.to_string()))?;
    if doc.needs_password().unwrap_or(false) {
        match password {
            None => return Err(OpenError::PasswordRequired),
            Some(pw) => {
                if !doc.authenticate(pw).unwrap_or(false) {
                    return Err(OpenError::WrongPassword);
                }
            }
        }
    }
    if doc.is_pdf() {
        let mut pdf = PdfDocument::try_from(doc).map_err(|e| OpenError::Failed(e.to_string()))?;
        let mut notes = Vec::new();
        if pdf.page_count().unwrap_or(0) <= 0 {
            match rebuild_page_tree(&mut pdf) {
                Ok(n) if n > 0 => notes.push(format!("ページツリーが壊れていたため、残存する {n} ページから再構築しました")),
                Ok(_) => {}
                Err(e) => notes.push(format!("ページツリーの再構築に失敗しました: {e}")),
            }
        }
        Ok((Engine::Pdf(pdf), notes))
    } else {
        Ok((Engine::Other(doc), Vec::new()))
    }
}

/// When the page tree is lost (truncated or corrupted file) but page objects
/// survive, rebuild a flat page tree from every `/Type /Page` object, in object
/// number order. The change lives only in memory.
fn rebuild_page_tree(pdf: &mut PdfDocument) -> Result<usize, mupdf::Error> {
    use mupdf::pdf::PdfObject;
    let is_name = |o: &PdfObject, key: &str, v: &[u8]| {
        o.get_dict(key).ok().flatten().and_then(|t| t.as_name().ok()).as_deref() == Some(v)
    };
    let n = pdf.xref_len()? as i32;
    let mut pages = Vec::new();
    let mut default_box = None;
    for i in 1..n {
        let Ok(Some(o)) = pdf.xref_object(i) else { continue };
        if o.is_dict().unwrap_or(false) && is_name(&o, "Type", b"Page") {
            if default_box.is_none() {
                default_box = o.get_dict("MediaBox").ok().flatten();
            }
            pages.push(i);
        }
    }
    if pages.is_empty() {
        return Ok(0);
    }
    let mut tree = pdf.new_dict()?;
    tree.dict_put("Type", PdfObject::new_name("Pages")?)?;
    let tree_ref = pdf.add_object(&tree)?;
    let mut kids = pdf.new_array()?;
    for &i in &pages {
        let r = pdf.new_indirect(i, 0)?;
        if let Some(mut page) = r.resolve()? {
            page.dict_put("Parent", tree_ref.try_clone()?)?;
            if page.get_dict_inheritable("MediaBox")?.is_none() {
                let mb = match &default_box {
                    Some(b) => b.try_clone()?,
                    None => pdf.new_object_from_str("[0 0 595 842]")?,
                };
                page.dict_put("MediaBox", mb)?;
            }
        }
        kids.array_push(r)?;
    }
    if let Some(mut t) = tree_ref.resolve()? {
        t.dict_put("Kids", kids)?;
        t.dict_put("Count", PdfObject::new_int(pages.len() as i32)?)?;
    }
    let catalog = pdf.catalog().ok().filter(|c| c.is_dict().unwrap_or(false));
    match catalog {
        Some(mut c) => c.dict_put("Pages", tree_ref)?,
        None => {
            let mut c = pdf.new_dict()?;
            c.dict_put("Type", PdfObject::new_name("Catalog")?)?;
            c.dict_put("Pages", tree_ref)?;
            let cref = pdf.add_object(&c)?;
            pdf.trailer()?.dict_put("Root", cref)?;
        }
    }
    Ok(pages.len())
}

fn page_size(doc: &mupdf::Document, page: i32) -> Option<SizeF> {
    let b = doc.load_page(page).ok()?.bounds().ok()?;
    let s = SizeF { w: b.width(), h: b.height() };
    (s.w > 0.0 && s.h > 0.0).then_some(s)
}

fn read_info(path: &Path, eng: &Engine) -> DocInfo {
    let m = |n: MetadataName| eng.metadata(n).unwrap_or_default();
    let mut info = DocInfo {
        path: path.to_path_buf(),
        format: m(MetadataName::Format),
        encryption: m(MetadataName::Encryption),
        title: m(MetadataName::Title),
        author: m(MetadataName::Author),
        subject: m(MetadataName::Subject),
        keywords: m(MetadataName::Keywords),
        creator: m(MetadataName::Creator),
        producer: m(MetadataName::Producer),
        creation_date: m(MetadataName::CreationDate),
        mod_date: m(MetadataName::ModDate),
        page_count: eng.page_count().unwrap_or(0).max(0) as usize,
        is_pdf: matches!(eng, Engine::Pdf(_)),
        reflowable: eng.is_reflowable().unwrap_or(false),
        ..Default::default()
    };
    if let Engine::Pdf(pdf) = eng {
        let name = |o: Option<mupdf::pdf::PdfObject>| {
            o.and_then(|o| o.as_name().ok()).map(|n| String::from_utf8_lossy(&n).into_owned())
        };
        if let Ok(cat) = pdf.catalog() {
            let vp = cat.get_dict("ViewerPreferences").ok().flatten();
            let dir = name(vp.and_then(|v| v.get_dict("Direction").ok().flatten()));
            info.right_to_left = dir.as_deref() == Some("R2L");
            info.page_layout = name(cat.get_dict("PageLayout").ok().flatten());
        }
    }
    info
}

fn dest_target(page: u32, kind: DestinationKind) -> LinkTarget {
    let y = match kind {
        DestinationKind::XYZ { top, .. } | DestinationKind::FitH { top } | DestinationKind::FitBH { top } => top,
        DestinationKind::FitR { top, bottom, .. } => Some(top.min(bottom)),
        _ => None,
    };
    LinkTarget::Page { page, y }
}

fn convert_outline(items: Vec<mupdf::Outline>) -> Vec<OutlineItem> {
    items
        .into_iter()
        .map(|o| {
            let target = match (o.dest, o.uri) {
                (Some(d), _) => Some(dest_target(d.loc.page_number, d.kind)),
                (None, Some(u)) if !u.is_empty() => Some(LinkTarget::Uri(u)),
                _ => None,
            };
            OutlineItem { title: o.title, target, children: convert_outline(o.down) }
        })
        .collect()
}

impl Document {
    /// Opens a document. Blocks until the file is parsed (can take seconds for a
    /// damaged multi-hundred-MB file), so call it off the UI thread.
    pub fn open(path: &Path, password: Option<String>, waker: Waker) -> Result<Document, OpenError> {
        let id = DocId(NEXT_DOC_ID.fetch_add(1, Ordering::Relaxed));
        let (init_tx, init_rx) = bounded::<Result<(DocInfo, SizeF), OpenError>>(1);
        let (tx, rx) = unbounded::<Cmd>();
        let ocr_store = Arc::new(crate::ocr::OcrStore::open(path));
        let thread_ocr = ocr_store.clone();
        let edit = Arc::new(EditState::default());
        let thread_edit = edit.clone();
        let serial = needs_serial_rendering(path).then(|| Arc::new(parking_lot::Mutex::new(())));
        let thread_serial = serial.clone();
        let path_buf = path.to_path_buf();
        let pw = password.clone();
        let wk = waker.clone();
        thread::Builder::new()
            .name(format!("strata-doc-{}", id.0))
            .spawn(move || {
                let (eng, notes) = match open_engine(&path_buf, pw.as_deref()) {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };
                let mut info = read_info(&path_buf, &eng);
                info.notes = notes;
                let first = page_size(&eng, 0).unwrap_or(SizeF { w: 595.0, h: 842.0 });
                if init_tx.send(Ok((info, first))).is_err() {
                    return;
                }
                doc_thread(eng, rx, wk, thread_ocr, thread_edit, thread_serial);
            })
            .map_err(|e| OpenError::Failed(e.to_string()))?;

        let (info, first) = init_rx.recv().map_err(|_| OpenError::Failed("document thread died".into()))??;
        let shared = Arc::new(Shared {
            serial,
            path: path.to_path_buf(),
            password,
            sizes: RwLock::new(vec![first; info.page_count]),
            exact_sizes: AtomicUsize::new(1.min(info.page_count)),
            revision: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            waker,
            ocr: ocr_store,
            edit,
        });
        if info.page_count > 1 {
            let sh = shared.clone();
            thread::Builder::new()
                .name(format!("strata-sizes-{}", id.0))
                .spawn(move || scan_sizes(sh))
                .ok();
        }
        Ok(Document { id, info: Arc::new(info), shared, tx })
    }

    pub fn id(&self) -> DocId {
        self.id
    }
    pub fn info(&self) -> &Arc<DocInfo> {
        &self.info
    }
    pub fn page_count(&self) -> usize {
        self.info.page_count
    }
    pub(crate) fn password(&self) -> Option<String> {
        self.shared.password.clone()
    }
    pub fn annotations(&self, page: u32) -> Pending<Arc<Vec<crate::annot::AnnotInfo>>> {
        self.request(|r| Cmd::Annots(page, r, true))
    }
    pub fn edit(&self, op: AnnotOp) -> Pending<AnnotResult> {
        self.request(|r| Cmd::Edit(op, r, true))
    }
    pub fn save(&self, opts: SaveOptions) -> Pending<()> {
        self.request(|r| Cmd::Save(opts, r, true))
    }
    /// Edit revision of a page (0 until the page is edited).
    pub fn page_rev(&self, page: u32) -> u32 {
        self.shared.edit.page_revs.read().get(&page).copied().unwrap_or(0)
    }
    pub fn is_dirty(&self) -> bool {
        self.shared.edit.dirty.load(Ordering::Acquire)
    }
    pub fn can_undo(&self) -> bool {
        self.shared.edit.can_undo.load(Ordering::Acquire)
    }
    pub fn can_redo(&self) -> bool {
        self.shared.edit.can_redo.load(Ordering::Acquire)
    }

    /// OCR results of this document (shared with background jobs).
    pub fn ocr_store(&self) -> &Arc<crate::ocr::OcrStore> {
        &self.shared.ocr
    }
    pub(crate) fn waker(&self) -> Waker {
        self.shared.waker.clone()
    }
    pub fn client(&self) -> DocClient {
        DocClient { id: self.id, tx: self.tx.clone(), serial: self.shared.serial.clone() }
    }

    /// Bumped whenever page sizes change; re-layout when it differs from the last seen value.
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }
    pub fn sizes_complete(&self) -> bool {
        self.shared.exact_sizes.load(Ordering::Acquire) >= self.info.page_count
    }
    pub fn page_sizes(&self) -> Vec<SizeF> {
        self.shared.sizes.read().clone()
    }

    fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Cmd) -> Pending<T> {
        let (tx, rx) = bounded(1);
        let _ = self.tx.send(make(tx));
        Pending { rx, value: None }
    }

    pub fn text(&self, page: u32) -> Pending<Arc<PageText>> {
        self.request(|r| Cmd::Text(page, r, true))
    }
    pub fn links(&self, page: u32) -> Pending<Arc<Vec<LinkInfo>>> {
        self.request(|r| Cmd::Links(page, r, true))
    }
    pub fn outline(&self) -> Pending<Arc<Vec<OutlineItem>>> {
        self.request(|r| Cmd::Outline(r, true))
    }
    /// Images drawn on a page, in drawing order.
    pub fn images(&self, page: u32) -> Pending<Arc<Vec<crate::images::PageImage>>> {
        self.request(|r| Cmd::Images(page, r, true))
    }
    /// Save the `index`-th image of `images(page)` to a file (see [`crate::images::save`]).
    pub fn save_image(&self, page: u32, index: usize, as_jpeg: bool, path: PathBuf) -> Pending<(u32, u32)> {
        self.request(|reply| Cmd::SaveImage { page, index, as_jpeg, path, reply })
    }

    /// Full-text search on a separate document instance. Stops when `cancel` is set
    /// or the receiver is dropped.
    pub fn search(&self, needle: String, start_page: u32, cancel: Arc<AtomicBool>) -> Receiver<SearchEvent> {
        let (tx, rx) = unbounded();
        let sh = self.shared.clone();
        let total = self.info.page_count;
        thread::Builder::new()
            .name(format!("strata-search-{}", self.id.0))
            .spawn(move || {
                let eng = match open_engine(&sh.path, sh.password.as_deref()) {
                    Ok((e, _)) => e,
                    Err(e) => {
                        let _ = tx.send(SearchEvent::Error(e.to_string()));
                        (sh.waker)();
                        return;
                    }
                };
                let start = (start_page as usize).min(total.saturating_sub(1));
                let mut last_wake = Instant::now();
                for (done, p) in (start..total).chain(0..start).enumerate() {
                    if cancel.load(Ordering::Relaxed) || sh.closed.load(Ordering::Relaxed) {
                        return;
                    }
                    let mut quads: Vec<QuadF> = Vec::new();
                    let forced = sh.ocr.get(p as u32).filter(|o| o.forced);
                    if let Some(o) = &forced {
                        quads = o.search(&needle);
                    } else if let Ok(page) = eng.load_page(p as i32)
                        && let Ok(hits) = page.search(&needle, 4096)
                    {
                        quads = hits.iter().map(|q| QuadF::from(q.clone())).collect();
                    }
                    if quads.is_empty()
                        && let Some(o) = sh.ocr.get(p as u32)
                    {
                        quads = o.search(&needle);
                    }
                    if !quads.is_empty() {
                        if tx.send(SearchEvent::Hits { page: p as u32, quads }).is_err() {
                            return;
                        }
                        (sh.waker)();
                    }
                    if last_wake.elapsed() > Duration::from_millis(100) {
                        let _ = tx.send(SearchEvent::Progress { done: done + 1, total });
                        (sh.waker)();
                        last_wake = Instant::now();
                    }
                }
                let _ = tx.send(SearchEvent::Done);
                (sh.waker)();
            })
            .ok();
        rx
    }
}

fn scan_sizes(sh: Arc<Shared>) {
    let Ok((eng, _)) = open_engine(&sh.path, sh.password.as_deref()) else { return };
    let n = sh.sizes.read().len();
    let mut batch: Vec<SizeF> = Vec::new();
    let mut batch_start = 1;
    let mut last_flush = Instant::now();
    let flush = |batch: &mut Vec<SizeF>, start: usize| {
        let mut sizes = sh.sizes.write();
        let mut changed = false;
        for (i, s) in batch.drain(..).enumerate() {
            if sizes[start + i] != s {
                sizes[start + i] = s;
                changed = true;
            }
        }
        drop(sizes);
        if changed {
            sh.revision.fetch_add(1, Ordering::AcqRel);
            (sh.waker)();
        }
    };
    let fallback = sh.sizes.read()[0];
    for p in 1..n {
        if sh.closed.load(Ordering::Relaxed) {
            return;
        }
        batch.push(page_size(&eng, p as i32).unwrap_or(fallback));
        if last_flush.elapsed() > Duration::from_millis(150) {
            let len = batch.len();
            flush(&mut batch, batch_start);
            batch_start += len;
            sh.exact_sizes.store(batch_start, Ordering::Release);
            last_flush = Instant::now();
        }
    }
    flush(&mut batch, batch_start);
    sh.exact_sizes.store(n, Ordering::Release);
    (sh.waker)();
}

/// The PDF's own text is unusable: almost no characters, or mostly undecodable ones.
fn poor_text(t: &PageText) -> bool {
    let (mut n, mut bad) = (0usize, 0usize);
    for (_, _, l) in t.lines() {
        for c in &l.chars {
            if c.c.is_whitespace() {
                continue;
            }
            n += 1;
            let u = c.c as u32;
            if c.c == '\u{FFFD}' || u < 0x20 || (0xE000..=0xF8FF).contains(&u) {
                bad += 1;
            }
        }
    }
    n < 20 || bad * 10 > n * 3
}

fn doc_thread(mut eng: Engine, rx: Receiver<Cmd>, waker: Waker, ocr: Arc<crate::ocr::OcrStore>, edit: Arc<EditState>, serial: Option<Arc<parking_lot::Mutex<()>>>) {
    let mut history = crate::annot::History::default();
    let cap = |n| NonZeroUsize::new(n).unwrap();
    let mut dl_cache: LruCache<u32, Arc<DisplayList>> = LruCache::new(cap(64));
    let mut text_cache: LruCache<u32, Arc<PageText>> = LruCache::new(cap(256));
    let mut link_cache: LruCache<u32, Arc<Vec<LinkInfo>>> = LruCache::new(cap(256));
    let mut outline: Option<Arc<Vec<OutlineItem>>> = None;
    let mut image_cache: LruCache<u32, Arc<Vec<crate::images::PageImage>>> = LruCache::new(cap(64));
    // Structured text that keeps image references (for listing and saving images).
    let image_page = |eng: &Engine, dl_cache: &mut LruCache<u32, Arc<DisplayList>>, p: u32| match dl_cache.get(&p) {
        Some(dl) => dl.to_text_page(crate::images::flags()),
        None => eng.load_page(p as i32).and_then(|pg| pg.to_text_page(crate::images::flags())),
    };

    fn send<T>(r: Reply<T>, v: Result<T, String>, wake: bool, waker: &Waker) {
        if r.send(v).is_ok() && wake {
            waker();
        }
    }

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::DisplayList(p, r, wake) => {
                let v = if let Some(dl) = dl_cache.get(&p) {
                    Ok(dl.clone())
                } else {
                    let res = eng
                        .load_page(p as i32)
                        .and_then(|pg| pg.to_display_list(true))
                        .map(Arc::new)
                        .map_err(|e| e.to_string());
                    if let Ok(dl) = &res {
                        dl_cache.put(p, dl.clone());
                    }
                    res
                };
                send(r, v, wake, &waker);
            }
            Cmd::Text(p, r, wake) => {
                let v = if let Some(t) = text_cache.get(&p) {
                    Ok(t.clone())
                } else {
                    // Reuse the display list when we have it: avoids re-interpreting the page.
                    let tp = match dl_cache.get(&p) {
                        Some(dl) => dl.to_text_page(TextPageFlags::empty()),
                        None => eng.load_page(p as i32).and_then(|pg| pg.to_text_page(TextPageFlags::empty())),
                    };
                    let res = tp.map(|tp| Arc::new(PageText::from_text_page(&tp))).map_err(|e| e.to_string());
                    if let Ok(t) = &res {
                        text_cache.put(p, t.clone());
                    }
                    res
                };
                // OCR text stands in for an unusable text layer.
                let v = match (&v, ocr.get(p)) {
                    (Ok(t), Some(o)) if o.forced || poor_text(t) => Ok(Arc::new(o.to_page_text())),
                    (Err(_), Some(o)) => Ok(Arc::new(o.to_page_text())),
                    _ => v,
                };
                send(r, v, wake, &waker);
            }
            Cmd::Links(p, r, wake) => {
                let v = if let Some(l) = link_cache.get(&p) {
                    Ok(l.clone())
                } else {
                    let res = eng.load_page(p as i32).and_then(|pg| pg.links()).map(|it| {
                        Arc::new(
                            it.filter_map(|l| {
                                let target = match l.dest {
                                    Some(d) => dest_target(d.loc.page_number, d.kind),
                                    None if !l.uri.is_empty() => LinkTarget::Uri(l.uri),
                                    None => return None,
                                };
                                Some(LinkInfo { rect: l.bounds.into(), target })
                            })
                            .collect::<Vec<_>>(),
                        )
                    });
                    let res = res.map_err(|e| e.to_string());
                    if let Ok(l) = &res {
                        link_cache.put(p, l.clone());
                    }
                    res
                };
                send(r, v, wake, &waker);
            }
            Cmd::Annots(p, r, wake) => {
                let v = match &eng {
                    Engine::Pdf(pdf) => crate::annot::list(pdf, p).map(Arc::new).map_err(|e| e.to_string()),
                    Engine::Other(_) => Ok(Arc::new(Vec::new())),
                };
                send(r, v, wake, &waker);
            }
            Cmd::Edit(op, r, wake) => {
                let v = match &mut eng {
                    Engine::Pdf(pdf) => {
                        use crate::annot::Edit;
                        let res: Result<AnnotResult, mupdf::Error> = (|| {
                            Ok(match op {
                                AnnotOp::Create { page, spec } => {
                                    let id = crate::annot::create(pdf, page, &spec)?;
                                    history.push(Edit::Created { page, id, spec });
                                    AnnotResult { page: Some(page), id: Some(id) }
                                }
                                AnnotOp::Modify { page, id, spec } => {
                                    let before = crate::annot::modify(pdf, page, id, &spec)?;
                                    history.push(Edit::Modified { page, id, before, after: spec });
                                    AnnotResult { page: Some(page), id: Some(id) }
                                }
                                AnnotOp::Delete { page, id } => {
                                    let spec = crate::annot::delete(pdf, page, id)?;
                                    history.push(Edit::Deleted { page, id, spec });
                                    AnnotResult { page: Some(page), id: None }
                                }
                                AnnotOp::Undo => AnnotResult { page: history.undo(pdf)?, id: None },
                                AnnotOp::Redo => AnnotResult { page: history.redo(pdf)?, id: None },
                            })
                        })();
                        if let Ok(AnnotResult { page: Some(p), .. }) = &res {
                            dl_cache.pop(p);
                            image_cache.pop(p);
                            *edit.page_revs.write().entry(*p).or_insert(0) += 1;
                            edit.dirty.store(true, Ordering::Release);
                        }
                        edit.can_undo.store(history.can_undo(), Ordering::Release);
                        edit.can_redo.store(history.can_redo(), Ordering::Release);
                        res.map_err(|e| e.to_string())
                    }
                    Engine::Other(_) => Err("注釈は PDF にのみ追加できます".into()),
                };
                send(r, v, wake, &waker);
            }
            Cmd::Save(opts, r, wake) => {
                let v = match &eng {
                    Engine::Pdf(pdf) => {
                        let mut w = mupdf::pdf::PdfWriteOptions::default();
                        if opts.incremental {
                            w.set_incremental(true);
                        } else {
                            w.set_garbage_level(1).set_compress(true);
                            // MuPDF's default keeps the encryption.
                            w.set_encryption(if opts.decrypt { mupdf::pdf::Encryption::None } else { mupdf::pdf::Encryption::Keep });
                        }
                        let res = pdf.save_with_options(&opts.path.to_string_lossy(), w).map_err(|e| e.to_string());
                        if res.is_ok() {
                            edit.dirty.store(false, Ordering::Release);
                        }
                        res
                    }
                    Engine::Other(_) => Err("PDF 以外は保存できません".into()),
                };
                send(r, v, wake, &waker);
            }
            Cmd::Outline(r, wake) => {
                let v = match &outline {
                    Some(o) => Ok(o.clone()),
                    None => {
                        let o = Arc::new(eng.outlines().map(convert_outline).unwrap_or_default());
                        outline = Some(o.clone());
                        Ok(o)
                    }
                };
                send(r, v, wake, &waker);
            }
            Cmd::Images(p, r, wake) => {
                let v = match image_cache.get(&p) {
                    Some(l) => Ok(l.clone()),
                    None => {
                        let res = image_page(&eng, &mut dl_cache, p).map(|tp| Arc::new(crate::images::list(&tp))).map_err(|e| e.to_string());
                        if let Ok(l) = &res {
                            image_cache.put(p, l.clone());
                        }
                        res
                    }
                };
                send(r, v, wake, &waker);
            }
            Cmd::SaveImage { page, index, as_jpeg, path, reply } => {
                // Decodes the image, like a render worker.
                let _serial = serial.as_ref().map(|m| m.lock());
                let v = image_page(&eng, &mut dl_cache, page).map_err(|e| e.to_string()).and_then(|tp| crate::images::save(&tp, index, as_jpeg, &path));
                send(reply, v, true, &waker);
            }
        }
    }
}
