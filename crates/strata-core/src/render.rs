//! Tile renderer. The UI publishes, once per frame and per view, the ordered list
//! of tiles it is missing; workers always take the most urgent tile that nobody is
//! working on. Tiles that scroll out of view simply disappear from the list, so
//! stale work is never started.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use mupdf::{Colorspace, Device, DisplayList, IRect, Matrix, Pixmap, Rect};
use parking_lot::{Condvar, Mutex};

use crate::doc::{DocClient, DocId, Waker};
use crate::geom::SizeF;

/// Tile edge in device pixels.
pub const TILE_PX: u32 = 512;
/// Zoom levels are quantised to quarter-octaves: scale = 2^(level/4) px per point.
pub const LEVELS_PER_OCTAVE: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub doc: DocId,
    pub page: u32,
    pub level: i16,
    pub tx: u16,
    pub ty: u16,
}

pub fn level_scale(level: i16) -> f32 {
    2f32.powf(level as f32 / LEVELS_PER_OCTAVE)
}

/// Smallest level whose scale is at least `scale` (never render blurrier than asked).
pub fn level_for_scale(scale: f32) -> i16 {
    (scale.max(1e-4).log2() * LEVELS_PER_OCTAVE - 1e-3).ceil().clamp(-40.0, 40.0) as i16
}

/// Page size in device pixels at `level`.
pub fn page_px(page: SizeF, level: i16) -> (u32, u32) {
    let s = level_scale(level);
    ((page.w * s).ceil().max(1.0) as u32, (page.h * s).ceil().max(1.0) as u32)
}

/// Number of tile columns and rows for a page at `level`.
pub fn tile_grid(page: SizeF, level: i16) -> (u16, u16) {
    let (w, h) = page_px(page, level);
    (w.div_ceil(TILE_PX).min(u16::MAX as u32) as u16, h.div_ceil(TILE_PX).min(u16::MAX as u32) as u16)
}

pub struct RenderedTile {
    pub key: TileKey,
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, premultiplied (opaque, so identical).
    pub rgba: Vec<u8>,
    pub error: Option<String>,
}

pub type ViewId = u64;

#[derive(Default)]
struct State {
    views: BTreeMap<ViewId, Vec<TileKey>>,
    docs: HashMap<DocId, DocClient>,
    in_flight: HashSet<TileKey>,
    /// Rendered but not yet drained by the UI; excluded from picking.
    delivered: HashSet<TileKey>,
    shutdown: bool,
}

impl State {
    /// The tile with the lowest position in any view's list wins; ties go to the
    /// view listed first. Views therefore share the workers fairly.
    fn pick(&self) -> Option<TileKey> {
        let longest = self.views.values().map(Vec::len).max().unwrap_or(0);
        for i in 0..longest {
            for list in self.views.values() {
                if let Some(k) = list.get(i)
                    && !self.in_flight.contains(k)
                    && !self.delivered.contains(k)
                    && self.docs.contains_key(&k.doc)
                {
                    return Some(*k);
                }
            }
        }
        None
    }
}

struct Inner {
    state: Mutex<State>,
    cv: Condvar,
    out: Mutex<Vec<RenderedTile>>,
    waker: Waker,
}

pub struct RenderPool {
    inner: Arc<Inner>,
    workers: Vec<JoinHandle<()>>,
}

impl RenderPool {
    pub fn new(threads: usize, waker: Waker) -> RenderPool {
        let inner = Arc::new(Inner { state: Mutex::new(State::default()), cv: Condvar::new(), out: Mutex::new(Vec::new()), waker });
        let workers = (0..threads.max(1))
            .map(|i| {
                let inner = inner.clone();
                thread::Builder::new().name(format!("strata-render-{i}")).spawn(move || worker(inner)).expect("spawn render worker")
            })
            .collect();
        RenderPool { inner, workers }
    }

