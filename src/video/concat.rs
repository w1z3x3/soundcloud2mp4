//! Combined-playlist export: stitch the per-track intermediate clips into a
//! single MP4 with crossfade transitions, playlist-level metadata and chapters.
//!
//! Clips are combined with `filter_complex` (xfade + acrossfade when a
//! transition is configured, otherwise the `concat` filter) and always
//! re-encoded — finished MP4s are never blindly concatenated, so timing,
//! transitions and metadata stay correct.
//!
//! # Why this is a *ladder* of ffmpeg passes, not one command
//!
//! One pass per playlist does not scale. Every clip costs an `-i <path>` pair
//! plus its own link labels in the filtergraph, so the command line grows
//! linearly with the track count. At ~500 tracks that command line passed
//! Windows' ~32 KB `CreateProcessW` limit and the spawn failed outright with
//! `ERROR_FILENAME_EXCED_RANGE (os error 206)` — reported as
//! "The filename or extension is too long", which is thoroughly misleading:
//! no individual path was ever too long.
//!
//! Three changes keep the command line flat regardless of playlist size:
//!
//! 1. **Batching (a two-tier ladder).** [`plan_passes`] splits the rendered
//!    clips into first-level batches of at most `leaf_max`
//!    (`combine_chunk_size`, default 40), then repeatedly combines the resulting
//!    *batch* files — but those upper levels use a much smaller `batch_max`
//!    (`batch_combine_chunk_size`, default 4), because a batch input is a full
//!    re-encoded video that is far heavier to decode than a still-image clip.
//!    500 clips become `493 → 13 (r1) → 4 (r2) → final(4)`.
//!
//!    **Why two tiers.** A first-level pass decodes up to 40 tiny still-image
//!    clips cheaply, but the final pass used to open *all* first-level batches at
//!    once (13 for a 500-track playlist) — each a ~1 GB, 100-minute 1080p video.
//!    Even with software decode that is a very memory-heavy process, and with
//!    hardware (d3d11va) decode it exhausted GPU memory outright. Capping the
//!    upper levels at `batch_max` means **no pass ever opens more than a handful
//!    of large intermediates simultaneously**, at any playlist size.
//!
//!    **The trade-off, made deliberately.** Each extra ladder level re-encodes
//!    the combined portion one more time, so this design accepts *one additional
//!    lossy intermediate generation* (intermediates are high-quality cqp + FLAC,
//!    so the loss is mild) in exchange for dramatically lower peak memory and
//!    much better scalability on very large playlists. Timing, chapter marks and
//!    total duration are unaffected — they are computed once from the original
//!    clip durations and compose exactly across however many levels the ladder
//!    grows (see [`plan_passes`]).
//! 2. **The filtergraph moves off the command line** into a script file passed
//!    with `-filter_complex_script`, so the largest single argument disappears.
//! 3. **Paths are made relative** to the work directory (ffmpeg runs with its
//!    cwd set there), turning a ~60-character absolute path into ~20.
//!
//! Batching is *exactly* equivalent to the single monster command, not an
//! approximation: chained `xfade`/`acrossfade` is associative over a batch
//! boundary, because the boundary crossfade still joins the same two clips'
//! tail and head. Durations compose the same way — see [`plan_passes`] — so
//! chapter timestamps are computed once, from the original clip list, and stay
//! correct no matter how many levels the ladder ends up with.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use crate::models::messages::Tx;
use crate::utils::process::{run_streaming, tool_command};
use crate::video::encoder::{QualityTier, ResolvedEncoder};

/// Maximum clip inputs handed to a single ffmpeg invocation.
///
/// Sized so a full pass stays far below Windows' ~32 KB command-line limit
/// even with long file names: 40 inputs is roughly 1 KB of arguments once the
/// filtergraph is in a script file and paths are relative. `combine_plan_stays_far_below_windows_limit`
/// asserts the real headroom for a 500-track playlist.
pub const MAX_INPUTS_PER_PASS: usize = 40;

/// Largest batch size a user may select. 64 inputs is still only ~2.5 KB of
/// command-line arguments (paths are relative and the filtergraph lives in a
/// script file), an order of magnitude under the Windows limit — verified by
/// `combine_plan_stays_far_below_windows_limit`.
pub const MAX_COMBINE_CHUNK: usize = 64;

/// Windows' documented command-line ceiling for `CreateProcessW`.
/// Only used by tests, which assert the plan stays an order of magnitude below it.
pub const WINDOWS_CMDLINE_LIMIT: usize = 32_767;

/// A rendered intermediate clip that will become one segment of the playlist.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub path: PathBuf,
    /// Exact clip length in seconds (audio padded to match — see renderer).
    pub duration: f64,
    pub title: String,
    pub uploader: String,
}

/// Transitions shorter than this are treated as a hard cut (concat).
const MIN_TRANSITION: f64 = 0.1;

/// When resuming, a batch already on disk is reused only if its measured length
/// is within this many seconds of the length the current plan predicts for it.
///
/// A batch left over from a *different* clip set (a track added or dropped) or a
/// different transition setting differs by far more than this, so it is rebuilt
/// rather than silently trusted. The slack only has to absorb the sub-frame
/// rounding between a prediction and a real re-encode, which is a few tens of
/// milliseconds even over a full batch.
const BATCH_REUSE_TOLERANCE: f64 = 2.0;

pub struct PlaylistRenderer {
    pub clips: Vec<ClipInfo>,
    pub output: PathBuf,
    pub playlist_title: String,
    /// Requested transition length in seconds (0 = hard cut).
    pub transition: f64,
    pub chapters: bool,
    pub audio_bitrate_k: u32,
    /// Encoder for every combine pass. The compositing (scale / xfade / text)
    /// runs on the CPU; only the H.264 encode is offloaded to the GPU.
    pub encoder: ResolvedEncoder,
    /// Max clips per FIRST-LEVEL pass. Smaller values bound each ffmpeg
    /// process's memory when combining still-image clips.
    pub chunk_size: usize,
    /// Max intermediate BATCH files per UPPER-LEVEL (round 2+) pass. Kept far
    /// smaller than `chunk_size` because a batch input is a full re-encoded
    /// video — much heavier to decode than a clip — so only a few may be open at
    /// once. This bounds peak memory in the final combine on huge playlists (see
    /// the module docs' two-tier ladder section).
    pub batch_chunk_size: usize,
}

/// Effective transition after clamping so it can never exceed the shortest
/// clip (which would make xfade/acrossfade offsets invalid).
pub fn safe_transition(durations: &[f64], requested: f64) -> f64 {
    if requested < MIN_TRANSITION || durations.len() < 2 {
        return 0.0;
    }
    let min_dur = durations.iter().cloned().fold(f64::INFINITY, f64::min);
    // Leave a little headroom on the shortest clip.
    let capped = requested.min((min_dur - 0.2).max(0.0));
    if capped < MIN_TRANSITION {
        0.0
    } else {
        capped
    }
}

