//! Combine-stage scaling test: proves the batched ladder actually works with a
//! real ffmpeg, on a playlist large enough to need more than one pass.
//!
//!     cargo test --test large_combine -- --ignored --nocapture
//!
//! This is the regression test for the 500-track export that died with
//! `failed to start 'ffmpeg': The filename or extension is too long
//! (os error 206)` — Windows' ERROR_FILENAME_EXCED_RANGE, raised because the
//! single monster `-i`/`-filter_complex` command line ran past the ~32 KB
//! `CreateProcessW` limit.
//!
//! It needs no network: the clips are synthesised with lavfi, so it exercises
//! exactly the stage that failed and nothing else.

use std::path::{Path, PathBuf};

use soundcloud2mp4::models::messages::{Tx, WorkerMsg};
use soundcloud2mp4::utils::process::{run_capture, tool_command};
use soundcloud2mp4::video::concat::{
    total_duration, ClipInfo, PlaylistRenderer, MAX_INPUTS_PER_PASS,
};
use soundcloud2mp4::video::probe;
use tokio_util::sync::CancellationToken;

/// One more than a single pass accepts, so the ladder is forced to batch.
const CLIPS: usize = MAX_INPUTS_PER_PASS + 5;
const CLIP_SECONDS: f64 = 1.5;
const TRANSITION: f64 = 0.4;

/// Synthesise one short clip with a colour bar and a tone.
async fn make_clip(ffmpeg: &str, path: &Path, index: usize) {
    let hue = (index * 37 % 360) as f64;
    let mut cmd = tool_command(ffmpeg);
    cmd.args([
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("color=c=0x{:06x}:s=320x240:r=15", (index * 4_000_000) % 0xFF_FFFF),
        "-f",
        "lavfi",
        "-i",
        &format!("sine=frequency={}:sample_rate=44100", 220.0 + hue),
        "-t",
        &format!("{CLIP_SECONDS}"),
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
    ])
    .arg(path);
    let (ok, _, err) = run_capture(cmd, ffmpeg, &CancellationToken::new(), None)
        .await
        .expect("ffmpeg should run");
    assert!(ok, "could not build test clip {index}:\n{err}");
}

/// Exact length of one file, via the app's own probe.
async fn probe_duration_of(ffmpeg: &str, path: &Path) -> f64 {
    let ffprobe = probe::ffprobe_path(ffmpeg);
    probe::probe_duration(&ffprobe, path, &CancellationToken::new(), None)
        .await
        .expect("ffprobe should measure the clip")
}

/// Video and audio stream durations, probed separately — the pair that has to
/// agree for a chapter mark to be correct in both streams.
async fn probe_stream_durations(ffmpeg: &str, path: &Path) -> (f64, f64) {
    let ffprobe = probe::ffprobe_path(ffmpeg);
    let mut out = Vec::new();
    for stream in ["v:0", "a:0"] {
        let mut cmd = tool_command(&ffprobe);
        cmd.args([
            "-v",
            "error",
            "-select_streams",
            stream,
            "-show_entries",
            "stream=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path);
        let (ok, stdout, err) = run_capture(cmd, &ffprobe, &CancellationToken::new(), None)
            .await
            .expect("ffprobe should run");
        assert!(ok, "ffprobe failed for {stream}: {err}");
        out.push(
            stdout
                .trim()
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("no {stream} duration in {stdout:?}")),
        );
    }
    (out[0], out[1])
}

/// Start time of every chapter embedded in the finished file, in seconds.
async fn probe_chapter_starts(ffmpeg: &str, path: &Path) -> Vec<f64> {
    let ffprobe = probe::ffprobe_path(ffmpeg);
    let mut cmd = tool_command(&ffprobe);
    cmd.args([
        "-v",
        "error",
        "-show_chapters",
        "-show_entries",
        "chapter=start_time",
        "-of",
        "csv=p=0",
    ])
    .arg(path);
    let (ok, stdout, err) = run_capture(cmd, &ffprobe, &CancellationToken::new(), None)
        .await
        .expect("ffprobe should run");
    assert!(ok, "ffprobe could not read chapters: {err}");
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // Rows come out as `start_time,<chapter title>` — the title is carried
        // along because -show_chapters includes tags.
        .map(|l| {
            let field = l.split(',').next().unwrap_or(l);
            field
                .parse()
                .unwrap_or_else(|_| panic!("bad chapter start {field:?} in {l:?}"))
        })
        .collect()
}

/// Seconds parsed out of ffmpeg's `Duration: 00:00:49.90` line.
fn parse_duration(stderr: &str) -> f64 {
    let line = stderr
        .lines()
        .find(|l| l.trim_start().starts_with("Duration:"))
        .unwrap_or_else(|| panic!("no Duration line in:\n{stderr}"));
    let ts = line
        .trim_start()
        .trim_start_matches("Duration:")
        .split(',')
        .next()
        .unwrap()
        .trim();
    let parts: Vec<f64> = ts.split(':').map(|p| p.parse().unwrap_or(0.0)).collect();
    parts[0] * 3600.0 + parts[1] * 60.0 + parts[2]
}

/// Not every ffmpeg install ships an ffprobe next to it. Measuring must then
/// degrade to the old behaviour — trusting the requested durations — rather
/// than failing an export that would otherwise have worked.
#[test]
fn missing_ffprobe_falls_back_to_requested_durations() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (tx_raw, rx) = std::sync::mpsc::channel();
        let tx = Tx { tx: tx_raw, ctx: egui::Context::default() };
        let requested = vec![100.0, 200.0, 300.0];
        let paths: Vec<PathBuf> =
            (0..3).map(|i| PathBuf::from(format!("no_such_clip_{i}.mp4"))).collect();

        let measured = probe::measure_all(
            "definitely-not-a-real-ffprobe-binary",
            &paths,
            &requested,
            &CancellationToken::new(),
            &tx,
            None,
        )
        .await;

        assert_eq!(measured.durations, requested, "must fall back to what was asked for");
        assert_eq!(measured.measured, 0);
        assert!(!measured.all_measured());
        assert!(measured.error.is_some(), "the failure should be reported");

        // The user is told, rather than silently getting drifted chapters.
        let logs: Vec<String> = rx
            .try_iter()
            .filter_map(|m| match m {
                WorkerMsg::Log(l) => Some(l),
                _ => None,
            })
            .collect();
        assert!(
            logs.iter().any(|l| l.contains("could not measure") && l.contains("chapter")),
            "fallback must warn about chapter accuracy: {logs:?}"
        );
    });
}

