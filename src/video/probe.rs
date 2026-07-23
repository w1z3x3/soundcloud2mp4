//! Measuring what ffmpeg actually produced, rather than trusting what it was
//! asked for.
//!
//! The renderer cuts each clip with `-t <duration>`, but the file that lands on
//! disk is not exactly that long: a video stream can only end on a frame
//! boundary, so a 1.500 s cut at 15 fps yields 23 frames = 1.5333 s. Feeding
//! the *requested* duration back into the combine stage therefore drifts a
//! little per clip, and over a few hundred clips that becomes seconds of error
//! in the chapter marks. Measuring closes the loop.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::models::messages::Tx;
use crate::utils::process::{run_capture, tool_command};

/// How many `ffprobe` calls to run at once. Each is a sub-100 ms metadata read,
/// so a little parallelism keeps a 500-clip playlist to a couple of seconds.
const PROBE_CONCURRENCY: usize = 8;

/// The `ffprobe` that ships alongside the configured `ffmpeg`.
///
/// The two binaries are distributed together, so deriving the path avoids a
/// second setting the user would have to keep in sync. Falls back to a bare
/// `ffprobe` (i.e. whatever is on PATH) when the ffmpeg path is named something
/// unexpected.
pub fn ffprobe_path(ffmpeg: &str) -> String {
    // Split on the last separator by hand rather than using `with_file_name`,
    // which rewrites the whole path with the platform's separator and would
    // turn "/usr/bin/ffmpeg" into "/usr/bin\ffprobe" on Windows.
    let split = ffmpeg.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
    let (dir, name) = ffmpeg.split_at(split);
    let probe = name.replacen("ffmpeg", "ffprobe", 1);
    if probe == name {
        return "ffprobe".into();
    }
    format!("{dir}{probe}")
}

/// Pull the first parseable duration out of ffprobe's output.
///
/// Matroska routinely reports `N/A` for a stream's duration and carries the
/// length at container level only, so both are requested and the first usable
/// answer wins.
fn parse_duration(stdout: &str) -> Option<f64> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "N/A")
        .find_map(|l| l.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
}

/// Exact length of `path` in seconds.
///
/// Asks for the video stream first — that is the timeline `xfade` positions
/// clips on — and falls back to the container duration.
pub async fn probe_duration(
    ffprobe: &str,
    path: &Path,
    token: &CancellationToken,
    log_file: Option<&Path>,
) -> Result<f64> {
    let mut cmd = tool_command(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=duration",
        "-show_entries",
        "format=duration",
        "-of",
        "csv=p=0",
    ])
    .arg(path);

    let (ok, stdout, stderr) = run_capture(cmd, ffprobe, token, log_file).await?;
    anyhow::ensure!(ok, "ffprobe failed for {}: {}", path.display(), stderr.trim());
    parse_duration(&stdout).with_context(|| {
        format!("ffprobe reported no usable duration for {}", path.display())
    })
}

/// Outcome of measuring a batch of clips.
pub struct Measured {
    /// One duration per input, in order: the measured value where ffprobe
    /// succeeded, the caller's requested value where it did not.
    pub durations: Vec<f64>,
    /// How many were genuinely measured.
    pub measured: usize,
    /// Largest single correction, for reporting.
    pub max_drift: f64,
    /// First failure, so the caller can explain the fallback once.
    pub error: Option<String>,
}

impl Measured {
    pub fn all_measured(&self) -> bool {
        self.error.is_none() && self.measured == self.durations.len()
    }
}

/// Measure every clip, falling back to `requested[i]` for any that cannot be
/// probed. Never fails the export: an unmeasurable clip just keeps the old
/// behaviour of trusting the requested duration.
pub async fn measure_all(
    ffprobe: &str,
    paths: &[PathBuf],
    requested: &[f64],
    token: &CancellationToken,
    tx: &Tx,
    log_file: Option<&Path>,
) -> Measured {
    assert_eq!(paths.len(), requested.len());

    let semaphore = Arc::new(Semaphore::new(PROBE_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    for (i, path) in paths.iter().enumerate() {
        let permit_source = semaphore.clone();
        let ffprobe = ffprobe.to_string();
        let path = path.clone();
        let token = token.clone();
        let log_file = log_file.map(Path::to_path_buf);
        set.spawn(async move {
            let _permit = permit_source.acquire().await;
            if token.is_cancelled() {
                return (i, Err("cancelled".to_string()));
            }
            let got = probe_duration(&ffprobe, &path, &token, log_file.as_deref())
                .await
                .map_err(|e| format!("{e:#}"));
            (i, got)
        });
    }

    let mut durations = requested.to_vec();
    let mut measured = 0usize;
    let mut max_drift = 0.0f64;
    let mut error: Option<String> = None;
    while let Some(joined) = set.join_next().await {
        let Ok((i, result)) = joined else { continue };
        match result {
            Ok(d) => {
                max_drift = max_drift.max((d - requested[i]).abs());
                durations[i] = d;
                measured += 1;
            }
            Err(e) => {
                if error.is_none() {
                    error = Some(e);
                }
            }
        }
    }

    if measured > 0 {
        tx.log(format!(
            "Measured {measured}/{} clip(s) with ffprobe (largest correction {max_drift:.3}s)",
            paths.len()
        ));
    }
    if let Some(e) = &error {
        tx.log(format!(
            "WARN: could not measure {} clip(s) ({e}); falling back to their requested \
             lengths, which may shift chapter marks slightly.",
            paths.len() - measured
        ));
    }

    Measured { durations, measured, max_drift, error }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffprobe_is_found_next_to_ffmpeg() {
        assert_eq!(ffprobe_path(r"C:\ytdlp\ffmpeg.exe"), r"C:\ytdlp\ffprobe.exe");
        assert_eq!(ffprobe_path("/usr/bin/ffmpeg"), "/usr/bin/ffprobe");
        assert_eq!(ffprobe_path("ffmpeg"), "ffprobe");
        assert_eq!(ffprobe_path("ffmpeg.exe"), "ffprobe.exe");
        // Unrecognisable name -> hope for one on PATH rather than guessing.
        assert_eq!(ffprobe_path(r"C:\tools\my-encoder.exe"), "ffprobe");
    }

    #[test]
    fn duration_prefers_the_stream_and_falls_back_to_the_container() {
        // mp4: both lines present and equal.
        assert_eq!(parse_duration("1.533333\n1.533333\n"), Some(1.533333));
        // Matroska: the stream duration is N/A, the container's is not.
        assert_eq!(parse_duration("N/A\n2.667000\n"), Some(2.667));
        // Nothing usable.
        assert_eq!(parse_duration("N/A\nN/A\n"), None);
        assert_eq!(parse_duration(""), None);
        // Zero-length or nonsense output is not a duration.
        assert_eq!(parse_duration("0.000000\n"), None);
        assert_eq!(parse_duration("garbage\n"), None);
    }
}