/// Start timestamp (seconds) of each clip in the combined timeline.
/// Overlapping transitions shift every clip earlier by `transition`.
pub fn chapter_starts(durations: &[f64], transition: f64) -> Vec<f64> {
    let mut starts = Vec::with_capacity(durations.len());
    let mut cum = 0.0;
    for d in durations {
        starts.push(cum);
        cum += d - transition;
    }
    starts
}

/// Total length of the combined video in seconds.
pub fn total_duration(durations: &[f64], transition: f64) -> f64 {
    if durations.is_empty() {
        return 0.0;
    }
    let sum: f64 = durations.iter().sum();
    (sum - transition * (durations.len() as f64 - 1.0)).max(0.0)
}

fn escape_meta(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '=' | ';' | '#' | '\\' | '\n') {
            out.push('\\');
        }
        out.push(c);
    }
    out.replace(['\r'], " ")
}

/// Build an FFMETADATA1 document with playlist-level tags and (optionally)
/// one chapter per track.
///
/// `durations` is passed separately from `clips` — and deliberately so. Chapter
/// marks must be derived from the clips' *measured* lengths (see
/// [`crate::video::probe`]), not the lengths the renderer was asked to produce;
/// taking `ClipInfo::duration` here would quietly reintroduce the drift.
pub fn build_ffmetadata(
    playlist_title: &str,
    clips: &[ClipInfo],
    durations: &[f64],
    transition: f64,
    chapters: bool,
) -> String {
    assert_eq!(clips.len(), durations.len(), "one duration per clip");
    let mut s = String::from(";FFMETADATA1\n");
    s.push_str(&format!("title={}\n", escape_meta(playlist_title)));
    s.push_str("artist=SoundCloud Playlist\n");
    s.push_str("comment=Generated by SoundCloud2MP4\n");

    if chapters && !clips.is_empty() {
        let starts = chapter_starts(durations, transition);
        let total = total_duration(durations, transition);
        for (i, clip) in clips.iter().enumerate() {
            let start_ms = (starts[i] * 1000.0).round() as i64;
            let end_ms = starts
                .get(i + 1)
                .map(|s| (s * 1000.0).round() as i64)
                .unwrap_or_else(|| (total * 1000.0).round() as i64);
            s.push_str("\n[CHAPTER]\nTIMEBASE=1/1000\n");
            s.push_str(&format!("START={start_ms}\nEND={end_ms}\n"));
            s.push_str(&format!(
                "title={}\n",
                escape_meta(&format!("{} - {}", clip.uploader, clip.title))
            ));
        }
    }
    s
}

/// Build the `filter_complex` string that merges N clip inputs into a single
/// `[vout]`/`[aout]` pair. Pure and unit-tested.
///
/// - `transition > 0`: chained `xfade` (video) + `acrossfade` (audio).
/// - `transition == 0`: the `concat` filter (hard cuts).
///
/// # Why every input's audio is trimmed first
///
/// `xfade` is told exactly where each cut lands (`offset=`), so the video
/// timeline is whatever this function computes. `acrossfade` has no such
/// control: it simply consumes both inputs, so the audio timeline is whatever
/// the *decoded* streams happen to be. Those two disagree.
///
/// A 1.500 s AAC stream decodes to 65 whole 1024-sample frames = 1.5093 s, and
/// that ~9 ms surplus compounds once per clip: measured at +0.028 s over 3
/// clips, +0.093 s over 10, +0.186 s over 20 — about 4.6 s adrift by 500. The
/// audio would slide progressively later than the picture it belongs to, and no
/// chapter timestamp could be right for both streams at once.
///
/// So each input's audio is padded and cut to exactly the duration the video
/// side is using. Both timelines then run `Σd - t(n-1)`, and a chapter mark at
/// `Σd - k·t` is correct in both.
pub fn build_combine_filter(durations: &[f64], transition: f64) -> String {
    let n = durations.len();
    assert!(n >= 2, "combine filter needs at least two clips");

    // Pin every input's audio to the same length the video side assumes.
    // `apad` makes it arbitrarily long, `atrim` cuts it back to exactly `d`.
    let mut chains: Vec<String> = durations
        .iter()
        .enumerate()
        .map(|(i, d)| format!("[{i}:a]apad,atrim=0:{d:.6},asetpts=N/SR/TB[an{i}]"))
        .collect();

    if transition <= 0.0 {
        let mut inputs = String::new();
        for i in 0..n {
            inputs.push_str(&format!("[{i}:v][an{i}]"));
        }
        chains.push(format!("{inputs}concat=n={n}:v=1:a=1[vout][aout]"));
        return chains.join(";");
    }

    // Video: chain xfade, tracking the accumulated-timeline offset.
    let mut prev = "[0:v]".to_string();
    let mut cum = durations[0];
    for i in 1..n {
        let offset = (cum - transition).max(0.0);
        let out = if i == n - 1 {
            "[vout]".to_string()
        } else {
            format!("[vx{i}]")
        };
        chains.push(format!(
            "{prev}[{i}:v]xfade=transition=fade:duration={transition:.3}:offset={offset:.3}{out}"
        ));
        prev = out;
        cum += durations[i] - transition;
    }

    // Audio: acrossfade overlaps the (now exactly sized) normalised streams.
    let mut prev_a = "[an0]".to_string();
    for i in 1..n {
        let out = if i == n - 1 {
            "[aout]".to_string()
        } else {
            format!("[ax{i}]")
        };
        chains.push(format!(
            "{prev_a}[an{i}]acrossfade=d={transition:.3}:c1=tri:c2=tri{out}"
        ));
        prev_a = out;
    }

    chains.join(";")
}

/// One ffmpeg invocation in the combine ladder.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedPass {
    /// Input paths exactly as they will appear on the command line.
    pub inputs: Vec<String>,
    /// Length of each input, in the same order.
    pub durations: Vec<f64>,
    /// Where this pass writes.
    pub output: String,
    /// File the filtergraph is written to (`-filter_complex_script`).
    pub filter_script: String,
    /// The last pass — the one that produces the user's MP4 and carries the
    /// playlist metadata and chapters. Every other pass is a throwaway batch.
    pub is_final: bool,
}

impl PlannedPass {
    /// Length of the video this pass produces.
    pub fn output_duration(&self, transition: f64) -> f64 {
        total_duration(&self.durations, transition)
    }
}

