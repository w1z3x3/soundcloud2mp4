use egui::{Color32, RichText, Ui, Vec2};

use crate::models::track::{FailureKind, MetaState, Track, TrackStatus};
use crate::utils::filesystem::format_duration;

pub const ACCENT: Color32 = Color32::from_rgb(255, 85, 0); // SoundCloud orange
pub const OK_GREEN: Color32 = Color32::from_rgb(90, 200, 120);
pub const ERR_RED: Color32 = Color32::from_rgb(235, 90, 90);
/// Permanently unavailable tracks: a fact to report, not an error to act on.
pub const WARN_AMBER: Color32 = Color32::from_rgb(225, 170, 70);

pub fn status_label(status: &TrackStatus) -> (String, Color32) {
    match status {
        TrackStatus::Pending => ("•".into(), Color32::GRAY),
        TrackStatus::Downloading => ("⬇ downloading".into(), ACCENT),
        TrackStatus::Rendering => ("🎬 rendering".into(), ACCENT),
        TrackStatus::Done(_) => ("✓ done".into(), OK_GREEN),
        TrackStatus::Failed(_) => ("✗ failed".into(), ERR_RED),
    }
}

/// One row in the track list. Returns true if any field changed.
pub fn track_row(ui: &mut Ui, track: &mut Track, global_max: f64, busy: bool) -> bool {
    let mut changed = false;

    ui.horizontal(|ui| {
        ui.set_min_height(56.0);

        ui.add_enabled_ui(!busy, |ui| {
            if ui.checkbox(&mut track.selected, "").changed() {
                changed = true;
            }
        });

        // Thumbnail (fetched over HTTP by egui's image loader) or placeholder.
        let thumb_size = Vec2::splat(48.0);
        match &track.thumbnail {
            Some(url) => {
                ui.add(
                    egui::Image::new(url.as_str())
                        .fit_to_exact_size(thumb_size)
                        .corner_radius(4.0),
                );
            }
            None => {
                let (rect, _) = ui.allocate_exact_size(thumb_size, egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, 4.0, Color32::from_gray(45));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "🎵",
                    egui::FontId::proportional(20.0),
                    Color32::from_gray(120),
                );
            }
        }

        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(&track.title).strong().size(14.5));
            match &track.meta {
                MetaState::Loading => {
                    ui.label(RichText::new("fetching metadata...").weak().italics().size(12.5));
                }
                MetaState::Failed(failure) => match failure.kind {
                    // Gone for good: a statement of fact with its reason on its
                    // own line, not a retryable error — nothing will change it.
                    FailureKind::Permanent => {
                        ui.label(
                            RichText::new("Metadata unavailable").color(WARN_AMBER).size(12.5),
                        );
                        ui.label(
                            RichText::new(format!("Reason: {}", failure.headline()))
                                .color(WARN_AMBER)
                                .size(12.0)
                                .italics(),
                        )
                        .on_hover_text(&failure.message);
                    }
                    // Refused to an anonymous client. Deliberately worded as an
                    // observation rather than a verdict: these tracks are often
                    // fine and reachable once signed in, so claiming "DRM
                    // protected / cannot be downloaded" would be wrong.
                    FailureKind::Restricted => {
                        ui.label(
                            RichText::new("Metadata unavailable").color(WARN_AMBER).size(12.5),
                        );
                        ui.label(
                            RichText::new(failure.headline())
                                .color(WARN_AMBER)
                                .size(12.0)
                                .italics(),
                        )
                        .on_hover_text(format!(
                            "{}\n\nSoundCloud refused this track to an anonymous client. \
                             Setting a cookies file (Settings → SoundCloud Authentication) and pressing \
                             Retry Failed Metadata may recover it.",
                            failure.message
                        ));
                    }
                    FailureKind::Retryable => {
                        ui.label(
                            RichText::new(format!("metadata unavailable: {}", failure.headline()))
                                .color(ERR_RED)
                                .size(12.5),
                        )
                        .on_hover_text(&failure.message);
                    }
                },
                MetaState::Loaded => {
                    ui.label(
                        RichText::new(format!(
                            "{}   ·   {}",
                            track.uploader,
                            format_duration(track.duration as f64)
                        ))
                        .weak()
                        .size(12.5),
                    );
                }
            }
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (text, color) = status_label(&track.status);
            let resp = ui.label(RichText::new(text).color(color));
            if let TrackStatus::Failed(err) = &track.status {
                resp.on_hover_text(err);
            } else if let TrackStatus::Done(path) = &track.status {
                resp.on_hover_text(path.display().to_string());
            }

            ui.add_space(8.0);

            // Per-track play time editor.
            let full = track.duration as f64;
            let max = if global_max > 0.0 {
                global_max
            } else if full > 0.0 {
                full
            } else {
                7200.0
            };
            ui.add_enabled_ui(!busy, |ui| {
                if ui
                    .add(
                        egui::DragValue::new(&mut track.play_seconds)
                            .range(1.0..=max)
                            .speed(1.0)
                            .suffix(" s"),
                    )
                    .on_hover_text("How many seconds of this song the video plays")
                    .changed()
                {
                    changed = true;
                }
            });
            ui.label(RichText::new("plays").weak().size(12.0));
        });
    });

    // A failed conversion shows its actual reason (ffmpeg/yt-dlp exit code and
    // stderr tail) directly under the row, not just "failed".
    if let TrackStatus::Failed(reason) = &track.status {
        ui.indent("fail_reason", |ui| {
            let mut shown: String = reason.chars().take(600).collect();
            if shown.len() < reason.len() {
                shown.push_str(" [...]");
            }
            ui.label(RichText::new(format!("Reason:\n{shown}")).color(ERR_RED).size(12.0))
                .on_hover_text(reason);
        });
    }

    changed
}
