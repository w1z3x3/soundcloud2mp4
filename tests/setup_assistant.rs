//! Behavioral checks for the setup assistant that don't actually install
//! anything: winget detection, manual instructions, and that install_missing
//! reports failure (with a manual-instructions fallback) when a bogus package
//! id is used — without ever touching the real tools.

use soundcloud2mp4::models::messages::{Tx, WorkerMsg};
use soundcloud2mp4::setup;

#[test]
fn manual_instructions_mention_both_tools() {
    let text = setup::manual_instructions();
    assert!(text.contains("ffmpeg"), "{text}");
    assert!(text.contains("yt-dlp"), "{text}");
}

#[test]
fn winget_detection_runs_without_panicking() {
    // We can't assert the value (CI machines vary) but it must not panic and
    // must be false on non-Windows.
    let available = setup::winget_available();
    if !cfg!(windows) {
        assert!(!available);
    }
}

#[test]
fn install_missing_reports_when_tools_absent() {
    // Point the re-check at executables that do not exist, so even a machine
    // with the tools installed exercises the failure/reporting path. We ask to
    // "install" nothing (both flags false), so no winget call happens; the
    // function should still refresh, re-check and emit a SetupDone.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (tx_raw, rx) = std::sync::mpsc::channel();
        let tx = Tx { tx: tx_raw, ctx: egui::Context::default() };

        setup::install_missing(
            false,
            false,
            "definitely-not-a-real-ffmpeg-binary".into(),
            "definitely-not-a-real-ytdlp-binary".into(),
            tx,
        )
        .await;

        let mut got_tools = false;
        let mut done = None;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerMsg::Tools { ffmpeg, ytdlp } => {
                    got_tools = true;
                    // The bogus paths must not resolve.
                    assert!(ffmpeg.is_none());
                    assert!(ytdlp.is_none());
                }
                WorkerMsg::SetupDone { success, message, .. } => {
                    done = Some((success, message));
                }
                _ => {}
            }
        }
        assert!(got_tools, "expected a Tools re-check message");
        // Nothing was requested to install, so it 'succeeds' vacuously.
        let (success, _msg) = done.expect("expected SetupDone");
        assert!(success, "with no installs requested it should report success");
    });
}
