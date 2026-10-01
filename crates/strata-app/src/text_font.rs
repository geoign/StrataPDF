//! Fonts of the text view: a Japanese face and a Latin face, chosen from presets
//! or installed families and handed to the page as CSS variables.
//!
//! Noto Serif JP, Noto Sans JP and LINE Seed JP ship in `fonts\` next to the
//! executable (`tools\fetch-fonts.ps1`, SIL OFL 1.1) and are served to the page
//! under their own family names, so they are there whatever is installed.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A bundled face: file in the fonts folder, CSS family, weight (range).
const BUNDLED: &[(&str, &str, &str)] = &[
    ("NotoSerifJP-VF.ttf", "StrataPDF Noto Serif JP", "200 900"),
    ("NotoSansJP-VF.ttf", "StrataPDF Noto Sans JP", "100 900"),
    ("LINESeedJP-Regular.ttf", "StrataPDF LINE Seed JP", "400"),
    ("LINESeedJP-Bold.ttf", "StrataPDF LINE Seed JP", "700"),
];

/// Folder of the bundled fonts: `fonts\` beside the executable, or in a debug
/// build the repository's `assets\fonts`.
fn fonts_dir() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok()?.parent()?.join("fonts");
    if beside.is_dir() || !cfg!(debug_assertions) {
        return Some(beside);
    }
    Some(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/fonts")))
}

/// Data of a bundled font requested by the page as `/fonts/<file>`.
pub fn bundled_file(path: &str) -> Option<Vec<u8>> {
    let name = path.strip_prefix("/fonts/")?;
    // Only the known files: the name never reaches the file system unchecked.
    let (file, ..) = BUNDLED.iter().find(|(f, ..)| *f == name)?;
    std::fs::read(fonts_dir()?.join(file)).ok()
}

