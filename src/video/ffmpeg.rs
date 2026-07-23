use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use crate::models::messages::Tx;
use crate::utils::process::{run_streaming, tool_command};
use crate::video::encoder::{QualityTier, ResolvedEncoder};

/// One video render request.
#[derive(Debug)]
pub struct RenderJob {
    pub cover: PathBuf,
    pub audio: PathBuf,
    pub output: PathBuf,
    pub title: String,
    pub uploader: String,
    pub width: u32,
    pub height: u32,
    pub audio_bitrate_k: u32,
    /// Seconds of the song the video should play.
    pub duration: f64,
    pub fade: bool,
    pub zoom: bool,
    /// Which encoder produces the H.264 video. The compositing filters always
    /// run on the CPU; only the final H.264 encode is offloaded to the GPU.
    pub encoder: ResolvedEncoder,
    /// When true, pad the audio with silence to exactly `duration` and drop
    /// `-shortest`, so the clip is a predictable fixed length. Used for
    /// combined-playlist intermediate clips whose durations feed xfade offsets.
    pub pad_audio: bool,
}

const FPS: u32 = 30;
/// Names of the helper files written into the ffmpeg working directory.
/// ffmpeg runs with cwd = the track's work dir and references these
/// RELATIVELY — absolute paths inside a filtergraph would need double
/// escaping of the Windows drive colon (`C\\:`) and broke every render.
pub const FONT_FILE: &str = "font.ttf";
pub const TITLE_FILE: &str = "title.txt";
pub const ARTIST_FILE: &str = "artist.txt";

/// Locate a usable TTF font for drawtext on each platform.
pub fn find_font() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\Windows\Fonts\segoeui.ttf",
            r"C:\Windows\Fonts\arial.ttf",
            r"C:\Windows\Fonts\calibri.ttf",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
            "/Library/Fonts/Arial.ttf",
        ]
    } else {
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        ]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

/// Build the video filtergraph. Only relative file names appear in it, so no
/// path escaping is ever needed.
pub fn build_filter(job: &RenderJob, has_font: bool) -> String {
    let (w, h) = (job.width, job.height);
    let dur = job.duration.max(1.0);

    let mut filter = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=0x101014"
    );

    if job.zoom {
        let frames = (dur * FPS as f64).ceil() as u64;
        filter.push_str(&format!(
            ",zoompan=z='min(zoom+0.0004,1.25)':\
             x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':\
             d={frames}:s={w}x{h}:fps={FPS}"
        ));
    } else {
        filter.push_str(&format!(",fps={FPS}"));
    }

    let font_part = if has_font {
        format!("fontfile={FONT_FILE}:")
    } else {
        String::new()
    };

    // Title + artist overlay in the bottom area, centered.
    filter.push_str(&format!(
        ",drawtext={font_part}textfile={TITLE_FILE}:fontsize=h/16:fontcolor=white:\
         borderw=3:bordercolor=black@0.65:x=(w-text_w)/2:y=h-h/4.8"
    ));
    filter.push_str(&format!(
        ",drawtext={font_part}textfile={ARTIST_FILE}:fontsize=h/26:fontcolor=0xdddddd:\
         borderw=2:bordercolor=black@0.65:x=(w-text_w)/2:y=h-h/4.8+h/11"
    ));

    if job.fade {
        let fade_out = (dur - 1.2).max(0.0);
        filter.push_str(&format!(
            ",fade=t=in:st=0:d=1,fade=t=out:st={fade_out:.2}:d=1.2"
        ));
    }
    filter.push_str(",format=yuv420p");
    filter
}

/// Build the complete ffmpeg argument list (pure, unit-testable).
pub fn build_render_args(job: &RenderJob, has_font: bool) -> Vec<String> {
    let dur = job.duration.max(1.0);
    let mut args: Vec<String> = vec!["-y".into()];
    if !job.zoom {
        args.extend(["-loop".into(), "1".into()]);
    }
    args.extend(["-i".into(), job.cover.to_string_lossy().into_owned()]);
    args.extend(["-i".into(), job.audio.to_string_lossy().into_owned()]);
    args.extend(["-vf".into(), build_filter(job, has_font)]);

    // Audio filter chain: optional fades, optional silence-pad to full length.
    let mut afilters: Vec<String> = Vec::new();
    if job.fade {
        let fade_out = (dur - 1.2).max(0.0);
        afilters.push(format!("afade=t=in:st=0:d=0.8,afade=t=out:st={fade_out:.2}:d=1.2"));
    }
    if job.pad_audio {
        // Guarantee the audio reaches `dur` (songs shorter than the requested
        // play time get trailing silence) so the clip length is deterministic.
        afilters.push("apad".into());
    }
    if !afilters.is_empty() {
        args.extend(["-af".into(), afilters.join(",")]);
    }

    args.extend(["-t".into(), format!("{dur:.3}")]);
    // Video: the selected encoder (CPU libx264 or a GPU encoder). Audio is
    // always AAC and unchanged.
    args.extend(job.encoder.video_args(QualityTier::Clip));
    args.extend(["-c:a".into(), "aac".into()]);
    args.extend(["-b:a".into(), format!("{}k", job.audio_bitrate_k)]);
    if !job.pad_audio {
        args.push("-shortest".into());
    }
    args.extend(["-movflags".into(), "+faststart".into()]);
    args.extend(["-metadata".into(), format!("title={}", job.title)]);
    args.extend(["-metadata".into(), format!("artist={}", job.uploader)]);
    args.push(job.output.to_string_lossy().into_owned());
    args
}