/// Expand a clip list into the full two-tier ladder of ffmpeg passes.
///
/// Round 1 batches the rendered *clips* into groups of at most `leaf_max`; every
/// later round batches the resulting *batch* files into groups of at most
/// `batch_max`, recursing until one pass can finish, then appends the final
/// pass. `batch_max` is deliberately much smaller than `leaf_max` because a
/// batch input is a full re-encoded video and far heavier to decode than a
/// still-image clip — so no pass ever opens more than `batch_max` large
/// intermediates at once (the memory bound; see the module docs). Batches within
/// a level are sized evenly, so a 41-clip level splits 21/20 rather than leaving
/// a batch of 1.
///
/// Keeping `leaf_max` = `combine_chunk_size` means round 1 is byte-identical to
/// the earlier single-tier algorithm — same `batch_r1_*` names and groupings —
/// so an interrupted run's first-level batches are still reused, not rebuilt.
///
/// Durations compose exactly: a batch of `k` inputs joined with transition `t`
/// lasts `Σd - t(k-1)`, so joining `b` such batches with the same `t` gives
/// `Σd - t(n-1)` — identical to combining all `n` in one pass, at any number of
/// levels. That is what lets chapter timestamps be computed once from the
/// original clips regardless of the ladder's depth.
///
/// Pure and unit-tested: the whole 500-track plan is verified without ffmpeg.
pub fn plan_passes(
    inputs: &[String],
    durations: &[f64],
    transition: f64,
    final_output: &str,
    leaf_max: usize,
    batch_max: usize,
) -> Vec<PlannedPass> {
    assert_eq!(inputs.len(), durations.len(), "inputs and durations must pair up");
    assert!(leaf_max >= 2, "a first-level pass must accept at least two clips");
    assert!(batch_max >= 2, "a batch-combine pass must accept at least two inputs");

    let mut passes = Vec::new();
    let mut level: Vec<(String, f64)> = inputs
        .iter()
        .cloned()
        .zip(durations.iter().copied())
        .collect();
    let mut round = 0u32;

    // `round == 0` is the clip level (cap `leaf_max`); every round after it
    // combines batch files (cap `batch_max`). The cap is read from `round`
    // *before* it is incremented for the pass names, so round 1 is the clip
    // level and its outputs are named `batch_r1_*`.
    loop {
        let cap = if round == 0 { leaf_max } else { batch_max };
        if level.len() <= cap {
            break;
        }
        round += 1;
        let mut next: Vec<(String, f64)> = Vec::new();
        for (i, range) in plan_batches(level.len(), cap).into_iter().enumerate() {
            // A lone input needs no re-encode — carry it into the next level.
            if range.len() == 1 {
                next.push(level[range.start].clone());
                continue;
            }
            let group = &level[range];
            let pass = PlannedPass {
                inputs: group.iter().map(|(p, _)| p.clone()).collect(),
                durations: group.iter().map(|(_, d)| *d).collect(),
                // Matroska + FLAC: intermediates are re-encoded once more by
                // the next level, so their audio must not lose anything here.
                output: format!("batch_r{round}_{i:04}.mkv"),
                filter_script: format!("fc_r{round}_{i:04}.txt"),
                is_final: false,
            };
            next.push((pass.output.clone(), pass.output_duration(transition)));
            passes.push(pass);
        }
        level = next;
    }

    passes.push(PlannedPass {
        inputs: level.iter().map(|(p, _)| p.clone()).collect(),
        durations: level.iter().map(|(_, d)| *d).collect(),
        output: final_output.to_string(),
        filter_script: "fc_final.txt".into(),
        is_final: true,
    });
    passes
}

/// Split `n` items into consecutive batches of at most `max`, as evenly as
/// possible. Never yields more than `max` per batch.
pub fn plan_batches(n: usize, max: usize) -> Vec<std::ops::Range<usize>> {
    assert!(max >= 2);
    if n <= max {
        // One batch spanning everything — a Vec holding a single Range, not a
        // Vec of the range's elements (which is what clippy assumes here).
        #[allow(clippy::single_range_in_vec_init)]
        return vec![0..n];
    }
    let batches = n.div_ceil(max);
    let base = n / batches;
    // The first `extra` batches take one more item than the rest.
    let extra = n % batches;
    let mut out = Vec::with_capacity(batches);
    let mut start = 0;
    for i in 0..batches {
        let len = base + usize::from(i < extra);
        out.push(start..start + len);
        start += len;
    }
    out
}

/// Build the ffmpeg argument list for one pass. Pure and tested.
///
/// The filtergraph is *not* included — it goes in `pass.filter_script` and is
/// referenced with `-filter_complex_script`, which is what keeps the command
/// line flat as the playlist grows. `metadata` is the FFMETADATA file and is
/// supplied for the final pass only.
pub fn build_pass_args(
    pass: &PlannedPass,
    metadata: Option<&str>,
    audio_bitrate_k: u32,
    encoder: &ResolvedEncoder,
) -> Vec<String> {
    // A single-input pass just stream-copies, so it decodes nothing — hardware
    // decode would set up an unused decoder. Only the multi-input passes, which
    // actually decode + filter + re-encode, get the `-hwaccel` prefix.
    let multi = pass.inputs.len() > 1;
    let decode = if multi { encoder.decode_args() } else { Vec::new() };

    let mut args: Vec<String> = vec!["-y".into()];
    for input in &pass.inputs {
        args.extend(decode.iter().cloned());
        args.extend(["-i".into(), input.clone()]);
    }
    let meta_index = pass.inputs.len();
    if let Some(meta) = metadata {
        // The FFMETADATA file is never hardware-decoded.
        args.extend(["-i".into(), meta.to_string()]);
    }

    if !multi {
        // Single input: nothing to stitch, so keep the streams as they are.
        args.extend(["-map", "0:v", "-map", "0:a"].map(String::from));
        if metadata.is_some() {
            args.extend(["-map_metadata".into(), meta_index.to_string()]);
        }
        args.extend(["-c".into(), "copy".into()]);
    } else {
        args.extend(["-filter_complex_script".into(), pass.filter_script.clone()]);
        args.extend(["-map", "[vout]", "-map", "[aout]"].map(String::from));
        if metadata.is_some() {
            args.extend(["-map_metadata".into(), meta_index.to_string()]);
        }
        if pass.is_final {
            // The kept file: the selected encoder at final quality, AAC audio.
            args.extend(encoder.video_args(QualityTier::Final));
            args.extend(["-c:a".into(), "aac".into()]);
            args.extend(["-b:a".into(), format!("{audio_bitrate_k}k")]);
        } else {
            // Intermediates are transcoded again by the next level, so they
            // trade size for speed: a faster encoder tier and lossless FLAC
            // audio so nothing is degraded before the final pass.
            args.extend(encoder.video_args(QualityTier::Intermediate));
            args.extend(["-c:a".into(), "flac".into()]);
        }
    }

    if pass.is_final {
        args.extend(["-movflags".into(), "+faststart".into()]);
    }
    args.push(pass.output.clone());
    args
}

/// Length of the command line this argument list produces, as Windows counts
/// it (arguments joined by spaces, plus the program name and quoting slack).
pub fn cmdline_len(program: &str, args: &[String]) -> usize {
    program.len() + args.iter().map(|a| a.len() + 3).sum::<usize>()
}

