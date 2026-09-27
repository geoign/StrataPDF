//! GPU texture cache for rendered tiles, shared by all views.

use std::collections::{BTreeSet, HashMap};

use egui::{ColorImage, TextureHandle, TextureOptions};
use strata_core::{DocId, RenderedTile, TileKey};

pub struct Tile {
    pub tex: TextureHandle,
    pub w: u32,
    pub h: u32,
    last_used: u64,
}

pub struct TileCache {
    tiles: HashMap<TileKey, Tile>,
    /// (doc, page) -> cached (level, tx, ty), for drawing fallbacks from other zoom levels.
    per_page: HashMap<(DocId, u32), BTreeSet<(i16, u16, u16)>>,
    errors: HashMap<(DocId, u32), String>,
    bytes: usize,
    pub budget_bytes: usize,
    frame: u64,
}

impl TileCache {
    pub fn new(budget_bytes: usize) -> Self {
        TileCache { tiles: HashMap::new(), per_page: HashMap::new(), errors: HashMap::new(), bytes: 0, budget_bytes, frame: 0 }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    pub fn insert(&mut self, ctx: &egui::Context, t: RenderedTile) {
        let k = t.key;
        if let Some(e) = t.error {
            self.errors.insert((k.doc, k.page), e);
            return;
        }
        let img = ColorImage::from_rgba_premultiplied([t.width as usize, t.height as usize], &t.rgba);
        let name = format!("tile-{}-{}-{}-{}-{}", k.doc.0, k.page, k.level, k.tx, k.ty);
        let tex = ctx.load_texture(name, img, TextureOptions::LINEAR);
        self.bytes += (t.width * t.height * 4) as usize;
        if let Some(old) = self.tiles.insert(k, Tile { tex, w: t.width, h: t.height, last_used: self.frame }) {
            self.bytes -= (old.w * old.h * 4) as usize;
        }
        self.per_page.entry((k.doc, k.page)).or_default().insert((k.level, k.tx, k.ty));
    }

    /// Look up a tile and mark it as used this frame.
    pub fn get(&mut self, k: &TileKey) -> Option<&Tile> {
        let f = self.frame;
        self.tiles.get_mut(k).map(|t| {
            t.last_used = f;
            &*t
        })
    }

    pub fn contains(&self, k: &TileKey) -> bool {
        self.tiles.contains_key(k)
    }

    pub fn error(&self, doc: DocId, page: u32) -> Option<&str> {
        self.errors.get(&(doc, page)).map(String::as_str)
    }

    /// Levels with at least one cached tile for a page, ascending.
    pub fn levels(&self, doc: DocId, page: u32) -> Vec<i16> {
        let mut v: Vec<i16> = self.per_page.get(&(doc, page)).map(|s| s.iter().map(|e| e.0).collect()).unwrap_or_default();
        v.dedup();
        v
    }

    pub fn forget_doc(&mut self, doc: DocId) {
        self.tiles.retain(|k, _| k.doc != doc);
        self.per_page.retain(|k, _| k.0 != doc);
        self.errors.retain(|k, _| k.0 != doc);
        self.bytes = self.tiles.values().map(|t| (t.w * t.h * 4) as usize).sum();
    }

    /// Drop least recently used tiles (never ones used this frame) until under budget.
    pub fn end_frame(&mut self) {
        if self.bytes <= self.budget_bytes {
            return;
        }
        let mut by_age: Vec<(u64, TileKey)> = self.tiles.iter().filter(|(_, t)| t.last_used < self.frame).map(|(k, t)| (t.last_used, *k)).collect();
        by_age.sort_unstable_by_key(|e| e.0);
        let target = self.budget_bytes * 9 / 10;
        for (_, k) in by_age {
            if self.bytes <= target {
                break;
            }
            if let Some(t) = self.tiles.remove(&k) {
                self.bytes -= (t.w * t.h * 4) as usize;
                if let Some(s) = self.per_page.get_mut(&(k.doc, k.page)) {
                    s.remove(&(k.level, k.tx, k.ty));
                    if s.is_empty() {
                        self.per_page.remove(&(k.doc, k.page));
                    }
                }
            }
        }
    }
}