/// drawtext renders text files verbatim; strip newlines so a stray trailing
/// `\n` doesn't draw an empty second line.
fn overlay_text(s: &str) -> String {
    s.replace(['\r', '\n'], " ").trim().to_string()
}

/// Render the MP4 for one track. `workdir` becomes ffmpeg's working directory
/// and holds font.ttf / title.txt / artist.txt.
pub async fn render(
    ffmpeg: &str,
    job: &RenderJob,
    workdir: &Path,
    token: &CancellationToken,
    tx: &Tx,
    log_file: Option<&Path>,
) -> Result<()> {
    std::fs::write(workdir.join(TITLE_FILE), overlay_text(&job.title))
        .context("writing title.txt")?;
    std::fs::write(workdir.join(ARTIST_FILE), overlay_text(&job.uploader))
        .context("writing artist.txt")?;

    let has_font = match find_font() {
        Some(font) => {
            std::fs::copy(&font, workdir.join(FONT_FILE))
                .with_context(|| format!("copying font {}", font.display()))?;
            true
        }
        None => {
            tx.log("WARN: no system font found; drawtext will use fontconfig defaults");
            false
        }
    };

    let mut cmd = tool_command(ffmpeg);
    cmd.current_dir(workdir);
    cmd.args(build_render_args(job, has_font));

    run_streaming(cmd, ffmpeg, token, tx, "ffmpeg", log_file).await
}

/// Generate a plain placeholder cover when a track has no artwork at all.
pub async fn make_placeholder_cover(
    ffmpeg: &str,
    out: &Path,
    width: u32,
    height: u32,
    token: &CancellationToken,
    tx: &Tx,
    log_file: Option<&Path>,
) -> Result<()> {
    let mut cmd = tool_command(ffmpeg);
    cmd.args([
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("gradients=s={width}x{height}:c0=0x1d1d33:c1=0x3a2c4f:n=2"),
        "-frames:v",
        "1",
    ])
    .arg(out);
    run_streaming(cmd, ffmpeg, token, tx, "ffmpeg", log_file).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> RenderJob {
        RenderJob {
            cover: PathBuf::from(r"C:\work\track.jpg"),
            audio: PathBuf::from(r"C:\work\track.mp3"),
            output: PathBuf::from(r"C:\out\Artist - Song.mp4"),
            title: "It's a: Song".into(),
            uploader: "Some Artist".into(),
            width: 1920,
            height: 1080,
            audio_bitrate_k: 320,
            duration: 215.0,
            fade: true,
            zoom: false,
            encoder: ResolvedEncoder::cpu(),
            pad_audio: false,
        }
    }

    #[test]
    fn filter_contains_no_absolute_paths_or_drive_colons() {
        let filter = build_filter(&job(), true);
        // The historical bug: `fontfile=C\:/...` broke filtergraph parsing.
        assert!(!filter.contains("C:"), "absolute path leaked into filter: {filter}");
        assert!(!filter.contains("C\\:"), "escaped path leaked into filter: {filter}");
        assert!(filter.contains("fontfile=font.ttf"));
        assert!(filter.contains("textfile=title.txt"));
        assert!(filter.contains("textfile=artist.txt"));
    }

    #[test]
    fn filter_scales_and_pads_to_requested_resolution() {
        let filter = build_filter(&job(), true);
        assert!(filter.contains("scale=1920:1080"));
        assert!(filter.contains("pad=1920:1080"));
        assert!(filter.contains("format=yuv420p"));
    }

    #[test]
    fn args_carry_quality_duration_and_metadata() {
        let args = build_render_args(&job(), true);
        let joined = args.join(" ");
        assert!(joined.contains("-b:a 320k"), "{joined}");
        assert!(joined.contains("-t 215.000"), "{joined}");
        assert_eq!(args.iter().filter(|a| *a == "-metadata").count(), 2);
        assert!(args.iter().any(|a| a == "title=It's a: Song"), "{joined}");
        assert!(args.iter().any(|a| a == "artist=Some Artist"), "{joined}");
        assert!(args.iter().any(|a| a.ends_with("Artist - Song.mp4")));
    }

    #[test]
    fn loop_flag_only_without_zoom() {
        let mut j = job();
        assert!(build_render_args(&j, true).contains(&"-loop".to_string()));
        j.zoom = true;
        let args = build_render_args(&j, true);
        assert!(!args.contains(&"-loop".to_string()));
        assert!(build_filter(&j, true).contains("zoompan"));
    }

    #[test]
    fn fade_filters_present_only_when_enabled() {
        let mut j = job();
        let args = build_render_args(&j, true).join(" ");
        assert!(args.contains("afade"));
        assert!(build_filter(&j, true).contains("fade=t=out:st=213.80"));
        j.fade = false;
        assert!(!build_render_args(&j, true).join(" ").contains("afade"));
        assert!(!build_filter(&j, true).contains("fade="));
    }

    #[test]
    fn missing_font_omits_fontfile() {
        let filter = build_filter(&job(), false);
        assert!(!filter.contains("fontfile"));
        assert!(filter.contains("textfile=title.txt"));
    }

    #[test]
    fn pad_audio_adds_apad_and_drops_shortest() {
        let mut j = job();
        j.fade = false;
        j.pad_audio = true;
        let args = build_render_args(&j, true);
        let joined = args.join(" ");
        assert!(joined.contains("-af apad"), "{joined}");
        assert!(!args.contains(&"-shortest".to_string()), "combined clips must be fixed-length");
    }

    #[test]
    fn separate_mode_keeps_shortest() {
        let args = build_render_args(&job(), true);
        assert!(args.contains(&"-shortest".to_string()));
    }
}
