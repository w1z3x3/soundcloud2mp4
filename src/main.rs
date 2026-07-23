#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use soundcloud2mp4::gui;
use tracing_subscriber::EnvFilter;

const APP_NAME: &str = "SoundCloud -> MP4 Converter";

/// Decode the bundled PNG into a window icon (center-cropped to a square).
fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/SoundCloud.png");
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    let side = w.min(h);
    let square = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    let resized = square.resize_exact(256, 256, image::imageops::FilterType::Lanczos3);
    let rgba = resized.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    Some(egui::IconData { rgba: rgba.into_raw(), width, height })
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!("starting soundcloud2mp4");

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1120.0, 800.0])
        .with_min_inner_size([900.0, 620.0])
        .with_title(APP_NAME);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(gui::app::App::new(cc)))),
    )
}