/// `@font-face` rules of the bundled fonts, for the viewer's pages.
pub fn font_faces() -> String {
    BUNDLED
        .iter()
        .map(|(file, family, weight)| format!("\n@font-face {{ font-family: \"{family}\"; src: url(\"/fonts/{file}\") format(\"truetype\"); font-weight: {weight}; }}"))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FontChoice {
    /// Key of an entry in `JA_PRESETS` / `LATIN_PRESETS`.
    Preset(String),
    /// An installed family by its English name.
    Family(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextFonts {
    pub ja: FontChoice,
    pub latin: FontChoice,
}

impl Default for TextFonts {
    fn default() -> Self {
        TextFonts { ja: FontChoice::Preset("noto_serif".into()), latin: FontChoice::Preset("book".into()) }
    }
}

struct Preset {
    key: &'static str,
    label: &'static str,
    /// Families tried in order; the first installed one is used.
    stack: &'static [&'static str],
    sans: bool,
    /// Gothic preset for headings next to a Mincho body.
    heading: &'static str,
}

const JA_PRESETS: &[Preset] = &[
    Preset { key: "noto_serif", label: "Noto Serif JP（源ノ明朝）", stack: &["StrataPDF Noto Serif JP", "Noto Serif JP", "Noto Serif CJK JP", "Source Han Serif JP"], sans: false, heading: "noto_sans" },
    Preset { key: "biz_mincho", label: "BIZ UD明朝", stack: &["BIZ UDPMincho"], sans: false, heading: "biz_gothic" },
    Preset { key: "yu_mincho", label: "游明朝", stack: &["Yu Mincho", "YuMincho"], sans: false, heading: "yu_gothic" },
    Preset { key: "ms_mincho", label: "MS P明朝", stack: &["MS PMincho"], sans: false, heading: "biz_gothic" },
    Preset { key: "noto_sans", label: "Noto Sans JP（源ノ角ゴシック）", stack: &["StrataPDF Noto Sans JP", "Noto Sans JP", "Noto Sans CJK JP", "Source Han Sans JP"], sans: true, heading: "noto_sans" },
    Preset { key: "biz_gothic", label: "BIZ UDゴシック", stack: &["BIZ UDPGothic"], sans: true, heading: "biz_gothic" },
    // Yu Gothic Regular is too thin on screen.
    Preset { key: "yu_gothic", label: "游ゴシック", stack: &["Yu Gothic Medium", "Yu Gothic", "YuGothic"], sans: true, heading: "yu_gothic" },
    // LINE's installers register it as "LINE Seed JP_OTF" / "_TTF", Google Fonts as "LINE Seed JP".
    Preset { key: "line_seed", label: "LINE Seed JP", stack: &["StrataPDF LINE Seed JP", "LINE Seed JP", "LINE Seed JP_OTF", "LINE Seed JP_TTF"], sans: true, heading: "line_seed" },
    Preset { key: "meiryo", label: "メイリオ", stack: &["Meiryo"], sans: true, heading: "meiryo" },
    Preset { key: "kyokasho", label: "UD デジタル 教科書体", stack: &["UD Digi Kyokasho NP"], sans: false, heading: "biz_gothic" },
];

const LATIN_PRESETS: &[Preset] = &[
    Preset { key: "none", label: "和文フォントに合わせる", stack: &[], sans: false, heading: "" },
    Preset { key: "book", label: "Charis SIL / Cambria", stack: &["Charis SIL", "Cambria", "Georgia"], sans: false, heading: "" },
    Preset { key: "georgia", label: "Georgia", stack: &["Georgia"], sans: false, heading: "" },
    Preset { key: "times", label: "Times New Roman", stack: &["Times New Roman"], sans: false, heading: "" },
    Preset { key: "noto_serif", label: "Noto Serif", stack: &["Noto Serif"], sans: false, heading: "" },
    Preset { key: "segoe", label: "Segoe UI", stack: &["Segoe UI"], sans: true, heading: "" },
];

/// Installed on every Windows 10 (1809+) and 11, so missing presets fall back to them.
const MINCHO_FALLBACK: &[&str] = &["BIZ UDPMincho", "Yu Mincho"];
const GOTHIC_FALLBACK: &[&str] = &["BIZ UDPGothic", "Yu Gothic"];

fn ja_preset(key: &str) -> Option<&'static Preset> {
    JA_PRESETS.iter().find(|p| p.key == key)
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Gothic-looking family names, for the generic fallback of a custom font.
fn looks_sans(name: &str) -> bool {
    let n = name.to_lowercase();
    ["gothic", "sans", "meiryo", "ゴシック", "seed"].iter().any(|k| n.contains(k))
}

impl TextFonts {
    /// The Japanese font as (families, is gothic, families for headings).
    fn ja_parts(&self) -> (Vec<String>, bool, Vec<String>) {
        let preset = match &self.ja {
            FontChoice::Preset(k) => ja_preset(k),
            FontChoice::Family(_) => None,
        };
        let (mut stack, sans, heading): (Vec<String>, bool, Option<&Preset>) = match (&self.ja, preset) {
            (_, Some(p)) => (p.stack.iter().map(|s| s.to_string()).collect(), p.sans, ja_preset(p.heading)),
            (FontChoice::Family(f), None) => {
                let sans = looks_sans(f);
                (vec![f.clone()], sans, ja_preset("biz_gothic"))
            }
            (FontChoice::Preset(_), None) => (ja_preset("noto_serif").unwrap().stack.iter().map(|s| s.to_string()).collect(), false, ja_preset("noto_sans")),
        };
        let mut head: Vec<String> = if sans { stack.clone() } else { heading.map(|p| p.stack.iter().map(|s| s.to_string()).collect()).unwrap_or_default() };
        for f in if sans { GOTHIC_FALLBACK } else { MINCHO_FALLBACK } {
            if !stack.iter().any(|s| s == f) {
                stack.push(f.to_string());
            }
        }
        for f in GOTHIC_FALLBACK {
            if !head.iter().any(|s| s == f) {
                head.push(f.to_string());
            }
        }
        (stack, sans, head)
    }

    fn latin_stack(&self) -> Vec<String> {
        match &self.latin {
            FontChoice::Preset(k) => LATIN_PRESETS.iter().find(|p| p.key == k).map(|p| p.stack.iter().map(|s| s.to_string()).collect()).unwrap_or_default(),
            FontChoice::Family(f) => vec![f.clone()],
        }
    }

    /// CSS custom properties of the text view's style sheet.
    pub fn css_vars(&self) -> Vec<(&'static str, String)> {
        let (ja, sans, head) = self.ja_parts();
        let generic = if sans { "sans-serif" } else { "serif" };
        let join = |v: &[String]| v.iter().map(|s| quote(s)).collect::<Vec<_>>().join(", ");
        let latin = self.latin_stack();
        let body = if latin.is_empty() { format!("{}, {generic}", join(&ja)) } else { format!("{}, {}, {generic}", join(&latin), join(&ja)) };
        vec![
            ("--font-body", body),
            ("--font-ja", format!("{}, {generic}", join(&ja))),
            ("--font-head", format!("\"Segoe UI\", {}, sans-serif", join(&head))),
        ]
    }

    /// `:root { ... }` rule for a saved HTML file.
    pub fn css_rule(&self) -> String {
        let decl: String = self.css_vars().iter().map(|(k, v)| format!("{k}: {v}; ")).collect();
        format!("\n:root {{ {decl}}}\n")
    }

    /// JSON object of the variables, for the page script.
    pub fn json(&self) -> String {
        serde_json::Value::Object(self.css_vars().into_iter().map(|(k, v)| (k.to_string(), serde_json::Value::String(v))).collect()).to_string()
    }

    fn choice_label(c: &FontChoice, presets: &[Preset]) -> String {
        match c {
            FontChoice::Preset(k) => presets.iter().find(|p| p.key == k).map(|p| p.label.to_string()).unwrap_or_default(),
            FontChoice::Family(f) => strata_core::fonts::families().iter().find(|x| &x.name == f).map(|x| x.display.clone()).unwrap_or_else(|| f.clone()),
        }
    }

    /// Short description for the toolbar button's tooltip.
    pub fn summary(&self) -> String {
        format!("和文: {}\n欧文: {}", Self::choice_label(&self.ja, JA_PRESETS), Self::choice_label(&self.latin, LATIN_PRESETS))
    }

    /// Contents of the font menu: one submenu per script, so the list fits the window.
    pub fn menu(&mut self, ui: &mut egui::Ui) {
        let ja = Self::choice_label(&self.ja, JA_PRESETS);
        ui.menu_button(format!("和文: {ja}"), |ui| section(ui, &mut self.ja, JA_PRESETS, true, "その他の和文フォント"));
        let latin = Self::choice_label(&self.latin, LATIN_PRESETS);
        ui.menu_button(format!("欧文: {latin}"), |ui| section(ui, &mut self.latin, LATIN_PRESETS, false, "その他の欧文フォント"));
        ui.separator();
        if ui.button("既定に戻す").clicked() {
            *self = TextFonts::default();
            ui.close();
        }
    }
}

fn section(ui: &mut egui::Ui, choice: &mut FontChoice, presets: &[Preset], japanese: bool, more: &str) {
    for p in presets {
        let sel = matches!(choice, FontChoice::Preset(k) if k == p.key);
        if ui.radio(sel, p.label).clicked() {
            *choice = FontChoice::Preset(p.key.into());
            // The menu covers the text; close it to show the result.
            ui.close();
        }
    }
    let families = strata_core::fonts::families();
    if let FontChoice::Family(f) = choice {
        let label = families.iter().find(|x| &x.name == f).map(|x| x.display.clone()).unwrap_or_else(|| f.clone());
        let _ = ui.radio(true, label);
    }
    ui.menu_button(more, |ui| {
        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            for fam in families.iter().filter(|f| f.japanese == japanese) {
                let sel = matches!(choice, FontChoice::Family(f) if *f == fam.name);
                let text = if fam.display != fam.name { format!("{}（{}）", fam.display, fam.name) } else { fam.name.clone() };
                if ui.selectable_label(sel, text).clicked() {
                    *choice = FontChoice::Family(fam.name.clone());
                    ui.close();
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_vars() {
        let v = TextFonts::default().css_vars();
        assert_eq!(v[0].1, r#""Charis SIL", "Cambria", "Georgia", "StrataPDF Noto Serif JP", "Noto Serif JP", "Noto Serif CJK JP", "Source Han Serif JP", "BIZ UDPMincho", "Yu Mincho", serif"#);
        assert_eq!(v[2].1, r#""Segoe UI", "StrataPDF Noto Sans JP", "Noto Sans JP", "Noto Sans CJK JP", "Source Han Sans JP", "BIZ UDPGothic", "Yu Gothic", sans-serif"#);
    }

    #[test]
    fn custom_gothic_without_latin() {
        let f = TextFonts { ja: FontChoice::Family("Meiryo UI".into()), latin: FontChoice::Preset("none".into()) };
        let v = f.css_vars();
        assert_eq!(v[0].1, r#""Meiryo UI", "BIZ UDPGothic", "Yu Gothic", sans-serif"#);
        assert_eq!(v[2].1, r#""Segoe UI", "Meiryo UI", "BIZ UDPGothic", "Yu Gothic", sans-serif"#);
    }

    #[test]
    fn only_bundled_files_are_served() {
        assert!(bundled_file("/fonts/../../Cargo.toml").is_none());
        assert!(bundled_file("/fonts/OFL-LINESeedJP.txt").is_none());
        assert!(bundled_file("/index.html").is_none());
    }

    #[test]
    fn quotes_are_escaped() {
        assert_eq!(quote(r#"A"B\C"#), r#""A\"B\\C""#);
    }
}