/// A path as it should appear on the command line: relative to the work
/// directory when possible (ffmpeg runs with its cwd set there), absolute
/// otherwise. Shortens every clip argument from ~60 characters to ~20.
fn arg_path(workdir: &Path, path: &Path) -> String {
    path.strip_prefix(workdir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Does this ffmpeg failure look like a hardware-*decode* problem — the kind a
/// software-decode retry can actually recover from — rather than an unrelated
/// error that would fail identically the second time?
///
/// The combine stage retries a failed pass with software decode only when this
/// returns true. It matches the signatures seen when the d3d11va / DXVA decoder
/// runs out of GPU memory or cannot hand a decoded frame back to the CPU —
/// ENOMEM ("Cannot allocate memory"), the `av_hwframe_transfer_data` error, and
/// the hwaccel / device names — and deliberately does NOT match failures like a
/// missing input, an invalid filter graph, a corrupt clip, a permission error,
/// disk-full, or a command-line mistake, none of which software decode would fix.
pub fn is_hwdecode_failure(stderr: &str) -> bool {
    // Lower-cased so identifier signatures (d3d11va, DXVA, ...) match whatever
    // casing ffmpeg emitted.
    const SIGNATURES: [&str; 7] = [
        "enomem",
        "cannot allocate memory",
        "failed to transfer data to output frame",
        "d3d11va",
        "hwaccel",
        "dxva",
        "d3d11",
    ];
    let haystack = stderr.to_lowercase();
    SIGNATURES.iter().any(|sig| haystack.contains(sig))
}

impl PlaylistRenderer {
    /// Combine the intermediate clips into the final playlist MP4.
    pub async fn combine(
        &self,
        ffmpeg: &str,
        workdir: &Path,
        token: &CancellationToken,
        tx: &Tx,
        log_file: Option<&Path>,
    ) -> Result<()> {
        anyhow::ensure!(!self.clips.is_empty(), "no clips to combine");

        // Measure what the renderer actually wrote. A clip cut at `-t 1.500`
        // lands at 1.5333 s because video can only end on a frame boundary, and
        // trusting the requested value would push every later chapter mark a
        // little further out of place.
        let requested: Vec<f64> = self.clips.iter().map(|c| c.duration).collect();
        let paths: Vec<PathBuf> = self.clips.iter().map(|c| c.path.clone()).collect();
        let ffprobe = crate::video::probe::ffprobe_path(ffmpeg);
        let measurement =
            crate::video::probe::measure_all(&ffprobe, &paths, &requested, token, tx, log_file)
                .await;
        let durations = measurement.durations.clone();

        let transition = safe_transition(&durations, self.transition);
        if transition <= 0.0 && self.transition >= MIN_TRANSITION {
            tx.log(
                "Transition shortened to a hard cut (a selected track is shorter than the \
                 requested transition)"
                    .to_string(),
            );
        }

        // Chapters come off the measured lengths, so a mark lands where the
        // track really starts rather than where it was meant to.
        let metadata = build_ffmetadata(
            &self.playlist_title,
            &self.clips,
            &durations,
            transition,
            self.chapters,
        );
        let meta_path = workdir.join("playlist_meta.txt");
        std::fs::write(&meta_path, &metadata).context("writing playlist metadata file")?;

        tx.log(format!(
            "Combining {} clip(s) into '{}' (transition {:.1}s, chapters {})",
            self.clips.len(),
            self.output.display(),
            transition,
            if self.chapters { "on" } else { "off" }
        ));

        // Everything below runs with ffmpeg's cwd set to `workdir`, so clips,
        // batch outputs and filter scripts are all referenced by bare file
        // name. Only the final output keeps an absolute path.
        let inputs: Vec<String> = self
            .clips
            .iter()
            .map(|c| arg_path(workdir, &c.path))
            .collect();
        let chunk = self.chunk_size.clamp(2, MAX_COMBINE_CHUNK);
        let batch_chunk = self.batch_chunk_size.clamp(2, MAX_COMBINE_CHUNK);
        let passes = plan_passes(
            &inputs,
            &durations,
            transition,
            &self.output.to_string_lossy(),
            chunk,
            batch_chunk,
        );

        tx.log(format!(
            "Encoding with: {}{}",
            self.encoder.kind.full_label(),
            if self.encoder.hardware_decode {
                " + hardware decode"
            } else {
                ""
            }
        ));
        if passes.len() > 1 {
            tx.log(format!(
                "{} clips combined in a two-tier ladder (up to {chunk} clips per first-level \
                 batch, then up to {batch_chunk} batches per combine) — {} passes ({} batches \
                 + final). The small batch cap keeps only a few large intermediates open at \
                 once, bounding memory.",
                self.clips.len(),
                passes.len(),
                passes.len() - 1
            ));
        }

        let total_passes = passes.len();
        // Intermediates present for the next level to consume. A batch this run
        // produced *or* reused from an interrupted run goes in here; the original
        // clips never do, so they are never deleted.
        let mut intermediates: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // Resume bookkeeping: whether any pass was skipped because its output was
        // already valid, and whether the "picking back up here" line was logged.
        let mut reused_any = false;
        let mut announced_resume = false;
        // Explicit progress record (resume.json). Advisory: every batch it names
        // is still ffprobe-validated before reuse — see the module docs.
        let mut checkpoint = crate::video::checkpoint::Checkpoint::load_or_new(workdir);
        // Measured length of everything the ladder will consume, keyed by the
        // name it appears under on the command line. Batch outputs are measured
        // as they are produced, so a correction at one level cannot leak into
        // the offsets of the next.
        let mut lengths: std::collections::HashMap<String, f64> = inputs
            .iter()
            .cloned()
            .zip(durations.iter().copied())
            .collect();

        for (n, pass) in passes.iter().enumerate() {
            if token.is_cancelled() {
                anyhow::bail!("cancelled");
            }

            let pass_durations: Vec<f64> = pass
                .inputs
                .iter()
                .zip(pass.durations.iter())
                .map(|(name, planned)| *lengths.get(name).unwrap_or(planned))
                .collect();

            // ---- Resume: reuse a batch an interrupted run already produced ----
            //
            // Applies at every ladder level, since the loop walks all of them in
            // order. Validity is decided by ffprobe, not mere existence: a batch a
            // crash truncated mid-write probes as unreadable and is rebuilt. The
            // measured length is also checked against what this plan predicts, so a
            // batch left over from a different clip set is never mistaken for one
            // of ours. The final pass is never reused — its output name is unique
            // per run, so it cannot pre-exist.
            if !pass.is_final {
                let produced = workdir.join(&pass.output);
                if produced.exists() {
                    let predicted = total_duration(&pass_durations, transition);
                    match crate::video::probe::probe_duration(
                        &ffprobe, &produced, token, log_file,
                    )
                    .await
                    {
                        Ok(actual) if (actual - predicted).abs() <= BATCH_REUSE_TOLERANCE => {
                            tx.log(format!(
                                "✓ Reusing {} — already combined ({actual:.1}s){}",
                                pass.output,
                                if checkpoint.has_batch(&pass.output) {
                                    " [checkpoint]"
                                } else {
                                    ""
                                }
                            ));
                            lengths.insert(pass.output.clone(), actual);
                            intermediates.insert(pass.output.clone());
                            checkpoint.record_batch(workdir, &pass.output);
                            // A filter script left behind by the crash is now dead.
                            let _ = std::fs::remove_file(workdir.join(&pass.filter_script));
                            reused_any = true;
                            continue;
                        }
                        Ok(actual) => {
                            tx.log(format!(
                                "✗ Rebuilding {}: measured {actual:.1}s but this plan expects \
                                 {predicted:.1}s — stale from a different run, regenerating.",
                                pass.output
                            ));
                            let _ = std::fs::remove_file(&produced);
                        }
                        Err(e) => {
                            tx.log(format!(
                                "✗ Rebuilding corrupt {} — ffprobe could not read it ({e:#}), \
                                 regenerating.",
                                pass.output
                            ));
                            let _ = std::fs::remove_file(&produced);
                        }
                    }
                }
            }
            // The first pass that actually runs after one or more were reused is
            // where the combine picks back up; say so once.
            if reused_any && !announced_resume {
                tx.log(format!("→ Continuing combine at {}", pass.output));
                announced_resume = true;
            }

            // The filtergraph goes to a file rather than the command line —
            // for 40 inputs it alone would be several KB of arguments.
            if pass.inputs.len() > 1 {
                std::fs::write(
                    workdir.join(&pass.filter_script),
                    build_combine_filter(&pass_durations, transition),
                )
                .with_context(|| format!("writing filtergraph {}", pass.filter_script))?;
            }

            let meta_arg = pass
                .is_final
                .then(|| arg_path(workdir, &meta_path));
            let args =
                build_pass_args(pass, meta_arg.as_deref(), self.audio_bitrate_k, &self.encoder);

            let length = cmdline_len(ffmpeg, &args);
            tx.log(format!(
                "Combine pass {}/{total_passes}: {} input(s) -> {} ({length} char command line)",
                n + 1,
                pass.inputs.len(),
                pass.output
            ));
            // The bug this architecture exists to prevent, caught before the
            // spawn instead of as a baffling "filename too long" from Windows.
            anyhow::ensure!(
                length < WINDOWS_CMDLINE_LIMIT,
                "internal error: combine pass {} would build a {length}-character command \
                 line, over the {WINDOWS_CMDLINE_LIMIT}-character Windows limit",
                n + 1
            );

            let stage = if pass.is_final {
                "combining playlist video".to_string()
            } else {
                format!("combining batch {}/{}", n + 1, total_passes - 1)
            };

            let mut cmd = tool_command(ffmpeg);
            cmd.current_dir(workdir);
            cmd.args(args);
            if let Err(hw_err) = run_streaming(cmd, ffmpeg, token, tx, "ffmpeg", log_file).await {
                // A multi-input pass run with hardware decode can exhaust the
                // GPU's decode memory ("Cannot allocate memory" / "Failed to
                // transfer data to output frame") once it opens many large
                // intermediates at once — even though every input is valid and
                // the same encoder handled the smaller, still-image batch passes
                // fine. This bites the final pass in particular: it is the one
                // pass whose input count is never bounded by the chunk size.
                //
                // Hardware *decode* is only an optimisation (the H.264 encode
                // still runs on the GPU either way), so when the failure looks
                // like a hardware-decode problem (see `is_hwdecode_failure`) we
                // retry just this pass with software decode — the same "never
                // assume; fall back" rule the encoder module applies to
                // encoding. All earlier batches are already on disk, so only this
                // one pass repeats. Failures that are NOT about hardware decode
                // (missing input, bad filter graph, corrupt clip, disk full, ...)
                // would fail identically the second time, so they propagate as-is.
                let retry_worthwhile = self.encoder.hardware_decode
                    && pass.inputs.len() > 1
                    && !token.is_cancelled()
                    && is_hwdecode_failure(&hw_err.to_string());
                if !retry_worthwhile {
                    return Err(hw_err).context(stage);
                }
                tx.log(format!(
                    "Combine pass {}/{total_passes} failed with what looks like a \
                     hardware-decode error ({}); retrying this pass with software decode.",
                    n + 1,
                    hw_err.to_string().lines().next().unwrap_or("error").trim()
                ));
                let sw = ResolvedEncoder { kind: self.encoder.kind, hardware_decode: false };
                let sw_args =
                    build_pass_args(pass, meta_arg.as_deref(), self.audio_bitrate_k, &sw);
                let mut sw_cmd = tool_command(ffmpeg);
                sw_cmd.current_dir(workdir);
                sw_cmd.args(sw_args);
                if let Err(sw_err) =
                    run_streaming(sw_cmd, ffmpeg, token, tx, "ffmpeg", log_file).await
                {
                    // Keep BOTH failures: report the original hardware-decode
                    // error, that a software-decode fallback was attempted, and
                    // how that fallback failed too — never silently replace the
                    // first error with the second.
                    return Err(anyhow::anyhow!(
                        "{stage}: failed with hardware decode, and the automatic \
                         software-decode fallback also failed.\n\n\
                         ── Original failure (hardware decode) ──\n{hw_err:#}\n\n\
                         ── Fallback attempt (software decode) also failed ──\n{sw_err:#}"
                    ));
                }
            }

            // Measure the batch we just produced instead of assuming it came
            // out the predicted length, so the next level builds its offsets on
            // fact. Falls back to the prediction if it cannot be probed.
            if !pass.is_final {
                let predicted = total_duration(&pass_durations, transition);
                let produced = workdir.join(&pass.output);
                let actual = crate::video::probe::probe_duration(
                    &ffprobe, &produced, token, log_file,
                )
                .await
                .unwrap_or_else(|e| {
                    tx.log(format!(
                        "WARN: could not measure {} ({e:#}); using the predicted {predicted:.3}s",
                        pass.output
                    ));
                    predicted
                });
                if (actual - predicted).abs() > 0.05 {
                    tx.log(format!(
                        "Note: {} came out {actual:.3}s, {:+.3}s from the predicted \
                         {predicted:.3}s — using the measured value.",
                        pass.output,
                        actual - predicted
                    ));
                }
                lengths.insert(pass.output.clone(), actual);
                // Record only after the pass has fully succeeded, so a batch a
                // crash truncated mid-write is never listed as complete.
                checkpoint.record_batch(workdir, &pass.output);
            }

            // Free each intermediate as soon as the pass consuming it succeeds;
            // a 500-track playlist would otherwise keep two full copies of the
            // video on disk. Original clips are never in this set.
            for input in &pass.inputs {
                if intermediates.remove(input) {
                    let _ = std::fs::remove_file(workdir.join(input));
                }
            }
            let _ = std::fs::remove_file(workdir.join(&pass.filter_script));
            if !pass.is_final {
                intermediates.insert(pass.output.clone());
            }
        }
        checkpoint.set_stage(workdir, "done");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clips(durs: &[f64]) -> Vec<ClipInfo> {
        durs.iter()
            .enumerate()
            .map(|(i, d)| ClipInfo {
                path: PathBuf::from(format!("clip_{i}.mp4")),
                duration: *d,
                title: format!("Song {}", i + 1),
                uploader: "Artist".into(),
            })
            .collect()
    }

    #[test]
    fn chapter_starts_account_for_overlap() {
        // No transition: plain cumulative sums.
        assert_eq!(chapter_starts(&[100.0, 200.0, 150.0], 0.0), vec![0.0, 100.0, 300.0]);
        // 2s transition shifts each subsequent clip earlier.
        assert_eq!(chapter_starts(&[100.0, 200.0, 150.0], 2.0), vec![0.0, 98.0, 296.0]);
    }

    #[test]
    fn total_duration_subtracts_transitions() {
        assert_eq!(total_duration(&[100.0, 200.0, 150.0], 0.0), 450.0);
        assert_eq!(total_duration(&[100.0, 200.0, 150.0], 2.0), 446.0);
        assert_eq!(total_duration(&[], 2.0), 0.0);
    }

    #[test]
    fn safe_transition_clamps_to_shortest_clip() {
        assert_eq!(safe_transition(&[100.0, 200.0], 2.0), 2.0);
        // Shortest clip is 1.5s -> transition clamped below it.
        let t = safe_transition(&[1.5, 200.0], 2.0);
        assert!(t < 1.5 && t > 0.0, "got {t}");
        // Zero requested -> zero.
        assert_eq!(safe_transition(&[100.0, 200.0], 0.0), 0.0);
        // Single clip -> no transition.
        assert_eq!(safe_transition(&[100.0], 2.0), 0.0);
    }

    #[test]
    fn concat_filter_when_no_transition() {
        let f = build_combine_filter(&[10.0, 20.0, 30.0], 0.0);
        assert!(
            f.ends_with("[0:v][an0][1:v][an1][2:v][an2]concat=n=3:v=1:a=1[vout][aout]"),
            "{f}"
        );
    }

    /// Every input's audio is pinned to the exact length the video side uses,
    /// which is what keeps the two timelines from drifting apart over hundreds
    /// of clips (see `build_combine_filter`).
    #[test]
    fn audio_is_normalised_to_the_measured_clip_length() {
        for transition in [0.0, 2.0] {
            let f = build_combine_filter(&[10.0, 20.5, 30.0], transition);
            assert!(f.contains("[0:a]apad,atrim=0:10.000000,asetpts=N/SR/TB[an0]"), "{f}");
            assert!(f.contains("[1:a]apad,atrim=0:20.500000,asetpts=N/SR/TB[an1]"), "{f}");
            assert!(f.contains("[2:a]apad,atrim=0:30.000000,asetpts=N/SR/TB[an2]"), "{f}");
            // The raw inputs are never crossfaded or concatenated directly.
            assert!(!f.contains("[1:a]acrossfade"), "{f}");
            assert!(!f.contains("[1:a][2:a]"), "{f}");
        }
    }

    #[test]
    fn xfade_offsets_are_correct_and_chained() {
        let f = build_combine_filter(&[100.0, 200.0, 150.0], 2.0);
        // First xfade at 100-2 = 98; second at (100+200-2)-2 = 296.
        assert!(f.contains("xfade=transition=fade:duration=2.000:offset=98.000[vx1]"), "{f}");
        assert!(f.contains("[vx1][2:v]xfade=transition=fade:duration=2.000:offset=296.000[vout]"), "{f}");
        // Audio crossfades chain to [aout], over the normalised streams.
        assert!(f.contains("[an0][an1]acrossfade=d=2.000"), "{f}");
        assert!(f.contains("acrossfade=d=2.000:c1=tri:c2=tri[aout]"), "{f}");
    }

    #[test]
    fn two_clips_produce_single_xfade_to_vout() {
        let f = build_combine_filter(&[10.0, 20.0], 2.0);
        assert!(f.contains("[0:v][1:v]xfade=transition=fade:duration=2.000:offset=8.000[vout]"), "{f}");
        assert!(f.contains("[an0][an1]acrossfade=d=2.000:c1=tri:c2=tri[aout]"), "{f}");
    }

    #[test]
    fn ffmetadata_has_playlist_tags_and_chapters() {
        let md =
            build_ffmetadata("My Playlist", &clips(&[100.0, 200.0]), &[100.0, 200.0], 2.0, true);
        assert!(md.starts_with(";FFMETADATA1"));
        assert!(md.contains("title=My Playlist"));
        assert!(md.contains("artist=SoundCloud Playlist"));
        assert!(md.contains("comment=Generated by SoundCloud2MP4"));
        assert_eq!(md.matches("[CHAPTER]").count(), 2);
        assert!(md.contains("START=0\nEND=98000"));
        assert!(md.contains("title=Artist - Song 1"));
        assert!(md.contains("title=Artist - Song 2"));
    }

    /// Chapters follow the *measured* clip lengths, not the requested ones —
    /// the whole point of probing. Here each clip was asked for 100 s but
    /// actually came out 100.5 s, so the second chapter has to move.
    #[test]
    fn ffmetadata_uses_measured_durations_not_requested_ones() {
        let requested = clips(&[100.0, 100.0, 100.0]);
        let measured = [100.5, 100.5, 100.5];
        let md = build_ffmetadata("P", &requested, &measured, 2.0, true);
        // Track 2 starts at 100.5 - 2 = 98.5 s, not the 98.0 s the requested
        // lengths imply; track 3 at 197.0 s rather than 196.0 s.
        assert!(md.contains("START=0\nEND=98500"), "{md}");
        assert!(md.contains("START=98500\nEND=197000"), "{md}");
        // Last chapter ends at the true total: 301.5 - 4 = 297.5 s.
        assert!(md.contains("START=197000\nEND=297500"), "{md}");
    }

    #[test]
    fn ffmetadata_escapes_special_characters() {
        let mut cs = clips(&[100.0]);
        cs[0].title = "A=B; C#D".into();
        let md = build_ffmetadata("List", &cs, &[100.0], 0.0, true);
        assert!(md.contains(r"title=Artist - A\=B\; C\#D"), "{md}");
    }

    #[test]
    fn chapters_can_be_disabled() {
        let md =
            build_ffmetadata("List", &clips(&[100.0, 200.0]), &[100.0, 200.0], 0.0, false);
        assert!(!md.contains("[CHAPTER]"));
        assert!(md.contains("title=List"));
    }

    /// The default upper-level batch cap used across these tests
    /// (`DEFAULT_BATCH_COMBINE_CHUNK_SIZE` in Settings).
    const TEST_BATCH_MAX: usize = 4;

    /// Level-0 inputs named the way `combine` names them, planned with the real
    /// default caps (leaf = `MAX_INPUTS_PER_PASS`, batches = `TEST_BATCH_MAX`).
    fn plan_for(n: usize, transition: f64) -> (Vec<PlannedPass>, Vec<f64>) {
        let inputs: Vec<String> = (0..n).map(|i| format!("clip_{i:04}.mp4")).collect();
        let durations: Vec<f64> = (0..n).map(|i| 180.0 + i as f64).collect();
        let passes = plan_passes(
            &inputs,
            &durations,
            transition,
            r"C:\Users\someone\Videos\SoundCloud Videos\My Playlist.mp4",
            MAX_INPUTS_PER_PASS,
            TEST_BATCH_MAX,
        );
        (passes, durations)
    }

    #[test]
    fn combine_args_multi_clip_reencodes_and_maps_metadata() {
        let (passes, _) = plan_for(2, 2.0);
        let args = build_pass_args(&passes[0], Some("meta.txt"), 320, &ResolvedEncoder::cpu());
        let j = args.join(" ");
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 3); // 2 clips + metadata
        assert!(j.contains("-filter_complex_script fc_final.txt"), "{j}");
        assert!(j.contains("-map [vout] -map [aout]"), "{j}");
        assert!(j.contains("-map_metadata 2"), "{j}"); // metadata is input index 2
        assert!(j.contains("-c:v libx264"), "{j}");
        assert!(j.contains("-b:a 320k"));
        assert!(j.contains("+faststart"));
        assert!(args.last().unwrap().ends_with("My Playlist.mp4"));
    }

    /// A representative stderr from a final-pass hardware-decode memory failure
    /// (and other hardware-decode signatures) must be recognised so the
    /// software-decode retry fires.
    #[test]
    fn hwdecode_failure_is_detected() {
        let real = "[h264 @ 0x0] Failed to transfer data to output frame: -12.\n\
                    [dec:h264] Error processing packet in decoder: Cannot allocate memory\n\
                    Task finished with error code: -12 (Cannot allocate memory)";
        assert!(is_hwdecode_failure(real));
        // Casing of device/hwaccel identifiers must not matter.
        assert!(is_hwdecode_failure("Cannot initialise -hwaccel d3d11va device"));
        assert!(is_hwdecode_failure("DXVA2 was unable to create the decoder"));
        assert!(is_hwdecode_failure("D3D11 texture allocation failed"));
        assert!(is_hwdecode_failure("device returned ENOMEM"));
    }

    /// Unrelated failures must NOT trigger a software-decode retry — they would
    /// fail identically the second time and only waste another pass.
    #[test]
    fn unrelated_failures_do_not_trigger_retry() {
        for stderr in [
            "clip_0003.mp4: No such file or directory",
            "Invalid argument: error parsing filter graph",
            "moov atom not found; input appears corrupt",
            "Permission denied",
            "No space left on device",
            "Unrecognized option 'foo'",
            "ffmpeg exited with code 1\n\nstderr:\nConversion failed!",
        ] {
            assert!(!is_hwdecode_failure(stderr), "false positive on: {stderr}");
        }
    }

    #[test]
    fn combine_args_single_clip_copies_streams() {
        let (passes, _) = plan_for(1, 2.0);
        let args = build_pass_args(&passes[0], Some("meta.txt"), 320, &ResolvedEncoder::cpu());
        let j = args.join(" ");
        assert!(j.contains("-c copy"), "{j}");
        assert!(j.contains("-map_metadata 1"), "{j}"); // 1 clip -> metadata index 1
        assert!(!j.contains("-filter_complex"), "{j}");
    }

    /// A GPU encoder swaps the codec and (for a multi-input pass) prefixes each
    /// input with the right `-hwaccel`, while audio stays AAC.
    #[test]
    fn gpu_encoder_sets_codec_and_hwaccel_on_the_final_pass() {
        let (passes, _) = plan_for(2, 2.0);
        let enc = ResolvedEncoder { kind: crate::video::encoder::EncoderKind::Amf, hardware_decode: true };
        let args = build_pass_args(&passes[0], Some("meta.txt"), 320, &enc);
        let j = args.join(" ");
        assert!(j.contains("-c:v h264_amf"), "{j}");
        assert!(j.contains("-c:a aac"), "{j}");
        // One -hwaccel per real input (the metadata input is never accelerated).
        assert_eq!(args.iter().filter(|a| *a == "-hwaccel").count(), 2, "{j}");
        assert!(j.contains("-hwaccel d3d11va"), "{j}");
    }

    #[test]
    fn small_playlists_still_use_exactly_one_pass() {
        for n in [2, 10, MAX_INPUTS_PER_PASS] {
            let (passes, _) = plan_for(n, 2.0);
            assert_eq!(passes.len(), 1, "n={n} should need no batching");
            assert!(passes[0].is_final);
            assert_eq!(passes[0].inputs.len(), n);
        }
    }

    #[test]
    fn batches_are_even_and_never_exceed_the_input_limit() {
        assert_eq!(plan_batches(5, 40), vec![0..5]);
        // 41 splits 21/20 rather than leaving a batch of one.
        assert_eq!(plan_batches(41, 40), vec![0..21, 21..41]);
        assert_eq!(plan_batches(80, 40), vec![0..40, 40..80]);
        for n in [41, 100, 500, 1_000, 5_000] {
            let batches = plan_batches(n, 40);
            assert!(batches.iter().all(|b| b.len() <= 40), "n={n}");
            assert!(batches.iter().all(|b| !b.is_empty()), "n={n}");
            assert_eq!(batches.iter().map(|b| b.len()).sum::<usize>(), n);
        }
    }

    #[test]
    fn large_playlist_is_split_into_batches_plus_one_final_pass() {
        let (passes, _) = plan_for(500, 2.0);
        let (batches, finals): (Vec<_>, Vec<_>) = passes.iter().partition(|p| !p.is_final);
        assert_eq!(finals.len(), 1, "exactly one final pass");
        assert!(passes.last().unwrap().is_final, "final pass must run last");
        assert!(!batches.is_empty(), "500 clips must be batched");
        // Each pass obeys its tier's cap: first-level (batch_r1_) passes take up
        // to the leaf cap, every other pass at most the small batch cap.
        for p in &passes {
            let cap = if p.output.starts_with("batch_r1_") {
                MAX_INPUTS_PER_PASS
            } else {
                TEST_BATCH_MAX
            };
            assert!(p.inputs.len() <= cap, "pass {} over its cap of {cap}", p.output);
        }
        // The first level consumes every clip exactly once, in order; the upper
        // levels consume batch files, not clips.
        let clips: Vec<&String> = passes
            .iter()
            .filter(|p| p.output.starts_with("batch_r1_"))
            .flat_map(|p| p.inputs.iter())
            .collect();
        assert_eq!(clips.len(), 500);
        for (i, name) in clips.iter().enumerate() {
            assert_eq!(*name, &format!("clip_{i:04}.mp4"), "order broken at {i}");
        }
        // Only the final pass writes an mp4; every batch stays in Matroska.
        assert!(batches.iter().all(|p| p.output.ends_with(".mkv")));
    }

    /// The two-tier ladder from the redesign: 493 clips → 13 first-level batches
    /// (cap 40) → 4 upper batches (cap 4) → final(4). No pass ever opens more than
    /// the small batch cap of large intermediates.
    #[test]
    fn two_tier_ladder_bounds_upper_levels_by_the_batch_cap() {
        let inputs: Vec<String> = (0..493).map(|i| format!("clip_{i:04}.mp4")).collect();
        let durations: Vec<f64> = (0..493).map(|i| 180.0 + i as f64).collect();
        let passes = plan_passes(&inputs, &durations, 2.0, "out.mp4", 40, 4);

        // Round 1: 493 clips → ⌈493/40⌉ = 13 batches, named exactly as the old
        // single-tier algorithm named them (so existing .work is reused).
        let r1: Vec<&PlannedPass> =
            passes.iter().filter(|p| p.output.starts_with("batch_r1_")).collect();
        assert_eq!(r1.len(), 13);
        for (i, p) in r1.iter().enumerate() {
            assert_eq!(p.output, format!("batch_r1_{i:04}.mkv"));
            assert!(p.inputs.len() <= 40);
        }
        // Round 2: 13 batches → ⌈13/4⌉ = 4 batches, each ≤ 4 inputs.
        let r2: Vec<&PlannedPass> =
            passes.iter().filter(|p| p.output.starts_with("batch_r2_")).collect();
        assert_eq!(r2.len(), 4);
        assert!(r2.iter().all(|p| p.inputs.len() <= 4));
        // r2 consumes the r1 batch files, in order.
        let r2_inputs: Vec<&String> = r2.iter().flat_map(|p| p.inputs.iter()).collect();
        assert_eq!(r2_inputs.len(), 13);
        for (i, name) in r2_inputs.iter().enumerate() {
            assert_eq!(*name, &format!("batch_r1_{i:04}.mkv"));
        }
        // Final: 4 batches → 1 pass, also within the batch cap.
        let last = passes.last().unwrap();
        assert!(last.is_final);
        assert_eq!(last.output, "out.mp4");
        assert_eq!(last.inputs.len(), 4);
        // No pass anywhere past the leaf level opens more than the batch cap.
        for p in passes.iter().filter(|p| !p.output.starts_with("batch_r1_")) {
            assert!(p.inputs.len() <= 4, "upper pass {} exceeds the batch cap", p.output);
        }
    }

    /// First-level batching must not depend on the batch cap, so the `batch_r1_*`
    /// files an interrupted run already produced still match this plan and are
    /// reused rather than rebuilt.
    #[test]
    fn first_level_batching_is_independent_of_the_batch_cap() {
        let inputs: Vec<String> = (0..493).map(|i| format!("clip_{i:04}.mp4")).collect();
        let durations: Vec<f64> = (0..493).map(|i| 180.0 + i as f64).collect();
        let r1_of = |batch_max: usize| -> Vec<(String, Vec<String>, Vec<f64>)> {
            plan_passes(&inputs, &durations, 2.0, "out.mp4", 40, batch_max)
                .into_iter()
                .filter(|p| p.output.starts_with("batch_r1_"))
                .map(|p| (p.output, p.inputs, p.durations))
                .collect()
        };
        // Same names, groupings AND durations regardless of the batch cap — which
        // is exactly what the resume reuse check (name + measured duration) needs.
        assert_eq!(r1_of(4), r1_of(8));
        assert_eq!(r1_of(4), r1_of(40));
    }

    /// Chapters/timing are invariant to ladder depth, including the extra levels
    /// the small batch cap introduces.
    #[test]
    fn two_tier_preserves_total_duration_exactly() {
        for n in [13, 41, 500, 1_500] {
            for transition in [0.0, 2.0] {
                let inputs: Vec<String> = (0..n).map(|i| format!("clip_{i:04}.mp4")).collect();
                let durations: Vec<f64> = (0..n).map(|i| 180.0 + i as f64).collect();
                let passes = plan_passes(&inputs, &durations, transition, "out.mp4", 40, 4);
                let expected = total_duration(&durations, transition);
                let actual = passes.last().unwrap().output_duration(transition);
                assert!((actual - expected).abs() < 1e-6, "n={n} t={transition}");
            }
        }
    }

    #[test]
    fn batching_preserves_total_duration_exactly() {
        for n in [2, 41, 500, 1_500] {
            for transition in [0.0, 2.0] {
                let (passes, durations) = plan_for(n, transition);
                let expected = total_duration(&durations, transition);
                let actual = passes.last().unwrap().output_duration(transition);
                assert!(
                    (actual - expected).abs() < 1e-6,
                    "n={n} t={transition}: batched {actual} != single-pass {expected}"
                );
            }
        }
    }

    /// The regression test for the reported failure: a 500-track playlist blew
    /// past Windows' ~32K command-line limit and ffmpeg failed to spawn with
    /// `os error 206`.
    #[test]
    fn combine_plan_stays_far_below_windows_limit() {
        let ffmpeg = r"C:\ytdlp\ffmpeg.exe";
        for n in [2, 500, 2_000] {
            let (passes, _) = plan_for(n, 2.0);
            for (i, pass) in passes.iter().enumerate() {
                let meta = pass.is_final.then_some("playlist_meta.txt");
                let len = cmdline_len(
                    ffmpeg,
                    &build_pass_args(pass, meta, 320, &ResolvedEncoder::cpu()),
                );
                assert!(
                    len < WINDOWS_CMDLINE_LIMIT / 4,
                    "n={n} pass {i}: {len} chars is uncomfortably close to the \
                     {WINDOWS_CMDLINE_LIMIT}-char limit"
                );
            }
        }
    }

    /// The largest user-selectable batch (64) must still build a command line
    /// far under the Windows limit, even with the hardware-decode `-hwaccel`
    /// prefix on every input.
    #[test]
    fn max_chunk_pass_stays_far_below_windows_limit() {
        let ffmpeg = r"C:\ytdlp\ffmpeg.exe";
        let inputs: Vec<String> = (0..500).map(|i| format!("clip_{i:04}.mp4")).collect();
        let durations: Vec<f64> = (0..500).map(|i| 180.0 + i as f64).collect();
        let passes =
            plan_passes(&inputs, &durations, 2.0, "out.mp4", MAX_COMBINE_CHUNK, MAX_COMBINE_CHUNK);
        let enc = ResolvedEncoder { kind: crate::video::encoder::EncoderKind::Amf, hardware_decode: true };
        for pass in &passes {
            assert!(pass.inputs.len() <= MAX_COMBINE_CHUNK);
            let meta = pass.is_final.then_some("playlist_meta.txt");
            let len = cmdline_len(ffmpeg, &build_pass_args(pass, meta, 320, &enc));
            assert!(len < WINDOWS_CMDLINE_LIMIT / 4, "pass of {} inputs: {len} chars", pass.inputs.len());
        }
    }

    /// The filtergraph is the other thing that grew with the track count; it
    /// must never reach the command line at all.
    #[test]
    fn filtergraph_is_never_passed_as_an_argument() {
        let (passes, _) = plan_for(500, 2.0);
        for pass in &passes {
            let args = build_pass_args(
                &pass.clone(),
                pass.is_final.then_some("m.txt"),
                320,
                &ResolvedEncoder::cpu(),
            );
            assert!(
                args.iter().all(|a| !a.contains("xfade") && !a.contains("acrossfade")),
                "filtergraph leaked onto the command line"
            );
            assert!(args.contains(&"-filter_complex_script".to_string()));
        }
        // ...and it is big enough that this matters.
        let graph = build_combine_filter(&passes[0].durations, 2.0);
        assert!(graph.len() > 1_000, "graph unexpectedly small: {}", graph.len());
    }
}