    pub fn register(&self, client: DocClient) {
        self.inner.state.lock().docs.insert(client.id, client);
    }

    pub fn unregister(&self, doc: DocId) {
        let mut st = self.inner.state.lock();
        st.docs.remove(&doc);
        st.delivered.retain(|k| k.doc != doc);
        for list in st.views.values_mut() {
            list.retain(|k| k.doc != doc);
        }
    }

    /// Replace the wanted-tile list of a view (most urgent first).
    pub fn set_wanted(&self, view: ViewId, keys: Vec<TileKey>) {
        let mut st = self.inner.state.lock();
        if keys.is_empty() {
            st.views.remove(&view);
        } else {
            st.views.insert(view, keys);
            self.inner.cv.notify_all();
        }
    }

    pub fn remove_view(&self, view: ViewId) {
        self.inner.state.lock().views.remove(&view);
    }

    /// Take finished tiles. Call once per frame.
    pub fn drain(&self) -> Vec<RenderedTile> {
        let tiles = std::mem::take(&mut *self.inner.out.lock());
        if !tiles.is_empty() {
            let mut st = self.inner.state.lock();
            for t in &tiles {
                st.delivered.remove(&t.key);
            }
        }
        tiles
    }

    pub fn busy(&self) -> bool {
        !self.inner.state.lock().in_flight.is_empty()
    }
}

