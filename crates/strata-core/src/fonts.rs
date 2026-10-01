//! Resolution of fonts that a document references but does not embed.
//!
//! The stock `font-kit` loader of the `mupdf` crate enumerates every installed
//! font on each lookup (about a second per unknown font on Windows). This loader
//! indexes the font directories once, in the background, by PostScript, full and
//! family names in every language the font carries (so `ＭＳ 明朝` and `MS-Mincho`
//! both resolve), and maps generic Japanese font names such as `Ryumin-Light`
//! or `GothicBBB-Medium` to installed Mincho/Gothic faces.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use mupdf::{CjkFontOrdering, Font, FontHints, FontLoader};
use parking_lot::Mutex;

#[derive(Clone, Debug)]
struct Face {
    path: Arc<Path>,
    index: u32,
    bold: bool,
    italic: bool,
}

#[derive(Default)]
struct Index {
    by_name: HashMap<String, Vec<Face>>,
    families: Vec<FontFamily>,
}

/// An installed font family, for font pickers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFamily {
    /// English family name (what CSS and DirectWrite match).
    pub name: String,
    /// Japanese family name when the font has one, else `name`.
    pub display: String,
    /// Has kana and kanji.
    pub japanese: bool,
}

static INDEX: OnceLock<Index> = OnceLock::new();
static BUILDING: Mutex<()> = Mutex::new(());

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(w) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(w).join("Fonts"));
    }
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(l).join("Microsoft").join("Windows").join("Fonts"));
    }
    dirs
}

/// Lowercase, drop separators, fold full-width ASCII to half-width.
fn normalize(name: &str) -> String {
    name.chars()
        .filter_map(|c| {
            let c = match c {
                '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
                '\u{3000}' => ' ',
                _ => c,
            };
            (!matches!(c, ' ' | '-' | '_' | ',' | '+' | '.')).then(|| c.to_lowercase()).map(|l| l.collect::<String>())
        })
        .collect()
}

/// Subset fonts are named `ABCDEF+RealName`.
fn strip_subset(name: &str) -> &str {
    match name.split_once('+') {
        Some((tag, rest)) if tag.len() == 6 && tag.bytes().all(|b| b.is_ascii_uppercase()) => rest,
        _ => name,
    }
}

fn index_file(path: &Path, out: &mut Vec<(String, Face)>, fams: &mut Vec<FontFamily>) {
    let Ok(file) = std::fs::File::open(path) else { return };
    // SAFETY: font files are not modified while we read their name tables.
    let Ok(map) = (unsafe { memmap2::Mmap::map(&file) }) else { return };
    let n = ttf_parser::fonts_in_collection(&map).unwrap_or(1);
    let path: Arc<Path> = Arc::from(path);
    for index in 0..n {
        let Ok(face) = ttf_parser::Face::parse(&map, index) else { continue };
        let f = Face { path: path.clone(), index, bold: face.is_bold() || face.weight().to_number() >= 600, italic: face.is_italic() || face.is_oblique() };
        if let Some(fam) = family_of(&face) {
            fams.push(fam);
        }
        for rec in face.names() {
            use ttf_parser::name_id::*;
            if !matches!(rec.name_id, FAMILY | FULL_NAME | POST_SCRIPT_NAME | TYPOGRAPHIC_FAMILY) {
                continue;
            }
            if let Some(s) = rec.to_string() {
                out.push((normalize(&s), f.clone()));
            }
        }
    }
}

/// The family a face belongs to; the typographic family (name ID 16) groups the
/// weights that the legacy family name (ID 1) splits into separate families.
fn family_of(face: &ttf_parser::Face) -> Option<FontFamily> {
    use ttf_parser::Language;
    use ttf_parser::name_id::{FAMILY, TYPOGRAPHIC_FAMILY};
    let get = |id: u16, ja: bool| {
        let want = if ja { Language::Japanese_Japan } else { Language::English_UnitedStates };
        // Mac-platform records share the language but do not decode to a string.
        face.names().into_iter().filter(|r| r.name_id == id && r.language() == want).find_map(|r| r.to_string())
    };
    let name = get(TYPOGRAPHIC_FAMILY, false).or_else(|| get(FAMILY, false))?;
    // Vertical-writing aliases of CJK fonts.
    if name.starts_with('@') {
        return None;
    }
    let display = get(TYPOGRAPHIC_FAMILY, true).or_else(|| get(FAMILY, true)).unwrap_or_else(|| name.clone());
    let japanese = face.glyph_index('あ').is_some() && face.glyph_index('漢').is_some();
    Some(FontFamily { name, display, japanese })
}

