//! Full-workflow integration test (network + real yt-dlp/ffmpeg required).
//!
//! Run explicitly with:
//!     cargo test --test e2e_workflow -- --ignored --nocapture
//!
//! Uses the tool paths from the saved config.json (falling back to
//! `yt-dlp`/`ffmpeg` on PATH). Set `SC2MP4_TEST_PLAYLIST` to a SoundCloud
//! playlist URL you have the rights to use before running.

use soundcloud2mp4::config::settings::Settings;
use soundcloud2mp4::downloader::ytdlp;
use soundcloud2mp4::models::messages::{Tx, WorkerMsg};
use soundcloud2mp4::models::track::{MetaState, Track, TrackStatus};
use soundcloud2mp4::pipeline;
use tokio_util::sync::CancellationToken;

/// Provide your own via `SC2MP4_TEST_PLAYLIST`. The default is a placeholder
/// that will not resolve, so this `#[ignore]`d test only runs meaningfully once
/// you point it at a real playlist you have the rights to use.
fn test_playlist() -> String {
    std::env::var("SC2MP4_TEST_PLAYLIST")
        .unwrap_or_else(|_| "https://soundcloud.com/xxxxxxxxxxxxxxxx".to_string())
}

#[test]
#[ignore = "requires network, yt-dlp and ffmpeg"]
fn full_workflow_produces_mp4_with_real_metadata() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut settings = Settings::load();
        settings.output_folder =
            std::env::temp_dir().join(format!("sc2mp4_e2e_{}", std::process::id()));
        settings.max_track_seconds = 0.0;
        settings.debug_mode = true;

        let (tx_raw, rx) = std::sync::mpsc::channel();
        let tx = Tx { tx: tx_raw, ctx: egui::Context::default() };
        let token = CancellationToken::new();

        // Step 1: flat playlist.
        let playlist = test_playlist();
        let (title, entries) =
            ytdlp::fetch_playlist(&settings.ytdlp_path, &playlist, &settings.cookie_source(), &token, &tx, None)
                .await
                .expect("playlist should load");
        println!("playlist '{title}' with {} entries", entries.len());
        assert!(!title.is_empty());
        assert!(!entries.is_empty());

        // Step 2: full metadata for the first track.
        let meta = ytdlp::fetch_track_info(&settings.ytdlp_path, &entries[0], &settings.cookie_source(), &token, &tx, None)
            .await
            .expect("track metadata should resolve");
        println!("metadata: {meta:?}");
        assert_ne!(meta.title, "Unknown");
        assert!(!meta.uploader.is_empty());
        assert!(meta.duration > 0, "duration must not be 0");
        assert!(meta.thumbnail.is_some(), "thumbnail must be present");

        // Steps 3-5: download + validate + render one short video.
        let mut track = Track::placeholder(
            entries[0].id.clone(),
            entries[0].url.clone(),
            entries[0].title.clone(),
        );
        track.apply_metadata(&meta, 0.0);
        assert_eq!(track.meta, MetaState::Loaded);
        track.play_seconds = track.play_seconds.min(12.0);

        pipeline::convert(
            settings.clone(),
            vec![(0, track)],
            soundcloud2mp4::video::encoder::ResolvedEncoder::cpu(),
            tx,
            token,
        )
        .await;

        // Inspect the messages the GUI would have received.
        let mut done_path = None;
        let mut failure = None;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerMsg::TrackStatus(_, TrackStatus::Done(p)) => done_path = Some(p),
                WorkerMsg::TrackStatus(_, TrackStatus::Failed(e)) => failure = Some(e),
                _ => {}
            }
        }
        if let Some(e) = failure {
            panic!("conversion failed:\n{e}");
        }
        let out = done_path.expect("no Done status received");
        let size = std::fs::metadata(&out).expect("output file must exist").len();
        println!("rendered {} ({size} bytes)", out.display());
        assert!(size > 100_000, "suspiciously small MP4: {size} bytes");
        assert!(out.file_name().unwrap().to_string_lossy().contains(" - "));

        let _ = std::fs::remove_dir_all(&settings.output_folder);
    });
}
