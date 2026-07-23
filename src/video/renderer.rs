use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use super::ffmpeg::{make_placeholder_cover, render, RenderJob};
use crate::config::settings::Settings;
use crate::downloader::metadata::DownloadedTrack;
use crate::models::messages::Tx;
use crate::models::track::TrackMetadata;
use crate::utils::filesystem::{sanitize_filename, unique_path};
use crate::video::encoder::ResolvedEncoder;

/// Effective render length for a track, honoring the requested play time, the
/// track's real length and the global per-song maximum (0 = unlimited).
pub fn effective_seconds(meta: &TrackMetadata, play_seconds: f64, max_seconds: f64) -> f64 {
    let mut d = play_seconds;
    if meta.duration > 0 {
        d = d.min(meta.duration as f64);
    }
    if max_seconds > 0.0 {
        d = d.min(max_seconds);
    }
    d.max(1.0)
}

/// Resolve the cover image, generating a gradient placeholder for tracks that
/// genuinely have no artwork (already verified by `read_downloaded`).
async fn resolve_cover(
    settings: &Settings,
    downloaded: &DownloadedTrack,
    workdir: &Path,
    token: &CancellationToken,
    tx: &Tx,
) -> Result<PathBuf> {
    if let Some(path) = &downloaded.cover {
        return Ok(path.clone());
    }
    let (width, height) = settings.resolution();
    tx.log("Track has no artwork — generating placeholder cover".to_string());
    let placeholder = workdir.join("placeholder.png");
    make_placeholder_cover(
        &settings.ffmpeg_path,
        &placeholder,
        width,
        height,
        token,
        tx,
        settings.tool_log("ffmpeg.log").as_deref(),
    )
    .await
    .context("generating placeholder cover")?;
    Ok(placeholder)
}

/// Core renderer shared by both export modes. Renders one validated track to
/// `output` and returns the exact clip duration in seconds.
///
/// - `intermediate = false`: standalone per-track video (fades follow the
///   user's setting, audio uses `-shortest`) — the original separate-mode file.
/// - `intermediate = true`: a fixed-length clip for the combined playlist —
///   per-clip fades are OFF (the concat stage supplies transitions instead) and
///   the audio is padded to the exact duration so xfade offsets are reliable.
pub async fn render_track_to(
    settings: &Settings,
    downloaded: &DownloadedTrack,
    play_seconds: f64,
    output: &Path,
    intermediate: bool,
    encoder: ResolvedEncoder,
    workdir: &Path,
    token: &CancellationToken,
    tx: &Tx,
) -> Result<f64> {
    let (width, height) = settings.resolution();
    let cover = resolve_cover(settings, downloaded, workdir, token, tx).await?;
    let meta = &downloaded.meta;
    let duration = effective_seconds(meta, play_seconds, settings.max_track_seconds);

    let job = RenderJob {
        cover,
        audio: downloaded.audio.clone(),
        output: output.to_path_buf(),
        title: meta.title.clone(),
        uploader: meta.uploader.clone(),
        width,
        height,
        audio_bitrate_k: settings.bitrate_k(),
        duration,
        fade: if intermediate { false } else { settings.effect_fade },
        zoom: settings.effect_zoom,
        encoder,
        pad_audio: intermediate,
    };

    render(
        &settings.ffmpeg_path,
        &job,
        workdir,
        token,
        tx,
        settings.tool_log("ffmpeg.log").as_deref(),
    )
    .await
    .context("rendering video")?;
    Ok(duration)
}

/// Separate-mode entry point: render a track to `Artist - Title.mp4` in the
/// output folder. Returns the path of the finished video.
pub async fn render_track(
    settings: &Settings,
    downloaded: &DownloadedTrack,
    play_seconds: f64,
    encoder: ResolvedEncoder,
    workdir: &Path,
    token: &CancellationToken,
    tx: &Tx,
) -> Result<PathBuf> {
    let meta = &downloaded.meta;
    let stem = sanitize_filename(&format!("{} - {}", meta.uploader, meta.title));
    let output = unique_path(&settings.output_folder, &stem, "mp4");
    render_track_to(
        settings, downloaded, play_seconds, &output, false, encoder, workdir, token, tx,
    )
    .await?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(duration: u64) -> TrackMetadata {
        TrackMetadata {
            id: "1".into(),
            title: "t".into(),
            uploader: "u".into(),
            duration,
            thumbnail: None,
            url: "https://x".into(),
        }
    }

    #[test]
    fn effective_seconds_respects_track_length_and_global_max() {
        // Requested longer than the song -> capped at song length.
        assert_eq!(effective_seconds(&meta(30), 60.0, 0.0), 30.0);
        // Global max wins when smaller.
        assert_eq!(effective_seconds(&meta(300), 300.0, 20.0), 20.0);
        // Unknown duration (0) -> play_seconds honored.
        assert_eq!(effective_seconds(&meta(0), 45.0, 0.0), 45.0);
        // Never below 1 second.
        assert_eq!(effective_seconds(&meta(0), 0.0, 0.0), 1.0);
    }
}