fn build_index() -> Index {
    let mut files = Vec::new();
    for dir in font_dirs() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
                if matches!(ext.as_deref(), Some("ttf" | "otf" | "ttc" | "otc")) {
                    files.push(p);
                }
            }
        }
    }
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let chunk = files.len().div_ceil(threads).max(1);
    let parts: Vec<(Vec<(String, Face)>, Vec<FontFamily>)> = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|c| {
                s.spawn(move || {
                    let (mut out, mut fams) = (Vec::new(), Vec::new());
                    for p in c {
                        index_file(p, &mut out, &mut fams);
                    }
                    (out, fams)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut idx = Index::default();
    for (names, fams) in parts {
        for (name, face) in names {
            let v = idx.by_name.entry(name).or_default();
            if !v.iter().any(|f| f.path == face.path && f.index == face.index) {
                v.push(face);
            }
        }
        idx.families.extend(fams);
    }
    idx.families.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    idx.families.dedup_by(|a, b| {
        let same = a.name == b.name;
        // A family is Japanese if any of its faces is.
        b.japanese |= same && a.japanese;
        same
    });
    idx
}

fn index() -> &'static Index {
    if let Some(i) = INDEX.get() {
        return i;
    }
    let _g = BUILDING.lock();
    INDEX.get_or_init(build_index)
}

/// Start indexing in the background and register the loader with MuPDF.
/// Call once at startup, before documents are opened.
pub fn install() {
    std::thread::Builder::new()
        .name("strata-font-index".into())
        .spawn(|| {
            index();
        })
        .ok();
    mupdf::set_font_loader(IndexedFontLoader::default());
}

/// First installed face matching one of `names` (any language), as (file, face index).
/// Blocks until the index is built.
pub fn find_face(names: &[&str]) -> Option<(PathBuf, u32)> {
    let idx = index();
    names.iter().find_map(|n| {
        let f = IndexedFontLoader::pick(idx.by_name.get(&normalize(n))?, false, false);
        Some((f.path.to_path_buf(), f.index))
    })
}

/// Installed font families sorted by name. Blocks until the index is built.
pub fn families() -> &'static [FontFamily] {
    &index().families
}

#[derive(Default)]
pub struct IndexedFontLoader {
    data: Mutex<HashMap<Arc<Path>, Arc<Vec<u8>>>>,
}

impl IndexedFontLoader {
    fn load_face(&self, f: &Face, name: &str) -> Option<Font> {
        let data = {
            let mut cache = self.data.lock();
            match cache.get(&f.path) {
                Some(d) => d.clone(),
                None => {
                    let d = Arc::new(std::fs::read(&f.path).ok()?);
                    cache.insert(f.path.clone(), d.clone());
                    d
                }
            }
        };
        Font::from_bytes_with_index(name, f.index as i32, &data).ok()
    }

    fn pick<'a>(faces: &'a [Face], bold: bool, italic: bool) -> &'a Face {
        faces.iter().max_by_key(|f| (f.bold == bold) as u8 * 2 + (f.italic == italic) as u8).unwrap_or(&faces[0])
    }

    fn by_names(&self, names: &[&str], bold: bool, italic: bool) -> Option<Font> {
        let idx = index();
        names.iter().find_map(|n| {
            let faces = idx.by_name.get(&normalize(n))?;
            self.load_face(Self::pick(faces, bold, italic), n)
        })
    }

    fn japanese(&self, serif: bool, bold: bool) -> Option<Font> {
        if serif {
            self.by_names(&["Yu Mincho", "MS Mincho", "BIZ UDMincho", "Noto Serif JP"], bold, false)
        } else {
            self.by_names(&["Yu Gothic", "Meiryo", "MS Gothic", "BIZ UDGothic", "Noto Sans JP"], bold, false)
        }
    }
}

/// Generic Japanese font families that are rarely installed on Windows.
fn japanese_generic(n: &str) -> Option<bool> {
    const SERIF: &[&str] = &["mincho", "明朝", "ryumin", "heiseimin", "kozmin", "minchobbb", "ipamin", "ipaexmin", "hiramin", "hiraginomin", "shinsei", "kaisho", "楷書", "教科書"];
    const SANS: &[&str] = &["gothic", "ゴシック", "kakugo", "heiseikaku", "kozgo", "gothicbbb", "futogo", "midashigo", "jun", "maru", "丸", "ipagothic", "ipaexgothic", "hirakaku", "hiraginokaku", "meiryo", "角"];
    if SERIF.iter().any(|k| n.contains(k)) {
        Some(true)
    } else if SANS.iter().any(|k| n.contains(k)) {
        Some(false)
    } else {
        None
    }
}