impl Drop for RenderPool {
    fn drop(&mut self) {
        self.inner.state.lock().shutdown = true;
        self.inner.cv.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn worker(inner: Arc<Inner>) {
    loop {
        let (key, client) = {
            let mut st = inner.state.lock();
            loop {
                if st.shutdown {
                    return;
                }
                if let Some(k) = st.pick() {
                    st.in_flight.insert(k);
                    let c = st.docs[&k.doc].clone();
                    break (k, c);
                }
                inner.cv.wait(&mut st);
            }
        };
        let result = client.display_list(key.page).and_then(|dl| render_tile(&dl, key));
        let tile = match result {
            Ok((width, height, rgba)) => RenderedTile { key, width, height, rgba, error: None },
            Err(e) => RenderedTile { key, width: 0, height: 0, rgba: Vec::new(), error: Some(e) },
        };
        {
            let mut st = inner.state.lock();
            st.in_flight.remove(&key);
            if st.docs.contains_key(&key.doc) {
                st.delivered.insert(key);
                inner.out.lock().push(tile);
            }
        }
        (inner.waker)();
    }
}

fn render_tile(dl: &DisplayList, key: TileKey) -> Result<(u32, u32, Vec<u8>), String> {
    let b = dl.bounds();
    let s = level_scale(key.level);
    let (pw, ph) = page_px(SizeF { w: b.x1 - b.x0, h: b.y1 - b.y0 }, key.level);
    let t = TILE_PX as i32;
    let x0 = key.tx as i32 * t;
    let y0 = key.ty as i32 * t;
    let x1 = (x0 + t).min(pw as i32);
    let y1 = (y0 + t).min(ph as i32);
    if x1 <= x0 || y1 <= y0 {
        return Err("tile outside page".into());
    }
    let ctm = Matrix::new(s, 0.0, 0.0, s, -b.x0 * s, -b.y0 * s);
    let irect = IRect { x0, y0, x1, y1 };
    let e = |e: mupdf::Error| e.to_string();
    let mut pix = Pixmap::new_with_rect(&Colorspace::device_rgb(), irect, false).map_err(e)?;
    pix.clear_with(255).map_err(e)?;
    {
        let dev = Device::from_pixmap(&pix).map_err(e)?;
        dl.run(&dev, &ctm, Rect { x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32 }).map_err(e)?;
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let n = pix.n() as usize;
    let stride = pix.stride() as usize;
    let src = pix.samples();
    let mut rgba = vec![255u8; w * h * 4];
    for y in 0..h {
        let row = &src[y * stride..y * stride + w * n];
        let dst = &mut rgba[y * w * 4..(y + 1) * w * 4];
        for (d, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(n)) {
            d[..3].copy_from_slice(&s[..3]);
        }
    }
    Ok((w as u32, h as u32, rgba))
}

/// Render a whole page to packed RGB (3 bytes per pixel, no padding) at `scale`
/// px per point, optionally rotated 90° clockwise. Used for printing and export.
pub fn render_page_rgb(dl: &DisplayList, scale: f32, rotate90: bool) -> Result<(u32, u32, Vec<u8>), String> {
    let b = dl.bounds();
    let (w, h) = ((b.x1 - b.x0) * scale, (b.y1 - b.y0) * scale);
    // Page space -> device: translate to origin, scale, then optionally rotate by 90°.
    let ctm = if rotate90 {
        // (x, y) -> (h - y, x) after scaling
        Matrix::new(0.0, scale, -scale, 0.0, h + b.y0 * scale, -b.x0 * scale)
    } else {
        Matrix::new(scale, 0.0, 0.0, scale, -b.x0 * scale, -b.y0 * scale)
    };
    let (dw, dh) = if rotate90 { (h, w) } else { (w, h) };
    let irect = IRect { x0: 0, y0: 0, x1: dw.ceil() as i32, y1: dh.ceil() as i32 };
    let e = |e: mupdf::Error| e.to_string();
    let mut pix = Pixmap::new_with_rect(&Colorspace::device_rgb(), irect, false).map_err(e)?;
    pix.clear_with(255).map_err(e)?;
    {
        let dev = Device::from_pixmap(&pix).map_err(e)?;
        dl.run(&dev, &ctm, Rect { x0: 0.0, y0: 0.0, x1: irect.x1 as f32, y1: irect.y1 as f32 }).map_err(e)?;
    }
    let (pw, ph) = (irect.x1 as usize, irect.y1 as usize);
    let stride = pix.stride() as usize;
    let n = pix.n() as usize;
    let src = pix.samples();
    let mut out = Vec::with_capacity(pw * ph * 3);
    for y in 0..ph {
        let row = &src[y * stride..y * stride + pw * n];
        if n == 3 {
            out.extend_from_slice(row);
        } else {
            for px in row.chunks_exact(n) {
                out.extend_from_slice(&px[..3]);
            }
        }
    }
    Ok((pw as u32, ph as u32, out))
}

/// Render a page-space rectangle of a display list to PNG bytes.
pub fn render_region_png(dl: &DisplayList, region: crate::geom::RectF, scale: f32) -> Result<(u32, u32, Vec<u8>), String> {
    let b = dl.bounds();
    let x0 = region.x0.max(b.x0);
    let y0 = region.y0.max(b.y0);
    let x1 = region.x1.min(b.x1);
    let y1 = region.y1.min(b.y1);
    if x1 <= x0 || y1 <= y0 {
        return Err("empty region".into());
    }
    let ctm = Matrix::new(scale, 0.0, 0.0, scale, -x0 * scale, -y0 * scale);
    let irect = IRect { x0: 0, y0: 0, x1: ((x1 - x0) * scale).ceil() as i32, y1: ((y1 - y0) * scale).ceil() as i32 };
    let e = |e: mupdf::Error| e.to_string();
    let mut pix = Pixmap::new_with_rect(&Colorspace::device_rgb(), irect, false).map_err(e)?;
    pix.clear_with(255).map_err(e)?;
    {
        let dev = Device::from_pixmap(&pix).map_err(e)?;
        dl.run(&dev, &ctm, Rect { x0: 0.0, y0: 0.0, x1: irect.x1 as f32, y1: irect.y1 as f32 }).map_err(e)?;
    }
    let mut png = Vec::new();
    pix.write_to(&mut png, mupdf::ImageFormat::PNG).map_err(e)?;
    Ok((irect.x1 as u32, irect.y1 as u32, png))
}