#[test]
#[ignore = "requires ffmpeg; synthesises clips and runs the full combine ladder"]
fn large_playlist_combines_in_batches_without_a_giant_command_line() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let settings = soundcloud2mp4::config::settings::Settings::load();
        let ffmpeg = settings.ffmpeg_path.clone();

        let workdir = std::env::temp_dir()
            .join(format!("sc2mp4_large_combine_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workdir);
        std::fs::create_dir_all(&workdir).unwrap();

        // ---- Build the intermediate clips ---------------------------------
        println!("synthesising {CLIPS} clips...");
        let mut clips: Vec<ClipInfo> = Vec::new();
        for i in 0..CLIPS {
            let path = workdir.join(format!("clip_{i:04}.mp4"));
            make_clip(&ffmpeg, &path, i).await;
            clips.push(ClipInfo {
                path,
                duration: CLIP_SECONDS,
                title: format!("Song {}", i + 1),
                uploader: "Test Artist".into(),
            });
        }

        let (tx_raw, rx) = std::sync::mpsc::channel();
        let tx = Tx { tx: tx_raw, ctx: egui::Context::default() };
        let output = workdir.join("playlist.mp4");

        let renderer = PlaylistRenderer {
            clips: clips.clone(),
            output: output.clone(),
            playlist_title: "Big Test Playlist".into(),
            transition: TRANSITION,
            chapters: true,
            audio_bitrate_k: 128,
            encoder: soundcloud2mp4::video::encoder::ResolvedEncoder::cpu(),
            chunk_size: soundcloud2mp4::config::settings::DEFAULT_COMBINE_CHUNK_SIZE,
            batch_chunk_size: soundcloud2mp4::config::settings::DEFAULT_BATCH_COMBINE_CHUNK_SIZE,
        };

        // ---- The stage that used to fail ----------------------------------
        let started = std::time::Instant::now();
        let result = renderer
            .combine(&ffmpeg, &workdir, &CancellationToken::new(), &tx, None)
            .await;
        let logs: Vec<String> = rx
            .try_iter()
            .filter_map(|m| match m {
                WorkerMsg::Log(l) => Some(l),
                _ => None,
            })
            .collect();

        if let Err(e) = &result {
            for l in &logs {
                println!("{l}");
            }
            panic!("combine failed: {e:#}");
        }
        println!("combined {CLIPS} clips in {:?}", started.elapsed());

        // More than one ffmpeg invocation actually ran.
        let passes: Vec<&String> =
            logs.iter().filter(|l| l.starts_with("Combine pass ")).collect();
        assert!(
            passes.len() > 1,
            "{CLIPS} clips should need several passes, got: {passes:?}"
        );
        for p in &passes {
            println!("{p}");
        }
        assert!(
            logs.iter().any(|l| l.contains("exceed the")),
            "the batching decision should be logged: {logs:?}"
        );

        // No pass came anywhere near the Windows limit.
        for p in &passes {
            let chars: usize = p
                .split("char command line")
                .next()
                .and_then(|s| s.rsplit('(').next())
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or_else(|| panic!("could not parse pass log: {p}"));
            assert!(chars < 8_000, "pass command line too long ({chars}): {p}");
        }

        // ---- The output must be a correct video ---------------------------
        assert!(output.exists(), "no output produced");
        let mut cmd = tool_command(&ffmpeg);
        cmd.arg("-i").arg(&output);
        let (_ok, _out, err) = run_capture(cmd, &ffmpeg, &CancellationToken::new(), None)
            .await
            .unwrap();

        // Every track kept its chapter, in order.
        let chapters = err.matches("Chapter #").count();
        assert_eq!(chapters, CLIPS, "expected one chapter per track:\n{err}");
        assert!(err.contains("Song 1"), "chapter titles missing:\n{err}");
        assert!(err.contains(&format!("Song {CLIPS}")), "last chapter missing:\n{err}");
        assert!(err.contains("Big Test Playlist"), "playlist title missing:\n{err}");

        // ---- Timeline accuracy --------------------------------------------
        //
        // Chapter marks are only meaningful if the picture and the sound agree
        // on where a track starts. They used not to: `xfade` places video by
        // the offsets we compute, while `acrossfade` just consumes whatever the
        // decoded audio happens to be, and a 1.500 s AAC stream decodes to 65
        // whole frames (1.5093 s). That ~9 ms surplus compounded once per clip
        // — +0.19 s over 20 clips, ~4.6 s by 500 — so the audio slid steadily
        // later than its own video. Normalising each input's audio to the
        // measured clip length collapses the two timelines back together.
        let (video, audio) = probe_stream_durations(&ffmpeg, &output).await;
        println!("video stream: {video:.3}s   audio stream: {audio:.3}s");
        assert!(
            (video - audio).abs() < 0.10,
            "audio and video drifted apart by {:.3}s over {CLIPS} clips — chapters \
             cannot be correct for both",
            (video - audio).abs()
        );

        // Both streams should land on the length the chapter maths predicts
        // from the *measured* clip durations.
        let measured = probe_duration_of(&ffmpeg, &clips[0].path).await;
        let expected = total_duration(&vec![measured; CLIPS], TRANSITION);
        let actual = parse_duration(&err);
        println!(
            "clip measured at {measured:.4}s (asked for {CLIP_SECONDS}) -> \
             expected total {expected:.3}s, got {actual:.3}s"
        );
        assert!(
            measured > CLIP_SECONDS,
            "test premise broken: the clip should be longer than requested"
        );
        assert!(
            (actual - expected).abs() < 0.10,
            "total {actual:.3}s does not match the measured-duration plan {expected:.3}s"
        );

        // ---- Chapter marks land where the tracks really start -------------
        //
        // The point of measuring: with the requested 1.5 s the marks would
        // creep 0.0333 s earlier per track, so by track 45 the last chapter
        // would sit ~1.5 s — a whole track — away from where the audio
        // actually changes.
        let starts = probe_chapter_starts(&ffmpeg, &output).await;
        assert_eq!(starts.len(), CLIPS, "expected one chapter per track");
        let step = measured - TRANSITION;
        let mut worst = 0.0f64;
        for (k, got) in starts.iter().enumerate() {
            let want = k as f64 * step;
            worst = worst.max((got - want).abs());
        }
        let naive_step = CLIP_SECONDS - TRANSITION;
        let naive_error = (CLIPS - 1) as f64 * (step - naive_step);
        println!(
            "chapter marks: worst error {worst:.4}s (requested durations would have \
             put the last mark {naive_error:.3}s out)"
        );
        assert!(
            worst < 0.05,
            "chapter {worst:.4}s off the true track start — marks are not accurate"
        );
        assert!(
            naive_error > 1.0,
            "test premise broken: requested durations should have been visibly wrong"
        );

        // Intermediates were cleaned up as the ladder climbed.
        let leftovers: Vec<PathBuf> = std::fs::read_dir(&workdir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("batch_"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(leftovers.is_empty(), "batch intermediates left behind: {leftovers:?}");

        let size = std::fs::metadata(&output).unwrap().len();
        println!("output: {} ({size} bytes)", output.display());
        assert!(size > 10_000, "output suspiciously small: {size}");

        let _ = std::fs::remove_dir_all(&workdir);
    });
}