impl FontLoader for IndexedFontLoader {
    fn load_font(&self, name: &str, hints: FontHints) -> Option<Font> {
        let idx = index();
        let raw = strip_subset(name);
        let mut bold = hints.bold;
        let mut italic = hints.italic;
        let n = normalize(raw);
        let mut candidates = vec![n.clone()];
        for suf in ["mt", "ps", "identityh", "identityv"] {
            if let Some(s) = n.strip_suffix(suf) {
                candidates.push(s.to_string());
            }
        }
        // "Arial,BoldItalic", "TimesNewRomanPS-BoldMT": split family and style.
        if let Some(pos) = raw.find([',', '-']) {
            let style = raw[pos + 1..].to_ascii_lowercase();
            bold |= ["bold", "black", "heavy", "semibold", "demi"].iter().any(|k| style.contains(k));
            italic |= style.contains("italic") || style.contains("oblique");
            let fam = normalize(&raw[..pos]);
            candidates.push(fam.strip_suffix("ps").unwrap_or(&fam).to_string());
            candidates.push(fam);
        }
        for c in &candidates {
            if let Some(faces) = idx.by_name.get(c) {
                let f = Self::pick(faces, bold, italic);
                if hints.needs_exact_metrics && ((bold && !f.bold) || (italic && !f.italic)) {
                    continue;
                }
                if let Some(font) = self.load_face(f, raw) {
                    return Some(font);
                }
            }
        }
        if let Some(serif) = japanese_generic(&n) {
            return self.japanese(serif, bold);
        }
        None
    }

    fn load_cjk_font(&self, name: &str, ordering: CjkFontOrdering, serif: bool) -> Option<Font> {
        if !name.is_empty()
            && let Some(f) = self.load_font(name, FontHints::default())
        {
            return Some(f);
        }
        let serif = japanese_generic(&normalize(name)).unwrap_or(serif);
        match ordering {
            CjkFontOrdering::AdobeJapan => self.japanese(serif, false),
            CjkFontOrdering::AdobeGb => self.by_names(if serif { &["SimSun", "Microsoft YaHei"] } else { &["Microsoft YaHei", "SimHei"] }, false, false),
            CjkFontOrdering::AdobeCns => self.by_names(if serif { &["MingLiU", "Microsoft JhengHei"] } else { &["Microsoft JhengHei", "MingLiU"] }, false, false),
            CjkFontOrdering::AdobeKorea => self.by_names(if serif { &["Batang", "Malgun Gothic"] } else { &["Malgun Gothic", "Gulim"] }, false, false),
        }
    }

    fn load_fallback_font(&self, script: u32, language: u32, hints: FontHints) -> Option<Font> {
        // Values of UCDN_SCRIPT_* and FZ_LANG_* from mupdf-sys.
        const HANGUL: u32 = 24;
        const HIRAGANA: u32 = 32;
        const KATAKANA: u32 = 33;
        const BOPOMOFO: u32 = 34;
        const HAN: u32 = 35;
        const THAI: u32 = 19;
        const LANG_KO: u32 = 416;
        const LANG_ZH_HANS: u32 = 14093;
        const LANG_ZH_HANT: u32 = 14822;
        const LANG_ZH: u32 = 242;
        let (b, i) = (hints.bold, hints.italic);
        match script {
            HIRAGANA | KATAKANA => self.japanese(hints.serif, b),
            HAN => match language {
                LANG_KO => self.by_names(&["Malgun Gothic"], b, false),
                LANG_ZH_HANS | LANG_ZH => self.by_names(&["Microsoft YaHei", "SimSun"], b, false),
                LANG_ZH_HANT => self.by_names(&["Microsoft JhengHei", "MingLiU"], b, false),
                _ => self.japanese(hints.serif, b),
            },
            HANGUL => self.by_names(&["Malgun Gothic", "Gulim"], b, false),
            BOPOMOFO => self.by_names(&["Microsoft JhengHei"], b, false),
            THAI => self.by_names(&["Leelawadee UI", "Tahoma"], b, false),
            _ => self.by_names(
                &["Segoe UI", "Arial", "Nirmala UI", "Ebrima", "Segoe UI Historic", "Segoe UI Symbol", "Segoe UI Emoji"],
                b,
                i,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_fullwidth_and_separators() {
        assert_eq!(normalize("ＭＳ 明朝"), "ms明朝");
        assert_eq!(normalize("MS-Mincho"), "msmincho");
        assert_eq!(strip_subset("ABCDEF+Ryumin-Light"), "Ryumin-Light");
        assert_eq!(strip_subset("Abcdef+X"), "Abcdef+X");
    }

    #[test]
    fn classifies_generic_japanese_names() {
        assert_eq!(japanese_generic(&normalize("Ryumin-Light-83pv-RKSJ-H")), Some(true));
        assert_eq!(japanese_generic(&normalize("GothicBBB-Medium")), Some(false));
        assert_eq!(japanese_generic(&normalize("Helvetica")), None);
    }
}
