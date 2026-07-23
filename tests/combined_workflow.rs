//! Combined-playlist export integration test (network + yt-dlp + ffmpeg).
//!
//!     cargo test --test combined_workflow -- --ignored --nocapture
//!
//! Downloads a small test playlist (set `SC2MP4_TEST_PLAYLIST` to a playlist
//! URL you have the rights to use), renders each track to a fixed-length clip
//! and combines them into ONE mp4 with a 2s transition and chapters, then
//! verifies the file exists, has a plausible duration and carries chapter
//! metadata.

use soundcloud2mp4::config::settings::{ExportMode, Settings};
use soundcloud2mp4::downloader::ytdlp;
use soundcloud2mp4::models::messages::{Tx, WorkerMsg};
use soundcloud2mp4::models::track::{Track, TrackStatus};
use soundcloud2mp4::pipeline;
use soundcloud2mp4::utils::process::{run_capture, tool_command};
use tokio_util::sync::CancellationToken;

/// Provide your own via `SC2MP4_TEST_PLAYLIST`. The default is a placeholder
/// that will not resolve, so this `#[ignore]`d test only runs meaningfully once
/// you point it at a real playlist (3+ tracks) you have the rights to use.
fn test_playlist() -> String {
    std::env::var("SC2MP4_TEST_PLAYLIST")
        .unwrap_or_else(|_| "https://soundcloud.com/xxxxxxxxxxxxxxxx".to_string())
}

#[test]
#[ignore = "requires network, yt-dlp and ffmpeg"]
fn combined_mode_creates_single_ordered_video_with_chapters() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut settings = Settings::load();
        settings.output_folder =
            std::env::temp_dir().join(format!("sc2mp4_combined_{}", std::process::id()));
        settings.max_track_seconds = 8.0; // keep the render quick
        settings.export_mode = ExportMode::Combined;
        settings.transition_seconds = 2.0;
        settings.enable_chapters = true;
        settings.continue_on_fail = true;
        settings.playlist_video_name = String::new(); // fall back to playlist title

        let (tx_raw, rx) = std::sync::mpsc::channel();
        let tx = Tx { tx: tx_raw, ctx: egui::Context::default() };
        let token = CancellationToken::new();

        let playlist = test_playlist();
        let (title, entries) =
            ytdlp::fetch_playlist(&settings.ytdlp_path, &playlist, &settings.cookie_source(), &token, &tx, None)
                .await
                .expect("playlist should load");
        println!("playlist '{title}' -> {} tracks", entries.len());
        assert!(entries.len() >= 3, "test playlist should have >= 3 tracks");

        // Build selected tracks with resolved metadata, capped to 8s each.
        let mut selected = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            let meta =
                ytdlp::fetch_track_info(&settings.ytdlp_path, entry, &settings.cookie_source(), &token, &tx, None)
                    .await
                    .expect("metadata should resolve");
            let mut track =
                Track::placeholder(entry.id.clone(), entry.url.clone(), entry.title.clone());
            track.apply_metadata(&meta, settings.max_track_seconds);
            selected.push((i, track));
        }

        pipeline::convert_combined(
            settings.clone(),
            title.clone(),
            String::new(),
            selected,
            soundcloud2mp4::video::encoder::ResolvedEncoder::cpu(),
            tx,
            token,
        )
        .await;

        let mut done = None;
        let mut failure = None;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerMsg::TrackStatus(_, TrackStatus::Done(p)) => done = Some(p),
                WorkerMsg::TrackStatus(_, TrackStatus::Failed(e)) => failure = Some(e),
                _ => {}
            }
        }
        if let Some(e) = failure {
            panic!("combined export failed:\n{e}");
        }
        let out = done.expect("no Done status / output produced");
        assert!(out.exists(), "output file missing: {}", out.display());
        // The combined name should be the playlist title, not "Artist - Song".
        assert!(out.file_stem().unwrap().to_string_lossy().contains(&title));
        let size = std::fs::metadata(&out).unwrap().len();
        println!("combined video: {} ({size} bytes)", out.display());
        assert!(size > 300_000, "combined mp4 suspiciously small: {size}");

        // Confirm chapters + duration via ffmpeg's own stderr metadata dump.
        let mut cmd = tool_command(&settings.ffmpeg_path);
        cmd.arg("-i").arg(&out);
        let (_ok, _out, err) = run_capture(cmd, &settings.ffmpeg_path, &CancellationToken::new(), None)
            .await
            .unwrap();
        assert!(err.contains("Chapter"), "no chapters embedded:\n{err}");
        // 3 tracks x ~8s minus two 2s transitions ~= 20s.
        assert!(
            err.contains("Duration: 00:00:1") || err.contains("Duration: 00:00:2"),
            "unexpected duration:\n{}",
            err.lines().find(|l| l.contains("Duration")).unwrap_or("")
        );
        println!("chapters present: {}", err.matches("Chapter #").count());

        let _ = std::fs::remove_dir_all(&settings.output_folder);
    });
}
