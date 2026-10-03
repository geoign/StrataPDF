#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod convert;
mod headless;
mod instance;
mod layout;
mod ocr_ui;
mod print;
mod reflow_view;
mod text_font;
mod tiles;
mod translate_ui;
mod view;

use std::path::PathBuf;
use std::sync::Arc;

fn main() -> eframe::Result {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(code) = headless::run(&args) {
        std::process::exit(code);
    }
    env_logger::init();
    strata_core::fonts::install();
    let new_window = args.iter().any(|a| a == "--new-window");
    let files: Vec<PathBuf> = args.into_iter().filter(|a| a != "--new-window").map(PathBuf::from).collect();
    if !new_window && instance::forward_to_running(&files) {
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 900.0])
            .with_min_inner_size([480.0, 320.0])
            .with_drag_and_drop(true)
            .with_title("StrataPDF")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/strata.png")).unwrap_or_default()),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("StrataPDF", options, Box::new(|cc| Ok(Box::new(app::StrataApp::new(cc, files)))))
}

/// Add a Japanese system font as fallback for egui's built-in fonts.
pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let names = ["Yu Gothic UI", "Meiryo UI", "Yu Gothic", "Meiryo", "MS UI Gothic", "MS Gothic"];
    if let Some((path, index)) = strata_core::fonts::find_face(&names)
        && let Ok(bytes) = std::fs::read(&path)
    {
        let data = egui::FontData { font: std::borrow::Cow::Owned(bytes), index, tweak: Default::default() };
        fonts.font_data.insert("jp".into(), Arc::new(data));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(fam).or_default().push("jp".into());
        }
    }
    ctx.set_fonts(fonts);
}
